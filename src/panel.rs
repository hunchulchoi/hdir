//! Panel state: directory loading, sorting, hidden toggle (pure logic, no UI).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Parent,
    Dir,
    File,
    Symlink { target_is_dir: bool },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: u32,
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnterResult {
    Entered,
    Opened(PathBuf),
    Nothing,
}

#[derive(Debug, Clone)]
pub struct Panel {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub scroll: usize,
    pub show_hidden: bool,
    pub selected: HashSet<String>,
    pub error: Option<String>,
}

impl Panel {
    pub fn new(path: impl Into<PathBuf>) -> Panel {
        let path = path.into();
        let cwd = fs::canonicalize(&path).unwrap_or(path);
        let mut panel = Panel {
            cwd,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            show_hidden: false,
            selected: HashSet::new(),
            error: None,
        };
        panel.reload();
        panel
    }

    /// Re-read `cwd`, keeping the cursor on the same entry name when possible.
    pub fn reload(&mut self) {
        let keep = self.current().map(|e| e.name.clone());
        match load_entries(&self.cwd, self.show_hidden) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err((entries, msg)) => {
                self.entries = entries;
                self.error = Some(msg);
            }
        }
        let found = keep.and_then(|n| self.entries.iter().position(|e| e.name == n));
        self.cursor = found.unwrap_or(self.cursor);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.cursor);
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload();
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    fn last_index(&self) -> usize {
        self.entries.len().saturating_sub(1)
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.cursor = (self.cursor + 1).min(self.last_index());
    }

    pub fn page_up(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_sub(page);
    }

    pub fn page_down(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_add(page).min(self.last_index());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.last_index();
    }

    /// Adjust `scroll` so the cursor lies within the visible window.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
    }

    /// Change directory, reset view state and reload; cursor lands on `focus` if present.
    fn change_dir(&mut self, dir: PathBuf, focus: Option<&str>) {
        self.cwd = dir;
        self.entries.clear();
        self.cursor = 0;
        self.scroll = 0;
        self.selected.clear();
        self.reload();
        if let Some(i) = focus.and_then(|n| self.entries.iter().position(|e| e.name == n)) {
            self.cursor = i;
        }
    }

    /// Go to the parent directory, focusing the directory we came from.
    /// Returns false at the filesystem root.
    pub fn go_parent(&mut self) -> bool {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return false;
        };
        let from = self
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.change_dir(parent, from.as_deref());
        true
    }

    pub fn enter(&mut self) -> EnterResult {
        let Some(entry) = self.current() else {
            return EnterResult::Nothing;
        };
        match entry.kind {
            EntryKind::Parent => {
                if self.go_parent() {
                    EnterResult::Entered
                } else {
                    EnterResult::Nothing
                }
            }
            EntryKind::Dir
            | EntryKind::Symlink {
                target_is_dir: true,
            } => {
                let dir = self.cwd.join(&entry.name);
                self.change_dir(dir, None);
                EnterResult::Entered
            }
            _ => EnterResult::Opened(self.cwd.join(&entry.name)),
        }
    }
}

fn parent_entry() -> Entry {
    Entry {
        name: "..".to_string(),
        kind: EntryKind::Parent,
        size: 0,
        modified: None,
        mode: 0,
        hidden: false,
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0
}

/// Returns entries, or (entries-so-far, error message) when the directory is unreadable.
fn load_entries(cwd: &Path, show_hidden: bool) -> Result<Vec<Entry>, (Vec<Entry>, String)> {
    let has_parent = cwd.parent().is_some();
    let parent = || {
        has_parent
            .then(parent_entry)
            .into_iter()
            .collect::<Vec<_>>()
    };
    let rd = fs::read_dir(cwd).map_err(|e| (parent(), format!("{}: {}", cwd.display(), e)))?;

    let mut items: Vec<Entry> = rd
        .filter_map(|r| r.ok())
        .filter_map(|de| {
            let name = de.file_name().to_string_lossy().into_owned();
            let hidden = name.starts_with('.');
            if hidden && !show_hidden {
                return None;
            }
            let meta = fs::symlink_metadata(de.path()).ok()?;
            let kind = if meta.file_type().is_symlink() {
                let target_is_dir = fs::metadata(de.path()).map(|m| m.is_dir()).unwrap_or(false);
                EntryKind::Symlink { target_is_dir }
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            Some(Entry {
                name,
                kind,
                size: meta.len(),
                modified: meta.modified().ok(),
                mode: mode_of(&meta),
                hidden,
            })
        })
        .collect();

    let is_dirlike = |e: &Entry| {
        matches!(
            e.kind,
            EntryKind::Dir
                | EntryKind::Symlink {
                    target_is_dir: true
                }
        )
    };
    items.sort_by(|a, b| {
        is_dirlike(b)
            .cmp(&is_dirlike(a))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut out = parent();
    out.extend(items);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(p: &Panel) -> Vec<&str> {
        p.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn dirs_before_files_case_insensitive_parent_first() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("b.txt"), "x").unwrap();
        fs::write(t.path().join("A.txt"), "x").unwrap();
        fs::create_dir(t.path().join("zdir")).unwrap();
        fs::create_dir(t.path().join("Adir")).unwrap();
        let p = Panel::new(t.path());
        assert_eq!(names(&p), vec!["..", "Adir", "zdir", "A.txt", "b.txt"]);
        assert_eq!(p.entries[0].kind, EntryKind::Parent);
        assert_eq!(p.entries[1].kind, EntryKind::Dir);
        assert_eq!(p.entries[3].kind, EntryKind::File);
        assert_eq!(p.entries[3].size, 1);
        assert!(p.error.is_none());
    }

    #[test]
    fn no_parent_at_root() {
        let p = Panel::new("/");
        assert!(p.entries.iter().all(|e| e.kind != EntryKind::Parent));
    }

    #[test]
    fn hidden_omitted_then_shown_cursor_preserved() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(".hid"), "").unwrap();
        fs::write(t.path().join("vis"), "").unwrap();
        let mut p = Panel::new(t.path());
        assert_eq!(names(&p), vec!["..", "vis"]);
        p.cursor = 1;
        p.toggle_hidden();
        assert!(p.show_hidden);
        assert_eq!(names(&p), vec!["..", ".hid", "vis"]);
        assert!(p.entries[1].hidden);
        assert_eq!(p.current().unwrap().name, "vis");
        assert_eq!(p.cursor, 2);
        p.toggle_hidden();
        assert_eq!(p.current().unwrap().name, "vis");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_kinds() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("f"), "").unwrap();
        symlink(t.path().join("d"), t.path().join("ld")).unwrap();
        symlink(t.path().join("f"), t.path().join("lf")).unwrap();
        symlink(t.path().join("nope"), t.path().join("lb")).unwrap();
        let p = Panel::new(t.path());
        let kind = |n: &str| p.entries.iter().find(|e| e.name == n).unwrap().kind.clone();
        assert_eq!(
            kind("ld"),
            EntryKind::Symlink {
                target_is_dir: true
            }
        );
        assert_eq!(
            kind("lf"),
            EntryKind::Symlink {
                target_is_dir: false
            }
        );
        assert_eq!(
            kind("lb"),
            EntryKind::Symlink {
                target_is_dir: false
            }
        );
        // symlink-to-dir sorts with directories
        assert_eq!(names(&p), vec!["..", "d", "ld", "f", "lb", "lf"]);
    }

    #[test]
    fn korean_and_space_names() {
        let t = tempfile::tempdir().unwrap();
        for n in ["나무 파일.txt", "가방.txt", "다람쥐.txt", "my file.txt"] {
            fs::write(t.path().join(n), "").unwrap();
        }
        fs::create_dir(t.path().join("한 글")).unwrap();
        let p = Panel::new(t.path());
        assert_eq!(
            names(&p),
            vec![
                "..",
                "한 글",
                "my file.txt",
                "가방.txt",
                "나무 파일.txt",
                "다람쥐.txt"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_sets_error() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("locked");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
        let p = Panel::new(&d);
        let readable = fs::read_dir(&d).is_ok(); // root can read
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            let err = p.error.as_ref().expect("error set");
            assert!(err.contains("ermission denied"), "{err}");
            assert!(err.contains("locked"), "{err}");
            assert_eq!(names(&p), vec![".."]);
        }
    }

    #[test]
    fn reload_keeps_cursor_on_name() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("b"), "").unwrap();
        fs::write(t.path().join("c"), "").unwrap();
        let mut p = Panel::new(t.path());
        p.cursor = 2; // c
        fs::write(t.path().join("a"), "").unwrap();
        p.reload();
        assert_eq!(p.current().unwrap().name, "c");
        assert_eq!(p.cursor, 3);
    }

    #[test]
    fn reload_clamps_when_cursor_entry_deleted() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "").unwrap();
        fs::write(t.path().join("b"), "").unwrap();
        let mut p = Panel::new(t.path());
        p.cursor = 2; // b, last
        fs::remove_file(t.path().join("b")).unwrap();
        p.reload();
        assert_eq!(p.cursor, 1);
        assert_eq!(p.current().unwrap().name, "a");
    }

    fn mk(n: usize) -> (tempfile::TempDir, Panel) {
        let t = tempfile::tempdir().unwrap();
        for i in 0..n {
            fs::write(t.path().join(format!("f{i:02}")), "").unwrap();
        }
        let p = Panel::new(t.path());
        (t, p)
    }

    #[test]
    fn move_up_down_clamp() {
        let (_t, mut p) = mk(2); // "..", f00, f01
        p.move_up();
        assert_eq!(p.cursor, 0);
        p.move_down();
        p.move_down();
        p.move_down();
        assert_eq!(p.cursor, 2);
    }

    #[test]
    fn page_home_end_clamp() {
        let (_t, mut p) = mk(9); // 10 entries
        p.page_down(4);
        assert_eq!(p.cursor, 4);
        p.page_down(100);
        assert_eq!(p.cursor, 9);
        p.page_up(4);
        assert_eq!(p.cursor, 5);
        p.page_up(100);
        assert_eq!(p.cursor, 0);
        p.end();
        assert_eq!(p.cursor, 9);
        p.home();
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn navigation_on_empty_panel_is_noop() {
        let mut p = Panel::new("/");
        p.entries.clear();
        p.cursor = 0;
        p.move_up();
        p.move_down();
        p.page_up(5);
        p.page_down(5);
        p.home();
        p.end();
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn enter_dir_changes_cwd_and_resets() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("sub")).unwrap();
        fs::write(t.path().join("sub/x"), "").unwrap();
        let mut p = Panel::new(t.path());
        p.cursor = 1;
        p.selected.insert("sub".into());
        assert_eq!(p.enter(), EnterResult::Entered);
        assert!(p.cwd.ends_with("sub"));
        assert_eq!(p.cursor, 0);
        assert!(p.selected.is_empty());
        assert_eq!(names(&p), vec!["..", "x"]);
    }

    #[test]
    fn enter_file_opens_without_state_change() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a.txt"), "").unwrap();
        let mut p = Panel::new(t.path());
        let cwd = p.cwd.clone();
        p.cursor = 1;
        assert_eq!(p.enter(), EnterResult::Opened(cwd.join("a.txt")));
        assert_eq!(p.cwd, cwd);
        assert_eq!(p.cursor, 1);
    }

    #[cfg(unix)]
    #[test]
    fn enter_symlink_dir_and_file() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("f"), "").unwrap();
        symlink(t.path().join("d"), t.path().join("ld")).unwrap();
        symlink(t.path().join("f"), t.path().join("lf")).unwrap();
        let mut p = Panel::new(t.path());
        let cwd = p.cwd.clone();
        p.cursor = p.entries.iter().position(|e| e.name == "lf").unwrap();
        assert_eq!(p.enter(), EnterResult::Opened(cwd.join("lf")));
        p.cursor = p.entries.iter().position(|e| e.name == "ld").unwrap();
        assert_eq!(p.enter(), EnterResult::Entered);
        assert_eq!(p.cwd, cwd.join("ld"));
    }

    #[test]
    fn enter_parent_entry_goes_up() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("sub")).unwrap();
        let mut p = Panel::new(t.path().join("sub"));
        assert_eq!(p.current().unwrap().kind, EntryKind::Parent);
        assert_eq!(p.enter(), EnterResult::Entered);
        assert_eq!(p.cwd, t.path().canonicalize().unwrap());
        assert_eq!(p.current().unwrap().name, "sub");
    }

    #[test]
    fn go_parent_restores_cursor_and_stops_at_root() {
        let t = tempfile::tempdir().unwrap();
        for d in ["a", "b", "c"] {
            fs::create_dir(t.path().join(d)).unwrap();
        }
        let mut p = Panel::new(t.path().join("c"));
        p.selected.insert("zz".into());
        assert!(p.go_parent());
        assert_eq!(p.current().unwrap().name, "c");
        assert!(p.selected.is_empty());
        let mut root = Panel::new("/");
        assert!(!root.go_parent());
        assert_eq!(root.cwd, PathBuf::from("/"));
    }

    #[test]
    fn korean_dir_enter_and_go_parent() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("가나 다")).unwrap();
        fs::create_dir(t.path().join("나")).unwrap();
        let mut p = Panel::new(t.path());
        p.cursor = p.entries.iter().position(|e| e.name == "가나 다").unwrap();
        assert_eq!(p.enter(), EnterResult::Entered);
        assert!(p.cwd.ends_with("가나 다"));
        assert!(p.go_parent());
        assert_eq!(p.current().unwrap().name, "가나 다");
    }

    #[cfg(unix)]
    #[test]
    fn enter_unreadable_dir_sets_error_no_panic() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("locked");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
        let mut p = Panel::new(t.path());
        p.cursor = 1;
        let r = p.enter();
        let readable = fs::read_dir(&d).is_ok();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r, EnterResult::Entered);
        assert!(p.cwd.ends_with("locked"));
        if !readable {
            assert!(p.error.is_some());
        }
    }

    #[test]
    fn ensure_visible_scrolls() {
        let (_t, mut p) = mk(19); // 20 entries
        p.cursor = 12;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 8);
        p.cursor = 2;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 2);
        p.cursor = 4;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 2);
        let before = p.scroll;
        p.cursor = 19;
        p.ensure_visible(0);
        assert_eq!(p.scroll, before);
    }

    #[test]
    fn new_canonicalizes_relative_path() {
        let p = Panel::new(".");
        assert_eq!(
            p.cwd,
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert!(p.cwd.is_absolute());
    }
}
