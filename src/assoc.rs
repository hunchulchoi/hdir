//! Extension associations: rule matching and shell-safe command templates.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AssocRule {
    pub ext: Option<Vec<String>>,
    pub glob: Option<String>,
    pub cmd: String,
    #[serde(default)]
    pub wait: bool,
    #[serde(default)]
    pub background: bool,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AssocTable {
    rules: Vec<AssocRule>,
}

impl AssocTable {
    /// Validates rules. Invalid rules are dropped (or trimmed) with a warning.
    pub fn from_rules(rules: Vec<AssocRule>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut out = Vec::new();
        for mut rule in rules {
            let mut bad_glob = false;
            if let Some(g) = &rule.glob
                && parse_glob(g).is_none()
            {
                bad_glob = true;
                warnings.push(format!(
                    "assoc: invalid glob {g:?} (cmd {:?}); glob ignored",
                    rule.cmd
                ));
                rule.glob = None;
            }
            if rule
                .ext
                .as_ref()
                .is_some_and(|e| normalized_exts(e).is_empty())
            {
                rule.ext = None;
            }
            if rule.ext.is_none() && rule.glob.is_none() {
                if !bad_glob {
                    warnings.push(format!(
                        "assoc: rule without ext/glob skipped (cmd {:?})",
                        rule.cmd
                    ));
                }
                continue;
            }
            if rule.wait && rule.background {
                warnings.push(format!(
                    "assoc: both wait and background set (cmd {:?}); using wait",
                    rule.cmd
                ));
                rule.background = false;
            }
            out.push(rule);
        }
        (Self { rules: out }, warnings)
    }

    /// First match: glob rules (in order) before ext rules (in order).
    pub fn find(&self, file_name: &OsStr) -> Option<&AssocRule> {
        self.candidates(file_name).into_iter().next()
    }

    /// All matching rules, glob rules first.
    pub fn candidates(&self, file_name: &OsStr) -> Vec<&AssocRule> {
        let name = file_name.to_string_lossy();
        let lower = name.to_lowercase();
        let mut v: Vec<&AssocRule> = self
            .rules
            .iter()
            .filter(|r| {
                r.glob
                    .as_deref()
                    .and_then(parse_glob)
                    .is_some_and(|toks| glob_match(&toks, &name))
            })
            .collect();
        for r in &self.rules {
            let ext_hit = r.ext.as_ref().is_some_and(|e| {
                normalized_exts(e)
                    .iter()
                    .any(|x| lower.len() > x.len() + 1 && lower.ends_with(&format!(".{x}")))
            });
            if ext_hit && !v.iter().any(|g| std::ptr::eq(*g, r)) {
                v.push(r);
            }
        }
        v
    }
}

fn normalized_exts(exts: &[String]) -> Vec<String> {
    exts.iter()
        .map(|e| e.trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

enum Tok {
    Lit(char),
    Any,
    Star,
    Class {
        neg: bool,
        ranges: Vec<(char, char)>,
    },
}

/// Parses a glob; `None` if malformed (unclosed `[`).
fn parse_glob(pat: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = pat.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => toks.push(Tok::Star),
            '?' => toks.push(Tok::Any),
            '[' => {
                i += 1;
                let neg = matches!(chars.get(i), Some('!' | '^'));
                if neg {
                    i += 1;
                }
                let mut ranges = Vec::new();
                let mut first = true;
                loop {
                    let c = *chars.get(i)?;
                    if c == ']' && !first {
                        break;
                    }
                    first = false;
                    if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&e| e != ']')
                    {
                        ranges.push((c, chars[i + 2]));
                        i += 3;
                    } else {
                        ranges.push((c, c));
                        i += 1;
                    }
                }
                toks.push(Tok::Class { neg, ranges });
            }
            c => toks.push(Tok::Lit(c)),
        }
        i += 1;
    }
    Some(toks)
}

fn tok_matches(t: &Tok, c: char) -> bool {
    match t {
        Tok::Lit(l) => *l == c,
        Tok::Any => true,
        Tok::Star => false,
        Tok::Class { neg, ranges } => ranges.iter().any(|&(a, b)| a <= c && c <= b) != *neg,
    }
}

fn glob_match(toks: &[Tok], name: &str) -> bool {
    let s: Vec<char> = name.chars().collect();
    let (mut ti, mut si) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while si < s.len() {
        if matches!(toks.get(ti), Some(Tok::Star)) {
            backtrack = Some((ti, si));
            ti += 1;
        } else if toks.get(ti).is_some_and(|t| tok_matches(t, s[si])) {
            ti += 1;
            si += 1;
        } else if let Some((bt, bs)) = backtrack {
            ti = bt + 1;
            si = bs + 1;
            backtrack = Some((bt, bs + 1));
        } else {
            return false;
        }
    }
    toks[ti..].iter().all(|t| matches!(t, Tok::Star))
}

/// Expands `{}`, `{dir}`, `{name}`; `{{`/`}}` are literal braces. Placeholders
/// are shell-escaped for the local shell (POSIX on unix, PowerShell on
/// windows). If no placeholder is present the escaped path is appended.
pub fn render_command(template: &str, path: &Path, cwd: &Path) -> OsString {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => cwd,
    };
    let stem = path.file_stem().unwrap_or_default();
    let mut out = OsString::new();
    let mut used = false;
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("{{") {
            out.push("{");
            rest = r;
        } else if let Some(r) = rest.strip_prefix("}}") {
            out.push("}");
            rest = r;
        } else if let Some((r, val)) = [
            ("{}", path.as_os_str()),
            ("{dir}", dir.as_os_str()),
            ("{name}", stem),
        ]
        .into_iter()
        .find_map(|(p, v)| rest.strip_prefix(p).map(|r| (r, v)))
        {
            out.push(shell_escape(val));
            used = true;
            rest = r;
        } else {
            let mut buf = [0u8; 4];
            out.push(&*c.encode_utf8(&mut buf));
            rest = &rest[c.len_utf8()..];
        }
    }
    if !used {
        out.push(" ");
        out.push(shell_escape(path.as_os_str()));
    }
    out
}

/// POSIX single-quote escaping, byte-preserving.
#[cfg(unix)]
pub fn shell_escape(s: &OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'\'');
    for &b in s.as_bytes() {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

/// PowerShell single-quote escaping (non-UTF-16-clean names become U+FFFD).
#[cfg(windows)]
pub fn shell_escape(s: &OsStr) -> OsString {
    OsString::from(powershell_quote(&s.to_string_lossy()))
}

/// Single-quoted PowerShell literal: inside `'...'` nothing is expanded
/// (`$`, backtick, `"` are literal); only a single quote must be doubled.
/// PowerShell also treats the typographic quotes U+2018/2019/201A/201B as
/// single quotes, so those are doubled too.
#[cfg(any(windows, test))]
fn powershell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        out.push(c);
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Shell used for association commands on windows: PowerShell 7.
///
/// If `pwsh` is not installed, the caller may fall back to Windows PowerShell
/// (`"powershell"`, same `-NoProfile -Command` flags) by replacing argv[0].
pub fn windows_shell() -> &'static str {
    "pwsh"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Wait,
    Background,
    Foreground,
}

pub fn plan_run(
    rule: &AssocRule,
    path: &Path,
    cwd: &Path,
    shell: &str,
) -> (Vec<OsString>, RunMode) {
    let cmd = render_command(&rule.cmd, path, cwd);
    let mode = if rule.wait {
        RunMode::Wait
    } else if rule.background {
        RunMode::Background
    } else {
        RunMode::Foreground
    };
    (shell_argv(shell, cmd), mode)
}

/// unix: `<shell> -c <cmd>`.
#[cfg(unix)]
fn shell_argv(shell: &str, cmd: OsString) -> Vec<OsString> {
    vec![OsString::from(shell), OsString::from("-c"), cmd]
}

/// windows: `pwsh -NoProfile -Command <cmd>`; `shell` (a unix `$SHELL`
/// concept, possibly a stray bash path) is ignored in favor of
/// [`windows_shell`].
#[cfg(windows)]
fn shell_argv(_shell: &str, cmd: OsString) -> Vec<OsString> {
    vec![
        OsString::from(windows_shell()),
        OsString::from("-NoProfile"),
        OsString::from("-Command"),
        cmd,
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultAction {
    View,
    OpenWithSystem,
}

pub fn default_action(preview_is_text: bool) -> DefaultAction {
    if preview_is_text {
        DefaultAction::View
    } else {
        DefaultAction::OpenWithSystem
    }
}

/// Program of the platform's "open with default application" command.
pub fn system_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "cmd"
    } else {
        "xdg-open"
    }
}

/// Full argv to open `path` with the system default application.
///
/// windows: `cmd /c start "" <path>`. `start` treats a first quoted argument
/// as the window title, so a path with spaces would be swallowed as the title;
/// the empty title `""` (the literal two characters) must come first. It is a
/// separate argv item. Note for the executor: `std::process::Command` escapes
/// embedded quotes as `\"`, which cmd does not understand, so spawn this with
/// `CommandExt::raw_arg` for the title item. cmd metacharacters (`&`, `^`) in
/// a path without spaces are not protected by this argv.
pub fn system_opener_argv(path: &Path) -> Vec<OsString> {
    if cfg!(windows) {
        vec![
            OsString::from("cmd"),
            OsString::from("/c"),
            OsString::from("start"),
            OsString::from("\"\""),
            path.as_os_str().to_os_string(),
        ]
    } else {
        vec![
            OsString::from(system_opener()),
            path.as_os_str().to_os_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext_rule(exts: &[&str], cmd: &str) -> AssocRule {
        AssocRule {
            ext: Some(exts.iter().map(|s| s.to_string()).collect()),
            glob: None,
            cmd: cmd.into(),
            wait: false,
            background: false,
            name: None,
        }
    }
    fn glob_rule(g: &str, cmd: &str) -> AssocRule {
        AssocRule {
            ext: None,
            glob: Some(g.into()),
            cmd: cmd.into(),
            wait: false,
            background: false,
            name: None,
        }
    }
    fn table(rules: Vec<AssocRule>) -> AssocTable {
        let (t, w) = AssocTable::from_rules(rules);
        assert!(w.is_empty(), "{w:?}");
        t
    }
    fn cmd_of<'a>(t: &'a AssocTable, n: &str) -> Option<&'a str> {
        t.find(OsStr::new(n)).map(|r| r.cmd.as_str())
    }
    fn s(o: OsString) -> String {
        o.into_string().unwrap()
    }
    fn esc(x: &str) -> String {
        s(shell_escape(OsStr::new(x)))
    }
    fn render(t: &str, p: &str) -> String {
        s(render_command(t, Path::new(p), Path::new("/cwd")))
    }

    #[test]
    fn ext_case_insensitive() {
        let t = table(vec![ext_rule(&["PNG", "jpg"], "view")]);
        assert_eq!(cmd_of(&t, "a.png"), Some("view"));
        assert_eq!(cmd_of(&t, "a.JPG"), Some("view"));
        assert_eq!(cmd_of(&t, "a.gif"), None);
    }

    #[test]
    fn ext_with_leading_dot_in_config_is_tolerated() {
        let t = table(vec![ext_rule(&[".md"], "ed")]);
        assert_eq!(cmd_of(&t, "x.md"), Some("ed"));
    }

    #[test]
    fn glob_before_ext() {
        let t = table(vec![ext_rule(&["txt"], "ext"), glob_rule("*.txt", "glob")]);
        assert_eq!(cmd_of(&t, "a.txt"), Some("glob"));
    }

    #[test]
    fn first_match_wins() {
        let t = table(vec![ext_rule(&["rs"], "one"), ext_rule(&["rs"], "two")]);
        assert_eq!(cmd_of(&t, "a.rs"), Some("one"));
        let t = table(vec![glob_rule("a*", "g1"), glob_rule("*", "g2")]);
        assert_eq!(cmd_of(&t, "abc"), Some("g1"));
    }

    #[test]
    fn multi_part_ext() {
        let t = table(vec![ext_rule(&["tar.gz"], "tgz"), ext_rule(&["gz"], "gz")]);
        assert_eq!(cmd_of(&t, "a.tar.gz"), Some("tgz"));
        assert_eq!(cmd_of(&t, "A.TAR.GZ"), Some("tgz"));
        assert_eq!(cmd_of(&t, "a.gz"), Some("gz"));
        assert_eq!(cmd_of(&t, "a.tgz"), None);
        let t = table(vec![ext_rule(&["gz"], "gz")]);
        assert_eq!(cmd_of(&t, "a.tar.gz"), Some("gz"));
    }

    #[test]
    fn dotfiles_and_no_ext() {
        let t = table(vec![ext_rule(&["bashrc", "gz", "x"], "x")]);
        assert_eq!(cmd_of(&t, ".bashrc"), None);
        assert_eq!(cmd_of(&t, ".gz"), None);
        assert_eq!(cmd_of(&t, "Makefile"), None);
        assert_eq!(cmd_of(&t, "trailingdot."), None);
        assert_eq!(cmd_of(&t, "x.bashrc"), Some("x"));
    }

    #[test]
    fn dotfile_matches_glob() {
        let t = table(vec![glob_rule(".*rc", "rc")]);
        assert_eq!(cmd_of(&t, ".bashrc"), Some("rc"));
    }

    #[test]
    fn glob_basics() {
        let t = table(vec![
            glob_rule("Dockerfile*", "docker"),
            glob_rule("img_?.png", "img"),
            glob_rule("[abc]x", "cls"),
            glob_rule("n[0-9][0-9]", "range"),
            glob_rule("z[!a-c]", "neg"),
        ]);
        assert_eq!(cmd_of(&t, "Dockerfile"), Some("docker"));
        assert_eq!(cmd_of(&t, "Dockerfile.dev"), Some("docker"));
        assert_eq!(cmd_of(&t, "dockerfile"), None);
        assert_eq!(cmd_of(&t, "img_1.png"), Some("img"));
        assert_eq!(cmd_of(&t, "img_12.png"), None);
        assert_eq!(cmd_of(&t, "bx"), Some("cls"));
        assert_eq!(cmd_of(&t, "dx"), None);
        assert_eq!(cmd_of(&t, "n42"), Some("range"));
        assert_eq!(cmd_of(&t, "n4a"), None);
        assert_eq!(cmd_of(&t, "zd"), Some("neg"));
        assert_eq!(cmd_of(&t, "zb"), None);
    }

    #[test]
    fn glob_star_backtracking_and_korean() {
        let t = table(vec![glob_rule("*a*b", "g"), glob_rule("보고서?.*", "k")]);
        assert_eq!(cmd_of(&t, "xxaxxab"), Some("g"));
        assert_eq!(cmd_of(&t, "xxaxxa"), None);
        assert_eq!(cmd_of(&t, "보고서1.docx"), Some("k"));
    }

    #[test]
    fn candidates_order_glob_first() {
        let t = table(vec![
            ext_rule(&["txt"], "e1"),
            glob_rule("*.txt", "g1"),
            ext_rule(&["txt"], "e2"),
            glob_rule("a*", "g2"),
            glob_rule("zzz", "never"),
        ]);
        let c: Vec<&str> = t
            .candidates(OsStr::new("a.txt"))
            .iter()
            .map(|r| r.cmd.as_str())
            .collect();
        assert_eq!(c, ["g1", "g2", "e1", "e2"]);
        assert!(t.candidates(OsStr::new("q.bin")).is_empty());
    }

    #[test]
    fn rule_with_both_ext_and_glob_matches_either() {
        let mut r = ext_rule(&["md"], "both");
        r.glob = Some("README*".into());
        let t = table(vec![r]);
        assert_eq!(cmd_of(&t, "README"), Some("both"));
        assert_eq!(cmd_of(&t, "a.md"), Some("both"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_name_matches_by_ext() {
        use std::os::unix::ffi::OsStrExt;
        let t = table(vec![ext_rule(&["txt"], "t")]);
        let name = OsStr::from_bytes(b"\xff\xfe.TXT");
        assert!(t.find(name).is_some());
    }

    #[test]
    fn warnings_for_bad_rules() {
        let mut both = ext_rule(&["a"], "c");
        both.wait = true;
        both.background = true;
        let (t, w) = AssocTable::from_rules(vec![
            AssocRule {
                ext: None,
                glob: None,
                cmd: "x".into(),
                wait: false,
                background: false,
                name: None,
            },
            ext_rule(&[], "empty"),
            glob_rule("[abc", "badglob"),
            both,
            ext_rule(&["ok"], "fine"),
        ]);
        assert_eq!(w.len(), 4, "{w:?}");
        assert!(w[0].contains('x'), "{}", w[0]);
        assert!(w[2].contains("[abc"), "{}", w[2]);
        // invalid rules dropped; both-flag rule kept (wait wins)
        assert_eq!(cmd_of(&t, "f.ok"), Some("fine"));
        assert_eq!(cmd_of(&t, "f.empty"), None);
        let r = t.find(OsStr::new("z.a")).unwrap();
        assert!(r.wait && !r.background);
    }

    #[test]
    fn toml_plan_example_parses() {
        #[derive(Deserialize)]
        struct Cfg {
            assoc: Vec<AssocRule>,
        }
        let src = r##"
[[assoc]]
ext = ["md", "txt", "rs", "toml"]
cmd = "$EDITOR {}"
[[assoc]]
ext = ["png", "jpg", "pdf"]
cmd = "open {}"
background = true
[[assoc]]
ext = ["sh"]
cmd = "bash {}"
wait = true
[[assoc]]
glob = "Dockerfile*"
cmd = "docker build -f {} ."
name = "build"
"##;
        let cfg: Cfg = toml::from_str(src).unwrap();
        assert_eq!(cfg.assoc.len(), 4);
        let (t, w) = AssocTable::from_rules(cfg.assoc);
        assert!(w.is_empty());
        assert_eq!(cmd_of(&t, "a.PDF"), Some("open {}"));
        assert!(t.find(OsStr::new("a.PDF")).unwrap().background);
        assert!(t.find(OsStr::new("run.sh")).unwrap().wait);
        let d = t.find(OsStr::new("Dockerfile")).unwrap();
        assert_eq!(d.name.as_deref(), Some("build"));
        assert!(!d.wait && !d.background);
    }

    #[cfg(unix)]
    #[test]
    fn escape_basic() {
        assert_eq!(esc("abc"), "'abc'");
        assert_eq!(esc(""), "''");
        assert_eq!(esc("a b"), "'a b'");
        assert_eq!(esc("a'b"), r"'a'\''b'");
        assert_eq!(esc("'"), r"''\'''");
        assert_eq!(
            esc("$HOME `x` $(y) \"q\" \\ ; & |"),
            "'$HOME `x` $(y) \"q\" \\ ; & |'"
        );
        assert_eq!(esc("a\nb"), "'a\nb'");
        assert_eq!(esc("-rf"), "'-rf'");
        assert_eq!(esc("한글 파일.txt"), "'한글 파일.txt'");
    }

    #[cfg(unix)]
    #[test]
    fn escape_keeps_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let out = shell_escape(OsStr::from_bytes(b"a\xffb'c"));
        assert_eq!(out.as_bytes(), b"'a\xffb'\\''c'");
    }

    #[cfg(unix)]
    #[test]
    fn render_path_placeholder() {
        assert_eq!(render("vi {}", "/a b/it's.txt"), r"vi '/a b/it'\''s.txt'");
        assert_eq!(render("$EDITOR {}", "/x/y.md"), "$EDITOR '/x/y.md'");
        assert_eq!(render("cp {} {}.bak", "/x"), "cp '/x' '/x'.bak");
    }

    #[cfg(unix)]
    #[test]
    fn render_dir_and_name() {
        assert_eq!(
            render("mv {} {dir}/{name}.old", "/a b/c.tar.gz"),
            "mv '/a b/c.tar.gz' '/a b'/'c.tar'.old"
        );
        assert_eq!(render("echo {name}", "/x/.bashrc"), "echo '.bashrc'");
        assert_eq!(render("echo {name}", "/x/Makefile"), "echo 'Makefile'");
    }

    #[cfg(unix)]
    #[test]
    fn render_relative_path_dir_falls_back_to_cwd() {
        assert_eq!(render("ls {dir}", "file.txt"), "ls '/cwd'");
    }

    #[cfg(unix)]
    #[test]
    fn render_brace_escapes_and_unknown() {
        assert_eq!(render("echo {{}} {}", "/p"), "echo {} '/p'");
        assert_eq!(render("awk '{{print}}' {}", "/p"), "awk '{print}' '/p'");
        assert_eq!(render("x {unknown} {}", "/p"), "x {unknown} '/p'");
        assert_eq!(render("lone { and } {}", "/p"), "lone { and } '/p'");
    }

    #[cfg(unix)]
    #[test]
    fn render_auto_append_path() {
        assert_eq!(render("less", "/a b"), "less '/a b'");
        assert_eq!(render("echo {{x}}", "/p"), "echo {x} '/p'");
        assert_eq!(render("echo {name}", "/p/q.txt"), "echo 'q'");
    }

    #[cfg(unix)]
    #[test]
    fn render_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(OsStr::from_bytes(b"/d/\xff\xfe.txt"));
        let out = render_command("vi {}", p, Path::new("/cwd"));
        assert_eq!(out.as_bytes(), b"vi '/d/\xff\xfe.txt'");
    }

    #[cfg(unix)]
    #[test]
    fn render_hostile_name_stays_one_word() {
        let out = render("cat {}", "/d/$(rm -rf ~); `id` -x'\n.txt");
        assert_eq!(out, "cat '/d/$(rm -rf ~); `id` -x'\\''\n.txt'");
    }

    #[cfg(unix)]
    #[test]
    fn plan_run_modes() {
        let p = Path::new("/d/a b.sh");
        let cwd = Path::new("/d");
        let mut r = ext_rule(&["sh"], "bash {}");
        let (argv, mode) = plan_run(&r, p, cwd, "/bin/zsh");
        assert_eq!(mode, RunMode::Foreground);
        assert_eq!(
            argv,
            vec![
                OsString::from("/bin/zsh"),
                OsString::from("-c"),
                OsString::from("bash '/d/a b.sh'")
            ]
        );
        r.wait = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").1, RunMode::Wait);
        r.wait = false;
        r.background = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").1, RunMode::Background);
        r.wait = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").1, RunMode::Wait);
    }

    #[test]
    fn default_actions() {
        assert_eq!(default_action(true), DefaultAction::View);
        assert_eq!(default_action(false), DefaultAction::OpenWithSystem);
    }

    #[test]
    fn system_opener_per_platform() {
        if cfg!(target_os = "macos") {
            assert_eq!(system_opener(), "open");
        } else if cfg!(windows) {
            assert_eq!(system_opener(), "cmd");
        } else {
            assert_eq!(system_opener(), "xdg-open");
        }
    }

    #[cfg(unix)]
    #[test]
    fn opener_argv_unix() {
        let argv = system_opener_argv(Path::new("/d/a b.png"));
        assert_eq!(
            argv,
            vec![
                OsString::from(system_opener()),
                OsString::from("/d/a b.png")
            ]
        );
    }

    // PowerShell quoting is pure, so it is verified on every platform.
    #[test]
    fn powershell_quote_rules() {
        assert_eq!(powershell_quote("abc"), "'abc'");
        assert_eq!(powershell_quote(""), "''");
        assert_eq!(powershell_quote("a b"), "'a b'");
        assert_eq!(powershell_quote("it's"), "'it''s'");
        assert_eq!(powershell_quote("'"), "''''");
        assert_eq!(
            powershell_quote("$env:X `n $(y) \"q\" ; & |"),
            "'$env:X `n $(y) \"q\" ; & |'"
        );
        assert_eq!(powershell_quote("a\nb"), "'a\nb'");
        assert_eq!(powershell_quote("한글 파일.txt"), "'한글 파일.txt'");
        // typographic quotes are quote characters to PowerShell
        assert_eq!(
            powershell_quote("\u{2018}x\u{2019}"),
            "'\u{2018}\u{2018}x\u{2019}\u{2019}'"
        );
    }

    #[test]
    fn windows_shell_is_pwsh() {
        assert_eq!(windows_shell(), "pwsh");
    }

    #[cfg(windows)]
    mod win {
        use super::*;

        fn render_w(t: &str, p: &str) -> String {
            s(render_command(t, Path::new(p), Path::new(r"C:\cwd")))
        }

        #[test]
        fn escape_windows() {
            assert_eq!(esc("abc"), "'abc'");
            assert_eq!(esc("a b"), "'a b'");
            assert_eq!(esc("it's"), "'it''s'");
            assert_eq!(esc("$HOME `x`"), "'$HOME `x`'");
            assert_eq!(esc(r"C:\Users\x\f.txt"), r"'C:\Users\x\f.txt'");
            assert_eq!(esc("한글 파일.txt"), "'한글 파일.txt'");
        }

        #[test]
        fn render_windows_paths() {
            assert_eq!(
                render_w("code {}", r"C:\My Docs\it's.txt"),
                r"code 'C:\My Docs\it''s.txt'"
            );
            assert_eq!(
                render_w(r"mv {} {dir}\{name}.old", r"D:\a b\c.tar.gz"),
                r"mv 'D:\a b\c.tar.gz' 'D:\a b'\'c.tar'.old"
            );
            assert_eq!(render_w("less", r"C:\한글\x.md"), r"less 'C:\한글\x.md'");
            assert_eq!(render_w("echo {{}} {}", r"C:\p"), r"echo {} 'C:\p'");
        }

        #[test]
        fn render_relative_dir_falls_back_to_cwd() {
            assert_eq!(render_w("ls {dir}", "file.txt"), r"ls 'C:\cwd'");
        }

        #[test]
        fn hostile_name_stays_one_word() {
            assert_eq!(
                render_w("cat {}", r"C:\d\$(rm x); `id` 'q'.txt"),
                r"cat 'C:\d\$(rm x); `id` ''q''.txt'"
            );
        }

        #[test]
        fn plan_run_argv_windows() {
            let p = Path::new(r"C:\d\a b.ps1");
            let cwd = Path::new(r"C:\d");
            let mut r = ext_rule(&["ps1"], "& {}");
            let (argv, mode) = plan_run(&r, p, cwd, "/bin/bash");
            assert_eq!(mode, RunMode::Foreground);
            assert_eq!(
                argv,
                vec![
                    OsString::from("pwsh"),
                    OsString::from("-NoProfile"),
                    OsString::from("-Command"),
                    OsString::from(r"& 'C:\d\a b.ps1'")
                ]
            );
            r.wait = true;
            assert_eq!(plan_run(&r, p, cwd, "").1, RunMode::Wait);
            r.wait = false;
            r.background = true;
            assert_eq!(plan_run(&r, p, cwd, "").1, RunMode::Background);
        }

        #[test]
        fn opener_argv_windows() {
            let argv = system_opener_argv(Path::new(r"C:\My Docs\한글.png"));
            assert_eq!(
                argv,
                vec![
                    OsString::from("cmd"),
                    OsString::from("/c"),
                    OsString::from("start"),
                    OsString::from("\"\""),
                    OsString::from(r"C:\My Docs\한글.png"),
                ]
            );
        }
    }
}
