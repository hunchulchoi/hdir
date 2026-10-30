//! Command line (`:`): pure parsing of the internal `cd`, path resolution,
//! input history and the editable line state. No IO.

use std::path::{Component, Path, PathBuf};

use ratatui::crossterm::event::KeyEvent;

use crate::dialog::{self, DialogResult};

/// Where an internal `cd` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdTarget {
    /// `cd` / `cd ~`
    Home,
    /// `cd -`: the active panel's previous directory.
    Prev,
    /// An explicit path (`~` already expanded; may be relative).
    Path(PathBuf),
}

/// Split `rest` into shell-like words. `None` when it uses anything beyond
/// plain quoting (variables, globs, pipes, ...), so the real shell must run it.
/// Each word carries whether it started with an unquoted `~`.
fn words(rest: &str) -> Option<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut tilde = false;
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    out.push((std::mem::take(&mut cur), tilde));
                    in_word = false;
                    tilde = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => cur.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.peek() {
                            Some(&n @ ('"' | '\\')) => {
                                cur.push(n);
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' if cfg!(unix) => {
                in_word = true;
                cur.push(chars.next()?);
            }
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | '{' | '!'
            | '#' => return None,
            c => {
                if !in_word && c == '~' {
                    tilde = true;
                }
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push((cur, tilde));
    }
    Some(out)
}

/// Recognize `cd`, `cd -`, `cd <path>` (quotes and `~` allowed). Anything more
/// complex (`cd a && ls`, `cd $X`, `cd a b`) is `None` and goes to the shell.
pub fn parse_cd(cmd: &str, home: Option<&Path>) -> Option<CdTarget> {
    let cmd = cmd.trim();
    let rest = cmd.strip_prefix("cd")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let ws = words(rest)?;
    match ws.as_slice() {
        [] => Some(CdTarget::Home),
        [(w, tilde)] => {
            if w == "-" {
                return Some(CdTarget::Prev);
            }
            if *tilde {
                if w == "~" {
                    return Some(CdTarget::Home);
                }
                let tail = w.strip_prefix("~/")?;
                let home = home?;
                return Some(CdTarget::Path(if tail.is_empty() {
                    home.to_path_buf()
                } else {
                    home.join(tail)
                }));
            }
            if w.is_empty() || w == "--" {
                return None;
            }
            Some(CdTarget::Path(PathBuf::from(w)))
        }
        _ => None,
    }
}

/// Strip a `\\?\` verbatim prefix (what `canonicalize` yields on Windows)
/// from drive and UNC paths so they display normally.
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// The physical, existing directory `p` points to (every component is
/// resolved through the filesystem, symlinks first, like `cd -P`).
/// Errors if it does not exist or is not a directory.
pub fn canonical_dir(p: &Path) -> std::io::Result<PathBuf> {
    let c = strip_verbatim(&std::fs::canonicalize(p)?);
    if !c.is_dir() {
        return Err(std::io::Error::other("디렉터리가 아님"));
    }
    Ok(c)
}

/// Lexically clean `p`: drop `.`, fold `..` (like `cd -L`).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Absolute destination of `target` seen from `cwd`.
pub fn resolve_cd(
    cwd: &Path,
    target: &CdTarget,
    home: Option<&Path>,
    prev: Option<&Path>,
) -> Result<PathBuf, String> {
    match target {
        CdTarget::Home => home
            .map(Path::to_path_buf)
            .ok_or_else(|| "홈 디렉터리를 알 수 없음".to_string()),
        CdTarget::Prev => prev
            .map(Path::to_path_buf)
            .ok_or_else(|| "이전 디렉터리 없음".to_string()),
        CdTarget::Path(p) => {
            let full = cwd.join(p);
            canonical_dir(&full).map_err(|e| format!("{}: {e}", p.display()))
        }
    }
}

/// In-memory command history: newest last, consecutive duplicates dropped.
#[derive(Debug, Clone, Default)]
pub struct History {
    items: Vec<String>,
}

pub const HISTORY_CAP: usize = 100;

impl History {
    pub fn push(&mut self, cmd: &str) {
        if cmd.is_empty() || self.items.last().is_some_and(|l| l == cmd) {
            return;
        }
        self.items.push(cmd.to_string());
        if self.items.len() > HISTORY_CAP {
            self.items.remove(0);
        }
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }
}

/// What a key did to the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdEvent {
    Pending,
    Run(String),
    Cancel,
}

/// The editable command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdLine {
    pub value: String,
    /// Cursor as a char index.
    pub cursor: usize,
    /// Index into the history while browsing it.
    browse: Option<usize>,
    /// What was typed before browsing started.
    draft: String,
}

impl CmdLine {
    pub fn key(&mut self, key: KeyEvent, history: &History) -> CmdEvent {
        use ratatui::crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => return CmdEvent::Cancel,
            KeyCode::Up => self.older(history),
            KeyCode::Down => self.newer(history),
            _ => {
                let before = self.value.clone();
                if let DialogResult::Submitted(v) =
                    dialog::input_key(&mut self.value, &mut self.cursor, key)
                {
                    return CmdEvent::Run(v);
                }
                if self.value != before {
                    self.browse = None;
                }
            }
        }
        CmdEvent::Pending
    }

    fn show(&mut self, s: String) {
        self.cursor = s.chars().count();
        self.value = s;
    }

    fn older(&mut self, h: &History) {
        let items = h.items();
        let next = match self.browse {
            None if items.is_empty() => return,
            None => {
                self.draft = self.value.clone();
                items.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.browse = Some(next);
        self.show(items[next].clone());
    }

    fn newer(&mut self, h: &History) {
        let Some(i) = self.browse else {
            return;
        };
        if i + 1 < h.items().len() {
            self.browse = Some(i + 1);
            self.show(h.items()[i + 1].clone());
        } else {
            self.browse = None;
            let d = std::mem::take(&mut self.draft);
            self.show(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn home() -> PathBuf {
        PathBuf::from("/home/u")
    }

    fn cd(s: &str) -> Option<CdTarget> {
        parse_cd(s, Some(&home()))
    }

    fn path(s: &str) -> Option<CdTarget> {
        Some(CdTarget::Path(PathBuf::from(s)))
    }

    #[test]
    fn plain_cd_variants() {
        assert_eq!(cd("cd"), Some(CdTarget::Home));
        assert_eq!(cd("  cd  "), Some(CdTarget::Home));
        assert_eq!(cd("cd -"), Some(CdTarget::Prev));
        assert_eq!(cd("cd ~"), Some(CdTarget::Home));
        assert_eq!(cd("cd /tmp"), path("/tmp"));
        assert_eq!(cd("cd sub/dir"), path("sub/dir"));
        assert_eq!(cd("cd .."), path(".."));
        assert_eq!(cd("cd 한글 폴더"), None, "two words");
    }

    #[test]
    fn tilde_expansion() {
        assert_eq!(cd("cd ~/x"), path("/home/u/x"));
        assert_eq!(cd("cd ~/"), path("/home/u"));
        assert_eq!(cd("cd ~root"), None, "other users go to the shell");
        assert_eq!(parse_cd("cd ~/x", None), None);
        assert_eq!(cd("cd '~/x'"), path("~/x"), "quoted tilde is literal");
    }

    #[test]
    fn quoted_paths() {
        assert_eq!(cd("cd \"my dir\""), path("my dir"));
        assert_eq!(cd("cd 'a b'"), path("a b"));
        assert_eq!(cd("cd \"한글 폴더\""), path("한글 폴더"));
        assert_eq!(cd("cd 'it''s'"), path("its"));
        assert_eq!(cd("cd \"a\\\"b\""), path("a\"b"));
        assert_eq!(cd("cd '-'"), Some(CdTarget::Prev), "shell treats it alike");
        assert_eq!(cd("cd \"unterminated"), None);
    }

    #[cfg(unix)]
    #[test]
    fn backslash_escapes_space_on_unix() {
        assert_eq!(cd("cd my\\ dir"), path("my dir"));
    }

    #[test]
    fn complex_commands_go_to_the_shell() {
        for c in [
            "cd a && ls",
            "cd a; ls",
            "cd $HOME",
            "cd \"$HOME/x\"",
            "cd *",
            "cd a | cat",
            "cd a > f",
            "cdx",
            "cdr",
            "ls",
            "cd -- a",
            "cd ''",
        ] {
            assert_eq!(cd(c), None, "{c}");
        }
    }

    #[test]
    fn resolve_relative_home_prev() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(base.join("b/c")).unwrap();
        std::fs::create_dir_all(base.join("x/y")).unwrap();
        let cwd = base.join("b");
        let r = |t: CdTarget, p: Option<&Path>| resolve_cd(&cwd, &t, Some(&home()), p);
        assert_eq!(r(CdTarget::Home, None), Ok(home()));
        assert_eq!(
            r(CdTarget::Prev, Some(Path::new("/z"))),
            Ok(PathBuf::from("/z"))
        );
        assert!(r(CdTarget::Prev, None).is_err());
        assert_eq!(r(CdTarget::Path("c".into()), None), Ok(base.join("b/c")));
        assert_eq!(
            r(CdTarget::Path("../x/./y".into()), None),
            Ok(base.join("x/y"))
        );
        assert_eq!(
            r(CdTarget::Path(base.join("x").into()), None),
            Ok(base.join("x"))
        );
        assert!(resolve_cd(&cwd, &CdTarget::Home, None, None).is_err());
    }

    #[test]
    fn cd_never_drops_a_missing_component_before_dotdot() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let r = |p: &str| resolve_cd(&base, &CdTarget::Path(p.into()), None, None);
        assert!(r("missing/..").is_err());
        assert!(r("missing/../..").is_err());
        assert_eq!(r("."), Ok(base.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn cd_follows_symlinks_physically_like_cd_dash_p() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(base.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(base.join("real/inner"), base.join("link")).unwrap();
        let r = resolve_cd(&base, &CdTarget::Path("link/..".into()), None, None);
        assert_eq!(r, Ok(base.join("real")));
    }

    #[test]
    fn verbatim_prefix_is_stripped_for_display() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\srv\sh\a")),
            PathBuf::from(r"\\srv\sh\a")
        );
        assert_eq!(strip_verbatim(Path::new("/a/b")), PathBuf::from("/a/b"));
    }

    #[test]
    fn history_dedups_consecutive_and_caps() {
        let mut h = History::default();
        h.push("ls");
        h.push("ls");
        h.push("pwd");
        h.push("ls");
        h.push("");
        assert_eq!(h.items(), ["ls", "pwd", "ls"]);
        let mut h = History::default();
        for i in 0..150 {
            h.push(&format!("c{i}"));
        }
        assert_eq!(h.items().len(), HISTORY_CAP);
        assert_eq!(h.items()[0], "c50");
        assert_eq!(h.items()[99], "c149");
    }

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(line: &mut CmdLine, h: &History, s: &str) {
        for c in s.chars() {
            assert_eq!(line.key(k(KeyCode::Char(c)), h), CmdEvent::Pending);
        }
    }

    #[test]
    fn editing_korean_cursor_ctrl_u_enter_esc() {
        let h = History::default();
        let mut l = CmdLine::default();
        typed(&mut l, &h, "echo 한글");
        assert_eq!(l.value, "echo 한글");
        l.key(k(KeyCode::Left), &h);
        l.key(k(KeyCode::Backspace), &h);
        assert_eq!(l.value, "echo 글");
        l.key(k(KeyCode::Home), &h);
        typed(&mut l, &h, "x");
        assert_eq!(l.value, "xecho 글");
        assert_eq!(l.cursor, 1);
        assert_eq!(
            l.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), &h),
            CmdEvent::Pending
        );
        assert_eq!(l.value, "");
        assert_eq!(l.key(k(KeyCode::Enter), &h), CmdEvent::Pending, "empty");
        typed(&mut l, &h, "ls");
        assert_eq!(l.key(k(KeyCode::Enter), &h), CmdEvent::Run("ls".into()));
        assert_eq!(l.key(k(KeyCode::Esc), &h), CmdEvent::Cancel);
    }

    #[test]
    fn up_down_browse_history_and_restore_draft() {
        let mut h = History::default();
        h.push("one");
        h.push("two");
        let mut l = CmdLine::default();
        typed(&mut l, &h, "dr");
        l.key(k(KeyCode::Up), &h);
        assert_eq!(l.value, "two");
        l.key(k(KeyCode::Up), &h);
        assert_eq!(l.value, "one");
        l.key(k(KeyCode::Up), &h);
        assert_eq!(l.value, "one", "stops at the oldest");
        l.key(k(KeyCode::Down), &h);
        assert_eq!(l.value, "two");
        l.key(k(KeyCode::Down), &h);
        assert_eq!(l.value, "dr", "draft comes back");
        assert_eq!(l.cursor, 2);
        l.key(k(KeyCode::Down), &h);
        assert_eq!(l.value, "dr");
    }

    #[test]
    fn up_with_empty_history_is_a_noop() {
        let h = History::default();
        let mut l = CmdLine::default();
        l.key(k(KeyCode::Up), &h);
        assert_eq!(l.value, "");
    }
}
