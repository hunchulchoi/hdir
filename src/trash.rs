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
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// One item in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// Backend identifier (Linux: `.trashinfo` path, macOS: path inside the
    /// trash). Raw path, so non-UTF-8 names survive.
    pub id: PathBuf,
    /// Original file name for display (lossy UTF-8).
    pub name: String,
    /// Original file name, raw.
    pub os_name: OsString,
    pub original_parent: Option<PathBuf>,
    /// Unix seconds.
    pub deleted_at: Option<i64>,
    pub size: Option<u64>,
    pub is_dir: bool,
}

impl TrashEntry {
    pub fn original_path(&self) -> Option<PathBuf> {
        self.original_parent.as_ref().map(|p| p.join(&self.os_name))
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

/// Result of trashing several paths: what worked (and can be undone), what
/// failed, and a warning when the items moved but bookkeeping did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashOutcome {
    pub entries: Vec<TrashEntry>,
    pub errors: Vec<(PathBuf, String)>,
    pub warning: Option<String>,
}

pub trait TrashStore: Send + Sync {
    /// Move `paths` to the trash. Partial results are kept: entries may be
    /// empty when the backend cannot identify them.
    fn trash(&self, paths: &[PathBuf]) -> io::Result<TrashOutcome>;
    /// Newest first.
    fn list(&self) -> io::Result<TrashListing>;
    /// Move the item to `dest` (full target path). Never overwrites:
    /// `AlreadyExists` when `dest` exists.
    fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()>;
    /// Like [`restore`](Self::restore) but bookkeeping cleanup is deferred to
    /// [`finish_restore`](Self::finish_restore) so a batch cleans up once.
    fn restore_move(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        self.restore(entry, dest)
    }
    /// Clean up after `done` (entry, restored path) were restored; returns
    /// warnings for failures that do not undo the restore.
    fn finish_restore(&self, _done: &[(TrashEntry, PathBuf)]) -> Vec<String> {
        Vec::new()
    }
    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()>;
}

pub const NO_PERMISSION_MSG: &str = "휴지통 접근 권한 없음 (전체 디스크 접근 권한 필요)";

fn is_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1)
}

// ---- index records (pure IO helpers) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(with = "pathser")]
    pub id: PathBuf,
    #[serde(with = "pathser")]
    pub trashed_path: PathBuf,
    #[serde(with = "pathser")]
    pub original_path: PathBuf,
    pub deleted_at: i64,
    pub size: Option<u64>,
    pub is_dir: bool,
}

/// JSON form of a path: a string when valid UTF-8, else an array of bytes
/// (serde refuses non-UTF-8 paths, which Linux file names can be).
mod pathser {
    use super::*;
    use serde::{Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Bytes(Vec<u8>),
    }

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        match p.to_str() {
            Some(t) => s.serialize_str(t),
            None => s.collect_seq(p.as_os_str().as_bytes()),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(match Repr::deserialize(d)? {
            Repr::Text(t) => PathBuf::from(t),
            Repr::Bytes(b) => PathBuf::from(OsString::from_vec(b)),
        })
    }
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
        .mode(0o600)
        .open(path)?;
    // The index lists original paths: keep it private, repairing old files.
    if f.metadata()?.permissions().mode() & 0o077 != 0 {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(out.as_bytes())
}

/// Remove the records with these ids by rewriting the file (temp + rename).
pub fn drop_records(path: &Path, ids: &[PathBuf]) -> io::Result<()> {
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
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let write = || -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(out.as_bytes())?;
        fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
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
            id: f.path.clone(),
            name: file_name_lossy(&f.path),
            os_name: os_file_name(&f.path),
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
        os_name: os_file_name(&r.original_path),
        original_parent: r.original_path.parent().map(Path::to_path_buf),
        deleted_at: Some(r.deleted_at),
        size: r.size.or(found.and_then(|f| f.size)),
        is_dir: r.is_dir || found.is_some_and(|f| f.is_dir),
    }
}

fn os_file_name(p: &Path) -> OsString {
    p.file_name()
        .map_or_else(|| p.as_os_str().to_os_string(), OsStr::to_os_string)
}

fn file_name_lossy(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// `name (복원됨)` for `n == 1`, `name (복원됨 2)` for `n == 2`, ...
pub fn restore_name(name: &str, n: usize) -> String {
    format!("{name}{}", restore_suffix(n))
}

fn restore_suffix(n: usize) -> String {
    if n <= 1 {
        " (복원됨)".to_string()
    } else {
        format!(" (복원됨 {n})")
    }
}

/// [`restore_name`] for raw (possibly non-UTF-8) names.
pub fn restore_os_name(name: &OsStr, n: usize) -> OsString {
    let mut out = name.to_os_string();
    out.push(restore_suffix(n));
    out
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

/// Appends records to the index (injectable for tests).
pub type Appender = Box<dyn Fn(&Path, &[Record]) -> io::Result<()> + Send + Sync>;

/// Removes records by id from the index (injectable for tests).
pub type Dropper = Box<dyn Fn(&Path, &[PathBuf]) -> io::Result<()> + Send + Sync>;

pub struct IndexTrash {
    trash_dir: PathBuf,
    scanner: Scanner,
    index_path: PathBuf,
    trasher: Trasher,
    clock: fn() -> i64,
    appender: Appender,
    dropper: Dropper,
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
            appender: Box::new(append_records),
            dropper: Box::new(drop_records),
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

    pub fn with_appender(mut self, appender: Appender) -> Self {
        self.appender = appender;
        self
    }

    pub fn with_dropper(mut self, dropper: Dropper) -> Self {
        self.dropper = dropper;
        self
    }

    fn drop_ids(&self, entries: &[TrashEntry]) -> io::Result<()> {
        let ids: Vec<PathBuf> = entries.iter().map(|e| e.id.clone()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        (self.dropper)(&self.index_path, &ids)
    }
}

impl TrashStore for IndexTrash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<TrashOutcome> {
        let mut records = Vec::new();
        let mut errors = Vec::new();
        for p in paths {
            let abs = match std::path::absolute(p) {
                Ok(a) => a,
                Err(e) => {
                    errors.push((p.clone(), e.to_string()));
                    continue;
                }
            };
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(e) => {
                    errors.push((abs, e.to_string()));
                    continue;
                }
            };
            match (self.trasher)(&abs) {
                Ok(trashed) => records.push(Record {
                    id: trashed.clone(),
                    trashed_path: trashed,
                    original_path: abs,
                    deleted_at: (self.clock)(),
                    size: (!meta.is_dir()).then_some(meta.len()),
                    is_dir: meta.is_dir(),
                }),
                Err(e) => errors.push((abs, e.to_string())),
            }
        }
        // Whatever was trashed is recorded even when other paths failed. A
        // failed index write keeps the entries (for undo) but is reported.
        let warning = (self.appender)(&self.index_path, &records)
            .err()
            .map(|e| format!("휴지통으로 이동했지만 복원 정보 저장 실패: {e}"));
        Ok(TrashOutcome {
            entries: records.iter().map(|r| entry_from_record(r, None)).collect(),
            errors,
            warning,
        })
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
        move_no_replace(&entry.id, dest)?;
        // A stale record is filtered out by `list` anyway.
        let _ = self.drop_ids(std::slice::from_ref(entry));
        Ok(())
    }

    fn restore_move(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        move_no_replace(&entry.id, dest)
    }

    fn finish_restore(&self, done: &[(TrashEntry, PathBuf)]) -> Vec<String> {
        let entries: Vec<TrashEntry> = done.iter().map(|(e, _)| e.clone()).collect();
        match self.drop_ids(&entries) {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("복원됨, 복원 정보 정리 실패: {e}")],
        }
    }

    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()> {
        let mut first_err = None;
        let mut done = Vec::new();
        for e in entries {
            match remove_any(&e.id) {
                Ok(()) => done.push(e.clone()),
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }
        let _ = self.drop_ids(&done);
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
    fn trash(&self, _: &[PathBuf]) -> io::Result<TrashOutcome> {
        Err(io::Error::other("홈 폴더를 찾을 수 없음"))
    }
    fn list(&self) -> io::Result<TrashListing> {
        self.trash(&[]).map(|_| TrashListing::default())
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
    before: &HashSet<PathBuf>,
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

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode a path for a `.trashinfo` `Path=` line (raw bytes).
pub fn encode_trash_path(p: &Path) -> String {
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
    out
}

pub fn decode_trash_path(s: &str) -> PathBuf {
    let bytes = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(OsString::from_vec(out))
}

/// `<$XDG_DATA_HOME or ~/.local/share>/Trash`.
pub fn default_root_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    match xdg.filter(|p| p.is_absolute()) {
        Some(x) => Some(x.join("Trash")),
        None => home.map(|h| h.join(".local/share/Trash")),
    }
}

pub fn default_root() -> Option<PathBuf> {
    default_root_from(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn local_tm(ts: i64) -> Option<libc::tm> {
    let t = ts as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    let ok = !unsafe { libc::localtime_r(&t, &mut tm) }.is_null();
    ok.then_some(tm)
}

/// `YYYY-MM-DDThh:mm:ss` in local time (the `DeletionDate=` format).
pub fn format_deletion_date(ts: i64) -> String {
    match local_tm(ts) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => "1970-01-01T00:00:00".to_string(),
    }
}

pub fn parse_deletion_date(s: &str) -> Option<i64> {
    let (d, t) = s.trim().split_once('T')?;
    let mut dp = d.split('-').map(|x| x.parse::<i32>().ok());
    let mut tp = t.split(':').map(|x| x.parse::<i32>().ok());
    let (y, mo, da) = (dp.next()??, dp.next()??, dp.next()??);
    let (h, mi, se) = (tp.next()??, tp.next()??, tp.next()??);
    // SAFETY: `tm` is plain data and is fully initialised below.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = y - 1900;
    tm.tm_mon = mo - 1;
    tm.tm_mday = da;
    tm.tm_hour = h;
    tm.tm_min = mi;
    tm.tm_sec = se;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a valid, exclusive pointer.
    let t = unsafe { libc::mktime(&mut tm) };
    (t != -1).then_some(t as i64)
}

/// A `.trashinfo` file: original path and deletion time.
fn parse_trashinfo(text: &str) -> (Option<PathBuf>, Option<i64>) {
    let (mut path, mut date) = (None, None);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Path=") {
            path = Some(decode_trash_path(v));
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            date = parse_deletion_date(v);
        }
    }
    (path, date)
}

/// All entries of the freedesktop trash at `root` (newest first).
fn list_root(root: &Path) -> io::Result<Vec<TrashEntry>> {
    let infos = match fs::read_dir(root.join("info")) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for ent in infos.filter_map(Result::ok) {
        let info = ent.path();
        if info.extension().is_none_or(|x| x != "trashinfo") {
            continue;
        }
        let Ok(text) = fs::read(&info) else { continue };
        let (path, date) = parse_trashinfo(&String::from_utf8_lossy(&text));
        let Some(orig) = path else { continue };
        let meta = fs::symlink_metadata(freedesktop_file(&info)).ok();
        out.push(TrashEntry {
            id: info,
            name: file_name_lossy(&orig),
            os_name: os_file_name(&orig),
            original_parent: orig.parent().map(Path::to_path_buf),
            deleted_at: date,
            size: meta.as_ref().filter(|m| !m.is_dir()).map(|m| m.len()),
            is_dir: meta.is_some_and(|m| m.is_dir()),
        });
    }
    out.sort_by(|a, b| {
        b.deleted_at
            .cmp(&a.deleted_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

const MAX_NAME_TRIES: usize = 10_000;

/// Move `p` into the freedesktop trash at `root` (same filesystem only).
fn trash_into_root(root: &Path, p: &Path) -> io::Result<()> {
    let abs = std::path::absolute(p)?;
    let name = abs
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "이름 없는 경로"))?
        .to_os_string();
    // Record the canonical parent, as the `trash` crate does.
    let orig = match abs.parent().map(fs::canonicalize) {
        Some(Ok(parent)) => parent.join(&name),
        _ => abs.clone(),
    };
    fs::symlink_metadata(&abs)?;
    let (files, infos) = (root.join("files"), root.join("info"));
    for d in [&files, &infos] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d)?;
    }
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_trash_path(&orig),
        format_deletion_date(now_secs())
    );
    for i in 1..=MAX_NAME_TRIES {
        let mut cand = name.clone();
        if i > 1 {
            cand.push(format!(".{i}"));
        }
        let mut info_name = cand.clone();
        info_name.push(".trashinfo");
        let info = infos.join(info_name);
        let mut f = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let target = files.join(&cand);
        let moved = if fs::symlink_metadata(&target).is_ok() {
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        } else {
            f.write_all(body.as_bytes())
                .and_then(|()| fs::rename(&abs, &target))
        };
        match moved {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&info);
                if e.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(io::Error::other("휴지통 이름 충돌이 너무 많음"))
}

/// Lists every entry of a freedesktop trash (injectable for tests).
pub type Lister = Box<dyn Fn() -> io::Result<Vec<TrashEntry>> + Send + Sync>;

/// Moves one path into a freedesktop trash.
pub type PathTrasher = Box<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

/// Freedesktop-layout trash; trashing and listing are injectable.
pub struct FreedesktopTrash {
    lister: Lister,
    trasher: PathTrasher,
}

impl FreedesktopTrash {
    pub fn with_parts(lister: Lister, trasher: PathTrasher) -> Self {
        Self { lister, trasher }
    }

    /// A self-contained freedesktop trash rooted at `root` (files/ + info/).
    /// Trashing only works within one filesystem.
    pub fn with_root(root: PathBuf) -> Self {
        let list_root_dir = root.clone();
        Self::with_parts(
            Box::new(move || list_root(&list_root_dir)),
            Box::new(move |p| trash_into_root(&root, p)),
        )
    }

    /// `None`: the system trash (all volumes, via the `trash` crate); `Some`:
    /// a trash at that root (default location: [`default_root`]).
    #[cfg(not(target_os = "macos"))]
    pub fn new(root: Option<PathBuf>) -> Self {
        root.map_or_else(Self::system, Self::with_root)
    }

    /// The system trash (all volumes) through the `trash` crate.
    #[cfg(not(target_os = "macos"))]
    pub fn system() -> Self {
        Self::with_parts(
            Box::new(system_entries),
            Box::new(|p| trash::delete(p).map_err(io::Error::other)),
        )
    }
}

#[cfg(not(target_os = "macos"))]
fn system_entries() -> io::Result<Vec<TrashEntry>> {
    let items = trash::os_limited::list().map_err(io::Error::other)?;
    let mut out: Vec<TrashEntry> = items
        .into_iter()
        .map(|it| {
            let info = PathBuf::from(&it.id);
            let meta = fs::symlink_metadata(freedesktop_file(&info)).ok();
            TrashEntry {
                id: info,
                name: it.name.to_string_lossy().into_owned(),
                os_name: it.name,
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

const UNDO_UNKNOWN_WARNING: &str = "되돌리기 정보 없음 (휴지통 목록 조회 실패)";
const UNDO_UNMATCHED_WARNING: &str = "되돌리기 정보 없음 (휴지통 항목 식별 실패)";

impl TrashStore for FreedesktopTrash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<TrashOutcome> {
        let mut out = TrashOutcome::default();
        let before = (self.lister)();
        let mut done: Vec<PathBuf> = Vec::new();
        for p in paths {
            // The trash crate records the canonical parent as the original location.
            let abs = std::path::absolute(p).unwrap_or_else(|_| p.clone());
            let norm = match (abs.parent().map(fs::canonicalize), abs.file_name()) {
                (Some(Ok(parent)), Some(name)) => parent.join(name),
                _ => abs,
            };
            match (self.trasher)(p) {
                Ok(()) => done.push(norm),
                Err(e) => out.errors.push((p.clone(), e.to_string())),
            }
        }
        if done.is_empty() {
            return Ok(out);
        }
        match (before, (self.lister)()) {
            (Ok(before), Ok(after)) => {
                let before: HashSet<PathBuf> = before.into_iter().map(|e| e.id).collect();
                out.entries = match_trashed(&done, &before, after);
                if out.entries.len() < done.len() {
                    out.warning = Some(UNDO_UNMATCHED_WARNING.into());
                }
            }
            _ => out.warning = Some(UNDO_UNKNOWN_WARNING.into()),
        }
        Ok(out)
    }

    fn list(&self) -> io::Result<TrashListing> {
        (self.lister)().map(TrashListing::new)
    }

    fn restore(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        self.restore_move(entry, dest)?;
        let _ = self.finish_restore(&[(entry.clone(), dest.to_path_buf())]);
        Ok(())
    }

    fn restore_move(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
        move_no_replace(&freedesktop_file(&entry.id), dest)
    }

    fn finish_restore(&self, done: &[(TrashEntry, PathBuf)]) -> Vec<String> {
        done.iter()
            .filter_map(|(e, dest)| match fs::remove_file(&e.id) {
                Ok(()) => None,
                Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                Err(err) => Some(format!(
                    "복원됨 {} (.trashinfo 삭제 실패: {err})",
                    dest.display()
                )),
            })
            .collect()
    }

    fn purge(&self, entries: &[TrashEntry]) -> io::Result<()> {
        let mut first = None;
        for e in entries {
            if let Err(err) = freedesktop_purge(&e.id) {
                first.get_or_insert(err);
            }
        }
        first.map_or(Ok(()), Err)
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
        Arc::new(FreedesktopTrash::system())
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
    /// Restored, but cleanup (index / `.trashinfo`) failed.
    pub warnings: Vec<String>,
}

/// Result of [`purge_verified`].
#[derive(Debug, Default)]
pub struct PurgeReport {
    pub purged: usize,
    /// Requested entries that no longer match the trash listing.
    pub skipped: Vec<TrashEntry>,
    pub error: Option<io::Error>,
}

/// Purge `entries`, but only those still identical (id, deleted_at, size) to
/// the backend's current listing, so an id reused by a newer item is safe.
pub fn purge_verified(store: &dyn TrashStore, entries: &[TrashEntry]) -> PurgeReport {
    let current = match store.list() {
        Ok(l) => l.entries,
        Err(e) => {
            return PurgeReport {
                purged: 0,
                skipped: entries.to_vec(),
                error: Some(e),
            };
        }
    };
    let (ok, skipped): (Vec<TrashEntry>, Vec<TrashEntry>) =
        entries.iter().cloned().partition(|e| {
            current
                .iter()
                .any(|c| c.id == e.id && c.deleted_at == e.deleted_at && c.size == e.size)
        });
    let error = if ok.is_empty() {
        None
    } else {
        store.purge(&ok).err()
    };
    PurgeReport {
        purged: if error.is_some() { 0 } else { ok.len() },
        skipped,
        error,
    }
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
    let mut done: Vec<(TrashEntry, PathBuf)> = Vec::new();
    for (entry, dest) in jobs {
        let mut result = store.restore_move(entry, dest).map(|()| dest.clone());
        if mode == OnConflict::Rename
            && matches!(&result, Err(e) if e.kind() == io::ErrorKind::AlreadyExists)
        {
            let name = os_file_name(dest);
            for n in 1..=MAX_RENAME_TRIES {
                let alt = dest.with_file_name(restore_os_name(&name, n));
                match store.restore_move(entry, &alt) {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    other => {
                        result = other.map(|()| alt);
                        break;
                    }
                }
            }
        }
        match result {
            Ok(path) => {
                done.push((entry.clone(), path.clone()));
                report.restored.push(path);
            }
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
    if !done.is_empty() {
        report.warnings = store.finish_restore(&done);
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
                .or_else(|| fallback_dir.map(|d| d.join(&e.os_name)))
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
    pub selected: HashSet<PathBuf>,
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
        let ids: HashSet<&Path> = self.entries.iter().map(|e| e.id.as_path()).collect();
        self.selected.retain(|id| ids.contains(id.as_path()));
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
        fn trash(&self, paths: &[PathBuf]) -> io::Result<TrashOutcome> {
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
        fn restore_move(&self, entry: &TrashEntry, dest: &Path) -> io::Result<()> {
            self.inner.restore_move(entry, dest)
        }
        fn finish_restore(&self, done: &[(TrashEntry, PathBuf)]) -> Vec<String> {
            self.inner.finish_restore(done)
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
            id: trashed.clone(),
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
        assert_eq!(got[3].id, Path::new("/trash/zeta"));
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
        let entries = f.store.trash(&[file.clone(), dir.clone()]).unwrap().entries;
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
            .entries
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
            .entries
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
        let e = f
            .store
            .trash(std::slice::from_ref(&dir))
            .unwrap()
            .entries
            .remove(0);
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
        let es = f.store.trash(&[file, dir]).unwrap().entries;
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
        let es = f.store.trash(&[a, b]).unwrap().entries;
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
        let es = f.store.trash(&[a, b]).unwrap().entries;
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
            os_name: name.into(),
            original_parent: Some(PathBuf::from(parent)),
            deleted_at: Some(at),
            size: None,
            is_dir: false,
        }
    }

    #[test]
    fn match_trashed_picks_new_entries_for_given_paths() {
        let before: HashSet<PathBuf> = [PathBuf::from("i1")].into();
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
        let ids: Vec<_> = got.iter().map(|e| e.id.to_str().unwrap()).collect();
        assert_eq!(ids, ["i3", "i4"]);
    }

    // ---- restore orchestration ----

    fn trashed(f: &Fixture, name: &str, content: &str) -> (TrashEntry, PathBuf) {
        let p = f.root.join("work").join(name);
        fs::write(&p, content).unwrap();
        let e = f
            .store
            .trash(std::slice::from_ref(&p))
            .unwrap()
            .entries
            .remove(0);
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
        assert_eq!(v.current().unwrap().id, Path::new("id2"));
        assert_eq!(v.selected, HashSet::from([PathBuf::from("id2")]));
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
        assert_eq!(v.targets()[0].id, Path::new("id0"));
        v.toggle_current(); // select id0, cursor -> 1
        v.move_down();
        v.toggle_current(); // select id2, cursor -> 3
        let ids: Vec<_> = v.targets().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, [PathBuf::from("id0"), PathBuf::from("id2")]);
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

    // ---- review fixes ----

    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn store_with(f: &Fixture, trasher: Trasher) -> IndexTrash {
        IndexTrash::new(f.trash_dir.clone(), f.index.clone(), trasher).with_clock(|| 1_700_000_000)
    }

    // fix 1: partial results

    #[test]
    fn trash_keeps_earlier_items_when_a_later_path_vanishes() {
        let f = fixture();
        let a = f.root.join("work/a.txt");
        let c = f.root.join("work/c.txt");
        fs::write(&a, "1").unwrap();
        fs::write(&c, "3").unwrap();
        let gone = f.root.join("work/gone.txt");
        let out = f.store.trash(&[a, gone.clone(), c]).unwrap();
        let names: Vec<_> = out.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "c.txt"]);
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].0, gone);
        assert_eq!(read_index(&f.index).len(), 2, "recorded in the index");
    }

    #[test]
    fn trash_keeps_first_item_when_trasher_fails_on_second() {
        let f = fixture();
        let inner = mover(f.trash_dir.clone());
        let store = store_with(
            &f,
            Box::new(move |p| {
                if p.file_name().is_some_and(|n| n == "b.txt") {
                    return Err(io::Error::other("nope"));
                }
                inner(p)
            }),
        );
        let a = f.root.join("work/a.txt");
        let b = f.root.join("work/b.txt");
        fs::write(&a, "1").unwrap();
        fs::write(&b, "2").unwrap();
        let out = store.trash(&[a, b.clone()]).unwrap();
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].name, "a.txt");
        assert_eq!(out.errors, vec![(b.clone(), "nope".to_string())]);
        assert!(b.exists());
        assert_eq!(read_index(&f.index).len(), 1);
    }

    // fix 2: index write errors + permissions

    #[test]
    fn index_append_failure_is_reported_and_entries_kept() {
        let f = fixture();
        let store = store_with(&f, mover(f.trash_dir.clone()))
            .with_appender(Box::new(|_, _| Err(io::Error::other("disk full"))));
        let a = f.root.join("work/a.txt");
        fs::write(&a, "1").unwrap();
        let out = store.trash(&[a]).unwrap();
        assert_eq!(out.entries.len(), 1, "kept in memory for undo");
        let w = out.warning.expect("warning");
        assert!(
            w.starts_with("휴지통으로 이동했지만 복원 정보 저장 실패:") && w.contains("disk full"),
            "{w}"
        );
    }

    #[test]
    fn index_file_is_created_private_and_existing_perms_repaired() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("sub/idx.jsonl");
        let a = rec(tmp.path(), "a", "/x/a", 1);
        append_records(&path, std::slice::from_ref(&a)).unwrap();
        assert_eq!(mode(&path), 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        append_records(&path, std::slice::from_ref(&a)).unwrap();
        assert_eq!(mode(&path), 0o600, "append repairs perms");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        drop_records(&path, &[PathBuf::from("zzz")]).unwrap();
        assert_eq!(mode(&path), 0o600, "rewrite is private too");
    }

    #[test]
    fn drop_records_replaces_a_stale_world_readable_tmp() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("idx.jsonl");
        append_records(&path, &[rec(tmp.path(), "a", "/x/a", 1)]).unwrap();
        let stale = tmp.path().join("idx.jsonl.tmp");
        fs::write(&stale, "old").unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
        drop_records(&path, &[PathBuf::from("zzz")]).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert!(!stale.exists());
    }

    // fix 3: non-UTF-8 names

    fn bad_name() -> std::ffi::OsString {
        OsStr::from_bytes(b"bad\xff").to_os_string()
    }

    #[test]
    fn record_with_non_utf8_paths_roundtrips_through_json() {
        let dir = Path::new("/trash");
        let mut r = rec(dir, "x", "/orig/x", 5);
        r.id = dir.join(bad_name());
        r.trashed_path = dir.join(bad_name());
        r.original_path = Path::new("/orig").join(bad_name());
        let line = serde_json::to_string(&r).expect("serializes");
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), r);
    }

    #[test]
    fn non_utf8_index_roundtrip_keeps_raw_names() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("idx.jsonl");
        let mut r = rec(tmp.path(), "x", "/orig/x", 5);
        r.original_path = Path::new("/orig").join(bad_name());
        append_records(&path, std::slice::from_ref(&r)).unwrap();
        assert_eq!(read_index(&path), vec![r.clone()]);
        let e = entry_from_record(&r, None);
        assert_eq!(e.os_name, bad_name());
        assert_eq!(e.original_path().unwrap(), r.original_path);
        assert!(e.name.contains('\u{FFFD}'), "display is lossy");
    }

    #[test]
    fn restore_rename_keeps_raw_bytes_of_non_utf8_dest() {
        struct Rec(std::sync::Mutex<Vec<PathBuf>>);
        impl TrashStore for Rec {
            fn trash(&self, _: &[PathBuf]) -> io::Result<TrashOutcome> {
                unreachable!()
            }
            fn list(&self) -> io::Result<TrashListing> {
                unreachable!()
            }
            fn restore(&self, _: &TrashEntry, dest: &Path) -> io::Result<()> {
                let mut v = self.0.lock().unwrap();
                v.push(dest.to_path_buf());
                if v.len() == 1 {
                    Err(io::Error::from(io::ErrorKind::AlreadyExists))
                } else {
                    Ok(())
                }
            }
            fn purge(&self, _: &[TrashEntry]) -> io::Result<()> {
                Ok(())
            }
        }
        let store = Rec(Default::default());
        let mut e = entry("id", "/p", "x", 1);
        e.os_name = bad_name();
        let dest = Path::new("/p").join(bad_name());
        restore_entries(&store, &[(e, dest.clone())], OnConflict::Rename);
        let tried = store.0.lock().unwrap().clone();
        let mut want = dest.into_os_string();
        want.push(" (복원됨)");
        assert_eq!(tried[1].as_os_str(), want.as_os_str());
    }

    #[test]
    fn build_jobs_fallback_uses_raw_name() {
        let mut e = entry("id", "/x", "n", 1);
        e.original_parent = None;
        e.os_name = bad_name();
        let jobs = build_jobs(&[e], Some(Path::new("/dst")));
        assert_eq!(jobs[0].1, Path::new("/dst").join(bad_name()));
    }

    #[test]
    fn non_utf8_file_through_index_trash_when_fs_allows() {
        let f = fixture();
        let p = f.root.join("work").join(bad_name());
        if fs::write(&p, "raw").is_err() {
            return; // APFS refuses invalid UTF-8 names; helpers covered above
        }
        let e = f
            .store
            .trash(std::slice::from_ref(&p))
            .unwrap()
            .entries
            .remove(0);
        assert_eq!(e.os_name, bad_name());
        let listed = f.store.list().unwrap().entries;
        assert_eq!(listed, vec![e.clone()]);
        f.store.restore(&e, &p).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "raw");
    }

    // fix 4: purge identity

    #[test]
    fn purge_verified_skips_entries_whose_id_was_reused() {
        let f = fixture();
        let p = f.root.join("work/a.txt");
        fs::write(&p, "old").unwrap();
        let stale = f
            .store
            .trash(std::slice::from_ref(&p))
            .unwrap()
            .entries
            .remove(0);
        f.store.purge(std::slice::from_ref(&stale)).unwrap();
        // a new item now occupies the same trash path (same id), later time
        fs::write(&p, "new-and-longer").unwrap();
        let store = IndexTrash::new(
            f.trash_dir.clone(),
            f.index.clone(),
            mover(f.trash_dir.clone()),
        )
        .with_clock(|| 1_700_000_999);
        let fresh = store
            .trash(std::slice::from_ref(&p))
            .unwrap()
            .entries
            .remove(0);
        assert_eq!(fresh.id, stale.id);
        let rep = purge_verified(&store, std::slice::from_ref(&stale));
        assert_eq!(rep.purged, 0);
        assert_eq!(rep.skipped, vec![stale]);
        assert!(rep.error.is_none());
        assert_eq!(store.list().unwrap().entries, vec![fresh.clone()]);
        let rep = purge_verified(&store, std::slice::from_ref(&fresh));
        assert_eq!(rep.purged, 1);
        assert!(store.list().unwrap().entries.is_empty());
    }

    // fix 5: cleanup failure after a successful restore

    fn fd_store(root: PathBuf) -> FreedesktopTrash {
        FreedesktopTrash::with_parts(
            Box::new(move || {
                Ok(vec![TrashEntry {
                    id: root.join("info/a.txt.trashinfo"),
                    name: "a.txt".into(),
                    os_name: "a.txt".into(),
                    original_parent: None,
                    deleted_at: Some(1),
                    size: None,
                    is_dir: false,
                }])
            }),
            Box::new(|_| Ok(())),
        )
    }

    #[test]
    fn restore_with_trashinfo_removal_failure_is_restored_with_warning() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("Trash");
        let info = fd_item(&root, "a.txt", "payload");
        fs::remove_file(&info).unwrap();
        fs::create_dir(&info).unwrap(); // remove_file on a directory fails
        fs::write(info.join("keep"), "x").unwrap();
        let store = fd_store(root.clone());
        let e = store.list().unwrap().entries.remove(0);
        let dest = tmp.path().join("out/a.txt");
        let r = restore_entries(&store, &[(e, dest.clone())], OnConflict::Report);
        assert_eq!(r.restored, vec![dest.clone()]);
        assert!(r.failed.is_empty() && r.conflicts.is_empty());
        assert_eq!(r.warnings.len(), 1);
        assert!(
            r.warnings[0].contains(&*dest.to_string_lossy()),
            "{:?}",
            r.warnings
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "payload");
    }

    // fix 6: list failures around trashing

    fn flaky_fd(fail_on: Vec<usize>) -> (FreedesktopTrash, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let store = FreedesktopTrash::with_parts(
            Box::new(move || {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if fail_on.contains(&n) {
                    Err(io::Error::other("list broke"))
                } else {
                    Ok(Vec::new())
                }
            }),
            Box::new(|p| fs::remove_file(p)),
        );
        (store, calls)
    }

    #[test]
    fn list_failure_before_or_after_trashing_warns_but_trashes() {
        for fail_on in [vec![0usize], vec![1]] {
            let (store, _) = flaky_fd(fail_on.clone());
            let tmp = tempdir().unwrap();
            let p = tmp.path().join("a.txt");
            fs::write(&p, "x").unwrap();
            let out = store
                .trash(std::slice::from_ref(&p))
                .expect("trash itself succeeds");
            assert!(!p.exists());
            assert!(out.entries.is_empty());
            assert_eq!(
                out.warning.as_deref(),
                Some("되돌리기 정보 없음 (휴지통 목록 조회 실패)"),
                "{fail_on:?}"
            );
        }
    }

    #[test]
    fn freedesktop_partial_trash_keeps_successes() {
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a.txt");
        fs::write(&a, "x").unwrap();
        let gone = tmp.path().join("gone.txt");
        let (store, _) = flaky_fd(vec![]);
        let out = store.trash(&[gone.clone(), a.clone()]).unwrap();
        assert!(!a.exists());
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].0, gone);
    }

    // fix 8: batched index updates

    fn counting_store(f: &Fixture) -> (IndexTrash, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&n);
        let store =
            store_with(f, mover(f.trash_dir.clone())).with_dropper(Box::new(move |p, ids| {
                c.fetch_add(1, Ordering::SeqCst);
                drop_records(p, ids)
            }));
        (store, n)
    }

    fn three_trashed(f: &Fixture, store: &IndexTrash) -> Vec<(TrashEntry, PathBuf)> {
        let paths: Vec<PathBuf> = ["a", "b", "c"]
            .iter()
            .map(|n| {
                let p = f.root.join("work").join(n);
                fs::write(&p, n).unwrap();
                p
            })
            .collect();
        let out = store.trash(&paths).unwrap();
        out.entries.into_iter().zip(paths).collect()
    }

    #[test]
    fn restore_of_many_rewrites_the_index_once() {
        let f = fixture();
        let (store, n) = counting_store(&f);
        let jobs = three_trashed(&f, &store);
        let r = restore_entries(&store, &jobs, OnConflict::Report);
        assert_eq!(r.restored.len(), 3);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(read_index(&f.index).is_empty());
    }

    #[test]
    fn purge_of_many_rewrites_the_index_once() {
        let f = fixture();
        let (store, n) = counting_store(&f);
        let entries: Vec<_> = three_trashed(&f, &store).into_iter().map(|j| j.0).collect();
        store.purge(&entries).unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(read_index(&f.index).is_empty());
    }

    // fix 9: isolated freedesktop root (auto-run roundtrip)

    #[test]
    fn freedesktop_root_roundtrip_trash_list_restore_purge() {
        let tmp = tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let store = FreedesktopTrash::with_root(base.join("Trash"));
        let work = base.join("work");
        fs::create_dir_all(&work).unwrap();
        let name = "한글 100% a b.txt";
        let file = work.join(name);
        fs::write(&file, "payload").unwrap();
        let out = store.trash(std::slice::from_ref(&file)).unwrap();
        assert!(!file.exists());
        assert!(out.errors.is_empty() && out.warning.is_none());
        assert_eq!(out.entries.len(), 1);
        let listed = store.list().unwrap().entries;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, out.entries[0].id);
        assert_eq!(listed[0].name, name);
        assert_eq!(listed[0].original_parent.as_deref(), Some(work.as_path()));
        assert_eq!(listed[0].size, Some(7));
        let info = fs::read_to_string(&listed[0].id).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath="), "{info}");
        assert!(info.contains("%25") && info.contains("%20"), "{info}");
        assert!(info.contains("\nDeletionDate="), "{info}");
        store.restore(&listed[0], &file).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "payload");
        assert!(store.list().unwrap().entries.is_empty());
        // same name again, then purge
        let again = store.trash(std::slice::from_ref(&file)).unwrap().entries;
        assert_eq!(again.len(), 1);
        store.purge(&again).unwrap();
        assert!(store.list().unwrap().entries.is_empty());
        assert_eq!(fs::read_dir(base.join("Trash/files")).unwrap().count(), 0);
    }

    #[test]
    fn freedesktop_root_same_names_get_distinct_ids() {
        let tmp = tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let store = FreedesktopTrash::with_root(base.join("Trash"));
        let mut ids = Vec::new();
        for _ in 0..2 {
            let p = base.join("a.txt");
            fs::write(&p, "x").unwrap();
            ids.push(store.trash(&[p]).unwrap().entries.remove(0).id);
        }
        assert_ne!(ids[0], ids[1]);
        assert_eq!(store.list().unwrap().entries.len(), 2);
    }

    #[test]
    fn trashinfo_path_encoding_roundtrips_raw_bytes() {
        let p = Path::new("/a b/%/한글").join(bad_name());
        let enc = encode_trash_path(&p);
        assert!(enc.is_ascii() && !enc.contains(' '), "{enc}");
        assert_eq!(decode_trash_path(&enc), p);
    }

    #[test]
    fn default_root_follows_xdg_data_home() {
        assert_eq!(
            default_root_from(Some("/x/data".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/data/Trash"))
        );
        assert_eq!(
            default_root_from(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/share/Trash"))
        );
    }

    #[test]
    fn deletion_date_parses_back_to_the_same_second() {
        let ts = 1_700_000_000;
        assert_eq!(parse_deletion_date(&format_deletion_date(ts)), Some(ts));
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
        assert_eq!(made.entries.len(), 1, "{:?}", made.warning);
        assert_eq!(made.entries[0].id, mine.id);
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
        assert_eq!(again.entries.len(), 1);
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
