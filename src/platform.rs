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

// ---- extended attributes (Finder tags, resource forks, `user.*`) ----
//
// Copied best effort on Linux and macOS; elsewhere nothing happens. Paths
// are never followed through a final symlink. A target filesystem without
// xattr support is not an error.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod xattr_sys {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::raw::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    use std::path::Path;

    /// What the calls act on: an open file, or a path (not followed).
    #[derive(Clone, Copy)]
    pub enum On<'a> {
        Fd(RawFd),
        Path(&'a CStr),
    }

    pub fn cpath(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
    }

    fn check(n: isize) -> io::Result<usize> {
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    #[cfg(target_os = "macos")]
    const NOFOLLOW: i32 = libc::XATTR_NOFOLLOW;

    // SAFETY (all calls below): the buffers are valid for the lengths passed,
    // names and paths are NUL-terminated C strings.
    fn raw_list(on: On, buf: *mut c_char, len: usize) -> io::Result<usize> {
        check(unsafe {
            match on {
                #[cfg(target_os = "linux")]
                On::Fd(fd) => libc::flistxattr(fd, buf, len),
                #[cfg(target_os = "linux")]
                On::Path(p) => libc::llistxattr(p.as_ptr(), buf, len),
                #[cfg(target_os = "macos")]
                On::Fd(fd) => libc::flistxattr(fd, buf, len, 0),
                #[cfg(target_os = "macos")]
                On::Path(p) => libc::listxattr(p.as_ptr(), buf, len, NOFOLLOW),
            }
        })
    }

    fn raw_get(on: On, name: &CStr, buf: *mut c_void, len: usize) -> io::Result<usize> {
        check(unsafe {
            match on {
                #[cfg(target_os = "linux")]
                On::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), buf, len),
                #[cfg(target_os = "linux")]
                On::Path(p) => libc::lgetxattr(p.as_ptr(), name.as_ptr(), buf, len),
                #[cfg(target_os = "macos")]
                On::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), buf, len, 0, 0),
                #[cfg(target_os = "macos")]
                On::Path(p) => libc::getxattr(p.as_ptr(), name.as_ptr(), buf, len, 0, NOFOLLOW),
            }
        })
    }

    pub fn set(on: On, name: &CStr, value: &[u8]) -> io::Result<()> {
        let (v, n) = (value.as_ptr().cast::<c_void>(), value.len());
        let r = unsafe {
            match on {
                #[cfg(target_os = "linux")]
                On::Fd(fd) => libc::fsetxattr(fd, name.as_ptr(), v, n, 0),
                #[cfg(target_os = "linux")]
                On::Path(p) => libc::lsetxattr(p.as_ptr(), name.as_ptr(), v, n, 0),
                #[cfg(target_os = "macos")]
                On::Fd(fd) => libc::fsetxattr(fd, name.as_ptr(), v, n, 0, 0),
                #[cfg(target_os = "macos")]
                On::Path(p) => libc::setxattr(p.as_ptr(), name.as_ptr(), v, n, 0, NOFOLLOW),
            }
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Size query then read; retried when the value grew in between.
    fn read_sized(
        mut read: impl FnMut(*mut u8, usize) -> io::Result<usize>,
    ) -> io::Result<Vec<u8>> {
        for _ in 0..4 {
            let len = read(std::ptr::null_mut(), 0)?;
            let mut buf = vec![0u8; len];
            match read(buf.as_mut_ptr(), len) {
                // with a 0-length buffer the call is a size query again: a
                // value that grew in between must be read anew
                Ok(n) if n <= len && (len > 0 || n == 0) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Ok(_) => continue,
                Err(e) if e.raw_os_error() == Some(libc::ERANGE) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::from_raw_os_error(libc::ERANGE))
    }

    pub fn names(on: On) -> io::Result<Vec<CString>> {
        let all = read_sized(|b, n| raw_list(on, b.cast(), n))?;
        Ok(all
            .split(|&c| c == 0)
            .filter(|n| !n.is_empty())
            .filter_map(|n| CString::new(n).ok())
            .collect())
    }

    pub fn get(on: On, name: &CStr) -> io::Result<Vec<u8>> {
        read_sized(|b, n| raw_get(on, name, b.cast(), n))
    }

    pub fn is_missing(e: &io::Error) -> bool {
        #[cfg(target_os = "linux")]
        let nodata = libc::ENODATA;
        #[cfg(target_os = "macos")]
        let nodata = libc::ENOATTR;
        e.raw_os_error() == Some(nodata)
    }

    pub fn is_unsupported(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(c) if c == libc::ENOTSUP || c == libc::EOPNOTSUPP)
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        /// A reader answering size queries and reads from a script of
        /// (size, value) steps, like a value that changes between calls.
        fn scripted(
            steps: Vec<io::Result<Vec<u8>>>,
        ) -> impl FnMut(*mut u8, usize) -> io::Result<usize> {
            let mut steps = steps.into_iter();
            move |buf, len| {
                let v = steps.next().expect("no more calls")?;
                if len > 0 && v.len() <= len {
                    // SAFETY: the caller's buffer holds `len` bytes.
                    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len()) };
                }
                Ok(v.len())
            }
        }

        #[test]
        fn a_value_that_grows_from_zero_is_read_again() {
            // size 0, then (buffer of 0) it is now 3 long, size 3, read 3
            let r = read_sized(scripted(vec![
                Ok(vec![]),
                Ok(b"abc".to_vec()),
                Ok(b"abc".to_vec()),
                Ok(b"abc".to_vec()),
            ]));
            assert_eq!(r.unwrap(), b"abc");
        }

        #[test]
        fn erange_retries_and_then_gives_up() {
            let erange = || Err(io::Error::from_raw_os_error(libc::ERANGE));
            let r = read_sized(scripted(vec![
                Ok(b"ab".to_vec()),
                erange(),
                Ok(b"abcd".to_vec()),
                Ok(b"abcd".to_vec()),
            ]));
            assert_eq!(r.unwrap(), b"abcd");
            let mut steps = Vec::new();
            for _ in 0..4 {
                steps.push(Ok(b"ab".to_vec()));
                steps.push(erange());
            }
            let e = read_sized(scripted(steps)).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::ERANGE));
        }
    }
}

/// Copy every extended attribute from `from` to `to`. Attributes the target
/// filesystem cannot hold at all are not an error; the first other failure
/// is returned after trying the rest.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_xattrs_on(from: xattr_sys::On, to: xattr_sys::On) -> io::Result<()> {
    let names = match xattr_sys::names(from) {
        Ok(n) => n,
        Err(e) if xattr_sys::is_unsupported(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut first = None;
    for name in names {
        let res = xattr_sys::get(from, &name).and_then(|v| xattr_sys::set(to, &name, &v));
        match res {
            Ok(()) => {}
            Err(e) if xattr_sys::is_unsupported(&e) || xattr_sys::is_missing(&e) => {}
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// [`copy_xattrs_on`] between two open files.
pub fn copy_xattrs_fd(from: &fs::File, to: &fs::File) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::io::AsRawFd;
        use xattr_sys::On;
        copy_xattrs_on(On::Fd(from.as_raw_fd()), On::Fd(to.as_raw_fd()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (from, to);
        Ok(())
    }
}

/// [`copy_xattrs_on`] between two paths (final symlinks not followed).
pub fn copy_xattrs_path(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use xattr_sys::{On, cpath};
        let (f, t) = (cpath(from)?, cpath(to)?);
        copy_xattrs_on(On::Path(&f), On::Path(&t))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (from, to);
        Ok(())
    }
}

/// Set one extended attribute on `path` (not followed).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn xattr_set_path(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let p = xattr_sys::cpath(path)?;
    let n = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
    xattr_sys::set(xattr_sys::On::Path(&p), &n, value)
}

/// One extended attribute of `path` (not followed); `None` when absent.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn xattr_get_path(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    let p = xattr_sys::cpath(path)?;
    let n = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
    match xattr_sys::get(xattr_sys::On::Path(&p), &n) {
        Ok(v) => Ok(Some(v)),
        Err(e) if xattr_sys::is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

// ---- copy sources opened relative to their folder (HDIR-63) ----
//
// A folder being copied is opened once, without following links, and every
// entry inside it is reached through that handle (`*at` calls). Swapping the
// folder, or any folder above it, for a symlink while the copy runs can then
// never redirect the copy elsewhere. On Windows the copy stays path based.

/// What a copy source entry is, as seen without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrcKind {
    Dir,
    File,
    Symlink,
    Other,
}

/// An open source folder (unix).
#[cfg(unix)]
#[derive(Debug)]
pub struct DirFd(std::os::fd::OwnedFd);

/// Never constructed off unix (it has no values): copies there are path
/// based.
#[cfg(not(unix))]
#[derive(Debug)]
pub enum DirFd {}

#[cfg(not(unix))]
impl DirFd {
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        match *self {}
    }
    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        match *self {}
    }
    pub fn copy_xattrs_to(&self, _to: &Path) -> io::Result<()> {
        match *self {}
    }
}

#[cfg(unix)]
fn name_cstr(name: &OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))
}

/// The entry `name` of `path` (its last component) as seen from `parent`,
/// or `path` itself without a parent.
#[cfg(unix)]
fn last_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))
}

#[cfg(unix)]
impl DirFd {
    fn open_raw(dirfd: Option<&DirFd>, path: &std::ffi::CStr) -> io::Result<Option<DirFd>> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let at = dirfd.map_or(libc::AT_FDCWD, |d| d.0.as_raw_fd());
        // SAFETY: valid C string; the fd is owned on success.
        let fd = unsafe { libc::openat(at, path.as_ptr(), flags) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ELOOP) | Some(libc::ENOTDIR) => Ok(None),
                _ => Err(e),
            };
        }
        // SAFETY: a fresh descriptor we own.
        Ok(Some(DirFd(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Metadata of the opened folder itself.
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        let f = fs::File::from(self.0.try_clone()?);
        f.metadata()
    }

    /// Names in the folder, without `.` and `..`.
    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        use std::os::fd::IntoRawFd;
        use std::os::unix::ffi::OsStrExt;
        let dup = self.0.try_clone()?.into_raw_fd();
        // SAFETY: `dup` is ours; fdopendir takes it over (closedir closes it).
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: still ours when fdopendir failed.
            unsafe { libc::close(dup) };
            return Err(e);
        }
        // SAFETY: rewind so a reused description starts at the beginning.
        unsafe { libc::rewinddir(dir) };
        let mut out = Vec::new();
        let res = loop {
            // SAFETY: errno is reset to tell the end from an error.
            unsafe { *errno_ptr() = 0 };
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                let err = unsafe { *errno_ptr() };
                break if err == 0 {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(err))
                };
            }
            // SAFETY: d_name is NUL-terminated inside the entry.
            let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
            let b = name.to_bytes();
            if b != b"." && b != b".." {
                out.push(OsStr::from_bytes(b).to_os_string());
            }
        };
        // SAFETY: opened above.
        unsafe { libc::closedir(dir) };
        res.map(|()| out)
    }

    /// Copy this folder's extended attributes onto `to` (not followed).
    pub fn copy_xattrs_to(&self, to: &Path) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::fd::AsRawFd;
            use xattr_sys::{On, cpath};
            let t = cpath(to)?;
            copy_xattrs_on(On::Fd(self.0.as_raw_fd()), On::Path(&t))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = to;
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_ptr() -> *mut i32 {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
unsafe fn errno_ptr() -> *mut i32 {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))
))]
unsafe fn errno_ptr() -> *mut i32 {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno() }
}

/// Kind, device and inode of `path`, reached through `parent` when given
/// (then only the last component of `path` is used). Never follows a link.
pub fn src_kind(parent: Option<&DirFd>, path: &Path) -> io::Result<(SrcKind, u64, u64)> {
    #[cfg(unix)]
    {
        let st = match parent {
            Some(dir) => {
                use std::os::fd::AsRawFd;
                let name = name_cstr(last_name(path)?)?;
                let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: valid fd and name; `st` is written on success.
                let r = unsafe {
                    libc::fstatat(
                        dir.0.as_raw_fd(),
                        name.as_ptr(),
                        st.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if r != 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: initialised by the successful call.
                let st = unsafe { st.assume_init() };
                let kind = match st.st_mode & libc::S_IFMT {
                    libc::S_IFDIR => SrcKind::Dir,
                    libc::S_IFREG => SrcKind::File,
                    libc::S_IFLNK => SrcKind::Symlink,
                    _ => SrcKind::Other,
                };
                #[allow(clippy::unnecessary_cast)]
                return Ok((kind, st.st_dev as u64, st.st_ino as u64));
            }
            None => fs::symlink_metadata(path)?,
        };
        use std::os::unix::fs::MetadataExt;
        Ok((kind_of(&st.file_type()), st.dev(), st.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        let m = fs::symlink_metadata(path)?;
        Ok((kind_of(&m.file_type()), 0, 0))
    }
}

fn kind_of(ft: &fs::FileType) -> SrcKind {
    if ft.is_symlink() {
        SrcKind::Symlink
    } else if ft.is_dir() {
        SrcKind::Dir
    } else if ft.is_file() {
        SrcKind::File
    } else {
        SrcKind::Other
    }
}

/// Open a source folder without following links (`None`: no longer a real
/// folder). Off unix there is nothing to open: `None` always.
pub fn open_src_dir(parent: Option<&DirFd>, path: &Path) -> io::Result<Option<DirFd>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = match parent {
            Some(_) => name_cstr(last_name(path)?)?,
            None => std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?,
        };
        DirFd::open_raw(parent, &c)
    }
    #[cfg(not(unix))]
    {
        let _ = (parent, path);
        Ok(None)
    }
}

/// [`open_source`] reached through `parent` when given.
pub fn open_src_file(parent: Option<&DirFd>, path: &Path) -> io::Result<Option<fs::File>> {
    #[cfg(unix)]
    if let Some(dir) = parent {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let name = name_cstr(last_name(path)?)?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: valid fd and name; the result is owned on success.
        let fd = unsafe { libc::openat(dir.0.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ELOOP) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // SAFETY: a fresh descriptor we own.
        return Ok(Some(fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) })));
    }
    let _ = parent;
    open_source(path)
}

/// The target of the symlink `path`, reached through `parent` when given.
pub fn read_src_link(parent: Option<&DirFd>, path: &Path) -> io::Result<std::path::PathBuf> {
    #[cfg(unix)]
    if let Some(dir) = parent {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let name = name_cstr(last_name(path)?)?;
        let mut buf = vec![0u8; 4096];
        loop {
            // SAFETY: the buffer is valid for its length.
            let n = unsafe {
                libc::readlinkat(
                    dir.0.as_raw_fd(),
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let n = n as usize;
            if n < buf.len() {
                buf.truncate(n);
                return Ok(OsStr::from_bytes(&buf).into());
            }
            buf.resize(buf.len() * 2, 0);
        }
    }
    let _ = parent;
    fs::read_link(path)
}

#[cfg(all(test, unix))]
mod src_at_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn a_source_folder_is_read_through_its_handle() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        fs::create_dir(&d).unwrap();
        // APFS refuses names that are not UTF-8; Linux takes any bytes
        let odd = OsStr::from_bytes(if cfg!(target_os = "linux") {
            b"x\n\xff y"
        } else {
            "x\n 한 y".as_bytes()
        });
        fs::write(d.join(odd), "data").unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        std::os::unix::fs::symlink("../elsewhere", d.join("ln")).unwrap();
        let dir = open_src_dir(None, &d).unwrap().expect("a real folder");
        let mut names = dir.entries().unwrap();
        names.sort();
        let mut want = vec![
            OsString::from("ln"),
            OsString::from("sub"),
            odd.to_os_string(),
        ];
        want.sort();
        assert_eq!(names, want);
        assert_eq!(dir.entries().unwrap().len(), 3, "readable again");
        let kind = |n: &OsStr| src_kind(Some(&dir), &d.join(n)).unwrap().0;
        assert_eq!(kind(odd), SrcKind::File);
        assert_eq!(kind(OsStr::new("sub")), SrcKind::Dir);
        assert_eq!(kind(OsStr::new("ln")), SrcKind::Symlink);
        assert_eq!(
            read_src_link(Some(&dir), &d.join("ln")).unwrap(),
            std::path::PathBuf::from("../elsewhere")
        );
        let mut f = open_src_file(Some(&dir), &d.join(odd)).unwrap().unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "data");
        // links are never opened as files or folders
        assert!(open_src_file(Some(&dir), &d.join("ln")).unwrap().is_none());
        assert!(open_src_dir(Some(&dir), &d.join("ln")).unwrap().is_none());
        assert!(open_src_dir(Some(&dir), &d.join("sub")).unwrap().is_some());
        // the handle keeps pointing at the folder after it was renamed away
        fs::rename(&d, t.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(t.path(), &d).unwrap();
        assert_eq!(dir.entries().unwrap().len(), 3);
        assert_eq!(kind(odd), SrcKind::File);
        assert!(open_src_dir(None, &d).unwrap().is_none(), "a link: refused");
    }
}

/// Roots of the drives set in a `GetLogicalDrives` bit mask (bit 0 = `A:`).
pub fn drive_roots_from_mask(mask: u32) -> Vec<std::path::PathBuf> {
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| std::path::PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
        .collect()
}

/// The drives present on this machine (`C:\\`, `D:\\`, ...); none off Windows.
pub fn drive_roots() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        // SAFETY: no arguments, no preconditions.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        drive_roots_from_mask(mask)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod drive_tests {
    use super::drive_roots_from_mask;
    use std::path::PathBuf;

    #[test]
    fn mask_bits_are_drive_letters() {
        assert_eq!(
            drive_roots_from_mask(0b1100),
            vec![PathBuf::from("C:\\"), PathBuf::from("D:\\")]
        );
        assert!(drive_roots_from_mask(0).is_empty());
        assert_eq!(drive_roots_from_mask(1 << 25), vec![PathBuf::from("Z:\\")]);
    }
}

/// Bytes of a *remote* (POSIX) path. Remote paths live in `PathBuf`s, and on
/// Windows `join` adds `\\`: it is turned back into `/` here. (A remote name
/// that itself holds a backslash cannot be reached from Windows; listings
/// there leave such names out.)
pub fn remote_bytes(p: &OsStr) -> Cow<'_, [u8]> {
    to_slashes(os_bytes(p), cfg!(windows))
}

fn to_slashes(b: Cow<'_, [u8]>, windows: bool) -> Cow<'_, [u8]> {
    if windows && b.contains(&b'\\') {
        Cow::Owned(
            b.iter()
                .map(|&c| if c == b'\\' { b'/' } else { c })
                .collect(),
        )
    } else {
        b
    }
}

#[cfg(test)]
mod remote_bytes_tests {
    use super::*;

    #[test]
    fn windows_joins_become_slashes_only_on_windows() {
        let b = |s: &str, w: bool| to_slashes(Cow::Borrowed(s.as_bytes()), w).into_owned();
        assert_eq!(b("/home/u\\sub\\x", true), b"/home/u/sub/x");
        assert_eq!(b("/home/u\\sub", false), b"/home/u\\sub");
        assert_eq!(b("/plain", true), b"/plain");
    }
}

/// Whether a remote `name` stays exactly one plain component when joined
/// onto a path here (on Windows `a\\b` splits and `C:x` is a drive prefix).
pub fn plain_remote_name(name: &OsStr) -> bool {
    let mut c = Path::new(name).components();
    matches!(
        (c.next(), c.next()),
        (Some(std::path::Component::Normal(n)), None) if n == name
    )
}

#[cfg(test)]
mod plain_name_tests {
    use super::*;

    #[test]
    fn only_single_plain_components_pass() {
        for ok in ["a", "a b", "한글", ".hidden"] {
            assert!(plain_remote_name(OsStr::new(ok)), "{ok}");
        }
        for bad in ["a/b", "..", "."] {
            assert!(!plain_remote_name(OsStr::new(bad)), "{bad}");
        }
        if cfg!(windows) {
            for bad in ["a\\b", "C:foo", "C:"] {
                assert!(!plain_remote_name(OsStr::new(bad)), "{bad}");
            }
        }
    }
}

/// Windows: give `dst` the access rules (DACL) of `src`. A source whose
/// rules do not inherit from its folder (protected) keeps exactly those;
/// otherwise its own explicit rules are kept and the rest is inherited from
/// the new folder, as a fresh file there would. Elsewhere: nothing.
pub fn copy_dacl(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
        let (s, d) = (wide(src), wide(dst));
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated wide strings and out pointers; the
        // descriptor is freed with LocalFree below.
        let r = unsafe {
            GetNamedSecurityInfoW(
                s.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if r != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let mut control: u16 = 0;
        let mut rev: u32 = 0;
        // SAFETY: `sd` came from the call above.
        let protected = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) } != 0
            && control & SE_DACL_PROTECTED != 0;
        let info = DACL_SECURITY_INFORMATION
            | if protected {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
        // SAFETY: `dacl` points into `sd`, still alive.
        let r = unsafe {
            SetNamedSecurityInfoW(
                d.as_ptr(),
                SE_FILE_OBJECT,
                info,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: allocated by GetNamedSecurityInfoW.
        unsafe { LocalFree(sd as _) };
        if r != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (src, dst);
        Ok(())
    }
}
