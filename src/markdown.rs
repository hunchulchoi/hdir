//! Markdown to styled terminal lines for the F3 viewer.
//!
//! Text blocks (paragraphs, headings, list items, quotes) wrap to the
//! width; code blocks and tables keep their lines (the viewer scrolls them
//! sideways). HTML is shown as text and never interpreted; links and images
//! show their target. Pure: no IO.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Longest link target shown (`…` after); a huge URL used many times
/// cannot blow up the screen.
pub const URL_MAX: usize = 200;
/// Text rendered at most; the rest is left out with a note.
pub const MAX_OUTPUT: usize = 8 << 20;
/// Quote and list levels drawn in a line's prefix; deeper ones add nothing.
pub const MAX_DEPTH: usize = 16;

/// Widest a table column gets; longer cells are cut with `…`.
pub const COL_MAX_WIDTH: usize = 80;
/// Widest a table line gets; columns past it are left out (`…`).
pub const TABLE_MAX_WIDTH: usize = 1000;

/// `line` cut to `width` columns, ending in `…` when cut.
fn cut_line(line: &MdLine, width: usize) -> MdLine {
    if plain(line).width() <= width {
        return line.clone();
    }
    let mut out: MdLine = Vec::new();
    let mut used = 0;
    'outer: for (text, st) in line {
        for g in text.graphemes(true) {
            if used + g.width() > width.saturating_sub(1) {
                break 'outer;
            }
            used += g.width();
            push_seg(&mut out, g, *st);
        }
    }
    out.push(("…".into(), Style::of(Kind::Table)));
    out
}

/// What a piece of text is, for its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Heading(u8),
    Code,
    CodeBlock,
    /// `│ ` of a quote, list bullets and numbers.
    Marker,
    Link,
    /// A link's or image's target.
    Url,
    Rule,
    /// Table lines and separators.
    Table,
    /// HTML shown as plain text.
    Html,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub kind: Kind,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

impl Style {
    const fn of(kind: Kind) -> Self {
        Style {
            kind,
            bold: false,
            italic: false,
            strike: false,
        }
    }
}

pub type MdLine = Vec<(String, Style)>;

/// The plain text of a line.
pub fn plain(line: &MdLine) -> String {
    line.iter().map(|(t, _)| t.as_str()).collect()
}

/// A container a block sits in: quotes and list items add a prefix.
#[derive(Debug, Clone)]
enum Frame {
    Quote,
    /// A list item: its marker (shown on its first line) and the indent
    /// of its other lines.
    Item {
        marker: Option<String>,
        indent: usize,
    },
}

#[derive(Default)]
struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<MdLine>>,
    head_rows: usize,
    in_head: bool,
}

struct Builder {
    width: usize,
    out: Vec<MdLine>,
    /// Inline content of the current text block.
    inline: MdLine,
    frames: Vec<Frame>,
    /// Numbers of the open ordered lists (`None`: bullets).
    lists: Vec<Option<u64>>,
    bold: usize,
    italic: usize,
    strike: usize,
    heading: Option<u8>,
    /// Targets of the open links / images, and where their text starts
    /// (segment index, byte offset in it) in the current inline buffer.
    links: Vec<(String, usize, usize)>,
    in_image: usize,
    code: Option<String>,
    table: Option<Table>,
    cell: MdLine,
    /// Bytes of text taken in so far, and whether [`MAX_OUTPUT`] was hit.
    size: usize,
    stopped: bool,
}

/// Lines for `src` laid out `width` cells wide (at least 10).
pub fn render(src: &str, width: usize) -> Vec<MdLine> {
    let width = width.max(10);
    let mut b = Builder {
        width,
        out: Vec::new(),
        inline: Vec::new(),
        frames: Vec::new(),
        lists: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        heading: None,
        links: Vec::new(),
        in_image: 0,
        code: None,
        table: None,
        cell: Vec::new(),
        size: 0,
        stopped: false,
    };
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for ev in Parser::new_ext(src, opts) {
        if b.stopped {
            break;
        }
        b.event(ev);
    }
    b.code = None;
    b.table = None;
    b.flush();
    while b.out.last().is_some_and(|l| l.is_empty()) {
        b.out.pop();
    }
    if b.stopped {
        b.out.push(Vec::new());
        b.out.push(vec![(
            crate::i18n::t(
                "… 문서가 길어 여기까지 표시함",
                "… the document is too long, shown up to here",
            )
            .to_string(),
            Style::of(Kind::Url),
        )]);
    }
    b.out
}

impl Builder {
    fn style(&self, kind: Kind) -> Style {
        let kind = match (kind, self.heading) {
            (Kind::Text, Some(h)) => Kind::Heading(h),
            (Kind::Text, None) if !self.links.is_empty() && self.in_image == 0 => Kind::Link,
            (k, _) => k,
        };
        Style {
            kind,
            bold: self.bold > 0 || matches!(kind, Kind::Heading(_)),
            italic: self.italic > 0,
            strike: self.strike > 0,
        }
    }

    /// Where the next text goes in the current buffer: (segment, offset).
    fn text_mark(&self) -> (usize, usize) {
        let target = if self.table.is_some() {
            &self.cell
        } else {
            &self.inline
        };
        match target.last() {
            Some((t, _)) => (target.len() - 1, t.len()),
            None => (0, 0),
        }
    }

    /// Count `n` more bytes toward [`MAX_OUTPUT`].
    fn grow(&mut self, n: usize) {
        self.size += n;
        if self.size > MAX_OUTPUT {
            self.stopped = true;
        }
    }

    fn push_text(&mut self, text: &str, kind: Kind) {
        if self.stopped {
            return;
        }
        // never more than the budget left, cut on a character boundary
        let room = MAX_OUTPUT.saturating_sub(self.size);
        let text = if text.len() > room {
            let mut end = room;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };
        self.grow(text.len());
        if self.size >= MAX_OUTPUT {
            self.stopped = true;
        }
        let st = self.style(kind);
        let target = if self.table.is_some() {
            &mut self.cell
        } else {
            &mut self.inline
        };
        match target.last_mut() {
            Some((t, s)) if *s == st => t.push_str(text),
            _ => target.push((text.to_string(), st)),
        }
    }

    /// One blank line between blocks (none at the very top).
    fn gap(&mut self) {
        if self.out.last().is_some_and(|l| !l.is_empty()) {
            let p = self.prefix(false);
            if p.is_empty() {
                self.out.push(Vec::new());
            } else {
                self.out.push(p);
            }
        }
    }

    /// The container prefix of a line; `first`: this block's first line
    /// (shows the list marker, which is then used up).
    /// Only the outer [`MAX_DEPTH`] levels are drawn, and the prefix counts
    /// toward [`MAX_OUTPUT`].
    fn prefix(&mut self, first: bool) -> MdLine {
        let mut p: MdLine = Vec::new();
        let n = self.frames.len();
        for (i, f) in self.frames.iter_mut().enumerate().take(MAX_DEPTH) {
            match f {
                Frame::Quote => p.push(("│ ".into(), Style::of(Kind::Marker))),
                Frame::Item { marker, indent } => {
                    let innermost = i + 1 == n;
                    match marker.take().filter(|_| first && innermost) {
                        Some(m) => p.push((m, Style::of(Kind::Marker))),
                        None => p.push((" ".repeat(*indent), Style::of(Kind::Text))),
                    }
                }
            }
        }
        let bytes = p.iter().map(|(t, _)| t.len()).sum();
        self.grow(bytes);
        p
    }

    fn prefix_width(&self) -> usize {
        self.frames
            .iter()
            .take(MAX_DEPTH)
            .map(|f| match f {
                Frame::Quote => 2,
                Frame::Item { indent, .. } => *indent,
            })
            .sum()
    }

    /// Wrap the pending inline text into lines.
    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let segs = std::mem::take(&mut self.inline);
        let room = self.width.saturating_sub(self.prefix_width()).max(4);
        let mut lines: Vec<MdLine> = vec![Vec::new()];
        let mut used = 0;
        for (text, st) in segs {
            for (i, part) in text.split('\n').enumerate() {
                if i > 0 {
                    lines.push(Vec::new());
                    used = 0;
                }
                for word in part.split_word_bounds() {
                    let w = word.width();
                    if used + w > room && used > 0 {
                        lines.push(Vec::new());
                        used = 0;
                        if word.trim().is_empty() {
                            continue;
                        }
                    }
                    if w <= room - used {
                        push_seg(lines.last_mut().unwrap(), word, st);
                        used += w;
                        continue;
                    }
                    // a word wider than the room: cut by cluster, one pass
                    let mut from = 0;
                    for (b, g) in word.grapheme_indices(true) {
                        let gw = g.width();
                        if used + gw > room && used > 0 {
                            push_seg(lines.last_mut().unwrap(), &word[from..b], st);
                            lines.push(Vec::new());
                            used = 0;
                            from = b;
                        }
                        used += gw;
                    }
                    push_seg(lines.last_mut().unwrap(), &word[from..], st);
                }
            }
        }
        let heading = self.heading;
        for (i, l) in lines.into_iter().enumerate() {
            if self.stopped && i > 0 {
                break;
            }
            let mut line = self.prefix(i == 0);
            line.extend(trim_end(l));
            self.out.push(line);
        }
        if let Some(h) = heading.filter(|h| *h <= 2) {
            // the underline as wide as the heading's longest line
            let w = self
                .out
                .last()
                .map_or(0, |l| plain(l).width())
                .saturating_sub(self.prefix_width());
            let mut line = self.prefix(false);
            let ch = if h == 1 { "═" } else { "─" };
            line.push((ch.repeat(w.max(1)), Style::of(Kind::Heading(h))));
            self.out.push(line);
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(code) = self.code.as_mut() {
                    code.push_str(&t);
                    self.grow(t.len());
                } else {
                    self.push_text(&t, Kind::Text);
                }
            }
            Event::Code(t) => self.push_text(&t, Kind::Code),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.push_text(&t, Kind::Code),
            Event::Html(t) | Event::InlineHtml(t) => {
                // shown, never run
                let t = t.trim_end_matches('\n');
                self.push_text(t, Kind::Html);
                if self.code.is_none() && self.table.is_none() && t.contains('\n') {
                    self.flush();
                }
            }
            Event::SoftBreak => self.push_text(" ", Kind::Text),
            Event::HardBreak => self.push_text("\n", Kind::Text),
            Event::Rule => {
                self.flush();
                self.gap();
                let mut line = self.prefix(false);
                let w = self.width.saturating_sub(self.prefix_width());
                line.push(("─".repeat(w), Style::of(Kind::Rule)));
                self.out.push(line);
            }
            Event::TaskListMarker(done) => {
                self.push_text(if done { "[x] " } else { "[ ] " }, Kind::Marker);
            }
            Event::FootnoteReference(name) => self.push_text(&format!("[^{name}]"), Kind::Url),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.flush();
                    if !self.starts_item() {
                        self.gap();
                    }
                }
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.gap();
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
                if let Some(h @ 3..) = self.heading {
                    self.push_text(&format!("{} ", "#".repeat(h as usize)), Kind::Heading(h));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.gap();
                self.frames.push(Frame::Quote);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.gap();
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    let mut line = self.prefix(false);
                    line.push((format!("  {lang}"), Style::of(Kind::Url)));
                    self.out.push(line);
                }
                self.code = Some(String::new());
            }
            Tag::HtmlBlock => {
                self.flush();
                self.gap();
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                let indent = marker.width();
                self.frames.push(Frame::Item {
                    marker: Some(marker),
                    indent,
                });
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                let at = self.text_mark();
                self.links.push((dest_url.to_string(), at.0, at.1));
            }
            Tag::Image { dest_url, .. } => {
                let at = self.text_mark();
                self.links.push((dest_url.to_string(), at.0, at.1));
                self.in_image += 1;
                self.push_text(crate::i18n::t("[그림: ", "[image: "), Kind::Marker);
            }
            Tag::Table(aligns) => {
                self.flush();
                self.gap();
                self.table = Some(Table {
                    aligns,
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.cell.clear(),
            Tag::FootnoteDefinition(name) => {
                self.flush();
                self.gap();
                self.push_text(&format!("[^{name}]: "), Kind::Url);
            }
            _ => {}
        }
    }

    /// A paragraph that opens a list item: no gap before its first line.
    fn starts_item(&self) -> bool {
        matches!(
            self.frames.last(),
            Some(Frame::Item {
                marker: Some(_),
                ..
            })
        )
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.table.is_none() {
                    self.flush();
                }
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.frames.pop();
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                for l in code.strip_suffix('\n').unwrap_or(&code).split('\n') {
                    let mut line = self.prefix(false);
                    line.push((
                        format!("  {}", l.replace('\t', "    ")),
                        Style::of(Kind::CodeBlock),
                    ));
                    self.out.push(line);
                }
            }
            TagEnd::HtmlBlock => self.flush(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                // an item with no text of its own still shows its marker
                if let Some(Frame::Item {
                    marker: Some(_), ..
                }) = self.frames.last()
                {
                    let line = self.prefix(true);
                    self.out.push(line);
                }
                self.frames.pop();
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                let (url, seg, off) = self.links.pop().unwrap_or_default();
                // only the link's own text (never the whole paragraph)
                let target = if self.table.is_some() {
                    &self.cell
                } else {
                    &self.inline
                };
                let mut text = String::new();
                for (i, (t, _)) in target.iter().enumerate().skip(seg) {
                    text.push_str(if i == seg {
                        t.get(off..).unwrap_or("")
                    } else {
                        t
                    });
                }
                if !url.is_empty() && text != url {
                    self.push_text(&format!(" ({})", short_url(&url)), Kind::Url);
                }
            }
            TagEnd::Image => {
                let (url, ..) = self.links.pop().unwrap_or_default();
                self.in_image = self.in_image.saturating_sub(1);
                self.push_text("] ", Kind::Marker);
                self.push_text(&short_url(&url), Kind::Url);
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cell);
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = false;
                    t.head_rows = t.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_lines(t);
                }
            }
            TagEnd::FootnoteDefinition => self.flush(),
            _ => {}
        }
    }

    /// `│ a │ b │` rows with a `├─┼─┤` line under the header; no wrapping.
    fn table_lines(&mut self, t: Table) {
        let all = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; all];
        for row in &t.rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(plain(c).width().min(COL_MAX_WIDTH));
            }
        }
        // only the columns that fit in TABLE_MAX_WIDTH; a `…` column after
        let mut total = 1;
        let cols = widths
            .iter()
            .take_while(|w| {
                total += *w + 3;
                total <= TABLE_MAX_WIDTH
            })
            .count()
            .max(1)
            .min(all);
        let more = cols < all;
        widths.truncate(cols);
        let bar = Style::of(Kind::Table);
        let rule = |l: &str, m: &str, r: &str| -> String {
            let mid: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            let tail = if more { "…" } else { "" };
            format!("{l}{}{r}{tail}", mid.join(m))
        };
        let top = rule("┌", "┬", "┐");
        let mut line = self.prefix(false);
        line.push((top, bar));
        self.out.push(line);
        for (ri, row) in t.rows.iter().enumerate() {
            let mut line = self.prefix(false);
            line.push(("│".into(), bar));
            for (i, w) in widths.iter().enumerate() {
                let empty = Vec::new();
                let cell = cut_line(row.get(i).unwrap_or(&empty), *w);
                let cell = &cell;
                let pad = w - plain(cell).width();
                let (l, r) = match t.aligns.get(i) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                line.push((" ".repeat(l + 1), Style::of(Kind::Text)));
                for (text, st) in cell {
                    let mut st = *st;
                    if ri < t.head_rows {
                        st.bold = true;
                    }
                    line.push((text.clone(), st));
                }
                line.push((" ".repeat(r + 1), Style::of(Kind::Text)));
                line.push(("│".into(), bar));
            }
            if more {
                line.push(("…".into(), bar));
            }
            self.out.push(line);
            if ri + 1 == t.head_rows {
                let mut line = self.prefix(false);
                line.push((rule("├", "┼", "┤"), bar));
                self.out.push(line);
            }
        }
        let mut line = self.prefix(false);
        line.push((rule("└", "┴", "┘"), bar));
        self.out.push(line);
    }
}

/// `url` cut to [`URL_MAX`] columns with `…`.
fn short_url(url: &str) -> String {
    // one pass that stops just past URL_MAX cells: a huge URL used many
    // times costs no more than a short one
    let (mut used, mut cut) = (0, None);
    for (i, g) in url.grapheme_indices(true) {
        used += g.width();
        if used > URL_MAX - 1 && cut.is_none() {
            cut = Some(i);
        }
        if used > URL_MAX {
            let mut out = url[..cut.unwrap_or(i)].to_string();
            out.push('…');
            return out;
        }
    }
    url.to_string()
}

fn push_seg(line: &mut MdLine, text: &str, st: Style) {
    if text.is_empty() {
        return;
    }
    match line.last_mut() {
        Some((t, s)) if *s == st => t.push_str(text),
        _ => line.push((text.to_string(), st)),
    }
}

/// Without trailing spaces.
fn trim_end(mut l: MdLine) -> MdLine {
    while let Some((t, _)) = l.last_mut() {
        let k = t.trim_end().len();
        if k == 0 {
            l.pop();
        } else {
            t.truncate(k);
            break;
        }
    }
    l
}

/// Whether a file name is Markdown by its extension.
pub fn is_markdown(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "md" | "markdown" | "mdown" | "mkd" | "mdx"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(src: &str, w: usize) -> Vec<String> {
        render(src, w).iter().map(plain).collect()
    }

    fn style_of(lines: &[MdLine], needle: &str) -> Style {
        lines
            .iter()
            .flatten()
            .find(|(t, _)| t.contains(needle))
            .map(|(_, s)| *s)
            .unwrap_or_else(|| panic!("{needle} not found"))
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        let src = "# 제목\n\n본문 **굵게** *기울임* ~~취소~~ `코드`\n\n## Sub\n\n### 셋째";
        let lines = render(src, 40);
        assert_eq!(
            lines.iter().map(plain).collect::<Vec<_>>(),
            [
                "제목",
                "════",
                "",
                "본문 굵게 기울임 취소 코드",
                "",
                "Sub",
                "───",
                "",
                "### 셋째",
            ]
        );
        assert_eq!(style_of(&lines, "제목").kind, Kind::Heading(1));
        assert!(style_of(&lines, "굵게").bold);
        assert!(style_of(&lines, "기울임").italic);
        assert!(style_of(&lines, "취소").strike);
        assert_eq!(style_of(&lines, "코드").kind, Kind::Code);
    }

    #[test]
    fn paragraphs_wrap_to_the_width_and_keep_korean_whole() {
        let src = "가나다 라마바 사아자 차카타 파하 abc defgh";
        for line in text(src, 12) {
            assert!(line.width() <= 12, "{line:?}");
        }
        assert_eq!(text(src, 12).join(" ").replace("  ", " "), src);
        // a word wider than the screen is cut, nothing lost
        let long = "x".repeat(25);
        let got = text(&long, 10);
        assert_eq!(got.concat(), long);
        assert!(got.iter().all(|l| l.width() <= 10));
    }

    #[test]
    fn lists_nest_with_markers_and_tasks() {
        let src = "- 하나\n- 둘\n  - 안쪽\n- [x] 끝남\n\n1. first\n2. second";
        assert_eq!(
            text(src, 40),
            [
                "• 하나",
                "• 둘",
                "  • 안쪽",
                "• [x] 끝남",
                "",
                "1. first",
                "2. second"
            ]
        );
    }

    #[test]
    fn quotes_rules_links_and_images() {
        let src = "> 인용\n> 둘째 줄\n\n---\n\n[문서](https://x.y/z) <https://a.b>\n\n![그림 설명](<img/사진 1.png>)";
        let lines = text(src, 20);
        assert_eq!(lines[0], "│ 인용 둘째 줄");
        assert_eq!(lines[2], "─".repeat(20));
        assert!(
            lines.iter().any(|l| l.contains("문서 (https://x.y/z)")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "https://a.b"),
            "a bare link once: {lines:?}"
        );
        let wide = text(src, 60);
        let img = wide.iter().find(|l| l.contains("그림 설명")).unwrap();
        assert!(
            img.contains("[그림: 그림 설명]") && img.contains("img/사진"),
            "{img}"
        );
        let styled = render(src, 20);
        assert_eq!(style_of(&styled, "문서").kind, Kind::Link);
        assert_eq!(style_of(&styled, "https://x.y/z").kind, Kind::Url);
    }

    #[test]
    fn code_blocks_keep_lines_and_indentation_and_never_wrap() {
        let src = "```rust\nfn main() {\n\tlet x = \"아주 긴 줄 아주 긴 줄 아주 긴 줄\";\n}\n```";
        let lines = render(src, 20);
        let t: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(
            t,
            [
                "  rust",
                "  fn main() {",
                "      let x = \"아주 긴 줄 아주 긴 줄 아주 긴 줄\";",
                "  }"
            ]
        );
        assert_eq!(style_of(&lines, "fn main").kind, Kind::CodeBlock);
    }

    #[test]
    fn tables_align_columns_and_are_not_wrapped() {
        let src = "| 이름 | 크기 |\n|:---|---:|\n| a.txt | 5 |\n| 긴 파일 이름.md | 12345 |";
        let t = text(src, 10);
        assert_eq!(
            t,
            [
                "┌─────────────────┬───────┐",
                "│ 이름            │  크기 │",
                "├─────────────────┼───────┤",
                "│ a.txt           │     5 │",
                "│ 긴 파일 이름.md │ 12345 │",
                "└─────────────────┴───────┘",
            ]
        );
        let widths: Vec<usize> = t.iter().map(|l| l.width()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
        assert!(style_of(&render(src, 10), "이름").bold, "header bold");
    }

    #[test]
    fn a_huge_cell_or_many_columns_never_blow_up_the_table() {
        // one 64 KiB cell and many short rows
        let mut src = format!("| a | b |\n|---|---|\n| {} | x |\n", "w".repeat(65_536));
        for _ in 0..2_000 {
            src.push_str("| s | t |\n");
        }
        let lines = render(&src, 80);
        let widest = lines.iter().map(|l| plain(l).width()).max().unwrap();
        assert!(widest <= TABLE_MAX_WIDTH + 4, "{widest}");
        assert!(
            lines.iter().any(|l| plain(l).contains('…')),
            "cut cells are marked"
        );
        // a thousand columns
        let head = format!("|{}\n|{}\n", " c |".repeat(1_000), "---|".repeat(1_000));
        let lines = render(&head, 80);
        let widest = lines.iter().map(|l| plain(l).width()).max().unwrap();
        assert!(widest <= TABLE_MAX_WIDTH + 4, "{widest}");
    }

    #[test]
    fn many_links_in_one_paragraph_render_quickly() {
        let src = "[x](u) ".repeat(50_000);
        let start = std::time::Instant::now();
        let lines = render(&src, 80);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert!(plain(&lines[0]).starts_with("x (u) x (u)"));
        // a bare link is not repeated after its text
        assert_eq!(
            text("<https://a.b> and [https://c.d](https://c.d)", 80),
            ["https://a.b and https://c.d"]
        );
    }

    #[test]
    fn deep_quotes_count_toward_the_output_cap() {
        // 1,000 nested quotes around a 1 MiB paragraph: every wrapped line
        // would carry a 1,000-part prefix
        let src = format!("{}{}", "> ".repeat(1_000), "word ".repeat(200_000));
        let start = std::time::Instant::now();
        let lines = render(&src, 80);
        let segs: usize = lines.iter().map(|l| l.len()).sum();
        let total: usize = lines.iter().map(|l| plain(l).len()).sum();
        assert!(total <= MAX_OUTPUT + 64 * 1024, "{total}");
        assert!(segs <= 4_000_000, "{segs} segments");
        assert!(lines.iter().all(|l| l.len() <= MAX_DEPTH + 8));
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        // a few levels still show in full
        assert_eq!(text("> > a", 80), ["│ │ a"]);
    }

    #[test]
    fn urls_are_cut_at_the_cap_without_splitting_wide_characters() {
        let fits = "a".repeat(URL_MAX);
        assert_eq!(short_url(&fits), fits);
        let over = "a".repeat(URL_MAX + 1);
        assert_eq!(short_url(&over), format!("{}…", "a".repeat(URL_MAX - 1)));
        // a wide character that would cross the cut is left out whole
        let wide = format!("{}가{}", "a".repeat(URL_MAX - 2), "b".repeat(5));
        assert_eq!(short_url(&wide), format!("{}…", "a".repeat(URL_MAX - 2)));
        assert!(short_url(&wide).width() <= URL_MAX);
    }

    #[test]
    fn huge_urls_and_endless_output_are_capped() {
        // a 1 MiB link target referenced many times
        let url = "u".repeat(1 << 20);
        let mut src = format!("[r]: {url}\n\n");
        for _ in 0..3_000 {
            src.push_str("[a][r] ");
        }
        let lines = render(&src, 80);
        let total: usize = lines.iter().map(|l| plain(l).len()).sum();
        assert!(total <= MAX_OUTPUT + 1024, "{total}");
        let all: String = lines.iter().map(plain).collect();
        assert!(
            all.contains(&format!("({}…)", "u".repeat(URL_MAX - 1))),
            "a cut URL is still shown"
        );
        assert!(!all.contains(&"u".repeat(URL_MAX + 1)));
        // endless text stops with a note
        let long = "word ".repeat(3_000_000);
        let lines = render(&long, 80);
        let total: usize = lines.iter().map(|l| plain(l).len()).sum();
        assert!(total <= MAX_OUTPUT + 1024, "{total}");
        assert!(
            plain(lines.last().unwrap()).contains("…"),
            "{:?}",
            lines.last()
        );
    }

    #[test]
    fn a_very_long_word_wraps_in_linear_time() {
        let word = "가".repeat(200_000);
        let start = std::time::Instant::now();
        let lines = render(&word, 80);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(lines.len(), 200_000 * 2 / 80);
        assert!(lines.iter().all(|l| plain(l).width() == 80));
    }

    #[test]
    fn html_is_shown_as_text_never_run() {
        let src = "<script>alert(1)</script>\n\n텍스트 <b>굵게</b>";
        let lines = render(src, 40);
        let t: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            t.iter().any(|l| l.contains("<script>alert(1)</script>")),
            "{t:?}"
        );
        assert!(t.iter().any(|l| l.contains("<b>굵게</b>")), "{t:?}");
        assert_eq!(style_of(&lines, "<script>").kind, Kind::Html);
    }

    #[test]
    fn empty_and_broken_markdown_render_without_errors() {
        assert!(render("", 40).is_empty());
        for src in [
            "**unclosed",
            "[link](",
            "```\nno end",
            "| a |\n|",
            "> > > deep",
            "-",
            "#",
        ] {
            let _ = render(src, 40);
            let _ = render(src, 1);
        }
        assert_eq!(text("**unclosed", 40), ["**unclosed"]);
    }

    #[test]
    fn markdown_files_by_extension() {
        for p in ["README.md", "a.MARKDOWN", "x.mdown"] {
            assert!(is_markdown(std::path::Path::new(p)), "{p}");
        }
        for p in ["a.txt", "md", ".md"] {
            assert!(!is_markdown(std::path::Path::new(p)), "{p}");
        }
    }
}
