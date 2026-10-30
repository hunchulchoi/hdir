//! Trash browsing, restore and purge (no terminal IO).
//!
//! Two backends implement [`TrashStore`]:
//! - Linux: freedesktop trash (`trash` crate for trashing/listing, direct
//!   no-replace moves for restore/purge).
//! - macOS: the OS trash has no list/restore API, so items are trashed through
//!   `NSFileManager` (which reports the resulting path) and recorded in a
//!   hdir-owned JSON-lines index (`<data_dir>/hdir/trash-index.jsonl`, where
//!   `data_dir` is `~/Library/Application Support` on macOS).
//!
//! The index logic ([`IndexTrash`]) is platform independent and unit-tested
//! with temp directories on every OS.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One item in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// Backend identifier (Linux: `.trashinfo` path, macOS: path inside the trash).
    pub id: String,
    /// Original file name (lossy UTF-8).
    pub name: String,
    pub original_parent: Option<PathBuf>,
    /// Unix seconds.
    pub deleted_at: Option<i64>,
    pub size: Option<u64>,
    pub is_dir: bool,
}

impl TrashEntry {
    pub fn original_path(&self) -> Option<PathBuf> {
        self.original_parent.as_ref().map(|p| p.join(&self.name))
    }
}

/// Result of listing the trash: entries (newest first) plus an optional
/// warning when the listing is known to be incomplete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashListing {
    pub entries: Vec<TrashEntry>,
    pub warning: Option<String>,
}

impl TrashListing {
    pub fn new(entries: Vec<TrashEntry>) -> Self {
        Self {
            entries,
            warning: None,
        }
    }
}

pub const PARTIAL_LISTING_WARNING: &str =
    "Finder 휴지통 전체 목록은 전체 디스크 접근 권한이 필요함 · hdir가 지운 항목만 표시";

pub trait TrashStore: Send + Sync {
    /// Move `paths` to the trash; returns the created entries (best effort:
    /// may be empty when the backend cannot identify them).
    fn trash(&self, paths: &[PathBuf]) -> io::Result<Vec<TrashEntry>>;
    /// Newest first.
    fn list(&self) -> io::Result<TrashListing>;
    /// Move the item to `dest` (full target path). Never overwrites:
    /// `AlreadyExists` when `dest` exists.
    fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()>;
    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()>;
}

pub const NO_PERMISSION_MSG: &str = "휴지통 접근 권한 없음 (전체 디스크 접근 권한 필요)";

fn is_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1)
}

// ---- index records (pure IO helpers) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub trashed_path: PathBuf,
    pub original_path: PathBuf,
    pub deleted_at: i64,
    pub size: Option<u64>,
    pub is_dir: bool,
}

/// Records in file order; a missing file is empty, unparsable lines are skipped.
pub fn read_index(path: &Path) -> Vec<Record> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn append_records(path: &Path, records: &[Record]) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in records {
        out.push_str(&serde_json::to_string(r).map_err(io::Error::other)?);
        out.push('\n');
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(out.as_bytes())
}

/// Remove the records with these ids by rewriting the file (temp + rename).
pub fn drop_records(path: &Path, ids: &[String]) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let keep: Vec<Record> = read_index(path)
        .into_iter()
        .filter(|r| !ids.contains(&r.id))
        .collect();
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut out = String::new();
    for r in &keep {
        out.push_str(&serde_json::to_string(r).map_err(io::Error::other)?);
        out.push('\n');
    }
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// An item physically present in the trash directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub size: Option<u64>,
    pub is_dir: bool,
}

/// Merge index records with what is physically in the trash: records whose
/// file is gone are dropped, unknown items get `original_parent: None`.
/// Newest first (unknown last).
pub fn merge_listing(records: Vec<Record>, found: Vec<Found>) -> Vec<TrashEntry> {
    let present: HashMap<&Path, &Found> = found.iter().map(|f| (f.path.as_path(), f)).collect();
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut out: Vec<TrashEntry> = Vec::new();
    // Later records are newer and win over earlier duplicates.
    for r in records.iter().rev() {
        let Some(f) = present.get(r.trashed_path.as_path()) else {
            continue;
        };
        if !seen.insert(r.trashed_path.as_path()) {
            continue;
        }
        out.push(entry_from_record(r, Some(f)));
    }
    let mut unknown: Vec<TrashEntry> = found
        .iter()
        .filter(|f| !seen.contains(f.path.as_path()))
        .filter(|f| f.path.file_name().is_some_and(|n| n != ".DS_Store"))
        .map(|f| TrashEntry {
            id: f.path.to_string_lossy().into_owned(),
            name: file_name_lossy(&f.path),
            original_parent: None,
            deleted_at: None,
            size: f.size,
            is_dir: f.is_dir,
        })
        .collect();
    unknown.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(unknown);
    // Stable: equal timestamps keep "later record first".
    out.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
    out
}

fn entry_from_record(r: &Record, found: Option<&Found>) -> TrashEntry {
    TrashEntry {
        id: r.id.clone(),
        name: file_name_lossy(&r.original_path),
        original_parent: r.original_path.parent().map(Path::to_path_buf),
        deleted_at: Some(r.deleted_at),
        size: r.size.or(found.and_then(|f| f.size)),
        is_dir: r.is_dir || found.is_some_and(|f| f.is_dir),
    }
}

fn file_name_lossy(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// `name (복원됨)` for `n == 1`, `name (복원됨 2)` for `n == 2`, ...
pub fn restore_name(name: &str, n: usize) -> String {
    if n <= 1 {
        format!("{name} (복원됨)")
    } else {
        format!("{name} (복원됨 {n})")
    }
}

/// Move `src` to `dest` creating missing parents, never overwriting.
pub fn move_no_replace(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    crate::fs_ops::move_path_no_overwrite(src, dest)
}

fn remove_any(path: &Path) -> io::Result<()> {
    let res = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match res {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn found_for(path: &Path) -> Option<Found> {
    let m = fs::symlink_metadata(path).ok()?;
    Some(Found {
        path: path.to_path_buf(),
        size: (!m.is_dir()).then_some(m.len()),
        is_dir: m.is_dir(),
    })
}

// ---- index-backed store ----

/// Trashes one path and returns where it ended up.
pub type Trasher = Box<dyn Fn(&Path) -> io::Result<PathBuf> + Send + Sync>;

/// Lists the paths inside the trash directory (injectable for tests).
pub type Scanner = Box<dyn Fn(&Path) -> io::Result<Vec<PathBuf>> + Send + Sync>;

fn read_dir_paths(dir: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect())
}

pub struct IndexTrash {
    trash_dir: PathBuf,
    scanner: Scanner,
    index_path: PathBuf,
    trasher: Trasher,
    clock: fn() -> i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl IndexTrash {
    pub fn new(trash_dir: PathBuf, index_path: PathBuf, trasher: Trasher) -> Self {
        Self {
            trash_dir,
            scanner: Box::new(read_dir_paths),
            index_path,
            trasher,
            clock: now_secs,
        }
    }

    pub fn with_scanner(mut self, scanner: Scanner) -> Self {
        self.scanner = scanner;
        self
    }

    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }
}

impl TrashStore for IndexTrash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<Vec<TrashEntry>> {
        let mut records = Vec::new();
        let mut failure = None;
        for p in paths {
            let abs = std::path::absolute(p)?;
            let meta = fs::symlink_metadata(&abs)?;
            match (self.trasher)(&abs) {
                Ok(trashed) => records.push(Record {
                    id: trashed.to_string_lossy().into_owned(),
                    trashed_path: trashed,
                    original_path: abs,
                    deleted_at: (self.clock)(),
                    size: (!meta.is_dir()).then_some(meta.len()),
                    is_dir: meta.is_dir(),
                }),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        // Record what was trashed even when a later path failed. A failed
        // index write only loses the original path, not the item.
        let _ = append_records(&self.index_path, &records);
        if let Some(e) = failure {
            return Err(e);
        }
        Ok(records.iter().map(|r| entry_from_record(r, None)).collect())
    }

    fn list(&self) -> io::Result<TrashListing> {
        let records = read_index(&self.index_path);
        match (self.scanner)(&self.trash_dir) {
            Ok(paths) => {
                let mut found: Vec<Found> = paths.iter().filter_map(|p| found_for(p)).collect();
                // Items trashed from other volumes live outside the home trash.
                for r in &records {
                    if r.trashed_path.parent() != Some(self.trash_dir.as_path())
                        && let Some(f) = found_for(&r.trashed_path)
                    {
                        found.push(f);
                    }
                }
                Ok(TrashListing::new(merge_listing(records, found)))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(TrashListing::default()),
            // macOS privacy (TCC) blocks the directory scan without Full Disk
            // Access; fall back to what hdir itself recorded.
            Err(e) if is_denied(&e) => {
                let mut kept = Vec::new();
                for r in &records {
                    match fs::symlink_metadata(&r.trashed_path) {
                        Ok(_) => kept.push(r),
                        Err(e) if is_denied(&e) => kept.push(r),
                        Err(_) => {}
                    }
                }
                let found = kept
                    .iter()
                    .map(|r| {
                        found_for(&r.trashed_path).unwrap_or_else(|| Found {
                            path: r.trashed_path.clone(),
                            size: r.size,
                            is_dir: r.is_dir,
                        })
                    })
                    .collect();
                Ok(TrashListing {
                    entries: merge_listing(records, found),
                    warning: Some(PARTIAL_LISTING_WARNING.into()),
                })
            }
            Err(e) => Err(e),
        }
    }

    fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        move_no_replace(Path::new(&entry.id), dest)?;
        // A stale record is filtered out by `list` anyway.
        let _ = drop_records(&self.index_path, std::slice::from_ref(&entry.id));
        Ok(())
    }

    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()> {
        let mut first_err = None;
        let mut done = Vec::new();
        for e in entries {
            match remove_any(Path::new(&e.id)) {
                Ok(()) => done.push(e.id.clone()),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        let _ = drop_records(&self.index_path, &done);
        first_err.map_or(Ok(()), Err)
    }
}

// ---- macOS backend ----

#[cfg(target_os = "macos")]
fn ns_trash_item(path: &Path) -> io::Result<PathBuf> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let p = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "경로가 UTF-8이 아님"))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(p));
    let mut out = None;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut out))
        .map_err(|e| io::Error::other(e.localizedDescription().to_string()))?;
    out.and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()))
        .ok_or_else(|| io::Error::other("휴지통 경로를 알 수 없음"))
}

#[cfg(target_os = "macos")]
fn macos_store() -> Option<IndexTrash> {
    let trash_dir = dirs::home_dir()?.join(".Trash");
    let index = dirs::data_dir()?.join("hdir").join("trash-index.jsonl");
    Some(IndexTrash::new(trash_dir, index, Box::new(ns_trash_item)))
}

#[cfg(target_os = "macos")]
struct Unavailable;

#[cfg(target_os = "macos")]
impl TrashStore for Unavailable {
    fn trash(&self, _: &[PathBuf]) -> io::Result<Vec<TrashEntry>> {
        Err(io::Error::other("홈 폴더를 찾을 수 없음"))
    }
    fn list(&self) -> io::Result<TrashListing> {
        self.trash(&[]).map(TrashListing::new)
    }
    fn restore(&self, _: &TrashEntry, _: &Path) -> io::Result<()> {
        self.trash(&[]).map(|_| ())
    }
    fn purge(&self, _: &[TrashEntry]) -> io::Result<()> {
        self.trash(&[]).map(|_| ())
    }
}

// ---- freedesktop helpers (pure, testable everywhere) ----

/// The payload file for a `.trashinfo` path: `<trash>/info/x.trashinfo` ->
/// `<trash>/files/x`.
pub fn freedesktop_file(info: &Path) -> PathBuf {
    let trash = info.parent().and_then(Path::parent).unwrap_or(info);
    let stem = info.file_stem().unwrap_or_default();
    trash.join("files").join(stem)
}

/// Move the payload to `dest` (no replace) and drop its `.trashinfo`.
pub fn freedesktop_restore(info: &Path, dest: &Path) -> io::Result<()> {
    move_no_replace(&freedesktop_file(info), dest)?;
    fs::remove_file(info)
}

pub fn freedesktop_purge(info: &Path) -> io::Result<()> {
    remove_any(&freedesktop_file(info))?;
    match fs::remove_file(info) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Pick the entries created by a just-finished trash call: not in `before`,
/// original path equal to one of `paths`; newest per path.
pub fn match_trashed(
    paths: &[PathBuf],
    before: &HashSet<String>,
    after: Vec<TrashEntry>,
) -> Vec<TrashEntry> {
    paths
        .iter()
        .filter_map(|p| {
            after
                .iter()
                .filter(|e| !before.contains(&e.id) && e.original_path().as_deref() == Some(p))
                .max_by_key(|e| e.deleted_at)
                .cloned()
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub struct FreedesktopTrash;

#[cfg(not(target_os = "macos"))]
impl FreedesktopTrash {
    fn entries() -> io::Result<Vec<TrashEntry>> {
        let items = trash::os_limited::list().map_err(io::Error::other)?;
        let mut out: Vec<TrashEntry> = items
            .into_iter()
            .map(|it| {
                let info = PathBuf::from(&it.id);
                let meta = fs::symlink_metadata(freedesktop_file(&info)).ok();
                TrashEntry {
                    id: it.id.to_string_lossy().into_owned(),
                    name: it.name.to_string_lossy().into_owned(),
                    original_parent: Some(it.original_parent),
                    deleted_at: Some(it.time_deleted),
                    size: meta.as_ref().filter(|m| !m.is_dir()).map(|m| m.len()),
                    is_dir: meta.is_some_and(|m| m.is_dir()),
                }
            })
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
        Ok(out)
    }
}

#[cfg(not(target_os = "macos"))]
impl TrashStore for FreedesktopTrash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<Vec<TrashEntry>> {
        // The trash crate records the canonical parent as the original location.
        let norm: Vec<PathBuf> = paths
            .iter()
            .map(|p| {
                let abs = std::path::absolute(p).unwrap_or_else(|_| p.clone());
                match (abs.parent().map(fs::canonicalize), abs.file_name()) {
                    (Some(Ok(parent)), Some(name)) => parent.join(name),
                    _ => abs,
                }
            })
            .collect();
        let before: HashSet<String> = Self::entries()
            .map(|v| v.into_iter().map(|e| e.id).collect())
            .unwrap_or_default();
        trash::delete_all(paths).map_err(io::Error::other)?;
        Ok(Self::entries()
            .map(|after| match_trashed(&norm, &before, after))
            .unwrap_or_default())
    }

    fn list(&self) -> io::Result<TrashListing> {
        Self::entries().map(TrashListing::new)
    }

    fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        freedesktop_restore(Path::new(&entry.id), dest)
    }

    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()> {
        entries
            .iter()
            .try_for_each(|e| freedesktop_purge(Path::new(&e.id)))
    }
}

/// The trash backend of the running OS.
pub fn system_store() -> Arc<dyn TrashStore> {
    #[cfg(target_os = "macos")]
    {
        match macos_store() {
            Some(s) => Arc::new(s),
            None => Arc::new(Unavailable),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(FreedesktopTrash)
    }
}

// ---- restore orchestration ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    Report,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub entry: TrashEntry,
    pub message: String,
    /// Worth keeping for a retry (false when the item is gone).
    pub retry: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub conflicts: Vec<(TrashEntry, PathBuf)>,
    pub failed: Vec<Failure>,
}

const MAX_RENAME_TRIES: usize = 1000;

/// Restore every `(entry, dest)`; with [`OnConflict::Rename`] an occupied
/// `dest` gets a free `name (복원됨 N)` sibling instead.
pub fn restore_entries(
    store: &dyn TrashStore,
    jobs: &[(TrashEntry, PathBuf)],
    mode: OnConflict,
) -> RestoreReport {
    let mut report = RestoreReport::default();
    for (entry, dest) in jobs {
        let mut result = store.restore(entry, dest).map(|()| dest.clone());
        if mode == OnConflict::Rename
            && matches!(&result, Err(e) if e.kind() == io::ErrorKind::AlreadyExists)
        {
            let name = file_name_lossy(dest);
            for n in 1..=MAX_RENAME_TRIES {
                let alt = dest.with_file_name(restore_name(&name, n));
                match store.restore(entry, &alt) {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    other => {
                        result = other.map(|()| alt);
                        break;
                    }
                }
            }
        }
        match result {
            Ok(path) => report.restored.push(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                report.conflicts.push((entry.clone(), dest.clone()));
            }
            Err(e) => report.failed.push(Failure {
                entry: entry.clone(),
                message: format!("{}: {e}", entry.name),
                retry: e.kind() != io::ErrorKind::NotFound,
            }),
        }
    }
    report
}

/// Destination for each entry: its original path, else `fallback_dir/name`.
/// Entries with neither are skipped.
pub fn build_jobs(
    entries: &[TrashEntry],
    fallback_dir: Option<&Path>,
) -> Vec<(TrashEntry, PathBuf)> {
    entries
        .iter()
        .filter_map(|e| {
            e.original_path()
                .or_else(|| fallback_dir.map(|d| d.join(&e.name)))
                .map(|dest| (e.clone(), dest))
        })
        .collect()
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
    pub selected: HashSet<String>,
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
        let keep = self.current().map(|e| e.id.clone());
        self.entries = entries;
        if let Some(i) = keep.and_then(|id| self.entries.iter().position(|e| e.id == id)) {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(self.last_index());
        let ids: HashSet<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
        self.selected.retain(|id| ids.contains(id.as_str()));
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
        if let Some(id) = self.current().map(|e| e.id.clone())
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
            .filter(|e| self.selected.contains(&e.id))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
pub use fake::FakeTrashStore;

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

    static CLOCK: AtomicI64 = AtomicI64::new(1_700_000_000);

    /// Trasher that moves into `trash_dir`, adding a numeric suffix on clash.
    pub fn mover(trash_dir: PathBuf) -> Trasher {
        Box::new(move |p: &Path| {
            fs::create_dir_all(&trash_dir)?;
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let mut target = trash_dir.join(&name);
            let mut i = 1;
            while target.symlink_metadata().is_ok() {
                i += 1;
                target = trash_dir.join(format!("{name} {i}"));
            }
            fs::rename(p, &target)?;
            Ok(target)
        })
    }

    /// Functional store backed by a temp directory (a real [`IndexTrash`]),
    /// with hooks to block or fail `trash`.
    pub struct FakeTrashStore {
        _tmp: tempfile::TempDir,
        trash_dir: PathBuf,
        inner: IndexTrash,
        /// Every path handed to `trash`.
        pub trashed: Mutex<Vec<PathBuf>>,
        /// While set and false, `trash` blocks (up to 2 s).
        pub gate: Mutex<Option<Arc<AtomicBool>>>,
        pub fail_trash: Mutex<Option<String>>,
    }

    impl Default for FakeTrashStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeTrashStore {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let trash_dir = tmp.path().join("Trash");
            let index = tmp.path().join("index.jsonl");
            let inner = IndexTrash::new(trash_dir.clone(), index, mover(trash_dir.clone()))
                .with_clock(|| CLOCK.fetch_add(1, Ordering::SeqCst));
            Self {
                _tmp: tmp,
                trash_dir,
                inner,
                trashed: Mutex::new(Vec::new()),
                gate: Mutex::new(None),
                fail_trash: Mutex::new(None),
            }
        }

        /// An item in the trash that has no index record (unknown origin).
        pub fn add_foreign(&self, name: &str, content: &str) {
            fs::create_dir_all(&self.trash_dir).unwrap();
            fs::write(self.trash_dir.join(name), content).unwrap();
        }
    }

    impl TrashStore for FakeTrashStore {
        fn trash(&self, paths: &[PathBuf]) -> io::Result<Vec<TrashEntry>> {
            let gate = self.gate.lock().unwrap().clone();
            if let Some(g) = gate {
                let start = std::time::Instant::now();
                while !g.load(Ordering::SeqCst) && start.elapsed().as_secs() < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            if let Some(msg) = self.fail_trash.lock().unwrap().clone() {
                return Err(io::Error::other(msg));
            }
            self.trashed.lock().unwrap().extend(paths.iter().cloned());
            self.inner.trash(paths)
        }
        fn list(&self) -> io::Result<TrashListing> {
            self.inner.list()
        }
        fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
            self.inner.restore(entry, dest)
        }
        fn purge(&self, entries: &[TrashEntry]) -> io::Result<()> {
            self.inner.purge(entries)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::mover;
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn rec(dir: &Path, name: &str, orig: &str, at: i64) -> Record {
        let trashed = dir.join(name);
        Record {
            id: trashed.to_string_lossy().into_owned(),
            trashed_path: trashed,
            original_path: PathBuf::from(orig),
            deleted_at: at,
            size: Some(1),
            is_dir: false,
        }
    }

    struct Fixture {
        _tmp: TempDir,
        root: PathBuf,
        trash_dir: PathBuf,
        index: PathBuf,
        store: IndexTrash,
    }

    fn fixture() -> Fixture {
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let trash_dir = root.join(".Trash");
        let index = root.join("data/hdir/trash-index.jsonl");
        fs::create_dir_all(root.join("work")).unwrap();
        let store = IndexTrash::new(trash_dir.clone(), index.clone(), mover(trash_dir.clone()))
            .with_clock(|| 1_700_000_000);
        Fixture {
            _tmp: tmp,
            root,
            trash_dir,
            index,
            store,
        }
    }

    // ---- index records ----

    #[test]
    fn index_append_and_read_roundtrip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("sub/idx.jsonl");
        let a = rec(tmp.path(), "a", "/x/a", 10);
        let b = rec(tmp.path(), "한글 \"b\"\n", "/x/한글 \"b\"\n", 20);
        append_records(&path, std::slice::from_ref(&a)).unwrap();
        append_records(&path, std::slice::from_ref(&b)).unwrap();
        assert_eq!(read_index(&path), vec![a, b]);
    }

    #[test]
    fn index_read_missing_is_empty_and_bad_lines_skipped() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("idx.jsonl");
        assert!(read_index(&path).is_empty());
        let a = rec(tmp.path(), "a", "/x/a", 10);
        append_records(&path, std::slice::from_ref(&a)).unwrap();
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("not json\n\n");
        fs::write(&path, text).unwrap();
        assert_eq!(read_index(&path), vec![a]);
    }

    #[test]
    fn drop_records_removes_only_given_ids_without_leftovers() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("idx.jsonl");
        let (a, b, c) = (
            rec(tmp.path(), "a", "/x/a", 1),
            rec(tmp.path(), "b", "/x/b", 2),
            rec(tmp.path(), "c", "/x/c", 3),
        );
        append_records(&path, &[a.clone(), b.clone(), c.clone()]).unwrap();
        drop_records(&path, std::slice::from_ref(&b.id)).unwrap();
        assert_eq!(read_index(&path), vec![a, c]);
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("idx.jsonl")]);
    }

    #[test]
    fn drop_records_on_missing_index_is_ok() {
        let tmp = tempdir().unwrap();
        drop_records(&tmp.path().join("none.jsonl"), &["x".into()]).unwrap();
    }

    // ---- merge ----

    fn found(path: PathBuf, is_dir: bool) -> Found {
        Found {
            path,
            size: if is_dir { None } else { Some(5) },
            is_dir,
        }
    }

    #[test]
    fn merge_listing_combines_index_and_unknown_newest_first() {
        let d = Path::new("/trash");
        let old = rec(d, "old.txt", "/home/u/docs/old.txt", 100);
        let new = rec(d, "new.txt", "/home/u/new.txt", 200);
        let gone = rec(d, "gone.txt", "/home/u/gone.txt", 300);
        let found = vec![
            found(d.join("zeta"), false),
            found(d.join("old.txt"), false),
            found(d.join(".DS_Store"), false),
            found(d.join("new.txt"), false),
            found(d.join("alpha"), true),
        ];
        let got = merge_listing(vec![old, new, gone], found);
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["new.txt", "old.txt", "alpha", "zeta"]);
        assert_eq!(
            got[1].original_parent.as_deref(),
            Some(Path::new("/home/u/docs"))
        );
        assert_eq!(got[1].deleted_at, Some(100));
        assert_eq!(got[2].original_parent, None);
        assert_eq!(got[2].deleted_at, None);
        assert!(got[2].is_dir);
        assert_eq!(got[3].id, "/trash/zeta");
    }

    #[test]
    fn merge_listing_keeps_input_order_for_same_second_newest_first() {
        let d = Path::new("/t");
        let (a, b) = (rec(d, "a", "/x/a", 5), rec(d, "b", "/x/b", 5));
        let got = merge_listing(
            vec![a, b],
            vec![found(d.join("a"), false), found(d.join("b"), false)],
        );
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b", "a"], "later record is newer");
    }

    #[test]
    fn merge_listing_duplicate_ids_last_wins() {
        let d = Path::new("/t");
        let first = rec(d, "a", "/old/a", 1);
        let second = rec(d, "a", "/new/a", 2);
        let got = merge_listing(vec![first, second], vec![found(d.join("a"), false)]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].original_parent.as_deref(), Some(Path::new("/new")));
    }

    #[test]
    fn restore_name_numbering() {
        assert_eq!(restore_name("a.txt", 1), "a.txt (복원됨)");
        assert_eq!(restore_name("a.txt", 2), "a.txt (복원됨 2)");
        assert_eq!(restore_name("a.txt", 10), "a.txt (복원됨 10)");
    }

    // ---- IndexTrash ----

    #[test]
    fn trash_records_entries_and_list_finds_them() {
        let f = fixture();
        let file = f.root.join("work/내 파일 'q'.txt");
        fs::write(&file, "hello").unwrap();
        let dir = f.root.join("work/adir");
        fs::create_dir(&dir).unwrap();
        let entries = f.store.trash(&[file.clone(), dir.clone()]).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(!file.exists() && !dir.exists());
        assert_eq!(entries[0].name, "내 파일 'q'.txt");
        assert_eq!(
            entries[0].original_parent.as_deref(),
            Some(f.root.join("work").as_path())
        );
        assert_eq!(entries[0].deleted_at, Some(1_700_000_000));
        assert_eq!(entries[0].size, Some(5));
        assert!(entries[1].is_dir);
        let listed = f.store.list().unwrap().entries;
        assert_eq!(listed.len(), 2);
        for e in &entries {
            assert!(listed.contains(e), "{e:?} not listed");
        }
        assert_eq!(read_index(&f.index).len(), 2);
    }

    #[test]
    fn restore_moves_back_and_drops_record() {
        let f = fixture();
        let file = f.root.join("work/a.txt");
        fs::write(&file, "hello").unwrap();
        let e = f
            .store
            .trash(std::slice::from_ref(&file))
            .unwrap()
            .remove(0);
        f.store.restore(&e, &file).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
        assert!(f.store.list().unwrap().entries.is_empty());
        assert!(read_index(&f.index).is_empty());
    }

    #[test]
    fn restore_never_overwrites_and_keeps_both() {
        let f = fixture();
        let file = f.root.join("work/a.txt");
        fs::write(&file, "old").unwrap();
        let e = f
            .store
            .trash(std::slice::from_ref(&file))
            .unwrap()
            .remove(0);
        fs::write(&file, "new").unwrap();
        let err = f.store.restore(&e, &file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(f.store.list().unwrap().entries, vec![e], "still in trash");
    }

    #[test]
    fn restore_creates_missing_parent_and_works_for_dirs() {
        let f = fixture();
        let dir = f.root.join("work/d");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("in.txt"), "x").unwrap();
        let e = f.store.trash(std::slice::from_ref(&dir)).unwrap().remove(0);
        let dest = f.root.join("elsewhere/deep/d");
        f.store.restore(&e, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("in.txt")).unwrap(), "x");
    }

    #[test]
    fn unknown_item_lists_without_parent_and_restores_to_given_dest() {
        let f = fixture();
        fs::create_dir_all(&f.trash_dir).unwrap();
        fs::write(f.trash_dir.join("stray.txt"), "s").unwrap();
        let listed = f.store.list().unwrap().entries;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original_parent, None);
        assert_eq!(listed[0].name, "stray.txt");
        let dest = f.root.join("work/stray.txt");
        f.store.restore(&listed[0], &dest).unwrap();
        assert_eq!(fs::read_to_string(dest).unwrap(), "s");
    }

    #[test]
    fn purge_removes_files_dirs_and_records() {
        let f = fixture();
        let file = f.root.join("work/a.txt");
        let dir = f.root.join("work/d");
        fs::write(&file, "x").unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("i"), "x").unwrap();
        let es = f.store.trash(&[file, dir]).unwrap();
        f.store.purge(&es).unwrap();
        assert!(f.store.list().unwrap().entries.is_empty());
        assert_eq!(fs::read_dir(&f.trash_dir).unwrap().count(), 0);
        assert!(read_index(&f.index).is_empty());
        // purging again (already gone) is not an error
        f.store.purge(&es).unwrap();
    }

    #[test]
    fn missing_trash_dir_lists_empty() {
        let f = fixture();
        assert!(f.store.list().unwrap().entries.is_empty());
    }

    fn denied_store(f: &Fixture, kind: io::Error) -> IndexTrash {
        let kind = std::sync::Mutex::new(Some(kind));
        IndexTrash::new(
            f.trash_dir.clone(),
            f.index.clone(),
            mover(f.trash_dir.clone()),
        )
        .with_clock(|| 1_700_000_000)
        .with_scanner(Box::new(move |_| {
            Err(kind
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied)))
        }))
    }

    #[test]
    fn scan_denied_lists_index_entries_with_warning() {
        let f = fixture();
        let a = f.root.join("work/a.txt");
        let b = f.root.join("work/b.txt");
        fs::write(&a, "1").unwrap();
        fs::write(&b, "2").unwrap();
        let es = f.store.trash(&[a, b]).unwrap();
        let store = denied_store(&f, io::Error::from(io::ErrorKind::PermissionDenied));
        let listing = store.list().unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.warning.as_deref(), Some(PARTIAL_LISTING_WARNING));
        // restore and purge still work without the directory scan
        let dest = f.root.join("work/a.txt");
        let ea = es.iter().find(|e| e.name == "a.txt").unwrap();
        store.restore(ea, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest).unwrap(), "1");
        let eb = es.iter().find(|e| e.name == "b.txt").unwrap();
        store.purge(std::slice::from_ref(eb)).unwrap();
    }

    #[test]
    fn scan_eperm_os_error_also_falls_back() {
        let f = fixture();
        let a = f.root.join("work/a.txt");
        fs::write(&a, "1").unwrap();
        f.store.trash(&[a]).unwrap();
        let store = denied_store(&f, io::Error::from_raw_os_error(1));
        let listing = store.list().unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.warning.is_some());
    }

    #[test]
    fn scan_denied_drops_not_found_records() {
        let f = fixture();
        let a = f.root.join("work/a.txt");
        let b = f.root.join("work/b.txt");
        fs::write(&a, "1").unwrap();
        fs::write(&b, "2").unwrap();
        let es = f.store.trash(&[a, b]).unwrap();
        fs::remove_file(&es.iter().find(|e| e.name == "a.txt").unwrap().id).unwrap();
        let store = denied_store(&f, io::Error::from(io::ErrorKind::PermissionDenied));
        let listing = store.list().unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "b.txt");
    }

    #[test]
    fn scan_denied_keeps_records_whose_stat_is_denied() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let a = f.root.join("work/a.txt");
        fs::write(&a, "1").unwrap();
        f.store.trash(&[a]).unwrap();
        // Trash dir without search permission: stat of the child gives EACCES.
        fs::set_permissions(&f.trash_dir, fs::Permissions::from_mode(0o000)).unwrap();
        let store = denied_store(&f, io::Error::from(io::ErrorKind::PermissionDenied));
        let listing = store.list();
        let stat_denied = fs::symlink_metadata(f.trash_dir.join("a.txt")).is_err();
        fs::set_permissions(&f.trash_dir, fs::Permissions::from_mode(0o755)).unwrap();
        if !stat_denied {
            return; // running as root
        }
        assert_eq!(
            listing.unwrap().entries.len(),
            1,
            "unknown -> assume present"
        );
    }

    #[test]
    fn scan_other_errors_still_fail() {
        let f = fixture();
        let store = denied_store(&f, io::Error::other("boom"));
        assert!(store.list().is_err());
    }

    #[test]
    fn missing_trash_dir_has_no_warning() {
        let f = fixture();
        assert_eq!(f.store.list().unwrap(), TrashListing::default());
    }

    // ---- freedesktop layout ----

    fn fd_item(root: &Path, name: &str, content: &str) -> PathBuf {
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        fs::write(root.join("files").join(name), content).unwrap();
        let info = root.join("info").join(format!("{name}.trashinfo"));
        fs::write(&info, "[Trash Info]\nPath=/x\n").unwrap();
        info
    }

    #[test]
    fn freedesktop_file_maps_info_to_files() {
        assert_eq!(
            freedesktop_file(Path::new("/h/Trash/info/a b.txt.trashinfo")),
            PathBuf::from("/h/Trash/files/a b.txt")
        );
    }

    #[test]
    fn freedesktop_restore_moves_no_replace_and_removes_info() {
        let tmp = tempdir().unwrap();
        let info = fd_item(&tmp.path().join("Trash"), "a.txt", "payload");
        let dest = tmp.path().join("out/a (복원됨).txt");
        freedesktop_restore(&info, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "payload");
        assert!(!info.exists());
        // conflict keeps the item
        let info2 = fd_item(&tmp.path().join("Trash"), "b.txt", "two");
        fs::write(tmp.path().join("out/b.txt"), "mine").unwrap();
        let err = freedesktop_restore(&info2, &tmp.path().join("out/b.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(info2.exists() && freedesktop_file(&info2).exists());
    }

    #[test]
    fn freedesktop_purge_removes_payload_and_info() {
        let tmp = tempdir().unwrap();
        let info = fd_item(&tmp.path().join("Trash"), "a.txt", "p");
        freedesktop_purge(&info).unwrap();
        assert!(!info.exists() && !freedesktop_file(&info).exists());
        freedesktop_purge(&info).unwrap();
    }

    fn entry(id: &str, parent: &str, name: &str, at: i64) -> TrashEntry {
        TrashEntry {
            id: id.into(),
            name: name.into(),
            original_parent: Some(PathBuf::from(parent)),
            deleted_at: Some(at),
            size: None,
            is_dir: false,
        }
    }

    #[test]
    fn match_trashed_picks_new_entries_for_given_paths() {
        let before: HashSet<String> = ["i1".to_string()].into();
        let after = vec![
            entry("i1", "/w", "a", 1),
            entry("i2", "/w", "a", 5),
            entry("i3", "/w", "a", 9),
            entry("i4", "/w", "b", 9),
            entry("i5", "/other", "c", 9),
        ];
        let got = match_trashed(
            &[PathBuf::from("/w/a"), PathBuf::from("/w/b")],
            &before,
            after,
        );
        let ids: Vec<_> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["i3", "i4"]);
    }

    // ---- restore orchestration ----

    fn trashed(f: &Fixture, name: &str, content: &str) -> (TrashEntry, PathBuf) {
        let p = f.root.join("work").join(name);
        fs::write(&p, content).unwrap();
        let e = f.store.trash(std::slice::from_ref(&p)).unwrap().remove(0);
        (e, p)
    }

    #[test]
    fn restore_entries_reports_conflicts_without_touching_them() {
        let f = fixture();
        let (e1, p1) = trashed(&f, "a.txt", "1");
        let (e2, p2) = trashed(&f, "b.txt", "2");
        fs::write(&p2, "blocker").unwrap();
        let r = restore_entries(
            &f.store,
            &[(e1, p1.clone()), (e2.clone(), p2.clone())],
            OnConflict::Report,
        );
        assert_eq!(r.restored, vec![p1.clone()]);
        assert_eq!(r.conflicts, vec![(e2, p2.clone())]);
        assert!(r.failed.is_empty());
        assert_eq!(fs::read_to_string(&p1).unwrap(), "1");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "blocker");
    }

    #[test]
    fn restore_entries_rename_picks_free_numbered_name() {
        let f = fixture();
        let (e1, p) = trashed(&f, "a.txt", "one");
        fs::write(&p, "blocker").unwrap();
        let (e2, _) = trashed(&f, "a.txt", "two");
        fs::write(&p, "blocker").unwrap();
        let jobs = [(e1, p.clone()), (e2, p.clone())];
        let r = restore_entries(&f.store, &jobs, OnConflict::Rename);
        let w = f.root.join("work");
        assert_eq!(
            r.restored,
            vec![w.join("a.txt (복원됨)"), w.join("a.txt (복원됨 2)")]
        );
        assert!(r.conflicts.is_empty() && r.failed.is_empty());
        assert_eq!(fs::read_to_string(&p).unwrap(), "blocker");
        let mut got = [
            fs::read_to_string(w.join("a.txt (복원됨)")).unwrap(),
            fs::read_to_string(w.join("a.txt (복원됨 2)")).unwrap(),
        ];
        got.sort();
        assert_eq!(got, ["one", "two"]);
    }

    #[test]
    fn restore_entries_missing_item_fails_without_retry() {
        let f = fixture();
        let (e, p) = trashed(&f, "a.txt", "1");
        f.store.purge(std::slice::from_ref(&e)).unwrap();
        let r = restore_entries(&f.store, &[(e.clone(), p)], OnConflict::Report);
        assert!(r.restored.is_empty());
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].entry, e);
        assert!(!r.failed[0].retry);
        assert!(r.failed[0].message.contains("a.txt"));
    }

    #[test]
    fn build_jobs_uses_original_path_or_fallback() {
        let with = entry("1", "/home/u", "a.txt", 1);
        let mut without = entry("2", "/x", "b.txt", 1);
        without.original_parent = None;
        let jobs = build_jobs(&[with.clone(), without.clone()], Some(Path::new("/dst")));
        assert_eq!(jobs[0].1, PathBuf::from("/home/u/a.txt"));
        assert_eq!(jobs[1].1, PathBuf::from("/dst/b.txt"));
        let jobs = build_jobs(&[with, without], None);
        assert_eq!(jobs.len(), 1, "no fallback: unplaceable entry skipped");
    }

    // ---- time ----

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

    // ---- view ----

    fn view_with(n: usize) -> TrashView {
        let mut v = TrashView::new(PathBuf::from("/prev"));
        v.set_entries(
            (0..n)
                .map(|i| entry(&format!("id{i}"), "/p", &format!("n{i}"), i as i64))
                .collect(),
        );
        v
    }

    #[test]
    fn view_set_entries_clears_loading_semantics_and_clamps_cursor() {
        let mut v = view_with(5);
        v.cursor = 4;
        v.set_entries(vec![
            entry("id0", "/p", "n0", 0),
            entry("id1", "/p", "n1", 1),
        ]);
        assert_eq!(v.cursor, 1);
        let empty: Vec<TrashEntry> = vec![];
        v.set_entries(empty);
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
            entry("id2", "/p", "n2", 2),
            entry("id3", "/p", "n3", 3),
        ]);
        assert_eq!(v.current().unwrap().id, "id2");
        assert_eq!(v.selected, HashSet::from(["id2".to_string()]));
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
        assert_eq!(v.targets()[0].id, "id0");
        v.toggle_current(); // select id0, cursor -> 1
        v.move_down();
        v.toggle_current(); // select id2, cursor -> 3
        let ids: Vec<_> = v.targets().into_iter().map(|e| e.id).collect();
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

    // ---- real platform trash (run manually: cargo test -- --ignored) ----

    #[test]
    #[ignore = "touches the real OS trash"]
    fn real_trash_roundtrip() {
        let store = system_store();
        let tmp = tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let name = format!("hdir-trash-test-{}.txt", std::process::id());
        let file = dir.join(&name);
        fs::write(&file, "payload").unwrap();
        let made = store.trash(std::slice::from_ref(&file)).unwrap();
        assert!(!file.exists());
        let listed = store.list().unwrap().entries;
        let mine = listed
            .iter()
            .find(|e| e.name == name && e.original_parent.as_deref() == Some(dir.as_path()))
            .unwrap_or_else(|| panic!("{name} not listed"))
            .clone();
        if let Some(m) = made.first() {
            assert_eq!(m.id, mine.id);
        }
        store.restore(&mine, &file).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "payload");
        assert!(
            !store
                .list()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.id == mine.id)
        );
        // purge path
        let again = store.trash(std::slice::from_ref(&file)).unwrap();
        let listed = store.list().unwrap().entries;
        let mine = listed
            .iter()
            .find(|e| e.name == name && e.original_parent.as_deref() == Some(dir.as_path()))
            .unwrap()
            .clone();
        let _ = again;
        store.purge(std::slice::from_ref(&mine)).unwrap();
        assert!(
            !store
                .list()
                .unwrap()
                .entries
                .iter()
                .any(|e| e.id == mine.id)
        );
    }
}
