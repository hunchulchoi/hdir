//! SSH sessions: ControlMaster connect / check / reconnect and remote listing.
//!
//! All process spawning goes through [`Cmd`] so the flows are testable with a
//! fake runner. Argv construction lives in [`crate::remote`].
//!
//! Decisions:
//! - A session is shared (`Arc`) by every panel showing that server. Its
//!   `alive` flag flips to false the first time any command exits 255 (ssh's
//!   own failure code); Enter on a dead panel reconnects and flips it back.
//! - Interactive auth (password, host key) runs `ssh -fN` with the real
//!   terminal; everything else is `BatchMode=yes`.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::loader::Listing;
use crate::panel::finish_listing;
use crate::platform::{os_from_bytes, remote_bytes};
use crate::remote::{
    RemoteEntry, RemoteOs, ScpMode, SshTarget, check_argv, exec_argv, list_cmd, master_argv,
    parse_listing, parse_uname, scp_mode_from_version, sh_quote,
};
use crate::vfs::Vfs;

/// Status text of a panel whose connection died.
pub fn disconnected_msg() -> &'static str {
    crate::i18n::t(
        "연결 끊김 · Enter로 재연결",
        "Disconnected · Enter to reconnect",
    )
}

/// Result of one finished process. `code` is `None` when it could not be
/// spawned or was killed by a signal (the reason is in `stderr`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs external commands; replaceable in tests.
pub trait Cmd: Send + Sync {
    /// Run to completion with captured output and no stdin.
    fn run(&self, argv: &[OsString]) -> CmdOutput;
    /// Run attached to the real terminal (prompts); output is not captured.
    fn run_interactive(&self, argv: &[OsString]) -> CmdOutput;
    /// Like [`run`](Self::run) for file transfers, which may legitimately take
    /// longer than [`CMD_TIMEOUT`] (a dead link is still caught by ssh's
    /// ServerAlive options).
    fn run_transfer(&self, argv: &[OsString]) -> CmdOutput {
        self.run(argv)
    }
    /// Like [`run_transfer`](Self::run_transfer), but stopped (with
    /// [`cancelled_msg`]) as soon as `cancel` is set.
    fn run_cancellable(&self, argv: &[OsString], _cancel: &AtomicBool) -> CmdOutput {
        self.run_transfer(argv)
    }
    /// `from | to`, stopped when `cancel` is set (see [`run_pipe_cancellable`]).
    fn run_pipe(
        &self,
        _from: &[OsString],
        _to: &[OsString],
        _cancel: &AtomicBool,
    ) -> (CmdOutput, CmdOutput) {
        let no = spawn_failed("pipe not supported");
        (no.clone(), no)
    }
}

/// Stderr text of a command stopped through its cancel flag.
pub fn cancelled_msg() -> &'static str {
    crate::i18n::t("취소됨", "Cancelled")
}

/// Stderr text of a command that was killed for running too long.
pub fn timeout_msg() -> &'static str {
    crate::i18n::t("시간 초과", "Timed out")
}

/// Hard limit for any non-interactive command run through [`SystemCmd`].
pub const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for a single scp.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// The real process runner.
pub struct SystemCmd;

/// Run `argv` to completion with captured output, killing it after `limit`
/// (then `code` is `None` and stderr is [`timeout_msg`]).
pub fn run_limited(argv: &[OsString], limit: Duration) -> CmdOutput {
    run_with(argv, limit, None)
}

/// Like [`run_limited`], also stopped once `cancel` is set (then `code` is
/// `None` and stderr is [`cancelled_msg`]). On unix the command gets its own
/// process group and the whole group is killed, so helpers it spawned (scp's
/// ssh) cannot keep the output pipes open.
pub fn run_cancellable(argv: &[OsString], limit: Duration, cancel: &AtomicBool) -> CmdOutput {
    run_with(argv, limit, Some(cancel))
}

fn run_with(argv: &[OsString], limit: Duration, cancel: Option<&AtomicBool>) -> CmdOutput {
    if cfg!(windows) {
        return run_through_files(argv, None, limit, cancel, false).0;
    }
    let Some((prog, args)) = argv.split_first() else {
        return spawn_failed("empty command");
    };
    let mut command = Command::new(prog);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    if cancel.is_some() {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return spawn_failed(&format!("{}: {e}", prog.to_string_lossy())),
    };
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + limit;
    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline || cancel.is_some_and(is_set) => {
                cancelled = Instant::now() < deadline;
                kill_tree(&mut child, cancel.is_some());
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        Some(st) => CmdOutput {
            code: st.code(),
            stdout,
            stderr,
        },
        None if cancelled => spawn_failed(cancelled_msg()),
        None => spawn_failed(timeout_msg()),
    }
}

/// Run `from | to` (from's stdout into to's stdin), both killed together
/// after `limit` or once `cancel` is set (each output then has `code: None`
/// and [`timeout_msg`] / [`cancelled_msg`]). On unix both share one new
/// process group, so helpers they spawned (ssh) die with them.
pub fn run_pipe_cancellable(
    from: &[OsString],
    to: &[OsString],
    limit: Duration,
    cancel: &AtomicBool,
) -> (CmdOutput, CmdOutput) {
    if cfg!(windows) {
        return run_pipe_through_file(from, to, limit, cancel);
    }
    let (Some((pa, aa)), Some((pb, ab))) = (from.split_first(), to.split_first()) else {
        return (spawn_failed("empty command"), spawn_failed("empty command"));
    };
    let mut ca = Command::new(pa);
    ca.args(aa)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        ca.process_group(0);
    }
    let mut a = match ca.spawn() {
        Ok(c) => c,
        Err(e) => {
            let f = spawn_failed(&format!("{}: {e}", pa.to_string_lossy()));
            return (f, CmdOutput::default());
        }
    };
    let mut cb = Command::new(pb);
    cb.args(ab).stdout(Stdio::piped()).stderr(Stdio::piped());
    match a.stdout.take() {
        Some(o) => cb.stdin(Stdio::from(o)),
        None => cb.stdin(Stdio::null()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if let Ok(pg) = i32::try_from(a.id()) {
            cb.process_group(pg);
        }
    }
    let mut b = match cb.spawn() {
        Ok(c) => c,
        Err(e) => {
            kill_tree(&mut a, true);
            let _ = a.wait();
            let f = spawn_failed(&format!("{}: {e}", pb.to_string_lossy()));
            return (CmdOutput::default(), f);
        }
    };
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let (ea, ob, eb) = (
        drain(a.stderr.take()),
        drain(b.stdout.take()),
        drain(b.stderr.take()),
    );
    let deadline = Instant::now() + limit;
    let (mut sa, mut sb) = (None, None);
    let stopped = loop {
        if sa.is_none() {
            sa = a.try_wait().ok().flatten();
        }
        if sb.is_none() {
            sb = b.try_wait().ok().flatten();
        }
        if sa.is_some() && sb.is_some() {
            break None;
        }
        if Instant::now() >= deadline || is_set(cancel) {
            // `a` leads the group `b` joined: one kill stops both.
            kill_tree(&mut a, true);
            let _ = b.kill();
            let _ = a.wait();
            let _ = b.wait();
            break Some(if is_set(cancel) {
                cancelled_msg()
            } else {
                timeout_msg()
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (err_a, out_b, err_b) = (
        ea.join().unwrap_or_default(),
        ob.join().unwrap_or_default(),
        eb.join().unwrap_or_default(),
    );
    if let Some(msg) = stopped {
        return (spawn_failed(msg), spawn_failed(msg));
    }
    let code = |s: Option<std::process::ExitStatus>| s.and_then(|s| s.code());
    (
        CmdOutput {
            code: code(sa),
            stdout: Vec::new(),
            stderr: err_a,
        },
        CmdOutput {
            code: code(sb),
            stdout: out_b,
            stderr: err_b,
        },
    )
}

// ---- running through files (Windows OpenSSH) ----
//
// Win32-OpenSSH's ssh.exe does not exit when its stdout is a pipe, nor when
// it is given data on stdin; with stdin empty and the output going to files
// it behaves. On Windows every command therefore runs this way, and a
// "pipe" becomes two runs through a temporary file. The code is portable
// so it is tested everywhere.

/// Run `argv` with `stdin` (empty when `None`), stdout and stderr captured
/// in temporary files; stopped like [`run_cancellable`]. Returns the output
/// (with `stdout` read back) and, when `keep_out`, the stdout file itself
/// rewound instead (then `stdout` stays empty).
pub fn run_through_files(
    argv: &[OsString],
    stdin: Option<std::fs::File>,
    limit: Duration,
    cancel: Option<&AtomicBool>,
    keep_out: bool,
) -> (CmdOutput, Option<std::fs::File>) {
    use std::io::Seek;
    let Some((prog, args)) = argv.split_first() else {
        return (spawn_failed("empty command"), None);
    };
    let files = tempfile::tempfile().and_then(|o| Ok((o, tempfile::tempfile()?)));
    let (mut out, mut err) = match files {
        Ok(f) => f,
        Err(e) => {
            return (
                spawn_failed(&crate::tr!("임시 파일: {e}", "temp file: {e}")),
                None,
            );
        }
    };
    let (o2, e2) = match (out.try_clone(), err.try_clone()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            return (
                spawn_failed(&crate::tr!("임시 파일: {e}", "temp file: {e}")),
                None,
            );
        }
    };
    let mut command = Command::new(prog);
    command.args(args).stdout(o2).stderr(e2);
    match stdin {
        Some(f) => command.stdin(Stdio::from(f)),
        None => command.stdin(Stdio::null()),
    };
    #[cfg(unix)]
    if cancel.is_some() {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                spawn_failed(&format!("{}: {e}", prog.to_string_lossy())),
                None,
            );
        }
    };
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if Instant::now() >= deadline || cancel.is_some_and(is_set) => {
                let cancelled = Instant::now() < deadline;
                kill_tree(&mut child, cancel.is_some());
                let _ = child.wait();
                break Err(if cancelled {
                    cancelled_msg()
                } else {
                    timeout_msg()
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break Err(crate::i18n::t("실행 실패", "Failed to run")),
        }
    };
    let read_all = |f: &mut std::fs::File| -> Vec<u8> {
        let mut v = Vec::new();
        if f.rewind().is_ok() {
            let _ = f.read_to_end(&mut v);
        }
        v
    };
    let stderr = read_all(&mut err);
    match status {
        Err(msg) => (spawn_failed(msg), None),
        Ok(st) => {
            if keep_out {
                let kept = out.rewind().ok().map(|()| out);
                (
                    CmdOutput {
                        code: st.code(),
                        stdout: Vec::new(),
                        stderr,
                    },
                    kept,
                )
            } else {
                let stdout = read_all(&mut out);
                (
                    CmdOutput {
                        code: st.code(),
                        stdout,
                        stderr,
                    },
                    None,
                )
            }
        }
    }
}

/// `from | to` as two runs through a temporary file ([`run_through_files`]):
/// `to` starts only once `from` finished well.
pub fn run_pipe_through_file(
    from: &[OsString],
    to: &[OsString],
    limit: Duration,
    cancel: &AtomicBool,
) -> (CmdOutput, CmdOutput) {
    let (a, file) = run_through_files(from, None, limit, Some(cancel), true);
    let Some(file) = file.filter(|_| a.code == Some(0)) else {
        return (a, CmdOutput::default());
    };
    let (b, _) = run_through_files(to, Some(file), limit, Some(cancel), false);
    (a, b)
}

fn is_set(flag: &AtomicBool) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Kill `child`, and its whole process group when it leads one.
fn kill_tree(child: &mut std::process::Child, group: bool) {
    #[cfg(unix)]
    if group && let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; the group id is our own child's pid.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = group;
    let _ = child.kill();
}

impl Cmd for SystemCmd {
    fn run(&self, argv: &[OsString]) -> CmdOutput {
        run_limited(argv, CMD_TIMEOUT)
    }
    fn run_transfer(&self, argv: &[OsString]) -> CmdOutput {
        run_limited(argv, TRANSFER_TIMEOUT)
    }
    fn run_cancellable(&self, argv: &[OsString], cancel: &AtomicBool) -> CmdOutput {
        run_cancellable(argv, TRANSFER_TIMEOUT, cancel)
    }
    fn run_pipe(
        &self,
        from: &[OsString],
        to: &[OsString],
        cancel: &AtomicBool,
    ) -> (CmdOutput, CmdOutput) {
        run_pipe_cancellable(from, to, TRANSFER_TIMEOUT, cancel)
    }
    fn run_interactive(&self, argv: &[OsString]) -> CmdOutput {
        let Some((prog, args)) = argv.split_first() else {
            return spawn_failed("empty command");
        };
        match Command::new(prog).args(args).status() {
            Ok(st) => CmdOutput {
                code: st.code(),
                ..CmdOutput::default()
            },
            Err(e) => spawn_failed(&format!("{}: {e}", prog.to_string_lossy())),
        }
    }
}

fn spawn_failed(msg: &str) -> CmdOutput {
    CmdOutput {
        code: None,
        stdout: Vec::new(),
        stderr: msg.as_bytes().to_vec(),
    }
}

/// First non-empty stderr line, or `fallback`.
pub(crate) fn first_line(stderr: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| fallback.to_string(), str::to_string)
}

/// Why a remote command did not run to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Disconnected,
    Spawn(String),
}

impl SessionError {
    pub fn message(&self) -> String {
        match self {
            SessionError::Disconnected => disconnected_msg().to_string(),
            SessionError::Spawn(m) => m.clone(),
        }
    }
}

/// One server connection (shared by panels).
pub struct RemoteSession {
    pub name: String,
    pub target: SshTarget,
    pub os: RemoteOs,
    pub scp_mode: ScpMode,
    cmd: Arc<dyn Cmd>,
    alive: AtomicBool,
}

impl std::fmt::Debug for RemoteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSession")
            .field("name", &self.name)
            .field("host", &self.target.host)
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// Sessions are identities, not values: two are equal only when they are the
/// same connection.
impl PartialEq for RemoteSession {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for RemoteSession {}

impl RemoteSession {
    pub fn new(
        name: String,
        target: SshTarget,
        os: RemoteOs,
        scp_mode: ScpMode,
        cmd: Arc<dyn Cmd>,
    ) -> Self {
        Self {
            name,
            target,
            os,
            scp_mode,
            cmd,
            alive: AtomicBool::new(true),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn set_alive(&self, alive: bool) {
        self.alive.store(alive, Ordering::SeqCst);
    }

    /// Run one remote command. Exit 255 (ssh failure) marks the session dead.
    pub fn exec(&self, remote_cmd: &[u8]) -> Result<CmdOutput, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        let out = self.cmd.run(&exec_argv(&self.target, remote_cmd));
        self.exec_result(out)
    }

    /// Like [`exec`](Self::exec) for a command that reads or writes a whole
    /// file (checksum, in-place copy): limited like a transfer, not by
    /// [`CMD_TIMEOUT`].
    pub fn exec_long(&self, remote_cmd: &[u8]) -> Result<CmdOutput, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        let out = self.cmd.run_transfer(&exec_argv(&self.target, remote_cmd));
        self.exec_result(out)
    }

    fn exec_result(&self, out: CmdOutput) -> Result<CmdOutput, SessionError> {
        match out.code {
            Some(255) => {
                self.set_alive(false);
                Err(SessionError::Disconnected)
            }
            None => Err(SessionError::Spawn(first_line(
                &out.stderr,
                &crate::tr!("ssh 실행 실패", "Failed to run ssh"),
            ))),
            Some(_) => Ok(out),
        }
    }

    /// Run an `scp` argv (see [`crate::remote::scp_argv`]) through the same
    /// liveness rules as [`exec`](Self::exec).
    pub fn scp(&self, argv: &[OsString]) -> Result<CmdOutput, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        let out = self.cmd.run_transfer(argv);
        match out.code {
            Some(255) => {
                self.set_alive(false);
                Err(SessionError::Disconnected)
            }
            None => Err(SessionError::Spawn(first_line(
                &out.stderr,
                &crate::tr!("scp 실행 실패", "Failed to run scp"),
            ))),
            Some(_) => Ok(out),
        }
    }

    /// Like [`exec_long`](Self::exec_long), stopped when `cancel` is set.
    pub fn exec_cancellable(
        &self,
        remote_cmd: &[u8],
        cancel: &AtomicBool,
    ) -> Result<CmdOutput, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        let out = self
            .cmd
            .run_cancellable(&exec_argv(&self.target, remote_cmd), cancel);
        self.exec_result(out)
    }

    /// Like [`scp`](Self::scp), stopped when `cancel` is set.
    pub fn scp_cancellable(
        &self,
        argv: &[OsString],
        cancel: &AtomicBool,
    ) -> Result<CmdOutput, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        let out = self.cmd.run_cancellable(argv, cancel);
        match out.code {
            Some(255) => {
                self.set_alive(false);
                Err(SessionError::Disconnected)
            }
            None => Err(SessionError::Spawn(first_line(
                &out.stderr,
                &crate::tr!("scp 실행 실패", "Failed to run scp"),
            ))),
            Some(_) => Ok(out),
        }
    }

    /// Run `from | to` through this session's runner (one side is usually an
    /// ssh to this host). Liveness is the caller's to judge: it knows which
    /// side was an ssh to which host.
    pub fn pipe(
        &self,
        from: &[OsString],
        to: &[OsString],
        cancel: &AtomicBool,
    ) -> Result<(CmdOutput, CmdOutput), SessionError> {
        if !self.is_alive() {
            return Err(SessionError::Disconnected);
        }
        Ok(self.cmd.run_pipe(from, to, cancel))
    }

    /// Run a *local* command (`tar` for a transfer) through this session's
    /// runner, stopped when `cancel` is set.
    pub fn run_local(&self, argv: &[OsString], cancel: &AtomicBool) -> CmdOutput {
        self.cmd.run_cancellable(argv, cancel)
    }

    /// Reconnect check: is the master alive again? Flips `alive` accordingly.
    pub fn revive(&self) -> bool {
        let up = master_alive(&*self.cmd, &self.target);
        self.set_alive(up);
        up
    }

    /// List `dir` on the remote host (blocking; run on a worker).
    pub fn list(&self, dir: &Path, show_hidden: bool) -> Listing {
        let bytes = remote_bytes(dir.as_os_str());
        let out = self
            .exec(&list_cmd(&self.os, &bytes, show_hidden))
            .map_err(|e| e.message())?;
        let failed = out.code != Some(0);
        if failed && out.stdout.is_empty() {
            return Err(first_line(
                &out.stderr,
                &crate::tr!("종료 코드 {}", "exit code {}", out.code.unwrap_or(-1)),
            ));
        }
        let (mut entries, bad) = parse_listing(&self.os, &out.stdout);
        // On Windows a remote name that would not stay one plain path
        // component once joined (a backslash, a `C:` prefix) or was not
        // UTF-8 is left out (and counted).
        let before = entries.len();
        if cfg!(windows) {
            entries.retain(|e| crate::platform::usable_remote_name(&e.name));
        }
        let hidden = before - entries.len();
        let items = entries.iter().map(RemoteEntry::to_entry).collect();
        // A non-zero exit with output means find/stat skipped something.
        let skipped = bad + hidden + usize::from(failed);
        Ok((finish_listing(dir, items), skipped))
    }
}

/// Lists through a [`RemoteSession`].
pub struct SshVfs {
    pub session: Arc<RemoteSession>,
}

impl Vfs for SshVfs {
    fn list(&self, dir: &Path, show_hidden: bool) -> Listing {
        self.session.list(dir, show_hidden)
    }
    fn source_key(&self) -> usize {
        Arc::as_ptr(&self.session) as usize
    }
    fn display_path(&self, dir: &Path) -> String {
        format!("{}:{}", self.session.name, dir.to_string_lossy())
    }
}

/// `ControlPath` for sockets: `<cache>/hdir/cm-%C`, falling back to
/// `/tmp/hdir-<uid>/cm-%C` when the socket path could exceed the ~104 byte
/// unix socket limit (`%C` expands to 40 chars; ssh adds a 17 char suffix
/// while binding).
pub fn control_path(cache_dir: Option<&Path>, uid: u32) -> PathBuf {
    // "/cm-" + 40 (%C) + 17 (ssh's temporary suffix)
    const EXTRA: usize = 4 + 40 + 17;
    const LIMIT: usize = 100;
    if let Some(c) = cache_dir {
        let dir = c.join("hdir");
        if dir.as_os_str().len() + EXTRA <= LIMIT {
            return dir.join("cm-%C");
        }
    }
    PathBuf::from(format!("/tmp/hdir-{uid}")).join("cm-%C")
}

/// Create the control socket directory with mode 0700. An existing directory
/// is only accepted when it is a real directory (never a symlink) owned by the
/// current user without group/other access; it is never chmod-ed.
pub fn ensure_control_dir(path: &Path) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            Err(e) => return Err(e),
        }
        // Checked after creation too, so a racing symlink is caught.
        check_private_dir(&std::fs::symlink_metadata(dir)?, dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(unix)]
fn check_private_dir(m: &std::fs::Metadata, dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let deny = |why: &str| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{}: {why}", dir.display()),
        ))
    };
    if !m.is_dir() {
        return deny(crate::i18n::t(
            "실제 디렉터리가 아님",
            "not a real directory",
        ));
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    if m.uid() != unsafe { libc::getuid() } {
        return deny(crate::i18n::t("소유자가 다름", "owned by someone else"));
    }
    if m.permissions().mode() & 0o077 != 0 {
        return deny(crate::i18n::t(
            "권한이 너무 넓음 (0700 필요)",
            "permissions too open (0700 required)",
        ));
    }
    Ok(())
}

/// Is the ControlMaster for `t` alive (`ssh -O check` exits 0)?
pub fn master_alive(cmd: &dyn Cmd, t: &SshTarget) -> bool {
    cmd.run(&check_argv(t)).code == Some(0)
}

/// Start the master on the real terminal (auth prompts). Ok when ssh exits 0.
pub fn start_master(cmd: &dyn Cmd, t: &SshTarget) -> Result<(), String> {
    let out = cmd.run_interactive(&master_argv(t));
    match out.code {
        Some(0) => Ok(()),
        Some(c) => Err(crate::tr!(
            "ssh 연결 실패 (종료 코드 {c})",
            "ssh connection failed (exit code {c})"
        )),
        None => Err(first_line(
            &out.stderr,
            &crate::tr!("ssh 실행 실패", "Failed to run ssh"),
        )),
    }
}

/// Remote shell command that prints the absolute form (NUL-terminated) of `path` (`~` and
/// `~/x` mean the remote home; empty/`.` the login directory).
pub fn start_path_cmd(path: &str) -> Vec<u8> {
    let target: Vec<u8> = match path {
        "" | "." => b".".to_vec(),
        "~" => b"\"$HOME\"".to_vec(),
        p => match p.strip_prefix("~/") {
            Some(rest) => [
                b"\"$HOME\"".as_slice(),
                &sh_quote(format!("/{rest}").as_bytes()),
            ]
            .concat(),
            None => sh_quote(p.as_bytes()),
        },
    };
    // NUL-terminated so newlines and non-UTF-8 bytes in the path survive.
    [
        b"cd -- ".as_slice(),
        &target,
        b" && printf '%s\\0' \"$PWD\"",
    ]
    .concat()
}

/// Remote command that changes to `cwd`, then to `target` (see
/// [`start_path_cmd`] for the forms: `~`, `~/x`, plain paths) and prints the
/// resulting absolute directory, NUL-terminated.
pub fn cd_cmd(cwd: &[u8], target: &str) -> Vec<u8> {
    [
        b"cd -- ".as_slice(),
        &sh_quote(cwd),
        b" && ",
        &start_path_cmd(target),
    ]
    .concat()
}

/// A freshly established session plus the directory to show first.
#[derive(Debug)]
pub struct Established {
    pub session: Arc<RemoteSession>,
    pub start: PathBuf,
}

/// With the master up: detect the remote OS and local scp mode, resolve the
/// start directory. Blocking; run on a worker.
pub fn establish(
    cmd: &Arc<dyn Cmd>,
    name: &str,
    target: SshTarget,
    start_path: Option<&str>,
) -> Result<Established, String> {
    let uname = cmd.run(&exec_argv(&target, b"uname -s"));
    match uname.code {
        Some(0) => {}
        Some(255) => {
            return Err(first_line(
                &uname.stderr,
                &crate::tr!("ssh 연결 실패", "ssh connection failed"),
            ));
        }
        Some(c) => {
            return Err(first_line(
                &uname.stderr,
                &crate::tr!("uname 실패 (종료 코드 {c})", "uname failed (exit code {c})"),
            ));
        }
        None => {
            return Err(first_line(
                &uname.stderr,
                &crate::tr!("ssh 실행 실패", "Failed to run ssh"),
            ));
        }
    }
    let os = parse_uname(&String::from_utf8_lossy(&uname.stdout));
    // `ssh -V` prints to stderr; failing to run it is not fatal (assume sftp).
    let v = cmd.run(&[OsString::from("ssh"), OsString::from("-V")]);
    let version = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stderr),
        String::from_utf8_lossy(&v.stdout)
    );
    let session = Arc::new(RemoteSession::new(
        name.to_string(),
        target,
        os,
        scp_mode_from_version(&version),
        Arc::clone(cmd),
    ));
    let out = session
        .exec(&start_path_cmd(start_path.unwrap_or("")))
        .map_err(|e| e.message())?;
    if out.code != Some(0) {
        return Err(first_line(
            &out.stderr,
            &crate::tr!("시작 경로를 열 수 없음", "Cannot open the start path"),
        ));
    }
    let Some(end) = out.stdout.iter().position(|&b| b == 0) else {
        return Err(crate::tr!(
            "시작 경로를 알 수 없음",
            "Cannot determine the start path"
        ));
    };
    if end == 0 {
        return Err(crate::tr!(
            "시작 경로를 알 수 없음",
            "Cannot determine the start path"
        ));
    }
    let start = PathBuf::from(os_from_bytes(out.stdout[..end].to_vec()));
    Ok(Established { session, start })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{check_argv, exec_argv, master_argv};
    use std::sync::Mutex;

    type Responder = Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>;

    struct Fake {
        log: Mutex<Vec<Vec<String>>>,
        interactive: Mutex<Vec<Vec<String>>>,
        cancellable: Mutex<usize>,
        respond: Responder,
    }

    impl Fake {
        fn new(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                log: Mutex::new(Vec::new()),
                interactive: Mutex::new(Vec::new()),
                cancellable: Mutex::new(0),
                respond: Box::new(respond),
            })
        }
        fn calls(&self) -> usize {
            self.log.lock().unwrap().len()
        }
    }

    fn strs(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    impl Cmd for Fake {
        fn run(&self, argv: &[OsString]) -> CmdOutput {
            let a = strs(argv);
            self.log.lock().unwrap().push(a.clone());
            (self.respond)(&a)
        }
        fn run_interactive(&self, argv: &[OsString]) -> CmdOutput {
            let a = strs(argv);
            self.interactive.lock().unwrap().push(a.clone());
            (self.respond)(&a)
        }
        fn run_cancellable(&self, argv: &[OsString], _cancel: &AtomicBool) -> CmdOutput {
            *self.cancellable.lock().unwrap() += 1;
            self.run(argv)
        }
    }

    fn out(code: i32, stdout: &str) -> CmdOutput {
        CmdOutput {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn target() -> SshTarget {
        SshTarget {
            host: "choi@example.com".into(),
            port: Some(2222),
            control_path: PathBuf::from("/tmp/hdir-1/cm-%C"),
        }
    }

    fn remote_cmd_of(a: &[String]) -> Option<&str> {
        // exec_argv: ssh -o CP -o BatchMode=yes [-p N] -- host CMD
        let i = a.iter().position(|s| s == "--")?;
        a.get(i + 2).map(String::as_str)
    }

    fn session_with(fake: Arc<Fake>, os: RemoteOs) -> Arc<RemoteSession> {
        Arc::new(RemoteSession::new(
            "web".into(),
            target(),
            os,
            ScpMode::Sftp,
            fake,
        ))
    }

    // ---- control path ----

    #[cfg(unix)]
    #[test]
    fn control_path_uses_cache_dir_when_short() {
        let p = control_path(Some(Path::new("/home/u/.cache")), 501);
        assert_eq!(p, PathBuf::from("/home/u/.cache/hdir/cm-%C"));
    }

    #[cfg(unix)]
    #[test]
    fn control_path_falls_back_to_tmp_when_too_long() {
        let long = "/Users/someone-with-a-very-long-name/Library/Caches/and/more/nesting";
        let p = control_path(Some(Path::new(long)), 501);
        assert_eq!(p, PathBuf::from("/tmp/hdir-501/cm-%C"));
        assert_eq!(control_path(None, 7), PathBuf::from("/tmp/hdir-7/cm-%C"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_control_dir_creates_private_dir() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("a/b/cm-%C");
        ensure_control_dir(&sock).unwrap();
        let dir = sock.parent().unwrap();
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        ensure_control_dir(&sock).unwrap(); // idempotent
    }

    #[cfg(unix)]
    #[test]
    fn ensure_control_dir_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_control_dir(&link.join("cm-%C")).is_err());
        let mode = real.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "never touched through the link");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_control_dir_refuses_loose_permissions_without_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("loose");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_control_dir(&dir.join("cm-%C")).is_err());
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(ensure_control_dir(&dir.join("cm-%C")).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(
            ensure_control_dir(&dir.join("cm-%C")).is_ok(),
            "stricter is fine"
        );
    }

    // ---- timeouts ----

    #[cfg(unix)]
    #[test]
    fn run_limited_kills_a_hanging_child() {
        let start = std::time::Instant::now();
        let argv: Vec<OsString> = vec!["sleep".into(), "30".into()];
        let o = run_limited(&argv, std::time::Duration::from_millis(200));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(o.code, None);
        assert_eq!(first_line(&o.stderr, ""), timeout_msg());
    }

    #[cfg(unix)]
    #[test]
    fn run_cancellable_stops_the_child_and_its_children_when_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c2.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        // The grandchild keeps stderr open; it must die with the group.
        let argv: Vec<OsString> = vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()];
        let o = run_cancellable(&argv, Duration::from_secs(60), &cancel);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(o.code, None);
        assert_eq!(first_line(&o.stderr, ""), cancelled_msg());
    }

    #[cfg(windows)]
    #[test]
    fn running_through_files_works_on_windows() {
        let no = AtomicBool::new(false);
        let cmd = |c: &str| -> Vec<OsString> { vec!["cmd".into(), "/C".into(), c.into()] };
        let (o, _) = run_through_files(
            &cmd("echo hi& echo oops 1>&2& exit /b 3"),
            None,
            Duration::from_secs(10),
            None,
            false,
        );
        assert_eq!(o.code, Some(3));
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "hi");
        assert_eq!(first_line(&o.stderr, ""), "oops");
        let (a, b) = run_pipe_through_file(
            &cmd("echo abc"),
            &cmd("findstr b"),
            Duration::from_secs(10),
            &no,
        );
        assert_eq!((a.code, b.code), (Some(0), Some(0)));
        assert_eq!(String::from_utf8_lossy(&b.stdout).trim(), "abc");
        let (a, b) = run_pipe_through_file(
            &cmd("exit /b 2"),
            &cmd("echo never"),
            Duration::from_secs(10),
            &no,
        );
        assert_eq!(
            (a.code, b.code),
            (Some(2), None),
            "a failed writer never starts the reader"
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let (o, _) = run_through_files(
            &cmd("ping -n 30 127.0.0.1 >NUL"),
            None,
            Duration::from_secs(60),
            Some(&cancel),
            false,
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(first_line(&o.stderr, ""), cancelled_msg());
    }

    #[cfg(unix)]
    #[test]
    fn running_through_files_captures_output_and_pipes_in_two_steps() {
        let no = AtomicBool::new(false);
        let sh = |c: &str| -> Vec<OsString> { vec!["sh".into(), "-c".into(), c.into()] };
        let (o, _) = run_through_files(
            &sh("printf hi; echo oops >&2; exit 3"),
            None,
            Duration::from_secs(5),
            None,
            false,
        );
        assert_eq!((o.code, o.stdout.as_slice()), (Some(3), &b"hi"[..]));
        assert_eq!(first_line(&o.stderr, ""), "oops");
        let (a, b) = run_pipe_through_file(
            &sh("printf 'a b'"),
            &sh("tr a-z A-Z"),
            Duration::from_secs(5),
            &no,
        );
        assert_eq!(
            (a.code, b.code, b.stdout.as_slice()),
            (Some(0), Some(0), &b"A B"[..])
        );
        // a failed writer never starts the reader
        let (a, b) = run_pipe_through_file(
            &sh("exit 2"),
            &sh("touch /nonexistent/x"),
            Duration::from_secs(5),
            &no,
        );
        assert_eq!((a.code, b.code), (Some(2), None));
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let (o, _) = run_through_files(
            &sh("sleep 30"),
            None,
            Duration::from_secs(60),
            Some(&cancel),
            false,
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(first_line(&o.stderr, ""), cancelled_msg());
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_feeds_one_into_the_other_and_reports_both() {
        let no = AtomicBool::new(false);
        let a: Vec<OsString> = vec![
            "sh".into(),
            "-c".into(),
            "printf 'a b'; echo oops >&2; exit 3".into(),
        ];
        let b: Vec<OsString> = vec!["sh".into(), "-c".into(), "tr a-z A-Z; exit 4".into()];
        let (oa, ob) = run_pipe_cancellable(&a, &b, Duration::from_secs(5), &no);
        assert_eq!((oa.code, ob.code), (Some(3), Some(4)));
        assert_eq!(ob.stdout, b"A B");
        assert_eq!(first_line(&oa.stderr, ""), "oops");
        let bad: Vec<OsString> = vec!["/nonexistent/x".into()];
        let (oa, ob) = run_pipe_cancellable(&a, &bad, Duration::from_secs(5), &no);
        assert_eq!((oa.code, ob.code), (None, None), "{oa:?} {ob:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_pipe_stops_both_sides_quickly() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c2.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        let a: Vec<OsString> = vec![
            "sh".into(),
            "-c".into(),
            "sleep 30 & while :; do echo x; sleep 1; done".into(),
        ];
        let b: Vec<OsString> = vec!["sh".into(), "-c".into(), "cat >/dev/null; sleep 30".into()];
        let (oa, ob) = run_pipe_cancellable(&a, &b, Duration::from_secs(60), &cancel);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(first_line(&oa.stderr, ""), cancelled_msg());
        assert_eq!(first_line(&ob.stderr, ""), cancelled_msg());
    }

    #[cfg(unix)]
    #[test]
    fn run_cancellable_returns_normally_when_not_cancelled() {
        let cancel = AtomicBool::new(false);
        let argv: Vec<OsString> = vec!["sh".into(), "-c".into(), "printf hi; exit 4".into()];
        let o = run_cancellable(&argv, Duration::from_secs(5), &cancel);
        assert_eq!((o.code, o.stdout.as_slice()), (Some(4), &b"hi"[..]));
    }

    #[test]
    fn cancellable_session_calls_use_the_cancellable_runner() {
        let f = Fake::new(|_| CmdOutput {
            code: Some(0),
            ..CmdOutput::default()
        });
        let s = session_with(Arc::clone(&f), RemoteOs::Linux);
        let cancel = AtomicBool::new(false);
        s.exec_cancellable(b"cp a b", &cancel).unwrap();
        s.scp_cancellable(&["scp".into(), "x".into()], &cancel)
            .unwrap();
        assert_eq!(*f.cancellable.lock().unwrap(), 2);
    }

    #[test]
    fn a_cancellable_call_exiting_255_marks_the_session_dead() {
        let f = Fake::new(|_| CmdOutput {
            code: Some(255),
            ..CmdOutput::default()
        });
        let s = session_with(f, RemoteOs::Linux);
        let cancel = AtomicBool::new(false);
        let e = s.scp_cancellable(&["scp".into()], &cancel).unwrap_err();
        assert_eq!(e, SessionError::Disconnected);
        assert!(!s.is_alive());
        assert_eq!(
            s.exec_cancellable(b"x", &cancel).unwrap_err(),
            SessionError::Disconnected
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_limited_returns_output_of_a_quick_child() {
        let argv: Vec<OsString> = vec!["sh".into(), "-c".into(), "printf hi; exit 3".into()];
        let o = run_limited(&argv, std::time::Duration::from_secs(5));
        assert_eq!((o.code, o.stdout.as_slice()), (Some(3), &b"hi"[..]));
    }

    #[test]
    fn a_timed_out_listing_is_an_error_but_keeps_the_session_alive() {
        let f = Fake::new(|_| CmdOutput {
            code: None,
            stdout: Vec::new(),
            stderr: timeout_msg().as_bytes().to_vec(),
        });
        let s = session_with(f, RemoteOs::Linux);
        assert_eq!(s.list(Path::new("/x"), false).unwrap_err(), "시간 초과");
        assert!(s.is_alive());
    }

    // ---- master / establish ----

    #[test]
    fn master_alive_follows_check_exit_code() {
        let ok = Fake::new(|_| out(0, ""));
        assert!(master_alive(&*ok, &target()));
        assert_eq!(strs(&check_argv(&target())), ok.log.lock().unwrap()[0]);
        let dead = Fake::new(|_| out(255, ""));
        assert!(!master_alive(&*dead, &target()));
    }

    #[test]
    fn start_master_runs_master_argv_interactively() {
        let f = Fake::new(|_| out(0, ""));
        start_master(&*f, &target()).unwrap();
        assert_eq!(f.calls(), 0);
        assert_eq!(
            f.interactive.lock().unwrap()[0],
            strs(&master_argv(&target()))
        );
        let bad = Fake::new(|_| out(255, ""));
        assert!(start_master(&*bad, &target()).is_err());
    }

    #[test]
    fn start_path_cmd_quotes_and_expands_home() {
        let s = |p: &str| String::from_utf8(start_path_cmd(p)).unwrap();
        let tail = r#" && printf '%s\0' "$PWD""#;
        assert_eq!(s(""), format!("cd -- .{tail}"));
        assert_eq!(s("."), format!("cd -- .{tail}"));
        assert_eq!(
            s("/var/it's here"),
            format!("cd -- '/var/it'\\''s here'{tail}")
        );
        assert_eq!(s("~"), format!("cd -- \"$HOME\"{tail}"));
        assert_eq!(s("~/my dir"), format!("cd -- \"$HOME\"'/my dir'{tail}"));
    }

    #[test]
    fn cd_cmd_resolves_relative_to_the_cwd_and_prints_the_new_pwd() {
        let s = |cwd: &str, t: &str| String::from_utf8(cd_cmd(cwd.as_bytes(), t)).unwrap();
        let tail = r#" && printf '%s\0' "$PWD""#;
        assert_eq!(
            s("/srv/it's", "sub dir"),
            format!("cd -- '/srv/it'\\''s' && cd -- 'sub dir'{tail}")
        );
        assert_eq!(s("/x", "~"), format!("cd -- '/x' && cd -- \"$HOME\"{tail}"));
        assert_eq!(
            s("/x", "~/a b"),
            format!("cd -- '/x' && cd -- \"$HOME\"'/a b'{tail}")
        );
        assert_eq!(s("/x", "-v"), format!("cd -- '/x' && cd -- '-v'{tail}"));
    }

    fn establish_fake(ssh_v: &'static str, uname: &'static str, pwd: &'static [u8]) -> Arc<Fake> {
        Fake::new(move |a| {
            if a == ["ssh", "-V"] {
                return CmdOutput {
                    code: Some(0),
                    stdout: Vec::new(),
                    stderr: ssh_v.as_bytes().to_vec(),
                };
            }
            match remote_cmd_of(a) {
                Some("uname -s") => out(0, uname),
                Some(c) if c.contains("$PWD") => CmdOutput {
                    code: Some(0),
                    stdout: pwd.to_vec(),
                    stderr: Vec::new(),
                },
                _ => out(0, ""),
            }
        })
    }

    #[test]
    fn establish_detects_os_scp_mode_and_start_dir() {
        let f = establish_fake("OpenSSH_8.4p1, OpenSSL\n", "Linux\n", b"/home/choi\0");
        let cmd: Arc<dyn Cmd> = f.clone();
        let e = establish(&cmd, "web", target(), None).unwrap();
        assert_eq!(e.session.os, RemoteOs::Linux);
        assert_eq!(e.session.scp_mode, ScpMode::Legacy);
        assert_eq!(e.start, PathBuf::from("/home/choi"));
        assert!(e.session.is_alive());
        assert_eq!(e.session.name, "web");
        let log = f.log.lock().unwrap();
        assert_eq!(log[0], strs(&exec_argv(&target(), b"uname -s")));
    }

    #[test]
    fn establish_start_dir_keeps_newlines() {
        let f = establish_fake("OpenSSH_9.6p1\n", "Linux\n", b"/srv/a\nb \xed\x95\x9c\0");
        let cmd: Arc<dyn Cmd> = f;
        let e = establish(&cmd, "web", target(), None).unwrap();
        assert_eq!(e.start, PathBuf::from("/srv/a\nb 한"));
    }

    #[cfg(unix)]
    #[test]
    fn establish_start_dir_keeps_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let f = establish_fake("OpenSSH_9.6p1\n", "Linux\n", b"/srv/\xff\xfe\0trailing");
        let cmd: Arc<dyn Cmd> = f;
        let e = establish(&cmd, "web", target(), None).unwrap();
        assert_eq!(e.start.as_os_str().as_bytes(), b"/srv/\xff\xfe");
    }

    #[test]
    fn establish_rejects_unterminated_or_empty_start_dir() {
        for pwd in [&b"/no/nul"[..], b"\0", b""] {
            let f = establish_fake("OpenSSH_9.6p1\n", "Linux\n", pwd);
            let cmd: Arc<dyn Cmd> = f;
            assert!(establish(&cmd, "web", target(), None).is_err(), "{pwd:?}");
        }
    }

    #[test]
    fn establish_darwin_is_bsd_and_sftp() {
        let f = establish_fake("OpenSSH_9.6p1\n", "Darwin\n", b"/Users/x\0");
        let cmd: Arc<dyn Cmd> = f;
        let e = establish(&cmd, "mac", target(), Some("~/src")).unwrap();
        assert_eq!(e.session.os, RemoteOs::Bsd);
        assert_eq!(e.session.scp_mode, ScpMode::Sftp);
    }

    #[test]
    fn establish_fails_when_ssh_cannot_connect() {
        let f = Fake::new(|a| {
            if a == ["ssh", "-V"] {
                return out(0, "");
            }
            CmdOutput {
                code: Some(255),
                stdout: Vec::new(),
                stderr: b"Permission denied (publickey).\n".to_vec(),
            }
        });
        let cmd: Arc<dyn Cmd> = f;
        let err = establish(&cmd, "web", target(), None).unwrap_err();
        assert!(err.contains("Permission denied"), "{err}");
    }

    #[test]
    fn establish_fails_when_start_dir_is_missing() {
        let f = Fake::new(|a| match remote_cmd_of(a) {
            Some("uname -s") => out(0, "Linux\n"),
            Some(_) => CmdOutput {
                code: Some(1),
                stdout: Vec::new(),
                stderr: b"sh: cd: /nope: No such file or directory\n".to_vec(),
            },
            None => out(0, ""),
        });
        let cmd: Arc<dyn Cmd> = f;
        let err = establish(&cmd, "web", target(), Some("/nope")).unwrap_err();
        assert!(err.contains("No such file"), "{err}");
    }

    // ---- exec / disconnect / reconnect ----

    #[test]
    fn exit_255_marks_session_dead_and_blocks_further_commands() {
        let f = Fake::new(|_| out(255, ""));
        let s = session_with(f.clone(), RemoteOs::Linux);
        assert_eq!(s.exec(b"true").unwrap_err(), SessionError::Disconnected);
        assert!(!s.is_alive());
        let before = f.calls();
        assert_eq!(s.exec(b"true").unwrap_err(), SessionError::Disconnected);
        assert_eq!(f.calls(), before, "dead sessions do not spawn ssh");
        assert_eq!(SessionError::Disconnected.message(), disconnected_msg());
    }

    #[test]
    fn other_exit_codes_do_not_kill_the_session() {
        let f = Fake::new(|_| out(1, ""));
        let s = session_with(f, RemoteOs::Linux);
        assert_eq!(s.exec(b"false").unwrap().code, Some(1));
        assert!(s.is_alive());
    }

    #[test]
    fn revive_checks_master_and_flips_alive() {
        let up = Arc::new(AtomicBool::new(false));
        let up2 = Arc::clone(&up);
        let f = Fake::new(move |_| out(if up2.load(Ordering::SeqCst) { 0 } else { 255 }, ""));
        let s = session_with(f, RemoteOs::Linux);
        s.set_alive(false);
        assert!(!s.revive());
        assert!(!s.is_alive());
        up.store(true, Ordering::SeqCst);
        assert!(s.revive());
        assert!(s.is_alive());
    }

    // ---- listing (HDIR-37) ----

    fn linux_rec(kind: &str, size: u64, name: &str) -> String {
        format!("{kind}\t{size}\t1700000000.0\t644\t\t{name}\0")
    }

    #[test]
    fn list_parses_sorts_and_adds_parent() {
        let body = format!(
            "{}{}{}",
            linux_rec("f", 5, "b.txt"),
            linux_rec("d", 4096, "zdir"),
            linux_rec("f", 1, "한글 a.txt")
        );
        let f = Fake::new(move |_| out(0, &body));
        let s = session_with(f.clone(), RemoteOs::Linux);
        let (entries, skipped) = s.list(Path::new("/srv/it's"), false).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["..", "zdir", "b.txt", "한글 a.txt"]);
        assert_eq!(skipped, 0);
        let log = f.log.lock().unwrap();
        let cmd = remote_cmd_of(&log[0]).unwrap();
        assert!(cmd.starts_with("cd -- '/srv/it'\\''s' && find"), "{cmd}");
    }

    #[test]
    fn list_root_has_no_parent_entry() {
        let f = Fake::new(|_| out(0, ""));
        let s = session_with(f, RemoteOs::Linux);
        let (entries, _) = s.list(Path::new("/"), false).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn list_counts_malformed_records_and_partial_failures() {
        let body = format!("{}garbage\0", linux_rec("f", 5, "ok"));
        let f = Fake::new(move |_| out(1, &body)); // find: some dirs unreadable
        let s = session_with(f, RemoteOs::Linux);
        let (entries, skipped) = s.list(Path::new("/x"), false).unwrap();
        assert_eq!(entries.len(), 2); // .. + ok
        assert_eq!(skipped, 2); // garbage + exit status 1
    }

    #[test]
    fn list_error_carries_stderr_first_line() {
        let f = Fake::new(|_| CmdOutput {
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"sh: 1: cd: can't cd to /nope\nmore\n".to_vec(),
        });
        let s = session_with(f, RemoteOs::Linux);
        let err = s.list(Path::new("/nope"), false).unwrap_err();
        assert!(
            err.contains("can't cd to /nope") && !err.contains("more"),
            "{err}"
        );
    }

    #[test]
    fn list_on_disconnect_reports_reconnect_hint() {
        let f = Fake::new(|_| out(255, ""));
        let s = session_with(f, RemoteOs::Linux);
        assert_eq!(
            s.list(Path::new("/x"), false).unwrap_err(),
            disconnected_msg()
        );
        assert!(!s.is_alive());
    }

    #[test]
    fn ssh_vfs_lists_and_titles_with_server_name() {
        let f = Fake::new(|_| out(0, ""));
        let s = session_with(f, RemoteOs::Linux);
        let vfs = SshVfs { session: s };
        assert_eq!(vfs.display_path(Path::new("/var/log")), "web:/var/log");
        assert!(vfs.list(Path::new("/var"), false).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_refusal_in_english() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = std::fs::symlink_metadata(d.path()).unwrap();
        let e = crate::i18n::with_lang(crate::i18n::Lang::En, || {
            check_private_dir(&m, d.path()).unwrap_err().to_string()
        });
        assert!(e.contains("permissions too open (0700 required)"), "{e}");
        let ko = check_private_dir(&m, d.path()).unwrap_err().to_string();
        assert!(ko.contains("권한이 너무 넓음"), "{ko}");
    }

    #[test]
    fn spawn_error_texts_in_english() {
        crate::i18n::with_lang(crate::i18n::Lang::En, || {
            let out = spawn_failed(&crate::tr!("임시 파일: {}", "temp file: {}", "x"));
            assert_eq!(String::from_utf8_lossy(&out.stderr), "temp file: x");
            assert_eq!(
                first_line(b"", &crate::tr!("종료 코드 {}", "exit code {}", 3)),
                "exit code 3"
            );
            assert_eq!(disconnected_msg(), "Disconnected · Enter to reconnect");
        });
    }
}
