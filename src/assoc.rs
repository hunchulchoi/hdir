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

/// The PowerShell counterpart of [`render_script`], for a script run with
/// `-File` and the path, folder and name as `$args[0..3]` (so they are never
/// parsed as code). A placeholder inside `"..."` becomes `$($args[n])`; a
/// bare word holding one becomes a single double-quoted string (PowerShell
/// would otherwise split `{dir}\{name}` into two arguments), with `` ` ``,
/// `"` and `$` of the word escaped. Placeholders inside `'...'` or in a
/// word that also has quotes cannot be expressed and are refused. `{{`/`}}`
/// are literal braces; without any placeholder ` "$($args[0])"` is appended.
pub fn render_ps(template: &str) -> Result<String, String> {
    const OPS: &[char] = &[';', '|', '&', '(', ')', ','];
    // a real placeholder, not one written with doubled braces
    fn has_placeholder(w: &str) -> bool {
        let mut r = w;
        while !r.is_empty() {
            if r.starts_with("{{") || r.starts_with("}}") {
                r = &r[2..];
            } else if ["{}", "{dir}", "{name}"].iter().any(|p| r.starts_with(p)) {
                return true;
            } else {
                let n = r.chars().next().map_or(1, char::len_utf8);
                r = &r[n..];
            }
        }
        false
    }
    const PH: [(&str, usize); 3] = [("{}", 0), ("{dir}", 1), ("{name}", 2)];
    let refused = || {
        crate::i18n::t(
            "PowerShell: 작은따옴표나 따옴표가 섞인 단어 안의 {}는 쓸 수 없음",
            "PowerShell: {} cannot be used inside a word that mixes quotes",
        )
        .to_string()
    };
    let placeholder = |s: &str| {
        PH.iter()
            .find_map(|(p, n)| s.strip_prefix(p).map(|r| (r.len(), *n)))
    };
    let mut out = String::new();
    let mut used = false;
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        // one word: up to the next whitespace outside quotes
        let mut end = 0;
        let mut q: Option<char> = None;
        for (i, ch) in rest.char_indices() {
            match q {
                Some(open) if ch == open => q = None,
                Some(_) => {}
                None if ch == '"' || ch == '\'' => q = Some(ch),
                None if ch.is_whitespace() => break,
                None => {}
            }
            end = i + ch.len_utf8();
        }
        let word = &rest[..end];
        rest = &rest[end..];
        // operators around the word stay outside any quoting: `{};` is the
        // path, then a statement separator
        let core = word.trim_matches(|ch: char| OPS.contains(&ch));
        let lead =
            &word[..word.len() - word.trim_start_matches(|ch: char| OPS.contains(&ch)).len()];
        let tail = &word[lead.len() + core.len()..];
        out.push_str(lead);
        let word = core;
        let has_quotes = word.contains(['"', '\'']);
        if !has_placeholder(word) {
            out.push_str(&word.replace("{{", "{").replace("}}", "}"));
            out.push_str(tail);
            continue;
        }
        used = true;
        if !has_quotes {
            // a bare word: each run between operators (`{}|more`) holding a
            // placeholder becomes one "..." string; the operators stay bare
            for piece in word.split_inclusive(|ch: char| OPS.contains(&ch)) {
                let (text, op) = match piece.char_indices().last() {
                    Some((i, ch)) if OPS.contains(&ch) => (&piece[..i], &piece[i..]),
                    _ => (piece, ""),
                };
                if has_placeholder(text) {
                    out.push('"');
                    let mut w = text;
                    while let Some(ch) = w.chars().next() {
                        if let Some((left, n)) = placeholder(w) {
                            out.push_str(&format!("$($args[{n}])"));
                            w = &w[w.len() - left..];
                        } else if w.starts_with("{{") || w.starts_with("}}") {
                            out.push(ch);
                            w = &w[2..];
                        } else {
                            if matches!(ch, '`' | '"' | '$') {
                                out.push('`');
                            }
                            out.push(ch);
                            w = &w[ch.len_utf8()..];
                        }
                    }
                    out.push('"');
                } else {
                    out.push_str(&text.replace("{{", "{").replace("}}", "}"));
                }
                out.push_str(op);
            }
            out.push_str(tail);
            continue;
        }
        // quoted parts: only double quotes may hold a placeholder, and only
        // when the word is a single "..." string
        let inner = word
            .strip_prefix('"')
            .and_then(|w| w.strip_suffix('"'))
            .filter(|w| !w.contains('"'))
            .ok_or_else(refused)?;
        out.push('"');
        let mut w = inner;
        while let Some(ch) = w.chars().next() {
            if let Some((left, n)) = placeholder(w) {
                out.push_str(&format!("$($args[{n}])"));
                w = &w[w.len() - left..];
            } else if w.starts_with("{{") || w.starts_with("}}") {
                out.push(ch);
                w = &w[2..];
            } else {
                out.push(ch);
                w = &w[ch.len_utf8()..];
            }
        }
        out.push('"');
        out.push_str(tail);
    }
    if !used {
        out.push_str(" \"$($args[0])\"");
    }
    Ok(out)
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

/// Windows: the rule becomes a temporary `.ps1` ([`render_ps`]) that
/// deletes itself first, run with `-File` so the path, folder and name
/// arrive as `$args[0..3]`. `shell` is used when it is a PowerShell, else
/// `pwsh` (or Windows PowerShell).
#[cfg(windows)]
fn shell_argv(
    rule: &AssocRule,
    path: &Path,
    cwd: &Path,
    shell: &str,
) -> Result<Vec<OsString>, String> {
    use std::io::Write;
    let body = render_ps(&rule.cmd)?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => cwd,
    };
    let mut f = tempfile::Builder::new()
        .prefix("hdir-assoc-")
        .suffix(".ps1")
        .tempfile()
        .map_err(|e| {
            crate::tr!(
                "임시 스크립트를 만들 수 없음: {e}",
                "Could not create temporary script: {e}"
            )
        })?;
    // BOM: Windows PowerShell reads a BOM-less script as the ANSI code page
    let script = format!(
        "\u{feff}Remove-Item -LiteralPath $PSCommandPath -ErrorAction SilentlyContinue\r\n{body}\r\n"
    );
    f.write_all(script.as_bytes()).map_err(|e| {
        crate::tr!(
            "임시 스크립트를 쓸 수 없음: {e}",
            "Could not write temporary script: {e}"
        )
    })?;
    let (_, script_path) = f.keep().map_err(|e| {
        crate::tr!(
            "임시 스크립트를 둘 수 없음: {e}",
            "Could not keep temporary script: {e}"
        )
    })?;
    let ps = if matches!(
        Path::new(shell)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("pwsh" | "powershell")
    ) {
        OsString::from(shell)
    } else {
        crate::cmdline::default_shell(None, std::env::var_os("PATH"), true)
    };
    let mut v = vec![ps];
    v.extend(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"].map(OsString::from));
    v.push(script_path.into_os_string());
    v.push(path.as_os_str().to_os_string());
    v.push(dir.as_os_str().to_os_string());
    v.push(path.file_stem().unwrap_or_default().to_os_string());
    Ok(v)
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
        // hands the file to its registered application (ShellExecute)
        "explorer.exe"
    } else {
        "xdg-open"
    }
}

/// Full argv to open `path` with the system default application.
pub fn system_opener_argv(path: &Path) -> Result<Vec<OsString>, String> {
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
    fn a_windows_rule_runs_as_a_self_deleting_script_with_literal_args() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("a $b;(x) 'q'.txt");
        std::fs::write(&file, "x").unwrap();
        let r = ext_rule(
            &["txt"],
            r"Set-Content -LiteralPath {dir}\out.txt -Value {name}",
        );
        let (argv, _) = plan_run(&r, &file, t.path(), "pwsh").unwrap();
        assert_eq!(
            argv[1..5],
            ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"].map(OsString::from)
        );
        let script = std::path::PathBuf::from(&argv[5]);
        assert!(script.exists());
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status();
        // the Windows gate has PowerShell: a failed start is a failure
        let status = status.unwrap();
        assert!(status.success());
        let got = std::fs::read_to_string(t.path().join("out.txt")).unwrap();
        assert_eq!(
            got.trim_end(),
            "a $b;(x) 'q'",
            "the name stayed text, never code"
        );
        assert!(!script.exists(), "the script deleted itself");
        assert_eq!(
            system_opener_argv(&file).unwrap(),
            vec![OsString::from("explorer.exe"), file.into_os_string()]
        );
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

#[cfg(test)]
mod ps_tests {
    use super::render_ps;

    #[test]
    fn placeholders_become_args_and_bare_words_stay_one_argument() {
        assert_eq!(render_ps("code {}").unwrap(), r#"code "$($args[0])""#);
        assert_eq!(
            render_ps(r"Copy-Item {} {dir}\{name}.old").unwrap(),
            r#"Copy-Item "$($args[0])" "$($args[1])\$($args[2]).old""#
        );
        assert_eq!(
            render_ps(r#"Write-Host "file: {} in {dir}""#).unwrap(),
            r#"Write-Host "file: $($args[0]) in $($args[1])""#
        );
        // literal text of a rewritten word keeps its meaning inside "..."
        assert_eq!(
            render_ps("echo $x{name}`").unwrap(),
            r#"echo "`$x$($args[2])``""#
        );
        assert_eq!(render_ps("notepad").unwrap(), r#"notepad "$($args[0])""#);
        assert_eq!(
            render_ps("echo {{x}} {}").unwrap(),
            r#"echo {x} "$($args[0])""#
        );
        assert_eq!(
            render_ps("echo 'a b' {}").unwrap(),
            r#"echo 'a b' "$($args[0])""#
        );
        // operators stay outside the quotes
        assert_eq!(
            render_ps("notepad {}; Write-Host done").unwrap(),
            r#"notepad "$($args[0])"; Write-Host done"#
        );
        assert_eq!(render_ps("(type {})").unwrap(), r#"(type "$($args[0])")"#);
        assert_eq!(
            render_ps("Get-Content {}|more").unwrap(),
            r#"Get-Content "$($args[0])"|more"#
        );
        assert_eq!(
            render_ps(r"cp {}&echo {dir}\x").unwrap(),
            r#"cp "$($args[0])"&echo "$($args[1])\x""#
        );
        // doubled braces are text, not placeholders: the path is appended
        assert_eq!(
            render_ps("echo {{name}}").unwrap(),
            r#"echo {name} "$($args[0])""#
        );
        assert_eq!(render_ps("echo {{}}").unwrap(), r#"echo {} "$($args[0])""#);
    }

    #[test]
    fn placeholders_that_cannot_be_expressed_are_refused() {
        for t in ["echo '{}'", r#"echo a"{}"b"#, r#"echo "x"'{}'"#] {
            assert!(render_ps(t).is_err(), "{t}");
        }
    }
}
