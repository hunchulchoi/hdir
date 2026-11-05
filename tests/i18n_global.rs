//! The real (process-wide) language setting: unit tests only see the
//! per-thread override, so this checks that `set_lang` reaches every
//! thread, workers included. A test binary of its own: nothing else here
//! depends on the language.

use hdir::i18n::{Lang, lang, set_lang, t};

#[test]
fn set_lang_reaches_new_threads_and_workers() {
    set_lang(Lang::En);
    assert_eq!(lang(), Lang::En);
    let on_thread = std::thread::spawn(|| (lang(), t("한국어", "English")))
        .join()
        .unwrap();
    assert_eq!(on_thread, (Lang::En, "English"));
    // a message built on a worker, as loaders and jobs do
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (_, warnings) = hdir::places::parse_favorites("paths = [1, 2]\n");
        tx.send(warnings).unwrap();
    });
    let warnings = rx.recv().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with("favorites.toml could not be read"),
        "{warnings:?}"
    );
    // and back
    set_lang(Lang::Ko);
    let ko = std::thread::spawn(|| t("한국어", "English"))
        .join()
        .unwrap();
    assert_eq!(ko, "한국어");
}
