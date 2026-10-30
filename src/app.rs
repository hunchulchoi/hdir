//! Application state and key handling (no terminal IO).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::widgets::{Block, Borders};

pub struct App {
    pub running: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self { running: true }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        match key.code {
            KeyCode::F(10) | KeyCode::Char('q') => self.running = false,
            _ => {}
        }
    }

    /// Minimal placeholder; the real layout is built elsewhere.
    pub fn draw(&self, frame: &mut Frame) {
        let block = Block::default().borders(Borders::ALL).title("hdir");
        frame.render_widget(block, frame.area());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    #[test]
    fn new_app_is_running() {
        assert!(App::new().running);
    }

    #[test]
    fn f10_quits() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::F(10), KeyEventKind::Press));
        assert!(!app.running);
    }

    #[test]
    fn q_quits() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Press));
        assert!(!app.running);
    }

    #[test]
    fn repeat_is_handled() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Repeat));
        assert!(!app.running);
    }

    #[test]
    fn other_keys_do_not_quit() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('x'), KeyEventKind::Press));
        app.handle_key(key(KeyCode::F(1), KeyEventKind::Press));
        app.handle_key(key(KeyCode::Enter, KeyEventKind::Press));
        assert!(app.running);
    }

    #[test]
    fn release_events_are_ignored() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Release));
        app.handle_key(key(KeyCode::F(10), KeyEventKind::Release));
        assert!(app.running);
    }

    #[test]
    fn draw_renders_title() {
        let app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("hdir"));
    }
}
