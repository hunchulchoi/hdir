//! Theme slots, color parsing, base inheritance, built-in and user themes.

use ratatui::style::Color;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct ExtGroup {
    pub name: String,
    pub exts: Vec<String>,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub bg: Color,
    pub fg: Color,
    pub border: Color,
    pub border_active: Color,
    pub title: Color,
    pub dir: Color,
    pub file: Color,
    pub exec: Color,
    pub symlink: Color,
    pub hidden: Color,
    pub assoc_file: Color,
    pub selected: Color,
    pub cursor_bg: Color,
    pub cursor_fg: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub fkey_num_fg: Color,
    pub fkey_num_bg: Color,
    pub fkey_label_fg: Color,
    pub fkey_label_bg: Color,
    pub dialog_bg: Color,
    pub dialog_fg: Color,
    pub danger_bg: Color,
    pub danger_fg: Color,
    pub preview_bg: Color,
    pub error: Color,
    pub dir_bold: bool,
    pub ext_colors: Vec<ExtGroup>,
}

impl Theme {
    /// All slots set to `Reset`; only used as the starting point for `mdir`,
    /// which has no base and must define everything itself.
    fn blank(name: &str) -> Theme {
        let c = Color::Reset;
        Theme {
            name: name.to_string(),
            bg: c,
            fg: c,
            border: c,
            border_active: c,
            title: c,
            dir: c,
            file: c,
            exec: c,
            symlink: c,
            hidden: c,
            assoc_file: c,
            selected: c,
            cursor_bg: c,
            cursor_fg: c,
            status_bg: c,
            status_fg: c,
            fkey_num_fg: c,
            fkey_num_bg: c,
            fkey_label_fg: c,
            fkey_label_bg: c,
            dialog_bg: c,
            dialog_fg: c,
            danger_bg: c,
            danger_fg: c,
            preview_bg: c,
            error: c,
            dir_bold: false,
            ext_colors: Vec::new(),
        }
    }

    fn slot_mut(&mut self, slot: &str) -> Option<&mut Color> {
        Some(match slot {
            "bg" => &mut self.bg,
            "fg" => &mut self.fg,
            "border" => &mut self.border,
            "border_active" => &mut self.border_active,
            "title" => &mut self.title,
            "dir" => &mut self.dir,
            "file" => &mut self.file,
            "exec" => &mut self.exec,
            "symlink" => &mut self.symlink,
            "hidden" => &mut self.hidden,
            "assoc_file" => &mut self.assoc_file,
            "selected" => &mut self.selected,
            "cursor_bg" => &mut self.cursor_bg,
            "cursor_fg" => &mut self.cursor_fg,
            "status_bg" => &mut self.status_bg,
            "status_fg" => &mut self.status_fg,
            "fkey_num_fg" => &mut self.fkey_num_fg,
            "fkey_num_bg" => &mut self.fkey_num_bg,
            "fkey_label_fg" => &mut self.fkey_label_fg,
            "fkey_label_bg" => &mut self.fkey_label_bg,
            "dialog_bg" => &mut self.dialog_bg,
            "dialog_fg" => &mut self.dialog_fg,
            "danger_bg" => &mut self.danger_bg,
            "danger_fg" => &mut self.danger_fg,
            "preview_bg" => &mut self.preview_bg,
            "error" => &mut self.error,
            _ => return None,
        })
    }

    /// Color for a file name's last extension (case-insensitive); `.tar.gz` -> "gz".
    /// Dotfiles without a further extension (`.zip`) and trailing dots have none.
    pub fn color_for_ext(&self, name: &str) -> Option<Color> {
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() || ext.is_empty() {
            return None;
        }
        let ext = ext.to_lowercase();
        self.ext_colors
            .iter()
            .find(|g| g.exts.contains(&ext))
            .map(|g| g.color)
    }
}

/// Parse a color: name, `#RRGGBB`, `#RGB`, or 256-color index `0..=255`.
pub fn parse_color(s: &str) -> Result<Color, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty color".into());
    }
    if let Some(h) = t.strip_prefix('#') {
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("invalid hex color {s:?}"));
        }
        let v = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).unwrap_or(0);
        return match h.len() {
            6 => Ok(Color::Rgb(v(0, 2), v(2, 2), v(4, 2))),
            3 => Ok(Color::Rgb(v(0, 1) * 17, v(1, 1) * 17, v(2, 1) * 17)),
            _ => Err(format!("invalid hex color {s:?}")),
        };
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t
            .parse::<u8>()
            .map(Color::Indexed)
            .map_err(|_| format!("color index out of range {s:?}"));
    }
    let key: String = t
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .collect::<String>()
        .to_lowercase();
    Ok(match key.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        "reset" => Color::Reset,
        _ => return Err(format!("unknown color {s:?}")),
    })
}

fn mdir_fallback(resolve_base: &impl Fn(&str) -> Option<Theme>) -> Theme {
    resolve_base("mdir")
        .or_else(|| builtin("mdir"))
        .unwrap_or_else(|| Theme::blank("mdir"))
}

/// Parse a theme from TOML. Never fails: problems become warnings and the
/// affected values are inherited from the base theme.
pub fn from_toml(src: &str, resolve_base: impl Fn(&str) -> Option<Theme>) -> (Theme, Vec<String>) {
    from_toml_named(src, "custom", resolve_base)
}

fn from_toml_named(
    src: &str,
    default_name: &str,
    resolve_base: impl Fn(&str) -> Option<Theme>,
) -> (Theme, Vec<String>) {
    let mut warnings = Vec::new();
    let table: toml::Table = match src.parse() {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("theme {default_name}: invalid TOML: {e}"));
            let mut t = mdir_fallback(&resolve_base);
            t.name = default_name.to_string();
            return (t, warnings);
        }
    };
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(default_name)
        .to_string();

    let mut theme = match table.get("base").and_then(|v| v.as_str()) {
        Some(base) => resolve_base(base).unwrap_or_else(|| {
            warnings.push(format!("theme {name}: unknown base {base:?}, using mdir"));
            mdir_fallback(&resolve_base)
        }),
        // `mdir` is the root of the hierarchy and defines every slot itself.
        None if name == "mdir" => Theme::blank("mdir"),
        None => mdir_fallback(&resolve_base),
    };
    theme.name = name.clone();

    for key in table.keys() {
        match key.as_str() {
            "name" | "base" | "colors" | "ext_colors" => {}
            other => warnings.push(format!("theme {name}: unknown key {other:?}")),
        }
    }

    if let Some(colors) = table.get("colors") {
        match colors.as_table() {
            Some(colors) => {
                for (slot, val) in colors {
                    if slot == "dir_bold" {
                        match val.as_bool() {
                            Some(b) => theme.dir_bold = b,
                            None => warnings
                                .push(format!("theme {name}: dir_bold must be true or false")),
                        }
                        continue;
                    }
                    let Some(target) = theme.slot_mut(slot) else {
                        warnings.push(format!("theme {name}: unknown slot {slot:?}"));
                        continue;
                    };
                    match val.as_str().map(parse_color) {
                        Some(Ok(c)) => *target = c,
                        Some(Err(e)) => warnings.push(format!("theme {name}: slot {slot}: {e}")),
                        None => {
                            warnings.push(format!("theme {name}: slot {slot}: expected a string"))
                        }
                    }
                }
            }
            None => warnings.push(format!("theme {name}: [colors] must be a table")),
        }
    }

    if let Some(groups) = table.get("ext_colors") {
        match groups.as_table() {
            Some(groups) => {
                for (gname, g) in groups {
                    match parse_group(gname, g) {
                        Ok(group) => {
                            match theme.ext_colors.iter_mut().find(|x| x.name == group.name) {
                                Some(slot) => *slot = group,
                                None => theme.ext_colors.push(group),
                            }
                        }
                        Err(e) => warnings.push(format!("theme {name}: ext_colors.{gname}: {e}")),
                    }
                }
            }
            None => warnings.push(format!("theme {name}: [ext_colors] must be a table")),
        }
    }
    (theme, warnings)
}

fn parse_group(name: &str, v: &toml::Value) -> Result<ExtGroup, String> {
    let exts = v
        .get("exts")
        .and_then(|e| e.as_array())
        .ok_or("missing exts array")?
        .iter()
        .map(|e| {
            e.as_str()
                .map(|s| s.trim_start_matches('.').to_lowercase())
                .ok_or_else(|| "exts must be strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let color = v
        .get("color")
        .and_then(|c| c.as_str())
        .ok_or("missing color")?;
    Ok(ExtGroup {
        name: name.to_string(),
        exts,
        color: parse_color(color)?,
    })
}

const BUILTINS: [(&str, &str); 5] = [
    ("mdir", include_str!("../themes/mdir.toml")),
    ("mdir-red", include_str!("../themes/mdir-red.toml")),
    ("mdir-green", include_str!("../themes/mdir-green.toml")),
    ("dark", include_str!("../themes/dark.toml")),
    ("light", include_str!("../themes/light.toml")),
];

pub fn builtin_names() -> &'static [&'static str] {
    &["mdir", "mdir-red", "mdir-green", "dark", "light"]
}

pub fn builtin(name: &str) -> Option<Theme> {
    let (_, src) = BUILTINS.iter().find(|(n, _)| *n == name)?;
    // Built-in bases only point at other built-ins and form no cycles.
    Some(from_toml_named(src, name, builtin).0)
}

/// Load `*.toml` themes from `dir`. A missing directory is not an error.
/// A theme's `base` may be another user theme or a built-in (a user theme may
/// extend the built-in of the same name).
pub fn load_user_themes(dir: &Path) -> (Vec<Theme>, Vec<String>) {
    let mut warnings = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (Vec::new(), warnings);
    };
    let mut paths: Vec<_> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();

    // name -> (source, base)
    let mut sources: Vec<(String, String)> = Vec::new();
    for p in paths {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match std::fs::read_to_string(&p) {
            Ok(src) => match src.parse::<toml::Table>() {
                Ok(t) => {
                    let name = t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&stem)
                        .to_string();
                    sources.push((name, src));
                }
                Err(e) => warnings.push(format!("{}: invalid TOML: {e}", p.display())),
            },
            Err(e) => warnings.push(format!("{}: {e}", p.display())),
        }
    }

    let base_of = |src: &str| -> Option<String> {
        src.parse::<toml::Table>()
            .ok()?
            .get("base")?
            .as_str()
            .map(str::to_string)
    };
    let find = |n: &str| sources.iter().find(|(name, _)| name == n);

    // Cycle detection: follow user-theme bases; a base equal to the current
    // name refers to the built-in and ends the chain.
    let mut cyclic: Vec<String> = Vec::new();
    for (name, src) in &sources {
        let mut seen = vec![name.clone()];
        let mut cur = (name.clone(), base_of(src));
        while let Some(b) = cur.1 {
            if b == cur.0 {
                break;
            }
            if seen.contains(&b) {
                cyclic.push(name.clone());
                warnings.push(format!("theme {name}: base cycle via {b:?}, using mdir"));
                break;
            }
            let Some((bn, bs)) = find(&b) else { break };
            seen.push(bn.clone());
            cur = (bn.clone(), base_of(bs));
        }
    }

    fn build(
        name: &str,
        sources: &[(String, String)],
        cyclic: &[String],
        parent: Option<&str>,
        sink: &std::cell::RefCell<Vec<String>>,
    ) -> Option<Theme> {
        // A theme extending its own name means the built-in.
        if parent == Some(name) {
            return builtin(name);
        }
        // Not a user theme: fall through to the built-in of that name.
        let Some((_, src)) = sources.iter().find(|(n, _)| n == name) else {
            return builtin(name);
        };
        if cyclic.iter().any(|c| c == name) {
            let mut t = builtin("mdir")?;
            t.name = name.to_string();
            return Some(t);
        }
        let (t, w) = from_toml_named(src, name, |b| build(b, sources, cyclic, Some(name), sink));
        sink.borrow_mut().extend(w);
        Some(t)
    }

    let sink = std::cell::RefCell::new(Vec::new());
    let mut themes = Vec::new();
    for (name, _) in &sources {
        if let Some(t) = build(name, &sources, &cyclic, None, &sink) {
            themes.push(t);
        }
    }
    // Base resolution for names that are not user themes falls through to
    // built-ins.
    warnings.extend(sink.into_inner());
    (themes, warnings)
}

/// Pick a theme by name: user themes override built-ins; unknown -> mdir + warning.
pub fn resolve(name: &str, user: &[Theme]) -> (Theme, Vec<String>) {
    if let Some(t) = user.iter().find(|t| t.name == name) {
        return (t.clone(), Vec::new());
    }
    if let Some(t) = builtin(name) {
        return (t, Vec::new());
    }
    (
        builtin("mdir").expect("mdir is built in"),
        vec![format!("unknown theme {name:?}, using mdir")],
    )
}

/// Convert an RGB color to the nearest xterm-256 index (other colors pass through).
/// Candidates: the 6x6x6 cube (16..=231, levels 0,95,135,175,215,255) and the
/// grayscale ramp (232..=255, 8+10*i). Nearest by squared RGB distance; ties go
/// to the cube. The 16 system colors are skipped because terminals redefine them.
pub fn to_256(c: Color) -> Color {
    let Color::Rgb(r, g, b) = c else { return c };
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let dist = |(x, y, z): (i32, i32, i32)| (x - r).pow(2) + (y - g).pow(2) + (z - b).pow(2);
    let nearest = |v: i32| (0..6).min_by_key(|&i| (LEVELS[i] - v).abs()).unwrap_or(0);
    let (ci, cj, ck) = (nearest(r), nearest(g), nearest(b));
    let cube_d = dist((LEVELS[ci], LEVELS[cj], LEVELS[ck]));
    let cube_idx = 16 + 36 * ci + 6 * cj + ck;
    let avg = (r + g + b) / 3;
    let gi = (0..24)
        .min_by_key(|&i| (8 + 10 * i as i32 - avg).abs())
        .unwrap_or(0);
    let gv = 8 + 10 * gi as i32;
    if dist((gv, gv, gv)) < cube_d {
        Color::Indexed(232 + gi)
    } else {
        Color::Indexed(cube_idx as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(r, g, b)
    }

    #[test]
    fn parse_color_names() {
        assert_eq!(parse_color("black"), Ok(Color::Black));
        assert_eq!(parse_color("Red"), Ok(Color::Red));
        assert_eq!(parse_color("GREEN"), Ok(Color::Green));
        assert_eq!(parse_color("yellow"), Ok(Color::Yellow));
        assert_eq!(parse_color("blue"), Ok(Color::Blue));
        assert_eq!(parse_color("magenta"), Ok(Color::Magenta));
        assert_eq!(parse_color("cyan"), Ok(Color::Cyan));
        assert_eq!(parse_color("gray"), Ok(Color::Gray));
        assert_eq!(parse_color("grey"), Ok(Color::Gray));
        assert_eq!(parse_color("darkgray"), Ok(Color::DarkGray));
        assert_eq!(parse_color("dark_gray"), Ok(Color::DarkGray));
        assert_eq!(parse_color("lightred"), Ok(Color::LightRed));
        assert_eq!(parse_color("lightgreen"), Ok(Color::LightGreen));
        assert_eq!(parse_color("lightyellow"), Ok(Color::LightYellow));
        assert_eq!(parse_color("lightblue"), Ok(Color::LightBlue));
        assert_eq!(parse_color("lightmagenta"), Ok(Color::LightMagenta));
        assert_eq!(parse_color("lightcyan"), Ok(Color::LightCyan));
        assert_eq!(parse_color("white"), Ok(Color::White));
        assert_eq!(parse_color("reset"), Ok(Color::Reset));
        assert_eq!(parse_color("  White "), Ok(Color::White));
    }

    #[test]
    fn parse_color_hex() {
        assert_eq!(parse_color("#0000AA"), Ok(rgb(0, 0, 0xAA)));
        assert_eq!(parse_color("#ffaa00"), Ok(rgb(0xFF, 0xAA, 0)));
        assert_eq!(parse_color("#1e3a8a"), Ok(rgb(0x1E, 0x3A, 0x8A)));
        assert_eq!(parse_color("#F80"), Ok(rgb(0xFF, 0x88, 0)));
        assert_eq!(parse_color("#abc"), Ok(rgb(0xAA, 0xBB, 0xCC)));
    }

    #[test]
    fn parse_color_indexed() {
        assert_eq!(parse_color("0"), Ok(Color::Indexed(0)));
        assert_eq!(parse_color("236"), Ok(Color::Indexed(236)));
        assert_eq!(parse_color("255"), Ok(Color::Indexed(255)));
    }

    #[test]
    fn parse_color_errors() {
        for bad in [
            "", "#", "#12", "#12345", "#1234567", "#GGGGGG", "256", "-1", "purple", "#00 000",
            "1.5", "#+12", "#é12",
        ] {
            assert!(parse_color(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn mdir_matches_ui_palette() {
        // Expected values copied from ui.rs `mod palette`.
        let t = builtin("mdir").unwrap();
        assert_eq!(t.name, "mdir");
        assert_eq!(t.bg, rgb(0x00, 0x00, 0xAA));
        assert_eq!(t.fg, rgb(0xAA, 0xAA, 0xAA));
        assert_eq!(t.border, rgb(0x55, 0xFF, 0xFF));
        assert_eq!(t.border_active, rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(t.dir, rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(t.exec, rgb(0x55, 0xFF, 0x55));
        assert_eq!(t.hidden, rgb(0x55, 0x55, 0x55));
        assert_eq!(t.selected, rgb(0xFF, 0xFF, 0x55));
        assert_eq!(t.cursor_bg, rgb(0x00, 0xAA, 0xAA));
        assert_eq!(t.cursor_fg, rgb(0x00, 0x00, 0x00));
        assert_eq!(t.status_bg, rgb(0x00, 0xAA, 0xAA));
        assert_eq!(t.status_fg, rgb(0x00, 0x00, 0x00));
        assert_eq!(t.fkey_num_fg, rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(t.fkey_num_bg, rgb(0x00, 0x00, 0x00));
        assert_eq!(t.fkey_label_fg, rgb(0x00, 0x00, 0x00));
        assert_eq!(t.fkey_label_bg, rgb(0x00, 0xAA, 0xAA));
        assert_eq!(t.error, rgb(0xFF, 0x55, 0x55));
        assert_eq!(t.file, rgb(0xAA, 0xAA, 0xAA));
        assert_eq!(t.assoc_file, rgb(0x55, 0xFF, 0xFF));
        assert_eq!(t.dialog_bg, rgb(0xAA, 0xAA, 0xAA));
        assert_eq!(t.dialog_fg, rgb(0, 0, 0));
        assert!(t.dir_bold);
        assert_eq!(t.color_for_ext("a.sh"), Some(rgb(0x55, 0xFF, 0x55)));
        assert_eq!(t.color_for_ext("a.zip"), Some(rgb(0xFF, 0x55, 0xFF)));
        assert_eq!(t.color_for_ext("a.md"), Some(rgb(0x55, 0xFF, 0xFF)));
    }

    #[test]
    fn inherits_missing_slots_from_base() {
        let src = "name = \"x\"\nbase = \"mdir\"\n[colors]\nbg = \"red\"\n";
        let (t, w) = from_toml(src, builtin);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.name, "x");
        assert_eq!(t.bg, Color::Red);
        assert_eq!(t.fg, builtin("mdir").unwrap().fg);
        assert_eq!(t.ext_colors, builtin("mdir").unwrap().ext_colors);
    }

    #[test]
    fn default_base_is_mdir() {
        let (t, w) = from_toml("name = \"x\"\n", builtin);
        assert!(w.is_empty(), "{w:?}");
        let mut expect = builtin("mdir").unwrap();
        expect.name = "x".into();
        assert_eq!(t, expect);
    }

    #[test]
    fn custom_base_is_used() {
        let (t, _) = from_toml("name = \"x\"\nbase = \"dark\"\n", builtin);
        assert_eq!(t.bg, builtin("dark").unwrap().bg);
    }

    #[test]
    fn invalid_color_warns_and_inherits() {
        let src = "[colors]\nbg = \"notacolor\"\nfg = \"#00FF00\"\n";
        let (t, w) = from_toml(src, builtin);
        assert_eq!(t.bg, builtin("mdir").unwrap().bg);
        assert_eq!(t.fg, rgb(0, 0xFF, 0));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bg"), "{w:?}");
    }

    #[test]
    fn unknown_slot_warns() {
        let (t, w) = from_toml("[colors]\nbogus = \"red\"\n", builtin);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bogus"));
        assert_eq!(t.bg, builtin("mdir").unwrap().bg);
    }

    #[test]
    fn unknown_base_falls_back_to_mdir_with_warning() {
        let (t, w) = from_toml("base = \"nope\"\n", builtin);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("nope"));
        assert_eq!(t.bg, builtin("mdir").unwrap().bg);
    }

    #[test]
    fn invalid_toml_warns_and_yields_mdir() {
        let (t, w) = from_toml("this is = = not toml", builtin);
        assert_eq!(w.len(), 1);
        assert_eq!(t.bg, builtin("mdir").unwrap().bg);
    }

    #[test]
    fn ext_group_override_and_add() {
        let src = "[ext_colors.archive]\nexts = [\"ZIP\", \".Rar\"]\ncolor = \"red\"\n\
                   [ext_colors.img]\nexts = [\"png\"]\ncolor = \"#F80\"\n";
        let (t, w) = from_toml(src, builtin);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.color_for_ext("a.zip"), Some(Color::Red));
        assert_eq!(t.color_for_ext("a.rar"), Some(Color::Red));
        assert_eq!(t.color_for_ext("a.tar"), None); // replaced group
        assert_eq!(t.color_for_ext("x.PNG"), Some(rgb(0xFF, 0x88, 0)));
        assert_eq!(t.color_for_ext("a.sh"), Some(rgb(0x55, 0xFF, 0x55)));
    }

    #[test]
    fn ext_group_invalid_color_warns() {
        let src = "[ext_colors.img]\nexts = [\"png\"]\ncolor = \"zzz\"\n";
        let (t, w) = from_toml(src, builtin);
        assert_eq!(w.len(), 1);
        assert_eq!(t.color_for_ext("a.png"), None);
    }

    #[test]
    fn dir_bold_can_be_disabled() {
        let (t, w) = from_toml("[colors]\ndir_bold = false\n", builtin);
        assert!(w.is_empty(), "{w:?}");
        assert!(!t.dir_bold);
    }

    #[test]
    fn color_for_ext_rules() {
        let t = builtin("mdir").unwrap();
        assert_eq!(t.color_for_ext("a.tar.gz"), Some(rgb(0xFF, 0x55, 0xFF)));
        assert_eq!(t.color_for_ext("README.MD"), Some(rgb(0x55, 0xFF, 0xFF)));
        assert_eq!(t.color_for_ext("noext"), None);
        assert_eq!(t.color_for_ext(".zip"), None); // dotfile, no extension
        assert_eq!(t.color_for_ext("trailing."), None);
        assert_eq!(t.color_for_ext("한글 파일.rs"), Some(rgb(0x55, 0xFF, 0xFF)));
    }

    #[test]
    fn builtins_load_without_warnings() {
        let names = builtin_names();
        for n in ["mdir", "mdir-red", "mdir-green", "dark", "light"] {
            assert!(names.contains(&n), "{n}");
        }
        for n in names {
            let t = builtin(n).unwrap_or_else(|| panic!("{n} missing"));
            assert_eq!(&t.name, n);
            let (_, w) = resolve(n, &[]);
            assert!(w.is_empty(), "{n}: {w:?}");
        }
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn builtin_mdir_defines_every_slot() {
        // mdir has no base: no slot may be left at the Reset placeholder.
        let t = builtin("mdir").unwrap();
        for c in [
            t.bg,
            t.fg,
            t.border,
            t.border_active,
            t.title,
            t.dir,
            t.file,
            t.exec,
            t.symlink,
            t.hidden,
            t.assoc_file,
            t.selected,
            t.cursor_bg,
            t.cursor_fg,
            t.status_bg,
            t.status_fg,
            t.fkey_num_fg,
            t.fkey_num_bg,
            t.fkey_label_fg,
            t.fkey_label_bg,
            t.dialog_bg,
            t.dialog_fg,
            t.danger_bg,
            t.danger_fg,
            t.preview_bg,
            t.error,
        ] {
            assert_ne!(c, Color::Reset);
        }
    }

    #[test]
    fn variants_differ_from_mdir() {
        let m = builtin("mdir").unwrap();
        assert_eq!(builtin("mdir-red").unwrap().bg, rgb(0xAA, 0, 0));
        assert_eq!(builtin("mdir-green").unwrap().bg, rgb(0, 0xAA, 0));
        assert_eq!(builtin("mdir-red").unwrap().fg, m.fg);
        assert_ne!(builtin("dark").unwrap().bg, m.bg);
        assert_ne!(builtin("light").unwrap().bg, builtin("dark").unwrap().bg);
    }

    #[test]
    fn load_user_themes_and_override() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("mine.toml"),
            "base = \"dark\"\n[colors]\nbg = \"#010203\"\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("dark.toml"),
            "name = \"dark\"\n[colors]\nbg = \"#FF0000\"\n",
        )
        .unwrap();
        std::fs::write(d.path().join("readme.txt"), "ignored").unwrap();
        std::fs::write(d.path().join("broken.toml"), "= = =").unwrap();
        let (themes, warnings) = load_user_themes(d.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let mut names: Vec<_> = themes.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["dark", "mine"]);
        let mine = themes.iter().find(|t| t.name == "mine").unwrap();
        assert_eq!(mine.bg, rgb(1, 2, 3)); // name from file stem
        // "mine" extends the user "dark" (which overrides builtin dark and inherits mdir).
        assert_eq!(mine.fg, builtin("mdir").unwrap().fg);
        let (t, w) = resolve("dark", &themes);
        assert!(w.is_empty());
        assert_eq!(t.bg, rgb(0xFF, 0, 0)); // user overrides built-in
        let (t, _) = resolve("light", &themes);
        assert_eq!(t, builtin("light").unwrap());
    }

    #[test]
    fn user_theme_can_extend_a_builtin_it_does_not_override() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("mine.toml"),
            "base = \"dark\"\n[colors]\nbg = \"#010203\"\n",
        )
        .unwrap();
        let (themes, warnings) = load_user_themes(d.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mine = &themes[0];
        assert_eq!(mine.bg, rgb(1, 2, 3));
        assert_eq!(mine.fg, builtin("dark").unwrap().fg);
    }

    #[test]
    fn user_theme_can_extend_user_theme() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.toml"),
            "base = \"b\"\n[colors]\nfg = \"red\"\n",
        )
        .unwrap();
        std::fs::write(d.path().join("b.toml"), "[colors]\nbg = \"blue\"\n").unwrap();
        let (themes, w) = load_user_themes(d.path());
        assert!(w.is_empty(), "{w:?}");
        let a = themes.iter().find(|t| t.name == "a").unwrap();
        assert_eq!((a.fg, a.bg), (Color::Red, Color::Blue));
    }

    #[test]
    fn base_cycle_warns_and_uses_mdir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.toml"), "base = \"b\"\n").unwrap();
        std::fs::write(d.path().join("b.toml"), "base = \"a\"\n").unwrap();
        let (themes, w) = load_user_themes(d.path());
        assert!(w.iter().any(|m| m.contains("cycle")), "{w:?}");
        assert_eq!(themes.len(), 2);
        for t in themes {
            assert_eq!(t.bg, builtin("mdir").unwrap().bg);
        }
    }

    #[test]
    fn missing_user_dir_is_empty_without_warning() {
        let d = tempfile::tempdir().unwrap();
        let (t, w) = load_user_themes(&d.path().join("nope"));
        assert!(t.is_empty() && w.is_empty());
    }

    #[test]
    fn resolve_unknown_falls_back_with_warning() {
        let (t, w) = resolve("ghost", &[]);
        assert_eq!(t, builtin("mdir").unwrap());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("ghost"));
    }

    #[test]
    fn to_256_known_colors() {
        // Rule: candidates are the 6x6x6 cube (16..=231, levels 0,95,135,175,215,255)
        // and the grayscale ramp (232..=255, 8+10*i); nearest by squared RGB distance,
        // ties go to the cube. The 16 system colors are never chosen (terminal-defined).
        assert_eq!(to_256(rgb(0x00, 0x00, 0xAA)), Color::Indexed(19));
        assert_eq!(to_256(rgb(0xFF, 0xFF, 0xFF)), Color::Indexed(231));
        assert_eq!(to_256(rgb(0x00, 0x00, 0x00)), Color::Indexed(16));
        assert_eq!(to_256(rgb(0xAA, 0xAA, 0xAA)), Color::Indexed(248));
        assert_eq!(to_256(rgb(0x55, 0x55, 0x55)), Color::Indexed(240));
        assert_eq!(to_256(rgb(0xFF, 0x55, 0x55)), Color::Indexed(203));
        assert_eq!(to_256(rgb(0x00, 0xAA, 0xAA)), Color::Indexed(37));
    }

    #[test]
    fn to_256_passes_non_rgb_through() {
        assert_eq!(to_256(Color::Red), Color::Red);
        assert_eq!(to_256(Color::Indexed(7)), Color::Indexed(7));
        assert_eq!(to_256(Color::Reset), Color::Reset);
    }
}
