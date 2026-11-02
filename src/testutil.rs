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
    /// The argvs that came through `run_cancellable` (also in the others).
    pub cancellable: Mutex<Vec<Vec<String>>>,
    respond: Responder,
}

impl FakeCmd {
    pub fn new(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            transfers: Mutex::new(Vec::new()),
            cancellable: Mutex::new(Vec::new()),
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
    fn run_cancellable(
        &self,
        argv: &[OsString],
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> CmdOutput {
        self.cancellable.lock().unwrap().push(strs(argv));
        self.run_transfer(argv)
    }
    /// A pipe is answered as one argv: `["pipe", <from...>, "|", <to...>]`;
    /// the reply is the `to` side's (the `from` side succeeds), unless its
    /// stderr starts with `from:`: then it is the sending side's.
    fn run_pipe(
        &self,
        from: &[OsString],
        to: &[OsString],
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> (CmdOutput, CmdOutput) {
        let mut all = vec![OsString::from("pipe")];
        all.extend(from.iter().cloned());
        all.push(OsString::from("|"));
        all.extend(to.iter().cloned());
        let out = self.run_transfer(&all);
        // a reply whose stderr starts with "from:" is the sending side's
        if out.stderr.starts_with(b"from:") {
            return (out, fail(141, "broken pipe\n"));
        }
        (ok(""), out)
    }
}

impl FakeCmd {
    /// Every pipe run, as the combined argv `run_pipe` answers.
    pub fn pipe_calls(&self) -> Vec<Vec<String>> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().is_some_and(|p| p == "pipe"))
            .cloned()
            .collect()
    }
}

/// The two halves of a `pipe` argv from [`FakeCmd`]: what was read and
/// where it was unpacked, as `host:dir/name` / `host:stage` or local paths.
pub fn pipe_ends(a: &[String]) -> (String, String) {
    let bar = a.iter().position(|x| x == "|").expect("a pipe argv");
    let (from, to) = (&a[1..bar], &a[bar + 1..]);
    let quoted = |s: &str, after: &str| -> String {
        s.split_once(after)
            .map(|(_, r)| {
                // shell quoting as built by sh_quote: '...' with '\'' inside
                let mut out = String::new();
                let mut rest = r;
                while let Some(body) = rest.strip_prefix('\'') {
                    let end = body.find('\'').unwrap();
                    out.push_str(&body[..end]);
                    rest = &body[end + 1..];
                    match rest.strip_prefix("\\'") {
                        Some(r2) => {
                            out.push('\'');
                            rest = r2;
                        }
                        None => break,
                    }
                }
                out
            })
            .unwrap_or_default()
    };
    let side = |part: &[String], reading: bool| -> String {
        if part.first().is_some_and(|p| p == "ssh") {
            let host = &part[part.iter().position(|x| x == "--").unwrap() + 1];
            let cmd = remote_cmd(part).unwrap_or_default();
            let dir = quoted(cmd, "cd -- ");
            if reading {
                let name = quoted(cmd, "tar -cf - ./");
                format!("{host}:{dir}/{name}")
            } else {
                format!("{host}:{dir}")
            }
        } else if part.first().is_some_and(|p| p == "sh") && reading {
            // local_tar_out: sh -c SCRIPT sh DIR ID NAME
            format!("{}/{}", part[4], part[6])
        } else {
            let c = part.iter().position(|x| x == "-C").unwrap();
            if reading {
                let name = part[c + 2].strip_prefix("./").unwrap_or(&part[c + 2]);
                format!("{}/{name}", part[c + 1])
            } else {
                part[c + 1].clone()
            }
        }
    };
    (side(from, true), side(to, false))
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

/// Answer a `transfer::stage_cmd` (`cd -- '<dir>' ... mktemp -d "$t"`)
/// with `<dir>` as the physical dir and `<dir>/.hdir-xfer.ABC123`.
pub fn fake_mktemp(cmd: &str) -> Option<CmdOutput> {
    if !cmd.contains("mktemp -d \"$t\"") {
        return None;
    }
    let dir = cmd.strip_prefix("cd -- '")?.split_once('\'')?.0;
    let sep = if dir == "/" { "" } else { "/" };
    Some(ok(&format!("{dir}\0{dir}{sep}.hdir-xfer.ABC123\0")))
}

/// Answer a `transfer::quarantine_cmd` with a fixed `.hdir-del.Q1`.
pub fn fake_quarantine(cmd: &str) -> Option<CmdOutput> {
    cmd.contains("mktemp -d .hdir-del.")
        .then(|| ok(".hdir-del.Q1\0"))
}
