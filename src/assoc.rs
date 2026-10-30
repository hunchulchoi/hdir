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

const WIN_PLANNED: &str = "Windows 지원 예정";

/// Turns a command template into a POSIX shell script that never contains
/// the file name: `{}` -> `$1` (path), `{dir}` -> `$2`, `{name}` -> `$3`
/// (stem), quoted according to the surrounding quote context
/// (unquoted `"$1"`, inside `"..."` plain `$1`, inside `'...'` `'"$1"'`).
/// `{{`/`}}` are literal braces. If no placeholder is used, ` "$1"` is
/// appended. Best effort for `$(...)`/backtick nesting inside the template.
pub fn render_script(template: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Q {
        None,
        Single,
        Double,
    }
    let mut out = String::new();
    let mut used = false;
    let mut q = Q::None;
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("{{") {
            out.push('{');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("}}") {
            out.push('}');
            rest = r;
        } else if let Some((r, n)) = [("{}", 1), ("{dir}", 2), ("{name}", 3)]
            .into_iter()
            .find_map(|(p, n)| rest.strip_prefix(p).map(|r| (r, n)))
        {
            match q {
                Q::None => out.push_str(&format!("\"${n}\"")),
                Q::Double => out.push_str(&format!("${n}")),
                Q::Single => out.push_str(&format!("'\"${n}\"'")),
            }
            used = true;
            rest = r;
        } else {
            match (q, c) {
                (Q::None, '\'') => q = Q::Single,
                (Q::None, '"') => q = Q::Double,
                (Q::Single, '\'') | (Q::Double, '"') => q = Q::None,
                (Q::None | Q::Double, '\\') => {
                    // an escaped char never changes the quote state
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                    if let Some(n) = rest.chars().next() {
                        out.push(n);
                        rest = &rest[n.len_utf8()..];
                    }
                    continue;
                }
                _ => {}
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !used {
        out.push_str(" \"$1\"");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Wait,
    Background,
    Foreground,
}

/// Argv for running an association. unix: `[shell, "-c", script, "hdir",
/// path, dir, name]` - the path reaches the script only as `$1`/`$2`/`$3`,
/// never as script text. Windows: not supported yet.
pub fn plan_run(
    rule: &AssocRule,
    path: &Path,
    cwd: &Path,
    shell: &str,
) -> Result<(Vec<OsString>, RunMode), String> {
    let mode = if rule.wait {
        RunMode::Wait
    } else if rule.background {
        RunMode::Background
    } else {
        RunMode::Foreground
    };
    shell_argv(rule, path, cwd, shell).map(|a| (a, mode))
}

#[cfg(unix)]
fn shell_argv(
    rule: &AssocRule,
    path: &Path,
    cwd: &Path,
    shell: &str,
) -> Result<Vec<OsString>, String> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => cwd,
    };
    Ok(vec![
        OsString::from(shell),
        OsString::from("-c"),
        OsString::from(render_script(&rule.cmd)),
        OsString::from("hdir"),
        path.as_os_str().to_os_string(),
        dir.as_os_str().to_os_string(),
        path.file_stem().unwrap_or_default().to_os_string(),
    ])
}

#[cfg(windows)]
fn shell_argv(_: &AssocRule, _: &Path, _: &Path, _: &str) -> Result<Vec<OsString>, String> {
    Err(WIN_PLANNED.to_string())
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
    } else {
        "xdg-open"
    }
}

/// Full argv to open `path` with the system default application
/// (windows: not supported yet).
pub fn system_opener_argv(path: &Path) -> Result<Vec<OsString>, String> {
    if cfg!(windows) {
        return Err(WIN_PLANNED.to_string());
    }
    Ok(vec![
        OsString::from(system_opener()),
        path.as_os_str().to_os_string(),
    ])
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
    fn script_placeholders_by_quote_context() {
        assert_eq!(render_script("vi {}"), r#"vi "$1""#);
        assert_eq!(render_script("vi \"{}\""), r#"vi "$1""#);
        assert_eq!(render_script("vi '{}'"), r#"vi ''"$1"''"#);
        assert_eq!(render_script("cp {} {}.bak"), r#"cp "$1" "$1".bak"#);
        assert_eq!(
            render_script("mv {} {dir}/{name}.old"),
            r#"mv "$1" "$2"/"$3".old"#
        );
        assert_eq!(
            render_script("echo \"{dir}/{name}.old\""),
            r#"echo "$2/$3.old""#
        );
        // escaped quote does not change the context
        assert_eq!(render_script(r#"echo \" {}"#), r#"echo \" "$1""#);
        assert_eq!(render_script(r#"echo "a\"{}""#), r#"echo "a\"$1""#);
    }

    #[cfg(unix)]
    #[test]
    fn script_braces_unknown_and_auto_append() {
        assert_eq!(render_script("echo {{}} {}"), r#"echo {} "$1""#);
        assert_eq!(render_script("awk '{{print}}' {}"), r#"awk '{print}' "$1""#);
        assert_eq!(render_script("x {unknown} {}"), r#"x {unknown} "$1""#);
        assert_eq!(render_script("lone { and } {}"), r#"lone { and } "$1""#);
        assert_eq!(render_script("less"), r#"less "$1""#);
        assert_eq!(render_script("echo {{x}}"), r#"echo {x} "$1""#);
        // only {name} used: path is still not appended
        assert_eq!(render_script("echo {name}"), r#"echo "$3""#);
    }

    #[cfg(unix)]
    #[test]
    fn plan_run_modes_and_argv() {
        let p = Path::new("/d/a b.sh");
        let cwd = Path::new("/d");
        let mut r = ext_rule(&["sh"], "bash {}");
        let (argv, mode) = plan_run(&r, p, cwd, "/bin/zsh").unwrap();
        assert_eq!(mode, RunMode::Foreground);
        assert_eq!(
            argv,
            vec![
                OsString::from("/bin/zsh"),
                OsString::from("-c"),
                OsString::from(r#"bash "$1""#),
                OsString::from("hdir"),
                OsString::from("/d/a b.sh"),
                OsString::from("/d"),
                OsString::from("a b"),
            ]
        );
        r.wait = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").unwrap().1, RunMode::Wait);
        r.wait = false;
        r.background = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").unwrap().1, RunMode::Background);
        r.wait = true;
        assert_eq!(plan_run(&r, p, cwd, "sh").unwrap().1, RunMode::Wait);
    }

    #[cfg(unix)]
    #[test]
    fn plan_run_relative_path_dir_falls_back_to_cwd() {
        let r = ext_rule(&["txt"], "ls {dir}");
        let (argv, _) = plan_run(&r, Path::new("file.txt"), Path::new("/cwd"), "sh").unwrap();
        assert_eq!(argv[5], OsString::from("/cwd"));
        assert_eq!(argv[6], OsString::from("file"));
    }

    #[cfg(unix)]
    #[test]
    fn plan_run_keeps_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(OsStr::from_bytes(b"/d/\xff\xfe.txt"));
        let r = ext_rule(&["txt"], "vi {}");
        let (argv, _) = plan_run(&r, p, Path::new("/cwd"), "sh").unwrap();
        assert_eq!(argv[4].as_bytes(), b"/d/\xff\xfe.txt");
    }

    /// Really executes the argv with /bin/sh: hostile file names must neither
    /// run commands nor split into several words, in every quote context.
    #[cfg(unix)]
    #[test]
    fn hostile_names_are_inert_when_executed() {
        let names = [
            "$(touch pwned)",
            "`touch pwned`",
            "`id`",
            "a'b\"c $HOME",
            "x; touch pwned",
            "한글 파일 & more",
            "-rf",
        ];
        let templates = [
            "printf %s {}",
            "printf %s \"{}\"",
            "printf %s '{}'",
            "printf %s",
            "printf %s \"{dir}/{name}\"|cat",
        ];
        for name in names {
            for tpl in templates {
                let d = tempfile::tempdir().unwrap();
                let f = d.path().join(name);
                std::fs::write(&f, "x").unwrap();
                let r = ext_rule(&[""], tpl);
                let (argv, _) = plan_run(&r, &f, d.path(), "/bin/sh").unwrap();
                let out = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(d.path())
                    .output()
                    .unwrap();
                assert!(!d.path().join("pwned").exists(), "{name:?} via {tpl:?}");
                let stem = f.file_stem().unwrap().to_string_lossy().into_owned();
                let want = if tpl.contains("{dir}") {
                    format!("{}/{}", d.path().display(), stem)
                } else {
                    f.display().to_string()
                };
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    want,
                    "{name:?} via {tpl:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_is_planned_not_supported() {
        let r = ext_rule(&["txt"], "type {}");
        let e = plan_run(&r, Path::new(r"C:.txt"), Path::new(r"C:"), "sh").unwrap_err();
        assert!(e.contains("Windows 지원 예정"), "{e}");
        let e = system_opener_argv(Path::new(r"C:.png")).unwrap_err();
        assert!(e.contains("Windows 지원 예정"), "{e}");
    }

    #[test]
    fn default_actions() {
        assert_eq!(default_action(true), DefaultAction::View);
        assert_eq!(default_action(false), DefaultAction::OpenWithSystem);
    }

    #[cfg(unix)]
    #[test]
    fn system_opener_per_platform() {
        if cfg!(target_os = "macos") {
            assert_eq!(system_opener(), "open");
        } else {
            assert_eq!(system_opener(), "xdg-open");
        }
    }

    #[cfg(unix)]
    #[test]
    fn opener_argv_unix() {
        let argv = system_opener_argv(Path::new("/d/a b.png")).unwrap();
        assert_eq!(
            argv,
            vec![
                OsString::from(system_opener()),
                OsString::from("/d/a b.png")
            ]
        );
    }
}
