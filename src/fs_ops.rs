//! Pure filesystem operations (copy / move / mkdir / rename / trash). No UI.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Overwrite,
    Skip,
    Cancel,
}

pub trait OpControl {
    fn on_progress(&mut self, bytes_done_delta: u64, current: &Path);
    fn on_file_done(&mut self, _path: &Path) {}
    fn is_cancelled(&self) -> bool;
    fn on_conflict(&mut self, dest: &Path) -> ConflictAction;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpOutcome {
    Done,
    Cancelled,
}

const CHUNK: usize = 1 << 20;
const EXDEV: i32 = 18;

/// Result of one entry operation. `Partial` = completed but something was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Done,
    Partial,
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
    fn walk(p: &Path, t: &mut Totals) -> io::Result<()> {
        let m = fs::symlink_metadata(p)?;
        if m.is_dir() {
            for e in fs::read_dir(p)? {
                walk(&e?.path(), t)?;
            }
        } else {
            t.files += 1;
            if m.is_file() {
                t.bytes += m.len();
            }
        }
        Ok(())
    }
    let mut t = Totals { files: 0, bytes: 0 };
    for s in sources {
        walk(s, &mut t)?;
    }
    Ok(t)
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

fn copy_file(
    src: &Path,
    dest: &Path,
    meta: &fs::Metadata,
    ctl: &mut dyn OpControl,
) -> io::Result<Step> {
    let mut input = fs::File::open(src)?;
    let mut out = fs::File::create(dest)?;
    let mut buf = vec![0u8; CHUNK];
    let res = (|| -> io::Result<bool> {
        loop {
            if ctl.is_cancelled() {
                return Ok(false);
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                return Ok(true);
            }
            out.write_all(&buf[..n])?;
            ctl.on_progress(n as u64, src);
        }
    })();
    match res {
        Ok(true) => {
            out.set_modified(meta.modified()?)?;
            drop(out);
            fs::set_permissions(dest, meta.permissions())?;
            ctl.on_file_done(dest);
            Ok(Step::Done)
        }
        Ok(false) => {
            drop(out);
            let _ = fs::remove_file(dest);
            Ok(Step::Cancelled)
        }
        Err(e) => {
            drop(out);
            let _ = fs::remove_file(dest);
            Err(e)
        }
    }
}

fn copy_dir(
    src: &Path,
    dest: &Path,
    meta: &fs::Metadata,
    ctl: &mut dyn OpControl,
) -> io::Result<Step> {
    let created = !is_real_dir(dest);
    if created {
        if exists(dest) {
            fs::remove_file(dest)?;
        }
        fs::create_dir(dest)?;
    }
    let mut result = Step::Done;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        match copy_entry(&entry.path(), &dest.join(entry.file_name()), ctl, true)? {
            Step::Cancelled => return Ok(Step::Cancelled),
            Step::Partial => result = Step::Partial,
            Step::Done => {}
        }
    }
    if created {
        fs::File::open(dest)?.set_modified(meta.modified()?)?;
        fs::set_permissions(dest, meta.permissions())?;
    }
    Ok(result)
}

/// Copy one entry. With `prompt`, an existing destination triggers `on_conflict`.
fn copy_entry(src: &Path, dest: &Path, ctl: &mut dyn OpControl, prompt: bool) -> io::Result<Step> {
    if ctl.is_cancelled() {
        return Ok(Step::Cancelled);
    }
    let meta = fs::symlink_metadata(src)?;
    if prompt && exists(dest) {
        match ctl.on_conflict(dest) {
            ConflictAction::Skip => return Ok(Step::Partial),
            ConflictAction::Cancel => return Ok(Step::Cancelled),
            ConflictAction::Overwrite => {}
        }
    }
    if meta.is_dir() {
        return copy_dir(src, dest, &meta, ctl);
    }
    if is_real_dir(dest) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination is a directory",
        ));
    }
    if exists(dest) {
        fs::remove_file(dest)?;
    }
    if meta.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dest)?;
        ctl.on_file_done(dest);
        Ok(Step::Done)
    } else {
        copy_file(src, dest, &meta, ctl)
    }
}

pub fn copy_items(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
) -> io::Result<OpOutcome> {
    let dests = check_all(sources, dest_dir)?;
    for (src, dest) in sources.iter().zip(&dests) {
        if copy_entry(src, dest, ctl, true)? == Step::Cancelled {
            return Ok(OpOutcome::Cancelled);
        }
    }
    Ok(OpOutcome::Done)
}

pub fn move_items(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
) -> io::Result<OpOutcome> {
    move_items_with(sources, dest_dir, ctl, &|a, b| fs::rename(a, b))
}

fn remove_source(src: &Path) -> io::Result<()> {
    if is_real_dir(src) {
        fs::remove_dir_all(src)
    } else {
        fs::remove_file(src)
    }
}

/// `rename_fn` is injectable so the cross-device fallback can be tested.
fn move_items_with(
    sources: &[PathBuf],
    dest_dir: &Path,
    ctl: &mut dyn OpControl,
    rename_fn: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<OpOutcome> {
    let dests = check_all(sources, dest_dir)?;
    for (src, dest) in sources.iter().zip(&dests) {
        if ctl.is_cancelled() {
            return Ok(OpOutcome::Cancelled);
        }
        if exists(dest) {
            match ctl.on_conflict(dest) {
                ConflictAction::Skip => continue,
                ConflictAction::Cancel => return Ok(OpOutcome::Cancelled),
                ConflictAction::Overwrite => {}
            }
            if is_real_dir(src) && is_real_dir(dest) {
                // Merge: cannot rename over a directory.
                match copy_entry(src, dest, ctl, false)? {
                    Step::Cancelled => return Ok(OpOutcome::Cancelled),
                    Step::Done => remove_source(src)?,
                    Step::Partial => {}
                }
                continue;
            }
            if is_real_dir(dest) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination is a directory",
                ));
            }
            fs::remove_file(dest)?;
        }
        match rename_fn(src, dest) {
            Ok(()) => {}
            Err(e) if is_cross_device(&e) => match copy_entry(src, dest, ctl, false)? {
                Step::Cancelled => return Ok(OpOutcome::Cancelled),
                Step::Done => remove_source(src)?,
                Step::Partial => {}
            },
            Err(e) => return Err(e),
        }
    }
    Ok(OpOutcome::Done)
}

/// Whether a rename failure means "different filesystem" (EXDEV).
fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(EXDEV)
}

pub fn make_dir(parent: &Path, name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    let p = parent.join(name);
    fs::create_dir(&p)?;
    Ok(p)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    validate_name(new_name)?;
    let target = path.with_file_name(new_name);
    if exists(&target) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "target already exists",
        ));
    }
    fs::rename(path, &target)?;
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
            copy_items(std::slice::from_ref(&src), &dst, &mut c).unwrap(),
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
        copy_items(&[src], &dst, &mut c).unwrap();
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
        copy_items(&[link], &dst, &mut ctl()).unwrap();
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
        assert_eq!(copy_items(&[src], &dst, &mut c).unwrap(), OpOutcome::Done);
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "new");
        assert_eq!(c.conflicts, vec![dst.join("f.txt")]);
    }

    #[test]
    fn conflict_skip() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Skip);
        assert_eq!(copy_items(&[src], &dst, &mut c).unwrap(), OpOutcome::Done);
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn conflict_cancel() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Cancel);
        assert_eq!(
            copy_items(&[src], &dst, &mut c).unwrap(),
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
        copy_items(&[src], &dst, &mut c).unwrap();
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
            let e = copy_items(std::slice::from_ref(&src), &dest, &mut ctl()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            let e = move_items(std::slice::from_ref(&src), &dest, &mut ctl()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn same_path_rejected() {
        let t = tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, "x").unwrap();
        let e = copy_items(std::slice::from_ref(&f), t.path(), &mut ctl()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let e = move_items(std::slice::from_ref(&f), t.path(), &mut ctl()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
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
            copy_items(&[src], &dst, &mut c).unwrap(),
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
        copy_items(&[src], &dst, &mut c).unwrap();
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
            move_items(std::slice::from_ref(&src), &dst, &mut ctl()).unwrap(),
            OpOutcome::Done
        );
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("d/f")).unwrap(), "x");
    }

    #[test]
    fn move_conflict_skip_keeps_source() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Skip);
        move_items(std::slice::from_ref(&src), &dst, &mut c).unwrap();
        assert!(src.exists());
        assert_eq!(fs::read_to_string(dst.join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn move_conflict_overwrite_replaces() {
        let (_t, src, dst) = conflict_setup();
        let mut c = Ctl::new(ConflictAction::Overwrite);
        move_items(std::slice::from_ref(&src), &dst, &mut c).unwrap();
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
        let r = move_items_with(std::slice::from_ref(&src), &dst, &mut c, &exdev).unwrap();
        assert_eq!(r, OpOutcome::Done);
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
        let r = move_items_with(std::slice::from_ref(&src), &dst, &mut c, &exdev).unwrap();
        assert_eq!(r, OpOutcome::Cancelled);
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
        let e = move_items_with(std::slice::from_ref(&src), &dst, &mut ctl(), &denied).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
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
        copy_items(&[src], &dst, &mut ctl()).unwrap();
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
}
