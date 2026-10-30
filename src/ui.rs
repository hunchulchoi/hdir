//! Layout rendering (pure: draws a `View` into a ratatui `Frame`, no IO).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::panel::{Entry, EntryKind, Panel};
use crate::trash::{TrashEntry, TrashView, format_local};

/// Mdir DOS palette. Centralized so a later theme issue can replace it.
mod palette {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(0x00, 0x00, 0xAA);
    pub const FG: Color = Color::Rgb(0xAA, 0xAA, 0xAA);
    pub const BORDER: Color = Color::Rgb(0x55, 0xFF, 0xFF);
    pub const BORDER_ACTIVE: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
    pub const DIR: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
    pub const EXEC: Color = Color::Rgb(0x55, 0xFF, 0x55);
    pub const HIDDEN: Color = Color::Rgb(0x55, 0x55, 0x55);
    pub const SELECTED: Color = Color::Rgb(0xFF, 0xFF, 0x55);
    pub const CURSOR_BG: Color = Color::Rgb(0x00, 0xAA, 0xAA);
    pub const CURSOR_FG: Color = Color::Rgb(0x00, 0x00, 0x00);
    pub const STATUS_BG: Color = Color::Rgb(0x00, 0xAA, 0xAA);
    pub const STATUS_FG: Color = Color::Rgb(0x00, 0x00, 0x00);
    pub const FKEY_NUM_FG: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
    pub const FKEY_NUM_BG: Color = Color::Rgb(0x00, 0x00, 0x00);
    pub const FKEY_LABEL_FG: Color = Color::Rgb(0x00, 0x00, 0x00);
    pub const FKEY_LABEL_BG: Color = Color::Rgb(0x00, 0xAA, 0xAA);
    pub const ERROR: Color = Color::Rgb(0xFF, 0x55, 0x55);
}

pub struct View<'a> {
    pub panels: [&'a Panel; 2],
    pub active: usize,
    pub status: Option<&'a str>,
    pub shell: &'a str,
    pub os: &'a str,
    /// A panel showing the trash instead of its directory.
    pub trash: [Option<&'a TrashView>; 2],
}

const FKEYS: [(&str, &str); 10] = [
    ("1", "Help"),
    ("2", "Rename"),
    ("3", "View"),
    ("4", "Edit"),
    ("5", "Copy"),
    ("6", "Move"),
    ("7", "Mkdir"),
    ("8", "Del"),
    ("9", "Menu"),
    ("10", "Quit"),
];

/// F-key bar while a panel shows the trash; unavailable keys stay blank.
const TRASH_FKEYS: [(&str, &str); 10] = [
    ("1", ""),
    ("2", ""),
    ("3", ""),
    ("4", ""),
    ("5", "Restore"),
    ("6", ""),
    ("7", ""),
    ("8", "Purge"),
    ("Esc", "Back"),
    ("10", "Quit"),
];

/// Offset that keeps `cursor` within a window of `height` rows, starting from `scroll`.
pub fn visible_offset(cursor: usize, scroll: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    if cursor < scroll {
        cursor
    } else if cursor >= scroll + height {
        cursor + 1 - height
    } else {
        scroll
    }
}

/// Truncate (with `…`) or right-pad `s` to exactly `width` display columns.
pub fn fit_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    if s.width() <= width {
        out.push_str(s);
        used = s.width();
    } else if width > 0 {
        for c in s.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width - 1 {
                break;
            }
            out.push(c);
            used += w;
        }
        out.push('…');
        used += 1;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Keep the tail of `s` within `width` columns, prefixed with `…` when cut.
fn tail_width(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        tail.push(c);
        used += w;
    }
    tail.push('…');
    tail.reverse();
    tail.into_iter().collect()
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 3] = ["K", "M", "G"];
    if size < 1024 {
        return format!("{size}B");
    }
    let mut v = size as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1}{}", UNITS[i])
}

/// `path` with a leading home directory shortened to `~`.
fn tilde(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let h = home.display().to_string();
        if s == h {
            return "~".into();
        }
        if let Some(rest) = s.strip_prefix(&format!("{h}/")) {
            return format!("~/{rest}");
        }
    }
    s
}

fn title_for(panel: &Panel, width: usize) -> String {
    let mut s = tilde(&panel.cwd);
    if panel.loading.is_some() {
        s.push_str(" 읽는 중…");
    }
    tail_width(&s, width)
}

fn is_dirlike(e: &Entry) -> bool {
    matches!(
        e.kind,
        EntryKind::Dir
            | EntryKind::Symlink {
                target_is_dir: true
            }
    )
}

fn entry_style(e: &Entry, selected: bool) -> Style {
    let base = Style::default().bg(palette::BG);
    if selected {
        return base.fg(palette::SELECTED);
    }
    if is_dirlike(e) || e.kind == EntryKind::Parent {
        return base.fg(palette::DIR).add_modifier(Modifier::BOLD);
    }
    if e.hidden {
        return base.fg(palette::HIDDEN);
    }
    if e.kind == EntryKind::File && e.mode & 0o111 != 0 {
        return base.fg(palette::EXEC);
    }
    base.fg(palette::FG)
}

fn row_text(e: &Entry, selected: bool, width: usize) -> String {
    let right = if e.kind == EntryKind::Parent {
        "<UP-DIR>".to_string()
    } else if is_dirlike(e) {
        "<DIR>".to_string()
    } else {
        human_size(e.size)
    };
    let name = format!("{}{}", if selected { "*" } else { " " }, e.name);
    if width < 10 {
        return fit_width(&name, width);
    }
    format!("{} {:>8}", fit_width(&name, width - 9), right)
}

fn draw_panel(frame: &mut Frame, area: Rect, panel: &Panel, active: bool) {
    let border = if active {
        palette::BORDER_ACTIVE
    } else {
        palette::BORDER
    };
    let title = title_for(panel, (area.width as usize).saturating_sub(4));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border).bg(palette::BG))
        .title(title)
        .style(Style::default().fg(palette::FG).bg(palette::BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let w = inner.width as usize;
    let mut rows = inner.height as usize;
    let err_row = panel.error.is_some() && rows > 0;
    if err_row {
        rows -= 1;
    }
    let offset = visible_offset(panel.cursor, panel.scroll, rows);
    let mut lines: Vec<Line> = panel
        .entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
        .map(|(i, e)| {
            let sel = panel.selected.contains(&e.os_name);
            let mut style = entry_style(e, sel);
            if active && i == panel.cursor {
                style = style.bg(palette::CURSOR_BG).fg(palette::CURSOR_FG);
            }
            Line::from(Span::styled(row_text(e, sel, w), style))
        })
        .collect();
    if let (true, Some(err)) = (err_row, &panel.error) {
        while lines.len() < rows {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            fit_width(err, w),
            Style::default().fg(palette::ERROR).bg(palette::BG),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

const TIME_W: usize = 16;
const SIZE_W: usize = 7;

fn trash_row_text(e: &TrashEntry, selected: bool, width: usize) -> String {
    let mark = if selected { "*" } else { " " };
    let size = if e.is_dir {
        "<DIR>".to_string()
    } else {
        e.size.map_or_else(|| "-".to_string(), human_size)
    };
    let time = e.deleted_at.map_or_else(|| "-".to_string(), format_local);
    // mark + name + ' ' + time + ' ' + size
    let fixed = 1 + 1 + TIME_W + 1 + SIZE_W;
    if width < fixed + 4 {
        return fit_width(&format!("{mark}{}", e.name), width);
    }
    let time = fit_width(&time, TIME_W);
    // Room for a parent column only when both columns stay readable.
    if width >= fixed + 1 + 28 {
        let rest = width - fixed - 1;
        let name_w = rest / 2;
        let parent = e
            .original_parent
            .as_deref()
            .map_or_else(|| "-".to_string(), tilde);
        return format!(
            "{mark}{} {} {time} {size:>SIZE_W$}",
            fit_width(&e.name, name_w),
            fit_width(&tail_width(&parent, rest - name_w), rest - name_w),
        );
    }
    format!(
        "{mark}{} {time} {size:>SIZE_W$}",
        fit_width(&e.name, width - fixed)
    )
}

fn draw_trash_panel(frame: &mut Frame, area: Rect, tv: &TrashView, active: bool) {
    let border = if active {
        palette::BORDER_ACTIVE
    } else {
        palette::BORDER
    };
    let mut title = "휴지통".to_string();
    if tv.loading {
        title.push_str(" 읽는 중…");
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border).bg(palette::BG))
        .title(tail_width(&title, (area.width as usize).saturating_sub(4)))
        .style(Style::default().fg(palette::FG).bg(palette::BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let w = inner.width as usize;
    let mut rows = inner.height as usize;
    let err_row = tv.error.is_some() && rows > 0;
    if err_row {
        rows -= 1;
    }
    let offset = visible_offset(tv.cursor, tv.scroll, rows);
    let mut lines: Vec<Line> = tv
        .entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
        .map(|(i, e)| {
            let sel = tv.selected.contains(&e.id);
            let mut style = Style::default().bg(palette::BG).fg(if sel {
                palette::SELECTED
            } else if e.is_dir {
                palette::DIR
            } else {
                palette::FG
            });
            if e.is_dir {
                style = style.add_modifier(Modifier::BOLD);
            }
            if active && i == tv.cursor {
                style = style.bg(palette::CURSOR_BG).fg(palette::CURSOR_FG);
            }
            Line::from(Span::styled(trash_row_text(e, sel, w), style))
        })
        .collect();
    if let (true, Some(err)) = (err_row, &tv.error) {
        while lines.len() < rows {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            fit_width(err, w),
            Style::default().fg(palette::ERROR).bg(palette::BG),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn trash_status(tv: &TrashView) -> String {
    if tv.loading {
        return "읽는 중…".to_string();
    }
    let place = match tv.current() {
        Some(e) => match e.original_path() {
            Some(p) => format!("원래 위치: {} · ", tilde(&p)),
            None => "원래 위치: 알 수 없음 · ".to_string(),
        },
        None => String::new(),
    };
    let mut s = format!("{place}휴지통 {}개", tv.entries.len());
    if !tv.selected.is_empty() {
        s.push_str(&format!(" · 선택 {}개", tv.selected.len()));
    }
    s
}

fn default_status(panel: &Panel) -> String {
    if panel.loading.is_some() {
        return "읽는 중…".to_string();
    }
    let dirs = panel.entries.iter().filter(|e| is_dirlike(e)).count();
    let files = panel
        .entries
        .iter()
        .filter(|e| e.kind != EntryKind::Parent && !is_dirlike(e))
        .count();
    format!("{files} files · {dirs} dirs")
}

fn fkey_line(width: usize, keys: &[(&str, &str); 10]) -> Line<'static> {
    let label_style = Style::default()
        .fg(palette::FKEY_LABEL_FG)
        .bg(palette::FKEY_LABEL_BG);
    let num_style = Style::default()
        .fg(palette::FKEY_NUM_FG)
        .bg(palette::FKEY_NUM_BG);
    let slot = width / keys.len();
    let mut spans = Vec::new();
    for (i, (n, l)) in keys.iter().enumerate() {
        let w = if i == keys.len() - 1 {
            width - slot * i
        } else {
            slot
        };
        spans.push(Span::styled((*n).to_string(), num_style));
        spans.push(Span::styled(
            fit_width(l, w.saturating_sub(n.len())),
            label_style,
        ));
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let header = format!(
        " ■ {} {} · {} · {} · UTF-8",
        crate::APP_NAME,
        crate::VERSION,
        view.shell,
        view.os
    );
    frame.render_widget(
        Paragraph::new(fit_width(&header, area.width as usize))
            .style(Style::default().fg(palette::FG).bg(palette::BG)),
        rows[0],
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    for i in 0..2 {
        match view.trash[i] {
            Some(tv) => draw_trash_panel(frame, cols[i], tv, view.active == i),
            None => draw_panel(frame, cols[i], view.panels[i], view.active == i),
        }
    }

    let active = view.active.min(1);
    let status = match (view.status, view.trash[active]) {
        (Some(s), _) => s.to_string(),
        (None, Some(tv)) => trash_status(tv),
        (None, None) => default_status(view.panels[active]),
    };
    frame.render_widget(
        Paragraph::new(fit_width(&format!(" {status}"), area.width as usize)).style(
            Style::default()
                .fg(palette::STATUS_FG)
                .bg(palette::STATUS_BG),
        ),
        rows[2],
    );
    let keys = if view.trash[active].is_some() {
        &TRASH_FKEYS
    } else {
        &FKEYS
    };
    frame.render_widget(
        Paragraph::new(fkey_line(area.width as usize, keys)),
        rows[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn fit_width_truncates_ascii() {
        assert_eq!(fit_width("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn fit_width_pads_exact() {
        assert_eq!(fit_width("ab", 5), "ab   ");
        assert_eq!(fit_width("abcde", 5), "abcde");
        assert_eq!(fit_width("abc", 0), "");
    }

    #[test]
    fn fit_width_korean() {
        let r = fit_width("한글파일.txt", 6);
        assert_eq!(r.width(), 6);
        assert_eq!(r, "한글… ");
        assert_eq!(fit_width("한글", 5).width(), 5);
    }

    #[test]
    fn visible_offset_cases() {
        assert_eq!(visible_offset(10, 0, 5), 6);
        assert_eq!(visible_offset(2, 5, 5), 2);
        assert_eq!(visible_offset(7, 5, 5), 5);
        assert_eq!(visible_offset(3, 2, 0), 0);
    }

    #[test]
    fn human_size_format() {
        assert_eq!(human_size(10), "10B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    fn render(a: &Panel, b: &Panel, active: usize, status: Option<&str>, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let view = View {
            panels: [a, b],
            active,
            status,
            shell: "zsh",
            os: "macos",
            trash: [None, None],
        };
        t.draw(|f| draw(f, &view)).unwrap();
        t.backend().buffer().clone()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::create_dir(a.path().join("sub")).unwrap();
        fs::write(a.path().join("f1.txt"), "hello").unwrap();
        fs::write(a.path().join("f2.txt"), "hello").unwrap();
        fs::create_dir(b.path().join("sub")).unwrap();
        (a, b)
    }

    #[test]
    fn draw_basic_layout() {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let buf = render(&pa, &pb, 0, None, 100, 24);
        let all: String = (0..24).map(|y| row(&buf, y)).collect();
        assert!(row(&buf, 0).contains("hdir"));
        assert!(all.contains("<DIR>"));
        assert!(all.contains("<UP-DIR>"));
        assert!(row(&buf, 23).contains("Copy"));
        assert!(row(&buf, 23).contains("Quit"));
        assert!(row(&buf, 22).contains("2 files · 1 dirs"));
    }

    #[test]
    fn titles_present() {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let buf = render(&pa, &pb, 0, None, 250, 24);
        let top = row(&buf, 1);
        assert!(top.contains(&a.path().display().to_string()));
        assert!(top.contains(&b.path().display().to_string()));
    }

    #[test]
    fn cursor_only_in_active_panel() {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let buf = render(&pa, &pb, 0, None, 80, 24);
        assert_eq!(buf[(5, 2)].bg, palette::CURSOR_BG);
        assert_ne!(buf[(45, 2)].bg, palette::CURSOR_BG);
        let buf = render(&pa, &pb, 1, None, 80, 24);
        assert_ne!(buf[(5, 2)].bg, palette::CURSOR_BG);
        assert_eq!(buf[(45, 2)].bg, palette::CURSOR_BG);
    }

    #[test]
    fn custom_status_and_error() {
        let (a, b) = setup();
        let mut pa = Panel::new(a.path());
        pa.error = Some("boom failure".into());
        let pb = Panel::new(b.path());
        let buf = render(&pa, &pb, 0, Some("custom msg"), 80, 24);
        assert!(row(&buf, 22).contains("custom msg"));
        assert!(row(&buf, 20).contains("boom failure"));
    }

    fn fixed_panel(cwd: &str, names: &[(&str, EntryKind, u64)]) -> Panel {
        let mut entries = vec![Entry {
            name: "..".into(),
            os_name: "..".into(),
            kind: EntryKind::Parent,
            size: 0,
            modified: None,
            mode: 0,
            hidden: false,
        }];
        for (n, kind, size) in names {
            entries.push(Entry {
                name: (*n).into(),
                os_name: (*n).into(),
                kind: kind.clone(),
                size: *size,
                modified: None,
                mode: 0o644,
                hidden: false,
            });
        }
        Panel {
            cwd: cwd.into(),
            entries,
            cursor: 0,
            scroll: 0,
            show_hidden: false,
            selected: Default::default(),
            error: None,
            loading: None,
            reload_after_load: false,
        }
    }

    #[test]
    fn full_buffer_snapshot_60x12() {
        let mut pa = fixed_panel(
            "/tmp/a",
            &[
                ("src", EntryKind::Dir, 0),
                ("a.txt", EntryKind::File, 5),
                ("b.bin", EntryKind::File, 1536),
            ],
        );
        pa.selected.insert("b.bin".into());
        let pb = fixed_panel("/tmp/b", &[("docs", EntryKind::Dir, 0)]);
        let buf = render(&pa, &pb, 0, None, 60, 12);
        let got: Vec<String> = (0..12).map(|y| row(&buf, y)).collect();
        let expected = [
            format!(" ■ hdir {} · zsh · macos · UTF-8", crate::VERSION),
            "┌/tmp/a──────────────────────┐┌/tmp/b──────────────────────┐".to_string(),
            "│ ..                 <UP-DIR>││ ..                 <UP-DIR>│".to_string(),
            "│ src                   <DIR>││ docs                  <DIR>│".to_string(),
            "│ a.txt                    5B││                            │".to_string(),
            "│*b.bin                  1.5K││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "└────────────────────────────┘└────────────────────────────┘".to_string(),
            " 2 files · 1 dirs".to_string(),
            "1Help 2Rena…3View 4Edit 5Copy 6Move 7Mkdir8Del  9Menu 10Quit".to_string(),
        ]
        .map(|l| fit_width(&l, 60));
        assert_eq!(got.join("\n"), expected.join("\n"));

        // active cursor row, directory, and selected styles
        assert_eq!(buf[(2, 2)].bg, palette::CURSOR_BG);
        assert_eq!(buf[(2, 2)].fg, palette::CURSOR_FG);
        assert_eq!(buf[(2, 3)].fg, palette::DIR);
        assert_eq!(buf[(2, 3)].bg, palette::BG);
        assert_eq!(buf[(2, 5)].fg, palette::SELECTED);
        // inactive panel has no cursor background
        assert_ne!(buf[(32, 2)].bg, palette::CURSOR_BG);
    }

    #[test]
    fn long_korean_name_stays_inside_border() {
        let (a, b) = setup();
        fs::write(
            a.path()
                .join("아주아주아주긴한글파일이름입니다아주아주아주긴한글파일이름.txt"),
            "x",
        )
        .unwrap();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let buf = render(&pa, &pb, 0, None, 80, 24);
        for y in 2..21 {
            assert_eq!(buf[(39, y)].symbol(), "│", "row {y}: {}", row(&buf, y));
        }
    }

    #[test]
    fn loading_panel_snapshot_60x12() {
        let mut pa = fixed_panel("/tmp/a", &[("src", EntryKind::Dir, 0)]);
        pa.loading = Some(crate::panel::LoadRequest {
            id: 1,
            dir: "/tmp/a".into(),
            focus: None,
            kind: crate::panel::LoadKind::Enter,
        });
        let pb = fixed_panel("/tmp/b", &[("docs", EntryKind::Dir, 0)]);
        let buf = render(&pa, &pb, 0, None, 60, 12);
        let got: Vec<String> = (0..12).map(|y| row(&buf, y)).collect();
        // Wide glyphs leave blank continuation cells in the buffer.
        let expected = [
            format!(" ■ hdir {} · zsh · macos · UTF-8", crate::VERSION),
            "┌/tmp/a 읽 는  중 …─────────────┐┌/tmp/b──────────────────────┐".to_string(),
            "│ ..                 <UP-DIR>││ ..                 <UP-DIR>│".to_string(),
            "│ src                   <DIR>││ docs                  <DIR>│".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "│                            ││                            │".to_string(),
            "└────────────────────────────┘└────────────────────────────┘".to_string(),
            " 읽 는  중 …".to_string(),
            "1Help 2Rena…3View 4Edit 5Copy 6Move 7Mkdir8Del  9Menu 10Quit".to_string(),
        ]
        .map(|l| format!("{l:<60}"));
        assert_eq!(got.join("\n"), expected.join("\n"));

        // Marker styles: title in the active border colors, status bar colors.
        for x in [8, 10, 13, 15] {
            assert_eq!(buf[(x, 1)].fg, palette::BORDER_ACTIVE, "title x={x}");
            assert_eq!(buf[(x, 1)].bg, palette::BG, "title x={x}");
        }
        for x in [1, 3, 6, 8] {
            assert_eq!(buf[(x, 10)].fg, palette::STATUS_FG, "status x={x}");
            assert_eq!(buf[(x, 10)].bg, palette::STATUS_BG, "status x={x}");
        }
        // The idle panel carries no marker.
        assert!(!row(&buf, 1).chars().skip(30).any(|c| c == '읽'));
    }

    // ---- HDIR-62: trash view ----

    use crate::trash::{TrashEntry, TrashView, format_local};

    const TS: i64 = 1_700_000_000;

    fn tentry(name: &str, parent: Option<&str>, at: Option<i64>, size: Option<u64>) -> TrashEntry {
        TrashEntry {
            id: format!("id-{name}"),
            name: name.into(),
            original_parent: parent.map(PathBuf::from),
            deleted_at: at,
            size,
            is_dir: size.is_none() && at.is_some(),
        }
    }

    fn trash_view() -> TrashView {
        let mut v = TrashView::new(PathBuf::from("/prev"));
        v.set_entries(vec![
            tentry("report.pdf", Some("/srv/data"), Some(TS), Some(1536)),
            tentry("한글 문서", Some("/srv/data/a"), Some(TS + 60), None),
            tentry("stray", None, None, Some(3)),
        ]);
        v
    }

    fn render_trash(tv: &TrashView, active: usize, w: u16) -> Buffer {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (Panel::new(dir.path()), Panel::new(dir.path()));
        let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
        let view = View {
            panels: [&a, &b],
            active,
            status: None,
            shell: "zsh",
            os: "macos",
            trash: [Some(tv), None],
        };
        t.draw(|f| draw(f, &view)).unwrap();
        t.backend().buffer().clone()
    }

    /// Text of cells `x0..x1` with the filler cell after a wide char skipped.
    fn cells(buf: &Buffer, y: u16, x0: u16, x1: u16) -> String {
        let mut out = String::new();
        let mut x = x0;
        while x < x1 {
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            x += if sym.width() > 1 { 2 } else { 1 };
        }
        out
    }

    fn left_half(buf: &Buffer, y: u16) -> String {
        cells(buf, y, 0, 40)
    }

    fn trow(buf: &Buffer, y: u16) -> String {
        cells(buf, y, 0, buf.area.width)
    }

    #[test]
    fn trash_view_80x24_rows_snapshot() {
        let buf = render_trash(&trash_view(), 0, 80);
        let title = left_half(&buf, 1);
        assert!(title.starts_with("┌휴지통"), "{title}");
        assert_eq!(
            left_half(&buf, 2),
            format!("│ report.pdf   {}    1.5K│", format_local(TS))
        );
        assert!(left_half(&buf, 3).contains("<DIR>"));
        assert!(left_half(&buf, 3).contains(&format_local(TS + 60)));
        assert_eq!(
            left_half(&buf, 4),
            format!(
                "│ {} {} {:>7}│",
                fit_width("stray", 12),
                fit_width("-", 16),
                "3B"
            )
        );
        assert_eq!(left_half(&buf, 5), format!("│{}│", " ".repeat(38)));
        let keys = trow(&buf, 23);
        for want in ["5Restore", "8Purge", "EscBack", "10Quit"] {
            assert!(keys.contains(want), "{want} missing in {keys}");
        }
        assert!(!keys.contains("Copy") && !keys.contains("Mkdir"));
        // the other half is a normal panel
        assert!(cells(&buf, 2, 40, 80).contains(".."));
    }

    #[test]
    fn trash_view_cursor_selection_and_active_border() {
        let mut v = trash_view();
        v.toggle_current(); // select report.pdf, cursor -> 한글 문서
        let buf = render_trash(&v, 0, 80);
        assert!(left_half(&buf, 2).starts_with("│*report.pdf"));
        assert_eq!(buf[(20, 3)].bg, palette::CURSOR_BG);
        assert_ne!(buf[(20, 2)].bg, palette::CURSOR_BG);
        let inactive = render_trash(&v, 1, 80);
        assert_ne!(inactive[(20, 3)].bg, palette::CURSOR_BG);
        assert_eq!(buf[(0, 1)].fg, palette::BORDER_ACTIVE);
        assert_eq!(inactive[(0, 1)].fg, palette::BORDER);
    }

    #[test]
    fn trash_view_wide_shows_original_parent_column() {
        let buf = render_trash(&trash_view(), 0, 140);
        let r = trow(&buf, 2);
        assert!(r.contains("report.pdf") && r.contains("/srv/data"), "{r}");
        assert!(r.find("report.pdf") < r.find("/srv/data"));
        assert!(r.find("/srv/data") < r.find(&format_local(TS)));
    }

    #[test]
    fn trash_view_status_shows_original_path_of_current() {
        let mut v = trash_view();
        let buf = render_trash(&v, 0, 80);
        assert!(trow(&buf, 22).contains("원래 위치: /srv/data/report.pdf"));
        v.end();
        let buf = render_trash(&v, 0, 80);
        assert!(trow(&buf, 22).contains("원래 위치: 알 수 없음"));
    }

    #[test]
    fn trash_view_loading_and_error_and_empty() {
        let mut v = TrashView::new(PathBuf::from("/prev"));
        let buf = render_trash(&v, 0, 80);
        assert!(left_half(&buf, 1).contains("휴지통 읽는 중…"));
        assert!(trow(&buf, 22).contains("읽는 중…"));
        v.set_entries(vec![]);
        v.error = Some("휴지통 접근 권한 없음".into());
        let buf = render_trash(&v, 0, 80);
        assert!(!left_half(&buf, 1).contains("읽는 중"));
        assert!(left_half(&buf, 20).contains("휴지통 접근 권한 없음"));
        assert!(trow(&buf, 22).contains("휴지통 0개"));
    }

    #[test]
    fn trash_view_scrolls_with_cursor() {
        let mut v = TrashView::new(PathBuf::from("/prev"));
        v.set_entries(
            (0..40)
                .map(|i| tentry(&format!("file{i:02}"), Some("/x"), Some(TS + i), Some(1)))
                .collect(),
        );
        v.cursor = 30;
        let buf = render_trash(&v, 0, 80);
        let text: String = (2..21).map(|y| left_half(&buf, y)).collect();
        assert!(text.contains("file30") && !text.contains("file00"));
    }
}
