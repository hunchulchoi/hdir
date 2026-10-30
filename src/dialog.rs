//! Modal dialogs: pure state machine (`Dialog::handle_key`) plus rendering. No IO.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod palette {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(0xAA, 0xAA, 0xAA);
    pub const FG: Color = Color::Rgb(0x00, 0x00, 0x00);
    pub const TITLE: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
    pub const DANGER_BG: Color = Color::Rgb(0xAA, 0x00, 0x00);
    pub const DANGER_FG: Color = Color::Rgb(0xFF, 0xFF, 0xFF);
    pub const SELECTED_BG: Color = Color::Rgb(0x00, 0xAA, 0xAA);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    Input {
        title: String,
        prompt: String,
        value: String,
        /// Cursor position as a char index (0..=value.chars().count()).
        cursor: usize,
    },
    Confirm {
        title: String,
        message: Vec<String>,
        /// Red style, used for permanent delete.
        danger: bool,
    },
    Choice {
        title: String,
        message: Vec<String>,
        options: Vec<String>,
        selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogResult {
    Pending,
    Submitted(String),
    Confirmed,
    Chosen(usize),
    Cancelled,
}

impl Dialog {
    pub fn input(title: &str, prompt: &str, initial: &str) -> Self {
        Dialog::Input {
            title: title.to_string(),
            prompt: prompt.to_string(),
            value: initial.to_string(),
            cursor: initial.chars().count(),
        }
    }

    pub fn confirm(title: &str, message: Vec<String>) -> Self {
        Dialog::Confirm {
            title: title.to_string(),
            message,
            danger: false,
        }
    }

    pub fn danger(title: &str, message: Vec<String>) -> Self {
        Dialog::Confirm {
            title: title.to_string(),
            message,
            danger: true,
        }
    }

    pub fn choice(title: &str, message: Vec<String>, options: Vec<String>) -> Self {
        Self::choice_with_default(title, message, options, 0)
    }

    /// Like [`choice`](Self::choice) with `selected` pre-highlighted (clamped into range).
    pub fn choice_with_default(
        title: &str,
        message: Vec<String>,
        options: Vec<String>,
        selected: usize,
    ) -> Self {
        let selected = selected.min(options.len().saturating_sub(1));
        Dialog::Choice {
            title: title.to_string(),
            message,
            options,
            selected,
        }
    }

    /// Feed a key event. `Release` events are ignored.
    ///
    /// Choice letter shortcuts pick the FIRST option whose first character
    /// matches case-insensitively; later options sharing that letter (e.g.
    /// "Overwrite all" after "Overwrite") are only reachable via Left/Right/Tab.
    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult {
        if key.kind == KeyEventKind::Release {
            return DialogResult::Pending;
        }
        if key.code == KeyCode::Esc {
            return DialogResult::Cancelled;
        }
        match self {
            Dialog::Input { value, cursor, .. } => input_key(value, cursor, key),
            Dialog::Confirm { .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => DialogResult::Confirmed,
                KeyCode::Char('n' | 'N') => DialogResult::Cancelled,
                _ => DialogResult::Pending,
            },
            Dialog::Choice {
                options, selected, ..
            } => {
                let n = options.len();
                match key.code {
                    _ if n == 0 => {}
                    KeyCode::Right | KeyCode::Tab => *selected = (*selected + 1) % n,
                    KeyCode::Left | KeyCode::BackTab => *selected = (*selected + n - 1) % n,
                    KeyCode::Enter => return DialogResult::Chosen(*selected),
                    KeyCode::Char(c) => {
                        let hit = options.iter().position(|o| {
                            o.chars()
                                .next()
                                .is_some_and(|f| f.to_lowercase().eq(c.to_lowercase()))
                        });
                        if let Some(i) = hit {
                            return DialogResult::Chosen(i);
                        }
                    }
                    _ => {}
                }
                DialogResult::Pending
            }
        }
    }
}

fn byte_index(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

fn input_key(value: &mut String, cursor: &mut usize, key: KeyEvent) -> DialogResult {
    let len = value.chars().count();
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('u') if ctrl => {
            value.clear();
            *cursor = 0;
        }
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            value.insert(byte_index(value, *cursor), c);
            *cursor += 1;
        }
        KeyCode::Backspace if *cursor > 0 => {
            *cursor -= 1;
            value.remove(byte_index(value, *cursor));
        }
        KeyCode::Delete if *cursor < len => {
            value.remove(byte_index(value, *cursor));
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(len),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = len,
        KeyCode::Enter if !value.is_empty() => {
            return DialogResult::Submitted(value.clone());
        }
        _ => {}
    }
    DialogResult::Pending
}

/// Center a `width` x `height` rect in `area`, clamped to the area size.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn cw(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// First char index to display so that the cursor cell fits in `width` columns.
/// Stays at 0 while the cursor fits; otherwise the cursor sits at the right edge.
fn input_window_start(value: &str, cursor: usize, width: usize) -> usize {
    let chars: Vec<char> = value.chars().collect();
    let cell = chars.get(cursor).map_or(1, |&c| cw(c).max(1));
    let before: usize = chars.iter().take(cursor).map(|&c| cw(c)).sum();
    if before + cell <= width {
        return 0;
    }
    let mut used = cell;
    let mut start = cursor;
    while start > 0 {
        let w = cw(chars[start - 1]);
        if used + w > width {
            break;
        }
        used += w;
        start -= 1;
    }
    start
}

fn input_line(value: &str, cursor: usize, width: usize, base: Style) -> Line<'static> {
    let start = input_window_start(value, cursor, width);
    let chars: Vec<char> = value.chars().collect();
    let mut used = 0;
    let mut spans = Vec::new();
    let mut buf = String::new();
    for (i, &c) in chars.iter().enumerate().skip(start) {
        let w = cw(c);
        if used + w > width {
            break;
        }
        if i == cursor {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), base));
            }
            spans.push(Span::styled(
                c.to_string(),
                base.add_modifier(Modifier::REVERSED),
            ));
        } else {
            buf.push(c);
        }
        used += w;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base));
    }
    if cursor >= chars.len() && used < width {
        spans.push(Span::styled(" ", base.add_modifier(Modifier::REVERSED)));
    }
    Line::from(spans)
}

fn buttons_line(options: &[String], selected: usize, base: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, o) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", base));
        }
        let style = if i == selected {
            base.fg(palette::FG).bg(palette::SELECTED_BG)
        } else {
            base
        };
        spans.push(Span::styled(format!("[ {o} ]"), style));
    }
    Line::from(spans)
}

/// Draw `dialog` as a centered box over `area` (clearing what is underneath).
pub fn render(frame: &mut Frame, area: Rect, dialog: &Dialog) {
    let (title, danger) = match dialog {
        Dialog::Input { title, .. } | Dialog::Choice { title, .. } => (title, false),
        Dialog::Confirm { title, danger, .. } => (title, *danger),
    };
    let (bg, fg): (Color, Color) = if danger {
        (palette::DANGER_BG, palette::DANGER_FG)
    } else {
        (palette::BG, palette::FG)
    };
    let base = Style::default().fg(fg).bg(bg);

    // Inner width is the box width minus border and one column of padding per side.
    let content_w = match dialog {
        Dialog::Input { prompt, .. } => prompt.width(),
        Dialog::Confirm { message, .. } => message.iter().map(|m| m.width()).max().unwrap_or(0),
        Dialog::Choice {
            message, options, ..
        } => {
            let buttons = options.iter().map(|o| o.width() + 4).sum::<usize>()
                + options.len().saturating_sub(1);
            message
                .iter()
                .map(|m| m.width())
                .max()
                .unwrap_or(0)
                .max(buttons)
        }
    }
    .max(title.width());
    let max_w = (area.width as usize).saturating_sub(4);
    let box_w = (content_w + 4).max(40).min(max_w) as u16;
    let inner_w = (box_w as usize).saturating_sub(4);

    let lines: Vec<Line> = match dialog {
        Dialog::Input {
            prompt,
            value,
            cursor,
            ..
        } => vec![
            Line::styled(prompt.clone(), base),
            input_line(value, *cursor, inner_w, base),
        ],
        Dialog::Confirm { message, .. } => message
            .iter()
            .map(|m| Line::styled(m.clone(), base))
            .chain([
                Line::styled("", base),
                Line::styled("Enter: Yes   N/Esc: No", base),
            ])
            .collect(),
        Dialog::Choice {
            message,
            options,
            selected,
            ..
        } => message
            .iter()
            .map(|m| Line::styled(m.clone(), base))
            .chain([
                Line::styled("", base),
                buttons_line(options, *selected, base),
            ])
            .collect(),
    };

    let rect = centered(area, box_w, lines.len() as u16 + 2);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette::TITLE).bg(bg))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(palette::TITLE).bg(bg),
        ))
        .style(base);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    // One column of padding inside the border.
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    frame.render_widget(Paragraph::new(lines).style(base), padded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::KeyEventState;

    const GRAY: Color = Color::Rgb(0xAA, 0xAA, 0xAA);
    const RED: Color = Color::Rgb(0xAA, 0x00, 0x00);
    const CYAN: Color = Color::Rgb(0x00, 0xAA, 0xAA);

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    fn release(mut k: KeyEvent) -> KeyEvent {
        k.kind = KeyEventKind::Release;
        k.state = KeyEventState::NONE;
        k
    }
    fn type_str(d: &mut Dialog, s: &str) {
        for c in s.chars() {
            d.handle_key(ch(c));
        }
    }
    fn val(d: &Dialog) -> (&str, usize) {
        match d {
            Dialog::Input { value, cursor, .. } => (value, *cursor),
            _ => panic!("not input"),
        }
    }
    fn opts() -> Vec<String> {
        ["Overwrite", "Skip", "Overwrite all", "Skip all", "Cancel"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
    fn draw(d: &Dialog) -> Buffer {
        let area = Rect::new(0, 0, 80, 24);
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render(f, area, d)).unwrap();
        t.backend().buffer().clone()
    }
    fn cells(_buf: &Buffer, r: Rect) -> Vec<(u16, u16)> {
        (r.y..r.y + r.height)
            .flat_map(|y| (r.x..r.x + r.width).map(move |x| (x, y)))
            .collect()
    }
    fn text_in(buf: &Buffer, r: Rect) -> String {
        (r.y..r.y + r.height)
            .map(|y| {
                let mut line = String::new();
                let mut x = r.x;
                while x < r.x + r.width {
                    let sym = buf[(x, y)].symbol();
                    line.push_str(sym);
                    // wide glyphs occupy following continuation cells
                    x += sym.width().max(1) as u16;
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn all_text(buf: &Buffer) -> String {
        text_in(buf, buf.area)
    }
    fn reversed(buf: &Buffer, r: Rect) -> Vec<(u16, u16)> {
        cells(buf, r)
            .into_iter()
            .filter(|&p| buf[p].modifier.contains(Modifier::REVERSED))
            .collect()
    }
    /// Bounding rect of cells having background `bg`.
    fn box_rect(buf: &Buffer, bg: Color) -> Rect {
        let (mut x0, mut y0, mut x1, mut y1) = (u16::MAX, u16::MAX, 0, 0);
        for (x, y) in cells(buf, buf.area) {
            if buf[(x, y)].bg == bg {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        assert!(x0 != u16::MAX, "no cell with bg {bg:?}");
        Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    }

    #[test]
    fn esc_cancels_all_kinds() {
        let mut a = Dialog::input("t", "p", "x");
        let mut b = Dialog::confirm("t", vec![]);
        let mut c = Dialog::choice("t", vec![], opts());
        for d in [&mut a, &mut b, &mut c] {
            assert_eq!(d.handle_key(key(KeyCode::Esc)), DialogResult::Cancelled);
        }
    }

    #[test]
    fn input_types_korean_and_submits() {
        let mut d = Dialog::input("이름", "새 이름:", "");
        type_str(&mut d, "한글a");
        assert_eq!(val(&d), ("한글a", 3));
        assert_eq!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submitted("한글a".into())
        );
    }

    #[test]
    fn input_initial_cursor_at_end_in_chars() {
        let d = Dialog::input("t", "p", "가나다");
        assert_eq!(val(&d), ("가나다", 3));
    }

    #[test]
    fn input_backspace_and_delete_by_char() {
        let mut d = Dialog::input("t", "p", "가나다");
        d.handle_key(key(KeyCode::Backspace));
        assert_eq!(val(&d), ("가나", 2));
        d.handle_key(key(KeyCode::Left));
        d.handle_key(key(KeyCode::Delete));
        assert_eq!(val(&d), ("가", 1));
        d.handle_key(key(KeyCode::Home));
        d.handle_key(key(KeyCode::Backspace)); // no-op at start
        assert_eq!(val(&d), ("가", 0));
        d.handle_key(key(KeyCode::End));
        d.handle_key(key(KeyCode::Delete)); // no-op at end
        assert_eq!(val(&d), ("가", 1));
    }

    #[test]
    fn input_cursor_movement_and_insert_in_middle() {
        let mut d = Dialog::input("t", "p", "ac");
        d.handle_key(key(KeyCode::Left));
        d.handle_key(ch('ㅂ'));
        assert_eq!(val(&d), ("aㅂc", 2));
        d.handle_key(key(KeyCode::Home));
        assert_eq!(val(&d).1, 0);
        d.handle_key(key(KeyCode::Left)); // clamped
        assert_eq!(val(&d).1, 0);
        d.handle_key(key(KeyCode::End));
        d.handle_key(key(KeyCode::Right)); // clamped
        assert_eq!(val(&d).1, 3);
    }

    #[test]
    fn input_ctrl_u_clears() {
        let mut d = Dialog::input("t", "p", "abc");
        d.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(val(&d), ("", 0));
    }

    #[test]
    fn input_ctrl_chars_not_inserted() {
        let mut d = Dialog::input("t", "p", "");
        d.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(val(&d), ("", 0));
    }

    #[test]
    fn input_empty_enter_stays_pending_but_spaces_submit() {
        let mut d = Dialog::input("t", "p", "");
        assert_eq!(d.handle_key(key(KeyCode::Enter)), DialogResult::Pending);
        type_str(&mut d, "  ");
        assert_eq!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submitted("  ".into())
        );
    }

    #[test]
    fn release_events_ignored() {
        let mut d = Dialog::input("t", "p", "");
        assert_eq!(d.handle_key(release(ch('a'))), DialogResult::Pending);
        assert_eq!(val(&d), ("", 0));
        let mut c = Dialog::confirm("t", vec![]);
        assert_eq!(c.handle_key(release(ch('y'))), DialogResult::Pending);
        assert_eq!(
            c.handle_key(release(key(KeyCode::Esc))),
            DialogResult::Pending
        );
    }

    #[test]
    fn confirm_keys() {
        for k in [key(KeyCode::Enter), ch('y'), ch('Y')] {
            let mut d = Dialog::confirm("t", vec!["m".into()]);
            assert_eq!(d.handle_key(k), DialogResult::Confirmed);
        }
        for k in [ch('n'), ch('N')] {
            let mut d = Dialog::danger("t", vec!["m".into()]);
            assert_eq!(d.handle_key(k), DialogResult::Cancelled);
        }
        let mut d = Dialog::confirm("t", vec![]);
        assert_eq!(d.handle_key(ch('x')), DialogResult::Pending);
    }

    #[test]
    fn choice_cycles_with_wrap() {
        let mut d = Dialog::choice("t", vec![], opts());
        let sel = |d: &Dialog| match d {
            Dialog::Choice { selected, .. } => *selected,
            _ => unreachable!(),
        };
        d.handle_key(key(KeyCode::Left));
        assert_eq!(sel(&d), 4);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(sel(&d), 0);
        d.handle_key(key(KeyCode::Tab));
        assert_eq!(sel(&d), 1);
        assert_eq!(d.handle_key(key(KeyCode::Enter)), DialogResult::Chosen(1));
    }

    #[test]
    fn choice_letter_shortcuts_first_match_wins() {
        let mut d = Dialog::choice("t", vec![], opts());
        // 'o' -> Overwrite (0, not "Overwrite all"); 's' -> Skip (1, not "Skip all")
        assert_eq!(d.handle_key(ch('o')), DialogResult::Chosen(0));
        assert_eq!(d.handle_key(ch('S')), DialogResult::Chosen(1));
        assert_eq!(d.handle_key(ch('c')), DialogResult::Chosen(4));
        assert_eq!(d.handle_key(ch('z')), DialogResult::Pending);
    }

    #[test]
    fn centered_math() {
        let a = Rect::new(0, 0, 80, 24);
        assert_eq!(centered(a, 40, 10), Rect::new(20, 7, 40, 10));
        let a = Rect::new(5, 3, 30, 10);
        assert_eq!(centered(a, 10, 4), Rect::new(15, 6, 10, 4));
        // smaller than requested -> clamp to area
        assert_eq!(centered(a, 100, 50), a);
        assert_eq!(centered(Rect::new(0, 0, 0, 0), 5, 5), Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn render_confirm_shows_title_message_inside_centered_box() {
        let d = Dialog::confirm("삭제", vec!["파일을 삭제할까요?".into(), "foo.txt".into()]);
        let buf = draw(&d);
        let r = box_rect(&buf, GRAY);
        assert!(r.width >= 40);
        assert_eq!(r.x, (80 - r.width) / 2);
        assert_eq!(r.y, (24 - r.height) / 2);
        let inside = text_in(&buf, r);
        assert!(inside.contains("삭제"));
        assert!(inside.contains("파일을 삭제할까요?"));
        assert!(inside.contains("foo.txt"));
        assert_eq!(all_text(&buf).matches("foo.txt").count(), 1);
    }

    #[test]
    fn render_clears_underneath() {
        let area = Rect::new(0, 0, 80, 24);
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let d = Dialog::confirm("t", vec!["m".into()]);
        t.draw(|f| {
            f.render_widget(Paragraph::new("X".repeat(80 * 24)), area);
            render(f, area, &d);
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        let r = box_rect(&buf, GRAY);
        assert_ne!(buf[(r.x + r.width - 2, r.y + 1)].symbol(), "X");
    }

    #[test]
    fn render_input_cursor_cell_reversed() {
        let d = Dialog::input("이름", "새 이름:", "abc");
        let buf = draw(&d);
        let r = box_rect(&buf, GRAY);
        let rev = reversed(&buf, r);
        assert_eq!(rev.len(), 1);
        let (x, y) = rev[0];
        assert_eq!(buf[(x, y)].symbol(), " ");
        assert_eq!(buf[(x - 1, y)].symbol(), "c");
    }

    #[test]
    fn render_input_cursor_on_char_when_in_middle() {
        let mut d = Dialog::input("t", "p", "abc");
        d.handle_key(key(KeyCode::Left));
        let buf = draw(&d);
        let r = box_rect(&buf, GRAY);
        let found: Vec<String> = reversed(&buf, r)
            .into_iter()
            .map(|p| buf[p].symbol().to_string())
            .collect();
        assert_eq!(found, vec!["c"]);
    }

    #[test]
    fn render_long_korean_input_scrolls_keeping_cursor_visible() {
        let long = "가나다라마바사아자차카타파하".repeat(3); // 42 chars, 84 cols
        let d = Dialog::input("이름", "새 이름:", &long);
        let buf = draw(&d);
        let r = box_rect(&buf, GRAY);
        let rev = reversed(&buf, r);
        assert_eq!(rev.len(), 1, "cursor cell must be visible");
        assert!(rev[0].0 > r.x && rev[0].0 < r.x + r.width - 1);
        let text = all_text(&buf);
        assert!(text.contains("카타파하"));
        assert!(!text.contains("가나다라마바사아자차카타파하가나"));
    }

    #[test]
    fn render_danger_is_red() {
        let d = Dialog::danger("영구 삭제", vec!["복구할 수 없습니다".into()]);
        let buf = draw(&d);
        let r = box_rect(&buf, RED);
        assert!(r.width >= 40);
        assert!(!cells(&buf, buf.area).iter().any(|&p| buf[p].bg == GRAY));
        assert_eq!(buf[(r.x + 2, r.y + 1)].fg, Color::Rgb(0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn render_choice_buttons_and_highlight() {
        let mut d = Dialog::choice("충돌", vec!["이미 존재함".into()], opts());
        d.handle_key(key(KeyCode::Right));
        let buf = draw(&d);
        let text = all_text(&buf);
        assert!(text.contains("[ Overwrite ]"));
        assert!(text.contains("[ Skip ]"));
        let hl: String = cells(&buf, buf.area)
            .into_iter()
            .filter(|&p| buf[p].bg == CYAN)
            .map(|p| buf[p].symbol().to_string())
            .collect();
        assert_eq!(hl, "[ Skip ]");
    }

    #[test]
    fn render_small_area_does_not_panic() {
        let d = Dialog::choice("t", vec!["m".into()], opts());
        let mut t = Terminal::new(TestBackend::new(20, 5)).unwrap();
        t.draw(|f| render(f, f.area(), &d)).unwrap();
    }
}
