//! A small list menu: pure cursor state plus a centered popup renderer.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::dialog::centered;
use crate::keymap::Action;
use crate::theme::Theme;
use crate::ui::{fit_width, visible_offset};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub title: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

/// What a menu key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent {
    /// Nothing to report (or the cursor moved: see [`MenuEvent::Moved`]).
    None,
    Moved,
    /// Enter on the item at this index.
    Chosen(usize),
    Cancelled,
}

impl Menu {
    pub fn new(title: &str, items: Vec<String>, cursor: usize) -> Self {
        let cursor = cursor.min(items.len().saturating_sub(1));
        Self {
            title: title.to_string(),
            items,
            cursor,
        }
    }

    /// Apply a menu-context action.
    pub fn apply(&mut self, action: Action) -> MenuEvent {
        let last = self.items.len().saturating_sub(1);
        let before = self.cursor;
        match action {
            Action::CursorUp => self.cursor = self.cursor.saturating_sub(1),
            Action::CursorDown => self.cursor = (self.cursor + 1).min(last),
            Action::Home => self.cursor = 0,
            Action::End => self.cursor = last,
            Action::Enter if !self.items.is_empty() => return MenuEvent::Chosen(self.cursor),
            Action::Cancel => return MenuEvent::Cancelled,
            _ => {}
        }
        if self.cursor != before {
            MenuEvent::Moved
        } else {
            MenuEvent::None
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, menu: &Menu, theme: &Theme) {
    let base = Style::default().fg(theme.dialog_fg).bg(theme.dialog_bg);
    let sel = Style::default().fg(theme.cursor_fg).bg(theme.cursor_bg);
    let content_w = menu
        .items
        .iter()
        .map(|i| i.width())
        .chain([menu.title.width()])
        .max()
        .unwrap_or(0);
    let box_w = (content_w + 6).max(30).min(area.width as usize) as u16;
    let box_h = (menu.items.len() as u16 + 2).min(area.height);
    let rect = centered(area, box_w, box_h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.title).bg(theme.dialog_bg))
        .title(Span::styled(
            format!(" {} ", menu.title),
            Style::default().fg(theme.title).bg(theme.dialog_bg),
        ))
        .style(base);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    let rows = inner.height as usize;
    let offset = visible_offset(menu.cursor, 0, rows);
    let w = inner.width as usize;
    let lines: Vec<Line> = menu
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
        .map(|(i, item)| {
            let style = if i == menu.cursor { sel } else { base };
            Line::styled(fit_width(&format!(" {item}"), w), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).style(base), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn th() -> Theme {
        crate::theme::builtin("mdir").unwrap()
    }

    fn menu(n: usize) -> Menu {
        Menu::new("메뉴", (0..n).map(|i| format!("item{i}")).collect(), 0)
    }

    #[test]
    fn cursor_moves_clamps_and_reports() {
        let mut m = menu(3);
        assert_eq!(m.apply(Action::CursorUp), MenuEvent::None);
        assert_eq!(m.apply(Action::CursorDown), MenuEvent::Moved);
        assert_eq!(m.apply(Action::CursorDown), MenuEvent::Moved);
        assert_eq!(m.apply(Action::CursorDown), MenuEvent::None);
        assert_eq!(m.cursor, 2);
        assert_eq!(m.apply(Action::Home), MenuEvent::Moved);
        assert_eq!(m.apply(Action::End), MenuEvent::Moved);
        assert_eq!(m.cursor, 2);
    }

    #[test]
    fn enter_chooses_and_cancel_cancels() {
        let mut m = menu(3);
        m.apply(Action::CursorDown);
        assert_eq!(m.apply(Action::Enter), MenuEvent::Chosen(1));
        assert_eq!(m.apply(Action::Cancel), MenuEvent::Cancelled);
        assert_eq!(m.apply(Action::Copy), MenuEvent::None);
        let mut empty = menu(0);
        assert_eq!(empty.apply(Action::Enter), MenuEvent::None);
        assert_eq!(empty.apply(Action::CursorDown), MenuEvent::None);
    }

    #[test]
    fn initial_cursor_is_clamped() {
        assert_eq!(Menu::new("t", vec!["a".into(), "b".into()], 9).cursor, 1);
    }

    fn draw(m: &Menu, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, f.area(), m, &th())).unwrap();
        t.backend().buffer().clone()
    }

    fn text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut x = 0;
            while x < buf.area.width {
                let s = buf[(x, y)].symbol();
                out.push_str(s);
                x += if s.width() > 1 { 2 } else { 1 };
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_lists_items_and_highlights_the_cursor_row() {
        let mut m = menu(3);
        m.apply(Action::CursorDown);
        let buf = draw(&m, 60, 20);
        let t = text(&buf);
        assert!(
            t.contains("item0") && t.contains("item1") && t.contains("item2"),
            "{t}"
        );
        assert!(t.contains("메뉴"));
        let th = th();
        let find = |needle: &str| {
            (0..20u16)
                .find(|&y| {
                    (0..60u16)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap()
        };
        let y1 = find("item1");
        let y0 = find("item0");
        let x = (0..60u16).find(|&x| buf[(x, y1)].symbol() == "i").unwrap();
        assert_eq!(buf[(x, y1)].bg, th.cursor_bg);
        assert_eq!(buf[(x, y0)].bg, th.dialog_bg);
    }

    #[test]
    fn long_menu_scrolls_to_keep_the_cursor_visible() {
        let mut m = menu(30);
        m.apply(Action::End);
        let t = text(&draw(&m, 40, 10));
        assert!(t.contains("item29") && !t.contains("item0 "), "{t}");
    }

    #[test]
    fn tiny_area_does_not_panic() {
        draw(&menu(5), 4, 2);
        draw(&menu(0), 40, 10);
    }
}
