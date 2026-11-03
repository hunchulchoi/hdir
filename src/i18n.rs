//! Screen language. Every user-facing message comes in Korean and English;
//! [`tr!`](crate::tr) picks one by the current [`Lang`].
//!
//! The language is chosen once at startup ([`detect`]) and may be replaced by
//! the config file's `lang`. Tests can switch it per thread with [`with_lang`]
//! without touching other tests.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Ko,
    En,
}

static GLOBAL: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::Cell<Option<Lang>> = const { std::cell::Cell::new(None) };
}

/// The language messages are shown in.
pub fn lang() -> Lang {
    #[cfg(test)]
    if let Some(l) = OVERRIDE.with(|o| o.get()) {
        return l;
    }
    match GLOBAL.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Ko,
    }
}

/// Switch the language. In tests only this thread switches, so tests that
/// load a config with `lang` cannot change other tests' messages.
pub fn set_lang(l: Lang) {
    #[cfg(test)]
    {
        OVERRIDE.with(|o| o.set(Some(l)));
    }
    #[cfg(not(test))]
    GLOBAL.store(
        match l {
            Lang::Ko => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// Run `f` with this thread's messages in `l`.
#[cfg(test)]
pub fn with_lang<T>(l: Lang, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Lang>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(l))));
    f()
}

/// Pick the Korean or English text.
pub fn t(ko: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Ko => ko,
        Lang::En => en,
    }
}

/// Config value: `ko`, `en` or `auto` (case-insensitive). `auto` is `None`.
pub fn parse_setting(v: &str) -> Result<Option<Lang>, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "ko" | "korean" => Ok(Some(Lang::Ko)),
        "en" | "english" => Ok(Some(Lang::En)),
        "" | "auto" => Ok(None),
        _ => Err(format!(
            "{}: {v}",
            t(
                "lang 값은 ko, en, auto 중 하나여야 함",
                "lang must be ko, en or auto"
            )
        )),
    }
}

/// A locale name like `ko_KR.UTF-8` or `en-US`: Korean only for `ko*`.
pub fn from_locale(name: &str) -> Lang {
    if name.trim().to_ascii_lowercase().starts_with("ko") {
        Lang::Ko
    } else {
        Lang::En
    }
}

/// `HDIR_LANG`, then the first set one of `LC_ALL`, `LC_MESSAGES`, `LANG`,
/// then the OS user language (`os`, used on Windows where those are unset).
/// Nothing at all: English.
pub fn detect(env: impl Fn(&str) -> Option<String>, os: Option<Lang>) -> Lang {
    let set = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    if let Some(Ok(Some(l))) = set("HDIR_LANG").map(|v| parse_setting(&v)) {
        return l;
    }
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(set)
        .map(|v| from_locale(&v))
        .or(os)
        .unwrap_or(Lang::En)
}

/// The Windows user interface language, if it can be read.
#[cfg(windows)]
pub fn os_lang() -> Option<Lang> {
    // LANG_KOREAN is primary language 0x12.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    Some(if id & 0x3ff == 0x12 {
        Lang::Ko
    } else {
        Lang::En
    })
}

#[cfg(not(windows))]
pub fn os_lang() -> Option<Lang> {
    None
}

/// Format the Korean or English text by the current language.
/// `tr!("{n}개 삭제", "Delete {n} items")`; extra arguments go to both.
#[macro_export]
macro_rules! tr {
    ($ko:literal, $en:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Ko => format!($ko),
            $crate::i18n::Lang::En => format!($en),
        }
    };
    ($ko:literal, $en:literal, $($arg:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Ko => format!($ko, $($arg)+),
            $crate::i18n::Lang::En => format!($en, $($arg)+),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn locale_names_pick_korean_only_for_ko() {
        assert_eq!(from_locale("ko_KR.UTF-8"), Lang::Ko);
        assert_eq!(from_locale("ko"), Lang::Ko);
        assert_eq!(from_locale("en_US.UTF-8"), Lang::En);
        assert_eq!(from_locale("C"), Lang::En);
        assert_eq!(from_locale("ja_JP"), Lang::En);
    }

    #[test]
    fn detect_follows_the_locale_order() {
        assert_eq!(detect(env(&[("LANG", "ko_KR.UTF-8")]), None), Lang::Ko);
        assert_eq!(
            detect(env(&[("LANG", "ko_KR.UTF-8"), ("LC_ALL", "en_US")]), None),
            Lang::En
        );
        assert_eq!(
            detect(env(&[("LANG", "en_US"), ("LC_MESSAGES", "ko_KR")]), None),
            Lang::Ko
        );
        assert_eq!(
            detect(env(&[("LC_ALL", ""), ("LANG", "ko_KR")]), None),
            Lang::Ko
        );
    }

    #[test]
    fn hdir_lang_wins_over_the_locale() {
        assert_eq!(
            detect(env(&[("HDIR_LANG", "ko"), ("LANG", "en_US")]), None),
            Lang::Ko
        );
        assert_eq!(
            detect(env(&[("HDIR_LANG", "auto"), ("LANG", "ko_KR")]), None),
            Lang::Ko
        );
        // A bad value is ignored rather than breaking startup.
        assert_eq!(
            detect(env(&[("HDIR_LANG", "fr"), ("LANG", "ko_KR")]), None),
            Lang::Ko
        );
    }

    #[test]
    fn without_any_locale_the_os_language_then_english_is_used() {
        assert_eq!(detect(env(&[]), Some(Lang::Ko)), Lang::Ko);
        assert_eq!(detect(env(&[]), None), Lang::En);
        assert_eq!(detect(env(&[("LANG", "en_US")]), Some(Lang::Ko)), Lang::En);
    }

    #[test]
    fn setting_values() {
        assert_eq!(parse_setting("ko"), Ok(Some(Lang::Ko)));
        assert_eq!(parse_setting("EN"), Ok(Some(Lang::En)));
        assert_eq!(parse_setting("auto"), Ok(None));
        assert!(parse_setting("fr").is_err());
    }

    #[test]
    fn tr_formats_the_text_for_the_thread_language() {
        let n = 3;
        assert_eq!(
            with_lang(Lang::Ko, || crate::tr!("{n}개 삭제", "Delete {n} items")),
            "3개 삭제"
        );
        assert_eq!(
            with_lang(Lang::En, || crate::tr!("{n}개 삭제", "Delete {n} items")),
            "Delete 3 items"
        );
        assert_eq!(
            with_lang(Lang::En, || crate::tr!("{}개", "{} items", n + 1)),
            "4 items"
        );
        assert_eq!(with_lang(Lang::En, || t("닫기", "Close")), "Close");
    }

    #[test]
    fn tests_default_to_korean() {
        assert_eq!(lang(), Lang::Ko);
    }
}
