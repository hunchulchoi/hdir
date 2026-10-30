//! Trash support built only on the operating system's own trash.
//!
//! hdir keeps no trash store of its own: F8 hands paths to the `trash` crate
//! (macOS: Finder, so "Put Back" works), and on Linux the freedesktop trash is
//! listed / restored / purged through `trash::os_limited`. Everything sits
//! behind [`TrashBackend`] so the app logic can be tested with a fake.

use std::collections::{HashMap, HashSet};
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
    /// Items skipped because the trash no longer holds them unchanged.
    pub changed: usize,
}

pub const CHANGED_MSG: &str = "휴지통 항목이 바뀌어 건너뜀";
pub const COLLISION_MSG: &str = "원래 위치에 같은 이름이 있어 건너뜀";

impl RestoreResult {
    /// One status line, e.g. `복원: 2개` or
    /// `복원: 1개 · 원래 위치에 같은 이름이 있어 건너뜀: /a/b`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.restored.is_empty()
            && (!self.collisions.is_empty() || !self.errors.is_empty() || self.changed > 0)
        {
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
        if self.changed > 0 {
            parts.push(format!("{CHANGED_MSG}: {}개", self.changed));
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
    /// Cheap identity listing (no per-entry metadata), for matching only.
    fn list_items(&self) -> Result<Vec<TrashItem>, String> {
        self.list().map(|v| v.into_iter().map(|e| e.item).collect())
    }
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

/// Run `restore_all` once per item (single pass) so a collision or error on
/// one item never blocks the rest. Items sharing an original path are
/// collisions too.
pub fn restore_with<F>(items: Vec<TrashItem>, mut restore_all: F) -> RestoreResult
where
    F: FnMut(Vec<TrashItem>) -> Result<(), trash::Error>,
{
    let mut res = RestoreResult::default();
    let mut seen = HashSet::new();
    for item in items {
        let path = item.original_path();
        if !seen.insert(path.clone()) {
            res.collisions.push(path);
            continue;
        }
        let name = item.name.to_string_lossy().into_owned();
        match restore_all(vec![item]) {
            Ok(()) => res.restored.push(path),
            Err(trash::Error::RestoreCollision { path, .. }) => res.collisions.push(path),
            Err(e) => res.errors.push(format!("{name}: {e}")),
        }
    }
    res
}

/// Restore only the `wanted` entries that a fresh identity listing still
/// holds unchanged (id, parent, name, time); the rest count as `changed`.
pub fn restore_verified(store: &dyn TrashBackend, wanted: &[TrashEntry]) -> RestoreResult {
    let fresh = match store.list_items() {
        Ok(f) => f,
        Err(e) => {
            return RestoreResult {
                errors: vec![e],
                ..Default::default()
            };
        }
    };
    let (keep, changed) = match_items(wanted, &fresh);
    let mut res = if keep.is_empty() {
        RestoreResult::default()
    } else {
        store.restore(&keep)
    };
    res.changed += changed;
    res
}

/// Like [`match_fresh`] against a plain item listing; keeps the wanted entries.
pub fn match_items(wanted: &[TrashEntry], fresh: &[TrashItem]) -> (Vec<TrashEntry>, usize) {
    let by_id: HashMap<&OsStr, &TrashItem> = fresh.iter().map(|f| (f.id.as_os_str(), f)).collect();
    let mut keep = Vec::new();
    let mut skipped = 0;
    for w in wanted {
        match by_id.get(w.id()) {
            Some(f) if same_identity(f, &w.item) => keep.push(w.clone()),
            _ => skipped += 1,
        }
    }
    (keep, skipped)
}

pub const UNVERIFIED_MSG: &str = "저장된 항목을 확인할 수 없어 복원 거부";
pub const SYMLINK_MSG: &str = "심볼릭 링크 항목은 파일 관리자에서 복원하세요";

/// Linux: the stored payload of a trash entry, from its `…/info/X.trashinfo`
/// id to `…/files/X`.
#[cfg(unix)]
pub fn payload_path(id: &OsStr) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let info = Path::new(id);
    let file = info.file_name()?.as_bytes();
    let name = file.strip_suffix(b".trashinfo")?;
    let info_dir = info.parent()?;
    if info_dir.file_name()? != "info" {
        return None;
    }
    Some(
        info_dir
            .parent()?
            .join("files")
            .join(OsStr::from_bytes(name)),
    )
}

/// Whether the payload of `id` is a symlink. Fails closed: anything but a
/// definite answer (or a plain NotFound) is an error.
#[cfg(unix)]
pub fn payload_is_link(id: &OsStr) -> Result<bool, String> {
    let p = payload_path(id).ok_or("저장 위치를 알 수 없음")?;
    match p.symlink_metadata() {
        Ok(m) => Ok(m.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

/// Restore through `restore`, but refuse items whose payload is a symlink:
/// the crate may treat a symlink to a directory as a directory and leave an
/// empty one behind.
pub fn restore_guarded(
    items: &[TrashEntry],
    is_link: impl Fn(&TrashEntry) -> Result<bool, String>,
    restore: impl FnOnce(&[TrashEntry]) -> RestoreResult,
) -> RestoreResult {
    let mut linked = Vec::new();
    let mut unknown = Vec::new();
    let mut plain = Vec::new();
    for e in items {
        match is_link(e) {
            Ok(false) => plain.push(e.clone()),
            Ok(true) => linked.push(e),
            Err(m) => unknown.push((e, m)),
        }
    }
    let mut res = if plain.is_empty() {
        RestoreResult::default()
    } else {
        restore(&plain)
    };
    res.errors.extend(
        linked
            .iter()
            .map(|e| format!("{}: {SYMLINK_MSG}", e.name())),
    );
    res.errors.extend(
        unknown
            .iter()
            .map(|(e, m)| format!("{}: {UNVERIFIED_MSG} ({m})", e.name())),
    );
    res
}

/// Same trash entry: the id alone is not enough because backends may reuse it.
pub fn same_identity(a: &TrashItem, b: &TrashItem) -> bool {
    a.id == b.id
        && a.original_parent == b.original_parent
        && a.name == b.name
        && a.time_deleted == b.time_deleted
}

/// Pick, from `fresh`, the entries of `wanted` that are still listed with the
/// same identity (id, original parent, name, deletion time); the count of
/// vanished or changed ones is returned too.
pub fn match_fresh(wanted: &[TrashEntry], fresh: &[TrashEntry]) -> (Vec<TrashEntry>, usize) {
    let by_id: HashMap<&OsStr, &TrashEntry> = fresh.iter().map(|f| (f.id(), f)).collect();
    let mut keep = Vec::new();
    let mut skipped = 0;
    for w in wanted {
        match by_id.get(w.id()) {
            Some(f) if same_identity(&f.item, &w.item) => keep.push((*f).clone()),
            _ => skipped += 1,
        }
    }
    (keep, skipped)
}

/// One item an F8 sent to the trash, identified exactly so Ctrl+Z restores
/// that very item and never "the newest one with the same path".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedItem {
    /// Backend id; empty when the backend cannot list (macOS).
    pub id: OsString,
    pub path: PathBuf,
    pub time_deleted: i64,
}

/// Result of [`delete_tracked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedDelete {
    pub results: Vec<(PathBuf, Result<(), String>)>,
    /// Exactly identified items, for the undo op.
    pub undo: Vec<TrashedItem>,
    /// Trashed paths that could not be identified (no undo info).
    pub lost: usize,
}

type ItemKey = (OsString, PathBuf, OsString, i64);

fn item_key(i: &TrashItem) -> ItemKey {
    (
        i.id.clone(),
        i.original_parent.clone(),
        i.name.clone(),
        i.time_deleted,
    )
}

/// Trash `paths` and work out which trash entries that created: the entries
/// listed after but not before, matched to the paths by original path.
pub fn delete_tracked(store: &dyn TrashBackend, paths: &[PathBuf]) -> TrackedDelete {
    if !store.supports_view() {
        let results = store.delete(paths);
        let undo = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(p, _)| TrashedItem {
                id: OsString::new(),
                path: p.clone(),
                time_deleted: 0,
            })
            .collect();
        return TrackedDelete {
            results,
            undo,
            lost: 0,
        };
    }
    let before = store.list_items();
    let results = store.delete(paths);
    let done: Vec<&PathBuf> = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(p, _)| p)
        .collect();
    let after = store.list_items();
    let (Ok(before), Ok(after)) = (before, after) else {
        return TrackedDelete {
            lost: done.len(),
            undo: Vec::new(),
            results,
        };
    };
    let known: HashSet<ItemKey> = before.iter().map(item_key).collect();
    let mut fresh: Vec<&TrashItem> = after
        .iter()
        .filter(|i| !known.contains(&item_key(i)))
        .collect();
    let mut undo = Vec::new();
    for p in &done {
        if let Some(pos) = fresh.iter().position(|i| i.original_path() == **p) {
            let i = fresh.remove(pos);
            undo.push(TrashedItem {
                id: i.id.clone(),
                path: (*p).clone(),
                time_deleted: i.time_deleted,
            });
        }
    }
    TrackedDelete {
        lost: done.len() - undo.len(),
        undo,
        results,
    }
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
            // After a failed bulk call a path is "trashed" only when it is
            // really gone; any other stat error is a failure for that path.
            let r = match paths.len() > 1 {
                true => match p.symlink_metadata() {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.to_string()),
                    Ok(_) => single(p),
                },
                false => single(p),
            };
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
    fn list_items(&self) -> Result<Vec<TrashItem>, String> {
        trash::os_limited::list().map_err(|e| e.to_string())
    }

    #[cfg(target_os = "linux")]
    fn list(&self) -> Result<Vec<TrashEntry>, String> {
        let mut entries: Vec<TrashEntry> = self
            .list_items()?
            .into_iter()
            .map(|item| {
                // lstat of the payload: no directory child counting, and a
                // symlink to a directory is not shown as a directory.
                let meta = payload_path(&item.id).and_then(|p| p.symlink_metadata().ok());
                let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                let size = meta.filter(|m| m.is_file()).map(|m| m.len());
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
        restore_guarded(
            items,
            |e| payload_is_link(e.id()),
            |ok| {
                restore_with(ok.iter().map(|e| e.item.clone()).collect(), |v| {
                    trash::os_limited::restore_all(v)
                })
            },
        )
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
#[cfg(windows)]
pub fn format_local(ts: i64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;
    // Unix seconds -> 100ns ticks since 1601-01-01.
    let Some(ticks) = ts
        .checked_add(11_644_473_600)
        .filter(|s| *s >= 0)
        .and_then(|s| (s as u64).checked_mul(10_000_000))
    else {
        return "-".to_string();
    };
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: plain-data structs; both pointers are valid for the call.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let ok = unsafe {
        FileTimeToLocalFileTime(&utc, &mut local) != 0 && FileTimeToSystemTime(&local, &mut st) != 0
    };
    if !ok {
        return "-".to_string();
    }
    fmt_datetime(
        st.wYear as i32,
        st.wMonth as u32,
        st.wDay as u32,
        st.wHour as u32,
        st.wMinute as u32,
    )
}

/// `YYYY-MM-DD HH:MM` in local time.
#[cfg(unix)]
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
        /// When set, every deletion gets exactly this timestamp.
        pub fixed_time: Mutex<Option<i64>>,
        /// List in the reverse of the usual order (arbitrary order).
        pub reverse_list: AtomicBool,
        items: Mutex<Vec<(TrashEntry, PathBuf)>>,
        /// Every path handed to `delete`.
        pub trashed: Mutex<Vec<PathBuf>>,
        /// While set and false, `delete` blocks (up to 2 s).
        pub gate: Mutex<Option<Arc<AtomicBool>>>,
        /// While set and false, `open_os_trash` blocks (up to 2 s).
        pub open_gate: Mutex<Option<Arc<AtomicBool>>>,
        pub fail_paths: Mutex<HashMap<PathBuf, String>>,
        pub fail_list: Mutex<Option<String>>,
        /// Original paths of each `restore` call.
        pub restore_calls: Mutex<Vec<Vec<PathBuf>>>,
        /// Ids of each `purge` call.
        pub purge_calls: Mutex<Vec<Vec<OsString>>>,
        pub list_calls: AtomicUsize,
        pub item_list_calls: AtomicUsize,
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
                fixed_time: Mutex::new(None),
                reverse_list: AtomicBool::new(false),
                items: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
                open_gate: Mutex::new(None),
                fail_paths: Mutex::new(HashMap::new()),
                fail_list: Mutex::new(None),
                restore_calls: Mutex::new(Vec::new()),
                purge_calls: Mutex::new(Vec::new()),
                list_calls: AtomicUsize::new(0),
                item_list_calls: AtomicUsize::new(0),
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

        /// Make every stored item look like a different deletion (new time).
        pub fn bump_times(&self) {
            for (e, _) in self.items.lock().unwrap().iter_mut() {
                e.item.time_deleted += 1000;
            }
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

    impl FakeTrash {
        fn restore_inner(&self, items: &[TrashEntry]) -> RestoreResult {
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
                    let t = match *self.fixed_time.lock().unwrap() {
                        Some(t) => t,
                        None => now_secs().max(self.last_time.load(Ordering::SeqCst) + 1),
                    };
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
            if self.reverse_list.load(Ordering::SeqCst) {
                v.reverse();
            }
            Ok(v)
        }

        fn list_items(&self) -> Result<Vec<TrashItem>, String> {
            self.item_list_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(m) = self.fail_list.lock().unwrap().clone() {
                return Err(m);
            }
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .map(|(e, _)| e.item.clone())
                .collect())
        }

        fn restore(&self, items: &[TrashEntry]) -> RestoreResult {
            restore_guarded(
                items,
                |e| {
                    self.items
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(x, _)| x.id() == e.id())
                        .map_or(Ok(false), |(_, p)| {
                            p.symlink_metadata()
                                .map(|m| m.file_type().is_symlink())
                                .map_err(|e| e.to_string())
                        })
                },
                |ok| self.restore_inner(ok),
            )
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
            let gate = self.open_gate.lock().unwrap().clone();
            if let Some(g) = gate {
                let start = std::time::Instant::now();
                while !g.load(Ordering::SeqCst) && start.elapsed().as_secs() < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
    fn restore_with_restores_one_item_per_call() {
        let sizes = RefCell::new(Vec::new());
        let r = restore_with(
            vec![item("1", "/p", "a", 1), item("2", "/p", "b", 2)],
            |v| {
                sizes.borrow_mut().push(v.len());
                Ok(())
            },
        );
        assert_eq!(r.restored, [PathBuf::from("/p/a"), PathBuf::from("/p/b")]);
        assert!(r.collisions.is_empty() && r.errors.is_empty());
        assert_eq!(*sizes.borrow(), [1, 1]);
    }

    #[test]
    fn restore_with_collision_skips_that_item_and_continues_with_the_rest() {
        let r = restore_with(
            vec![
                item("1", "/p", "a", 1),
                item("2", "/p", "b", 2),
                item("3", "/p", "c", 3),
            ],
            |v| {
                if v[0].name == "b" {
                    return Err(trash::Error::RestoreCollision {
                        path: PathBuf::from("/p/b"),
                        remaining_items: v,
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
        let calls = RefCell::new(0);
        let r = restore_with(
            vec![item("1", "/p", "a", 1), item("2", "/p", "a", 2)],
            |_| {
                *calls.borrow_mut() += 1;
                Ok(())
            },
        );
        assert_eq!(*calls.borrow(), 1);
        assert_eq!(r.restored.len(), 1);
        assert_eq!(r.collisions, [PathBuf::from("/p/a")]);
    }

    #[test]
    fn restore_with_an_error_does_not_stop_the_other_items() {
        let r = restore_with(
            vec![item("1", "/p", "a", 1), item("2", "/p", "b", 2)],
            |v| {
                if v[0].name == "a" {
                    return Err(trash::Error::Unknown {
                        description: "boom".into(),
                    });
                }
                Ok(())
            },
        );
        assert_eq!(r.restored, [PathBuf::from("/p/b")]);
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].contains("boom") && r.errors[0].contains("a"));
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
    fn match_fresh_requires_the_whole_identity_not_just_the_id() {
        let wanted = vec![entry("1", "/p", "a", 1)];
        for changed in [
            entry("1", "/p", "a", 2),
            entry("1", "/q", "a", 1),
            entry("1", "/p", "b", 1),
        ] {
            let (keep, skipped) = match_fresh(&wanted, &[changed]);
            assert!(keep.is_empty(), "reused id must not match");
            assert_eq!(skipped, 1);
        }
        let (keep, skipped) = match_fresh(&wanted, &[entry("1", "/p", "a", 1)]);
        assert_eq!((keep.len(), skipped), (1, 0));
    }

    #[cfg(unix)]
    #[test]
    fn payload_path_maps_info_id_to_files_dir() {
        assert_eq!(
            payload_path(OsStr::new("/h/.local/share/Trash/info/a b.txt.trashinfo")),
            Some(PathBuf::from("/h/.local/share/Trash/files/a b.txt"))
        );
        assert_eq!(payload_path(OsStr::new("/h/Trash/info/noext")), None);
        assert_eq!(payload_path(OsStr::new("id7")), None);
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_symlink_items_and_restores_the_rest() {
        use std::os::unix::fs::symlink;
        let store = FakeTrash::new();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "p").unwrap();
        for (_, r) in store.delete(&[link.clone(), plain.clone()]) {
            r.unwrap();
        }
        let all = store.list().unwrap();
        let res = store.restore(&all);
        assert_eq!(res.restored, std::slice::from_ref(&plain));
        assert_eq!(res.errors.len(), 1);
        assert!(res.errors[0].contains(SYMLINK_MSG), "{:?}", res.errors);
        assert!(res.errors[0].contains("link"));
        assert!(link.symlink_metadata().is_err(), "no stray dir or link");
        assert_eq!(store.count(), 1, "the link stays in the trash");
        assert_eq!(
            store.restore_calls.lock().unwrap().last().unwrap(),
            std::slice::from_ref(&plain),
            "the backend never sees the symlink item"
        );
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

    #[cfg(unix)]
    #[test]
    fn delete_with_bulk_failure_only_treats_not_found_as_trashed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        // "file/child": stat fails with ENOTDIR, which is not "already gone".
        let weird = file.join("child");
        let gone = dir.path().join("gone");
        let paths = vec![gone.clone(), weird.clone()];
        let r = delete_with(&paths, |_| Err("bulk".into()), |_| Err("single".into()));
        assert_eq!(r[0], (gone, Ok(())));
        assert_eq!(r[1].0, weird);
        assert!(r[1].1.is_err(), "not-found only; other errors are failures");
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

    // ---- payload / guard ----

    #[cfg(unix)]
    #[test]
    fn payload_path_keeps_non_utf8_names() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut id = b"/h/Trash/info/a".to_vec();
        id.push(0xff);
        id.extend_from_slice(b".trashinfo");
        let id = OsString::from_vec(id);
        let mut want = b"/h/Trash/files/a".to_vec();
        want.push(0xff);
        assert_eq!(
            payload_path(&id).map(|p| p.as_os_str().as_bytes().to_vec()),
            Some(want)
        );
    }

    #[cfg(unix)]
    #[test]
    fn payload_is_link_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("info");
        let files = dir.path().join("files");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::create_dir_all(&files).unwrap();
        let id = |n: &str| info.join(format!("{n}.trashinfo"));
        std::fs::write(files.join("plain"), "").unwrap();
        std::os::unix::fs::symlink("/", files.join("link")).unwrap();
        assert_eq!(payload_is_link(id("plain").as_os_str()), Ok(false));
        assert_eq!(payload_is_link(id("link").as_os_str()), Ok(true));
        assert_eq!(payload_is_link(id("absent").as_os_str()), Ok(false));
        // payload cannot be determined
        assert!(payload_is_link(OsStr::new("id7")).is_err());
        // lstat fails with something other than NotFound (ENOTDIR)
        std::fs::write(dir.path().join("files2"), "").unwrap();
        let odd = dir.path().join("files2").join("x");
        let _ = odd; // the payload dir itself is a file below
        let bad_root = dir.path().join("t2");
        std::fs::create_dir_all(bad_root.join("info")).unwrap();
        std::fs::write(bad_root.join("files"), "i am a file").unwrap();
        let bad = bad_root.join("info").join("z.trashinfo");
        assert!(payload_is_link(bad.as_os_str()).is_err());
    }

    #[test]
    fn restore_guarded_refuses_items_it_cannot_verify() {
        let e = entry("id7", "/p", "a", 1);
        let res = restore_guarded(
            std::slice::from_ref(&e),
            |_| Err("nope".into()),
            |_| panic!("must not reach the backend"),
        );
        assert!(res.restored.is_empty());
        assert!(res.errors[0].contains(UNVERIFIED_MSG), "{res:?}");
    }

    // ---- verified restore / cheap listing ----

    #[test]
    fn restore_verified_skips_entries_that_changed_since_they_were_listed() {
        let store = FakeTrash::new();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        for (_, r) in store.delete(&[a.clone(), b.clone()]) {
            r.unwrap();
        }
        let mut wanted = store.list().unwrap();
        // a's entry is stale (e.g. the id was reused for another deletion)
        let stale = wanted.iter_mut().find(|e| e.name() == "a").unwrap();
        stale.item.time_deleted += 1;
        let res = restore_verified(&store, &wanted);
        assert_eq!(res.restored, std::slice::from_ref(&b));
        assert_eq!(res.changed, 1);
        assert!(!a.exists());
        assert!(res.summary().contains(CHANGED_MSG), "{}", res.summary());
    }

    #[test]
    fn restore_verified_reports_a_listing_failure_and_restores_nothing() {
        let store = FakeTrash::new();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, "a").unwrap();
        store.delete(std::slice::from_ref(&a))[0].1.clone().unwrap();
        let wanted = store.list().unwrap();
        *store.fail_list.lock().unwrap() = Some("busy".into());
        let res = restore_verified(&store, &wanted);
        assert!(res.restored.is_empty() && res.errors[0].contains("busy"));
        assert!(store.restore_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn delete_tracked_identifies_with_the_cheap_listing_only() {
        use std::sync::atomic::Ordering;
        let store = FakeTrash::new();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::write(&a, "a").unwrap();
        let d = delete_tracked(&store, std::slice::from_ref(&a));
        assert_eq!(d.undo.len(), 1);
        assert_eq!(store.list_calls.load(Ordering::SeqCst), 0, "no lstat list");
        assert_eq!(store.item_list_calls.load(Ordering::SeqCst), 2);
    }

    // ---- fake sanity ----

    #[test]
    fn system_trash_view_support_follows_the_os() {
        assert_eq!(SystemTrash.supports_view(), cfg!(target_os = "linux"));
    }

    #[cfg(windows)]
    #[test]
    fn format_local_uses_the_local_time_zone_api() {
        // 2001-09-09 01:46:40 UTC: the local date is 08..10 whatever the zone.
        let s = format_local(1_000_000_000);
        assert!(s.starts_with("2001-09-") && s.len() == 16, "{s}");
        assert_eq!(format_local(i64::MIN), "-");
    }
}
