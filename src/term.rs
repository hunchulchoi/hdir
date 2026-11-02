//! Terminal setup/teardown helpers.
//!
//! The raw operations sit behind `TermOps` so the failure and recovery paths
//! can be tested with a recording fake.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Raw terminal steps. Each one is a single side effect.
trait TermOps {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
    /// Enter the alternate screen and hide the cursor.
    fn enter_screen(&mut self) -> io::Result<()>;
    /// Leave the alternate screen and show the cursor.
    fn leave_screen(&mut self) -> io::Result<()>;
    fn clear_all(&mut self) -> io::Result<()>;
}

/// The real terminal on stdout.
struct CrosstermOps;

impl TermOps for CrosstermOps {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }
    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
    fn enter_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen, Hide)
    }
    fn leave_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen, Show)
    }
    fn clear_all(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Clear(ClearType::All))
    }
}

fn restore_with(ops: &mut impl TermOps) -> io::Result<()> {
    let _ = ops.disable_raw();
    ops.leave_screen()
}

/// Enter raw mode and the alternate screen, then build the terminal. Anything
/// already entered is undone if a later step fails.
fn init_with<O: TermOps, T>(ops: &mut O, make: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    ops.enable_raw()?;
    if let Err(e) = ops.enter_screen() {
        let _ = restore_with(ops);
        return Err(e);
    }
    make().inspect_err(|_| {
        let _ = restore_with(ops);
    })
}

/// Enter raw mode and the alternate screen, hide the cursor.
pub fn init() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    init_with(&mut CrosstermOps, || {
        Terminal::new(CrosstermBackend::new(io::stdout()))
    })
}

/// Leave the alternate screen, show the cursor, disable raw mode.
///
/// Idempotent: safe to call repeatedly or without a prior `init`. Raw-mode
/// disable failures (e.g. no tty) are ignored as this is best-effort cleanup.
pub fn restore() -> io::Result<()> {
    restore_with(&mut CrosstermOps)
}

/// Run `restore` first, then `previous`.
fn run_hook(restore: impl FnOnce(), previous: impl FnOnce()) {
    restore();
    previous();
}

/// Install a panic hook that restores the terminal before the previous hook runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        run_hook(
            || {
                let _ = restore();
            },
            || previous(info),
        );
    }));
}

/// Re-enter raw mode and the alternate screen, then rebuild the terminal.
/// On failure the terminal is put back to cooked mode.
fn reenter_with<O: TermOps, T>(ops: &mut O, make: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let entered = ops
        .enable_raw()
        .and_then(|_| ops.enter_screen())
        .and_then(|_| ops.clear_all());
    if let Err(e) = entered {
        let _ = restore_with(ops);
        return Err(e);
    }
    make().inspect_err(|_| {
        let _ = restore_with(ops);
    })
}

/// Leave the TUI, run `f`, then re-enter. `f` always runs in cooked mode.
fn suspend_with<O: TermOps, T, R>(
    ops: &mut O,
    f: impl FnOnce() -> R,
    make: impl FnOnce() -> io::Result<T>,
) -> io::Result<(R, T)> {
    restore_with(ops)?;
    let result = f();
    let fresh = reenter_with(ops, make)?;
    Ok((result, fresh))
}

/// Temporarily leave the TUI, run `f` (e.g. `$EDITOR`), then re-enter and clear.
pub fn suspend<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    f: impl FnOnce() -> T,
) -> io::Result<T> {
    let (result, fresh) = suspend_with(&mut CrosstermOps, f, || {
        Terminal::new(CrosstermBackend::new(io::stdout()))
    })?;
    reenter(terminal, || Ok::<_, io::Error>(fresh))?;
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
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<&'static str>>>;

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

    /// Records every step; fails the step named in `fail_at`.
    struct Fake {
        log: Log,
        fail_at: Option<&'static str>,
    }

    impl Fake {
        fn new(fail_at: Option<&'static str>) -> (Self, Log) {
            let log = Log::default();
            (
                Fake {
                    log: log.clone(),
                    fail_at,
                },
                log,
            )
        }
        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.log.borrow_mut().push(name);
            if self.fail_at == Some(name) {
                Err(io::Error::other(name))
            } else {
                Ok(())
            }
        }
    }

    impl TermOps for Fake {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.step("enable_raw")
        }
        fn disable_raw(&mut self) -> io::Result<()> {
            self.step("disable_raw")
        }
        fn enter_screen(&mut self) -> io::Result<()> {
            self.step("enter_screen")
        }
        fn leave_screen(&mut self) -> io::Result<()> {
            self.step("leave_screen")
        }
        fn clear_all(&mut self) -> io::Result<()> {
            self.step("clear_all")
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

    #[test]
    fn init_success_enters_raw_then_screen_then_builds() {
        let (mut ops, log) = Fake::new(None);
        let log2 = log.clone();
        let v = init_with(&mut ops, || {
            log2.borrow_mut().push("make");
            Ok(1)
        })
        .unwrap();
        assert_eq!(v, 1);
        assert_eq!(*log.borrow(), ["enable_raw", "enter_screen", "make"]);
    }

    #[test]
    fn init_fails_on_raw_mode_enters_nothing() {
        let (mut ops, log) = Fake::new(Some("enable_raw"));
        assert!(init_with(&mut ops, || Ok(())).is_err());
        assert_eq!(*log.borrow(), ["enable_raw"]);
    }

    #[test]
    fn init_alt_screen_failure_undoes_raw_mode() {
        let (mut ops, log) = Fake::new(Some("enter_screen"));
        let err = init_with(&mut ops, || Ok(())).unwrap_err();
        assert_eq!(err.to_string(), "enter_screen");
        assert_eq!(
            *log.borrow(),
            ["enable_raw", "enter_screen", "disable_raw", "leave_screen"]
        );
    }

    #[test]
    fn init_terminal_creation_failure_undoes_everything() {
        let (mut ops, log) = Fake::new(None);
        let err = init_with::<_, ()>(&mut ops, || Err(io::Error::other("make"))).unwrap_err();
        assert_eq!(err.to_string(), "make");
        assert_eq!(
            *log.borrow(),
            ["enable_raw", "enter_screen", "disable_raw", "leave_screen"]
        );
    }

    #[test]
    fn restore_ignores_raw_failure_and_still_leaves_screen() {
        let (mut ops, log) = Fake::new(Some("disable_raw"));
        assert!(restore_with(&mut ops).is_ok());
        assert_eq!(*log.borrow(), ["disable_raw", "leave_screen"]);
    }

    #[test]
    fn panic_hook_restores_before_previous_hook() {
        let log = Log::default();
        let (l1, l2) = (log.clone(), log.clone());
        run_hook(
            move || l1.borrow_mut().push("restore"),
            move || l2.borrow_mut().push("previous"),
        );
        assert_eq!(*log.borrow(), ["restore", "previous"]);
    }

    #[test]
    fn suspend_runs_closure_in_cooked_mode_then_reenters() {
        let (mut ops, log) = Fake::new(None);
        let (l1, l2) = (log.clone(), log.clone());
        let (r, _) = suspend_with(
            &mut ops,
            move || {
                l1.borrow_mut().push("child");
                7
            },
            move || {
                l2.borrow_mut().push("make");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(r, 7);
        assert_eq!(
            *log.borrow(),
            [
                "disable_raw",
                "leave_screen",
                "child",
                "enable_raw",
                "enter_screen",
                "clear_all",
                "make"
            ]
        );
    }

    #[test]
    fn suspend_reenters_when_closure_returns_error() {
        let (mut ops, log) = Fake::new(None);
        let (res, _) = suspend_with(
            &mut ops,
            || Err::<(), _>(io::Error::other("child failed")),
            || Ok(()),
        )
        .unwrap();
        assert!(res.is_err());
        assert!(log.borrow().contains(&"enter_screen"));
        assert_eq!(log.borrow().last(), Some(&"clear_all"));
    }

    #[test]
    fn suspend_reenter_failure_leaves_cooked_mode() {
        let (mut ops, log) = Fake::new(None);
        ops.fail_at = Some("clear_all");
        assert!(suspend_with(&mut ops, || (), || Ok(())).is_err());
        assert_eq!(
            *log.borrow(),
            [
                "disable_raw",
                "leave_screen",
                "enable_raw",
                "enter_screen",
                "clear_all",
                "disable_raw",
                "leave_screen"
            ]
        );
    }

    #[test]
    fn suspend_terminal_rebuild_failure_leaves_cooked_mode() {
        let (mut ops, log) = Fake::new(None);
        let r = suspend_with::<_, (), _>(&mut ops, || (), || Err(io::Error::other("make")));
        assert!(r.is_err());
        let l = log.borrow();
        assert_eq!(l[l.len() - 2..], ["disable_raw", "leave_screen"]);
    }

    #[test]
    fn suspend_first_restore_failure_skips_closure() {
        let (mut ops, log) = Fake::new(Some("leave_screen"));
        let ran = std::cell::Cell::new(false);
        assert!(suspend_with(&mut ops, || ran.set(true), || Ok(())).is_err());
        assert!(!ran.get());
        assert!(!log.borrow().contains(&"enable_raw"));
    }
}
