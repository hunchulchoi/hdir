//! Terminal setup/teardown helpers.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Enter raw mode and the alternate screen, hide the cursor.
pub fn init() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, Hide) {
        let _ = restore();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(stdout))
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
    execute!(io::stdout(), EnterAlternateScreen, Hide)?;
    terminal.clear()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_idempotent_without_init() {
        assert!(restore().is_ok());
        assert!(restore().is_ok());
    }
}
