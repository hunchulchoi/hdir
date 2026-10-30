//! Panel state: directory loading, sorting, hidden toggle (pure logic, no UI).

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::loader::LoadResult;
use crate::vfs::Location;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Parent,
    Dir,
    File,
    Symlink { target_is_dir: bool },
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Lossy display name.
    pub name: String,
    /// Exact filesystem name; use for all path joins and identity checks.
    pub os_name: OsString,
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

/// Result of starting an async Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncEnter {
    /// A directory load was started; hand the request to the loader.
    Load(LoadRequest),
    Opened(PathBuf),
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadKind {
    Enter,
    Parent,
    Reload,
}

/// An in-flight directory load for a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadRequest {
    pub id: u64,
    pub dir: PathBuf,
    /// Entry to put the cursor on once loaded.
    pub focus: Option<OsString>,
    pub kind: LoadKind,
}

#[derive(Debug, Clone)]
pub struct Panel {
    /// Where `cwd` lives. Remote paths are byte strings stored as `PathBuf`
    /// (unix); never hand them to local filesystem calls.
    pub location: Location,
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub scroll: usize,
    pub show_hidden: bool,
    pub selected: HashSet<OsString>,
    pub error: Option<String>,
    /// Set while a background load is pending; the old listing stays visible.
    pub loading: Option<LoadRequest>,
    /// A task finished while a navigation load was in flight; reload once it lands.
    pub reload_after_load: bool,
}

impl Panel {
    pub fn new(path: impl Into<PathBuf>) -> Panel {
        let path = path.into();
        let cwd = fs::canonicalize(&path).unwrap_or(path);
        let mut panel = Panel {
            location: Location::Local,
            cwd,
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            show_hidden: false,
            selected: HashSet::new(),
            error: None,
            loading: None,
            reload_after_load: false,
        };
        panel.reload();
        panel
    }

    /// Re-read `cwd`, keeping the cursor on the same entry when possible and
    /// pruning selections that no longer exist in the listing.
    pub fn reload(&mut self) {
        self.loading = None;
        let keep = self.current().map(|e| e.os_name.clone());
        match read_listing(&self.cwd, self.show_hidden) {
            Ok(listing) => self.apply_listing(listing),
            Err(msg) => {
                // Keep the last good listing and selection; only report the error.
                if self.entries.is_empty() {
                    self.entries = parent_only(&self.cwd);
                }
                self.error = Some(msg);
                return;
            }
        }
        let found = keep.and_then(|n| self.entries.iter().position(|e| e.os_name == n));
        self.cursor = found.unwrap_or(self.cursor);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.cursor);
        let present: HashSet<&OsStr> = self.entries.iter().map(|e| e.os_name.as_os_str()).collect();
        self.selected.retain(|n| present.contains(n.as_os_str()));
    }

    fn apply_listing(&mut self, (entries, skipped): (Vec<Entry>, usize)) {
        self.entries = entries;
        self.error = (skipped > 0).then(|| format!("{skipped}개 항목을 읽지 못함"));
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

    /// Change directory, reset view state; cursor lands on `focus` if present.
    /// If the target cannot be read, the current location is kept and only
    /// `error` is set.
    fn change_dir(&mut self, dir: PathBuf, focus: Option<&OsStr>) -> bool {
        self.loading = None;
        let listing = match read_listing(&dir, self.show_hidden) {
            Ok(l) => l,
            Err(msg) => {
                self.error = Some(msg);
                return false;
            }
        };
        self.cwd = dir;
        self.cursor = 0;
        self.scroll = 0;
        self.selected.clear();
        self.apply_listing(listing);
        if let Some(i) = focus.and_then(|n| self.entries.iter().position(|e| e.os_name == n)) {
            self.cursor = i;
        }
        true
    }

    /// Register a pending load; a newer request supersedes any older one.
    pub fn begin_change_dir(
        &mut self,
        id: u64,
        dir: PathBuf,
        focus: Option<OsString>,
        kind: LoadKind,
    ) -> LoadRequest {
        let req = LoadRequest {
            id,
            dir,
            focus,
            kind,
        };
        self.loading = Some(req.clone());
        req
    }

    /// Async counterpart of `go_parent`; None at the filesystem root.
    pub fn begin_parent(&mut self, id: u64) -> Option<LoadRequest> {
        let parent = self.cwd.parent().map(Path::to_path_buf)?;
        let from = self.cwd.file_name().map(OsStr::to_os_string);
        Some(self.begin_change_dir(id, parent, from, LoadKind::Parent))
    }

    /// Async counterpart of `reload`. `focus` None keeps the cursor on the
    /// entry under it when the load finishes.
    pub fn begin_reload(&mut self, id: u64, focus: Option<OsString>) -> LoadRequest {
        let dir = self.cwd.clone();
        self.reload_after_load = false;
        self.begin_change_dir(id, dir, focus, LoadKind::Reload)
    }

    pub fn begin_toggle_hidden(&mut self, id: u64) -> LoadRequest {
        self.show_hidden = !self.show_hidden;
        self.begin_reload(id, None)
    }

    /// Async counterpart of `enter`.
    pub fn begin_enter(&mut self, id: u64) -> AsyncEnter {
        let Some(entry) = self.current() else {
            return AsyncEnter::Nothing;
        };
        match entry.kind {
            EntryKind::Parent => match self.begin_parent(id) {
                Some(req) => AsyncEnter::Load(req),
                None => AsyncEnter::Nothing,
            },
            EntryKind::Dir
            | EntryKind::Symlink {
                target_is_dir: true,
            } => {
                let dir = self.cwd.join(&entry.os_name);
                AsyncEnter::Load(self.begin_change_dir(id, dir, None, LoadKind::Enter))
            }
            _ => AsyncEnter::Opened(self.cwd.join(&entry.os_name)),
        }
    }

    /// Apply a finished load. Returns false (and changes nothing) when the
    /// result does not belong to the pending request.
    pub fn finish_load(&mut self, res: LoadResult) -> bool {
        if self.loading.as_ref().map(|r| r.id) != Some(res.id) {
            return false;
        }
        let Some(req) = self.loading.take() else {
            return false;
        };
        let listing = match res.result {
            Ok(l) => l,
            Err(msg) => {
                if self.entries.is_empty() {
                    self.entries = parent_only(&self.cwd);
                }
                self.error = Some(msg);
                return true;
            }
        };
        match req.kind {
            LoadKind::Enter | LoadKind::Parent => {
                self.cwd = req.dir;
                self.cursor = 0;
                self.scroll = 0;
                self.selected.clear();
                self.apply_listing(listing);
                if let Some(i) = req
                    .focus
                    .and_then(|n| self.entries.iter().position(|e| e.os_name == n))
                {
                    self.cursor = i;
                }
            }
            LoadKind::Reload => {
                let keep = req
                    .focus
                    .or_else(|| self.current().map(|e| e.os_name.clone()));
                self.apply_listing(listing);
                let found = keep.and_then(|n| self.entries.iter().position(|e| e.os_name == n));
                self.cursor = found.unwrap_or(self.cursor);
                self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
                self.scroll = self.scroll.min(self.cursor);
                let present: HashSet<&OsStr> =
                    self.entries.iter().map(|e| e.os_name.as_os_str()).collect();
                self.selected.retain(|n| present.contains(n.as_os_str()));
            }
        }
        true
    }

    /// Go to the parent directory, focusing the directory we came from.
    /// Returns false at the filesystem root or when the parent can't be read.
    pub fn go_parent(&mut self) -> bool {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return false;
        };
        let from = self.cwd.file_name().map(OsStr::to_os_string);
        self.change_dir(parent, from.as_deref())
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
                let dir = self.cwd.join(&entry.os_name);
                if self.change_dir(dir, None) {
                    EnterResult::Entered
                } else {
                    EnterResult::Nothing
                }
            }
            _ => EnterResult::Opened(self.cwd.join(&entry.os_name)),
        }
    }
}

fn parent_entry() -> Entry {
    Entry {
        name: "..".to_string(),
        os_name: "..".into(),
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

/// Windows has only the readonly attribute: report 0o444 if readonly, else 0o666.
#[cfg(windows)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(not(any(unix, windows)))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0
}

/// Hidden by the platform: a leading dot everywhere, plus the hidden attribute on Windows.
fn is_hidden(name: &str, meta: &fs::Metadata) -> bool {
    name.starts_with('.') || hidden_attr(meta)
}

#[cfg(windows)]
fn hidden_attr(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn hidden_attr(_meta: &fs::Metadata) -> bool {
    false
}

fn parent_only(cwd: &Path) -> Vec<Entry> {
    cwd.parent().map(|_| parent_entry()).into_iter().collect()
}

/// Entries (with `..` first) plus the number of items skipped due to errors,
/// or an error message when the directory itself is unreadable.
pub fn read_listing(cwd: &Path, show_hidden: bool) -> Result<(Vec<Entry>, usize), String> {
    let rd = fs::read_dir(cwd).map_err(|e| format!("{}: {}", cwd.display(), e))?;
    let names = rd.map(|r| r.map(|de| de.file_name()));
    Ok(build_listing(cwd, names, show_hidden))
}

/// Build the sorted listing from directory item names; items that errored
/// (iteration or metadata) are counted, not shown.
fn build_listing(
    cwd: &Path,
    names: impl Iterator<Item = io::Result<OsString>>,
    show_hidden: bool,
) -> (Vec<Entry>, usize) {
    build_listing_with(cwd, names, show_hidden, &|p| fs::symlink_metadata(p))
}

/// [`build_listing`] with an injectable `lstat`.
fn build_listing_with(
    cwd: &Path,
    names: impl Iterator<Item = io::Result<OsString>>,
    show_hidden: bool,
    stat: &dyn Fn(&Path) -> io::Result<fs::Metadata>,
) -> (Vec<Entry>, usize) {
    let mut skipped = 0;
    let mut items: Vec<Entry> = Vec::new();
    for r in names {
        let Ok(os_name) = r else {
            skipped += 1;
            continue;
        };
        let name = os_name.to_string_lossy().into_owned();
        // Dot-prefixed names are hidden everywhere: skip them before any stat.
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = cwd.join(&os_name);
        let Ok(meta) = stat(&path) else {
            // Hidden entries stay out of the error count when hidden are not shown.
            if !name.starts_with('.') || show_hidden {
                skipped += 1;
            }
            continue;
        };
        let hidden = is_hidden(&name, &meta);
        if hidden && !show_hidden {
            continue;
        }
        let kind = if meta.file_type().is_symlink() {
            let target_is_dir = fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
            EntryKind::Symlink { target_is_dir }
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        items.push(Entry {
            name,
            os_name,
            kind,
            size: meta.len(),
            modified: meta.modified().ok(),
            mode: mode_of(&meta),
            hidden,
        });
    }

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
            .then_with(|| a.os_name.cmp(&b.os_name))
    });

    let mut out = parent_only(cwd);
    out.extend(items);
    (out, skipped)
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
        assert!(parent_only(Path::new("/")).is_empty());
        assert_eq!(parent_only(Path::new("/tmp")).len(), 1);
    }

    #[test]
    fn enter_vanished_dir_fails_and_keeps_location() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("gone")).unwrap();
        let mut p = Panel::new(t.path());
        let cwd = p.cwd.clone();
        p.cursor = 1;
        fs::remove_dir(t.path().join("gone")).unwrap();
        assert_eq!(p.enter(), EnterResult::Nothing);
        assert_eq!(p.cwd, cwd);
        assert!(p.error.is_some());
    }

    #[test]
    fn reload_failure_keeps_entries_and_selection() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a"), "").unwrap();
        let mut p = Panel::new(&dir);
        p.selected.insert("a".into());
        fs::remove_file(dir.join("a")).unwrap();
        fs::remove_dir(&dir).unwrap();
        p.reload();
        assert!(p.error.is_some());
        assert_eq!(names(&p), vec!["..", "a"]);
        assert!(p.selected.contains(OsStr::new("a")));
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
        let top = if cfg!(windows) { r"C:\" } else { "/" };
        let mut root = Panel::new(top);
        assert!(!root.go_parent());
        assert_eq!(root.cwd, PathBuf::from(top).canonicalize().unwrap());
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
    fn enter_unreadable_dir_keeps_location_and_sets_error() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("locked");
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
        let mut p = Panel::new(t.path());
        let cwd = p.cwd.clone();
        p.cursor = 1;
        p.selected.insert("keep".into());
        let r = p.enter();
        let readable = fs::read_dir(&d).is_ok();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        if !readable {
            assert_eq!(r, EnterResult::Nothing);
            assert_eq!(p.cwd, cwd);
            assert_eq!(names(&p), vec!["..", "locked"]);
            assert_eq!(p.cursor, 1);
            assert_eq!(p.selected.len(), 1);
            assert!(p.error.is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_load_and_enter() {
        use std::os::unix::ffi::OsStrExt;
        let t = tempfile::tempdir().unwrap();
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff");
        if fs::create_dir(t.path().join(bad)).is_err() {
            return; // filesystem (e.g. APFS) rejects non-UTF-8 names
        }
        fs::write(t.path().join(bad).join("in"), "").unwrap();
        fs::write(t.path().join(std::ffi::OsStr::from_bytes(b"f\xfe")), "").unwrap();
        let mut p = Panel::new(t.path());
        assert!(p.error.is_none());
        assert_eq!(p.entries.len(), 3);
        p.cursor = p
            .entries
            .iter()
            .position(|e| e.kind == EntryKind::Dir)
            .unwrap();
        assert_eq!(p.enter(), EnterResult::Entered);
        assert!(p.error.is_none(), "{:?}", p.error);
        assert!(p.cwd.ends_with(bad));
        assert_eq!(names(&p), vec!["..", "in"]);
        assert!(p.go_parent());
        assert_eq!(p.current().unwrap().kind, EntryKind::Dir);
        assert!(p.current().unwrap().name.starts_with("bad"));
        // file opens with exact path
        p.cursor = p.entries.len() - 1;
        let EnterResult::Opened(path) = p.enter() else {
            panic!("expected Opened")
        };
        assert!(path.exists());
    }

    #[test]
    fn paths_use_os_name_not_display_name() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("real")).unwrap();
        fs::write(t.path().join("realf"), "").unwrap();
        let mut p = Panel::new(t.path());
        for e in &mut p.entries {
            e.name = format!("shown-{}", e.name);
        }
        let i = p.entries.iter().position(|e| e.os_name == "realf").unwrap();
        p.cursor = i;
        assert_eq!(p.enter(), EnterResult::Opened(p.cwd.join("realf")));
        p.cursor = p.entries.iter().position(|e| e.os_name == "real").unwrap();
        assert_eq!(p.enter(), EnterResult::Entered);
        assert!(p.cwd.ends_with("real"));
        assert!(p.error.is_none());
    }

    #[test]
    fn partial_read_errors_are_counted() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("ok"), "").unwrap();
        let items: Vec<io::Result<OsString>> = vec![
            Ok("ok".into()),
            Err(io::Error::other("iter fail")),
            Ok("vanished".into()), // metadata fails: no such file
        ];
        let (entries, skipped) = build_listing(t.path(), items.into_iter(), false);
        assert_eq!(skipped, 2);
        let n: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(n, vec!["..", "ok"]);
    }

    #[test]
    fn hidden_dot_names_are_not_statted_when_hidden_are_off() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(".h"), "").unwrap();
        fs::write(t.path().join("v"), "").unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let stat = |p: &Path| {
            calls.borrow_mut().push(p.to_path_buf());
            fs::symlink_metadata(p)
        };
        let items = || vec![Ok(OsString::from(".h")), Ok(OsString::from("v"))].into_iter();
        let (entries, _) = build_listing_with(t.path(), items(), false, &stat);
        assert_eq!(*calls.borrow(), vec![t.path().join("v")]);
        assert!(entries.iter().all(|e| e.name != ".h"));
        calls.borrow_mut().clear();
        let (entries, _) = build_listing_with(t.path(), items(), true, &stat);
        assert_eq!(calls.borrow().len(), 2);
        assert!(entries.iter().any(|e| e.name == ".h"));
    }

    #[test]
    fn apply_listing_sets_partial_error_message() {
        let t = tempfile::tempdir().unwrap();
        let mut p = Panel::new(t.path());
        p.apply_listing((Vec::new(), 3));
        assert_eq!(p.error.as_deref(), Some("3개 항목을 읽지 못함"));
        p.apply_listing((Vec::new(), 0));
        assert!(p.error.is_none());
    }

    #[test]
    fn toggle_hidden_prunes_hidden_selection() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(".hid"), "").unwrap();
        fs::write(t.path().join("vis"), "").unwrap();
        let mut p = Panel::new(t.path());
        p.toggle_hidden();
        p.selected.insert(".hid".into());
        p.toggle_hidden();
        assert!(p.selected.is_empty());
    }

    #[test]
    fn reload_prunes_deleted_selection_keeps_existing() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "").unwrap();
        fs::write(t.path().join("b"), "").unwrap();
        let mut p = Panel::new(t.path());
        p.selected.insert("a".into());
        p.selected.insert("b".into());
        fs::remove_file(t.path().join("a")).unwrap();
        p.reload();
        assert_eq!(p.selected.len(), 1);
        assert!(p.selected.iter().all(|n| n == "b"));
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

    // ---- async loading ----

    fn load_of(req: &LoadRequest, show_hidden: bool) -> crate::loader::LoadResult {
        crate::loader::LoadResult {
            id: req.id,
            dir: req.dir.clone(),
            result: read_listing(&req.dir, show_hidden),
        }
    }

    fn started(r: AsyncEnter) -> LoadRequest {
        match r {
            AsyncEnter::Load(req) => req,
            other => panic!("expected Load, got {other:?}"),
        }
    }

    fn tree() -> (tempfile::TempDir, Panel) {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("adir")).unwrap();
        fs::create_dir(t.path().join("bdir")).unwrap();
        fs::write(t.path().join("adir/inner"), "").unwrap();
        fs::write(t.path().join("f"), "").unwrap();
        let p = Panel::new(t.path());
        (t, p)
    }

    #[test]
    fn begin_enter_does_not_change_location_until_finish() {
        let (_t, mut p) = tree();
        let old = p.cwd.clone();
        p.selected.insert("f".into());
        p.cursor = 1;
        let req = started(p.begin_enter(5));
        assert_eq!(req.id, 5);
        assert_eq!(req.kind, LoadKind::Enter);
        assert_eq!(req.dir, old.join("adir"));
        assert_eq!(p.cwd, old);
        assert_eq!(p.loading.as_ref().unwrap().id, 5);
        assert!(p.finish_load(load_of(&req, false)));
        assert_eq!(p.cwd, old.join("adir"));
        assert_eq!(p.cursor, 0);
        assert!(p.selected.is_empty());
        assert!(p.loading.is_none());
        assert_eq!(names(&p), vec!["..", "inner"]);
    }

    #[test]
    fn stale_result_is_ignored() {
        let (_t, mut p) = tree();
        let old = p.cwd.clone();
        p.cursor = 1;
        let first = started(p.begin_enter(1));
        p.cursor = 2;
        let second = started(p.begin_enter(2));
        assert!(!p.finish_load(load_of(&first, false)));
        assert_eq!(p.cwd, old);
        assert_eq!(p.loading.as_ref().unwrap().id, 2);
        assert!(p.finish_load(load_of(&second, false)));
        assert_eq!(p.cwd, old.join("bdir"));
    }

    #[test]
    fn result_without_request_is_ignored() {
        let (_t, mut p) = tree();
        let req = LoadRequest {
            id: 9,
            dir: p.cwd.join("adir"),
            focus: None,
            kind: LoadKind::Enter,
        };
        let old = p.cwd.clone();
        assert!(!p.finish_load(load_of(&req, false)));
        assert_eq!(p.cwd, old);
    }

    #[test]
    fn begin_enter_file_opens_and_parent_at_root_is_nothing() {
        let (t, mut p) = tree();
        p.cursor = 3;
        assert_eq!(p.begin_enter(1), AsyncEnter::Opened(p.cwd.join("f")));
        assert!(p.loading.is_none());
        let _ = t;
        let mut root = Panel::new("/");
        assert!(root.begin_parent(1).is_none());
    }

    #[test]
    fn finish_load_failure_keeps_location_and_sets_error() {
        let (_t, mut p) = tree();
        let old = p.cwd.clone();
        p.cursor = 1;
        p.selected.insert("f".into());
        let req = started(p.begin_enter(1));
        fs::remove_dir_all(old.join("adir")).unwrap();
        assert!(p.finish_load(load_of(&req, false)));
        assert_eq!(p.cwd, old);
        assert!(p.error.is_some());
        assert!(p.selected.contains(OsStr::new("f")));
        assert!(p.loading.is_none());
        assert!(names(&p).contains(&"bdir"));
    }

    #[test]
    fn parent_load_restores_focus() {
        let (_t, mut p) = tree();
        let root = p.cwd.clone();
        p.cursor = 2;
        let req = started(p.begin_enter(1));
        p.finish_load(load_of(&req, false));
        let req = p.begin_parent(2).unwrap();
        assert_eq!(req.kind, LoadKind::Parent);
        assert_eq!(req.focus.as_deref(), Some(OsStr::new("bdir")));
        assert!(p.finish_load(load_of(&req, false)));
        assert_eq!(p.cwd, root);
        assert_eq!(p.current().unwrap().name, "bdir");
    }

    #[test]
    fn reload_load_keeps_cursor_by_name_and_prunes_selection() {
        let (t, mut p) = tree();
        p.cursor = 3;
        p.selected.insert("f".into());
        p.selected.insert("adir".into());
        let req = p.begin_reload(1, None);
        assert_eq!(req.kind, LoadKind::Reload);
        fs::create_dir(t.path().join("0new")).unwrap();
        fs::remove_dir_all(t.path().join("adir")).unwrap();
        assert!(p.finish_load(load_of(&req, false)));
        assert_eq!(p.current().unwrap().name, "f");
        assert!(p.selected.contains(OsStr::new("f")));
        assert!(!p.selected.contains(OsStr::new("adir")));
    }

    #[test]
    fn reload_load_focus_override_and_failure_keeps_listing() {
        let (t, mut p) = tree();
        let req = p.begin_reload(1, Some("bdir".into()));
        assert!(p.finish_load(load_of(&req, false)));
        assert_eq!(p.current().unwrap().name, "bdir");
        let req = p.begin_reload(2, None);
        let cwd = p.cwd.clone();
        fs::remove_dir_all(&cwd).unwrap();
        assert!(p.finish_load(load_of(&req, false)));
        assert!(p.error.is_some());
        assert!(names(&p).contains(&"f"));
        let _ = t;
    }

    #[test]
    fn toggle_hidden_load_shows_hidden() {
        let (t, mut p) = tree();
        fs::write(t.path().join(".hid"), "").unwrap();
        let req = p.begin_toggle_hidden(1);
        assert!(p.show_hidden);
        assert!(p.finish_load(load_of(&req, true)));
        assert!(names(&p).contains(&".hid"));
    }

    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)] // Windows-only readonly attribute
    #[test]
    fn readonly_attribute_maps_to_mode_444_else_666() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("rw.txt"), "x").unwrap();
        fs::write(t.path().join("ro.txt"), "x").unwrap();
        let mut perm = fs::metadata(t.path().join("ro.txt")).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(t.path().join("ro.txt"), perm).unwrap();
        let p = Panel::new(t.path());
        let mode = |n: &str| p.entries.iter().find(|e| e.name == n).unwrap().mode;
        assert_eq!(mode("ro.txt"), 0o444);
        assert_eq!(mode("rw.txt"), 0o666);
        let mut perm = fs::metadata(t.path().join("ro.txt")).unwrap().permissions();
        perm.set_readonly(false);
        fs::set_permissions(t.path().join("ro.txt"), perm).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn hidden_attribute_hides_entries_like_dotfiles() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("plain.txt"), "x").unwrap();
        fs::write(t.path().join("secret.txt"), "x").unwrap();
        fs::write(t.path().join(".dot"), "x").unwrap();
        let st = std::process::Command::new("attrib")
            .arg("+h")
            .arg(t.path().join("secret.txt"))
            .status()
            .unwrap();
        assert!(st.success());
        let (shown, _) = read_listing(t.path(), false).unwrap();
        let shown: Vec<_> = shown.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(shown, vec!["..", "plain.txt"]);
        let (all, _) = read_listing(t.path(), true).unwrap();
        let hidden = |n: &str| all.iter().find(|e| e.name == n).unwrap().hidden;
        assert!(hidden("secret.txt") && hidden(".dot") && !hidden("plain.txt"));
    }
}
