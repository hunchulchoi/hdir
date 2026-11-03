//! Full-screen scrollable text pages (help, key settings, config warnings).
//!
//! State and key handling are pure; keys arrive as keymap [`Action`]s from the
//! viewer context.

use std::path::Path;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::i18n::t;
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

/// Display name of an action (help screen).
pub fn action_label(a: Action) -> &'static str {
    match a {
        Action::CursorUp => t("위로 이동", "Move up"),
        Action::CursorDown => t("아래로 이동", "Move down"),
        Action::PageUp => t("페이지 위로", "Page up"),
        Action::PageDown => t("페이지 아래로", "Page down"),
        Action::Home => t("처음으로", "Go to top"),
        Action::End => t("끝으로", "Go to end"),
        Action::Enter => t("열기 / 실행", "Open / run"),
        Action::Parent => t("상위 디렉터리", "Parent directory"),
        Action::SwitchPanel => t("패널 전환", "Switch panel"),
        Action::ToggleSelect => t("선택 토글", "Toggle selection"),
        Action::ToggleHidden => t("숨김 파일 토글", "Toggle hidden files"),
        Action::View => t("보기", "View"),
        Action::Edit => t("편집", "Edit"),
        Action::Copy => t("복사", "Copy"),
        Action::Move => t("이동", "Move"),
        Action::Mkdir => t("새 폴더", "New folder"),
        Action::Rename => t("이름 변경", "Rename"),
        Action::Delete => t("삭제 (휴지통)", "Delete (to trash)"),
        Action::TrashView => t("휴지통 보기", "View trash"),
        Action::Menu => t("메뉴", "Menu"),
        Action::Help => t("도움말", "Help"),
        Action::Quit => t("종료", "Quit"),
        Action::QuitNoCd => t("종료 (cd 안 함)", "Quit (no cd)"),
        Action::Undo => t("되돌리기", "Undo"),
        Action::CommandLine => t("명령줄", "Command line"),
        Action::ToggleMode => t("모드 전환", "Switch mode"),
        Action::ConnectLeft => t("왼쪽 패널 연결", "Connect left panel"),
        Action::ConnectRight => t("오른쪽 패널 연결", "Connect right panel"),
        Action::OpenWith => t("연결 프로그램 선택", "Choose program"),
        Action::Favorites => t("즐겨찾기", "Favorites"),
        Action::History => t("최근 경로", "History"),
        Action::Tree => t("디렉터리 트리", "Directory tree"),
        Action::ToggleMarkdown => t(
            "서식 / 원문 전환 (Markdown)",
            "Formatted / source (Markdown)",
        ),
        Action::Expand => t("펼치기", "Expand"),
        Action::Collapse => t("접기 / 상위", "Collapse / up"),
        Action::CancelJob => t("작업 취소", "Cancel job"),
        Action::ScrollLeft => t("왼쪽으로 스크롤", "Scroll left"),
        Action::ScrollRight => t("오른쪽으로 스크롤", "Scroll right"),
        Action::Search => t("검색", "Search"),
        Action::SearchNext => t("다음 검색 결과", "Next match"),
        Action::Confirm => t("확인", "OK"),
        Action::Cancel => t("취소", "Cancel"),
    }
}

fn context_label(c: Context) -> &'static str {
    match c {
        Context::Panel => t("패널", "Panel"),
        Context::Trash => t("휴지통", "Trash"),
        Context::Viewer => t("뷰어 / 도움말", "Viewer / help"),
        Context::Dialog => t("대화상자", "Dialog"),
        Context::Menu => t("메뉴", "Menu"),
        Context::Tree => t("디렉터리 트리", "Directory tree"),
    }
}

const KEY_COL: usize = 26;

/// Help text: every context's bindings, generated from the live keymap.
pub fn help_lines(km: &Keymap) -> Vec<String> {
    help_lines_for(
        km,
        &[
            Context::Panel,
            Context::Trash,
            Context::Viewer,
            Context::Dialog,
            Context::Menu,
            Context::Tree,
        ],
    )
}

/// Help text for just the given contexts.
pub fn help_lines_for(km: &Keymap, contexts: &[Context]) -> Vec<String> {
    let mut out = Vec::new();
    for &ctx in contexts {
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
        Some(p) if file_exists => {
            out.push(crate::tr!("설정 파일: {}", "Config file: {}", p.display()))
        }
        Some(p) => out.push(crate::tr!(
            "설정 파일: {} (파일 없음 - 기본값 사용)",
            "Config file: {} (not found - using defaults)",
            p.display()
        )),
        None => out.push(
            t(
                "설정 파일: (설정 디렉터리를 알 수 없음)",
                "Config file: (config directory unknown)",
            )
            .to_string(),
        ),
    }
    out.push(crate::tr!("키 프리셋: {preset}", "Key preset: {preset}"));
    out.push(
        t(
            "[keys.bind] 등으로 재정의하고 '설정 다시 읽기'로 적용",
            "Override with [keys.bind] etc., apply with 'Reload config'",
        )
        .to_string(),
    );
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

#[cfg(test)]
mod en_tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    fn rows(s: &mut TextScreen, w: u16, h: u16) -> Vec<String> {
        let th = crate::theme::builtin("mdir").unwrap();
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, s, &th)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn english_help_screen_snapshot() {
        with_lang(Lang::En, || {
            let km = crate::keymap::build(&Default::default()).0;
            let mut s = TextScreen::new("Help", help_lines(&km), "Esc/q/F3/F10: close".into());
            let r = rows(&mut s, 50, 8);
            assert!(r[0].starts_with("┌ Help "), "{:?}", r[0]);
            assert_eq!(r[1].trim_matches(['│', ' ']), "[Panel]");
            assert!(r[2].contains("Move up"), "{:?}", r[2]);
            assert!(r[7].contains("Esc/q/F3/F10: close"), "{:?}", r[7]);
        });
    }

    #[test]
    fn english_help_descriptions_start_in_one_column() {
        with_lang(Lang::En, || {
            let km = crate::keymap::build(&Default::default()).0;
            let lines = help_lines(&km);
            assert!(
                lines
                    .iter()
                    .all(|l| !l.chars().any(|c| ('가'..='힣').contains(&c)))
            );
            for l in lines.iter().filter(|l| l.starts_with("  ")) {
                // keys are padded to KEY_COL after the 2-space indent
                let desc_col = 2 + KEY_COL;
                assert!(UnicodeWidthStr::width(l.as_str()) > desc_col, "{l:?}");
            }
            assert!(lines.contains(&"[Dialog]".to_string()));
            assert!(lines.iter().any(|l| l.ends_with("Toggle hidden files")));
        });
    }

    #[test]
    fn english_key_settings_header() {
        with_lang(Lang::En, || {
            let km = crate::keymap::build(&Default::default()).0;
            let l = key_settings_lines(None, false, "mdir", &km);
            assert_eq!(l[0], "Config file: (config directory unknown)");
            assert_eq!(l[1], "Key preset: mdir");
        });
    }
}
