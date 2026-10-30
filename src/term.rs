//! Terminal setup/teardown helpers.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Enter raw mode and the alternate screen, hide the cursor.
pub fn init() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, Hide) {
        let _ = restore();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(stdout)).inspect_err(|_| {
        let _ = restore();
    })
}

/// Leave the alternate screen, show the cursor, disable raw mode.
///
/// Idempotent: safe to call repeatedly or without a prior `init`. Raw-mode
/// disable failures (e.g. no tty) are ignored as this is best-effort cleanup.
pub fn restore() -> io::Result<()> {
    let _ = disable_raw_mode();
    execute!(io::stdout(), LeaveAlternateScreen, Show)
}

/// Install a panic hook that restores the terminal before the previous hook runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        previous(info);
    }));
}

/// Temporarily leave the TUI, run `f` (e.g. `$EDITOR`), then re-enter and clear.
pub fn suspend<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    f: impl FnOnce() -> T,
) -> io::Result<T> {
    restore()?;
    let result = f();
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        Hide,
        Clear(ClearType::All)
    )?;
    reenter(terminal, || {
        Terminal::new(CrosstermBackend::new(io::stdout()))
    })?;
    Ok(result)
}

/// Replace `terminal` with a freshly built one so the next draw repaints
/// everything.
///
/// `Terminal::clear` asks the terminal for the cursor position (DSR) and fails
/// when the answer doesn't arrive in time (slow ssh links, tmux, `script`),
/// which would leave the app unable to return to the TUI. A fullscreen
/// `Terminal::new` only reads the size, so rebuilding avoids that query.
fn reenter<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    make: impl FnOnce() -> Result<Terminal<B>, B::Error>,
) -> Result<(), B::Error> {
    *terminal = make()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};

    /// A backend that fails any cursor-position query, like a terminal that
    /// never answers the DSR request (slow ssh, tmux, `script`).
    struct NoCursorQuery(TestBackend);

    impl Backend for NoCursorQuery {
        type Error = io::Error;
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.0.draw(content).map_err(io::Error::other)
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            self.0.hide_cursor().map_err(io::Error::other)
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.0.show_cursor().map_err(io::Error::other)
        }
        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Err(io::Error::other("cursor position could not be read"))
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
            self.0.set_cursor_position(p).map_err(io::Error::other)
        }
        fn clear(&mut self) -> io::Result<()> {
            self.0.clear().map_err(io::Error::other)
        }
        fn clear_region(&mut self, t: ClearType) -> io::Result<()> {
            self.0.clear_region(t).map_err(io::Error::other)
        }
        fn size(&self) -> io::Result<Size> {
            self.0.size().map_err(io::Error::other)
        }
        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.0.window_size().map_err(io::Error::other)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush().map_err(io::Error::other)
        }
    }

    #[test]
    fn reenter_never_queries_the_cursor_position() {
        let mut terminal = Terminal::new(NoCursorQuery(TestBackend::new(20, 4))).unwrap();
        reenter(&mut terminal, || {
            Terminal::new(NoCursorQuery(TestBackend::new(20, 4)))
        })
        .unwrap();
        terminal.draw(|f| f.render_widget("ok", f.area())).unwrap();
    }

    #[test]
    fn restore_is_idempotent_without_init() {
        assert!(restore().is_ok());
        assert!(restore().is_ok());
    }
}
