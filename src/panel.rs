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
        let mut panel = Panel {
            cwd: path.into(),
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
}
