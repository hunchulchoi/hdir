//! Pure filesystem operations (copy / move / mkdir / rename / trash). No UI.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Overwrite,
    Skip,
    Cancel,
}

pub trait OpControl {
    fn on_progress(&mut self, bytes_done_delta: u64, current: &Path);
    fn on_file_done(&mut self, _path: &Path) {}
    /// A top-level source finished (moved, copied or skipped).
    fn on_item_done(&mut self, _src: &Path) {}
    fn is_cancelled(&self) -> bool;
    fn on_conflict(&mut self, dest: &Path) -> ConflictAction;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    Done,
    Cancelled,
    Failed(String),
}

/// What a copy/move did, even when it did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpReport {
    pub outcome: OpOutcome,
    /// Exactly what was relocated: `(src, dest)`. A directory merged into an
    /// existing one records its children, not the directory.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Destination files that were replaced.
    pub overwritten: usize,
    /// Entries not processed (conflict skip, special files).
    pub skipped: usize,
    /// Top-level sources fully copied / moved.
    pub copied: usize,
    /// Top-level sources only partly moved (special files skipped); the source stays.
    pub partial: usize,
    /// Source directories removed after being merged into an existing one
    /// (so undo can recreate them).
    pub removed_dirs: Vec<PathBuf>,
    /// Non-fatal problems, e.g. a merged source directory that could not be removed.
    pub warnings: Vec<String>,
    /// Top-level sources that were fully copied / moved (not skipped, not partial).
    pub done_sources: Vec<PathBuf>,
}

impl OpReport {
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            outcome: OpOutcome::Failed(msg.into()),
            moved: vec![],
            overwritten: 0,
            skipped: 0,
            copied: 0,
            partial: 0,
            removed_dirs: vec![],
            warnings: vec![],
            done_sources: vec![],
        }
    }
}

#[derive(Default)]
struct Acc {
    moved: Vec<(PathBuf, PathBuf)>,
    overwritten: usize,
    skipped: usize,
    copied: usize,
    partial: usize,
    removed_dirs: Vec<PathBuf>,
    warnings: Vec<String>,
    /// Directories this operation created itself (the only ones it may delete
    /// when rolling back).
    created: Vec<PathBuf>,
    done_sources: Vec<PathBuf>,
}

impl Acc {
    fn into_report(self, outcome: OpOutcome) -> OpReport {
        OpReport {
            outcome,
            moved: self.moved,
            overwritten: self.overwritten,
            skipped: self.skipped,
            copied: self.copied,
            partial: self.partial,
            removed_dirs: self.removed_dirs,
            warnings: self.warnings,
            done_sources: self.done_sources,
        }
    }
}

const CHUNK: usize = 1 << 20;
const EXDEV: i32 = 18;

/// Result of one entry operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Done,
    Skipped,
    /// Moved only in part (special files were skipped); the source stays.
    Partial,
    /// Destination is complete but the source could only be removed in part.
    /// Never roll back: the destination may be the only full copy.
    SourceLeft,
    Cancelled,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(invalid("invalid file name"));
    }
    Ok(())
}

/// Recursive file/byte count. Symlinks count as one file, zero bytes, and are not followed.
pub fn measure(sources: &[PathBuf]) -> io::Result<Totals> {
    Ok(measure_cancellable(sources, &|| false)?.unwrap_or(Totals { files: 0, bytes: 0 }))
}

/// Like [`measure`] but polls `is_cancelled`; `Ok(None)` means cancelled.
pub fn measure_cancellable(
    sources: &[PathBuf],
    is_cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Totals>> {
    fn walk(p: &Path, t: &mut Totals, stop: &dyn Fn() -> bool) -> io::Result<bool> {
        if stop() {
            return Ok(false);
        }
        let m = fs::symlink_metadata(p)?;
        if m.is_dir() {
            for e in fs::read_dir(p)? {
                if !walk(&e?.path(), t, stop)? {
                    return Ok(false);
                }
            }
        } else {
            t.files += 1;
            if m.is_file() {
                t.bytes += m.len();
            }
        }
        Ok(true)
    }
    let mut t = Totals { files: 0, bytes: 0 };
    for s in sources {
        if !walk(s, &mut t, is_cancelled)? {
            return Ok(None);
        }
    }
    Ok(Some(t))
}

/// Canonical path of an entry without following the entry itself if it is a symlink.
fn canon_entry(p: &Path) -> io::Result<PathBuf> {
    let name = p
        .file_name()
        .ok_or_else(|| invalid("path has no file name"))?;
    let parent = match p.parent() {
        Some(x) if !x.as_os_str().is_empty() => x,
        _ => Path::new("."),
    };
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Validate `src` -> `dest_dir` and return the destination path.
fn check_target(src: &Path, dest_dir: &Path) -> io::Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| invalid("path has no file name"))?;
    let src_meta = fs::symlink_metadata(src)?;
    let src_c = canon_entry(src)?;
    let dest_c = fs::canonicalize(dest_dir)?.join(name);
    if src_c == dest_c {
        return Err(invalid("source and destination are the same"));
    }
    if src_meta.is_dir() && dest_c.starts_with(&src_c) {
        return Err(invalid("cannot copy a directory into itself"));
    }
    Ok(dest_dir.join(name))
}

fn check_all(sources: &[PathBuf], dest_dir: &Path) -> io::Result<Vec<PathBuf>> {
    sources.iter().map(|s| check_target(s, dest_dir)).collect()
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique sibling temp path (`.hdir-tmp-<pid>-<n>-<name>`), name capped to stay under NAME_MAX.
fn temp_sibling(dest: &Path) -> PathBuf {
    sibling_named(dest, "tmp")
}

/// Unique sibling path used to park an existing destination while it is being replaced.
fn backup_sibling(dest: &Path) -> PathBuf {
    sibling_named(dest, "bak")
}

/// Move `dest` aside to a fresh backup name without ever replacing anything.
fn park_aside(dest: &Path) -> io::Result<PathBuf> {
    park_aside_with(dest, &mut || backup_sibling(dest))
}

fn park_aside_with(dest: &Path, next: &mut dyn FnMut() -> PathBuf) -> io::Result<PathBuf> {
    for _ in 0..PARK_ATTEMPTS {
        let b = next();
        match rename_noreplace(dest, &b) {
            Ok(()) => return Ok(b),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "백업 이름을 정하지 못함",
    ))
}

const PARK_ATTEMPTS: usize = 16;

fn sibling_named(dest: &Path, kind: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = dest.file_name().map(|n| n.as_bytes()).unwrap_or_default();
    let name = &name[..name.len().min(100)];
    let mut tmp = OsString::from(format!(".hdir-{kind}-{}-{n}-", std::process::id()));
    tmp.push(OsString::from_vec(name.to_vec()));
    dest.with_file_name(tmp)
}

/// Copy file data to a temp file next to `dest`, then atomically rename over `dest`.
/// `dest` is never touched unless the new content is complete.
///
/// Unless the caller already `approved` replacing `dest`, the final rename never
/// clobbers: a file that appeared meanwhile goes through `on_conflict` again.
fn copy_file(
    src: &Path,
    dest: &Path,
    ctl: &mut dyn OpControl,
    acc: &mut Acc,
    approved: bool,
) -> io::Result<Step> {
    // The entry may have been swapped since the lstat: never follow a symlink
    // (O_NOFOLLOW) and never block on a FIFO (O_NONBLOCK), then trust fstat only.
    let mut input = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
    {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return skip_changed(src, acc);
        }
        Err(e) => return Err(e),
    };
    let meta = &input.metadata()?;
    if !meta.is_file() {
        return skip_changed(src, acc);
    }
    let tmp = temp_sibling(dest);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // Private until the data is complete; source permissions are applied below.
        .mode(0o600)
        .open(&tmp)?;
    let mut buf = vec![0u8; CHUNK];
    let res = (|| -> io::Result<bool> {
        loop {
            if ctl.is_cancelled() {
                return Ok(false);
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            ctl.on_progress(n as u64, src);
        }
        out.set_permissions(meta.permissions())?;
        out.set_modified(meta.modified()?)?;
        out.sync_all()?;
        Ok(true)
    })();
    drop(out);
    match res {
        Ok(true) => {
            let step = place(&tmp, dest, approved, ctl, acc)?;
            if step == Step::Done {
                ctl.on_file_done(dest);
            }
            Ok(step)
        }
        Ok(false) => {
            let _ = fs::remove_file(&tmp);
            Ok(Step::Cancelled)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn skip_changed(src: &Path, acc: &mut Acc) -> io::Result<Step> {
    acc.skipped += 1;
    acc.warnings.push(format!(
        "{}: 복사 중 파일 종류가 바뀌어 건너뜀",
        src.display()
    ));
    Ok(Step::Skipped)
}

/// Move the finished `tmp` to `dest`. Without `approved`, an existing `dest` is
/// never replaced silently: the user is asked (again). `tmp` is always consumed.
fn place(
    tmp: &Path,
    dest: &Path,
    approved: bool,
    ctl: &mut dyn OpControl,
    acc: &mut Acc,
) -> io::Result<Step> {
    let mut replaced = approved && exists(dest);
    let res = if approved {
        fs::rename(tmp, dest)
    } else {
        match rename_noreplace(tmp, dest) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match ctl.on_conflict(dest) {
                ConflictAction::Overwrite => {
                    replaced = true;
                    fs::rename(tmp, dest)
                }
                ConflictAction::Skip => {
                    let _ = fs::remove_file(tmp);
                    acc.skipped += 1;
                    return Ok(Step::Skipped);
                }
                ConflictAction::Cancel => {
                    let _ = fs::remove_file(tmp);
                    return Ok(Step::Cancelled);
                }
            },
            other => other,
        }
    };
    if let Err(e) = res {
        let _ = fs::remove_file(tmp);
        return Err(e);
    }
    if replaced {
        acc.overwritten += 1;
    }
    Ok(Step::Done)
}

/// Create a symlink at `dest` atomically (temp link + rename), see [`place`].
fn copy_symlink(
    src: &Path,
    dest: &Path,
    ctl: &mut dyn OpControl,
    acc: &mut Acc,
    approved: bool,
) -> io::Result<Step> {
    let tmp = temp_sibling(dest);
    std::os::unix::fs::symlink(fs::read_link(src)?, &tmp)?;
    let step = place(&tmp, dest, approved, ctl, acc)?;
    if step == Step::Done {
        ctl.on_file_done(dest);
    }
    Ok(step)
}

/// Put a replaced destination back after a failed/cancelled replacement.
///
/// Only a `dest` this operation created (`acc.created`) is discarded. If `dest`
/// holds anything else, both it and the backup are kept and the error names the
/// backup. The restore itself never clobbers.
fn restore_backup(backup: &Path, dest: &Path, acc: &mut Acc) -> io::Result<()> {
    if exists(dest) {
        if !acc.created.iter().any(|c| c == dest) {
            return Err(io::Error::other(format!(
                "대상에 다른 항목이 있어 원본을 복원하지 못함. 원본 백업: {}",
                backup.display()
            )));
        }
        let _ = remove_source(dest);
    }
    rename_noreplace(backup, dest).map_err(|e| {
        io::Error::other(format!(
            "원본 복원 실패: {e}. 원본 백업: {}",
            backup.display()
        ))
    })
}

fn copy_dir(
    src: &Path,
    dest: &Path,
    meta: &fs::Metadata,
    ctl: &mut dyn OpControl,
    acc: &mut Acc,
    mut approved: bool,
) -> io::Result<Step> {
    if ctl.is_cancelled() {
        return Ok(Step::Cancelled);
    }
    let mut backup = None;
    let created;
    loop {
        if approved && exists(dest) {
            if is_real_dir(dest) {
                created = false;
                break;
            }
            // Park the existing file; it is deleted only once the copy succeeded.
            backup = Some(park_aside(dest)?);
        }
        // Atomic: never adopts a directory that appeared since the conflict check.
        // Private until complete; the source mode is applied after the children.
        match fs::DirBuilder::new().mode(0o700).create(dest) {
            Ok(()) => {
                acc.created.push(dest.to_path_buf());
                created = true;
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && !approved => {
                match ctl.on_conflict(dest) {
                    ConflictAction::Skip => {
                        acc.skipped += 1;
                        return Ok(Step::Skipped);
                    }
                    ConflictAction::Cancel => return Ok(Step::Cancelled),
                    ConflictAction::Overwrite => approved = true,
                }
            }
            Err(e) => {
                if let Some(b) = &backup {
                    restore_backup(b, dest, acc)?;
                }
                return Err(e);
            }
        }
    }
    let skipped_before = acc.skipped;
    let mut run = || -> io::Result<Step> {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            if copy_entry(
                &entry.path(),
                &dest.join(entry.file_name()),
                ctl,
                true,
                false,
                acc,
            )? == Step::Cancelled
            {
                return Ok(Step::Cancelled);
            }
        }
        if created {
            fs::File::open(dest)?.set_modified(meta.modified()?)?;
            fs::set_permissions(dest, meta.permissions())?;
        }
        Ok(Step::Done)
    };
    let mut res = run();
    if let Some(b) = backup {
        if matches!(res, Ok(Step::Done)) && acc.skipped != skipped_before {
            // Incomplete copy: it must not replace the original. Drop it, put the
            // original back and report the item as partial.
            let _ = remove_source(dest);
            acc.created.retain(|c| c != dest);
            restore_backup(&b, dest, acc)?;
            acc.warnings.push(format!(
                "{}: 일부 항목을 건너뛰어 복사하지 않고 원본 파일을 유지함",
                dest.display()
            ));
            res = Ok(Step::Partial);
        } else if matches!(res, Ok(Step::Done)) {
            acc.overwritten += 1;
            let _ = fs::remove_file(&b);
        } else {
            restore_backup(&b, dest, acc)?;
        }
    }
    res
}

/// Copy one entry. With `prompt`, an existing destination triggers `on_conflict`;
/// `replace_ok` means the caller already approved replacing a file at `dest`.
/// FIFOs, sockets and device files are skipped (never opened).
fn copy_entry(
    src: &Path,
    dest: &Path,
    ctl: &mut dyn OpControl,
    prompt: bool,
    replace_ok: bool,
    acc: &mut Acc,
) -> io::Result<Step> {
    if ctl.is_cancelled() {
        return Ok(Step::Cancelled);
    }
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if !(ft.is_dir() || ft.is_file() || ft.is_symlink()) {
        acc.skipped += 1;
        return Ok(Step::Skipped);
    }
    let mut approved = replace_ok;
    if prompt && exists(dest) {
        match ctl.on_conflict(dest) {
            ConflictAction::Skip => {
                acc.skipped += 1;
                return Ok(Step::Skipped);
            }
            ConflictAction::Cancel => return Ok(Step::Cancelled),
            ConflictAction::Overwrite => approved = true,
        }
    }
    if ft.is_dir() {
        return copy_dir(src, dest, &meta, ctl, acc, approved);
    }
    if is_real_dir(dest) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination is a directory",
        ));
    }
    if ft.is_symlink() {
        copy_symlink(src, dest, ctl, acc, approved)
    } else {
        copy_file(src, dest, ctl, acc, approved)
    }
}

pub fn copy_items(sources: &[PathBuf], dest_dir: &Path, ctl: &mut dyn OpControl) -> OpReport {
    let mut acc = Acc::default();
    let outcome = match copy_loop(sources, dest_dir, ctl, &mut acc) {
        Ok(o) => o,
        Err(e) => OpOutcome::Failed(e.to_string()),
    };
    acc.into_report(outcome)
}

fn copy_loop(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    acc: &mut Acc,
) -> io::Result<OpOutcome> {
    let dests = check_all(sources, dest_dir)?;
    for (src, dest) in sources.iter().zip(&dests) {
        match copy_entry(src, dest, ctl, true, false, acc)? {
            Step::Cancelled => return Ok(OpOutcome::Cancelled),
            Step::Done => {
                acc.copied += 1;
                acc.done_sources.push(src.clone());
            }
            Step::Partial => acc.partial += 1,
            Step::Skipped | Step::SourceLeft => {}
        }
        ctl.on_item_done(src);
    }
    Ok(OpOutcome::Done)
}

pub fn move_items(sources: &[PathBuf], dest_dir: &Path, ctl: &mut dyn OpControl) -> OpReport {
    let r = Renamer {
        replace: &|a, b| fs::rename(a, b),
        noreplace: &rename_noreplace,
        remove: &remove_source,
    };
    move_items_impl(sources, dest_dir, ctl, &r)
}

fn remove_source(src: &Path) -> io::Result<()> {
    if is_real_dir(src) {
        fs::remove_dir_all(src)
    } else {
        fs::remove_file(src)
    }
}

type RenameFn<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

/// The two rename flavors a move needs: overwrite moves replace the destination
/// on purpose, every other move must never clobber anything.
struct Renamer<'a> {
    replace: RenameFn<'a>,
    noreplace: RenameFn<'a>,
    /// Deletes a source after a cross-device copy.
    remove: &'a dyn Fn(&Path) -> io::Result<()>,
}

/// `rename_fn` is injectable (for both flavors) so failures can be tested.
#[cfg(test)]
fn move_items_with(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    rename_fn: RenameFn,
) -> OpReport {
    let r = Renamer {
        replace: rename_fn,
        noreplace: rename_fn,
        remove: &remove_source,
    };
    move_items_impl(sources, dest_dir, ctl, &r)
}

/// Like [`move_items_with`] with an injectable source removal.
#[cfg(test)]
fn move_items_with_remove(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    rename_fn: RenameFn,
    remove_fn: &dyn Fn(&Path) -> io::Result<()>,
) -> OpReport {
    let r = Renamer {
        replace: rename_fn,
        noreplace: rename_fn,
        remove: remove_fn,
    };
    move_items_impl(sources, dest_dir, ctl, &r)
}

fn move_items_impl(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    r: &Renamer,
) -> OpReport {
    let mut acc = Acc::default();
    let outcome = match move_loop(sources, dest_dir, ctl, r, &mut acc) {
        Ok(o) => o,
        Err(e) => OpOutcome::Failed(e.to_string()),
    };
    acc.into_report(outcome)
}

fn move_loop(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    r: &Renamer,
    acc: &mut Acc,
) -> io::Result<OpOutcome> {
    let dests = check_all(sources, dest_dir)?;
    for (src, dest) in sources.iter().zip(&dests) {
        match move_entry(src, dest, ctl, r, acc)? {
            Step::Cancelled => return Ok(OpOutcome::Cancelled),
            Step::Done => {
                acc.copied += 1;
                acc.done_sources.push(src.clone());
            }
            Step::Partial | Step::SourceLeft => acc.partial += 1,
            Step::Skipped => {}
        }
        ctl.on_item_done(src);
    }
    Ok(OpOutcome::Done)
}

fn move_entry(
    src: &Path,
    dest: &Path,
    ctl: &mut dyn OpControl,
    r: &Renamer,
    acc: &mut Acc,
) -> io::Result<Step> {
    if ctl.is_cancelled() {
        return Ok(Step::Cancelled);
    }
    let mut replaced = false;
    let mut backup = None;
    if exists(dest) {
        match ctl.on_conflict(dest) {
            ConflictAction::Skip => {
                acc.skipped += 1;
                return Ok(Step::Skipped);
            }
            ConflictAction::Cancel => return Ok(Step::Cancelled),
            ConflictAction::Overwrite => {}
        }
        let src_dir = is_real_dir(src);
        let dest_dir = is_real_dir(dest);
        if src_dir && dest_dir {
            // Merge: cannot rename over a directory. Move children one by one.
            let mut children = Vec::new();
            for e in fs::read_dir(src)? {
                children.push(e?.file_name());
            }
            let mut complete = true;
            for name in children {
                let (cs, cd) = (src.join(&name), dest.join(&name));
                match move_entry(&cs, &cd, ctl, r, acc)? {
                    Step::Cancelled => return Ok(Step::Cancelled),
                    Step::Done => {}
                    Step::Skipped | Step::Partial | Step::SourceLeft => complete = false,
                }
            }
            // Leftovers (skipped entries) keep the source directory in place.
            if !complete {
                return Ok(Step::Partial);
            }
            return Ok(match fs::remove_dir(src) {
                Ok(()) => {
                    acc.removed_dirs.push(src.to_path_buf());
                    Step::Done
                }
                Err(e) => {
                    acc.warnings
                        .push(format!("{}: 빈 원본 폴더 삭제 실패: {e}", src.display()));
                    Step::Partial
                }
            });
        }
        if dest_dir {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination is a directory",
            ));
        }
        if src_dir {
            // rename(dir, file) is not allowed; the user confirmed replacing the
            // file. Park it and delete it only once the move succeeded.
            backup = Some(park_aside(dest)?);
        }
        // File onto file: rename replaces atomically, dest survives any failure.
        replaced = true;
    }
    // A parked destination is gone, so the plain no-clobber rename is the right one.
    let res = relocate(src, dest, ctl, r, acc, replaced && backup.is_none());
    let Some(b) = backup else {
        return res;
    };
    match res {
        Ok(Step::Done) => {
            let _ = fs::remove_file(&b);
            acc.overwritten += 1;
            Ok(Step::Done)
        }
        // The destination is complete (the replacement happened); only the source
        // cleanup failed. Drop the parked original, never the new data.
        Ok(Step::SourceLeft) => {
            let _ = fs::remove_file(&b);
            acc.overwritten += 1;
            Ok(Step::SourceLeft)
        }
        // Failure, cancel or an incomplete move: put the original file back.
        other => {
            restore_backup(&b, dest, acc)?;
            match other {
                Ok(Step::Partial) => Ok(Step::Skipped),
                o => o,
            }
        }
    }
}

/// Rename `src` to `dest`, falling back to copy + remove across devices.
fn relocate(
    src: &Path,
    dest: &Path,
    ctl: &mut dyn OpControl,
    r: &Renamer,
    acc: &mut Acc,
    replaced: bool,
) -> io::Result<Step> {
    let rename_fn = if replaced { r.replace } else { r.noreplace };
    match rename_fn(src, dest) {
        Ok(()) => {}
        Err(e) if is_cross_device(&e) => {
            let skipped_before = acc.skipped;
            match copy_entry(src, dest, ctl, false, replaced, acc)? {
                Step::Cancelled => return Ok(Step::Cancelled),
                Step::Skipped | Step::Partial | Step::SourceLeft => return Ok(Step::Skipped),
                Step::Done => {
                    if acc.skipped != skipped_before {
                        // Something was not copied: the source is not fully moved,
                        // keep it intact.
                        return Ok(Step::Partial);
                    }
                    // copy_entry already counted the replaced file.
                    if (r.remove)(src).is_err() {
                        // Source cleanup has started: the destination copy may be the
                        // only complete one, so it is kept and the leftovers stay.
                        // No undo pair is recorded: the source is no longer whole.
                        acc.warnings
                            .push(format!("원본 일부 정리 실패: {}", src.display()));
                        return Ok(Step::SourceLeft);
                    }
                    acc.moved.push((src.to_path_buf(), dest.to_path_buf()));
                    return Ok(Step::Done);
                }
            }
        }
        Err(e) => return Err(e),
    }
    if replaced {
        acc.overwritten += 1;
    }
    acc.moved.push((src.to_path_buf(), dest.to_path_buf()));
    Ok(Step::Done)
}

/// Move `src` to the exact path `dest`, refusing to overwrite anything.
/// Falls back to copy + remove across devices.
pub fn move_path_no_overwrite(src: &Path, dest: &Path) -> io::Result<()> {
    move_path_with(
        src,
        dest,
        &rename_noreplace,
        &rename_noreplace,
        &remove_source,
    )
}

struct Quiet;
impl OpControl for Quiet {
    fn on_progress(&mut self, _: u64, _: &Path) {}
    fn is_cancelled(&self) -> bool {
        false
    }
    fn on_conflict(&mut self, _: &Path) -> ConflictAction {
        ConflictAction::Cancel
    }
}

fn move_path_with(
    src: &Path,
    dest: &Path,
    rename_fn: RenameFn,
    finalize_fn: RenameFn,
    remove_fn: &dyn Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if exists(dest) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "target already exists",
        ));
    }
    match rename_fn(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            // Copy into a private temp sibling and finalize with a no-replace
            // rename, so a `dest` that appeared meanwhile is never touched.
            let tmp = temp_sibling(dest);
            let mut acc = Acc::default();
            let res = copy_entry(src, &tmp, &mut Quiet, false, false, &mut acc);
            match res {
                Ok(Step::Done) if acc.skipped == 0 => {
                    if let Err(e) = finalize_fn(&tmp, dest) {
                        // `tmp` is ours; `dest` (if any) belongs to someone else.
                        let _ = remove_source(&tmp);
                        return Err(e);
                    }
                    remove_fn(src).map_err(|_| {
                        // The copy at `dest` is complete; deleting it would risk the
                        // only full copy. Keep both.
                        io::Error::other(CleanupFailed(src.to_path_buf()))
                    })
                }
                other => {
                    let _ = remove_source(&tmp);
                    match other {
                        Err(e) => Err(e),
                        _ => Err(io::Error::other("could not copy every entry")),
                    }
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Whether a rename failure means "different filesystem" (EXDEV).
fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(EXDEV)
}

/// The data was restored/copied to the new place but the old place could not be
/// fully removed; both copies are kept.
#[derive(Debug)]
pub struct CleanupFailed(pub PathBuf);

impl std::fmt::Display for CleanupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "copied, but could not clean up {}", self.0.display())
    }
}

impl std::error::Error for CleanupFailed {}

/// Rename that never replaces an existing `to` (atomic where the OS supports it).
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rename_noreplace_with(from, to, &rename_noreplace_os, &|a, b| fs::hard_link(a, b))
}

/// [`rename_noreplace`] with injectable primitives: `os` is the atomic no-replace
/// rename, `link` the hard-link used when `os` is unsupported.
///
/// Without the atomic rename a check-then-rename would race, so regular files go
/// through link + unlink (link fails atomically if the target exists) and
/// anything else is refused.
fn rename_noreplace_with(from: &Path, to: &Path, os: RenameFn, link: RenameFn) -> io::Result<()> {
    match os(from, to) {
        Err(e) if is_unsupported(&e) => {
            if !fs::symlink_metadata(from)?.is_file() {
                return Err(unsafe_rename());
            }
            match link(from, to) {
                Ok(()) => {
                    if let Err(e) = fs::remove_file(from) {
                        let _ = fs::remove_file(to);
                        return Err(e);
                    }
                    Ok(())
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
                Err(e)
                    if is_cross_device(&e)
                        || is_unsupported(&e)
                        || matches!(e.raw_os_error(), Some(libc::EPERM)) =>
                {
                    Err(unsafe_rename())
                }
                Err(e) => Err(e),
            }
        }
        other => other,
    }
}

fn unsafe_rename() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "이 파일시스템은 안전한 이름 변경을 지원하지 않음",
    )
}

fn is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_noreplace_os(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    let f = CString::new(from.as_os_str().as_bytes()).map_err(|_| invalid("nul in path"))?;
    let t = CString::new(to.as_os_str().as_bytes()).map_err(|_| invalid("nul in path"))?;
    #[cfg(target_os = "macos")]
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_noreplace_os(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

pub fn make_dir(parent: &Path, name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    let p = parent.join(name);
    fs::create_dir(&p)?;
    Ok(p)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    validate_name(new_name)?;
    let target = path.with_file_name(new_name);
    if exists(&target) {
        // Case-only change on a case-insensitive filesystem: target IS the source.
        let old = path.file_name().map(|n| n.to_string_lossy().to_lowercase());
        let case_only = old.is_some_and(|o| o == new_name.to_lowercase());
        if case_only && same_file(path, &target) {
            let tmp = temp_sibling(path);
            fs::rename(path, &tmp)?;
            if let Err(e) = rename_noreplace(&tmp, &target) {
                let _ = fs::rename(&tmp, path);
                return Err(e);
            }
            return Ok(target);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "target already exists",
        ));
    }
    rename_noreplace(path, &target)?;
    Ok(target)
}

pub trait Trash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<()>;
}

pub struct SystemTrash;

impl Trash for SystemTrash {
    fn trash(&self, paths: &[PathBuf]) -> io::Result<()> {
        trash::delete_all(paths).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    struct Ctl {
        progress: u64,
        done: Vec<PathBuf>,
        conflicts: Vec<PathBuf>,
        action: ConflictAction,
        cancel_on_progress: bool,
        cancelled: bool,
    }
    impl Ctl {
        fn new(action: ConflictAction) -> Self {
            Ctl {
                progress: 0,
                done: vec![],
                conflicts: vec![],
                action,
                cancel_on_progress: false,
                cancelled: false,
            }
        }
    }
    impl OpControl for Ctl {
        fn on_progress(&mut self, d: u64, _c: &Path) {
            self.progress += d;
            if self.cancel_on_progress {
                self.cancelled = true;
            }
        }
        fn on_file_done(&mut self, p: &Path) {
            self.done.push(p.to_path_buf());
        }
        fn is_cancelled(&self) -> bool {
            self.cancelled
        }
        fn on_conflict(&mut self, d: &Path) -> ConflictAction {
            self.conflicts.push(d.to_path_buf());
            self.action
        }
    }

    fn failed(r: OpReport) -> String {
        match r.outcome {
            OpOutcome::Failed(m) => m,
            o => panic!("expected Failed, got {o:?}"),
        }
    }

    fn ctl() -> Ctl {
        Ctl::new(ConflictAction::Cancel)
    }

    #[test]
    fn copy_file() {
        let t = tempdir().unwrap();
        let (src, dst) = (t.path().join("a.txt"), t.path().join("dst"));
        fs::write(&src, "hello").unwrap();
        fs::create_dir(&dst).unwrap();
        let mut c = ctl();
        assert_eq!(
            copy_items(std::slice::from_ref(&src), &dst, &mut c).outcome,
            OpOutcome::Done
        );
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert!(src.exists());
        assert_eq!(c.progress, 5);
        assert_eq!(c.done, vec![dst.join("a.txt")]);
    }

    #[test]
    fn copy_nested_dir_preserves_content_perms_mtime() {
        let t = tempdir().unwrap();
        let src = t.path().join("src");
        let dst = t.path().join("dst");
        fs::create_dir_all(src.join("sub/deep")).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("sub/deep/f.bin"), [1u8, 2, 3]).unwrap();
        fs::write(src.join("top.sh"), "x").unwrap();
        fs::set_permissions(src.join("top.sh"), fs::Permissions::from_mode(0o750)).unwrap();
        let mt = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(src.join("top.sh"))
            .unwrap()
            .set_modified(mt)
            .unwrap();
        let mut c = ctl();
        copy_items(&[src], &dst, &mut c);
        assert_eq!(fs::read(dst.join("src/sub/deep/f.bin")).unwrap(), [1, 2, 3]);
        let m = fs::metadata(dst.join("src/top.sh")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o750);
        assert_eq!(m.modified().unwrap(), mt);
    }

    #[test]
    fn symlink_preserved() {
        let t = tempdir().unwrap();
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(t.path().join("target"), "t").unwrap();
        let link = t.path().join("lnk");
        std::os::unix::fs::symlink("target", &link).unwrap();
        copy_items(&[link], &dst, &mut ctl());
        let m = fs::symlink_metadata(dst.join("lnk")).unwrap();
        assert!(m.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dst.join("lnk")).unwrap(),
            PathBuf::from("target")
        );
    }

    fn conflict_setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempdir().unwrap();
        let src = t.path().join("f.txt");
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(&src, "new").unwrap();
        fs::write(dst.join("f.txt"), "old").unwrap();
        (t, src, dst)
    }

    #[test]
    fn conflict_overwrite() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        assert_eq!(copy_items(&[src], &dst, &mut c).outcome, OpOutcome::Done);
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "new");
        assert_eq!(c.conflicts, vec![dst.join("f.txt")]);
    }

    #[test]
    fn conflict_skip() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Skip);
        assert_eq!(copy_items(&[src], &dst, &mut c).outcome, OpOutcome::Done);
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn conflict_cancel() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Cancel);
        assert_eq!(
            copy_items(&[src], &dst, &mut c).outcome,
            OpOutcome::Cancelled
        );
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn overwrite_merges_dirs() {
        let t = tempdir().unwrap();
        let src = t.path().join("d");
        let dst = t.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst.join("d")).unwrap();
        fs::write(src.join("a"), "new-a").unwrap();
        fs::write(src.join("b"), "new-b").unwrap();
        fs::write(dst.join("d/a"), "old-a").unwrap();
        fs::write(dst.join("d/keep"), "keep").unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        copy_items(&[src], &dst, &mut c);
        assert_eq!(fs::read_to_string(dst.join("d/a")).unwrap(), "new-a");
        assert_eq!(fs::read_to_string(dst.join("d/b")).unwrap(), "new-b");
        assert_eq!(fs::read_to_string(dst.join("d/keep")).unwrap(), "keep");
        assert_eq!(c.conflicts.len(), 2); // dir itself + file a
    }

    #[test]
    fn copy_into_self_rejected() {
        let t = tempdir().unwrap();
        let src = t.path().join("d");
        fs::create_dir_all(src.join("sub")).unwrap();
        for dest in [src.clone(), src.join("sub")] {
            let e = failed(copy_items(std::slice::from_ref(&src), &dest, &mut ctl()));
            assert!(e.contains("same") || e.contains("itself"), "{e}");
            let e = failed(move_items(std::slice::from_ref(&src), &dest, &mut ctl()));
            assert!(e.contains("same") || e.contains("itself"), "{e}");
        }
    }

    #[test]
    fn same_path_rejected() {
        let t = tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, "x").unwrap();
        let e = failed(copy_items(std::slice::from_ref(&f), t.path(), &mut ctl()));
        assert!(e.contains("same") || e.contains("itself"), "{e}");
        let e = failed(move_items(std::slice::from_ref(&f), t.path(), &mut ctl()));
        assert!(e.contains("same") || e.contains("itself"), "{e}");
        assert_eq!(fs::read_to_string(&f).unwrap(), "x");
    }

    #[test]
    fn cancel_mid_copy_removes_partial() {
        let t = tempdir().unwrap();
        let src = t.path().join("big");
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(&src, vec![7u8; 3 * 1024 * 1024]).unwrap();
        let mut c = ctl();
        c.cancel_on_progress = true;
        assert_eq!(
            copy_items(&[src], &dst, &mut c).outcome,
            OpOutcome::Cancelled
        );
        assert!(!dst.join("big").exists());
        assert!(c.progress < 3 * 1024 * 1024);
    }

    #[test]
    fn progress_sum_equals_measure() {
        let t = tempdir().unwrap();
        let src = t.path().join("s");
        let dst = t.path().join("dst");
        fs::create_dir_all(src.join("n")).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("a"), vec![1u8; 2_500_000]).unwrap();
        fs::write(src.join("n/b"), "bb").unwrap();
        fs::write(src.join("empty"), "").unwrap();
        std::os::unix::fs::symlink("a", src.join("l")).unwrap();
        let tot = measure(std::slice::from_ref(&src)).unwrap();
        assert_eq!(
            tot,
            Totals {
                files: 4,
                bytes: 2_500_002
            }
        );
        let mut c = ctl();
        copy_items(&[src], &dst, &mut c);
        assert_eq!(c.progress, tot.bytes);
    }

    #[test]
    fn move_same_fs() {
        let t = tempdir().unwrap();
        let src = t.path().join("d");
        let dst = t.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        assert_eq!(
            move_items(std::slice::from_ref(&src), &dst, &mut ctl()).outcome,
            OpOutcome::Done
        );
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("d/f")).unwrap(), "x");
    }

    #[test]
    fn move_conflict_skip_keeps_source() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Skip);
        move_items(std::slice::from_ref(&src), &dst, &mut c);
        assert!(src.exists());
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn move_conflict_overwrite_replaces() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        move_items(std::slice::from_ref(&src), &dst, &mut c);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "new");
    }

    #[test]
    fn move_fallback_on_exdev() {
        let t = tempdir().unwrap();
        let src = t.path().join("d");
        let dst = t.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        let exdev = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(18));
        let mut c = ctl();
        let r = move_items_with(std::slice::from_ref(&src), &dst, &mut c, &exdev);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("d/f")).unwrap(), "x");
        assert_eq!(c.progress, 1);
    }

    #[test]
    fn move_fallback_cancelled_keeps_source() {
        let t = tempdir().unwrap();
        let src = t.path().join("big");
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(&src, vec![0u8; 3 * 1024 * 1024]).unwrap();
        let exdev = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::CrossesDevices));
        let mut c = ctl();
        c.cancel_on_progress = true;
        let r = move_items_with(std::slice::from_ref(&src), &dst, &mut c, &exdev);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert!(src.exists());
        assert!(!dst.join("big").exists());
    }

    #[test]
    fn move_other_rename_error_propagates() {
        let t = tempdir().unwrap();
        let src = t.path().join("f");
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        fs::write(&src, "x").unwrap();
        let denied = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let e = failed(move_items_with(
            std::slice::from_ref(&src),
            &dst,
            &mut ctl(),
            &denied,
        ));
        assert_eq!(e, "permission denied");
        assert!(src.exists());
    }

    #[test]
    fn cross_device_detection() {
        assert!(is_cross_device(&io::Error::from_raw_os_error(18)));
        assert!(is_cross_device(&io::Error::from(
            io::ErrorKind::CrossesDevices
        )));
        assert!(!is_cross_device(&io::Error::from(io::ErrorKind::NotFound)));
    }

    #[test]
    fn make_dir_ok_and_errors() {
        let t = tempdir().unwrap();
        let p = make_dir(t.path(), "새 폴더 1").unwrap();
        assert_eq!(p, t.path().join("새 폴더 1"));
        assert!(p.is_dir());
        assert_eq!(
            make_dir(t.path(), "새 폴더 1").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        for bad in ["", "a/b", ".", ".."] {
            assert_eq!(
                make_dir(t.path(), bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad}"
            );
        }
    }

    #[test]
    fn rename_ok_and_errors() {
        let t = tempdir().unwrap();
        let a = t.path().join("한글 파일.txt");
        fs::write(&a, "x").unwrap();
        fs::write(t.path().join("exists"), "y").unwrap();
        for bad in ["", "a/b", ".", ".."] {
            assert_eq!(
                rename(&a, bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad}"
            );
        }
        assert_eq!(
            rename(&a, "exists").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let n = rename(&a, "새 이름.txt").unwrap();
        assert_eq!(n, t.path().join("새 이름.txt"));
        assert!(!a.exists() && n.exists());
        assert_eq!(fs::read_to_string(t.path().join("exists")).unwrap(), "y");
    }

    #[test]
    fn korean_and_space_names_copy() {
        let t = tempdir().unwrap();
        let src = t.path().join("내 문서 'q'");
        let dst = t.path().join("대상 폴더");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("가 나.txt"), "한").unwrap();
        copy_items(&[src], &dst, &mut ctl());
        assert_eq!(
            fs::read_to_string(dst.join("내 문서 'q'/가 나.txt")).unwrap(),
            "한"
        );
    }

    #[test]
    fn trash_via_fake() {
        use std::cell::RefCell;
        struct Fake(RefCell<Vec<PathBuf>>);
        impl Trash for Fake {
            fn trash(&self, p: &[PathBuf]) -> io::Result<()> {
                self.0.borrow_mut().extend_from_slice(p);
                Ok(())
            }
        }
        let f = Fake(RefCell::new(vec![]));
        let t: &dyn Trash = &f;
        t.trash(&[PathBuf::from("/x/y")]).unwrap();
        assert_eq!(f.0.borrow().as_slice(), [PathBuf::from("/x/y")]);
    }

    // ---- HDIR-12 edge cases ----

    /// Restore permissions so tempdir cleanup can delete everything.
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    fn two_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempdir().unwrap();
        let (s, d) = (t.path().join("s"), t.path().join("d"));
        fs::create_dir(&s).unwrap();
        fs::create_dir(&d).unwrap();
        (t, s, d)
    }

    #[test]
    fn empty_dir_copy_and_move() {
        let (_t, s, d) = two_dirs();
        let e = s.join("empty");
        fs::create_dir(&e).unwrap();
        copy_items(std::slice::from_ref(&e), &d, &mut ctl());
        assert!(d.join("empty").is_dir());
        assert_eq!(fs::read_dir(d.join("empty")).unwrap().count(), 0);
        assert!(e.exists());
        let d2 = _t.path().join("d2");
        fs::create_dir(&d2).unwrap();
        move_items(std::slice::from_ref(&e), &d2, &mut ctl());
        assert!(d2.join("empty").is_dir());
        assert!(!e.exists());
        // EXDEV fallback path for an empty dir too.
        let exdev = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(18));
        let e2 = s.join("empty2");
        fs::create_dir(&e2).unwrap();
        move_items_with(std::slice::from_ref(&e2), &d2, &mut ctl(), &exdev);
        assert!(d2.join("empty2").is_dir() && !e2.exists());
    }

    #[test]
    fn readonly_source_keeps_perms_and_can_be_overwritten_twice() {
        let (_t, s, d) = two_dirs();
        let src = s.join("ro.txt");
        fs::write(&src, "v1").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o444)).unwrap();
        copy_items(std::slice::from_ref(&src), &d, &mut ctl());
        let dest = d.join("ro.txt");
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v1");
        // Second run: dest is read-only and must still be replaced.
        let mut c = Ctl::new(ConflictAction::Overwrite);
        assert_eq!(
            copy_items(std::slice::from_ref(&src), &d, &mut c).outcome,
            OpOutcome::Done
        );
        assert_eq!(c.conflicts.len(), 1);
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o444
        );
        // Move overwrite onto a read-only dest as well.
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&src, "v2").unwrap();
        move_items(
            std::slice::from_ref(&src),
            &d,
            &mut Ctl::new(ConflictAction::Overwrite),
        );
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v2");
    }

    #[test]
    fn readonly_dest_dir_errors_without_panic_or_partial() {
        let (_t, s, d) = two_dirs();
        let src = s.join("f.txt");
        fs::write(&src, "x").unwrap();
        let tree = s.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/g"), "g").unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o555)).unwrap();
        let _g = Unlock(d.clone());
        if fs::File::create(d.join("probe")).is_ok() {
            return; // running as root: permissions not enforced
        }
        let e = failed(copy_items(std::slice::from_ref(&src), &d, &mut ctl()));
        assert!(e.contains("Permission denied"), "{e}");
        let e = failed(copy_items(std::slice::from_ref(&tree), &d, &mut ctl()));
        assert!(e.contains("Permission denied"), "{e}");
        let e = failed(move_items(std::slice::from_ref(&src), &d, &mut ctl()));
        assert!(e.contains("Permission denied"), "{e}");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
        assert!(src.exists() && tree.exists());
    }

    #[test]
    fn symlink_to_parent_is_copied_as_link_not_followed() {
        let (_t, s, d) = two_dirs();
        let tree = s.join("tree");
        fs::create_dir_all(tree.join("a")).unwrap();
        fs::write(tree.join("a/f"), "f").unwrap();
        std::os::unix::fs::symlink("..", tree.join("a/up")).unwrap(); // -> tree
        std::os::unix::fs::symlink(&s, tree.join("abs_parent")).unwrap(); // -> s (contains tree)
        std::os::unix::fs::symlink("loop2", tree.join("loop1")).unwrap();
        std::os::unix::fs::symlink("loop1", tree.join("loop2")).unwrap();
        let tot = measure(std::slice::from_ref(&tree)).unwrap();
        assert_eq!(tot, Totals { files: 5, bytes: 1 });
        let mut c = ctl();
        copy_items(std::slice::from_ref(&tree), &d, &mut c);
        assert_eq!(c.progress, tot.bytes);
        for (l, t) in [("a/up", ".."), ("loop1", "loop2"), ("loop2", "loop1")] {
            let p = d.join("tree").join(l);
            assert!(fs::symlink_metadata(&p).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_link(&p).unwrap(), PathBuf::from(t));
        }
        assert_eq!(fs::read_link(d.join("tree/abs_parent")).unwrap(), s);
        // Moving across devices also must not follow links.
        let exdev = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(18));
        let d2 = _t.path().join("d2");
        fs::create_dir(&d2).unwrap();
        move_items_with(std::slice::from_ref(&tree), &d2, &mut ctl(), &exdev);
        assert!(!tree.exists());
        assert!(
            fs::symlink_metadata(d2.join("tree/a/up"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn symlink_to_dir_as_source_is_copied_as_link() {
        let (_t, s, d) = two_dirs();
        let real = s.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("f"), "f").unwrap();
        let lnk = s.join("lnk");
        std::os::unix::fs::symlink(&real, &lnk).unwrap();
        copy_items(std::slice::from_ref(&lnk), &d, &mut ctl());
        assert!(fs::symlink_metadata(d.join("lnk")).unwrap().is_symlink());
    }

    #[test]
    fn awkward_file_names() {
        let (_t, s, d) = two_dirs();
        let names = [
            "new\nline",
            "qu\"ote",
            "-dash",
            "--rf",
            "-",
            "tab\tname",
            "it's $HOME `x`",
        ];
        let mut srcs = vec![];
        for n in names {
            let p = s.join(n);
            fs::write(&p, n).unwrap();
            srcs.push(p);
        }
        let dir = s.join("-dir\n\"q\"");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("-in\nner"), "i").unwrap();
        srcs.push(dir);
        let tot = measure(&srcs).unwrap();
        let mut c = ctl();
        copy_items(&srcs, &d, &mut c);
        assert_eq!(c.progress, tot.bytes);
        for n in names {
            assert_eq!(fs::read_to_string(d.join(n)).unwrap(), n);
        }
        assert_eq!(
            fs::read_to_string(d.join("-dir\n\"q\"/-in\nner")).unwrap(),
            "i"
        );
        let d2 = _t.path().join("d2");
        fs::create_dir(&d2).unwrap();
        move_items(&srcs, &d2, &mut ctl());
        assert_eq!(
            fs::read_to_string(d2.join("new\nline")).unwrap(),
            "new\nline"
        );
        assert!(!s.join("-dash").exists());
        // make_dir / rename accept these as plain names.
        let m = make_dir(&d2, "-x\ny").unwrap();
        assert!(m.is_dir());
        let r = rename(&m, "--z\"").unwrap();
        assert!(r.is_dir());
    }

    #[test]
    fn measure_matches_bytes_copied_with_symlinks() {
        let (_t, s, d) = two_dirs();
        let tree = s.join("t");
        fs::create_dir_all(tree.join("x/y")).unwrap();
        fs::write(tree.join("big"), vec![3u8; 1_500_000]).unwrap();
        fs::write(tree.join("x/y/small"), "abc").unwrap();
        fs::write(tree.join("x/empty"), "").unwrap();
        std::os::unix::fs::symlink("big", tree.join("l1")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/dangling", tree.join("x/dangling")).unwrap();
        std::os::unix::fs::symlink("x", tree.join("dirlink")).unwrap();
        let tot = measure(std::slice::from_ref(&tree)).unwrap();
        assert_eq!(
            tot,
            Totals {
                files: 6,
                bytes: 1_500_003
            }
        );
        let mut c = ctl();
        copy_items(&[tree], &d, &mut c);
        assert_eq!(c.progress, tot.bytes);
        assert_eq!(c.done.len() as u64, tot.files);
    }

    #[test]
    fn deep_nesting_50_levels() {
        let (_t, s, d) = two_dirs();
        let mut deep = s.join("root");
        for i in 0..50 {
            deep = deep.join(format!("l{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("leaf"), "leaf").unwrap();
        let root = s.join("root");
        let tot = measure(std::slice::from_ref(&root)).unwrap();
        assert_eq!(tot, Totals { files: 1, bytes: 4 });
        let mut c = ctl();
        copy_items(std::slice::from_ref(&root), &d, &mut c);
        let mut copied = d.join("root");
        for i in 0..50 {
            copied = copied.join(format!("l{i}"));
        }
        assert_eq!(fs::read_to_string(copied.join("leaf")).unwrap(), "leaf");
        let d2 = _t.path().join("d2");
        fs::create_dir(&d2).unwrap();
        let exdev = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(18));
        move_items_with(std::slice::from_ref(&root), &d2, &mut ctl(), &exdev);
        assert!(!root.exists());
        assert!(d2.join("root/l0").is_dir());
    }

    #[test]
    fn readonly_source_dir_copies_contents() {
        let (_t, s, d) = two_dirs();
        let src = s.join("rod");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("f"), "x").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o555)).unwrap();
        let _g = Unlock(src.clone());
        copy_items(std::slice::from_ref(&src), &d, &mut ctl());
        assert_eq!(fs::read_to_string(d.join("rod/f")).unwrap(), "x");
        assert_eq!(
            fs::metadata(d.join("rod")).unwrap().permissions().mode() & 0o777,
            0o555
        );
        let _g2 = Unlock(d.join("rod"));
    }

    // ---- review fixes (phase A: old API) ----

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn overwrite_then_cancel_keeps_original_and_no_temp() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("big"), vec![7u8; 3 * 1024 * 1024]).unwrap();
        fs::write(d.join("big"), "ORIGINAL").unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        c.cancel_on_progress = true;
        assert_eq!(
            copy_items(&[s.join("big")], &d, &mut c).outcome,
            OpOutcome::Cancelled
        );
        assert_eq!(fs::read_to_string(d.join("big")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["big"]);
    }

    #[test]
    fn overwrite_with_unreadable_source_keeps_original() {
        let (_t, s, d) = two_dirs();
        let src = s.join("f");
        fs::write(&src, "new").unwrap();
        fs::write(d.join("f"), "ORIGINAL").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&src).is_ok() {
            return; // root
        }
        let mut c = Ctl::new(ConflictAction::Overwrite);
        assert!(copy_items(std::slice::from_ref(&src), &d, &mut c).outcome != OpOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["f"]);
    }

    #[test]
    fn plain_copy_cancel_leaves_nothing_under_any_name() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("big"), vec![1u8; 3 * 1024 * 1024]).unwrap();
        let mut c = ctl();
        c.cancel_on_progress = true;
        copy_items(&[s.join("big")], &d, &mut c);
        assert!(names(&d).is_empty());
    }

    #[test]
    fn move_overwrite_rename_failure_keeps_dest() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("f"), "new").unwrap();
        fs::write(d.join("f"), "ORIGINAL").unwrap();
        let denied = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let _ = move_items_with(&[s.join("f")], &d, &mut c, &denied);
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "ORIGINAL");
        assert_eq!(fs::read_to_string(s.join("f")).unwrap(), "new");
    }

    #[test]
    fn fifo_in_tree_is_skipped_not_opened() {
        let (_t, s, d) = two_dirs();
        let tree = s.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("reg"), "r").unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(tree.join("pipe"))
            .status();
        assert!(matches!(st, Ok(st) if st.success()), "mkfifo failed");
        let (tx, rx) = std::sync::mpsc::channel();
        let (t2, d2) = (tree.clone(), d.clone());
        std::thread::spawn(move || {
            let r = copy_items(&[t2], &d2, &mut ctl());
            let _ = tx.send(r.outcome == OpOutcome::Done);
        });
        let ok = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(ok, Ok(true), "copy hung on or failed with FIFO");
        assert_eq!(fs::read_to_string(d.join("tree/reg")).unwrap(), "r");
        assert!(!d.join("tree/pipe").exists());
    }

    #[test]
    fn rename_case_only_change() {
        let t = tempdir().unwrap();
        let a = t.path().join("foo");
        fs::write(&a, "x").unwrap();
        let n = rename(&a, "Foo").unwrap();
        assert_eq!(names(t.path()), vec!["Foo"]);
        assert_eq!(fs::read_to_string(n).unwrap(), "x");
    }

    // ---- review fixes (phase B: OpReport API) ----

    #[test]
    fn report_move_records_pairs_and_overwrites() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        fs::write(s.join("b"), "B").unwrap();
        fs::write(d.join("b"), "OLD").unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items(&[s.join("a"), s.join("b")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(
            r.moved,
            vec![(s.join("a"), d.join("a")), (s.join("b"), d.join("b"))]
        );
        assert_eq!((r.overwritten, r.skipped, r.copied), (1, 0, 2));
    }

    #[test]
    fn report_merge_records_children_not_parent() {
        let (_t, s, d) = two_dirs();
        fs::create_dir_all(s.join("x/sub")).unwrap();
        fs::create_dir_all(d.join("x")).unwrap();
        fs::write(s.join("x/a"), "A").unwrap();
        fs::write(s.join("x/sub/b"), "B").unwrap();
        fs::write(d.join("x/keep"), "K").unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        let mut m = r.moved.clone();
        m.sort();
        assert_eq!(
            m,
            vec![
                (s.join("x/a"), d.join("x/a")),
                (s.join("x/sub"), d.join("x/sub"))
            ]
        );
        assert!(!s.join("x").exists());
        assert_eq!(fs::read_to_string(d.join("x/keep")).unwrap(), "K");
    }

    #[test]
    fn report_failed_keeps_partial_progress_and_validation_is_failed() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        fs::write(s.join("b"), "B").unwrap();
        let flaky = |a: &Path, b: &Path| {
            if a.ends_with("b") {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                fs::rename(a, b)
            }
        };
        let r = move_items_with(&[s.join("a"), s.join("b")], &d, &mut ctl(), &flaky);
        assert!(matches!(r.outcome, OpOutcome::Failed(_)));
        assert_eq!(r.moved, vec![(s.join("a"), d.join("a"))]);
        assert_eq!(r.copied, 1);
        let r = copy_items(&[s.join("missing")], &d, &mut ctl());
        assert!(matches!(r.outcome, OpOutcome::Failed(_)));
        assert!(r.moved.is_empty());
    }

    #[test]
    fn report_counts_skips_and_special_files() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        fs::write(d.join("a"), "OLD").unwrap();
        let r = copy_items(&[s.join("a")], &d, &mut Ctl::new(ConflictAction::Skip));
        assert_eq!((r.skipped, r.copied, r.overwritten), (1, 0, 0));
        let r = copy_items(&[s.join("a")], &d, &mut Ctl::new(ConflictAction::Overwrite));
        assert_eq!((r.skipped, r.copied, r.overwritten), (0, 1, 1));
    }

    #[test]
    fn measure_cancellable_stops_early() {
        let (_t, s, _d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        assert_eq!(
            measure_cancellable(std::slice::from_ref(&s), &|| true).unwrap(),
            None
        );
        assert_eq!(
            measure_cancellable(&[s], &|| false).unwrap(),
            Some(Totals { files: 1, bytes: 1 })
        );
    }

    #[test]
    fn move_path_exdev_falls_back_and_never_overwrites() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("dir")).unwrap();
        fs::write(s.join("dir/f"), "x").unwrap();
        let exdev = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(18));
        move_path_with(
            &s.join("dir"),
            &d.join("dir"),
            &exdev,
            &rename_noreplace,
            &remove_source,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(d.join("dir/f")).unwrap(), "x");
        assert!(!s.join("dir").exists());
        fs::write(s.join("f"), "n").unwrap();
        fs::write(d.join("f"), "KEEP").unwrap();
        assert!(
            move_path_with(
                &s.join("f"),
                &d.join("f"),
                &exdev,
                &rename_noreplace,
                &remove_source
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "KEEP");
        assert!(s.join("f").exists());
    }

    // ---- round 2 ----

    #[test]
    fn rename_noreplace_file_ok_and_never_clobbers() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        rename_noreplace(&s.join("a"), &d.join("a")).unwrap();
        assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "A");
        assert!(!s.join("a").exists());
        fs::write(s.join("a"), "NEW").unwrap();
        let e = rename_noreplace(&s.join("a"), &d.join("a")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(s.join("a")).unwrap(), "NEW");
    }

    #[test]
    fn rename_noreplace_dirs() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::write(s.join("x/f"), "1").unwrap();
        rename_noreplace(&s.join("x"), &d.join("x")).unwrap();
        assert_eq!(fs::read_to_string(d.join("x/f")).unwrap(), "1");
        // Even an empty target dir is not replaced.
        fs::create_dir(s.join("x")).unwrap();
        let e = rename_noreplace(&s.join("x"), &d.join("x")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(s.join("x").exists() && d.join("x/f").exists());
    }

    fn denied(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    fn exdev_err(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(EXDEV))
    }

    fn dir_over_file_setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::write(s.join("x/f"), "inner").unwrap();
        fs::write(d.join("x"), "ORIGINAL").unwrap();
        (t, s, d)
    }

    #[test]
    fn move_dir_over_file_rename_failure_keeps_file() {
        let (_t, s, d) = dir_over_file_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items_with(&[s.join("x")], &d, &mut c, &denied);
        assert!(matches!(r.outcome, OpOutcome::Failed(_)));
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
        assert!(s.join("x/f").exists());
    }

    #[test]
    fn move_dir_over_file_exdev_cancel_keeps_file() {
        let (_t, s, d) = dir_over_file_setup();
        fs::write(s.join("x/big"), vec![1u8; 3 * 1024 * 1024]).unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        c.cancel_on_progress = true;
        let r = move_items_with(&[s.join("x")], &d, &mut c, &exdev_err);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
        assert!(s.join("x/f").exists() && s.join("x/big").exists());
    }

    #[test]
    fn move_dir_over_file_success_leaves_no_backup() {
        let (_t, s, d) = dir_over_file_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("x/f")).unwrap(), "inner");
        assert_eq!(names(&d), vec!["x"]);
        assert_eq!(r.overwritten, 1);
    }

    #[test]
    fn copy_dir_over_file_cancel_keeps_file() {
        let (_t, s, d) = dir_over_file_setup();
        fs::write(s.join("x/big"), vec![1u8; 3 * 1024 * 1024]).unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        c.cancel_on_progress = true;
        let r = copy_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
    }

    #[test]
    fn copy_dir_over_file_failure_keeps_file() {
        let (_t, s, d) = dir_over_file_setup();
        let bad = s.join("x/bad");
        fs::write(&bad, "secret").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&bad).is_ok() {
            return; // root
        }
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = copy_items(&[s.join("x")], &d, &mut c);
        assert!(matches!(r.outcome, OpOutcome::Failed(_)));
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
    }

    #[test]
    fn copy_dir_over_file_success_leaves_no_backup() {
        let (_t, s, d) = dir_over_file_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = copy_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("x/f")).unwrap(), "inner");
        assert_eq!(names(&d), vec!["x"]);
        assert_eq!(r.overwritten, 1);
    }

    #[test]
    fn move_path_cleanup_failure_keeps_both_copies() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("dir")).unwrap();
        fs::write(s.join("dir/a"), "A").unwrap();
        fs::write(s.join("dir/b"), "B").unwrap();
        // Removes one file, then fails: a partial cleanup.
        let partial_remove = |p: &Path| {
            fs::remove_file(p.join("a"))?;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        };
        let e = move_path_with(
            &s.join("dir"),
            &d.join("dir"),
            &exdev_err,
            &rename_noreplace,
            &partial_remove,
        )
        .unwrap_err();
        let cf = e
            .get_ref()
            .and_then(|x| x.downcast_ref::<CleanupFailed>())
            .expect("CleanupFailed error");
        assert_eq!(cf.0, s.join("dir"));
        assert_eq!(fs::read_to_string(d.join("dir/a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(d.join("dir/b")).unwrap(), "B");
        assert!(s.join("dir/b").exists());
    }

    #[test]
    fn move_path_exdev_intruder_at_dest_is_never_deleted() {
        for dir in [false, true] {
            let (_t, s, d) = two_dirs();
            if dir {
                fs::create_dir(s.join("x")).unwrap();
                fs::write(s.join("x/f"), "src").unwrap();
            } else {
                fs::write(s.join("x"), "src").unwrap();
            }
            let dest = d.join("x");
            // The destination appears after the copy, before the final rename.
            let intrude = |from: &Path, to: &Path| {
                if dir {
                    fs::create_dir(to).unwrap();
                    fs::write(to.join("keep"), "INTRUDER").unwrap();
                } else {
                    fs::write(to, "INTRUDER").unwrap();
                }
                rename_noreplace(from, to)
            };
            let e = move_path_with(&s.join("x"), &dest, &exdev_err, &intrude, &remove_source)
                .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "dir={dir}");
            if dir {
                assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "INTRUDER");
                assert_eq!(fs::read_to_string(s.join("x/f")).unwrap(), "src");
            } else {
                assert_eq!(fs::read_to_string(&dest).unwrap(), "INTRUDER");
                assert_eq!(fs::read_to_string(s.join("x")).unwrap(), "src");
            }
            assert_eq!(names(&d), vec!["x"], "no temp leftovers");
        }
    }

    struct ModeProbe {
        dir: PathBuf,
        seen: Vec<u32>,
    }
    impl OpControl for ModeProbe {
        fn on_progress(&mut self, _: u64, _: &Path) {
            for e in fs::read_dir(&self.dir).unwrap() {
                let e = e.unwrap();
                if e.file_name().to_string_lossy().starts_with(".hdir-tmp-") {
                    self.seen
                        .push(e.metadata().unwrap().permissions().mode() & 0o777);
                }
            }
        }
        fn is_cancelled(&self) -> bool {
            false
        }
        fn on_conflict(&mut self, _: &Path) -> ConflictAction {
            ConflictAction::Cancel
        }
    }

    #[test]
    fn temp_file_is_private_until_complete() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("f"), "data").unwrap();
        fs::set_permissions(s.join("f"), fs::Permissions::from_mode(0o644)).unwrap();
        let mut p = ModeProbe {
            dir: d.clone(),
            seen: vec![],
        };
        assert_eq!(
            copy_items(&[s.join("f")], &d, &mut p).outcome,
            OpOutcome::Done
        );
        assert!(!p.seen.is_empty());
        assert!(p.seen.iter().all(|m| m & 0o077 == 0), "{:?}", p.seen);
        let m = fs::metadata(d.join("f")).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o644);
    }

    #[test]
    fn merge_move_of_empty_dir_records_removed_dir() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::create_dir(d.join("x")).unwrap();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert!(!s.join("x").exists());
        assert_eq!(r.removed_dirs, vec![s.join("x")]);
    }

    #[test]
    fn exdev_move_with_skipped_special_file_is_partial() {
        let (_t, s, d) = two_dirs();
        let tree = s.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("reg"), "r").unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(tree.join("pipe"))
            .status();
        assert!(matches!(st, Ok(st) if st.success()), "mkfifo failed");
        let r = move_items_with(std::slice::from_ref(&tree), &d, &mut ctl(), &exdev_err);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.partial), (0, 1));
        assert!(r.moved.is_empty());
        assert!(tree.join("reg").exists());
        assert!(tree.join("pipe").exists());
    }

    // ---- round-5 review fixes ----

    #[test]
    fn park_aside_never_overwrites_existing_backup_name() {
        let (_t, _s, d) = two_dirs();
        let dest = d.join("x");
        fs::write(&dest, "ORIGINAL").unwrap();
        let taken = d.join("taken");
        fs::write(&taken, "OTHER").unwrap();
        let fresh = d.join("fresh");
        let mut names_iter = vec![taken.clone(), fresh.clone()].into_iter();
        let b = park_aside_with(&dest, &mut || names_iter.next().unwrap()).unwrap();
        assert_eq!(b, fresh);
        assert_eq!(fs::read_to_string(&taken).unwrap(), "OTHER");
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "ORIGINAL");
        assert!(!dest.exists());
    }

    #[test]
    fn park_aside_gives_up_after_bounded_attempts() {
        let (_t, _s, d) = two_dirs();
        let dest = d.join("x");
        fs::write(&dest, "ORIGINAL").unwrap();
        let taken = d.join("taken");
        fs::write(&taken, "OTHER").unwrap();
        let mut calls = 0;
        let e = park_aside_with(&dest, &mut || {
            calls += 1;
            taken.clone()
        })
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(calls <= 32, "unbounded: {calls}");
        assert_eq!(fs::read_to_string(&dest).unwrap(), "ORIGINAL");
        assert_eq!(fs::read_to_string(&taken).unwrap(), "OTHER");
    }

    #[test]
    fn copy_dir_over_file_with_fifo_child_restores_backup_as_partial() {
        let (_t, s, d) = dir_over_file_setup();
        let st = std::process::Command::new("mkfifo")
            .arg(s.join("x/pipe"))
            .status();
        assert!(matches!(st, Ok(st) if st.success()), "mkfifo failed");
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = copy_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
        assert_eq!((r.copied, r.partial, r.overwritten), (0, 1, 0));
        assert!(!r.warnings.is_empty());
    }

    /// First conflict (the top-level one) overwrites, later ones skip; creates
    /// `intruder` on the first progress callback.
    struct SkipChild {
        intruder: Option<PathBuf>,
        asked: usize,
    }
    impl OpControl for SkipChild {
        fn on_progress(&mut self, _: u64, _: &Path) {
            if let Some(p) = self.intruder.take() {
                fs::write(p, "INTRUDER").unwrap();
            }
        }
        fn is_cancelled(&self) -> bool {
            false
        }
        fn on_conflict(&mut self, _: &Path) -> ConflictAction {
            self.asked += 1;
            if self.asked == 1 {
                ConflictAction::Overwrite
            } else {
                ConflictAction::Skip
            }
        }
    }

    #[test]
    fn copy_dir_over_file_with_skipped_child_restores_backup_as_partial() {
        let (_t, s, d) = dir_over_file_setup();
        let mut c = SkipChild {
            intruder: Some(d.join("x/f")),
            asked: 0,
        };
        let r = copy_items(&[s.join("x")], &d, &mut c);
        assert_eq!(c.asked, 2);
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "ORIGINAL");
        assert_eq!(names(&d), vec!["x"]);
        assert_eq!((r.copied, r.partial, r.overwritten), (0, 1, 0));
        assert!(!r.warnings.is_empty());
    }

    // ---- round-3 review fixes ----

    /// Runs `f` on the first progress callback (simulates a concurrent change).
    struct Hook {
        f: Option<Box<dyn FnOnce()>>,
        action: ConflictAction,
        conflicts: Vec<PathBuf>,
    }
    impl Hook {
        fn new(action: ConflictAction, f: impl FnOnce() + 'static) -> Self {
            Hook {
                f: Some(Box::new(f)),
                action,
                conflicts: vec![],
            }
        }
    }
    impl OpControl for Hook {
        fn on_progress(&mut self, _: u64, _: &Path) {
            if let Some(f) = self.f.take() {
                f();
            }
        }
        fn is_cancelled(&self) -> bool {
            false
        }
        fn on_conflict(&mut self, d: &Path) -> ConflictAction {
            self.conflicts.push(d.to_path_buf());
            self.action
        }
    }

    #[test]
    fn dest_appearing_mid_copy_asks_and_skip_keeps_intruder() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("f"), "NEW").unwrap();
        let dest = d.join("f");
        let dc = dest.clone();
        let mut h = Hook::new(ConflictAction::Skip, move || {
            fs::write(dc, "INTRUDER").unwrap()
        });
        let r = copy_items(&[s.join("f")], &d, &mut h);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(h.conflicts, vec![dest.clone()]);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "INTRUDER");
        assert_eq!((r.skipped, r.copied, r.overwritten), (1, 0, 0));
        assert_eq!(names(&d), vec!["f"]);
    }

    #[test]
    fn dest_appearing_mid_copy_overwrite_and_cancel() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("f"), "NEW").unwrap();
        let dc = d.join("f");
        let mut h = Hook::new(ConflictAction::Overwrite, move || {
            fs::write(dc, "X").unwrap()
        });
        let r = copy_items(&[s.join("f")], &d, &mut h);
        assert_eq!((r.copied, r.overwritten), (1, 1));
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "NEW");
        assert_eq!(names(&d), vec!["f"]);

        fs::remove_file(d.join("f")).unwrap();
        let dc = d.join("f");
        let mut h = Hook::new(ConflictAction::Cancel, move || fs::write(dc, "X").unwrap());
        let r = copy_items(&[s.join("f")], &d, &mut h);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "X");
        assert_eq!(names(&d), vec!["f"]);
    }

    #[test]
    fn dest_appearing_mid_exdev_move_skip_keeps_both() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("f"), "NEW").unwrap();
        let dc = d.join("f");
        let mut h = Hook::new(ConflictAction::Skip, move || {
            fs::write(dc, "INTRUDER").unwrap()
        });
        let r = move_items_with(&[s.join("f")], &d, &mut h, &exdev_err);
        assert_eq!(h.conflicts, vec![d.join("f")]);
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "INTRUDER");
        assert_eq!(fs::read_to_string(s.join("f")).unwrap(), "NEW");
        assert!(r.moved.is_empty());
        assert_eq!(names(&d), vec!["f"]);
    }

    struct DirProbe {
        dir: PathBuf,
        seen: Vec<u32>,
    }
    impl OpControl for DirProbe {
        fn on_progress(&mut self, _: u64, _: &Path) {
            if let Ok(m) = fs::metadata(&self.dir) {
                self.seen.push(m.permissions().mode() & 0o777);
            }
        }
        fn is_cancelled(&self) -> bool {
            false
        }
        fn on_conflict(&mut self, _: &Path) -> ConflictAction {
            ConflictAction::Cancel
        }
    }

    #[test]
    fn copied_dir_is_private_until_complete() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::write(s.join("x/f"), "data").unwrap();
        fs::set_permissions(s.join("x"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut p = DirProbe {
            dir: d.join("x"),
            seen: vec![],
        };
        assert_eq!(
            copy_items(&[s.join("x")], &d, &mut p).outcome,
            OpOutcome::Done
        );
        assert!(!p.seen.is_empty());
        assert!(p.seen.iter().all(|m| m & 0o077 == 0), "{:?}", p.seen);
        let m = fs::metadata(d.join("x")).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o755);
    }

    #[test]
    fn merge_with_skipped_child_is_partial_and_keeps_source() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::create_dir(d.join("x")).unwrap();
        fs::write(s.join("x/a"), "A").unwrap();
        fs::write(d.join("x/a"), "OLD").unwrap();
        // The directory conflict is answered Overwrite, the file conflict Skip.
        struct Seq(Vec<ConflictAction>);
        impl OpControl for Seq {
            fn on_progress(&mut self, _: u64, _: &Path) {}
            fn is_cancelled(&self) -> bool {
                false
            }
            fn on_conflict(&mut self, _: &Path) -> ConflictAction {
                self.0.remove(0)
            }
        }
        let mut c = Seq(vec![ConflictAction::Overwrite, ConflictAction::Skip]);
        let r = move_items(&[s.join("x")], &d, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.partial, r.skipped), (0, 1, 1));
        assert!(r.removed_dirs.is_empty());
        assert_eq!(fs::read_to_string(s.join("x/a")).unwrap(), "A");
        assert_eq!(fs::read_to_string(d.join("x/a")).unwrap(), "OLD");
    }

    #[test]
    fn merge_with_unremovable_source_dir_is_partial_with_warning() {
        let (_t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::create_dir(d.join("x")).unwrap();
        fs::write(s.join("x/a"), "A").unwrap();
        fs::set_permissions(&s, fs::Permissions::from_mode(0o555)).unwrap();
        let _g = Unlock(s.clone());
        if fs::create_dir(s.join("probe")).is_ok() {
            return; // root
        }
        let r = move_items(&[s.join("x")], &d, &mut Ctl::new(ConflictAction::Overwrite));
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.partial), (0, 1));
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.removed_dirs.is_empty());
        assert!(s.join("x").is_dir());
        assert_eq!(fs::read_to_string(d.join("x/a")).unwrap(), "A");
    }

    // ---- round-4 review fixes ----

    /// Removes the first entry it finds inside `p`, then fails (partial cleanup).
    fn partial_remove(p: &Path) -> io::Result<()> {
        if let Some(e) = fs::read_dir(p)?.next() {
            remove_source(&e?.path())?;
        }
        Err(io::Error::other("injected"))
    }

    fn big_tree(s: &Path) {
        fs::create_dir(s.join("x")).unwrap();
        for n in ["a", "b", "c"] {
            fs::write(s.join("x").join(n), n).unwrap();
        }
    }

    fn assert_each_file_somewhere(s: &Path, d: &Path) {
        for n in ["a", "b", "c"] {
            let got = fs::read_to_string(d.join("x").join(n)).unwrap();
            assert_eq!(got, n);
        }
        assert!(!names(d).iter().any(|n| n.starts_with(".hdir")));
        let _ = s;
    }

    #[test]
    fn exdev_dir_move_partial_source_cleanup_keeps_complete_dest() {
        let (_t, s, d) = two_dirs();
        big_tree(&s);
        let r = move_items_with_remove(&[s.join("x")], &d, &mut ctl(), &exdev_err, &partial_remove);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_each_file_somewhere(&s, &d);
        assert_eq!((r.copied, r.partial), (0, 1));
        assert!(r.warnings.iter().any(
            |w| w.contains("원본 일부 정리 실패") && w.contains(s.join("x").to_str().unwrap())
        ));
        // Source keeps whatever was not removed; no undo pair (source not restorable).
        assert!(r.moved.is_empty());
        assert_eq!(fs::read_dir(s.join("x")).unwrap().count(), 2);
    }

    #[test]
    fn exdev_dir_over_file_partial_cleanup_keeps_dest_and_drops_backup() {
        let (_t, s, d) = dir_over_file_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        let r = move_items_with_remove(&[s.join("x")], &d, &mut c, &exdev_err, &partial_remove);
        assert_eq!(fs::read_to_string(d.join("x/f")).unwrap(), "inner");
        assert_eq!(names(&d), vec!["x"]);
        assert_eq!((r.copied, r.partial, r.overwritten), (0, 1, 1));
    }

    /// Runs `f` on the n-th `is_cancelled` poll (1-based).
    struct PollHook {
        n: usize,
        polls: std::cell::Cell<usize>,
        f: std::cell::RefCell<Option<Box<dyn FnOnce()>>>,
        action: ConflictAction,
        conflicts: Vec<PathBuf>,
    }
    impl PollHook {
        fn new(n: usize, action: ConflictAction, f: impl FnOnce() + 'static) -> Self {
            PollHook {
                n,
                polls: 0.into(),
                f: std::cell::RefCell::new(Some(Box::new(f))),
                action,
                conflicts: vec![],
            }
        }
    }
    impl OpControl for PollHook {
        fn on_progress(&mut self, _: u64, _: &Path) {}
        fn is_cancelled(&self) -> bool {
            self.polls.set(self.polls.get() + 1);
            if self.polls.get() == self.n
                && let Some(f) = self.f.borrow_mut().take()
            {
                f();
            }
            false
        }
        fn on_conflict(&mut self, d: &Path) -> ConflictAction {
            self.conflicts.push(d.to_path_buf());
            self.action
        }
    }

    /// Poll order: copy_loop's copy_entry (1), then copy_dir's pre-create check (2).
    fn dir_race_setup(
        action: ConflictAction,
        file_intruder: bool,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PollHook) {
        let (t, s, d) = two_dirs();
        fs::create_dir(s.join("x")).unwrap();
        fs::write(s.join("x/new"), "NEW").unwrap();
        let dc = d.join("x");
        let h = PollHook::new(2, action, move || {
            if file_intruder {
                fs::write(&dc, "INTRUDER").unwrap();
            } else {
                fs::create_dir(&dc).unwrap();
                fs::write(dc.join("keep"), "KEEP").unwrap();
            }
        });
        (t, s, d, h)
    }

    #[test]
    fn dir_appearing_before_create_asks_and_skip_keeps_intruder() {
        let (_t, s, d, mut h) = dir_race_setup(ConflictAction::Skip, false);
        let r = copy_items(&[s.join("x")], &d, &mut h);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(h.conflicts, vec![d.join("x")]);
        assert_eq!(fs::read_to_string(d.join("x/keep")).unwrap(), "KEEP");
        assert!(!d.join("x/new").exists());
        assert_eq!((r.skipped, r.copied), (1, 0));
        assert_eq!(names(&d), vec!["x"]);
    }

    #[test]
    fn dir_appearing_before_create_overwrite_merges_and_cancel_stops() {
        let (_t, s, d, mut h) = dir_race_setup(ConflictAction::Overwrite, false);
        let r = copy_items(&[s.join("x")], &d, &mut h);
        assert_eq!(h.conflicts, vec![d.join("x")]);
        assert_eq!(r.copied, 1);
        assert_eq!(fs::read_to_string(d.join("x/keep")).unwrap(), "KEEP");
        assert_eq!(fs::read_to_string(d.join("x/new")).unwrap(), "NEW");

        let (_t, s, d, mut h) = dir_race_setup(ConflictAction::Cancel, true);
        let r = copy_items(&[s.join("x")], &d, &mut h);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert_eq!(fs::read_to_string(d.join("x")).unwrap(), "INTRUDER");
        assert_eq!(names(&d), vec!["x"]);
    }

    #[test]
    fn restore_backup_keeps_both_when_dest_is_not_ours() {
        let (_t, _s, d) = two_dirs();
        let (b, dest) = (d.join(".hdir-bak-x"), d.join("x"));
        fs::write(&b, "ORIGINAL").unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("keep"), "KEEP").unwrap();
        let mut acc = Acc::default();
        let e = restore_backup(&b, &dest, &mut acc).unwrap_err();
        assert!(e.to_string().contains(b.to_str().unwrap()), "{e}");
        assert_eq!(fs::read_to_string(&b).unwrap(), "ORIGINAL");
        assert_eq!(fs::read_to_string(dest.join("keep")).unwrap(), "KEEP");

        // A dest this operation created is discarded and the original returns.
        fs::remove_dir_all(&dest).unwrap();
        fs::create_dir(&dest).unwrap();
        acc.created.push(dest.clone());
        restore_backup(&b, &dest, &mut acc).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "ORIGINAL");
    }

    #[test]
    fn move_dir_over_file_intruder_dir_is_kept_with_backup() {
        let (_t, s, d) = dir_over_file_setup();
        let dc = d.join("x");
        // Polls: move_entry (1), copy_entry (2), copy_dir (3) -> then create_dir.
        // The original file is parked by now; an intruder takes its name.
        let mut h = PollHook::new(3, ConflictAction::Skip, move || {
            // x is parked, so the name is free
            fs::create_dir(&dc).unwrap();
            fs::write(dc.join("keep"), "KEEP").unwrap();
        });
        // First conflict (file at d/x) is answered Skip too, so use Overwrite first.
        struct Seq<'a>(&'a mut PollHook, Vec<ConflictAction>);
        impl OpControl for Seq<'_> {
            fn on_progress(&mut self, _: u64, _: &Path) {}
            fn is_cancelled(&self) -> bool {
                self.0.is_cancelled()
            }
            fn on_conflict(&mut self, d: &Path) -> ConflictAction {
                self.0.conflicts.push(d.to_path_buf());
                self.1.remove(0)
            }
        }
        let mut c = Seq(
            &mut h,
            vec![ConflictAction::Overwrite, ConflictAction::Skip],
        );
        let r = move_items_with(&[s.join("x")], &d, &mut c, &exdev_err);
        let e = failed(r);
        assert!(e.contains(".hdir-bak"), "{e}");
        assert_eq!(fs::read_to_string(d.join("x/keep")).unwrap(), "KEEP");
        assert!(!d.join("x/f").exists());
        let bak: Vec<_> = names(&d)
            .into_iter()
            .filter(|n| n.starts_with(".hdir-bak"))
            .collect();
        assert_eq!(bak.len(), 1);
        assert_eq!(fs::read_to_string(d.join(&bak[0])).unwrap(), "ORIGINAL");
        assert_eq!(fs::read_to_string(s.join("x/f")).unwrap(), "inner");
    }

    #[test]
    fn copy_file_helper_never_follows_a_swapped_symlink() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("secret"), "SECRET").unwrap();
        std::os::unix::fs::symlink(s.join("secret"), s.join("link")).unwrap();
        let mut acc = Acc::default();
        let step =
            super::copy_file(&s.join("link"), &d.join("out"), &mut ctl(), &mut acc, false).unwrap();
        assert_eq!(step, Step::Skipped);
        assert_eq!(acc.skipped, 1);
        assert_eq!(acc.warnings.len(), 1, "{:?}", acc.warnings);
        assert!(names(&d).is_empty());

        // A directory swapped in is not a regular file either.
        fs::create_dir(s.join("dir")).unwrap();
        let step =
            super::copy_file(&s.join("dir"), &d.join("out2"), &mut ctl(), &mut acc, false).unwrap();
        assert_eq!(step, Step::Skipped);
        assert!(names(&d).is_empty());

        // A regular file still copies, with its own permissions.
        fs::set_permissions(s.join("secret"), fs::Permissions::from_mode(0o640)).unwrap();
        let step = super::copy_file(
            &s.join("secret"),
            &d.join("ok"),
            &mut ctl(),
            &mut acc,
            false,
        )
        .unwrap();
        assert_eq!(step, Step::Done);
        let m = fs::metadata(d.join("ok")).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o640);
    }

    fn os_unsupported(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    #[test]
    fn noreplace_fallback_links_regular_files_and_never_clobbers() {
        let (_t, s, d) = two_dirs();
        let link = |a: &Path, b: &Path| fs::hard_link(a, b);
        fs::write(s.join("f"), "NEW").unwrap();
        rename_noreplace_with(&s.join("f"), &d.join("f"), &os_unsupported, &link).unwrap();
        assert_eq!(fs::read_to_string(d.join("f")).unwrap(), "NEW");
        assert!(!s.join("f").exists());

        fs::write(s.join("g"), "NEW2").unwrap();
        fs::write(d.join("g"), "OLD").unwrap();
        let e =
            rename_noreplace_with(&s.join("g"), &d.join("g"), &os_unsupported, &link).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(d.join("g")).unwrap(), "OLD");
        assert_eq!(fs::read_to_string(s.join("g")).unwrap(), "NEW2");
    }

    #[test]
    fn noreplace_fallback_refuses_dirs_and_failed_links() {
        let (_t, s, d) = two_dirs();
        let link = |a: &Path, b: &Path| fs::hard_link(a, b);
        fs::create_dir(s.join("x")).unwrap();
        let e =
            rename_noreplace_with(&s.join("x"), &d.join("x"), &os_unsupported, &link).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(s.join("x").is_dir() && !d.join("x").exists());

        fs::write(s.join("f"), "F").unwrap();
        for code in [libc::EXDEV, libc::EPERM, libc::ENOTSUP] {
            let bad = move |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(code));
            let e = rename_noreplace_with(&s.join("f"), &d.join("f"), &os_unsupported, &bad)
                .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{code}");
            assert_eq!(fs::read_to_string(s.join("f")).unwrap(), "F");
            assert!(!d.join("f").exists());
        }
    }

    #[test]
    fn report_done_sources_lists_only_fully_processed_items() {
        let (_t, s, d) = two_dirs();
        fs::write(s.join("a"), "A").unwrap();
        fs::write(s.join("b"), "B").unwrap();
        fs::write(d.join("b"), "OLD").unwrap();
        let mut c = Ctl::new(ConflictAction::Skip);
        let r = copy_items(&[s.join("a"), s.join("b")], &d, &mut c);
        assert_eq!((r.copied, r.skipped), (1, 1));
        assert_eq!(r.done_sources, vec![s.join("a")]);

        fs::remove_file(d.join("a")).unwrap();
        let mut c = Ctl::new(ConflictAction::Skip);
        let r = move_items(&[s.join("a"), s.join("b")], &d, &mut c);
        assert_eq!(r.done_sources, vec![s.join("a")]);
    }
}
