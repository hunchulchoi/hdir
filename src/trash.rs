//! Trash support built only on the operating system's own trash.
//!
//! hdir keeps no trash store of its own: F8 hands paths to the `trash` crate
//! (macOS: Finder, so "Put Back" works), and on Linux the freedesktop trash is
//! listed / restored / purged through `trash::os_limited`. Everything sits
//! behind [`TrashBackend`] so the app logic can be tested with a fake.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use trash::TrashItem;

/// One item in the OS trash: the crate's [`TrashItem`] plus display extras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub item: TrashItem,
    pub size: Option<u64>,
    pub is_dir: bool,
}

impl TrashEntry {
    pub fn new(item: TrashItem, size: Option<u64>, is_dir: bool) -> Self {
        Self { item, size, is_dir }
    }

    /// Backend identifier (Linux: the `.trashinfo` path).
    pub fn id(&self) -> &OsStr {
        &self.item.id
    }

    /// Original file name, lossy UTF-8 (display only).
    pub fn name(&self) -> String {
        self.item.name.to_string_lossy().into_owned()
    }

    /// Original parent directory; `None` when the OS did not record one.
    pub fn original_parent(&self) -> Option<&Path> {
        let p = self.item.original_parent.as_path();
        p.is_absolute().then_some(p)
    }

    /// Full original path; `None` when unknown (cannot be restored).
    pub fn original_path(&self) -> Option<PathBuf> {
        self.original_parent().map(|p| p.join(&self.item.name))
    }

    /// Unix seconds; `None` when the OS gave no usable time.
    pub fn deleted_at(&self) -> Option<i64> {
        (self.item.time_deleted > 0).then_some(self.item.time_deleted)
    }
}

/// Outcome of a restore: nothing is ever overwritten.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreResult {
    /// Original paths that are back in place.
    pub restored: Vec<PathBuf>,
    /// Original paths skipped because something with that name exists.
    pub collisions: Vec<PathBuf>,
    pub errors: Vec<String>,
}

pub const COLLISION_MSG: &str = "원래 위치에 같은 이름이 있어 건너뜀";

impl RestoreResult {
    /// One status line, e.g. `복원: 2개` or
    /// `복원: 1개 · 원래 위치에 같은 이름이 있어 건너뜀: /a/b`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.restored.is_empty() && (!self.collisions.is_empty() || !self.errors.is_empty()) {
            // nothing restored: only the problems matter
        } else {
            parts.push(format!("복원: {}개", self.restored.len()));
        }
        if let Some(first) = self.collisions.first() {
            let mut s = format!("{COLLISION_MSG}: {}", first.to_string_lossy());
            if self.collisions.len() > 1 {
                s.push_str(&format!(" 외 {}개", self.collisions.len() - 1));
            }
            parts.push(s);
        }
        if let Some(first) = self.errors.first() {
            parts.push(format!("실패 {}개: {first}", self.errors.len()));
        }
        parts.join(" · ")
    }
}

/// Result of a verified purge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub purged: usize,
    /// Selected items that were no longer in the trash.
    pub skipped: usize,
    pub error: Option<String>,
}

/// The OS trash as the app sees it.
pub trait TrashBackend: Send + Sync {
    /// Move `paths` to the OS trash; one result per path.
    fn delete(&self, paths: &[PathBuf]) -> Vec<(PathBuf, Result<(), String>)>;
    /// Newest first. Only meaningful when [`supports_view`](Self::supports_view).
    fn list(&self) -> Result<Vec<TrashEntry>, String>;
    /// Restore to the original paths; never overwrites.
    fn restore(&self, items: &[TrashEntry]) -> RestoreResult;
    fn purge(&self, items: &[TrashEntry]) -> Result<(), String>;
    /// Show the OS trash in the system file manager (macOS: Finder).
    fn open_os_trash(&self) -> Result<(), String>;
    /// Whether the in-panel trash view (list / restore / purge) works.
    fn supports_view(&self) -> bool;
}

// ---- pure helpers ----

/// Newest first; ties keep their order.
pub fn sort_newest_first(entries: &mut [TrashEntry]) {
    entries.sort_by_key(|e| std::cmp::Reverse(e.item.time_deleted));
}

/// Run `restore_all`, retrying without each colliding item so one collision
/// does not block the rest. Items sharing an original path are collisions too.
pub fn restore_with<F>(items: Vec<TrashItem>, mut restore_all: F) -> RestoreResult
where
    F: FnMut(Vec<TrashItem>) -> Result<(), trash::Error>,
{
    let mut res = RestoreResult::default();
    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    for item in items {
        if seen.insert(item.original_path()) {
            pending.push(item);
        } else {
            res.collisions.push(item.original_path());
        }
    }
    while !pending.is_empty() {
        match restore_all(pending.clone()) {
            Ok(()) => {
                res.restored
                    .extend(pending.iter().map(TrashItem::original_path));
                break;
            }
            Err(trash::Error::RestoreCollision {
                path,
                remaining_items,
            }) => {
                let left: HashSet<&OsString> = remaining_items.iter().map(|i| &i.id).collect();
                res.restored.extend(
                    pending
                        .iter()
                        .filter(|i| !left.contains(&i.id))
                        .map(TrashItem::original_path),
                );
                let next: Vec<TrashItem> = remaining_items
                    .iter()
                    .filter(|i| i.original_path() != path)
                    .cloned()
                    .collect();
                res.collisions.push(path);
                if next.len() == pending.len() {
                    res.errors.push("복원 진행 불가".into());
                    break;
                }
                pending = next;
            }
            Err(e) => {
                res.errors.push(e.to_string());
                break;
            }
        }
    }
    res
}

/// Pick, from `fresh`, the entries of `wanted` that are still listed (same
/// id); the count of vanished ones is returned too.
pub fn match_fresh(wanted: &[TrashEntry], fresh: &[TrashEntry]) -> (Vec<TrashEntry>, usize) {
    let mut keep = Vec::new();
    let mut skipped = 0;
    for w in wanted {
        match fresh.iter().find(|f| f.id() == w.id()) {
            Some(f) => keep.push(f.clone()),
            None => skipped += 1,
        }
    }
    (keep, skipped)
}

/// Entries of `list` that are `paths` trashed at or after `since` (newest per
/// path), for undoing an F8.
pub fn find_trashed(list: &[TrashEntry], paths: &[PathBuf], since: i64) -> Vec<TrashEntry> {
    paths
        .iter()
        .filter_map(|p| {
            list.iter()
                .filter(|e| e.item.time_deleted >= since && e.original_path().as_ref() == Some(p))
                .max_by_key(|e| e.item.time_deleted)
                .cloned()
        })
        .collect()
}

/// Trash `paths`: one bulk call, and when it fails, each path that is still
/// there is retried alone so partial failures are reported per path.
pub fn delete_with(
    paths: &[PathBuf],
    bulk: impl FnOnce(&[PathBuf]) -> Result<(), String>,
    single: impl Fn(&Path) -> Result<(), String>,
) -> Vec<(PathBuf, Result<(), String>)> {
    if paths.len() > 1 && bulk(paths).is_ok() {
        return paths.iter().map(|p| (p.clone(), Ok(()))).collect();
    }
    paths
        .iter()
        .map(|p| {
            let gone = paths.len() > 1 && p.symlink_metadata().is_err();
            let r = if gone { Ok(()) } else { single(p) };
            (p.clone(), r)
        })
        .collect()
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---- the real thing ----

/// The OS trash through the `trash` crate.
pub struct SystemTrash;

pub fn system_store() -> Arc<dyn TrashBackend> {
    Arc::new(SystemTrash)
}

impl TrashBackend for SystemTrash {
    fn delete(&self, paths: &[PathBuf]) -> Vec<(PathBuf, Result<(), String>)> {
        delete_with(
            paths,
            |ps| trash::delete_all(ps).map_err(|e| e.to_string()),
            |p| trash::delete(p).map_err(|e| e.to_string()),
        )
    }

    #[cfg(target_os = "linux")]
    fn list(&self) -> Result<Vec<TrashEntry>, String> {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        let mut entries: Vec<TrashEntry> = items
            .into_iter()
            .map(|item| {
                let meta = trash::os_limited::metadata(&item).ok();
                let is_dir = matches!(
                    meta.as_ref().map(|m| &m.size),
                    Some(trash::TrashItemSize::Entries(_))
                );
                let size = meta.and_then(|m| m.size.size());
                TrashEntry::new(item, size, is_dir)
            })
            .collect();
        sort_newest_first(&mut entries);
        Ok(entries)
    }

    #[cfg(not(target_os = "linux"))]
    fn list(&self) -> Result<Vec<TrashEntry>, String> {
        Err("이 OS는 휴지통 목록을 지원하지 않음".into())
    }

    #[cfg(target_os = "linux")]
    fn restore(&self, items: &[TrashEntry]) -> RestoreResult {
        restore_with(items.iter().map(|e| e.item.clone()).collect(), |v| {
            trash::os_limited::restore_all(v)
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn restore(&self, _items: &[TrashEntry]) -> RestoreResult {
        RestoreResult {
            errors: vec!["이 OS는 복원을 지원하지 않음".into()],
            ..Default::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn purge(&self, items: &[TrashEntry]) -> Result<(), String> {
        trash::os_limited::purge_all(items.iter().map(|e| &e.item)).map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "linux"))]
    fn purge(&self, _items: &[TrashEntry]) -> Result<(), String> {
        Err("이 OS는 영구 삭제를 지원하지 않음".into())
    }

    #[cfg(target_os = "macos")]
    fn open_os_trash(&self) -> Result<(), String> {
        let dir = dirs::home_dir()
            .ok_or("홈 디렉터리를 찾을 수 없음")?
            .join(".Trash");
        let status = std::process::Command::new("open")
            .arg(dir)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("open 실패: {status}"))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn open_os_trash(&self) -> Result<(), String> {
        Err("이 OS는 휴지통 열기를 지원하지 않음".into())
    }

    fn supports_view(&self) -> bool {
        cfg!(target_os = "linux")
    }
}

// ---- local time ----

pub fn fmt_datetime(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> String {
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// `YYYY-MM-DD HH:MM` in local time.
pub fn format_local(ts: i64) -> String {
    let t = ts as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    if !ok {
        return "-".to_string();
    }
    fmt_datetime(
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

// ---- view state ----

#[derive(Debug, Clone)]
pub struct TrashView {
    pub entries: Vec<TrashEntry>,
    pub cursor: usize,
    pub scroll: usize,
    pub selected: HashSet<OsString>,
    pub prev_cwd: PathBuf,
    pub loading: bool,
    pub error: Option<String>,
}

impl TrashView {
    pub fn new(prev_cwd: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            scroll: 0,
            selected: HashSet::new(),
            prev_cwd,
            loading: true,
            error: None,
        }
    }

    /// Replace the listing, keeping the cursor on the same entry when it
    /// still exists and pruning vanished selections.
    pub fn set_entries(&mut self, entries: Vec<TrashEntry>) {
        let keep = self.current().map(|e| e.id().to_os_string());
        self.entries = entries;
        if let Some(i) = keep.and_then(|id| self.entries.iter().position(|e| e.id() == id)) {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(self.last_index());
        let ids: HashSet<&OsStr> = self.entries.iter().map(|e| e.id()).collect();
        self.selected.retain(|id| ids.contains(id.as_os_str()));
        self.loading = false;
        self.error = None;
    }

    fn last_index(&self) -> usize {
        self.entries.len().saturating_sub(1)
    }

    pub fn current(&self) -> Option<&TrashEntry> {
        self.entries.get(self.cursor)
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

    /// Toggle selection of the current entry and move down.
    pub fn toggle_current(&mut self) {
        if let Some(id) = self.current().map(|e| e.id().to_os_string())
            && !self.selected.remove(&id)
        {
            self.selected.insert(id);
        }
        self.move_down();
    }

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

    /// Selected entries in list order, else the current entry.
    pub fn targets(&self) -> Vec<TrashEntry> {
        if self.selected.is_empty() {
            return self.current().cloned().into_iter().collect();
        }
        self.entries
            .iter()
            .filter(|e| self.selected.contains(e.id()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
pub use fake::FakeTrash;

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

    /// Functional fake backed by a temp directory: `delete` really moves the
    /// file away, `restore` moves it back (never overwriting).
    pub struct FakeTrash {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        view: bool,
        next: AtomicI64,
        last_time: AtomicI64,
        items: Mutex<Vec<(TrashEntry, PathBuf)>>,
        /// Every path handed to `delete`.
        pub trashed: Mutex<Vec<PathBuf>>,
        /// While set and false, `delete` blocks (up to 2 s).
        pub gate: Mutex<Option<Arc<AtomicBool>>>,
        pub fail_paths: Mutex<HashMap<PathBuf, String>>,
        pub fail_list: Mutex<Option<String>>,
        /// Original paths of each `restore` call.
        pub restore_calls: Mutex<Vec<Vec<PathBuf>>>,
        /// Ids of each `purge` call.
        pub purge_calls: Mutex<Vec<Vec<OsString>>>,
        pub list_calls: AtomicUsize,
        pub open_calls: AtomicUsize,
    }

    impl FakeTrash {
        /// Linux-like: the in-panel view works.
        pub fn new() -> Self {
            Self::build(true)
        }

        /// macOS-like: no view, only "open the OS trash".
        pub fn mac() -> Self {
            Self::build(false)
        }

        fn build(view: bool) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("Trash");
            fs::create_dir_all(&dir).unwrap();
            Self {
                _tmp: tmp,
                dir,
                view,
                next: AtomicI64::new(1),
                last_time: AtomicI64::new(0),
                items: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
                fail_paths: Mutex::new(HashMap::new()),
                fail_list: Mutex::new(None),
                restore_calls: Mutex::new(Vec::new()),
                purge_calls: Mutex::new(Vec::new()),
                list_calls: AtomicUsize::new(0),
                open_calls: AtomicUsize::new(0),
            }
        }

        /// An item whose original location is unknown.
        pub fn add_orphan(&self, name: &str) {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let stored = self.dir.join(format!("orphan{n}"));
            fs::write(&stored, "o").unwrap();
            let item = TrashItem {
                id: format!("id{n}").into(),
                name: name.into(),
                original_parent: PathBuf::new(),
                time_deleted: now_secs(),
            };
            self.items
                .lock()
                .unwrap()
                .push((TrashEntry::new(item, Some(1), false), stored));
        }

        pub fn count(&self) -> usize {
            self.items.lock().unwrap().len()
        }

        pub fn names(&self) -> Vec<String> {
            let mut v: Vec<_> = self
                .items
                .lock()
                .unwrap()
                .iter()
                .map(|(e, _)| e.clone())
                .collect();
            sort_newest_first(&mut v);
            v.iter().map(TrashEntry::name).collect()
        }
    }

    impl Default for FakeTrash {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TrashBackend for FakeTrash {
        fn delete(&self, paths: &[PathBuf]) -> Vec<(PathBuf, Result<(), String>)> {
            let gate = self.gate.lock().unwrap().clone();
            if let Some(g) = gate {
                let start = std::time::Instant::now();
                while !g.load(Ordering::SeqCst) && start.elapsed().as_secs() < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            self.trashed.lock().unwrap().extend(paths.iter().cloned());
            paths
                .iter()
                .map(|p| {
                    if let Some(msg) = self.fail_paths.lock().unwrap().get(p) {
                        return (p.clone(), Err(msg.clone()));
                    }
                    let n = self.next.fetch_add(1, Ordering::SeqCst);
                    let stored = self.dir.join(format!("item{n}"));
                    if let Err(e) = fs::rename(p, &stored) {
                        return (p.clone(), Err(e.to_string()));
                    }
                    // Strictly increasing so "newest first" is deterministic.
                    let t = now_secs().max(self.last_time.load(Ordering::SeqCst) + 1);
                    self.last_time.store(t, Ordering::SeqCst);
                    let item = TrashItem {
                        id: format!("id{n}").into(),
                        name: p.file_name().unwrap().to_os_string(),
                        original_parent: p.parent().unwrap().to_path_buf(),
                        time_deleted: t,
                    };
                    let is_dir = stored.is_dir();
                    self.items
                        .lock()
                        .unwrap()
                        .push((TrashEntry::new(item, Some(1), is_dir), stored));
                    (p.clone(), Ok(()))
                })
                .collect()
        }

        fn list(&self) -> Result<Vec<TrashEntry>, String> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(m) = self.fail_list.lock().unwrap().clone() {
                return Err(m);
            }
            let mut v: Vec<_> = self
                .items
                .lock()
                .unwrap()
                .iter()
                .map(|(e, _)| e.clone())
                .collect();
            sort_newest_first(&mut v);
            Ok(v)
        }

        fn restore(&self, items: &[TrashEntry]) -> RestoreResult {
            self.restore_calls
                .lock()
                .unwrap()
                .push(items.iter().filter_map(TrashEntry::original_path).collect());
            let mut res = RestoreResult::default();
            let mut store = self.items.lock().unwrap();
            for it in items {
                let Some(dest) = it.original_path() else {
                    res.errors.push(format!("{}: 원래 위치 모름", it.name()));
                    continue;
                };
                let Some(pos) = store.iter().position(|(e, _)| e.id() == it.id()) else {
                    res.errors.push(format!("{}: 휴지통에 없음", it.name()));
                    continue;
                };
                if dest.symlink_metadata().is_ok() {
                    res.collisions.push(dest);
                    continue;
                }
                match fs::rename(&store[pos].1, &dest) {
                    Ok(()) => {
                        store.remove(pos);
                        res.restored.push(dest);
                    }
                    Err(e) => res.errors.push(e.to_string()),
                }
            }
            res
        }

        fn purge(&self, items: &[TrashEntry]) -> Result<(), String> {
            self.purge_calls
                .lock()
                .unwrap()
                .push(items.iter().map(|e| e.id().to_os_string()).collect());
            let mut store = self.items.lock().unwrap();
            for it in items {
                if let Some(pos) = store.iter().position(|(e, _)| e.id() == it.id()) {
                    let (_, stored) = store.remove(pos);
                    let _ = if stored.is_dir() {
                        fs::remove_dir_all(stored)
                    } else {
                        fs::remove_file(stored)
                    };
                }
            }
            Ok(())
        }

        fn open_os_trash(&self) -> Result<(), String> {
            self.open_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn supports_view(&self) -> bool {
            self.view
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn item(id: &str, parent: &str, name: &str, at: i64) -> TrashItem {
        TrashItem {
            id: id.into(),
            name: name.into(),
            original_parent: PathBuf::from(parent),
            time_deleted: at,
        }
    }

    fn entry(id: &str, parent: &str, name: &str, at: i64) -> TrashEntry {
        TrashEntry::new(item(id, parent, name, at), Some(1), false)
    }

    fn view_with(n: usize) -> TrashView {
        let mut v = TrashView::new(PathBuf::from("/prev"));
        v.set_entries(
            (0..n)
                .map(|i| entry(&format!("id{i}"), "/p", &format!("n{i}"), i as i64 + 1))
                .collect(),
        );
        v
    }

    // ---- entry ----

    #[test]
    fn entry_original_path_needs_an_absolute_parent() {
        let e = entry("i", "/srv/data", "한글 문서", 5);
        assert_eq!(
            e.original_path(),
            Some(PathBuf::from("/srv/data/한글 문서"))
        );
        assert_eq!(e.name(), "한글 문서");
        assert_eq!(e.deleted_at(), Some(5));
        let lost = entry("i", "", "x", 0);
        assert_eq!(lost.original_path(), None);
        assert_eq!(lost.original_parent(), None);
        assert_eq!(lost.deleted_at(), None);
    }

    #[test]
    fn non_utf8_names_display_lossy_but_keep_the_raw_name() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'a', 0xff, b'b']);
        let mut it = item("i", "/p", "x", 1);
        it.name = raw.clone();
        let e = TrashEntry::new(it, None, false);
        assert_eq!(e.name(), "a\u{fffd}b");
        assert_eq!(e.original_path(), Some(Path::new("/p").join(raw)));
    }

    #[test]
    fn sort_newest_first_orders_by_deletion_time() {
        let mut v = vec![
            entry("a", "/p", "a", 1),
            entry("c", "/p", "c", 3),
            entry("b", "/p", "b", 2),
        ];
        sort_newest_first(&mut v);
        let ids: Vec<_> = v.iter().map(|e| e.id().to_owned()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }

    // ---- restore_with ----

    #[test]
    fn restore_with_all_ok() {
        let calls = RefCell::new(0);
        let r = restore_with(
            vec![item("1", "/p", "a", 1), item("2", "/p", "b", 2)],
            |v| {
                *calls.borrow_mut() += 1;
                assert_eq!(v.len(), 2);
                Ok(())
            },
        );
        assert_eq!(r.restored, [PathBuf::from("/p/a"), PathBuf::from("/p/b")]);
        assert!(r.collisions.is_empty() && r.errors.is_empty());
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn restore_with_collision_skips_that_item_and_continues_with_the_rest() {
        // Fake crate behaviour: items before the collision are restored.
        let r = restore_with(
            vec![
                item("1", "/p", "a", 1),
                item("2", "/p", "b", 2),
                item("3", "/p", "c", 3),
            ],
            |v| {
                if let Some(pos) = v.iter().position(|i| i.name == "b") {
                    return Err(trash::Error::RestoreCollision {
                        path: PathBuf::from("/p/b"),
                        remaining_items: v[pos..].to_vec(),
                    });
                }
                Ok(())
            },
        );
        assert_eq!(r.restored, [PathBuf::from("/p/a"), PathBuf::from("/p/c")]);
        assert_eq!(r.collisions, [PathBuf::from("/p/b")]);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn restore_with_twins_restores_one_and_reports_the_other_as_collision() {
        let r = restore_with(
            vec![item("1", "/p", "a", 1), item("2", "/p", "a", 2)],
            |v| {
                assert_eq!(v.len(), 1, "twins would make the crate refuse everything");
                Ok(())
            },
        );
        assert_eq!(r.restored.len(), 1);
        assert_eq!(r.collisions, [PathBuf::from("/p/a")]);
    }

    #[test]
    fn restore_with_other_errors_stop_and_are_reported() {
        let r = restore_with(vec![item("1", "/p", "a", 1)], |_| {
            Err(trash::Error::Unknown {
                description: "boom".into(),
            })
        });
        assert!(r.restored.is_empty());
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].contains("boom"));
    }

    #[test]
    fn restore_summary_wording() {
        let mut r = RestoreResult {
            restored: vec!["/a".into(), "/b".into()],
            ..Default::default()
        };
        assert_eq!(r.summary(), "복원: 2개");
        r.collisions = vec!["/c".into(), "/d".into()];
        assert_eq!(
            r.summary(),
            "복원: 2개 · 원래 위치에 같은 이름이 있어 건너뜀: /c 외 1개"
        );
        r.restored.clear();
        r.errors = vec!["boom".into()];
        assert_eq!(
            r.summary(),
            "원래 위치에 같은 이름이 있어 건너뜀: /c 외 1개 · 실패 1개: boom"
        );
    }

    // ---- matching ----

    #[test]
    fn match_fresh_keeps_only_still_listed_ids_and_counts_the_rest() {
        let wanted = vec![entry("1", "/p", "a", 1), entry("2", "/p", "b", 2)];
        let fresh = vec![entry("2", "/p", "b", 2), entry("9", "/p", "z", 9)];
        let (keep, skipped) = match_fresh(&wanted, &fresh);
        assert_eq!(keep.len(), 1);
        assert_eq!(keep[0].id(), "2");
        assert_eq!(skipped, 1);
    }

    #[test]
    fn find_trashed_matches_path_and_time_and_prefers_the_newest() {
        let list = vec![
            entry("old", "/p", "a", 5),
            entry("new1", "/p", "a", 12),
            entry("new2", "/p", "a", 15),
            entry("other", "/q", "a", 20),
            entry("b", "/p", "b", 11),
        ];
        let got = find_trashed(
            &list,
            &[PathBuf::from("/p/a"), PathBuf::from("/p/missing")],
            10,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id(), "new2");
        assert!(find_trashed(&list, &[PathBuf::from("/p/a")], 99).is_empty());
    }

    // ---- delete_with ----

    #[test]
    fn delete_with_bulk_success_reports_every_path_ok() {
        let paths = vec![PathBuf::from("/x/a"), PathBuf::from("/x/b")];
        let r = delete_with(&paths, |_| Ok(()), |_| panic!("no single calls"));
        assert!(r.iter().all(|(_, r)| r.is_ok()) && r.len() == 2);
    }

    #[test]
    fn delete_with_bulk_failure_retries_the_remaining_paths_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let (gone, left, bad) = (
            dir.path().join("gone"),
            dir.path().join("left"),
            dir.path().join("bad"),
        );
        std::fs::write(&left, "").unwrap();
        std::fs::write(&bad, "").unwrap();
        let paths = vec![gone.clone(), left.clone(), bad.clone()];
        let singles = RefCell::new(Vec::new());
        let r = delete_with(
            &paths,
            |_| Err("bulk".into()),
            |p| {
                singles.borrow_mut().push(p.to_path_buf());
                if p == bad {
                    Err("denied".into())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(*singles.borrow(), [left.clone(), bad.clone()]);
        assert_eq!(r[0], (gone, Ok(())), "already trashed by the bulk call");
        assert_eq!(r[1], (left, Ok(())));
        assert_eq!(r[2], (bad, Err("denied".into())));
    }

    #[test]
    fn delete_with_single_path_skips_the_bulk_call() {
        let paths = vec![PathBuf::from("/x/a")];
        let r = delete_with(&paths, |_| panic!("no bulk"), |_| Err("e".into()));
        assert_eq!(r, [(PathBuf::from("/x/a"), Err("e".into()))]);
    }

    // ---- view ----

    #[test]
    fn fmt_datetime_pads() {
        assert_eq!(fmt_datetime(2026, 3, 7, 4, 5), "2026-03-07 04:05");
    }

    #[test]
    fn format_local_has_expected_shape() {
        let s = format_local(1_700_000_000);
        assert_eq!(s.len(), 16);
        assert!(s.starts_with("2023-11-1"), "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[13..14], ":");
    }

    #[test]
    fn view_set_entries_clamps_cursor() {
        let mut v = view_with(5);
        v.cursor = 4;
        v.set_entries(vec![
            entry("id0", "/p", "n0", 1),
            entry("id1", "/p", "n1", 2),
        ]);
        assert_eq!(v.cursor, 1);
        v.set_entries(vec![]);
        assert_eq!(v.cursor, 0);
        assert!(v.current().is_none());
    }

    #[test]
    fn view_set_entries_keeps_cursor_on_same_entry_and_prunes_selection() {
        let mut v = view_with(4);
        v.cursor = 2; // id2
        v.selected.insert("id0".into());
        v.selected.insert("id2".into());
        v.set_entries(vec![
            entry("id2", "/p", "n2", 3),
            entry("id3", "/p", "n3", 4),
        ]);
        assert_eq!(v.current().unwrap().id(), "id2");
        assert_eq!(v.selected, HashSet::from([OsString::from("id2")]));
    }

    #[test]
    fn view_movement_clamps() {
        let mut v = view_with(10);
        v.move_up();
        assert_eq!(v.cursor, 0);
        v.move_down();
        assert_eq!(v.cursor, 1);
        v.page_down(4);
        assert_eq!(v.cursor, 5);
        v.page_down(40);
        assert_eq!(v.cursor, 9);
        v.page_up(3);
        assert_eq!(v.cursor, 6);
        v.home();
        assert_eq!(v.cursor, 0);
        v.end();
        assert_eq!(v.cursor, 9);
        v.move_down();
        assert_eq!(v.cursor, 9);
    }

    #[test]
    fn view_toggle_and_targets() {
        let mut v = view_with(4);
        assert_eq!(v.targets().len(), 1);
        assert_eq!(v.targets()[0].id(), "id0");
        v.toggle_current(); // select id0, cursor -> 1
        v.move_down();
        v.toggle_current(); // select id2, cursor -> 3
        let ids: Vec<_> = v.targets().iter().map(|e| e.id().to_owned()).collect();
        assert_eq!(ids, ["id0", "id2"]);
        v.home();
        v.toggle_current(); // deselect id0
        assert_eq!(v.targets().len(), 1);
        assert!(view_with(0).targets().is_empty());
    }

    #[test]
    fn view_ensure_visible_scrolls() {
        let mut v = view_with(20);
        v.cursor = 12;
        v.ensure_visible(5);
        assert_eq!(v.scroll, 8);
        v.cursor = 2;
        v.ensure_visible(5);
        assert_eq!(v.scroll, 2);
    }

    // ---- fake sanity ----

    #[test]
    fn system_trash_view_support_follows_the_os() {
        assert_eq!(SystemTrash.supports_view(), cfg!(target_os = "linux"));
    }

    /// Real freedesktop trash roundtrip; run on Linux with
    /// `cargo test -- --ignored`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "touches the real user trash"]
    fn real_linux_roundtrip_delete_list_restore_then_purge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("hdir-roundtrip-{}", now_secs()));
        std::fs::write(&path, "data").unwrap();
        let store = SystemTrash;
        let find = |store: &SystemTrash| {
            store
                .list()
                .unwrap()
                .into_iter()
                .filter(|e| e.original_path().as_deref() == Some(path.as_path()))
                .collect::<Vec<_>>()
        };
        assert!(store.delete(std::slice::from_ref(&path))[0].1.is_ok());
        assert!(!path.exists());
        let found = find(&store);
        assert_eq!(found.len(), 1);
        let r = store.restore(&found);
        assert_eq!(r.restored, std::slice::from_ref(&path), "{r:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "data");
        assert!(store.delete(std::slice::from_ref(&path))[0].1.is_ok());
        let found = find(&store);
        assert_eq!(found.len(), 1);
        store.purge(&found).unwrap();
        assert!(find(&store).is_empty());
        assert!(!path.exists());
    }
}
