//! Full-screen scrollable text pages (help, key settings, config warnings).
//!
//! State and key handling are pure; keys arrive as keymap [`Action`]s from the
//! viewer context.

use std::path::Path;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::keymap::{Action, Context, Keymap};
use crate::theme::Theme;
use crate::ui::fit_width;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextScreen {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub footer: String,
    /// Visible rows as of the last render (page size for PageUp/PageDown).
    rows: usize,
}

impl TextScreen {
    pub fn new(title: &str, lines: Vec<String>, footer: String) -> Self {
        Self {
            title: title.to_string(),
            lines,
            scroll: 0,
            footer,
            rows: 10,
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.rows)
    }

    /// Apply a viewer-context action; returns true when the screen should close.
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
            _ => {}
        }
        false
    }
}

pub fn render(frame: &mut Frame, screen: &mut TextScreen, theme: &Theme) {
    let area = frame.area();
    let base = Style::default().fg(theme.fg).bg(theme.bg);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_active).bg(theme.bg))
        .title(format!(" {} ", screen.title))
        .title_bottom(format!(" {} ", screen.footer))
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    screen.rows = inner.height as usize;
    screen.scroll = screen.scroll.min(screen.max_scroll());
    let w = inner.width as usize;
    let lines: Vec<Line> = screen
        .lines
        .iter()
        .skip(screen.scroll)
        .take(screen.rows)
        .map(|l| Line::styled(fit_width(l, w), base))
        .collect();
    frame.render_widget(Paragraph::new(lines).style(base), inner);
}

/// Korean display name of an action (help screen).
pub fn action_label(a: Action) -> &'static str {
    match a {
        Action::CursorUp => "위로 이동",
        Action::CursorDown => "아래로 이동",
        Action::PageUp => "페이지 위로",
        Action::PageDown => "페이지 아래로",
        Action::Home => "처음으로",
        Action::End => "끝으로",
        Action::Enter => "열기 / 실행",
        Action::Parent => "상위 디렉터리",
        Action::SwitchPanel => "패널 전환",
        Action::ToggleSelect => "선택 토글",
        Action::ToggleHidden => "숨김 파일 토글",
        Action::View => "보기",
        Action::Edit => "편집",
        Action::Copy => "복사",
        Action::Move => "이동",
        Action::Mkdir => "새 폴더",
        Action::Rename => "이름 변경",
        Action::Delete => "삭제 (휴지통)",
        Action::TrashView => "휴지통 보기",
        Action::Menu => "메뉴",
        Action::Help => "도움말",
        Action::Quit => "종료",
        Action::QuitNoCd => "종료 (cd 안 함)",
        Action::Undo => "되돌리기",
        Action::CommandLine => "명령줄",
        Action::ToggleMode => "모드 전환",
        Action::ConnectLeft => "왼쪽 패널 연결",
        Action::ConnectRight => "오른쪽 패널 연결",
        Action::OpenWith => "연결 프로그램 선택",
        Action::CancelJob => "작업 취소",
        Action::ScrollLeft => "왼쪽으로 스크롤",
        Action::ScrollRight => "오른쪽으로 스크롤",
        Action::Search => "검색",
        Action::SearchNext => "다음 검색 결과",
        Action::Confirm => "확인",
        Action::Cancel => "취소",
    }
}

fn context_label(c: Context) -> &'static str {
    match c {
        Context::Panel => "패널",
        Context::Trash => "휴지통",
        Context::Viewer => "뷰어 / 도움말",
        Context::Dialog => "대화상자",
        Context::Menu => "메뉴",
    }
}

const KEY_COL: usize = 26;

/// Help text: every context's bindings, generated from the live keymap.
pub fn help_lines(km: &Keymap) -> Vec<String> {
    let mut out = Vec::new();
    for ctx in [
        Context::Panel,
        Context::Trash,
        Context::Viewer,
        Context::Dialog,
        Context::Menu,
    ] {
        let rows = km.describe(ctx);
        if rows.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("[{}]", context_label(ctx)));
        for (keys, action) in rows {
            out.push(format!(
                "  {}{}",
                fit_width(&keys, KEY_COL),
                action_label(action)
            ));
        }
    }
    out
}

/// Key settings page: config file location, preset, then all bindings.
pub fn key_settings_lines(
    config_path: Option<&Path>,
    file_exists: bool,
    preset: &str,
    km: &Keymap,
) -> Vec<String> {
    let mut out = Vec::new();
    match config_path {
        Some(p) if file_exists => out.push(format!("설정 파일: {}", p.display())),
        Some(p) => out.push(format!(
            "설정 파일: {} (파일 없음 - 기본값 사용)",
            p.display()
        )),
        None => out.push("설정 파일: (설정 디렉터리를 알 수 없음)".to_string()),
    }
    out.push(format!("키 프리셋: {preset}"));
    out.push("[keys.bind] 등으로 재정의하고 '설정 다시 읽기'로 적용".to_string());
    out.push(String::new());
    out.extend(help_lines(km));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeysConfig;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn th() -> Theme {
        crate::theme::builtin("mdir").unwrap()
    }

    fn km(src: &str) -> Keymap {
        let cfg: KeysConfig = toml::from_str(src).unwrap();
        crate::keymap::build(&cfg).0
    }

    fn numbered(n: usize) -> TextScreen {
        TextScreen::new(
            "T",
            (0..n).map(|i| format!("line{i}")).collect(),
            "Esc 닫기".into(),
        )
    }

    fn draw(s: &mut TextScreen, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, s, &th())).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn render_shows_title_footer_and_first_lines() {
        let mut s = numbered(50);
        let rows = draw(&mut s, 40, 8);
        assert!(rows[0].contains(" T "), "{}", rows[0]);
        assert!(
            rows[7].contains("Esc 닫기") || rows[7].contains("Esc"),
            "{}",
            rows[7]
        );
        assert!(rows[1].contains("line0") && rows[6].contains("line5"));
        assert!(!rows.join("").contains("line6"));
    }

    #[test]
    fn scroll_keys_move_and_clamp() {
        let mut s = numbered(20);
        draw(&mut s, 40, 8); // 6 visible rows
        assert!(!s.apply(Action::CursorUp));
        assert_eq!(s.scroll, 0);
        s.apply(Action::CursorDown);
        assert_eq!(s.scroll, 1);
        s.apply(Action::PageDown);
        assert_eq!(s.scroll, 7);
        s.apply(Action::End);
        assert_eq!(s.scroll, 14);
        s.apply(Action::CursorDown);
        assert_eq!(s.scroll, 14, "clamped at the last page");
        s.apply(Action::PageUp);
        assert_eq!(s.scroll, 8);
        s.apply(Action::Home);
        assert_eq!(s.scroll, 0);
        let rows = draw(&mut s, 40, 8);
        assert!(rows[1].contains("line0"));
    }

    #[test]
    fn scrolled_render_shows_the_window() {
        let mut s = numbered(20);
        draw(&mut s, 40, 8);
        s.apply(Action::End);
        let rows = draw(&mut s, 40, 8);
        assert!(
            rows[1].contains("line14") && rows[6].contains("line19"),
            "{rows:?}"
        );
    }

    #[test]
    fn short_content_never_scrolls() {
        let mut s = numbered(3);
        draw(&mut s, 40, 10);
        s.apply(Action::End);
        s.apply(Action::PageDown);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn quit_and_cancel_close() {
        let mut s = numbered(3);
        assert!(s.apply(Action::Quit));
        assert!(s.apply(Action::Cancel));
        assert!(!s.apply(Action::Help));
    }

    #[test]
    fn render_uses_theme_colors_and_survives_tiny_areas() {
        let mut s = numbered(3);
        let mut t = Terminal::new(TestBackend::new(40, 8)).unwrap();
        t.draw(|f| render(f, &mut s, &th())).unwrap();
        assert_eq!(t.backend().buffer()[(5, 3)].bg, th().bg);
        let mut t = Terminal::new(TestBackend::new(3, 2)).unwrap();
        t.draw(|f| render(f, &mut s, &th())).unwrap();
    }

    fn find<'a>(lines: &'a [String], needle: &str) -> Option<&'a String> {
        lines.iter().find(|l| l.contains(needle))
    }

    #[test]
    fn help_lists_default_bindings_with_labels() {
        let h = help_lines(&Keymap::preset("mdir").unwrap());
        assert_eq!(h[0], "[패널]");
        let copy = find(&h, "복사").unwrap();
        assert!(copy.contains("F5, Alt+5"), "{copy}");
        assert!(find(&h, "[휴지통]").is_some() && find(&h, "[대화상자]").is_some());
        assert!(find(&h, "Alt+F8").is_some());
    }

    #[test]
    fn help_reflects_remaps() {
        let h = help_lines(&km("[bind]\ncopy = [\"c\"]\nview = []\n"));
        let copy = find(&h, "복사").unwrap();
        assert!(copy.contains('c') && !copy.contains("F5"), "{copy}");
        assert!(
            !h.iter().any(|l| l.trim_end().ends_with("  보기")),
            "disabled action is not listed"
        );
    }

    #[test]
    fn key_settings_show_path_preset_and_bindings() {
        let p = PathBuf::from("/home/u/.config/hdir/config.toml");
        let k = Keymap::preset("mdir").unwrap();
        let l = key_settings_lines(Some(&p), true, "mdir", &k);
        assert_eq!(l[0], format!("설정 파일: {}", p.display()));
        assert_eq!(l[1], "키 프리셋: mdir");
        assert!(find(&l, "F5, Alt+5").is_some());
        let l = key_settings_lines(Some(&p), false, "vim", &k);
        assert!(l[0].contains("파일 없음"), "{}", l[0]);
        assert_eq!(l[1], "키 프리셋: vim");
        let l = key_settings_lines(None, false, "mdir", &k);
        assert!(l[0].contains("알 수 없음"));
    }
}
