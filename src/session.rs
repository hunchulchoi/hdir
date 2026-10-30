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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::loader::Listing;
use crate::panel::finish_listing;
use crate::platform::{os_bytes, os_from_bytes};
use crate::remote::{
    RemoteEntry, RemoteOs, ScpMode, SshTarget, check_argv, exec_argv, list_cmd, master_argv,
    parse_listing, parse_uname, scp_mode_from_version, sh_quote,
};
use crate::vfs::Vfs;

/// Status text of a panel whose connection died.
pub const DISCONNECTED_MSG: &str = "연결 끊김 · Enter로 재연결";

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
}

/// The real process runner.
pub struct SystemCmd;

impl Cmd for SystemCmd {
    fn run(&self, argv: &[OsString]) -> CmdOutput {
        let Some((prog, args)) = argv.split_first() else {
            return spawn_failed("empty command");
        };
        match Command::new(prog).args(args).stdin(Stdio::null()).output() {
            Ok(o) => CmdOutput {
                code: o.status.code(),
                stdout: o.stdout,
                stderr: o.stderr,
            },
            Err(e) => spawn_failed(&format!("{}: {e}", prog.to_string_lossy())),
        }
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
fn first_line(stderr: &[u8], fallback: &str) -> String {
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
            SessionError::Disconnected => DISCONNECTED_MSG.to_string(),
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
        match out.code {
            Some(255) => {
                self.set_alive(false);
                Err(SessionError::Disconnected)
            }
            None => Err(SessionError::Spawn(first_line(
                &out.stderr,
                "ssh 실행 실패",
            ))),
            Some(_) => Ok(out),
        }
    }

    /// Reconnect check: is the master alive again? Flips `alive` accordingly.
    pub fn revive(&self) -> bool {
        let up = master_alive(&*self.cmd, &self.target);
        self.set_alive(up);
        up
    }

    /// List `dir` on the remote host (blocking; run on a worker).
    pub fn list(&self, dir: &Path, show_hidden: bool) -> Listing {
        let bytes = os_bytes(dir.as_os_str());
        let out = self
            .exec(&list_cmd(&self.os, &bytes, show_hidden))
            .map_err(|e| e.message())?;
        let failed = out.code != Some(0);
        if failed && out.stdout.is_empty() {
            return Err(first_line(
                &out.stderr,
                &format!("종료 코드 {}", out.code.unwrap_or(-1)),
            ));
        }
        let (entries, bad) = parse_listing(&self.os, &out.stdout);
        let items = entries.iter().map(RemoteEntry::to_entry).collect();
        // A non-zero exit with output means find/stat skipped something.
        let skipped = bad + usize::from(failed);
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

/// Create the control socket directory with mode 0700.
pub fn ensure_control_dir(path: &Path) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
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
        Some(c) => Err(format!("ssh 연결 실패 (종료 코드 {c})")),
        None => Err(first_line(&out.stderr, "ssh 실행 실패")),
    }
}

/// Remote shell command that prints the absolute form of `path` (`~` and
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
    [b"cd -- ".as_slice(), &target, b" && pwd"].concat()
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
        Some(255) => return Err(first_line(&uname.stderr, "ssh 연결 실패")),
        Some(c) => {
            return Err(first_line(
                &uname.stderr,
                &format!("uname 실패 (종료 코드 {c})"),
            ));
        }
        None => return Err(first_line(&uname.stderr, "ssh 실행 실패")),
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
        return Err(first_line(&out.stderr, "시작 경로를 열 수 없음"));
    }
    let pwd = String::from_utf8_lossy(&out.stdout);
    let line = pwd.lines().next().unwrap_or("").trim_end_matches('\r');
    if line.is_empty() {
        return Err("시작 경로를 알 수 없음".to_string());
    }
    let start = PathBuf::from(os_from_bytes(line.as_bytes().to_vec()));
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
        respond: Responder,
    }

    impl Fake {
        fn new(respond: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                log: Mutex::new(Vec::new()),
                interactive: Mutex::new(Vec::new()),
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
        assert_eq!(s(""), "cd -- . && pwd");
        assert_eq!(s("."), "cd -- . && pwd");
        assert_eq!(s("/var/it's here"), "cd -- '/var/it'\\''s here' && pwd");
        assert_eq!(s("~"), "cd -- \"$HOME\" && pwd");
        assert_eq!(s("~/my dir"), "cd -- \"$HOME\"'/my dir' && pwd");
    }

    fn establish_fake(ssh_v: &'static str, uname: &'static str, pwd: &'static str) -> Arc<Fake> {
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
                Some(c) if c.ends_with("&& pwd") => out(0, pwd),
                _ => out(0, ""),
            }
        })
    }

    #[test]
    fn establish_detects_os_scp_mode_and_start_dir() {
        let f = establish_fake("OpenSSH_8.4p1, OpenSSL\n", "Linux\n", "/home/choi\n");
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
    fn establish_darwin_is_bsd_and_sftp() {
        let f = establish_fake("OpenSSH_9.6p1\n", "Darwin\n", "/Users/x\n");
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
        assert_eq!(SessionError::Disconnected.message(), DISCONNECTED_MSG);
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
            DISCONNECTED_MSG
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
}
