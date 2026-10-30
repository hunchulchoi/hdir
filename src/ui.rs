//! Layout rendering (pure: draws a `View` into a ratatui `Frame`, no IO).

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::assoc::AssocTable;
use crate::config::Mode;
use crate::keymap::{Action, Context, Keymap};
use crate::panel::{Entry, EntryKind, Panel};
use crate::preview::Preview;
use crate::theme::Theme;
use crate::trash::{TrashEntry, TrashView, format_local};

/// What the Dev-mode preview area shows.
#[derive(Debug, Clone, Copy)]
pub enum PreviewBody<'a> {
    /// Nothing to preview (no entry, `..`, trash view).
    Idle,
    Loading,
    Ready(&'a Preview),
}

#[derive(Debug, Clone)]
pub struct PreviewView<'a> {
    /// Name of the previewed entry.
    pub title: String,
    pub body: PreviewBody<'a>,
}

pub struct View<'a> {
    pub preview: Option<PreviewView<'a>>,
    pub mode: Mode,
    pub panels: [&'a Panel; 2],
    pub active: usize,
    pub status: Option<&'a str>,
    pub shell: &'a str,
    /// Extension rules; files matching one get the `assoc_file` color.
    pub assoc: &'a AssocTable,
    pub os: &'a str,
    /// A panel showing the trash instead of its directory.
    pub trash: [Option<&'a TrashView>; 2],
    pub theme: &'a Theme,
    /// F-key bar `(key, label)` slots, see [`fkey_bar`].
    pub fkeys: &'a [(String, String)],
    /// The open `:` line as (text, cursor char index).
    pub cmdline: Option<(&'a str, usize)>,
}

/// F-key bar slots: the action each slot stands for and its label.
const PANEL_BAR: [Option<(Action, &str)>; 10] = [
    Some((Action::Help, "Help")),
    Some((Action::Rename, "Rename")),
    Some((Action::View, "View")),
    Some((Action::Edit, "Edit")),
    Some((Action::Copy, "Copy")),
    Some((Action::Move, "Move")),
    Some((Action::Mkdir, "Mkdir")),
    Some((Action::Delete, "Del")),
    Some((Action::Menu, "Menu")),
    Some((Action::Quit, "Quit")),
];

/// F-key bar while a panel shows the trash; unavailable slots stay blank.
const TRASH_BAR: [Option<(Action, &str)>; 10] = [
    None,
    None,
    None,
    None,
    Some((Action::Copy, "Restore")),
    None,
    None,
    Some((Action::Delete, "Purge")),
    Some((Action::Cancel, "Back")),
    Some((Action::Quit, "Quit")),
];

/// The key to show for `action`: a plain F-key if bound (shown as its number),
/// else the shortest binding. `None` when the action has no key.
fn key_label(km: &Keymap, ctx: Context, action: Action) -> Option<String> {
    let keys = km.keys_for(ctx, action);
    if let Some(n) = keys.iter().find_map(|k| match (k.code, k.mods.is_empty()) {
        (KeyCode::F(n), true) => Some(n),
        _ => None,
    }) {
        return Some(n.to_string());
    }
    keys.iter()
        .map(ToString::to_string)
        .min_by_key(|s| s.width())
}

/// `(key, label)` for the 10 bar slots of `ctx` (`Trash` or anything else = panel),
/// generated from the keymap so remaps show immediately.
pub fn fkey_bar(km: &Keymap, ctx: Context) -> Vec<(String, String)> {
    let slots = if ctx == Context::Trash {
        &TRASH_BAR
    } else {
        &PANEL_BAR
    };
    let ctx = if ctx == Context::Trash {
        Context::Trash
    } else {
        Context::Panel
    };
    slots
        .iter()
        .enumerate()
        .map(|(i, slot)| match slot {
            None => ((i + 1).to_string(), String::new()),
            Some((action, label)) => match key_label(km, ctx, *action) {
                Some(k) => (k, (*label).to_string()),
                None => (String::new(), String::new()),
            },
        })
        .collect()
}

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

/// Display width of one grapheme cluster (ZWJ sequences, VS16 and combining
/// marks measured as a unit).
fn cluster_width(g: &str) -> usize {
    g.width()
}

/// Truncate (with `…`) or right-pad `s` to exactly `width` display columns.
/// Never splits a grapheme cluster.
pub fn fit_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    if s.width() <= width {
        out.push_str(s);
        used = s.width();
    } else if width > 0 {
        for g in s.graphemes(true) {
            let w = cluster_width(g);
            if used + w > width - 1 {
                break;
            }
            out.push_str(g);
            used += w;
        }
        out.push('…');
        used += 1;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Keep the tail of `s` within `width` columns, prefixed with `…` when cut.
/// Never splits a grapheme cluster.
pub(crate) fn tail_width(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 1;
    for g in s.graphemes(true).rev() {
        let w = cluster_width(g);
        if used + w > width {
            break;
        }
        tail.push(g);
        used += w;
    }
    tail.push("…");
    tail.reverse();
    tail.concat()
}

pub(crate) fn human_size(size: u64) -> String {
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

fn entry_style(e: &Entry, selected: bool, has_rule: bool, theme: &Theme) -> Style {
    let base = Style::default().bg(theme.bg);
    if selected {
        return base.fg(theme.selected);
    }
    if is_dirlike(e) || e.kind == EntryKind::Parent {
        let style = base.fg(theme.dir);
        return if theme.dir_bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
    }
    if matches!(e.kind, EntryKind::Symlink { .. }) {
        return base.fg(theme.symlink);
    }
    if e.hidden {
        return base.fg(theme.hidden);
    }
    if e.kind == EntryKind::File && e.mode & 0o111 != 0 {
        return base.fg(theme.exec);
    }
    // Precedence: selected > dir > symlink > hidden > exec > ext group >
    // assoc_file (any rule matches) > file.
    let plain = if has_rule {
        theme.assoc_file
    } else {
        theme.file
    };
    base.fg(theme.color_for_ext(&e.name).unwrap_or(plain))
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

fn draw_panel(
    frame: &mut Frame,
    area: Rect,
    panel: &Panel,
    active: bool,
    assoc: &AssocTable,
    theme: &Theme,
) {
    let border = if active {
        theme.border_active
    } else {
        theme.border
    };
    let title = title_for(panel, (area.width as usize).saturating_sub(4));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border).bg(theme.bg))
        .title(title)
        .style(Style::default().fg(theme.fg).bg(theme.bg));
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
            let has_rule = e.kind == EntryKind::File && assoc.find(&e.os_name).is_some();
            let mut style = entry_style(e, sel, has_rule, theme);
            if active && i == panel.cursor {
                style = style.bg(theme.cursor_bg).fg(theme.cursor_fg);
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
            Style::default().fg(theme.error).bg(theme.bg),
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
    let time = e.deleted_at().map_or_else(|| "-".to_string(), format_local);
    // mark + name + ' ' + time + ' ' + size
    let fixed = 1 + 1 + TIME_W + 1 + SIZE_W;
    if width < fixed + 4 {
        return fit_width(&format!("{mark}{}", e.name()), width);
    }
    let time = fit_width(&time, TIME_W);
    // Room for a parent column only when both columns stay readable.
    if width >= fixed + 1 + 28 {
        let rest = width - fixed - 1;
        let name_w = rest / 2;
        let parent = e.original_parent().map_or_else(|| "-".to_string(), tilde);
        return format!(
            "{mark}{} {} {time} {size:>SIZE_W$}",
            fit_width(&e.name(), name_w),
            fit_width(&tail_width(&parent, rest - name_w), rest - name_w),
        );
    }
    format!(
        "{mark}{} {time} {size:>SIZE_W$}",
        fit_width(&e.name(), width - fixed)
    )
}

fn draw_trash_panel(frame: &mut Frame, area: Rect, tv: &TrashView, active: bool, theme: &Theme) {
    let border = if active {
        theme.border_active
    } else {
        theme.border
    };
    let mut title = "휴지통".to_string();
    if tv.loading {
        title.push_str(" 읽는 중…");
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border).bg(theme.bg))
        .title(tail_width(&title, (area.width as usize).saturating_sub(4)))
        .style(Style::default().fg(theme.fg).bg(theme.bg));
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
            let sel = tv.selected.contains(e.id());
            let mut style = Style::default().bg(theme.bg).fg(if sel {
                theme.selected
            } else if e.is_dir {
                theme.dir
            } else {
                theme.fg
            });
            if e.is_dir {
                style = style.add_modifier(Modifier::BOLD);
            }
            if active && i == tv.cursor {
                style = style.bg(theme.cursor_bg).fg(theme.cursor_fg);
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
            Style::default().fg(theme.error).bg(theme.bg),
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

fn fkey_line(width: usize, keys: &[(String, String)], theme: &Theme) -> Line<'static> {
    let label_style = Style::default()
        .fg(theme.fkey_label_fg)
        .bg(theme.fkey_label_bg);
    let num_style = Style::default().fg(theme.fkey_num_fg).bg(theme.fkey_num_bg);
    let slot = width / keys.len().max(1);
    let mut spans = Vec::new();
    for (i, (n, l)) in keys.iter().enumerate() {
        let w = if i == keys.len() - 1 {
            width - slot * i
        } else {
            slot
        };
        let key_w = n.width().min(w);
        spans.push(Span::styled(
            if key_w == n.width() {
                n.clone()
            } else {
                fit_width(n, key_w)
            },
            num_style,
        ));
        spans.push(Span::styled(fit_width(l, w - key_w), label_style));
    }
    Line::from(spans)
}

/// Screen regions for a layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    pub header: Rect,
    pub panels: Rect,
    /// Dev only, and only when the terminal is tall enough.
    pub preview: Option<Rect>,
    /// Dev only: the `>` prompt row.
    pub command: Option<Rect>,
    pub status: Rect,
    pub fkeys: Rect,
}

/// Rows outside the panel/preview body: header, status, F-key bar (+ command
/// row in Dev).
fn fixed_rows(mode: Mode) -> u16 {
    match mode {
        Mode::Classic => 3,
        Mode::Dev => 4,
    }
}

/// Smallest body (panels + preview) that still gets a preview area.
const MIN_BODY_FOR_PREVIEW: u16 = 12;

/// `(panel rows, preview rows)` of a body `body` rows tall (Dev: the bottom
/// third is the preview).
fn body_split(mode: Mode, body: u16) -> (u16, u16) {
    if mode == Mode::Dev && body >= MIN_BODY_FOR_PREVIEW {
        let preview = body / 3;
        (body - preview, preview)
    } else {
        (body, 0)
    }
}

pub fn layout(mode: Mode, area: Rect) -> Areas {
    let body = area.height.saturating_sub(fixed_rows(mode));
    let (panels_h, preview_h) = body_split(mode, body);
    let mut constraints = vec![Constraint::Length(1), Constraint::Length(panels_h)];
    if preview_h > 0 {
        constraints.push(Constraint::Length(preview_h));
    }
    if mode == Mode::Dev {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    let r = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut it = r.iter().copied();
    let header = it.next().unwrap_or_default();
    let panels = it.next().unwrap_or_default();
    let preview = (preview_h > 0).then(|| it.next().unwrap_or_default());
    let command = (mode == Mode::Dev).then(|| it.next().unwrap_or_default());
    let status = it.next().unwrap_or_default();
    let fkeys = it.next().unwrap_or_default();
    Areas {
        header,
        panels,
        preview,
        command,
        status,
        fkeys,
    }
}

/// Visible entry rows inside a panel for a terminal of `total_height` rows.
pub fn panel_rows(mode: Mode, total_height: u16) -> usize {
    let body = total_height.saturating_sub(fixed_rows(mode));
    body_split(mode, body).0.saturating_sub(2) as usize
}

pub fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    let theme = view.theme;
    let areas = layout(view.mode, area);

    let header = format!(
        " ■ {} {} · {} · {} · UTF-8",
        crate::APP_NAME,
        crate::VERSION,
        view.shell,
        view.os
    );
    frame.render_widget(
        Paragraph::new(fit_width(&header, area.width as usize))
            .style(Style::default().fg(theme.fg).bg(theme.bg)),
        areas.header,
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(areas.panels);
    for i in 0..2 {
        match view.trash[i] {
            Some(tv) => draw_trash_panel(frame, cols[i], tv, view.active == i, view.theme),
            None => draw_panel(
                frame,
                cols[i],
                view.panels[i],
                view.active == i,
                view.assoc,
                view.theme,
            ),
        }
    }

    if let Some(r) = areas.preview {
        draw_preview(frame, r, view.preview.as_ref(), theme);
    }
    // Dev mode edits in its `>` row, Classic borrows the status row.
    let input_row = areas.command.unwrap_or(areas.status);
    let prompt = if areas.command.is_some() { "> " } else { ": " };
    // Scroll the input horizontally so the cursor cell stays on screen.
    // The status row is drawn with one leading space.
    let lead = if areas.command.is_some() { 0 } else { 1 };
    let avail = (input_row.width as usize).saturating_sub(lead + UnicodeWidthStr::width(prompt));
    let (line, cursor_col) = view.cmdline.map_or((String::new(), 0), |(v, c)| {
        let start = crate::dialog::input_window_start(v, c, avail);
        let before: usize = v
            .chars()
            .skip(start)
            .take(c.saturating_sub(start))
            .map(|ch| ch.width().unwrap_or(0))
            .sum();
        (v.chars().skip(start).collect(), before)
    });
    if let Some(r) = areas.command {
        frame.render_widget(
            Paragraph::new(fit_width(&format!("{prompt}{line}"), r.width as usize))
                .style(Style::default().fg(theme.fg).bg(theme.bg)),
            r,
        );
    }

    let active = view.active.min(1);
    let status = match (view.status, view.trash[active]) {
        _ if view.cmdline.is_some() && areas.command.is_none() => format!("{prompt}{line}"),
        (Some(s), _) => s.to_string(),
        (None, Some(tv)) => trash_status(tv),
        (None, None) => default_status(view.panels[active]),
    };
    frame.render_widget(
        Paragraph::new(fit_width(&format!(" {status}"), area.width as usize))
            .style(Style::default().fg(theme.status_fg).bg(theme.status_bg)),
        areas.status,
    );
    frame.render_widget(
        Paragraph::new(fkey_line(area.width as usize, view.fkeys, view.theme)),
        areas.fkeys,
    );
    if view.cmdline.is_some() {
        let x = UnicodeWidthStr::width(prompt) + cursor_col + lead;
        let x = input_row.x + (x as u16).min(input_row.width.saturating_sub(1));
        frame.set_cursor_position((x, input_row.y));
    }
}

/// The Dev-mode preview area.
/// Text rows describing a preview (one per screen row, uncut).
fn preview_lines(p: &Preview) -> Vec<String> {
    match p {
        Preview::Text { lines, .. } => lines.clone(),
        Preview::Binary {
            kind: "특수 파일",
        ..
        } => vec!["특수 파일".to_string()],
        Preview::Binary { size, kind } => {
            vec![format!("바이너리 파일 ({kind}, {})", human_size(*size))]
        }
        Preview::Empty => vec!["빈 파일".to_string()],
        Preview::Error(e) => vec![format!("오류: {e}")],
        Preview::Dir {
            files,
            dirs,
            total_size,
            truncated,
            errors,
        } => {
            let mut v = vec![
                format!("폴더 {dirs}개, 파일 {files}개"),
                format!("총 크기 {}", human_size(*total_size)),
            ];
            if *errors > 0 {
                v.push(format!("읽지 못함 {errors}개"));
            }
            if *truncated {
                v.push("일부만 집계됨 (항목이 너무 많음)".to_string());
            }
            v
        }
    }
}

/// The Dev-mode preview area.
fn draw_preview(frame: &mut Frame, area: Rect, pv: Option<&PreviewView>, theme: &Theme) {
    let base = Style::default().fg(theme.fg).bg(theme.preview_bg);
    let mut title = " 미리보기".to_string();
    if let Some(pv) = pv {
        if !pv.title.is_empty() {
            title.push_str(": ");
            title.push_str(&pv.title);
        }
        if let PreviewBody::Ready(Preview::Text {
            truncated: true, ..
        }) = pv.body
        {
            title.push_str(" · 잘림");
        }
    }
    title.push(' ');
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border).bg(theme.preview_bg))
        .title(tail_width(&title, (area.width as usize).saturating_sub(2)))
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(pv) = pv else {
        return;
    };
    let w = inner.width as usize;
    let (text, style) = match pv.body {
        PreviewBody::Idle => return,
        PreviewBody::Loading => (vec!["읽는 중…".to_string()], base),
        PreviewBody::Ready(p @ Preview::Error(_)) => (preview_lines(p), base.fg(theme.error)),
        PreviewBody::Ready(p) => (preview_lines(p), base),
    };
    let lines: Vec<Line> = text
        .iter()
        .take(inner.height as usize)
        .map(|l| Line::styled(fit_width(l, w), style))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Context, Keymap};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use std::fs;
    use std::path::PathBuf;

    fn th() -> Theme {
        crate::theme::builtin("mdir").unwrap()
    }

    #[test]
    fn fit_width_never_splits_grapheme_clusters() {
        // ZWJ emoji (2 cols), VS16 heart (2 cols), e + combining acute (1 col)
        let zwj = "👩\u{200d}💻";
        assert_eq!(fit_width(&format!("{zwj}ab"), 3), format!("{zwj}…"));
        assert_eq!(fit_width(zwj, 3), format!("{zwj} "));
        // width 2 for "a" + 2-wide cluster does not fit in 2 columns: cut before it
        assert_eq!(fit_width(&format!("a{zwj}"), 2), "a…");
        let heart = "❤\u{fe0f}";
        assert_eq!(
            fit_width(&format!("{heart}{heart}{heart}"), 5),
            format!("{heart}{heart}…")
        );
        let e = "e\u{301}";
        assert_eq!(fit_width(&format!("{e}{e}{e}{e}"), 3), format!("{e}{e}…"));
        assert_eq!(fit_width(&format!("{e}{e}"), 4), format!("{e}{e}  "));
        assert_eq!(fit_width("한글한글", 5), "한글…");
    }

    #[test]
    fn tail_width_never_splits_grapheme_clusters() {
        let zwj = "👩\u{200d}💻";
        assert_eq!(tail_width(&format!("ab{zwj}"), 3), format!("…{zwj}"));
        assert_eq!(tail_width(zwj, 1), "…");
        let e = "e\u{301}";
        assert_eq!(tail_width(&format!("{e}{e}{e}{e}"), 3), format!("…{e}{e}"));
        assert_eq!(tail_width("가나다라", 5), "…다라");
    }

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
            assoc: &AssocTable::default(),
            os: "macos",
            mode: Mode::Classic,
            preview: None,
            trash: [None, None],
            theme: &th(),
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Panel),
            cmdline: None,
        };
        t.draw(|f| draw(f, &view)).unwrap();
        t.backend().buffer().clone()
    }

    fn render_cmdline(mode: Mode, text: &str, cursor: usize, w: u16) -> (Buffer, (u16, u16)) {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
        let view = View {
            panels: [&pa, &pb],
            active: 0,
            status: None,
            shell: "zsh",
            assoc: &AssocTable::default(),
            os: "macos",
            mode,
            preview: None,
            trash: [None, None],
            theme: &th(),
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Panel),
            cmdline: Some((text, cursor)),
        };
        t.draw(|f| draw(f, &view)).unwrap();
        let pos = t.get_cursor_position().unwrap();
        (t.backend().buffer().clone(), (pos.x, pos.y))
    }

    #[test]
    fn cmdline_long_input_scrolls_to_keep_cursor_visible_both_modes() {
        let text = "0123456789".repeat(8); // 80 cols, window is narrower
        for mode in [Mode::Classic, Mode::Dev] {
            let (buf, (cx, cy)) = render_cmdline(mode, &text, text.len(), 40);
            assert!(cx < 40, "{mode:?}: cursor on screen, x={cx}");
            let r = row(&buf, cy);
            assert!(r.contains("789"), "{mode:?}: tail visible: {r:?}");
            assert!(!r.contains("0123456789 0123"), "{mode:?}");
            assert!(
                r.contains("> ") || r.contains(": "),
                "{mode:?}: prompt kept: {r:?}"
            );
            // cursor sits right after the last shown char
            let shown = r.trim_end();
            assert_eq!(
                cx as usize,
                UnicodeWidthStr::width(shown),
                "{mode:?}: {r:?}"
            );
        }
    }

    #[test]
    fn cmdline_korean_cursor_positions_with_scrolling() {
        let text = "가나다라마바사아자차카타파하".repeat(2); // 28 chars, 56 cols
        let n = text.chars().count();
        for mode in [Mode::Classic, Mode::Dev] {
            for cursor in [0, 3, 10, n - 1, n] {
                let (buf, (cx, cy)) = render_cmdline(mode, &text, cursor, 30);
                assert!(cx < 30, "{mode:?} c={cursor}: x={cx}");
                let r = row(&buf, cy);
                // the char under the cursor is exactly at cx
                if cursor < n {
                    let c = text.chars().nth(cursor).unwrap().to_string();
                    assert_eq!(buf[(cx, cy)].symbol(), c, "{mode:?} c={cursor}: {r:?}");
                }
            }
        }
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
        assert_eq!(buf[(5, 2)].bg, th().cursor_bg);
        assert_ne!(buf[(45, 2)].bg, th().cursor_bg);
        let buf = render(&pa, &pb, 1, None, 80, 24);
        assert_ne!(buf[(5, 2)].bg, th().cursor_bg);
        assert_eq!(buf[(45, 2)].bg, th().cursor_bg);
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
            location: Default::default(),
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
        assert_eq!(buf[(2, 2)].bg, th().cursor_bg);
        assert_eq!(buf[(2, 2)].fg, th().cursor_fg);
        assert_eq!(buf[(2, 3)].fg, th().dir);
        assert_eq!(buf[(2, 3)].bg, th().bg);
        assert_eq!(buf[(2, 5)].fg, th().selected);
        // inactive panel has no cursor background
        assert_ne!(buf[(32, 2)].bg, th().cursor_bg);
    }

    // ---- HDIR-21/22/23: theme-driven colors ----

    fn render_themed(a: &Panel, b: &Panel, theme: &Theme) -> Buffer {
        render_assoc(a, b, theme, &AssocTable::default())
    }

    fn render_assoc(a: &Panel, b: &Panel, theme: &Theme, assoc: &AssocTable) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let view = View {
            panels: [a, b],
            active: 0,
            status: None,
            shell: "zsh",
            assoc,
            os: "macos",
            mode: Mode::Classic,
            preview: None,
            trash: [None, None],
            theme,
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Panel),
            cmdline: None,
        };
        t.draw(|f| draw(f, &view)).unwrap();
        t.backend().buffer().clone()
    }

    #[test]
    fn theme_slots_drive_every_region() {
        let mut theme = th();
        theme.bg = Color::Rgb(1, 2, 3);
        theme.status_bg = Color::Rgb(4, 5, 6);
        theme.fkey_label_bg = Color::Rgb(7, 8, 9);
        theme.border_active = Color::Rgb(10, 11, 12);
        let pa = fixed_panel("/tmp/a", &[]);
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert_eq!(buf[(0, 0)].bg, theme.bg); // header
        assert_eq!(buf[(5, 5)].bg, theme.bg); // panel body
        assert_eq!(buf[(0, 1)].fg, theme.border_active);
        assert_eq!(buf[(1, 10)].bg, theme.status_bg);
        assert_eq!(buf[(5, 11)].bg, theme.fkey_label_bg);
    }

    /// Color of the first name cell of entry `idx` (0 is `..`) in the left panel.
    fn name_fg(buf: &Buffer, idx: u16) -> Color {
        buf[(2, 2 + idx)].fg
    }

    #[test]
    fn extension_groups_color_file_names() {
        let theme = th();
        let pa = fixed_panel(
            "/tmp/a",
            &[
                ("a.zip", EntryKind::File, 1),
                ("b.TXT", EntryKind::File, 1),
                ("c.bin", EntryKind::File, 1),
                ("noext", EntryKind::File, 1),
            ],
        );
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert_eq!(name_fg(&buf, 1), Color::Rgb(0xFF, 0x55, 0xFF), "archive");
        assert_eq!(
            name_fg(&buf, 2),
            Color::Rgb(0x55, 0xFF, 0xFF),
            "doc, case-insensitive"
        );
        assert_eq!(name_fg(&buf, 3), theme.file, "unknown ext");
        assert_eq!(name_fg(&buf, 4), theme.file, "no ext");
    }

    #[test]
    fn style_precedence_selected_dir_symlink_hidden_exec_ext() {
        let theme = th();
        let mut pa = fixed_panel(
            "/tmp/a",
            &[
                ("d.zip", EntryKind::Dir, 0),
                (
                    "l",
                    EntryKind::Symlink {
                        target_is_dir: false,
                    },
                    0,
                ),
                (".h.zip", EntryKind::File, 1),
                ("run.zip", EntryKind::File, 1),
                ("sel.zip", EntryKind::File, 1),
            ],
        );
        pa.entries[3].hidden = true;
        pa.entries[3].mode = 0o755; // exec bit but hidden: hidden wins
        pa.entries[4].mode = 0o755;
        pa.selected.insert("sel.zip".into());
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert_eq!(name_fg(&buf, 1), theme.dir, "dir beats ext");
        assert_eq!(name_fg(&buf, 2), theme.symlink);
        assert_eq!(name_fg(&buf, 3), theme.hidden, "hidden beats exec");
        assert_eq!(name_fg(&buf, 4), theme.exec, "exec beats ext");
        assert_eq!(name_fg(&buf, 5), theme.selected, "selected beats ext");
    }

    fn assoc_table(exts: &[&str]) -> AssocTable {
        let rule = crate::assoc::AssocRule {
            ext: Some(exts.iter().map(|e| e.to_string()).collect()),
            glob: None,
            cmd: "true".into(),
            wait: false,
            background: false,
            name: None,
        };
        AssocTable::from_rules(vec![rule]).0
    }

    #[test]
    fn assoc_files_use_the_assoc_file_color() {
        let mut theme = th();
        theme.assoc_file = Color::Rgb(7, 7, 7);
        let pa = fixed_panel(
            "/tmp/a",
            &[
                ("a.xyz", EntryKind::File, 1),
                ("b.other", EntryKind::File, 1),
                ("dir.xyz", EntryKind::Dir, 0),
            ],
        );
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_assoc(&pa, &pb, &theme, &assoc_table(&["xyz"]));
        assert_eq!(name_fg(&buf, 1), Color::Rgb(7, 7, 7));
        assert_eq!(name_fg(&buf, 2), theme.file, "no rule: plain file color");
        assert_eq!(name_fg(&buf, 3), theme.dir, "dirs ignore rules");
    }

    #[test]
    fn assoc_color_sits_below_ext_group_exec_and_hidden() {
        let mut theme = th();
        theme.assoc_file = Color::Rgb(7, 7, 7);
        theme.ext_colors = vec![crate::theme::ExtGroup {
            name: "mine".into(),
            exts: vec!["rs".into()],
            color: Color::Rgb(9, 9, 9),
        }];
        let mut pa = fixed_panel(
            "/tmp/a",
            &[
                ("m.rs", EntryKind::File, 1),
                ("x.sh", EntryKind::File, 1),
                (".h.sh", EntryKind::File, 1),
            ],
        );
        pa.entries[2].mode = 0o755;
        pa.entries[3].mode = 0o755;
        pa.entries[3].hidden = true;
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_assoc(&pa, &pb, &theme, &assoc_table(&["rs", "sh"]));
        assert_eq!(name_fg(&buf, 1), Color::Rgb(9, 9, 9), "ext group wins");
        assert_eq!(name_fg(&buf, 2), theme.exec, "exec wins");
        assert_eq!(name_fg(&buf, 3), theme.hidden, "hidden wins");
    }

    #[test]
    fn exec_bit_beats_extension_color() {
        let theme = th();
        let mut pa = fixed_panel("/tmp/a", &[("x.zip", EntryKind::File, 1)]);
        pa.entries[1].mode = 0o755;
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert_eq!(name_fg(&buf, 1), theme.exec);
    }

    #[test]
    fn dir_bold_follows_theme() {
        let mut theme = th();
        let pa = fixed_panel("/tmp/a", &[("sub", EntryKind::Dir, 0)]);
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert!(buf[(2, 3)].modifier.contains(Modifier::BOLD));
        theme.dir_bold = false;
        let buf = render_themed(&pa, &pb, &theme);
        assert!(!buf[(2, 3)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn user_ext_groups_apply() {
        let mut theme = th();
        theme.ext_colors = vec![crate::theme::ExtGroup {
            name: "mine".into(),
            exts: vec!["rs".into()],
            color: Color::Rgb(9, 9, 9),
        }];
        let pa = fixed_panel("/tmp/a", &[("main.rs", EntryKind::File, 1)]);
        let pb = fixed_panel("/tmp/b", &[]);
        let buf = render_themed(&pa, &pb, &theme);
        assert_eq!(name_fg(&buf, 1), Color::Rgb(9, 9, 9));
    }

    // ---- HDIR-20: F-key bar from the keymap ----

    fn km_with(src: &str) -> Keymap {
        let cfg: crate::keymap::KeysConfig = toml::from_str(src).unwrap();
        crate::keymap::build(&cfg).0
    }

    fn bar(km: &Keymap, ctx: Context) -> Vec<(String, String)> {
        fkey_bar(km, ctx)
    }

    fn pair(n: &str, l: &str) -> (String, String) {
        (n.to_string(), l.to_string())
    }

    #[test]
    fn default_panel_bar_matches_the_classic_f_keys() {
        let got = bar(&Keymap::preset("mdir").unwrap(), Context::Panel);
        let want = [
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
        ]
        .map(|(n, l)| pair(n, l));
        assert_eq!(got, want);
    }

    #[test]
    fn trash_bar_labels_only_available_actions() {
        let got = bar(&Keymap::preset("mdir").unwrap(), Context::Trash);
        assert_eq!(got.len(), 10);
        assert_eq!(got[0], pair("1", ""));
        assert_eq!(got[4], pair("5", "Restore"));
        assert_eq!(got[7], pair("8", "Purge"));
        assert_eq!(got[8], pair("Esc", "Back"));
        assert_eq!(got[9], pair("10", "Quit"));
        assert!(got.iter().all(|(_, l)| l != "Copy" && l != "Mkdir"));
    }

    #[test]
    fn remapped_keys_show_in_the_bar_f_key_preferred_else_shortest() {
        let km = km_with(
            "[bind]\ncopy = [\"c\"]\nmove = [\"Alt+6\", \"F6\"]\nmkdir = [\"Ctrl+Shift+x\", \"Alt+m\"]\nview = []\n",
        );
        let got = bar(&km, Context::Panel);
        assert_eq!(got[4], pair("c", "Copy"));
        assert_eq!(got[5], pair("6", "Move"), "F-key preferred over Alt+6");
        assert_eq!(got[6], pair("Alt+M", "Mkdir"), "shortest of the rest");
        assert_eq!(
            got[2],
            pair("", ""),
            "disabled action leaves the slot blank"
        );
    }

    #[test]
    fn draw_shows_remapped_key_in_fkey_row() {
        let km = km_with("[bind]\ncopy = [\"c\"]\n");
        let pa = fixed_panel("/tmp/a", &[]);
        let pb = fixed_panel("/tmp/b", &[]);
        let fkeys = fkey_bar(&km, Context::Panel);
        let mut t = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let view = View {
            panels: [&pa, &pb],
            active: 0,
            status: None,
            shell: "zsh",
            assoc: &AssocTable::default(),
            os: "macos",
            mode: Mode::Classic,
            preview: None,
            trash: [None, None],
            theme: &th(),
            fkeys: &fkeys,
            cmdline: None,
        };
        t.draw(|f| draw(f, &view)).unwrap();
        let row11 = row(t.backend().buffer(), 11);
        assert!(row11.contains("cCopy"), "{row11}");
        assert!(!row11.contains("5Copy"));
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
            assert_eq!(buf[(x, 1)].fg, th().border_active, "title x={x}");
            assert_eq!(buf[(x, 1)].bg, th().bg, "title x={x}");
        }
        for x in [1, 3, 6, 8] {
            assert_eq!(buf[(x, 10)].fg, th().status_fg, "status x={x}");
            assert_eq!(buf[(x, 10)].bg, th().status_bg, "status x={x}");
        }
        // The idle panel carries no marker.
        assert!(!row(&buf, 1).chars().skip(30).any(|c| c == '읽'));
    }

    // ---- HDIR-62: trash view ----

    use crate::trash::{TrashEntry, TrashView, format_local};

    const TS: i64 = 1_700_000_000;

    fn tentry(name: &str, parent: Option<&str>, at: Option<i64>, size: Option<u64>) -> TrashEntry {
        let item = trash::TrashItem {
            id: format!("id-{name}").into(),
            name: name.into(),
            original_parent: parent.map(PathBuf::from).unwrap_or_default(),
            time_deleted: at.unwrap_or(0),
        };
        TrashEntry::new(item, size, size.is_none() && at.is_some())
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
            assoc: &AssocTable::default(),
            os: "macos",
            mode: Mode::Classic,
            preview: None,
            trash: [Some(tv), None],
            theme: &th(),
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Trash),
            cmdline: None,
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
        assert_eq!(buf[(20, 3)].bg, th().cursor_bg);
        assert_ne!(buf[(20, 2)].bg, th().cursor_bg);
        let inactive = render_trash(&v, 1, 80);
        assert_ne!(inactive[(20, 3)].bg, th().cursor_bg);
        assert_eq!(buf[(0, 1)].fg, th().border_active);
        assert_eq!(inactive[(0, 1)].fg, th().border);
    }

    #[cfg(unix)]
    #[test]
    fn trash_view_wide_shows_original_parent_column() {
        let buf = render_trash(&trash_view(), 0, 140);
        let r = trow(&buf, 2);
        assert!(r.contains("report.pdf") && r.contains("/srv/data"), "{r}");
        assert!(r.find("report.pdf") < r.find("/srv/data"));
        assert!(r.find("/srv/data") < r.find(&format_local(TS)));
    }

    #[cfg(unix)]
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
    fn rows_of(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height).map(|y| row(buf, y)).collect()
    }

    fn render_mode(mode: Mode, w: u16, h: u16) -> Vec<String> {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let view = View {
            panels: [&pa, &pb],
            active: 0,
            status: None,
            shell: "zsh",
            assoc: &AssocTable::default(),
            os: "macos",
            mode,
            preview: None,
            trash: [None, None],
            theme: &th(),
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Panel),
            cmdline: None,
        };
        t.draw(|f| draw(f, &view)).unwrap();
        rows_of(t.backend().buffer())
    }

    #[test]
    fn classic_layout_has_no_preview_or_command_row() {
        let rows = render_mode(Mode::Classic, 100, 30);
        assert!(rows[0].contains("hdir"));
        assert!(rows[1].starts_with('┌'));
        assert!(rows[28].contains("files"), "status: {:?}", rows[28]);
        assert!(rows[29].contains("Copy"));
        assert!(
            rows[27].starts_with('└'),
            "panels run down to the status row"
        );
        assert!(!rows.iter().any(|r| r.starts_with("> ")));
        assert!(!rows.iter().any(|r| r.contains("미리보기")));
        let a = layout(Mode::Classic, Rect::new(0, 0, 100, 30));
        assert_eq!((a.preview, a.command), (None, None));
        assert_eq!(a.panels.height, 27);
    }

    #[test]
    fn dev_layout_adds_preview_area_and_command_prompt() {
        let rows = render_mode(Mode::Dev, 100, 30);
        assert!(rows[0].contains("hdir"));
        assert!(rows[27].starts_with("> "), "command row: {:?}", rows[27]);
        assert!(rows[28].contains("files"), "status: {:?}", rows[28]);
        assert!(rows[29].contains("Copy"));
        let a = layout(Mode::Dev, Rect::new(0, 0, 100, 30));
        let pv = a.preview.unwrap();
        assert_eq!((a.panels.height, pv.height), (18, 8));
        assert_eq!(pv.y, a.panels.y + a.panels.height);
        assert_eq!(a.command.unwrap().y, 27);
        assert!(
            rows[pv.y as usize].replace(' ', "").contains("미리보기"),
            "{:?}",
            rows[pv.y as usize]
        );
        assert!(rows[pv.y as usize].starts_with('┌'));
        // panels still render above the preview
        assert!(rows[1].starts_with('┌') && rows[18].starts_with('└'));
    }

    #[test]
    fn panel_rows_follow_the_active_layout() {
        assert_eq!(panel_rows(Mode::Classic, 24), 19);
        assert_eq!(panel_rows(Mode::Classic, 3), 0);
        assert_eq!(panel_rows(Mode::Classic, 0), 0);
        assert_eq!(panel_rows(Mode::Dev, 30), 16);
        assert_eq!(panel_rows(Mode::Dev, 0), 0);
        // small terminals drop the preview area but keep the command row
        assert_eq!(panel_rows(Mode::Dev, 14), 14 - 4 - 2);
        for mode in [Mode::Classic, Mode::Dev] {
            for h in 6..80u16 {
                let a = layout(mode, Rect::new(0, 0, 80, h));
                assert_eq!(
                    a.panels.height.saturating_sub(2) as usize,
                    panel_rows(mode, h),
                    "{mode:?} h={h}"
                );
                let used = 1
                    + a.panels.height
                    + a.preview.map_or(0, |r| r.height)
                    + a.command.map_or(0, |r| r.height)
                    + 2;
                assert_eq!(used, h, "{mode:?} h={h}: regions tile the screen");
            }
        }
    }

    #[test]
    fn dev_layout_survives_tiny_terminals() {
        for (w, h) in [(0, 0), (1, 1), (10, 3), (30, 5), (100, 9)] {
            render_mode(Mode::Dev, w, h);
            render_mode(Mode::Classic, w, h);
        }
    }
    #[test]
    fn preview_lines_cover_every_kind() {
        let l = |p: &Preview| preview_lines(p);
        assert_eq!(l(&Preview::Empty), ["빈 파일"]);
        assert_eq!(
            l(&Preview::Binary {
                size: 2048,
                kind: "PNG 이미지"
            }),
            ["바이너리 파일 (PNG 이미지, 2.0K)"]
        );
        assert_eq!(
            l(&Preview::Binary {
                size: 0,
                kind: "특수 파일"
            }),
            ["특수 파일"]
        );
        assert_eq!(l(&Preview::Error("권한 없음".into())), ["오류: 권한 없음"]);
        assert_eq!(
            l(&Preview::Text {
                lines: vec!["a".into(), "b".into()],
                truncated: true
            }),
            ["a", "b"]
        );
        let dir = l(&Preview::Dir {
            files: 3,
            dirs: 2,
            total_size: 1536,
            truncated: false,
            errors: 0,
        });
        assert_eq!(dir, ["폴더 2개, 파일 3개", "총 크기 1.5K"]);
        let dir = l(&Preview::Dir {
            files: 1,
            dirs: 0,
            total_size: 10,
            truncated: true,
            errors: 4,
        });
        assert_eq!(dir.len(), 4, "{dir:?}");
        assert_eq!(dir[2], "읽지 못함 4개");
        assert!(dir[3].contains("일부만"));
    }

    fn render_preview(pv: PreviewView, theme: &Theme, w: u16, h: u16) -> Buffer {
        let (a, b) = setup();
        let (pa, pb) = (Panel::new(a.path()), Panel::new(b.path()));
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let view = View {
            preview: Some(pv),
            panels: [&pa, &pb],
            active: 0,
            status: None,
            shell: "zsh",
            assoc: &AssocTable::default(),
            os: "macos",
            mode: Mode::Dev,
            trash: [None, None],
            theme,
            fkeys: &fkey_bar(&Keymap::preset("mdir").unwrap(), Context::Panel),
            cmdline: None,
        };
        t.draw(|f| draw(f, &view)).unwrap();
        t.backend().buffer().clone()
    }

    #[test]
    fn preview_area_shows_text_title_and_truncation() {
        let p = Preview::Text {
            lines: (0..20).map(|i| format!("줄{i}")).collect(),
            truncated: true,
        };
        let buf = render_preview(
            PreviewView {
                title: "a.txt".into(),
                body: PreviewBody::Ready(&p),
            },
            &th(),
            100,
            30,
        );
        let rows = rows_of(&buf);
        // preview block: y 19..27, 6 inner rows
        let top = rows[19].replace(' ', "");
        assert!(
            top.contains("미리보기:a.txt") && top.contains("잘림"),
            "{top}"
        );
        assert!(
            rows[20].replace(' ', "").starts_with("│줄0"),
            "{:?}",
            rows[20]
        );
        assert!(
            rows[25].replace(' ', "").starts_with("│줄5"),
            "{:?}",
            rows[25]
        );
        assert!(
            !rows.join("").replace(' ', "").contains("줄6"),
            "clipped to the area"
        );
    }

    #[test]
    fn preview_area_shows_loading_idle_and_binary() {
        let buf = render_preview(
            PreviewView {
                title: "x".into(),
                body: PreviewBody::Loading,
            },
            &th(),
            100,
            30,
        );
        assert!(rows_of(&buf)[20].replace(' ', "").contains("읽는중"));
        let buf = render_preview(
            PreviewView {
                title: String::new(),
                body: PreviewBody::Idle,
            },
            &th(),
            100,
            30,
        );
        assert!(rows_of(&buf)[20].replace(' ', "").starts_with("│"));
        let bin = Preview::Binary {
            size: 5,
            kind: "ELF 실행파일",
        };
        let buf = render_preview(
            PreviewView {
                title: "bin".into(),
                body: PreviewBody::Ready(&bin),
            },
            &th(),
            100,
            30,
        );
        assert!(
            rows_of(&buf)[20]
                .replace(' ', "")
                .contains("바이너리파일(ELF실행파일,5B)")
        );
    }

    #[test]
    fn preview_area_uses_the_preview_bg_slot() {
        let mut theme = th();
        theme.preview_bg = Color::Rgb(1, 2, 3);
        let p = Preview::Empty;
        let buf = render_preview(
            PreviewView {
                title: "e".into(),
                body: PreviewBody::Ready(&p),
            },
            &theme,
            100,
            30,
        );
        assert_eq!(buf[(5, 22)].bg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(40, 20)].bg, Color::Rgb(1, 2, 3));
        assert_ne!(buf[(5, 5)].bg, Color::Rgb(1, 2, 3), "panels keep bg");
    }

    #[test]
    fn preview_long_lines_are_cut_to_the_width() {
        let p = Preview::Text {
            lines: vec!["한".repeat(100)],
            truncated: false,
        };
        let buf = render_preview(
            PreviewView {
                title: "w".into(),
                body: PreviewBody::Ready(&p),
            },
            &th(),
            40,
            30,
        );
        let r = rows_of(&buf);
        assert!(r[20].contains('…'), "cut with an ellipsis: {:?}", r[20]);
    }
}
