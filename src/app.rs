//! Application state and key handling (no terminal IO).

use std::ffi::OsString;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use crate::panel::{EnterResult, EntryKind, Panel};
use crate::ui::{self, View};

/// Rows taken by header (1) + status (1) + fkey bar (1) + panel borders (2).
const CHROME_ROWS: u16 = 5;

/// Visible entry rows inside a panel for a terminal of `total_height` rows.
pub fn panel_rows(total_height: u16) -> usize {
    total_height.saturating_sub(CHROME_ROWS) as usize
}

pub struct App {
    pub running: bool,
    pub panels: [Panel; 2],
    pub active: usize,
    pub status: Option<String>,
    pub shell: String,
    page_height: usize,
}

impl Default for App {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd.clone(), cwd)
    }
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        let shell = std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_string());
        Self {
            running: true,
            panels: [Panel::new(left), Panel::new(right)],
            active: 0,
            status: None,
            shell,
            page_height: 1,
        }
    }

    pub fn active_panel(&self) -> &Panel {
        &self.panels[self.active]
    }

    pub fn active_panel_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    pub fn other_panel(&self) -> &Panel {
        &self.panels[1 - self.active]
    }

    /// Selected names (sorted) in the active panel, else the current entry
    /// unless it is the `..` entry.
    pub fn selected_or_current(&self) -> Vec<OsString> {
        let panel = self.active_panel();
        if !panel.selected.is_empty() {
            let mut names: Vec<OsString> = panel.selected.iter().cloned().collect();
            names.sort();
            return names;
        }
        match panel.current() {
            Some(e) if e.kind != EntryKind::Parent => vec![e.os_name.clone()],
            _ => Vec::new(),
        }
    }

    /// Handle a terminal event; returns true when the screen needs a redraw.
    pub fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key);
                true
            }
            Event::Resize(..) => true,
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let page = self.page_height.max(1);
        match key.code {
            KeyCode::F(10) | KeyCode::Char('q') => self.running = false,
            KeyCode::Tab => self.active = 1 - self.active,
            KeyCode::Up => self.navigate(Panel::move_up),
            KeyCode::Down => self.navigate(Panel::move_down),
            KeyCode::PageUp => self.navigate(|p| p.page_up(page)),
            KeyCode::PageDown => self.navigate(|p| p.page_down(page)),
            KeyCode::Home => self.navigate(Panel::home),
            KeyCode::End => self.navigate(Panel::end),
            KeyCode::Backspace => self.navigate(|p| {
                p.go_parent();
            }),
            KeyCode::Char('.') => self.navigate(Panel::toggle_hidden),
            KeyCode::Enter => {
                self.status = None;
                if let EnterResult::Opened(path) = self.active_panel_mut().enter() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.status = Some(format!("열기: {name}"));
                } else {
                    self.show_panel_error();
                }
            }
            KeyCode::Char(' ') | KeyCode::Insert => {
                self.status = None;
                let panel = self.active_panel_mut();
                if let Some(e) = panel.current()
                    && e.kind != EntryKind::Parent
                {
                    let name = e.os_name.clone();
                    if !panel.selected.remove(&name) {
                        panel.selected.insert(name);
                    }
                }
                self.active_panel_mut().move_down();
            }
            _ => {}
        }
    }

    /// Run a navigation action on the active panel, clearing stale status and
    /// surfacing any panel error afterwards.
    fn navigate(&mut self, f: impl FnOnce(&mut Panel)) {
        self.status = None;
        f(self.active_panel_mut());
        self.show_panel_error();
    }

    fn show_panel_error(&mut self) {
        if let Some(err) = self.active_panel().error.clone() {
            self.status = Some(err);
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.page_height = panel_rows(frame.area().height).max(1);
        let rows = panel_rows(frame.area().height);
        for p in &mut self.panels {
            p.ensure_visible(rows);
        }
        let view = View {
            panels: [&self.panels[0], &self.panels[1]],
            active: self.active,
            status: self.status.as_deref(),
            shell: &self.shell,
            os: std::env::consts::OS,
        };
        ui::draw(frame, &view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::ffi::OsStr;
    use std::fs;
    use tempfile::TempDir;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(key(code, KeyEventKind::Press));
    }

    /// Layout: .., adir, bdir, f.txt (+ hidden .h). adir contains inner.txt.
    fn fixture() -> (TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("adir")).unwrap();
        fs::create_dir(root.join("bdir")).unwrap();
        fs::write(root.join("adir/inner.txt"), "x").unwrap();
        fs::write(root.join("f.txt"), "x").unwrap();
        fs::write(root.join(".h"), "x").unwrap();
        let app = App::new(root.to_path_buf(), root.to_path_buf());
        (tmp, app)
    }

    fn name_at(app: &App, i: usize) -> String {
        app.panels[i]
            .current()
            .map(|e| e.name.clone())
            .unwrap_or_default()
    }

    #[test]
    fn new_app_is_running() {
        let (_t, app) = fixture();
        assert!(app.running);
        assert_eq!(app.active, 0);
        assert!(!app.shell.is_empty());
    }

    #[test]
    fn f10_quits() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(10));
        assert!(!app.running);
    }

    #[test]
    fn q_quits() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.running);
    }

    #[test]
    fn repeat_is_handled() {
        let (_t, mut app) = fixture();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Repeat));
        assert!(!app.running);
    }

    #[test]
    fn other_keys_do_not_quit() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::F(1));
        press(&mut app, KeyCode::Enter);
        assert!(app.running);
    }

    #[test]
    fn release_events_are_ignored() {
        let (_t, mut app) = fixture();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Release));
        app.handle_key(key(KeyCode::F(10), KeyEventKind::Release));
        app.handle_key(key(KeyCode::Down, KeyEventKind::Release));
        app.handle_key(key(KeyCode::Tab, KeyEventKind::Release));
        assert!(app.running);
        assert_eq!(app.panels[0].cursor, 0);
        assert_eq!(app.active, 0);
    }

    #[test]
    fn tab_toggles_active_and_keys_affect_only_active() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[1].cursor, 1);
        assert_eq!(app.panels[0].cursor, 0);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 0);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[0].cursor, 2);
        assert_eq!(app.panels[1].cursor, 1);
    }

    #[test]
    fn active_and_other_panel_accessors() {
        let (_t, mut app) = fixture();
        app.panels[1].cursor = 2;
        assert_eq!(app.active_panel().cursor, 0);
        assert_eq!(app.other_panel().cursor, 2);
        app.active_panel_mut().cursor = 1;
        assert_eq!(app.panels[0].cursor, 1);
    }

    #[test]
    fn navigation_keys_move_cursor() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        assert_eq!(name_at(&app, 0), "adir");
        press(&mut app, KeyCode::Up);
        assert_eq!(name_at(&app, 0), "..");
        press(&mut app, KeyCode::End);
        assert_eq!(name_at(&app, 0), "f.txt");
        press(&mut app, KeyCode::Home);
        assert_eq!(app.panels[0].cursor, 0);
        app.page_height = 2;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.panels[0].cursor, 2);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.panels[0].cursor, 0);
    }

    #[test]
    fn enter_dir_and_backspace_returns_to_it() {
        let (tmp, mut app) = fixture();
        let root = fs::canonicalize(tmp.path()).unwrap();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.panels[0].cwd, root.join("adir"));
        assert_eq!(app.panels[1].cwd, root);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.panels[0].cwd, root);
        assert_eq!(name_at(&app, 0), "adir");
    }

    #[test]
    fn enter_file_sets_status_with_name() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Enter);
        assert!(app.status.as_deref().unwrap().contains("f.txt"));
    }

    #[test]
    fn navigation_clears_status() {
        let (_t, mut app) = fixture();
        app.status = Some("old".into());
        press(&mut app, KeyCode::Down);
        assert_eq!(app.status, None);
    }

    #[test]
    fn space_toggles_selection_and_advances() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panels[0].selected.contains(OsStr::new("adir")));
        assert_eq!(name_at(&app, 0), "bdir");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.panels[0].selected.contains(OsStr::new("adir")));
    }

    #[test]
    fn space_on_parent_does_not_select() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panels[0].selected.is_empty());
    }

    #[test]
    fn insert_selects_like_space() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Insert);
        assert!(app.panels[0].selected.contains(OsStr::new("adir")));
        assert_eq!(name_at(&app, 0), "bdir");
        let (_t2, mut app2) = fixture();
        press(&mut app2, KeyCode::Insert);
        assert!(app2.panels[0].selected.is_empty());
    }

    #[test]
    fn selected_or_current_variants() {
        let (_t, mut app) = fixture();
        assert!(app.selected_or_current().is_empty()); // on ..
        press(&mut app, KeyCode::End);
        assert_eq!(app.selected_or_current(), vec![OsString::from("f.txt")]);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' ')); // adir
        press(&mut app, KeyCode::Down); // skip bdir
        press(&mut app, KeyCode::Char(' ')); // f.txt
        press(&mut app, KeyCode::Home);
        assert_eq!(
            app.selected_or_current(),
            vec![OsString::from("adir"), OsString::from("f.txt")]
        );
    }

    #[test]
    fn hidden_toggle_drops_hidden_selection_from_selected_or_current() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('.')); // show hidden: .., adir, bdir, .h, f.txt
        for _ in 0..3 {
            press(&mut app, KeyCode::Down); // .h
        }
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.selected_or_current(), vec![OsString::from(".h")]);
        press(&mut app, KeyCode::Char('.')); // hide again
        assert!(app.active_panel().selected.is_empty());
        let cur = app.active_panel().current().unwrap().os_name.clone();
        assert_eq!(app.selected_or_current(), vec![cur]);
    }

    #[test]
    fn handle_event_reports_whether_redraw_needed() {
        let (_t, mut app) = fixture();
        assert!(app.handle_event(Event::Key(key(KeyCode::Down, KeyEventKind::Press))));
        assert!(app.handle_event(Event::Resize(80, 24)));
        assert!(!app.handle_event(Event::Key(key(KeyCode::Down, KeyEventKind::Release))));
        assert!(!app.handle_event(Event::FocusGained));
        assert_eq!(app.panels[0].cursor, 1);
    }

    #[test]
    fn dot_toggles_hidden_on_active_only() {
        let (_t, mut app) = fixture();
        let before = app.panels[0].show_hidden;
        press(&mut app, KeyCode::Char('.'));
        assert_ne!(app.panels[0].show_hidden, before);
        assert_eq!(app.panels[1].show_hidden, before);
    }

    #[test]
    fn panel_rows_matches_layout() {
        assert_eq!(panel_rows(24), 19);
        assert_eq!(panel_rows(3), 0);
        assert_eq!(panel_rows(0), 0);
    }

    #[test]
    fn draw_renders_both_panels_and_stores_page_height() {
        let (_t, mut app) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.page_height, 19);
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("hdir"));
        let dir_name = app.panels[0]
            .cwd
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(text.matches(dir_name.as_str()).count() >= 2);
    }
}
