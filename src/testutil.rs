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
    respond: Responder,
}

impl FakeCmd {
    pub fn new(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
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
