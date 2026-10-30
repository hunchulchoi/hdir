//! Running external programs for extension associations: an injectable
//! launcher so tests never spawn real programs.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Prompt shown after a `wait` association finished.
pub const PRESS_ANY_KEY: &str = "계속하려면 아무 키나 누르세요";

/// Starts external programs. `Err` carries the user-facing reason.
pub trait Launcher: Send + Sync {
    /// Run `argv` (program first) in `cwd` with the terminal attached and wait
    /// for it. The caller has already left the TUI.
    fn run(&self, argv: &[OsString], cwd: &Path) -> Result<(), String>;
    /// Start `argv` in `cwd` with null stdio and do not wait for it.
    fn spawn_detached(&self, argv: &[OsString], cwd: &Path) -> Result<(), String>;
    /// Print [`PRESS_ANY_KEY`] and block until a key is pressed.
    fn wait_key(&self);
}

pub type SharedLauncher = Arc<dyn Launcher>;

/// The real launcher.
pub struct SystemLauncher;

pub fn system_launcher() -> SharedLauncher {
    Arc::new(SystemLauncher)
}

fn command(argv: &[OsString], cwd: &Path) -> Result<Command, String> {
    let (prog, args) = argv.split_first().ok_or("빈 명령")?;
    let mut c = Command::new(prog);
    c.args(args).current_dir(cwd);
    Ok(c)
}

impl Launcher for SystemLauncher {
    fn run(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
        let status = command(argv, cwd)?.status().map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(match status.code() {
                Some(c) => format!("종료 코드 {c}"),
                None => "신호로 종료됨".to_string(),
            })
        }
    }

    fn spawn_detached(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
        let mut child = command(argv, cwd)?
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        // Reap it so a finished program does not linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn wait_key(&self) {
        use ratatui::crossterm::event::{self, Event, KeyEventKind};
        use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        print!("\r\n{PRESS_ANY_KEY}");
        let _ = std::io::stdout().flush();
        if enable_raw_mode().is_err() {
            return;
        }
        while let Ok(ev) = event::read() {
            if matches!(&ev, Event::Key(k) if k.kind != KeyEventKind::Release) {
                break;
            }
        }
        let _ = disable_raw_mode();
        println!();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<OsString> {
        ["/bin/sh", "-c", script].map(OsString::from).to_vec()
    }

    #[test]
    fn run_executes_in_cwd_and_reports_exit_code() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            SystemLauncher.run(&sh("echo ok > marker"), t.path()),
            Ok(())
        );
        assert_eq!(
            std::fs::read_to_string(t.path().join("marker")).unwrap(),
            "ok\n"
        );
        assert_eq!(
            SystemLauncher.run(&sh("exit 3"), t.path()),
            Err("종료 코드 3".to_string())
        );
    }

    #[test]
    fn spawn_detached_runs_and_missing_program_errors() {
        let t = tempfile::tempdir().unwrap();
        SystemLauncher
            .spawn_detached(&sh("echo bg > marker"), t.path())
            .unwrap();
        let start = std::time::Instant::now();
        while !t.path().join("marker").exists() {
            assert!(start.elapsed().as_secs() < 5, "detached run never wrote");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let bad = [OsString::from("/nonexistent/prog-hdir")];
        assert!(SystemLauncher.spawn_detached(&bad, t.path()).is_err());
        assert!(SystemLauncher.run(&bad, t.path()).is_err());
        assert!(SystemLauncher.run(&[], t.path()).is_err());
    }
}
