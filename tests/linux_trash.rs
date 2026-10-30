//! Real freedesktop-trash roundtrips through the `trash` crate.
//!
//! The crate honours `XDG_DATA_HOME`, so every case points it at a private
//! tempdir (which also holds the files, hence the same filesystem). Cases run
//! from a single `#[test]` so the process-wide env var cannot race.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use hdir::trash::{SYMLINK_MSG, SystemTrash, TrashBackend, TrashEntry};

/// Fresh sandbox: `XDG_DATA_HOME` -> `<tmp>/xdg`, work files in `<tmp>/work`.
fn sandbox() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("xdg");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    // SAFETY: only one test function exists in this binary, so no other
    // thread reads or writes the environment concurrently.
    unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };
    (tmp, work)
}

fn find(store: &SystemTrash, path: &Path) -> Vec<TrashEntry> {
    store
        .list()
        .unwrap()
        .into_iter()
        .filter(|e| e.original_path().as_deref() == Some(path))
        .collect()
}

fn roundtrip_delete_list_restore_then_purge() {
    let (_tmp, work) = sandbox();
    let path = work.join("plain.txt");
    std::fs::write(&path, "data").unwrap();
    let store = SystemTrash;
    assert!(store.delete(std::slice::from_ref(&path))[0].1.is_ok());
    assert!(!path.exists());
    let found = find(&store, &path);
    assert_eq!(found.len(), 1);
    let r = store.restore(&found);
    assert_eq!(r.restored, std::slice::from_ref(&path), "{r:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "data");
    assert!(store.delete(std::slice::from_ref(&path))[0].1.is_ok());
    let found = find(&store, &path);
    assert_eq!(found.len(), 1);
    store.purge(&found).unwrap();
    assert!(find(&store, &path).is_empty());
    assert!(!path.exists());
}

fn non_utf8_name_roundtrip() {
    let (_tmp, work) = sandbox();
    let name = OsString::from_vec(vec![b'n', 0xff, b'.', b't']);
    let path = work.join(&name);
    std::fs::write(&path, "raw").unwrap();
    let store = SystemTrash;
    assert!(store.delete(std::slice::from_ref(&path))[0].1.is_ok());
    let found = find(&store, &path);
    assert_eq!(found.len(), 1, "listed with its raw name");
    let r = store.restore(&found);
    assert_eq!(r.restored, std::slice::from_ref(&path), "{r:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "raw");
}

fn non_utf8_symlink_to_dir_is_still_guarded() {
    let (_tmp, work) = sandbox();
    let target = work.join("target-dir");
    std::fs::create_dir(&target).unwrap();
    let link = work.join(OsString::from_vec(vec![b'l', 0xfe]));
    symlink(&target, &link).unwrap();
    let store = SystemTrash;
    assert!(store.delete(std::slice::from_ref(&link))[0].1.is_ok());
    let found = find(&store, &link);
    assert_eq!(found.len(), 1);
    let r = store.restore(&found);
    assert!(r.restored.is_empty(), "{r:?}");
    assert!(r.errors[0].contains(SYMLINK_MSG), "{r:?}");
    assert!(link.symlink_metadata().is_err(), "no stray dir");
    assert_eq!(find(&store, &link).len(), 1, "stays in the trash");
}

/// A symlink to a directory is refused by hdir's guard (the crate itself fails
/// with IsADirectory on it); nothing, in particular no empty dir, appears.
fn symlink_to_dir_is_refused_and_left_in_the_trash() {
    let (_tmp, work) = sandbox();
    let target = work.join("target-dir");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("inner"), "x").unwrap();
    let link = work.join("link");
    symlink(&target, &link).unwrap();
    let store = SystemTrash;
    assert!(store.delete(std::slice::from_ref(&link))[0].1.is_ok());
    assert!(link.symlink_metadata().is_err());
    let found = find(&store, &link);
    assert_eq!(found.len(), 1);
    assert!(!found[0].is_dir, "listed as a symlink, not a directory");
    let r = store.restore(&found);
    assert!(r.restored.is_empty(), "{r:?}");
    assert!(r.errors[0].contains(SYMLINK_MSG), "{r:?}");
    assert!(link.symlink_metadata().is_err(), "no stray empty dir");
    assert_eq!(find(&store, &link).len(), 1);
    assert!(target.join("inner").exists());
}

#[test]
fn real_linux_trash_cases() {
    roundtrip_delete_list_restore_then_purge();
    non_utf8_name_roundtrip();
    non_utf8_symlink_to_dir_is_still_guarded();
    symlink_to_dir_is_refused_and_left_in_the_trash();
}
