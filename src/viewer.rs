//! F3 full-screen text viewer: state, scrolling, search and rendering.
//! Loading happens elsewhere (a background task); this module is pure.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::i18n::t;
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

/// Display-column index of one line: where each grapheme starts (byte offset)
/// and ends (column), so rendering can start at a column without rescanning.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LineIndex {
    byte_starts: Vec<usize>,
    /// Column where grapheme `i` starts; `col_starts[i] + width(i)` is its end.
    col_starts: Vec<usize>,
    col_ends: Vec<usize>,
}

impl LineIndex {
    fn build(s: &str) -> Self {
        let mut idx = Self {
            byte_starts: Vec::new(),
            col_starts: Vec::new(),
            col_ends: Vec::new(),
        };
        let mut col = 0;
        for (b, g) in s.grapheme_indices(true) {
            idx.byte_starts.push(b);
            idx.col_starts.push(col);
            col += g.width();
            idx.col_ends.push(col);
        }
        idx
    }
}

/// A Markdown document: its source and the formatted lines for one width.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MdView {
    source: String,
    /// Showing the source instead of the formatted text.
    raw: bool,
    /// Width `lines` were laid out for (0: not yet).
    width: usize,
    lines: Vec<crate::markdown::MdLine>,
    plain: Vec<String>,
    max_width: usize,
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
    /// Test seam: how many times the whole document was lowercased.
    pub lower_builds: usize,
    /// Lowercase copy of every line, built on the first search of a document.
    lower: Option<Vec<String>>,
    /// Column indexes of the visible long lines (by line number).
    line_index: HashMap<usize, LineIndex>,
    /// Test seam: how many per-line column indexes were built.
    pub index_builds: usize,
    rows: usize,
    cols: usize,
    max_width: usize,
    md: Option<MdView>,
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
            lower_builds: 0,
            lower: None,
            line_index: HashMap::new(),
            index_builds: 0,
            rows: 10,
            cols: 80,
            max_width: 0,
            md: None,
        }
    }

    /// Showing formatted Markdown (not the source).
    fn formatted(&self) -> bool {
        self.md.as_ref().is_some_and(|m| !m.raw)
    }

    /// Lay the Markdown out for `width` columns (when that changed).
    fn layout_md(&mut self, width: usize) {
        let Some(m) = self.md.as_mut().filter(|m| !m.raw && m.width != width) else {
            return;
        };
        m.lines = crate::markdown::render(&m.source, width);
        m.plain = m.lines.iter().map(crate::markdown::plain).collect();
        m.max_width = m.plain.iter().map(|l| l.width()).max().unwrap_or(0);
        m.width = width;
        // line numbers changed: searches and indexes start over
        self.lower = None;
        self.line_index.clear();
        self.match_line = None;
    }

    /// Adopt the loaded preview.
    pub fn set_preview(&mut self, p: Preview) {
        self.scroll = 0;
        self.hscroll = 0;
        self.match_line = None;
        self.notice = None;
        self.lower = None;
        self.line_index.clear();
        self.md = None;
        self.content = match p {
            Preview::Text { lines, truncated } => {
                self.max_width = lines.iter().map(|l| l.width()).max().unwrap_or(0);
                if crate::markdown::is_markdown(&self.path) {
                    self.md = Some(MdView {
                        source: lines.join("\n"),
                        raw: false,
                        width: 0,
                        lines: Vec::new(),
                        plain: Vec::new(),
                        max_width: 0,
                    });
                }
                Content::Text { lines, truncated }
            }
            Preview::Binary { size, kind } => Content::Message(crate::tr!(
                "바이너리 파일 ({kind}, {})",
                "Binary file ({kind}, {})",
                human_size(size)
            )),
            Preview::Empty => Content::Message(t("빈 파일", "Empty file").to_string()),
            Preview::Dir { .. } => {
                Content::Message(t("디렉터리는 볼 수 없음", "Cannot view a directory").to_string())
            }
            Preview::Error(e) => Content::Message(crate::tr!("오류: {e}", "Error: {e}")),
            Preview::Info(m) => Content::Message(m),
        };
    }

    fn lines(&self) -> &[String] {
        match (&self.content, &self.md) {
            (Content::Text { .. }, Some(m)) if !m.raw => &m.plain,
            (Content::Text { lines, .. }, _) => lines,
            _ => &[],
        }
    }

    pub fn max_scroll(&self) -> usize {
        self.lines().len().saturating_sub(self.rows)
    }

    fn max_hscroll(&self) -> usize {
        let widest = match &self.md {
            Some(m) if !m.raw => m.max_width,
            _ => self.max_width,
        };
        widest.saturating_sub(self.cols)
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
            Action::ToggleMarkdown => {
                if let Some(m) = self.md.as_mut() {
                    m.raw = !m.raw;
                    m.width = 0;
                    self.scroll = 0;
                    self.hscroll = 0;
                    self.lower = None;
                    self.line_index.clear();
                    self.match_line = None;
                }
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
        if self.lower.is_none() {
            self.lower_builds += 1;
            self.lower = Some(self.lines().iter().map(|l| l.to_lowercase()).collect());
        }
        let lower = self.lower.as_deref().unwrap_or_default();
        let n = lower.len();
        let hit = (0..n)
            .map(|i| (from + i) % n)
            .find(|&idx| lower[idx].contains(&needle));
        match hit {
            Some(idx) => {
                self.match_line = Some(idx);
                self.scroll = idx.min(self.max_scroll());
                self.notice = None;
            }
            None => self.notice = Some(crate::tr!("찾을 수 없음: {q}", "Not found: {q}")),
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
        let Content::Text { truncated, .. } = &self.content else {
            return String::new();
        };
        let total = self.lines().len();
        let n = (self.scroll + 1).min(total);
        let mut s = format!("{n}/{total}");
        if *truncated {
            s.push_str(t(" · 잘림", " · truncated"));
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

/// Same result as [`window`], but starts at the grapheme found through the
/// prebuilt `idx` instead of scanning from the line start.
fn window_indexed(s: &str, idx: &LineIndex, start: usize, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    // First grapheme that begins at or after `start`.
    let first = idx.col_starts.partition_point(|&c| c < start);
    if first > 0 && idx.col_ends[first - 1] > start {
        // A wide cluster straddles the left edge: blank its visible part.
        let pad = (idx.col_ends[first - 1] - start).min(width);
        out.push_str(&" ".repeat(pad));
        used += pad;
    }
    if let Some(&b) = idx.byte_starts.get(first) {
        for g in s[b..].graphemes(true) {
            let w = g.width();
            if used + w > width {
                break;
            }
            out.push_str(g);
            used += w;
        }
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Terminal style of a Markdown piece.
fn md_style(st: &crate::markdown::Style, theme: &Theme) -> Style {
    use crate::markdown::Kind;
    use ratatui::style::Modifier;
    let fg = match st.kind {
        Kind::Text => theme.fg,
        Kind::Heading(_) => theme.title,
        Kind::Code | Kind::CodeBlock => theme.exec,
        Kind::Marker | Kind::Rule | Kind::Table => theme.border,
        Kind::Link => theme.symlink,
        Kind::Url | Kind::Html => theme.hidden,
    };
    let mut s = Style::default().fg(fg).bg(theme.bg);
    if st.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    if st.italic {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if st.strike {
        s = s.add_modifier(Modifier::CROSSED_OUT);
    }
    if st.kind == Kind::Link {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

/// A formatted line from column `start`, exactly `width` columns wide.
fn md_window(
    line: &crate::markdown::MdLine,
    start: usize,
    width: usize,
    theme: &Theme,
) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let mut out = Vec::new();
    let (mut col, mut used) = (0, 0);
    for (text, st) in line {
        let end = col + text.width();
        if end > start && used < width {
            let from = col.max(start);
            let piece = (end - from).min(width - used);
            out.push(Span::styled(
                window(text, from - col, piece),
                md_style(st, theme),
            ));
            used += piece;
        }
        col = end;
    }
    out.push(Span::styled(
        " ".repeat(width - used),
        Style::default().fg(theme.fg).bg(theme.bg),
    ));
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
    v.layout_md(w);
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
        Content::Text { .. } if v.formatted() => {
            let m = v.md.as_ref().expect("formatted");
            for (i, l) in m.lines.iter().enumerate().skip(v.scroll).take(v.rows) {
                let mut spans = md_window(l, v.hscroll, w, theme);
                if v.match_line == Some(i) {
                    let hl = Style::default().fg(theme.cursor_fg).bg(theme.cursor_bg);
                    for s in &mut spans {
                        s.style = hl;
                    }
                }
                lines.push(Line::from(spans));
            }
        }
        Content::Text { lines: text, .. } => {
            let visible = v.scroll..(v.scroll + v.rows).min(text.len());
            v.line_index.retain(|i, _| visible.contains(i));
            for (i, l) in text.iter().enumerate().skip(v.scroll).take(v.rows) {
                let style = if v.match_line == Some(i) {
                    Style::default().fg(theme.cursor_fg).bg(theme.cursor_bg)
                } else {
                    base
                };
                let row = if v.hscroll == 0 {
                    window(l, 0, w)
                } else if l.len() <= v.hscroll {
                    // Every cluster is at least one column and one byte wide
                    // or zero columns: nothing can reach the visible area.
                    " ".repeat(w)
                } else {
                    let idx = v.line_index.entry(i).or_insert_with(|| {
                        v.index_builds += 1;
                        LineIndex::build(l)
                    });
                    window_indexed(l, idx, v.hscroll, w)
                };
                lines.push(Line::styled(row, style));
            }
        }
        Content::Loading => lines.push(Line::styled(
            fit_width(t(" 읽는 중…", " Loading…"), w),
            base,
        )),
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
            (None, None) if v.md.is_some() => {
                let to = if v.formatted() {
                    t("원문", "source")
                } else {
                    t("서식", "formatted")
                };
                format!(" {close_hint} · m: {to}")
            }
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
    fn window_indexed_matches_window_for_every_start_and_width() {
        let zwj = "👩\u{200d}💻";
        for s in [
            "",
            "abcdef",
            "한글 mixed 텍스트 abc",
            &format!("a{zwj}b한c"),
            "e\u{301}x가",
        ] {
            let idx = LineIndex::build(s);
            for start in 0..(s.width() + 3) {
                for width in 0..8 {
                    assert_eq!(
                        window_indexed(s, &idx, start, width),
                        window(s, start, width),
                        "{s:?} start={start} width={width}"
                    );
                }
            }
        }
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
    fn repeated_next_does_not_relowercase_the_document() {
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: ["Alpha", "beta", "BETA2", "gamma", "beta3"]
                .map(String::from)
                .to_vec(),
            truncated: false,
        });
        draw(&mut v, 30, 4);
        v.apply(Action::Search);
        typed(&mut v, "Beta");
        enter(&mut v);
        assert_eq!(v.match_line, Some(1));
        for want in [2, 4, 1, 2] {
            v.apply(Action::SearchNext);
            assert_eq!(v.match_line, Some(want));
        }
        assert_eq!(v.lower_builds, 1, "lowercased once per loaded document");
        // A new document gets its own copy.
        v.set_preview(Preview::Text {
            lines: vec!["Zeta".into()],
            truncated: false,
        });
        v.apply(Action::Search);
        typed(&mut v, "zeta");
        enter(&mut v);
        assert_eq!(v.match_line, Some(0));
        assert_eq!(v.lower_builds, 2);
    }

    #[test]
    fn hscroll_renders_a_long_korean_line_from_a_cached_index() {
        let long: String = "가나다abc라마".repeat(2000); // 14 columns per unit
        let mut v = Viewer::loading(PathBuf::from("/x"));
        v.set_preview(Preview::Text {
            lines: vec!["short".into(), long.clone()],
            truncated: false,
        });
        draw(&mut v, 20, 5);
        v.hscroll = 1001; // lands in the middle of a wide char
        let first = draw(&mut v, 20, 5);
        let again = draw(&mut v, 20, 5);
        assert_eq!(first, again);
        assert_eq!(first[2].trim_end(), window(&long, 1001, 20).trim_end());
        assert_eq!(first[1].trim_end(), "", "short line is scrolled out");
        assert_eq!(
            v.index_builds, 1,
            "only the long visible line is indexed, once across frames"
        );
        v.hscroll += 8;
        draw(&mut v, 20, 5);
        assert_eq!(v.index_builds, 1, "scrolling reuses the index");
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
        v.set_preview(Preview::Info("원격 디렉터리 · 항목 2개".into()));
        assert_eq!(
            v.content,
            Content::Message("원격 디렉터리 · 항목 2개".into())
        );
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

#[cfg(test)]
mod md_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn th() -> Theme {
        crate::theme::builtin("mdir").unwrap()
    }

    fn md(src: &str) -> Viewer {
        let mut v = Viewer::loading(PathBuf::from("/docs/읽어 보기.md"));
        v.set_preview(Preview::Text {
            lines: src.lines().map(String::from).collect(),
            truncated: false,
        });
        v
    }

    fn draw(v: &mut Viewer, w: u16, h: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, v, &th(), "q: 닫기")).unwrap();
        let buf = t.backend().buffer().clone();
        let rows = (0..h)
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
            .collect();
        (rows, buf)
    }

    #[test]
    fn a_markdown_file_opens_formatted_and_m_shows_the_source() {
        let mut v = md("# 제목\n\n본문 **굵게**\n\n- 항목");
        let (rows, buf) = draw(&mut v, 40, 10);
        assert_eq!(rows[1].trim_end(), "제목");
        assert!(rows[2].starts_with("════"), "{rows:?}");
        assert!(rows[4].starts_with("본문 굵게"), "{rows:?}");
        assert!(rows[6].starts_with("• 항목"), "{rows:?}");
        assert_eq!(buf[(0, 1)].fg, th().title, "heading color");
        assert!(
            buf[(5, 4)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(rows[9].contains("m:"), "the toggle is shown: {}", rows[9]);
        assert!(!v.apply(Action::ToggleMarkdown));
        let (rows, _) = draw(&mut v, 40, 10);
        assert_eq!(rows[1].trim_end(), "# 제목", "source view");
        assert!(rows[0].contains("1/5"), "{}", rows[0]);
        v.apply(Action::ToggleMarkdown);
        let (rows, _) = draw(&mut v, 40, 10);
        assert_eq!(rows[1].trim_end(), "제목");
    }

    #[test]
    fn a_resize_rewraps_and_long_lines_scroll_sideways() {
        let src = format!(
            "{}\n\n```\n{}\n```\n\n| a | b |\n|---|---|\n| {} | x |",
            "단어 ".repeat(30),
            format!("{}ZZEND", "c".repeat(100)),
            "긴셀".repeat(20)
        );
        let mut v = md(&src);
        let (wide, _) = draw(&mut v, 80, 30);
        let (narrow, _) = draw(&mut v, 30, 30);
        let count = |rows: &[String]| rows.iter().filter(|r| r.contains("단어")).count();
        assert!(count(&narrow) > count(&wide), "paragraphs rewrap");
        // the code line is never wrapped: the right part is reached by scrolling
        assert_eq!(narrow.iter().filter(|r| r.contains("ccc")).count(), 1);
        assert!(
            !narrow.iter().any(|r| r.contains("ZZEND")),
            "not yet visible"
        );
        for _ in 0..10 {
            v.apply(Action::ScrollRight);
        }
        let (rows, _) = draw(&mut v, 30, 30);
        assert!(
            rows.iter().any(|r| r.contains("ZZEND")),
            "the end of the code line: {rows:?}"
        );
        for _ in 0..20 {
            v.apply(Action::ScrollRight);
        }
        assert!(v.hscroll > 0);
        let (rows, _) = draw(&mut v, 30, 30);
        assert!(
            rows.iter().any(|r| r.contains("긴셀")),
            "the wide table scrolls"
        );
        for r in &rows {
            assert_eq!(r.width(), 30, "{r:?}");
        }
    }

    #[test]
    fn search_finds_the_formatted_text() {
        let mut v = md("# Title\n\nfirst\n\n**찾는 말** here");
        draw(&mut v, 40, 10);
        v.apply(Action::Search);
        for c in "찾는".chars() {
            v.input_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        v.input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(v.match_line, Some(5));
    }

    #[test]
    fn plain_text_files_and_empty_markdown_are_unchanged() {
        let mut t = Viewer::loading(PathBuf::from("/a.txt"));
        t.set_preview(Preview::Text {
            lines: vec!["# not a heading".into()],
            truncated: false,
        });
        let (rows, _) = draw(&mut t, 30, 5);
        assert_eq!(rows[1].trim_end(), "# not a heading");
        assert!(!rows[4].contains("m:"));
        let mut e = Viewer::loading(PathBuf::from("/e.md"));
        e.set_preview(Preview::Empty);
        let (rows, _) = draw(&mut e, 30, 5);
        assert!(rows[1].contains("빈 파일"), "{rows:?}");
        e.apply(Action::ToggleMarkdown);
        draw(&mut e, 0, 0);
        draw(&mut e, 3, 2);
    }
}

#[cfg(test)]
mod en_tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn draw(v: &mut Viewer, w: u16, h: u16) -> Vec<String> {
        let th = crate::theme::builtin("mdir").unwrap();
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, v, &th, "Esc/q/F3/F10: close"))
            .unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn english_footer_hint_loading_and_truncated_header() {
        with_lang(Lang::En, || {
            let mut v = Viewer::loading(PathBuf::from("/tmp/a.txt"));
            let r = draw(&mut v, 40, 5);
            assert!(r[1].starts_with(" Loading…"), "{:?}", r[1]);
            assert!(r[4].starts_with(" Esc/q/F3/F10: close"), "{:?}", r[4]);
            v.set_preview(Preview::Text {
                lines: vec!["a".into(), "b".into()],
                truncated: true,
            });
            let r = draw(&mut v, 40, 5);
            assert!(r[0].contains("1/2 · truncated"), "{:?}", r[0]);
        });
    }

    #[test]
    fn english_messages_and_not_found() {
        with_lang(Lang::En, || {
            let mut v = Viewer::loading(PathBuf::from("/x"));
            v.set_preview(Preview::Binary {
                size: 5,
                kind: "ELF executable",
            });
            assert_eq!(
                v.content,
                Content::Message("Binary file (ELF executable, 5B)".into())
            );
            v.set_preview(Preview::Empty);
            assert_eq!(v.content, Content::Message("Empty file".into()));
            v.set_preview(Preview::Text {
                lines: vec!["abc".into()],
                truncated: false,
            });
            v.query = Some("zzz".into());
            v.apply(Action::SearchNext);
            assert_eq!(v.notice.as_deref(), Some("Not found: zzz"));
        });
    }
}
