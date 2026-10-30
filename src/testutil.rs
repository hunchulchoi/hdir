//! Test helpers shared by modules that run remote commands through a
//! [`RemoteSession`] with a fake [`Cmd`].

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::remote::{RemoteOs, ScpMode, SshTarget};
use crate::session::{Cmd, CmdOutput, RemoteSession};

type Responder = Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>;

/// Records every argv and answers through a closure.
pub struct FakeCmd {
    pub log: Mutex<Vec<Vec<String>>>,
    /// The argvs that came through `run_transfer` (also in `log`).
    pub transfers: Mutex<Vec<Vec<String>>>,
    respond: Responder,
}

impl FakeCmd {
    pub fn new(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            transfers: Mutex::new(Vec::new()),
            respond: Box::new(respond),
        })
    }

    /// The remote shell commands run through `ssh ... -- host CMD`, in order.
    pub fn remote_cmds(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|a| remote_cmd(a).map(str::to_string))
            .collect()
    }

    /// The remote shell commands run without the short timeout.
    pub fn long_cmds(&self) -> Vec<String> {
        self.transfers
            .lock()
            .unwrap()
            .iter()
            .filter_map(|a| remote_cmd(a).map(str::to_string))
            .collect()
    }

    /// Every argv whose program is `scp`.
    pub fn scp_calls(&self) -> Vec<Vec<String>> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().is_some_and(|p| p == "scp"))
            .cloned()
            .collect()
    }
}

/// `CMD` of an `ssh ... -- host CMD` argv built by `exec_argv`.
pub fn remote_cmd(a: &[String]) -> Option<&str> {
    if a.first().map(String::as_str) != Some("ssh") {
        return None;
    }
    let i = a.iter().position(|s| s == "--")?;
    a.get(i + 2).map(String::as_str)
}

fn strs(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

impl Cmd for FakeCmd {
    fn run(&self, argv: &[OsString]) -> CmdOutput {
        let a = strs(argv);
        self.log.lock().unwrap().push(a.clone());
        (self.respond)(&a)
    }
    fn run_interactive(&self, argv: &[OsString]) -> CmdOutput {
        self.run(argv)
    }
    fn run_transfer(&self, argv: &[OsString]) -> CmdOutput {
        self.transfers.lock().unwrap().push(strs(argv));
        self.run(argv)
    }
}

pub fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

pub fn code(c: i32) -> CmdOutput {
    CmdOutput {
        code: Some(c),
        ..CmdOutput::default()
    }
}

pub fn fail(c: i32, stderr: &str) -> CmdOutput {
    CmdOutput {
        code: Some(c),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

pub fn session(cmd: Arc<FakeCmd>) -> Arc<RemoteSession> {
    session_mode(cmd, ScpMode::Sftp)
}

pub fn session_mode(cmd: Arc<FakeCmd>, mode: ScpMode) -> Arc<RemoteSession> {
    Arc::new(RemoteSession::new(
        "prod".into(),
        SshTarget {
            host: "prod-web".into(),
            port: None,
            control_path: PathBuf::from("/tmp/hdir-1/cm-%C"),
        },
        RemoteOs::Linux,
        mode,
        cmd,
    ))
}

/// Runs `ssh ... -- host CMD` as `sh -c CMD` on this machine (with `HOME`
/// set) and `scp` as a file copy (uploads and downloads; source names must
/// not need SFTP glob escaping), so remote scripts can be exercised for real.
/// The session's `os` must match this machine (`stat` flags).
#[cfg(unix)]
pub struct LocalSh {
    pub home: PathBuf,
}

#[cfg(unix)]
impl Cmd for LocalSh {
    fn run(&self, argv: &[OsString]) -> CmdOutput {
        let a = strs(argv);
        let done = |o: std::process::Output| CmdOutput {
            code: o.status.code(),
            stdout: o.stdout,
            stderr: o.stderr,
        };
        if a[0] == "scp" {
            let (src, dst) = (&a[a.len() - 2], &a[a.len() - 1]);
            let src = src.strip_prefix("prod-web:").unwrap_or(src);
            let dst = dst.strip_prefix("prod-web:").unwrap_or(dst);
            // A download into a directory keeps the source's name.
            let dst = match std::path::Path::new(dst) {
                d if d.is_dir() => d.join(std::path::Path::new(src).file_name().unwrap()),
                d => d.to_path_buf(),
            };
            // Truncate in place like scp does (keeps the target's mode).
            return match std::fs::read(src).and_then(|b| std::fs::write(dst, b)) {
                Ok(()) => ok(""),
                Err(e) => fail(1, &format!("scp: {e}\n")),
            };
        }
        match remote_cmd(&a) {
            Some(c) => done(
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(c)
                    .env("HOME", &self.home)
                    .output()
                    .unwrap(),
            ),
            None => fail(1, "unsupported\n"),
        }
    }
    fn run_interactive(&self, argv: &[OsString]) -> CmdOutput {
        self.run(argv)
    }
}

/// The `RemoteOs` matching `stat` on this machine.
#[cfg(unix)]
pub fn local_os() -> RemoteOs {
    if cfg!(target_os = "linux") {
        RemoteOs::Linux
    } else {
        RemoteOs::Bsd
    }
}

#[cfg(unix)]
pub fn local_session(home: &std::path::Path) -> Arc<RemoteSession> {
    Arc::new(RemoteSession::new(
        "prod".into(),
        SshTarget {
            host: "prod-web".into(),
            port: None,
            control_path: PathBuf::from("/tmp/hdir-1/cm-%C"),
        },
        local_os(),
        ScpMode::Sftp,
        Arc::new(LocalSh {
            home: home.to_path_buf(),
        }),
    ))
}
