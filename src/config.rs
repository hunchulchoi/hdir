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

fn toml_string(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Set `[theme] <key> = "<name>"` in `text`, leaving everything else as is.
/// The result is re-parsed; if the edit did not take effect (an exotic layout
/// such as an inline table) an error is returned and nothing should be written.
pub fn set_theme_in_text(text: &str, key: &str, name: &str) -> Result<String, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let new_line = format!("{key} = {}", toml_string(name));

    let mut in_theme = false;
    let mut header = None;
    let mut key_line = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with('[') {
            let rest = t.strip_prefix("[theme]").map(str::trim);
            in_theme = rest.is_some_and(|r| r.is_empty() || r.starts_with('#'));
            if in_theme {
                header = Some(i);
            }
        } else if in_theme
            && let Some(rest) = t.strip_prefix(key)
            && rest.trim_start().starts_with('=')
        {
            key_line = Some(i);
        }
    }
    match (key_line, header) {
        (Some(i), _) => {
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            lines[i] = format!("{indent}{new_line}");
        }
        (None, Some(h)) => lines.insert(h + 1, new_line),
        (None, None) => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[theme]".into());
            lines.push(new_line);
        }
    }
    let mut out = lines.join(eol);
    out.push_str(eol);
    let ok = out
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("theme")?.get(key)?.as_str().map(|s| s == name))
        .unwrap_or(false);
    if ok {
        Ok(out)
    } else {
        Err("config.toml 구조를 자동 수정할 수 없음 (직접 [theme] 수정 필요)".into())
    }
}

/// Persist `theme.<mode> = name` into `<dir>/config.toml` (created if missing).
pub fn save_theme(dir: &Path, mode: Mode, name: &str) -> Result<(), String> {
    let path = config_file(dir);
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{} 읽기 실패: {e}", path.display())),
    };
    let updated = set_theme_in_text(&existing, mode.key(), name)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{} 생성 실패: {e}", dir.display()))?;
    let tmp = dir.join("config.toml.tmp");
    std::fs::write(&tmp, updated).map_err(|e| format!("저장 실패: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("저장 실패: {e}")
    })
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
            "# my config\n[theme]\nclassic = \"light\"\ndev = \"dark\"\n\n[keys]\npreset = \"vim\"\n"
        );
    }

    #[test]
    fn set_theme_adds_key_to_existing_table_or_new_table() {
        let out =
            set_theme_in_text("[theme]\ndev = \"dark\"\n[keys]\n", "classic", "light").unwrap();
        assert_eq!(
            out,
            "[theme]\nclassic = \"light\"\ndev = \"dark\"\n[keys]\n"
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
        assert_eq!(out, "[theme]\r\nclassic = \"a\\\"b\"\r\n");
    }

    #[test]
    fn set_theme_rejects_layouts_it_cannot_edit() {
        let err = set_theme_in_text("theme = { classic = \"mdir\" }\n", "classic", "dark");
        assert!(err.is_err());
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
        assert!(!dir.join("config.toml.tmp").exists());
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
