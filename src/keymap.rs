//! Configurable keymap: key strings, actions, presets, user overrides.
//!
//! Key -> [`Action`] resolution is a pure table lookup per [`Context`]; nothing
//! else in the app should hard-code keys.
//!
//! # Key string syntax
//! `Ctrl+` / `Alt+` / `Shift+` modifiers followed by `F1`..`F24`, `Enter`,
//! `Esc`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`,
//! `PageDown`, `Up`/`Down`/`Left`/`Right`, `Space`, or a single character.
//! Modifier and key names are case-insensitive.
//!
//! # Character normalization
//! Single characters are case-sensitive: `"Q"` is the character `Q`, `"q"` is
//! `q`. Because terminals disagree on whether an uppercase letter carries
//! `SHIFT`, SHIFT is ignored for character keys on both sides of a comparison:
//! `Shift+q` is parsed as `Q`, and an event `Char('q') + SHIFT` is treated as
//! `Q`. With `Ctrl` or `Alt` the letter case is ignored (`Ctrl+Q` matches
//! `Ctrl+q` and `Ctrl+Shift+q`). For non-character keys (`F6`, `Enter`, ...)
//! modifiers must match exactly.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    CursorUp,
    CursorDown,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Parent,
    SwitchPanel,
    ToggleSelect,
    ToggleHidden,
    View,
    Edit,
    Copy,
    Move,
    Mkdir,
    Rename,
    Delete,
    TrashView,
    Menu,
    Help,
    Quit,
    QuitNoCd,
    Undo,
    CommandLine,
    ToggleMode,
    ConnectLeft,
    ConnectRight,
    OpenWith,
    CancelJob,
    /// Dialog: accept.
    Confirm,
    /// Dialog/menu: dismiss.
    Cancel,
}

impl Action {
    /// Parse a snake_case action name (as used in config files).
    pub fn from_name(s: &str) -> Option<Action> {
        use serde::de::IntoDeserializer;
        let d: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
            s.into_deserializer();
        Action::deserialize(d).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    Panel,
    Viewer,
    Dialog,
    Menu,
    Trash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

fn map_char(c: char, upper: bool) -> char {
    let mut it: Box<dyn Iterator<Item = char>> = if upper {
        Box::new(c.to_uppercase())
    } else {
        Box::new(c.to_lowercase())
    };
    match (it.next(), it.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// See the module docs ("Character normalization").
fn normalize(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(c) = code else {
        return (code, mods);
    };
    let shift = mods.contains(KeyModifiers::SHIFT);
    let mods = mods - KeyModifiers::SHIFT;
    let c = if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        map_char(c, false)
    } else if shift {
        map_char(c, true)
    } else {
        c
    };
    (KeyCode::Char(c), mods)
}

fn parse_key_name(s: &str) -> Result<KeyCode, String> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(c));
    }
    let lower = s.to_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        other => {
            let n = other
                .strip_prefix('f')
                .and_then(|d| d.parse::<u8>().ok())
                .filter(|n| (1..=24).contains(n));
            match n {
                Some(n) => KeyCode::F(n),
                None => return Err(format!("unknown key '{s}'")),
            }
        }
    };
    Ok(code)
}

impl FromStr for KeySpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mut rest = s.trim();
        let mut mods = KeyModifiers::NONE;
        while let Some((head, tail)) = rest.split_once('+')
            && !tail.is_empty()
        {
            mods |= match head.to_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return Err(format!("unknown modifier '{head}' in '{s}'")),
            };
            rest = tail;
        }
        if rest.is_empty() {
            return Err("empty key".into());
        }
        let code = parse_key_name(rest)?;
        let (code, mods) = normalize(code, mods);
        Ok(KeySpec { code, mods })
    }
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(KeyModifiers::CONTROL) {
            f.write_str("Ctrl+")?;
        }
        if self.mods.contains(KeyModifiers::ALT) {
            f.write_str("Alt+")?;
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            f.write_str("Shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => {
                let c = if self
                    .mods
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    map_char(c, true)
                } else {
                    c
                };
                write!(f, "{c}")
            }
            KeyCode::F(n) => write!(f, "F{n}"),
            KeyCode::Enter => f.write_str("Enter"),
            KeyCode::Esc => f.write_str("Esc"),
            KeyCode::Tab => f.write_str("Tab"),
            KeyCode::Backspace => f.write_str("Backspace"),
            KeyCode::Delete => f.write_str("Delete"),
            KeyCode::Insert => f.write_str("Insert"),
            KeyCode::Home => f.write_str("Home"),
            KeyCode::End => f.write_str("End"),
            KeyCode::PageUp => f.write_str("PageUp"),
            KeyCode::PageDown => f.write_str("PageDown"),
            KeyCode::Up => f.write_str("Up"),
            KeyCode::Down => f.write_str("Down"),
            KeyCode::Left => f.write_str("Left"),
            KeyCode::Right => f.write_str("Right"),
            other => write!(f, "{other:?}"),
        }
    }
}

impl KeySpec {
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        normalize(self.code, self.mods) == normalize(ev.code, ev.modifiers)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    maps: HashMap<Context, Vec<(KeySpec, Action)>>,
}

use Action as A;

const MDIR_PANEL: &[(Action, &[&str])] = &[
    (A::CursorUp, &["Up"]),
    (A::CursorDown, &["Down"]),
    (A::PageUp, &["PageUp"]),
    (A::PageDown, &["PageDown"]),
    (A::Home, &["Home"]),
    (A::End, &["End"]),
    (A::Enter, &["Enter"]),
    (A::Parent, &["Backspace"]),
    (A::SwitchPanel, &["Tab"]),
    (A::ToggleSelect, &["Space", "Insert"]),
    (A::ToggleHidden, &["."]),
    (A::View, &["F3", "Alt+3"]),
    (A::Edit, &["F4", "Alt+4"]),
    (A::Copy, &["F5", "Alt+5"]),
    (A::Move, &["F6", "Alt+6"]),
    (A::Mkdir, &["F7", "Alt+7"]),
    (A::Rename, &["F2", "Shift+F6", "Alt+2"]),
    (A::Delete, &["F8", "Alt+8"]),
    (A::TrashView, &["Alt+F8"]),
    (A::Menu, &["F9", "Alt+9"]),
    (A::Help, &["F1", "Alt+1"]),
    (A::Quit, &["F10", "q", "Alt+0"]),
    (A::QuitNoCd, &["Shift+F10"]),
    (A::Undo, &["Ctrl+Z"]),
    (A::CommandLine, &[":"]),
    (A::ToggleMode, &["Ctrl+T"]),
    (A::ConnectLeft, &["Alt+F1"]),
    (A::ConnectRight, &["Alt+F2"]),
    (A::OpenWith, &["Ctrl+O"]),
    (A::CancelJob, &["Esc"]),
];

const MDIR_VIEWER: &[(Action, &[&str])] = &[
    (A::CursorUp, &["Up"]),
    (A::CursorDown, &["Down"]),
    (A::PageUp, &["PageUp"]),
    (A::PageDown, &["PageDown", "Space"]),
    (A::Home, &["Home"]),
    (A::End, &["End"]),
    (A::Edit, &["F4"]),
    (A::Help, &["F1"]),
    (A::Quit, &["Esc", "q", "F3", "F10"]),
];

const MDIR_DIALOG: &[(Action, &[&str])] = &[(A::Confirm, &["Enter"]), (A::Cancel, &["Esc"])];

const MDIR_MENU: &[(Action, &[&str])] = &[
    (A::CursorUp, &["Up"]),
    (A::CursorDown, &["Down"]),
    (A::Enter, &["Enter"]),
    (A::Cancel, &["Esc", "F9"]),
];

const MDIR_TRASH: &[(Action, &[&str])] = &[
    (A::CursorUp, &["Up"]),
    (A::CursorDown, &["Down"]),
    (A::PageUp, &["PageUp"]),
    (A::PageDown, &["PageDown"]),
    (A::Home, &["Home"]),
    (A::End, &["End"]),
    (A::Enter, &["Enter"]),
    (A::Copy, &["F5"]),
    (A::Delete, &["F8"]),
    (A::Parent, &["Backspace"]),
    (A::Cancel, &["Esc"]),
    (A::SwitchPanel, &["Tab"]),
    (A::ToggleSelect, &["Space", "Insert"]),
    (A::TrashView, &["Alt+F8"]),
    (A::Quit, &["F10", "q"]),
];

/// Extra single-key bindings layered on top of mdir for the vim preset.
const VIM_PANEL: &[(Action, &[&str])] = &[
    (A::CursorDown, &["j"]),
    (A::CursorUp, &["k"]),
    (A::Parent, &["h"]),
    (A::Enter, &["l"]),
    (A::Home, &["g"]),
    (A::End, &["G"]),
    (A::PageDown, &["Ctrl+D"]),
    (A::PageUp, &["Ctrl+U"]),
    (A::View, &["v"]),
    (A::Edit, &["e"]),
    (A::Copy, &["y"]),
    (A::Move, &["m"]),
    (A::Mkdir, &["a"]),
    (A::Delete, &["d"]),
    (A::Rename, &["r"]),
    (A::TrashView, &["T"]),
    (A::Undo, &["u"]),
    (A::Help, &["?"]),
    (A::Menu, &["Ctrl+P"]),
    (A::CommandLine, &[":"]),
    (A::Quit, &["q"]),
];

const VIM_LIST: &[(Action, &[&str])] = &[
    (A::CursorDown, &["j"]),
    (A::CursorUp, &["k"]),
    (A::Home, &["g"]),
    (A::End, &["G"]),
];

impl Keymap {
    /// Insert `key -> action` in `ctx`, replacing any existing binding of `key`.
    fn insert(&mut self, ctx: Context, key: KeySpec, action: Action) {
        let list = self.maps.entry(ctx).or_default();
        list.retain(|(k, _)| *k != key);
        list.push((key, action));
    }

    fn add_table(&mut self, ctx: Context, table: &[(Action, &[&str])]) {
        for (action, keys) in table {
            for k in *keys {
                let spec = k.parse().expect("built-in key string");
                self.insert(ctx, spec, *action);
            }
        }
    }

    /// Built-in preset by name (case-insensitive): `mdir` or `vim`.
    pub fn preset(name: &str) -> Option<Keymap> {
        let mut km = Keymap::default();
        km.add_table(Context::Panel, MDIR_PANEL);
        km.add_table(Context::Viewer, MDIR_VIEWER);
        km.add_table(Context::Dialog, MDIR_DIALOG);
        km.add_table(Context::Menu, MDIR_MENU);
        km.add_table(Context::Trash, MDIR_TRASH);
        match name.to_lowercase().as_str() {
            "mdir" => Some(km),
            "vim" => {
                km.add_table(Context::Panel, VIM_PANEL);
                km.add_table(Context::Viewer, VIM_LIST);
                km.add_table(Context::Menu, VIM_LIST);
                km.add_table(Context::Trash, VIM_LIST);
                Some(km)
            }
            _ => None,
        }
    }

    pub fn lookup(&self, ctx: Context, ev: &KeyEvent) -> Option<Action> {
        self.maps
            .get(&ctx)?
            .iter()
            .find(|(k, _)| k.matches(ev))
            .map(|(_, a)| *a)
    }

    /// All keys bound to `action` in `ctx`, in binding order.
    pub fn keys_for(&self, ctx: Context, action: Action) -> Vec<KeySpec> {
        self.maps
            .get(&ctx)
            .map(|l| {
                l.iter()
                    .filter(|(_, a)| *a == action)
                    .map(|(k, _)| *k)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `("F5, Alt+5", Copy)` rows for the help screen, one per bound action.
    pub fn describe(&self, ctx: Context) -> Vec<(String, Action)> {
        let mut order: Vec<Action> = Vec::new();
        if let Some(list) = self.maps.get(&ctx) {
            for (_, a) in list {
                if !order.contains(a) {
                    order.push(*a);
                }
            }
        }
        order
            .into_iter()
            .map(|a| {
                let keys: Vec<String> = self
                    .keys_for(ctx, a)
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                (keys.join(", "), a)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

/// The `[keys]` table of `config.toml`. `[keys.bind]` is the panel context.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    pub preset: Option<String>,
    pub bind: HashMap<String, StringOrVec>,
    pub viewer: HashMap<String, StringOrVec>,
    pub dialog: HashMap<String, StringOrVec>,
    pub menu: HashMap<String, StringOrVec>,
    pub trash: HashMap<String, StringOrVec>,
}

/// Dialog keys that can never be removed or rebound (prevents lock-out).
const DIALOG_LOCKED: &[(&str, Action)] = &[("Esc", Action::Cancel), ("Enter", Action::Confirm)];

fn locked_action(key: &KeySpec) -> Option<Action> {
    DIALOG_LOCKED
        .iter()
        .find(|(k, _)| k.parse::<KeySpec>().ok().as_ref() == Some(key))
        .map(|(_, a)| *a)
}

fn apply_section(
    km: &mut Keymap,
    ctx: Context,
    section_name: &str,
    section: &HashMap<String, StringOrVec>,
    warns: &mut Vec<String>,
) {
    let mut names: Vec<&String> = section.keys().collect();
    names.sort();
    for name in names {
        let Some(action) = Action::from_name(name) else {
            warns.push(format!("[{section_name}] unknown action '{name}' ignored"));
            continue;
        };
        let raw: Vec<&str> = match &section[name] {
            StringOrVec::One(s) => vec![s.as_str()],
            StringOrVec::Many(v) => v.iter().map(String::as_str).collect(),
        };
        let mut specs: Vec<KeySpec> = Vec::new();
        let mut bad = 0;
        for r in raw {
            match r.parse::<KeySpec>() {
                Ok(k) => {
                    if !specs.contains(&k) {
                        specs.push(k);
                    }
                }
                Err(e) => {
                    bad += 1;
                    warns.push(format!("[{section_name}] {name}: invalid key '{r}' ({e})"));
                }
            }
        }
        if specs.is_empty() && bad > 0 {
            continue; // nothing usable: keep the preset binding
        }
        let list = km.maps.entry(ctx).or_default();
        list.retain(|(_, a)| *a != action);
        for k in specs {
            if ctx == Context::Dialog
                && let Some(locked) = locked_action(&k)
                && locked != action
            {
                warns.push(format!(
                    "[{section_name}] {name}: key '{k}' is locked in dialogs and cannot be rebound"
                ));
                continue;
            }
            if let Some(pos) = list.iter().position(|(o, _)| *o == k) {
                warns.push(format!(
                    "[{section_name}] key '{k}' bound to both {:?} and {action:?}; using {action:?}",
                    list[pos].1
                ));
                list.remove(pos);
            }
            list.push((k, action));
        }
    }
    if ctx == Context::Dialog {
        let list = km.maps.entry(ctx).or_default();
        for (key, action) in DIALOG_LOCKED {
            let spec: KeySpec = key.parse().expect("built-in key string");
            if !list.contains(&(spec, *action)) {
                warns.push(format!(
                    "[{section_name}] key '{spec}' is locked to {action:?} in dialogs; restored"
                ));
                list.retain(|(k, _)| *k != spec);
                list.push((spec, *action));
            }
        }
    }
}

/// Build the effective keymap from config. Problems never abort: they yield
/// warnings (for the status line) and the offending entry is ignored.
pub fn build(cfg: &KeysConfig) -> (Keymap, Vec<String>) {
    let mut warns = Vec::new();
    let mut km = match cfg.preset.as_deref() {
        None => Keymap::preset("mdir"),
        Some(name) => Keymap::preset(name).or_else(|| {
            warns.push(format!("[keys] unknown preset '{name}'; using 'mdir'"));
            Keymap::preset("mdir")
        }),
    }
    .expect("mdir preset exists");
    let sections = [
        (Context::Panel, "keys.bind", &cfg.bind),
        (Context::Viewer, "keys.viewer", &cfg.viewer),
        (Context::Dialog, "keys.dialog", &cfg.dialog),
        (Context::Menu, "keys.menu", &cfg.menu),
        (Context::Trash, "keys.trash", &cfg.trash),
    ];
    for (ctx, name, section) in sections {
        apply_section(&mut km, ctx, name, section, &mut warns);
    }
    (km, warns)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn ks(s: &str) -> KeySpec {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }
    fn cfg(toml_src: &str) -> KeysConfig {
        #[derive(Deserialize)]
        struct Root {
            keys: KeysConfig,
        }
        toml::from_str::<Root>(toml_src).unwrap().keys
    }
    fn mdir() -> Keymap {
        Keymap::preset("mdir").unwrap()
    }
    fn look(km: &Keymap, ctx: Context, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        km.lookup(ctx, &ev(code, mods))
    }

    #[test]
    fn parse_named_keys() {
        let cases = [
            ("Enter", KeyCode::Enter),
            ("Esc", KeyCode::Esc),
            ("Tab", KeyCode::Tab),
            ("Backspace", KeyCode::Backspace),
            ("Delete", KeyCode::Delete),
            ("Insert", KeyCode::Insert),
            ("Home", KeyCode::Home),
            ("End", KeyCode::End),
            ("PageUp", KeyCode::PageUp),
            ("PageDown", KeyCode::PageDown),
            ("Up", KeyCode::Up),
            ("Down", KeyCode::Down),
            ("Left", KeyCode::Left),
            ("Right", KeyCode::Right),
            ("Space", KeyCode::Char(' ')),
            ("F1", KeyCode::F(1)),
            ("F10", KeyCode::F(10)),
            ("F12", KeyCode::F(12)),
        ];
        for (s, code) in cases {
            assert_eq!(ks(s), KeySpec { code, mods: NONE }, "{s}");
        }
    }

    #[test]
    fn parse_modifiers() {
        assert_eq!(
            ks("Ctrl+Q"),
            KeySpec {
                code: KeyCode::Char('q'),
                mods: CTRL
            }
        );
        assert_eq!(
            ks("alt+5"),
            KeySpec {
                code: KeyCode::Char('5'),
                mods: ALT
            }
        );
        assert_eq!(
            ks("Shift+F6"),
            KeySpec {
                code: KeyCode::F(6),
                mods: SHIFT
            }
        );
        assert_eq!(
            ks("Ctrl+Alt+Delete"),
            KeySpec {
                code: KeyCode::Delete,
                mods: CTRL | ALT
            }
        );
        assert_eq!(
            ks("Alt+F8"),
            KeySpec {
                code: KeyCode::F(8),
                mods: ALT
            }
        );
    }

    #[test]
    fn parse_is_case_insensitive_for_names_and_modifiers() {
        assert_eq!(ks("CTRL+q"), ks("ctrl+Q"));
        assert_eq!(ks("pageup"), ks("PageUp"));
        assert_eq!(ks("ESC"), ks("esc"));
        assert_eq!(ks("f5"), ks("F5"));
        assert_eq!(ks("SHIFT+f6"), ks("shift+F6"));
    }

    #[test]
    fn parse_single_chars_are_case_sensitive() {
        assert_eq!(ks("q").code, KeyCode::Char('q'));
        assert_eq!(ks("Q").code, KeyCode::Char('Q'));
        assert_ne!(ks("q"), ks("Q"));
        assert_eq!(ks(".").code, KeyCode::Char('.'));
        assert_eq!(ks(":").code, KeyCode::Char(':'));
        assert_eq!(ks("+").code, KeyCode::Char('+'));
        assert_eq!(
            ks("Ctrl++"),
            KeySpec {
                code: KeyCode::Char('+'),
                mods: CTRL
            }
        );
        assert_eq!(ks("한").code, KeyCode::Char('한'));
    }

    #[test]
    fn shift_letter_is_uppercase_char() {
        assert_eq!(ks("Shift+q"), ks("Q"));
        assert_eq!(ks("Shift+q").mods, NONE);
    }

    #[test]
    fn parse_rejects_bad_strings() {
        for s in [
            "", " ", "Ctrl+", "Foo", "F0", "F25", "Ctrl+Alt", "ab", "Ctrl+Foo", "Hyper+q",
        ] {
            assert!(s.parse::<KeySpec>().is_err(), "{s:?} should fail");
        }
    }

    #[test]
    fn display_roundtrip() {
        for s in [
            "F5",
            "Shift+F6",
            "Alt+F8",
            "Ctrl+Q",
            "Alt+5",
            "Ctrl+Alt+Delete",
            "Enter",
            "Esc",
            "Tab",
            "Backspace",
            "Insert",
            "PageUp",
            "PageDown",
            "Up",
            "Space",
            "q",
            "Q",
            ".",
            ":",
            "Ctrl+Z",
            "Home",
            "End",
            "Left",
            "Right",
        ] {
            let k = ks(s);
            assert_eq!(k.to_string(), s, "display of {s}");
            assert_eq!(ks(&k.to_string()), k);
        }
        assert_eq!(ks("ctrl+q").to_string(), "Ctrl+Q");
    }

    #[test]
    fn matches_ignores_shift_for_chars() {
        assert!(ks("Q").matches(&ev(KeyCode::Char('Q'), SHIFT)));
        assert!(ks("Q").matches(&ev(KeyCode::Char('Q'), NONE)));
        assert!(ks("Q").matches(&ev(KeyCode::Char('q'), SHIFT)));
        assert!(ks(":").matches(&ev(KeyCode::Char(':'), SHIFT)));
        assert!(!ks("q").matches(&ev(KeyCode::Char('Q'), SHIFT)));
        assert!(!ks("Q").matches(&ev(KeyCode::Char('q'), NONE)));
    }

    #[test]
    fn matches_ctrl_alt_letters_case_insensitively() {
        assert!(ks("Ctrl+Q").matches(&ev(KeyCode::Char('q'), CTRL)));
        assert!(ks("Ctrl+Q").matches(&ev(KeyCode::Char('Q'), CTRL | SHIFT)));
        assert!(ks("Alt+x").matches(&ev(KeyCode::Char('X'), ALT | SHIFT)));
        assert!(!ks("Ctrl+Q").matches(&ev(KeyCode::Char('q'), NONE)));
        assert!(!ks("q").matches(&ev(KeyCode::Char('q'), CTRL)));
    }

    #[test]
    fn matches_keeps_shift_for_non_chars() {
        assert!(ks("Shift+F6").matches(&ev(KeyCode::F(6), SHIFT)));
        assert!(!ks("F6").matches(&ev(KeyCode::F(6), SHIFT)));
        assert!(!ks("Shift+F6").matches(&ev(KeyCode::F(6), NONE)));
        assert!(!ks("F5").matches(&ev(KeyCode::F(5), ALT)));
    }

    #[test]
    fn mdir_preset_has_fkeys() {
        let km = mdir();
        let p = |c, m| look(&km, Context::Panel, c, m);
        assert_eq!(p(KeyCode::F(1), NONE), Some(Action::Help));
        assert_eq!(p(KeyCode::F(2), NONE), Some(Action::Rename));
        assert_eq!(p(KeyCode::F(6), SHIFT), Some(Action::Rename));
        assert_eq!(p(KeyCode::F(3), NONE), Some(Action::View));
        assert_eq!(p(KeyCode::F(4), NONE), Some(Action::Edit));
        assert_eq!(p(KeyCode::F(5), NONE), Some(Action::Copy));
        assert_eq!(p(KeyCode::F(6), NONE), Some(Action::Move));
        assert_eq!(p(KeyCode::F(7), NONE), Some(Action::Mkdir));
        assert_eq!(p(KeyCode::F(8), NONE), Some(Action::Delete));
        assert_eq!(p(KeyCode::F(8), ALT), Some(Action::TrashView));
        assert_eq!(p(KeyCode::F(9), NONE), Some(Action::Menu));
        assert_eq!(p(KeyCode::F(10), NONE), Some(Action::Quit));
        assert_eq!(p(KeyCode::Char('q'), NONE), Some(Action::Quit));
        assert_eq!(p(KeyCode::Char('z'), CTRL), Some(Action::Undo));
        assert_eq!(p(KeyCode::Esc, NONE), Some(Action::CancelJob));
        assert_eq!(p(KeyCode::Tab, NONE), Some(Action::SwitchPanel));
        assert_eq!(p(KeyCode::Up, NONE), Some(Action::CursorUp));
        assert_eq!(p(KeyCode::Down, NONE), Some(Action::CursorDown));
        assert_eq!(p(KeyCode::PageUp, NONE), Some(Action::PageUp));
        assert_eq!(p(KeyCode::PageDown, NONE), Some(Action::PageDown));
        assert_eq!(p(KeyCode::Home, NONE), Some(Action::Home));
        assert_eq!(p(KeyCode::End, NONE), Some(Action::End));
        assert_eq!(p(KeyCode::Enter, NONE), Some(Action::Enter));
        assert_eq!(p(KeyCode::Backspace, NONE), Some(Action::Parent));
        assert_eq!(p(KeyCode::Char('.'), NONE), Some(Action::ToggleHidden));
        assert_eq!(p(KeyCode::Char(' '), NONE), Some(Action::ToggleSelect));
        assert_eq!(p(KeyCode::Insert, NONE), Some(Action::ToggleSelect));
        assert_eq!(p(KeyCode::Char(':'), NONE), Some(Action::CommandLine));
        assert_eq!(p(KeyCode::Char('t'), CTRL), Some(Action::ToggleMode));
        assert_eq!(p(KeyCode::F(10), SHIFT), Some(Action::QuitNoCd));
        assert_eq!(p(KeyCode::F(1), ALT), Some(Action::ConnectLeft));
        assert_eq!(p(KeyCode::F(2), ALT), Some(Action::ConnectRight));
        assert_eq!(p(KeyCode::Char('o'), CTRL), Some(Action::OpenWith));
        assert_eq!(p(KeyCode::Char('x'), NONE), None);
    }

    #[test]
    fn mdir_preset_has_alt_digit_alternates() {
        let km = mdir();
        for (d, a) in [
            ('1', Action::Help),
            ('2', Action::Rename),
            ('3', Action::View),
            ('4', Action::Edit),
            ('5', Action::Copy),
            ('6', Action::Move),
            ('7', Action::Mkdir),
            ('8', Action::Delete),
            ('9', Action::Menu),
            ('0', Action::Quit),
        ] {
            assert_eq!(
                look(&km, Context::Panel, KeyCode::Char(d), ALT),
                Some(a),
                "Alt+{d}"
            );
        }
    }

    #[test]
    fn mdir_preset_other_contexts() {
        let km = mdir();
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Esc, NONE),
            Some(Action::Cancel)
        );
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Enter, NONE),
            Some(Action::Confirm)
        );
        assert_eq!(
            look(&km, Context::Viewer, KeyCode::Esc, NONE),
            Some(Action::Quit)
        );
        assert_eq!(
            look(&km, Context::Viewer, KeyCode::Char('q'), NONE),
            Some(Action::Quit)
        );
        assert_eq!(
            look(&km, Context::Viewer, KeyCode::Down, NONE),
            Some(Action::CursorDown)
        );
        assert_eq!(
            look(&km, Context::Menu, KeyCode::Enter, NONE),
            Some(Action::Enter)
        );
        assert_eq!(
            look(&km, Context::Menu, KeyCode::Esc, NONE),
            Some(Action::Cancel)
        );
        assert_eq!(
            look(&km, Context::Trash, KeyCode::Enter, NONE),
            Some(Action::Enter)
        );
        assert_eq!(
            look(&km, Context::Trash, KeyCode::F(8), NONE),
            Some(Action::Delete)
        );
        assert_eq!(
            look(&km, Context::Trash, KeyCode::Backspace, NONE),
            Some(Action::Parent)
        );
    }

    #[test]
    fn mdir_trash_context_covers_the_whole_trash_view() {
        let km = mdir();
        let t = |c, m| look(&km, Context::Trash, c, m);
        assert_eq!(t(KeyCode::Tab, NONE), Some(Action::SwitchPanel));
        assert_eq!(t(KeyCode::Char(' '), NONE), Some(Action::ToggleSelect));
        assert_eq!(t(KeyCode::Insert, NONE), Some(Action::ToggleSelect));
        assert_eq!(t(KeyCode::Char('q'), NONE), Some(Action::Quit));
        assert_eq!(t(KeyCode::F(8), ALT), Some(Action::TrashView));
        assert_eq!(t(KeyCode::F(5), NONE), Some(Action::Copy));
    }

    #[test]
    fn vim_preset_has_single_keys() {
        let km = Keymap::preset("vim").unwrap();
        let p = |c| look(&km, Context::Panel, KeyCode::Char(c), NONE);
        assert_eq!(p('j'), Some(Action::CursorDown));
        assert_eq!(p('k'), Some(Action::CursorUp));
        assert_eq!(p('h'), Some(Action::Parent));
        assert_eq!(p('l'), Some(Action::Enter));
        assert_eq!(p(' '), Some(Action::ToggleSelect));
        assert_eq!(p('v'), Some(Action::View));
        assert_eq!(p('e'), Some(Action::Edit));
        assert_eq!(p('y'), Some(Action::Copy));
        assert_eq!(p('m'), Some(Action::Move));
        assert_eq!(p('d'), Some(Action::Delete));
        assert_eq!(p('r'), Some(Action::Rename));
        assert_eq!(p(':'), Some(Action::CommandLine));
        assert_eq!(p('q'), Some(Action::Quit));
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Esc, NONE),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn unknown_preset_is_none() {
        assert!(Keymap::preset("emacs").is_none());
        assert!(Keymap::preset("MDIR").is_some());
    }

    #[test]
    fn empty_config_equals_mdir_preset() {
        let (km, warns) = build(&KeysConfig::default());
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(5), NONE),
            Some(Action::Copy)
        );
    }

    #[test]
    fn user_binding_replaces_preset_keys_for_action() {
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = \"c\"\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('c'), NONE),
            Some(Action::Copy)
        );
        assert_eq!(look(&km, Context::Panel, KeyCode::F(5), NONE), None);
        assert_eq!(look(&km, Context::Panel, KeyCode::Char('5'), ALT), None);
        // others untouched
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(6), NONE),
            Some(Action::Move)
        );
    }

    #[test]
    fn multi_bind() {
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = [\"F5\", \"c\"]\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(5), NONE),
            Some(Action::Copy)
        );
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('c'), NONE),
            Some(Action::Copy)
        );
        assert_eq!(
            km.keys_for(Context::Panel, Action::Copy),
            vec![ks("F5"), ks("c")]
        );
    }

    #[test]
    fn empty_array_disables_action() {
        let (km, warns) = build(&cfg("[keys.bind]\nview = []\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(look(&km, Context::Panel, KeyCode::F(3), NONE), None);
        assert!(km.keys_for(Context::Panel, Action::View).is_empty());
    }

    #[test]
    fn unknown_action_warns_and_is_ignored() {
        let (km, warns) = build(&cfg("[keys.bind]\nfrobnicate = \"x\"\n"));
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("frobnicate"), "{warns:?}");
        assert_eq!(look(&km, Context::Panel, KeyCode::Char('x'), NONE), None);
    }

    #[test]
    fn bad_key_warns_and_is_ignored() {
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = [\"Bogus+X\", \"c\"]\n"));
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("Bogus+X"), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('c'), NONE),
            Some(Action::Copy)
        );
    }

    #[test]
    fn all_bad_keys_keep_preset() {
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = \"Bogus+X\"\n"));
        assert_eq!(warns.len(), 1);
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(5), NONE),
            Some(Action::Copy)
        );
    }

    #[test]
    fn unknown_preset_warns_and_falls_back_to_mdir() {
        let (km, warns) = build(&cfg("[keys]\npreset = \"emacs\"\n"));
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("emacs"), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(5), NONE),
            Some(Action::Copy)
        );
    }

    #[test]
    fn vim_preset_selected_by_config() {
        let (km, warns) = build(&cfg("[keys]\npreset = \"vim\"\n"));
        assert!(warns.is_empty());
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('j'), NONE),
            Some(Action::CursorDown)
        );
    }

    #[test]
    fn duplicate_key_warns_and_user_beats_preset() {
        // F6 is Move in the preset; user binds it to copy.
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = [\"F6\"]\n"));
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("F6"), "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::F(6), NONE),
            Some(Action::Copy)
        );
        assert!(
            !km.keys_for(Context::Panel, Action::Move)
                .contains(&ks("F6"))
        );
    }

    #[test]
    fn duplicate_among_user_entries_later_sorted_action_wins() {
        // sorted action names: "copy" < "delete" -> delete wins.
        let (km, warns) = build(&cfg("[keys.bind]\ndelete = \"x\"\ncopy = \"x\"\n"));
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('x'), NONE),
            Some(Action::Delete)
        );
        assert!(km.keys_for(Context::Panel, Action::Copy).is_empty());
    }

    #[test]
    fn same_key_listed_twice_for_one_action_is_not_a_conflict() {
        let (km, warns) = build(&cfg("[keys.bind]\ncopy = [\"c\", \"C\", \"c\"]\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            km.keys_for(Context::Panel, Action::Copy),
            vec![ks("c"), ks("C")]
        );
    }

    #[test]
    fn viewer_section_overrides_viewer_context_only() {
        let (km, warns) = build(&cfg("[keys.viewer]\nquit = \"x\"\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Viewer, KeyCode::Char('x'), NONE),
            Some(Action::Quit)
        );
        assert_eq!(look(&km, Context::Viewer, KeyCode::Char('q'), NONE), None);
        assert_eq!(
            look(&km, Context::Panel, KeyCode::Char('q'), NONE),
            Some(Action::Quit)
        );
        assert_eq!(look(&km, Context::Panel, KeyCode::Char('x'), NONE), None);
    }

    #[test]
    fn menu_and_trash_sections_apply() {
        let (km, warns) = build(&cfg(
            "[keys.menu]\nenter = \"l\"\n[keys.trash]\nparent = \"h\"\n",
        ));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Menu, KeyCode::Char('l'), NONE),
            Some(Action::Enter)
        );
        assert_eq!(
            look(&km, Context::Trash, KeyCode::Char('h'), NONE),
            Some(Action::Parent)
        );
    }

    #[test]
    fn dialog_esc_and_enter_are_locked_against_rebinding() {
        let (km, warns) = build(&cfg("[keys.dialog]\nquit = [\"Esc\", \"Enter\"]\n"));
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Esc, NONE),
            Some(Action::Cancel)
        );
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Enter, NONE),
            Some(Action::Confirm)
        );
    }

    #[test]
    fn dialog_locked_keys_cannot_be_removed() {
        let (km, warns) = build(&cfg("[keys.dialog]\ncancel = []\nconfirm = \"y\"\n"));
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Esc, NONE),
            Some(Action::Cancel)
        );
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Enter, NONE),
            Some(Action::Confirm)
        );
        // extra key still added
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Char('y'), NONE),
            Some(Action::Confirm)
        );
    }

    #[test]
    fn dialog_extra_keys_allowed_without_warning() {
        let (km, warns) = build(&cfg("[keys.dialog]\ncancel = [\"Esc\", \"n\"]\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(
            look(&km, Context::Dialog, KeyCode::Char('n'), NONE),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn locked_keys_only_apply_to_dialog() {
        let (km, warns) = build(&cfg("[keys.menu]\ncancel = \"q\"\n"));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(look(&km, Context::Menu, KeyCode::Esc, NONE), None);
    }

    #[test]
    fn keys_for_lists_preset_order() {
        let km = mdir();
        assert_eq!(
            km.keys_for(Context::Panel, Action::Copy),
            vec![ks("F5"), ks("Alt+5")]
        );
        assert_eq!(km.keys_for(Context::Panel, Action::Quit)[0], ks("F10"));
        assert!(km.keys_for(Context::Viewer, Action::Copy).is_empty());
    }

    #[test]
    fn describe_groups_keys_per_action() {
        let km = mdir();
        let d = km.describe(Context::Panel);
        let copy = d.iter().find(|(_, a)| *a == Action::Copy).unwrap();
        assert_eq!(copy.0, "F5, Alt+5");
        assert_eq!(d.iter().filter(|(_, a)| *a == Action::Copy).count(), 1);
    }

    #[test]
    fn plan_example_toml_parses() {
        let src = r#"
[keys]
preset = "mdir"
[keys.bind]
copy        = ["F5", "c"]
delete      = ["F8", "Delete"]
quit        = ["F10", "Ctrl+Q"]
toggle_mode = "Ctrl+T"
view        = []
"#;
        let (km, warns) = build(&cfg(src));
        assert!(warns.is_empty(), "{warns:?}");
        let p = |c, m| look(&km, Context::Panel, c, m);
        assert_eq!(p(KeyCode::Char('c'), NONE), Some(Action::Copy));
        assert_eq!(p(KeyCode::Delete, NONE), Some(Action::Delete));
        assert_eq!(p(KeyCode::F(8), NONE), Some(Action::Delete));
        assert_eq!(p(KeyCode::Char('q'), CTRL), Some(Action::Quit));
        assert_eq!(p(KeyCode::F(10), NONE), Some(Action::Quit));
        assert_eq!(p(KeyCode::Char('q'), NONE), None); // quit replaced
        assert_eq!(p(KeyCode::Char('t'), CTRL), Some(Action::ToggleMode));
        assert_eq!(p(KeyCode::F(3), NONE), None);
        assert_eq!(
            km.describe(Context::Panel)
                .iter()
                .find(|(_, a)| *a == Action::View),
            None
        );
    }

    #[test]
    fn action_names_are_snake_case() {
        for (n, a) in [
            ("cursor_up", Action::CursorUp),
            ("quit_no_cd", Action::QuitNoCd),
            ("move", Action::Move),
            ("trash_view", Action::TrashView),
            ("open_with", Action::OpenWith),
            ("cancel_job", Action::CancelJob),
        ] {
            assert_eq!(Action::from_name(n), Some(a));
        }
        assert_eq!(Action::from_name("nope"), None);
    }
}
