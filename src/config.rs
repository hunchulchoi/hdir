//! `config.toml` loading and the settings derived from it (theme, keymap, assoc).
//!
//! Nothing here aborts on bad input: problems become warning strings and the
//! offending part falls back to its default. The config directory is injected
//! so tests never touch the real one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::assoc::{AssocRule, AssocTable};
use crate::keymap::{self, Keymap, KeysConfig};
use crate::servers::ServerConfig;
use crate::theme::{self, Theme};

/// Layout mode; the color comes from the per-mode theme setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Classic,
    Dev,
}

impl Mode {
    /// Key of this mode inside the `[theme]` table.
    pub fn key(self) -> &'static str {
        match self {
            Mode::Classic => "classic",
            Mode::Dev => "dev",
        }
    }

    pub fn toggled(self) -> Mode {
        match self {
            Mode::Classic => Mode::Dev,
            Mode::Dev => Mode::Classic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub classic: String,
    pub dev: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            classic: "mdir".into(),
            dev: "mdir".into(),
        }
    }
}

impl ThemeConfig {
    pub fn name_for(&self, mode: Mode) -> &str {
        match mode {
            Mode::Classic => &self.classic,
            Mode::Dev => &self.dev,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: ThemeConfig,
    pub keys: KeysConfig,
    pub assoc: Vec<AssocRule>,
    pub servers: Vec<ServerConfig>,
}

/// `$XDG_CONFIG_HOME/hdir` if set (and non-empty), else `<home>/.config/hdir`.
/// Used on unix including macOS.
pub fn config_dir_from(xdg: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match xdg.filter(|v| !v.is_empty()) {
        Some(x) => Some(PathBuf::from(x).join(crate::APP_NAME)),
        None => home.map(|h| h.join(".config").join(crate::APP_NAME)),
    }
}

/// The real config directory: `%APPDATA%\hdir` on Windows, else see [`config_dir_from`].
pub fn default_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        dirs::config_dir().map(|d| d.join(crate::APP_NAME))
    }
    #[cfg(not(windows))]
    {
        config_dir_from(std::env::var_os("XDG_CONFIG_HOME"), dirs::home_dir())
    }
}

/// `COLORTERM` to base the color depth on. Windows Terminal does not set it
/// but handles 24-bit color, so it defaults to `truecolor` there.
pub fn colorterm_value(env: Option<String>, windows: bool) -> Option<String> {
    env.filter(|v| !v.is_empty())
        .or_else(|| windows.then(|| "truecolor".to_string()))
}

pub fn config_file(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

/// Read `<dir>/config.toml`. Missing file or no dir: defaults, no warning.
/// Unreadable or invalid: defaults plus a warning.
pub fn load_config(dir: Option<&Path>) -> (Config, Vec<String>) {
    let Some(dir) = dir else {
        return (Config::default(), Vec::new());
    };
    let path = config_file(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Config::default(), Vec::new());
        }
        Err(e) => {
            return (
                Config::default(),
                vec![format!("config: {} 읽기 실패: {e}", path.display())],
            );
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(c) => (c, Vec::new()),
        Err(e) => {
            let msg = e.to_string();
            let first = msg.lines().next().unwrap_or("parse error");
            (
                Config::default(),
                vec![format!(
                    "config: {} 해석 실패, 기본값 사용: {first}",
                    path.display()
                )],
            )
        }
    }
}

/// Everything derived from the config directory, ready for the app to adopt.
#[derive(Debug, Clone)]
pub struct Settings {
    pub config: Config,
    pub keymap: Keymap,
    pub user_themes: Vec<Theme>,
    pub classic: Theme,
    pub dev: Theme,
    pub assoc: AssocTable,
    /// config, keymap, theme and assoc warnings, in that order.
    pub warnings: Vec<String>,
}

impl Settings {
    pub fn theme_for(&self, mode: Mode) -> &Theme {
        match mode {
            Mode::Classic => &self.classic,
            Mode::Dev => &self.dev,
        }
    }
}

pub fn load_settings(dir: Option<&Path>) -> Settings {
    let (config, mut warnings) = load_config(dir);
    let (keymap, w) = keymap::build(&config.keys);
    warnings.extend(w);
    let (user_themes, w) = match dir {
        Some(d) => theme::load_user_themes(&d.join("themes")),
        None => (Vec::new(), Vec::new()),
    };
    warnings.extend(w);
    let (classic, w) = theme::resolve(&config.theme.classic, &user_themes);
    warnings.extend(w);
    let (dev, w) = if config.theme.dev == config.theme.classic {
        (classic.clone(), Vec::new())
    } else {
        theme::resolve(&config.theme.dev, &user_themes)
    };
    warnings.extend(w);
    let (assoc, w) = AssocTable::from_rules(config.assoc.clone());
    warnings.extend(w);
    Settings {
        config,
        keymap,
        user_themes,
        classic,
        dev,
        assoc,
        warnings,
    }
}

/// Status-line text for startup/reload warnings: the first one plus "(외 N개)".
pub fn warning_summary(warnings: &[String]) -> Option<String> {
    let first = warnings.first()?;
    Some(match warnings.len() - 1 {
        0 => format!("설정 경고: {first}"),
        n => format!("설정 경고: {first} (외 {n}개)"),
    })
}

/// Set `theme.<key> = "<name>"` in `text` with `toml_edit`, so comments,
/// formatting, multiline strings and every other key are left as they are.
/// Works for a `[theme]` table, dotted keys and an inline table; a missing
/// table is created. Errors (unparsable file, `theme` not a table) mean nothing
/// should be written.
pub fn set_theme_in_text(text: &str, key: &str, name: &str) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let fail =
        |why: &str| format!("config.toml을 자동 수정할 수 없음 ({why}); 직접 [theme] 수정 필요");
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| fail(e.message()))?;
    if !doc.contains_key("theme") {
        // New table goes last; comments that trailed the file stay above it.
        let trailing = doc
            .trailing()
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default();
        doc.set_trailing("");
        let mut table = Table::new();
        let first = doc.as_table().is_empty() && trailing.is_empty();
        let sep = if first || trailing.ends_with("\n\n") {
            ""
        } else {
            "\n"
        };
        table.decor_mut().set_prefix(format!("{trailing}{sep}"));
        doc.insert("theme", Item::Table(table));
    }
    let item = doc.entry("theme").or_insert(Item::None);
    // Keep the existing value's trailing comment/whitespace when replacing.
    fn set(t: &mut dyn toml_edit::TableLike, key: &str, name: &str) {
        let mut v = value(name);
        if let Some(old) = t.get(key).and_then(Item::as_value)
            && let Some(nv) = v.as_value_mut()
        {
            *nv.decor_mut() = old.decor().clone();
        }
        t.insert(key, v);
    }
    match item.as_table_like_mut() {
        Some(t) => set(t, key, name),
        None => return Err(fail("theme이 테이블이 아님")),
    }
    let mut out = doc.to_string();
    if text.contains("\r\n") {
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    Ok(out)
}

/// Resolve a symlinked `config.toml` to its real file so a save replaces the
/// target and leaves the link intact.
fn real_config_path(path: &Path) -> Result<PathBuf, String> {
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return Ok(path.to_path_buf());
    }
    match std::fs::canonicalize(path) {
        Ok(p) => Ok(p),
        // Dangling link: create the file it points to.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let target = std::fs::read_link(path)
                .map_err(|e| format!("{} 링크 읽기 실패: {e}", path.display()))?;
            Ok(path.parent().map_or(target.clone(), |p| p.join(&target)))
        }
        Err(e) => Err(format!("{} 경로 확인 실패: {e}", path.display())),
    }
}

/// Write `text` to `real` via an exclusively created sibling temp file
/// (0600 on unix, never a fixed name, never follows a symlink), give it the
/// existing file's permissions, then atomically replace.
fn write_atomic(real: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let err = |e: std::io::Error| format!("저장 실패: {e}");
    let (tmp, mut file) = (0..16)
        .find_map(|_| {
            let tmp = crate::fs_ops::temp_sibling(real);
            match crate::platform::create_private_file(&tmp) {
                Ok(f) => Some(Ok((tmp, f))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(e)),
            }
        })
        .unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "임시 파일 이름을 정하지 못함",
            ))
        })
        .map_err(err)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(real) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        crate::platform::rename_replace(&tmp, real)
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(e)
    })
}

/// Persist `theme.<mode> = name` into `<dir>/config.toml` (created if missing;
/// a symlinked file is edited in place at its target).
pub fn save_theme(dir: &Path, mode: Mode, name: &str) -> Result<(), String> {
    let path = config_file(dir);
    let real = real_config_path(&path)?;
    let existing = match std::fs::read_to_string(&real) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{} 읽기 실패: {e}", real.display())),
    };
    let updated = set_theme_in_text(&existing, mode.key(), name)?;
    if let Some(parent) = real.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("{} 생성 실패: {e}", parent.display()))?;
    }
    write_atomic(&real, &updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_cfg(dir: &Path, text: &str) {
        fs::write(config_file(dir), text).unwrap();
    }

    #[test]
    fn config_dir_prefers_xdg_then_home_dot_config() {
        assert_eq!(
            config_dir_from(Some("/x/cfg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/cfg/hdir"))
        );
        assert_eq!(
            config_dir_from(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/hdir"))
        );
        // empty XDG_CONFIG_HOME counts as unset
        assert_eq!(
            config_dir_from(Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/hdir"))
        );
        assert_eq!(config_dir_from(None, None), None);
    }

    #[test]
    fn defaults_are_mdir_for_both_modes() {
        let c = Config::default();
        assert_eq!(c.theme.classic, "mdir");
        assert_eq!(c.theme.dev, "mdir");
        assert!(c.assoc.is_empty() && c.servers.is_empty());
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let (c, w) = load_config(Some(tmp.path()));
        assert!(w.is_empty());
        assert_eq!(c.theme, ThemeConfig::default());
        let (_, w) = load_config(None);
        assert!(w.is_empty());
    }

    #[test]
    fn parses_all_sections() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            r#"
[theme]
classic = "dark"
dev = "light"
[keys]
preset = "vim"
[keys.bind]
copy = ["c"]
[[assoc]]
ext = ["md"]
cmd = "$EDITOR {}"
[[servers]]
name = "prod"
host = "prod-web"
theme = "mdir-red"
"#,
        );
        let (c, w) = load_config(Some(tmp.path()));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.theme.classic, "dark");
        assert_eq!(c.theme.name_for(Mode::Dev), "light");
        assert_eq!(c.keys.preset.as_deref(), Some("vim"));
        assert_eq!(c.assoc.len(), 1);
        assert_eq!(c.servers[0].name.as_deref(), Some("prod"));
    }

    #[test]
    fn partial_theme_table_keeps_other_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "[theme]\ndev = \"dark\"\n");
        let (c, w) = load_config(Some(tmp.path()));
        assert!(w.is_empty());
        assert_eq!(
            (c.theme.classic.as_str(), c.theme.dev.as_str()),
            ("mdir", "dark")
        );
    }

    #[test]
    fn parse_error_gives_defaults_and_one_warning() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "[theme\nclassic = ");
        let (c, w) = load_config(Some(tmp.path()));
        assert_eq!(c.theme, ThemeConfig::default());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("config.toml"), "{}", w[0]);
    }

    #[test]
    fn settings_collect_warnings_from_every_source_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            r#"
[theme]
classic = "nope"
[keys.bind]
frobnicate = "F5"
[[assoc]]
cmd = "x {}"
"#,
        );
        fs::create_dir(tmp.path().join("themes")).unwrap();
        fs::write(tmp.path().join("themes/bad.toml"), "[colors\n").unwrap();
        let s = load_settings(Some(tmp.path()));
        let joined = s.warnings.join("\n");
        assert!(s.warnings.len() >= 4, "{joined}");
        let pos = |needle: &str| {
            joined
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {joined}"))
        };
        assert!(pos("frobnicate") < pos("bad"));
        assert!(pos("bad") < pos("nope"));
        assert!(pos("nope") < pos("assoc"));
        assert_eq!(s.classic.name, "mdir", "unknown theme falls back to mdir");
    }

    #[test]
    fn settings_resolve_user_theme_and_keymap() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "[theme]\nclassic = \"mine\"\ndev = \"dark\"\n[keys.bind]\ncopy = [\"c\"]\n",
        );
        fs::create_dir(tmp.path().join("themes")).unwrap();
        fs::write(
            tmp.path().join("themes/mine.toml"),
            "base = \"mdir\"\n[colors]\nbg = \"#112233\"\n",
        )
        .unwrap();
        let s = load_settings(Some(tmp.path()));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.classic.name, "mine");
        assert_eq!(s.dev.name, "dark");
        assert_eq!(s.theme_for(Mode::Classic).name, "mine");
        assert_eq!(s.user_themes.len(), 1);
        let ev = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('c'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(
            s.keymap.lookup(keymap::Context::Panel, &ev),
            Some(keymap::Action::Copy)
        );
    }

    #[test]
    fn no_dir_gives_all_defaults() {
        let s = load_settings(None);
        assert!(s.warnings.is_empty());
        assert_eq!(s.classic.name, "mdir");
        assert_eq!(s.dev.name, "mdir");
    }

    #[test]
    fn warning_summary_counts_the_rest() {
        assert_eq!(warning_summary(&[]), None);
        assert_eq!(warning_summary(&["a".into()]).unwrap(), "설정 경고: a");
        assert_eq!(
            warning_summary(&["a".into(), "b".into(), "c".into()]).unwrap(),
            "설정 경고: a (외 2개)"
        );
    }

    #[test]
    fn set_theme_replaces_existing_key_and_preserves_the_rest() {
        let src = "# my config\n[theme]\nclassic = \"mdir\" \ndev = \"dark\"\n\n[keys]\npreset = \"vim\"\n";
        let out = set_theme_in_text(src, "classic", "light").unwrap();
        assert_eq!(
            out,
            "# my config\n[theme]\nclassic = \"light\" \ndev = \"dark\"\n\n[keys]\npreset = \"vim\"\n"
        );
    }

    #[test]
    fn set_theme_adds_key_to_existing_table_or_new_table() {
        let out =
            set_theme_in_text("[theme]\ndev = \"dark\"\n[keys]\n", "classic", "light").unwrap();
        assert_eq!(
            out,
            "[theme]\ndev = \"dark\"\nclassic = \"light\"\n[keys]\n"
        );
        let out = set_theme_in_text("[keys]\npreset = \"vim\"", "dev", "dark").unwrap();
        assert_eq!(out, "[keys]\npreset = \"vim\"\n\n[theme]\ndev = \"dark\"\n");
        assert_eq!(
            set_theme_in_text("", "classic", "x").unwrap(),
            "[theme]\nclassic = \"x\"\n"
        );
    }

    #[test]
    fn set_theme_ignores_same_key_in_other_tables() {
        let src = "[servers.x]\nclassic = 1\n[theme]\n";
        let out = set_theme_in_text(src, "classic", "dark").unwrap();
        assert_eq!(
            out,
            "[servers.x]\nclassic = 1\n[theme]\nclassic = \"dark\"\n"
        );
    }

    #[test]
    fn set_theme_escapes_and_keeps_crlf() {
        let out = set_theme_in_text("[theme]\r\n", "classic", "a\"b").unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");
        let t: toml::Table = out.parse().unwrap();
        assert_eq!(t["theme"]["classic"].as_str(), Some("a\"b"));
    }

    #[test]
    fn set_theme_leaves_multiline_strings_and_comments_alone() {
        let src = "# top\nnote = \"\"\"\n[theme]\nclassic = \"x\"\n\"\"\"\n\n[theme] # mine\nclassic = \"mdir\" # keep me\n";
        let out = set_theme_in_text(src, "classic", "dark").unwrap();
        assert_eq!(
            out,
            "# top\nnote = \"\"\"\n[theme]\nclassic = \"x\"\n\"\"\"\n\n[theme] # mine\nclassic = \"dark\" # keep me\n"
        );
        let t: toml::Table = out.parse().unwrap();
        assert!(t["note"].as_str().unwrap().contains("classic = \"x\""));
    }

    #[test]
    fn set_theme_updates_inline_table_and_dotted_keys() {
        let out = set_theme_in_text("theme = { classic = \"mdir\" }\n", "classic", "dark").unwrap();
        assert_eq!(out, "theme = { classic = \"dark\" }\n");
        let out = set_theme_in_text("theme = { dev = \"x\" }\n", "classic", "dark").unwrap();
        let t: toml::Table = out.parse().unwrap();
        assert_eq!(t["theme"]["classic"].as_str(), Some("dark"));
        assert_eq!(t["theme"]["dev"].as_str(), Some("x"));
        let out =
            set_theme_in_text("theme.classic = \"mdir\"\n[keys]\n", "classic", "dark").unwrap();
        assert_eq!(out, "theme.classic = \"dark\"\n[keys]\n");
        let out = set_theme_in_text("theme.dev = \"x\"\n", "classic", "dark").unwrap();
        let t: toml::Table = out.parse().unwrap();
        assert_eq!(t["theme"]["classic"].as_str(), Some("dark"));
    }

    #[test]
    fn set_theme_rejects_a_non_table_theme() {
        assert!(set_theme_in_text("theme = 3\n", "classic", "dark").is_err());
        assert!(set_theme_in_text("[theme\n", "classic", "dark").is_err());
    }

    #[test]
    fn save_theme_writes_file_and_reload_sees_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/hdir"); // created on demand
        save_theme(&dir, Mode::Dev, "dark").unwrap();
        save_theme(&dir, Mode::Classic, "light").unwrap();
        let (c, w) = load_config(Some(&dir));
        assert!(w.is_empty());
        assert_eq!(
            (c.theme.classic.as_str(), c.theme.dev.as_str()),
            ("light", "dark")
        );
        assert!(leftovers(&dir).is_empty());
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        v.sort();
        v
    }

    #[cfg(unix)]
    #[test]
    fn save_theme_never_writes_through_a_planted_symlink_at_a_fixed_temp_name() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        let dir = tmp.path().join("cfg");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("config.toml.tmp")).unwrap();
        save_theme(&dir, Mode::Classic, "dark").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(
            fs::read_to_string(dir.join("config.toml"))
                .unwrap()
                .contains("dark")
        );
        assert_eq!(
            leftovers(&dir),
            ["config.toml.tmp"],
            "only the planted link remains"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_theme_creates_new_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        save_theme(tmp.path(), Mode::Classic, "dark").unwrap();
        let mode = fs::metadata(config_file(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_theme_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        for m in [0o600, 0o644] {
            let f = config_file(tmp.path());
            fs::write(&f, "[keys]\n").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(m)).unwrap();
            save_theme(tmp.path(), Mode::Dev, "dark").unwrap();
            assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, m);
            assert!(fs::read_to_string(&f).unwrap().contains("dark"));
        }
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn save_theme_through_a_symlinked_config_keeps_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("dotfiles");
        fs::create_dir(&real_dir).unwrap();
        fs::write(real_dir.join("hdir.toml"), "# mine\n").unwrap();
        let dir = tmp.path().join("cfg");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(real_dir.join("hdir.toml"), config_file(&dir)).unwrap();
        save_theme(&dir, Mode::Classic, "dark").unwrap();
        assert!(
            fs::symlink_metadata(config_file(&dir))
                .unwrap()
                .is_symlink()
        );
        let real = fs::read_to_string(real_dir.join("hdir.toml")).unwrap();
        assert!(
            real.starts_with("# mine\n") && real.contains("dark"),
            "{real}"
        );
        let names: Vec<_> = fs::read_dir(&real_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp left in the target dir: {names:?}");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn colorterm_defaults_to_truecolor_only_on_windows() {
        assert_eq!(colorterm_value(None, false), None);
        assert_eq!(colorterm_value(Some(String::new()), false), None);
        assert_eq!(colorterm_value(None, true).as_deref(), Some("truecolor"));
        assert_eq!(
            colorterm_value(Some("24bit".into()), true).as_deref(),
            Some("24bit")
        );
        assert_eq!(
            colorterm_value(Some("256color".into()), true).as_deref(),
            Some("256color")
        );
    }

    #[test]
    fn mode_helpers() {
        assert_eq!(Mode::default(), Mode::Classic);
        assert_eq!(Mode::Classic.toggled(), Mode::Dev);
        assert_eq!(Mode::Dev.key(), "dev");
    }
}
