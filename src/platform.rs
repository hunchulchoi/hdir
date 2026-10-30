//! Platform-specific filesystem primitives (unix / windows).
//!
//! Everything that needs `std::os::unix`, `libc` or Win32 lives here so that
//! `fs_ops` stays platform-neutral.
//!
//! Known platform differences (documented limitations):
//! - Private temp permissions (0600 files / 0700 dirs) exist on unix only. On
//!   Windows only the readonly attribute is copied from the source.
//! - Windows has no `O_NOFOLLOW`; sources are opened with
//!   `FILE_FLAG_OPEN_REPARSE_POINT` after an `lstat`, and the opened handle is
//!   re-checked for a reparse point.
//! - Windows has no `file_index` on stable std, so the case-only rename identity
//!   check compares canonicalized paths case-insensitively.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::Path;

// ---------------------------------------------------------------- names

/// `name` capped to `max` bytes (unix, byte-exact) or characters (windows).
#[cfg(unix)]
pub fn truncated_name(name: &OsStr, max: usize) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let b = name.as_bytes();
    OsString::from_vec(b[..b.len().min(max)].to_vec())
}

#[cfg(windows)]
pub fn truncated_name(name: &OsStr, max: usize) -> OsString {
    name.to_string_lossy()
        .chars()
        .take(max)
        .collect::<String>()
        .into()
}

// ------------------------------------------------------- os string <-> bytes

/// Bytes of an `OsStr`. Unix: the raw bytes. Windows: UTF-8 of the lossy
/// conversion (unpaired surrogates become U+FFFD). Used for remote (POSIX)
/// command text, which is byte oriented regardless of the local OS.
#[cfg(unix)]
pub fn os_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(s.as_bytes())
}

#[cfg(windows)]
pub fn os_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    match s.to_string_lossy() {
        Cow::Borrowed(b) => Cow::Borrowed(b.as_bytes()),
        Cow::Owned(o) => Cow::Owned(o.into_bytes()),
    }
}

/// Inverse of [`os_bytes`]. Windows: invalid UTF-8 becomes U+FFFD.
#[cfg(unix)]
pub fn os_from_bytes(b: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(b)
}

#[cfg(windows)]
pub fn os_from_bytes(b: Vec<u8>) -> OsString {
    String::from_utf8_lossy(&b).into_owned().into()
}

// ----------------------------------------------------------- open / create

/// Open a copy source for reading without following links.
///
/// `Ok(None)` means the entry is not (or no longer) a plain file: a symlink or
/// reparse point was swapped in since the caller's lstat.
#[cfg(unix)]
pub fn open_source(src: &Path) -> io::Result<Option<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    // Never follow a symlink (O_NOFOLLOW) and never block on a FIFO (O_NONBLOCK).
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
    {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
pub fn open_source(src: &Path) -> io::Result<Option<fs::File>> {
    open_source_with(src, &|| {}, &|f| {
        use std::os::windows::fs::MetadataExt;
        Ok(f.metadata()?.file_attributes())
    })
}

/// [`open_source`] with two seams: `between` runs after the lstat check and
/// before the open (a place to swap the entry), `attrs` reads the opened handle's
/// file attributes.
#[cfg(windows)]
fn open_source_with(
    src: &Path,
    between: &dyn Fn(),
    attrs: &dyn Fn(&fs::File) -> io::Result<u32>,
) -> io::Result<Option<fs::File>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let lmeta = fs::symlink_metadata(src)?;
    if lmeta.file_type().is_symlink() || !lmeta.is_file() {
        return Ok(None);
    }
    between();
    // Do not traverse a reparse point swapped in after the lstat, then verify the
    // handle we actually got.
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(src)?;
    if attrs(&f)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(None);
    }
    Ok(Some(f))
}

/// New file, private (0600) until complete on unix.
#[cfg(unix)]
pub fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
pub fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// New directory (atomic, fails if present), private (0700) on unix.
#[cfg(unix)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Set the modification time of a directory.
#[cfg(unix)]
pub fn set_dir_modified(dir: &Path, t: std::time::SystemTime) -> io::Result<()> {
    fs::File::open(dir)?.set_modified(t)
}

#[cfg(windows)]
pub fn set_dir_modified(dir: &Path, t: std::time::SystemTime) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .set_modified(t)
}

// ----------------------------------------------------------------- rename

/// Rename that replaces an existing `to` on purpose.
#[cfg(unix)]
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    match move_file_ex(from, to, MOVEFILE_REPLACE_EXISTING) {
        // A readonly destination file blocks the replace on Windows, unlike a
        // POSIX rename. The caller already approved overwriting: lift the attribute
        // for the replace and put it back if the replace still fails.
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            let Ok(meta) = fs::symlink_metadata(to) else {
                return Err(e);
            };
            if !meta.is_file() || !meta.permissions().readonly() {
                return Err(e);
            }
            let mut writable = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            writable.set_readonly(false);
            fs::set_permissions(to, writable)?;
            let res = move_file_ex(from, to, MOVEFILE_REPLACE_EXISTING);
            if res.is_err() {
                let _ = fs::set_permissions(to, meta.permissions());
            }
            res
        }
        other => other,
    }
}

/// `path` in `\\?\` form when it is absolute and too long for `MAX_PATH`
/// (std does this for its own calls, raw Win32 calls must do it themselves).
#[cfg(windows)]
pub fn long_path(path: &Path) -> std::path::PathBuf {
    use std::path::{Component, Prefix};
    const SHORT: usize = 240; // below MAX_PATH (260) with room for a trailing name
    if path.as_os_str().len() < SHORT {
        return path.to_path_buf();
    }
    let mut comps = path.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return path.to_path_buf(); // relative: leave it alone
    };
    let mut out = match prefix.kind() {
        Prefix::Disk(d) => format!(r"\\?\{}:", d as char),
        Prefix::UNC(server, share) => format!(
            r"\\?\UNC\{}\{}",
            server.to_string_lossy(),
            share.to_string_lossy()
        ),
        _ => return path.to_path_buf(), // already verbatim or a device path
    };
    for c in comps {
        match c {
            Component::RootDir => {}
            Component::Normal(n) => {
                out.push('\\');
                out.push_str(&n.to_string_lossy());
            }
            // `.`/`..` are not resolved in verbatim paths: keep the original.
            _ => return path.to_path_buf(),
        }
    }
    out.into()
}

/// `MoveFileExW` with `flags`; without `MOVEFILE_REPLACE_EXISTING` it fails with
/// `ERROR_ALREADY_EXISTS` (-> `AlreadyExists`) if `to` exists, and without
/// `MOVEFILE_COPY_ALLOWED` a cross-volume move fails with `ERROR_NOT_SAME_DEVICE`.
#[cfg(windows)]
fn move_file_ex(
    from: &Path,
    to: &Path,
    flags: windows_sys::Win32::Storage::FileSystem::MOVE_FILE_FLAGS,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| -> io::Result<Vec<u16>> {
        let p = long_path(p);
        let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
        if v.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "nul in path"));
        }
        v.push(0);
        Ok(v)
    };
    let (f, t) = (wide(from)?, wide(to)?);
    // SAFETY: both buffers are valid NUL-terminated UTF-16 strings for the call.
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), flags) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether a rename failure means "different filesystem" (EXDEV).
#[cfg(unix)]
pub fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(windows)]
pub fn is_cross_device(e: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE;
    e.kind() == io::ErrorKind::CrossesDevices
        || e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32)
}

/// The atomic no-replace rename is unavailable here (fall back to link + unlink).
#[cfg(unix)]
pub fn is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
    )
}

#[cfg(windows)]
pub fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
}

/// A hard link was refused by the filesystem (e.g. EPERM on some mounts).
#[cfg(unix)]
pub fn is_link_refused(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_link_refused(_e: &io::Error) -> bool {
    false
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn rename_noreplace_os(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "nul in path");
    let f = CString::new(from.as_os_str().as_bytes()).map_err(|_| bad())?;
    let t = CString::new(to.as_os_str().as_bytes()).map_err(|_| bad())?;
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

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn rename_noreplace_os(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

#[cfg(windows)]
pub fn rename_noreplace_os(from: &Path, to: &Path) -> io::Result<()> {
    // Atomic and never replaces: fails with AlreadyExists if `to` exists.
    move_file_ex(from, to, 0)
}

// ------------------------------------------------------------ test helpers

/// A cross-device rename failure as the OS reports it.
#[cfg(all(test, unix))]
pub fn test_exdev() -> io::Error {
    io::Error::from_raw_os_error(libc::EXDEV)
}

#[cfg(all(test, windows))]
pub fn test_exdev() -> io::Error {
    io::Error::from_raw_os_error(windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32)
}

/// "Atomic no-replace rename unsupported" as the OS reports it.
#[cfg(all(test, unix))]
pub fn test_unsupported() -> io::Error {
    io::Error::from_raw_os_error(libc::ENOTSUP)
}

#[cfg(all(test, windows))]
pub fn test_unsupported() -> io::Error {
    io::Error::from(io::ErrorKind::Unsupported)
}

/// Raw OS codes a hard link can fail with that must all end as "unsupported".
#[cfg(all(test, unix))]
pub fn test_link_failures() -> Vec<i32> {
    vec![libc::EXDEV, libc::EPERM, libc::ENOTSUP]
}

#[cfg(all(test, windows))]
pub fn test_link_failures() -> Vec<i32> {
    vec![windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE as i32]
}

// --------------------------------------------------------------- identity

/// Whether `a` and `b` are the same filesystem object (no symlink following).
#[cfg(unix)]
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(windows)]
pub fn same_file(a: &Path, b: &Path) -> bool {
    // `file_index` is not stable: compare the resolved paths case-insensitively.
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

// --------------------------------------------------------------- symlinks

/// Create a symlink at `link` pointing to `target`. `src` is the link being
/// copied, used on Windows to choose a file or directory symlink.
#[cfg(unix)]
pub fn create_symlink(target: &Path, link: &Path, _src: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn create_symlink(target: &Path, link: &Path, src: &Path) -> io::Result<()> {
    // Windows distinguishes file and directory links. The kind is a property of
    // the source link itself, so a broken or inaccessible dir link stays a dir link.
    use std::os::windows::fs::FileTypeExt;
    if fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_symlink_dir()) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Delete a symlink (or any non-directory entry). Windows directory symlinks
/// need `remove_dir`; file links and files need `remove_file`.
#[cfg(unix)]
pub fn remove_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
pub fn remove_link(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink_dir()) {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Creating a symlink was refused for lack of privilege (Windows without
/// developer mode / `SeCreateSymbolicLinkPrivilege`).
#[cfg(unix)]
pub fn is_symlink_privilege_error(_e: &io::Error) -> bool {
    false
}

#[cfg(windows)]
pub fn is_symlink_privilege_error(e: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
    e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD as i32)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn long_paths_get_the_verbatim_prefix_short_ones_do_not() {
        let short = Path::new(r"C:\a\b");
        assert_eq!(long_path(short), short);
        let long = format!(r"C:\{}\f", "x".repeat(250));
        assert_eq!(
            long_path(Path::new(&long)),
            Path::new(&format!(r"\\?\C:\{}\f", "x".repeat(250)))
        );
        let unc = format!(r"\\srv\share\{}\f", "x".repeat(250));
        assert_eq!(
            long_path(Path::new(&unc)),
            Path::new(&format!(r"\\?\UNC\srv\share\{}\f", "x".repeat(250)))
        );
        let rel = "y".repeat(300);
        assert_eq!(long_path(Path::new(&rel)), Path::new(&rel));
    }

    #[test]
    fn swapped_in_symlink_between_check_and_open_is_never_read() {
        use std::os::windows::fs::symlink_file;
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        let secret = t.path().join("secret");
        fs::write(&f, "plain").unwrap();
        fs::write(&secret, "SECRET").unwrap();
        let swapped = std::cell::Cell::new(false);
        let swap = || {
            fs::remove_file(&f).unwrap();
            match symlink_file(&secret, &f) {
                Ok(()) => swapped.set(true),
                Err(e) if is_symlink_privilege_error(&e) => fs::write(&f, "plain").unwrap(),
                Err(e) => panic!("swap failed: {e}"),
            }
        };
        let real = |h: &fs::File| {
            use std::os::windows::fs::MetadataExt;
            Ok(h.metadata()?.file_attributes())
        };
        let r = open_source_with(&f, &swap, &real).unwrap();
        if swapped.get() {
            assert!(r.is_none(), "a swapped-in symlink must not be opened");
        } else {
            // No symlink privilege: simulate the handle re-check seeing a reparse point.
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            let r = open_source_with(&f, &|| {}, &|_| Ok(FILE_ATTRIBUTE_REPARSE_POINT)).unwrap();
            assert!(r.is_none(), "reparse-point handle must be rejected");
        }
    }

    #[test]
    fn move_file_ex_works_beyond_max_path() {
        let t = tempfile::tempdir().unwrap();
        let mut d = t.path().to_path_buf();
        while d.as_os_str().len() < 280 {
            d.push("deepdir_deepdir");
        }
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a"), "x").unwrap();
        rename_noreplace_os(&d.join("a"), &d.join("b")).unwrap();
        assert_eq!(fs::read_to_string(d.join("b")).unwrap(), "x");
    }
}
