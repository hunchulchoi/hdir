//! The real Recycle Bin on Windows: delete, find, restore, delete again,
//! purge. It touches the user's Recycle Bin (only the one uniquely named
//! file it made), so it is ignored by default; run on the Windows gate with
//! `cargo test --test windows_trash -- --ignored`.
#![cfg(windows)]

use hdir::trash::{TrashBackend, system_store};

#[test]
#[ignore = "uses the real Recycle Bin"]
fn delete_list_restore_and_purge_one_file() {
    let t = tempfile::tempdir().unwrap();
    let name = format!("hdir-it-{}-한글 x.txt", std::process::id());
    let file = t.path().join(&name);
    std::fs::write(&file, "hello").unwrap();
    let store = system_store();
    assert!(store.supports_view());
    let find = || store.list().unwrap().into_iter().find(|e| e.name() == name);
    let r = store.delete(std::slice::from_ref(&file));
    assert!(r[0].1.is_ok(), "{r:?}");
    assert!(!file.exists());
    let entry = find().expect("listed in the Recycle Bin");
    assert_eq!(entry.original_path().as_deref(), Some(file.as_path()));
    let res = store.restore(std::slice::from_ref(&entry));
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello",
        "back in place"
    );
    assert!(find().is_none());
    // and away for good
    store.delete(std::slice::from_ref(&file));
    let entry = find().expect("listed again");
    store.purge(std::slice::from_ref(&entry)).unwrap();
    assert!(find().is_none() && !file.exists());
}
