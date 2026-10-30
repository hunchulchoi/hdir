//! F3 full-screen text viewer: state, scrolling, search and rendering.
//! Loading happens elsewhere (a background task); this module is pure.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::keymap::Action;
use crate::preview::Preview;
use crate::theme::Theme;
use crate::ui::{fit_width, human_size};

/// Columns moved by one Left/Right press.
const HSTEP: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Loading,
    Text {
        lines: Vec<String>,
        truncated: bool,
    },
    /// Binary summary, an error or "empty file": a single message.
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub path: PathBuf,
    pub content: Content,
    pub scroll: usize,
    pub hscroll: usize,
    /// Text being typed after `/`; `Some` while the search input is open.
    pub input: Option<String>,
    pub query: Option<String>,
    /// Line of the current match (highlighted).
    pub match_line: Option<usize>,
    /// Transient footer message (e.g. "찾을 수 없음").
    pub notice: Option<String>,
    rows: usize,
    cols: usize,
    max_width: usize,
}

impl Viewer {
    pub fn loading(path: PathBuf) -> Self {
        Self {
            path,
            content: Content::Loading,
            scroll: 0,
            hscroll: 0,
            input: None,
            query: None,
            match_line: None,
            notice: None,
            rows: 10,
            cols: 80,
            max_width: 0,
        }
    }

    /// Adopt the loaded preview.
    pub fn set_preview(&mut self, p: Preview) {
        self.scroll = 0;
        self.hscroll = 0;
        self.match_line = None;
        self.notice = None;
        self.content = match p {
            Preview::Text { lines, truncated } => {
                self.max_width = lines.iter().map(|l| l.width()).max().unwrap_or(0);
                Content::Text { lines, truncated }
            }
            Preview::Binary { size, kind } => {
                Content::Message(format!("바이너리 파일 ({kind}, {})", human_size(size)))
            }
            Preview::Empty => Content::Message("빈 파일".to_string()),
            Preview::Dir { .. } => Content::Message("디렉터리는 볼 수 없음".to_string()),
            Preview::Error(e) => Content::Message(format!("오류: {e}")),
        };
    }

    fn lines(&self) -> &[String] {
        match &self.content {
            Content::Text { lines, .. } => lines,
            _ => &[],
        }
    }

    pub fn max_scroll(&self) -> usize {
        self.lines().len().saturating_sub(self.rows)
    }

    fn max_hscroll(&self) -> usize {
        self.max_width.saturating_sub(self.cols)
    }

    /// Apply a viewer-context action; returns true when the viewer should close.
    pub fn apply(&mut self, action: Action) -> bool {
        let max = self.max_scroll();
        let page = self.rows.max(1);
        match action {
            Action::Quit | Action::Cancel => return true,
            Action::CursorUp => self.scroll = self.scroll.saturating_sub(1),
            Action::CursorDown => self.scroll = (self.scroll + 1).min(max),
            Action::PageUp => self.scroll = self.scroll.saturating_sub(page),
            Action::PageDown => self.scroll = (self.scroll + page).min(max),
            Action::Home => self.scroll = 0,
            Action::End => self.scroll = max,
            Action::ScrollLeft => self.hscroll = self.hscroll.saturating_sub(HSTEP),
            Action::ScrollRight => self.hscroll = (self.hscroll + HSTEP).min(self.max_hscroll()),
            Action::Search if !self.lines().is_empty() => {
                self.notice = None;
                self.input = Some(String::new());
            }
            Action::SearchNext if self.query.is_some() => {
                self.find(self.match_line.map_or(self.scroll, |m| m + 1));
            }
            _ => {}
        }
        false
    }

    /// Search `query` from line `from` (wrapping); scrolls to the match.
    fn find(&mut self, from: usize) {
        let Some(q) = self.query.clone() else {
            return;
        };
        let needle = q.to_lowercase();
        let n = self.lines().len();
        let hit = (0..n)
            .map(|i| (from + i) % n)
            .find(|&idx| self.lines()[idx].to_lowercase().contains(&needle));
        match hit {
            Some(idx) => {
                self.match_line = Some(idx);
                self.scroll = idx.min(self.max_scroll());
                self.notice = None;
            }
            None => self.notice = Some(format!("찾을 수 없음: {q}")),
        }
    }

    /// Raw key while the search input is open.
    pub fn input_key(&mut self, key: KeyEvent) {
        let Some(input) = self.input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let q = self.input.take().unwrap_or_default();
                if !q.is_empty() {
                    self.query = Some(q);
                    self.find(self.scroll);
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(c),
            _ => {}
        }
    }

    /// `N/M` plus the truncation marker; empty while loading or for messages.
    fn counter(&self) -> String {
        let Content::Text { lines, truncated } = &self.content else {
            return String::new();
        };
        let n = (self.scroll + 1).min(lines.len());
        let mut s = format!("{n}/{}", lines.len());
        if *truncated {
            s.push_str(" · 잘림");
        }
        s
    }

    /// Header text: path, line N/M and the truncation marker.
    pub fn header(&self) -> String {
        let c = self.counter();
        if c.is_empty() {
            format!(" {}", self.path.display())
        } else {
            format!(" {} · {c}", self.path.display())
        }
    }
}

/// `s` from display column `start`, exactly `width` columns wide (cut or
/// padded); clusters straddling the edges are replaced by spaces.
pub fn window(s: &str, start: usize, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if col < start {
            let end = col + w;
            col = end;
            if end > start {
                let pad = (end - start).min(width);
                out.push_str(&" ".repeat(pad));
                used += pad;
            }
            continue;
        }
        if used + w > width {
            break;
        }
        out.push_str(g);
        used += w;
        col += w;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

pub fn render(frame: &mut Frame, v: &mut Viewer, theme: &Theme, close_hint: &str) {
    let area = frame.area();
    let (w, h) = (area.width as usize, area.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let base = Style::default().fg(theme.fg).bg(theme.bg);
    let bar = Style::default().fg(theme.status_fg).bg(theme.status_bg);
    v.rows = h.saturating_sub(2);
    v.cols = w;
    v.scroll = v.scroll.min(v.max_scroll());
    v.hscroll = v.hscroll.min(v.max_hscroll());

    let counter = v.counter();
    let header = if counter.is_empty() {
        format!(" {}", v.path.display())
    } else {
        let room = w.saturating_sub(counter.width() + 4);
        format!(
            " {} · {counter}",
            crate::ui::tail_width(&v.path.display().to_string(), room)
        )
    };
    let mut lines: Vec<Line> = vec![Line::styled(fit_width(&header, w), bar)];

    match &v.content {
        Content::Text { lines: text, .. } => {
            for (i, l) in text.iter().enumerate().skip(v.scroll).take(v.rows) {
                let style = if v.match_line == Some(i) {
                    Style::default().fg(theme.cursor_fg).bg(theme.cursor_bg)
                } else {
                    base
                };
                lines.push(Line::styled(window(l, v.hscroll, w), style));
            }
        }
        Content::Loading => lines.push(Line::styled(fit_width(" 읽는 중…", w), base)),
        Content::Message(m) => lines.push(Line::styled(fit_width(&format!(" {m}"), w), base)),
    }
    if h >= 2 {
        while lines.len() < h - 1 {
            lines.push(Line::styled(" ".repeat(w), base));
        }
        lines.truncate(h - 1);
        let footer = match (&v.input, &v.notice) {
            (Some(q), _) => format!(" /{q}_"),
            (None, Some(n)) => format!(" {n}"),
            (None, None) => format!(" {close_hint}"),
        };
        lines.push(Line::styled(fit_width(&footer, w), bar));
    } else {
        lines.truncate(h);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn th() -> Theme {
        crate::theme::builtin("mdir").unwrap()
    }

    fn text(n: usize) -> Viewer {
        let mut v = Viewer::loading(PathBuf::from("/tmp/a.txt"));
        v.set_preview(Preview::Text {
            lines: (0..n).map(|i| format!("line{i}")).collect(),
            truncated: false,
        });
        v
    }

    fn draw(v: &mut Viewer, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, v, &th(), "q: 닫기")).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x += sym.width().max(1) as u16;
                }
                row
            })
            .collect()
    }

    fn typed(v: &mut Viewer, s: &str) {
        for c in s.chars() {
            v.input_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn enter(v: &mut Viewer) {
        v.input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn loading_shows_message_and_ignores_scroll() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        assert_eq!(v.content, Content::Loading);
        let rows = draw(&mut v, 40, 6);
        assert!(rows.join("").contains("읽는 중"), "{rows:?}");
        assert!(!v.apply(Action::PageDown));
        assert_eq!(v.scroll, 0);
        assert!(v.apply(Action::Quit));
    }

    #[test]
    fn scroll_math_clamps_at_both_ends() {
        let mut v = text(20);
        draw(&mut v, 40, 8); // 6 body rows
        assert_eq!(v.max_scroll(), 14);
        v.apply(Action::CursorUp);
        assert_eq!(v.scroll, 0);
        v.apply(Action::PageDown);
        assert_eq!(v.scroll, 6);
        v.apply(Action::End);
        assert_eq!(v.scroll, 14);
        v.apply(Action::CursorDown);
        assert_eq!(v.scroll, 14);
        v.apply(Action::PageUp);
        assert_eq!(v.scroll, 8);
        v.apply(Action::Home);
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn close_actions() {
        let mut v = text(3);
        assert!(v.apply(Action::Quit));
        assert!(v.apply(Action::Cancel));
        assert!(!v.apply(Action::Enter));
        assert!(!v.apply(Action::Help));
    }

    #[test]
    fn horizontal_scroll_clamps_to_widest_line() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: vec!["a".repeat(30), "b".into()],
            truncated: false,
        });
        draw(&mut v, 22, 6); // 22 columns visible
        v.apply(Action::ScrollRight);
        assert_eq!(v.hscroll, 8);
        v.apply(Action::ScrollRight);
        assert_eq!(v.hscroll, 8, "30 - 22 columns");
        v.apply(Action::ScrollLeft);
        assert_eq!(v.hscroll, 0);
        v.apply(Action::ScrollLeft);
        assert_eq!(v.hscroll, 0);
    }

    #[test]
    fn window_cuts_pads_and_keeps_clusters_whole() {
        assert_eq!(window("abcdef", 2, 3), "cde");
        assert_eq!(window("ab", 0, 4), "ab  ");
        assert_eq!(window("ab", 5, 3), "   ");
        // wide char straddling the left edge becomes a space
        assert_eq!(window("한글", 1, 4), " 글 ");
        assert_eq!(window("한글", 0, 3), "한 ");
        let zwj = "👩\u{200d}💻";
        assert_eq!(window(&format!("{zwj}x"), 0, 2), zwj);
        assert_eq!(window(&format!("{zwj}x"), 1, 2), " x");
    }

    #[test]
    fn hscrolled_render_shows_the_shifted_text() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: vec!["0123456789가나다".into()],
            truncated: false,
        });
        draw(&mut v, 8, 5);
        v.apply(Action::ScrollRight);
        let rows = draw(&mut v, 8, 5);
        assert!(rows[1].starts_with("89가나다"), "{:?}", rows[1]);
    }

    #[test]
    fn search_finds_forward_wraps_and_reports_missing() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: ["alpha", "beta", "Gamma", "beta2", "delta"]
                .map(String::from)
                .to_vec(),
            truncated: false,
        });
        draw(&mut v, 30, 4); // 2 body rows, max_scroll 3
        v.apply(Action::Search);
        assert_eq!(v.input.as_deref(), Some(""));
        typed(&mut v, "BETA");
        enter(&mut v);
        assert!(v.input.is_none());
        assert_eq!((v.match_line, v.scroll), (Some(1), 1));
        v.apply(Action::SearchNext);
        assert_eq!((v.match_line, v.scroll), (Some(3), 3));
        v.apply(Action::SearchNext);
        assert_eq!(v.match_line, Some(1), "wraps to the first match");
        assert_eq!(v.scroll, 1);
        v.apply(Action::Search);
        typed(&mut v, "zzz");
        enter(&mut v);
        assert!(v.notice.as_deref().unwrap().contains("zzz"));
        assert_eq!(v.match_line, Some(1), "failed search keeps the old match");
    }

    #[test]
    fn search_finds_korean_and_esc_cancels_input() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: vec!["하나".into(), "둘 셋".into()],
            truncated: false,
        });
        v.apply(Action::Search);
        typed(&mut v, "셋");
        enter(&mut v);
        assert_eq!(v.match_line, Some(1));
        v.apply(Action::Search);
        typed(&mut v, "x");
        v.input_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(v.input.as_deref(), Some(""));
        v.input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(v.input.is_none());
    }

    #[test]
    fn search_next_without_query_does_nothing() {
        let mut v = text(5);
        v.apply(Action::SearchNext);
        assert_eq!((v.match_line, v.scroll), (None, 0));
    }

    #[test]
    fn header_has_path_line_counter_and_truncation_marker() {
        let mut v = text(20);
        draw(&mut v, 60, 8);
        let h = v.header();
        assert!(h.contains("/tmp/a.txt") && h.contains("1/20"), "{h}");
        assert!(!h.contains("잘림"));
        v.apply(Action::PageDown);
        assert!(v.header().contains("7/20"), "{}", v.header());
        v.set_preview(Preview::Text {
            lines: vec!["x".into(); 3],
            truncated: true,
        });
        assert!(v.header().contains("잘림"), "{}", v.header());
    }

    #[test]
    fn binary_empty_and_error_become_messages() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Binary {
            size: 2048,
            kind: "PNG 이미지",
        });
        assert_eq!(
            v.content,
            Content::Message("바이너리 파일 (PNG 이미지, 2.0K)".into())
        );
        let rows = draw(&mut v, 50, 6);
        assert!(rows.join("").contains("바이너리 파일 (PNG 이미지, 2.0K)"));
        v.set_preview(Preview::Empty);
        assert!(matches!(&v.content, Content::Message(m) if m.contains("빈 파일")));
        v.set_preview(Preview::Error("권한 없음".into()));
        assert!(matches!(&v.content, Content::Message(m) if m.contains("권한 없음")));
        v.apply(Action::Search);
        assert!(v.input.is_none(), "no search on messages");
    }

    #[test]
    fn snapshot_header_body_footer_and_korean_width() {
        let mut v = Viewer::loading(PathBuf::from("/tmp/한글.txt"));
        v.set_preview(Preview::Text {
            lines: vec!["첫째 줄".into(), "second".into(), "셋째".into()],
            truncated: true,
        });
        let rows = draw(&mut v, 40, 6);
        assert!(
            rows[0].contains("/tmp/한글.txt") && rows[0].contains("1/3"),
            "{rows:?}"
        );
        assert!(rows[0].contains("잘림"));
        assert!(rows[1].starts_with("첫째 줄"));
        assert!(rows[2].starts_with("second"));
        assert!(rows[3].starts_with("셋째"));
        assert!(rows[5].contains("q: 닫기"), "{:?}", rows[5]);
    }

    #[test]
    fn footer_shows_search_input() {
        let mut v = text(5);
        v.apply(Action::Search);
        typed(&mut v, "한");
        let rows = draw(&mut v, 40, 6);
        assert!(rows[5].contains("/한"), "{:?}", rows[5]);
    }

    #[test]
    fn match_line_is_highlighted() {
        let mut v = text(5);
        draw(&mut v, 20, 8);
        v.apply(Action::Search);
        typed(&mut v, "line2");
        enter(&mut v);
        let mut t = Terminal::new(TestBackend::new(20, 8)).unwrap();
        t.draw(|f| render(f, &mut v, &th(), "")).unwrap();
        let buf = t.backend().buffer();
        // everything fits, so line2 stays on body row 3 (screen row 3)
        assert_eq!(buf[(0, 3)].bg, th().cursor_bg);
        assert_ne!(buf[(0, 2)].bg, th().cursor_bg);
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let mut v = text(5);
        draw(&mut v, 3, 2);
        draw(&mut v, 1, 1);
        draw(&mut v, 0, 0);
    }
}
