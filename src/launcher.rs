//! Running external programs for extension associations: an injectable
//! launcher so tests never spawn real programs.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// Prompt shown after a `wait` association finished.
pub fn press_any_key() -> &'static str {
    crate::i18n::t("계속하려면 아무 키나 누르세요", "Press any key to continue")
}

/// `Err` text of a failed run. [`exit_code_of`] reads it back in either
/// language, so a message made before a language switch still matches.
fn exit_code_msg(code: i32) -> String {
    crate::tr!("종료 코드 {code}", "Exit code {code}")
}

fn exit_code_of(msg: &str) -> Option<i32> {
    ["종료 코드 ", "Exit code "]
        .iter()
        .find_map(|p| msg.strip_prefix(p))
        .and_then(|c| c.parse().ok())
}

/// Starts external programs. `Err` carries the user-facing reason.
pub trait Launcher: Send + Sync {
    /// Run `argv` (program first) in `cwd` with the terminal attached and wait
    /// for it. The caller has already left the TUI.
    fn run(&self, argv: &[OsString], cwd: &Path) -> Result<(), String>;
    /// Start `argv` in `cwd` with null stdio and do not wait for it.
    fn spawn_detached(&self, argv: &[OsString], cwd: &Path) -> Result<(), String>;
    /// Print [`press_any_key`] and block until a key is pressed.
    fn wait_key(&self);

    /// Like [`run`](Self::run) but reports the exit code instead of turning a
    /// non-zero code into an error. `Err` only when the program did not run.
    fn run_shell(&self, argv: &[OsString], cwd: &Path) -> Result<i32, String> {
        match self.run(argv, cwd) {
            Ok(()) => Ok(0),
            Err(e) => match exit_code_of(&e) {
                Some(code) => Ok(code),
                None => Err(e),
            },
        }
    }

    /// Show `[exit code N]` plus the key prompt and wait for a key.
    fn wait_key_after(&self, _code: i32) {
        self.wait_key();
    }
}

/// Text printed before waiting for a key after a command line command.
pub fn exit_prompt(code: i32) -> String {
    format!("\r\n[{}] {}", exit_code_msg(code), press_any_key())
}

pub type SharedLauncher = Arc<dyn Launcher>;

/// The real launcher.
pub struct SystemLauncher;

pub fn system_launcher() -> SharedLauncher {
    Arc::new(SystemLauncher)
}

fn command(argv: &[OsString], cwd: &Path) -> Result<Command, String> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| crate::i18n::t("빈 명령", "Empty command"))?;
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
                Some(c) => exit_code_msg(c),
                None => crate::i18n::t("신호로 종료됨", "Killed by a signal").to_string(),
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

    fn run_shell(&self, argv: &[OsString], cwd: &Path) -> Result<i32, String> {
        let status = command(argv, cwd)?.status().map_err(|e| e.to_string())?;
        Ok(status.code().unwrap_or(-1))
    }

    fn wait_key(&self) {
        wait_for_key(&format!("\r\n{}", press_any_key()));
    }

    fn wait_key_after(&self, code: i32) {
        wait_for_key(&exit_prompt(code));
    }
}

fn wait_for_key(prompt: &str) {
    use ratatui::crossterm::event::{self, Event, KeyEventKind};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    print!("{prompt}");
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
    fn run_shell_returns_the_exit_code() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(SystemLauncher.run_shell(&sh("exit 0"), t.path()), Ok(0));
        assert_eq!(SystemLauncher.run_shell(&sh("exit 7"), t.path()), Ok(7));
        let bad = [OsString::from("/nonexistent/prog-hdir")];
        assert!(SystemLauncher.run_shell(&bad, t.path()).is_err());
    }

    #[test]
    fn default_run_shell_maps_run_results() {
        struct R(Result<(), String>);
        impl Launcher for R {
            fn run(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                self.0.clone()
            }
            fn spawn_detached(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn wait_key(&self) {}
        }
        let p = Path::new("/");
        assert_eq!(R(Ok(())).run_shell(&[], p), Ok(0));
        assert_eq!(R(Err("종료 코드 3".into())).run_shell(&[], p), Ok(3));
        assert_eq!(
            R(Err("No such file".into())).run_shell(&[], p),
            Err("No such file".to_string())
        );
    }

    #[test]
    fn exit_prompt_text_names_the_code() {
        assert_eq!(
            exit_prompt(2),
            "\r\n[종료 코드 2] 계속하려면 아무 키나 누르세요"
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

    #[test]
    fn prompts_and_exit_codes_in_english() {
        crate::i18n::with_lang(crate::i18n::Lang::En, || {
            assert_eq!(
                exit_prompt(2),
                "\r\n[Exit code 2] Press any key to continue"
            );
            let t = tempfile::tempdir().unwrap();
            assert_eq!(
                SystemLauncher.run(&sh("exit 3"), t.path()),
                Err("Exit code 3".to_string())
            );
            assert_eq!(SystemLauncher.run_shell(&sh("exit 3"), t.path()), Ok(3));
        });
    }
}
