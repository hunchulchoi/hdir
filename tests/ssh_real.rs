//! Real ssh round trip. Ignored by default; run with
//! `HDIR_SSH_HOST=user@host cargo test --test ssh_real -- --ignored`
//! (key auth must work without prompts).

#![cfg(unix)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use hdir::panel::EntryKind;
use hdir::remote::{SshTarget, exec_argv, sh_quote};
use hdir::session::{self, Cmd, Established, SystemCmd};

/// Closes the master and removes the socket dir even if an assertion fails.
struct Cleanup {
    target: SshTarget,
    dir: PathBuf,
    owned_dir: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut exit: Vec<OsString> = vec!["ssh".into(), "-O".into(), "exit".into()];
        exit.push("-o".into());
        let mut cp = OsString::from("ControlPath=");
        cp.push(&self.target.control_path);
        exit.push(cp);
        exit.push("--".into());
        exit.push((&self.target.host).into());
        let _ = SystemCmd.run(&exit);
        // The shared fallback dir (/tmp/hdir-<uid>) is left alone; the socket
        // vanishes with the master.
        if self.owned_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[test]
#[ignore = "needs HDIR_SSH_HOST and a reachable ssh server"]
fn controlmaster_connect_list_and_exit() {
    let host = std::env::var("HDIR_SSH_HOST").expect("set HDIR_SSH_HOST=user@host");
    let dir = PathBuf::from(format!("/tmp/hdir-it-{}", std::process::id()));
    let target = SshTarget {
        host,
        port: None,
        control_path: dir.join("cm-%C"),
    };
    session::ensure_control_dir(&target.control_path).unwrap();
    let _cleanup = Cleanup {
        target: target.clone(),
        dir,
        owned_dir: true,
    };

    let cmd: Arc<dyn Cmd> = Arc::new(SystemCmd);
    assert!(!session::master_alive(&*cmd, &target), "no master yet");
    session::start_master(&*cmd, &target).expect("ssh -fN");
    assert!(session::master_alive(&*cmd, &target), "master is up");

    let est = session::establish(&cmd, "it", target.clone(), Some("/tmp")).expect("establish");
    assert_eq!(est.start, PathBuf::from("/tmp"));
    println!(
        "remote os: {:?}, scp: {:?}",
        est.session.os, est.session.scp_mode
    );

    let (entries, skipped) = est
        .session
        .list(&est.start, true)
        .expect("list /tmp over ssh");
    println!("{} entries, {skipped} skipped", entries.len());
    assert_eq!(entries[0].name, "..");
    assert!(est.session.is_alive());

    // Kill the master: the next command is a disconnect, and revive fails.
    let mut exit: Vec<OsString> = vec!["ssh".into(), "-O".into(), "exit".into(), "-o".into()];
    let mut cp = OsString::from("ControlPath=");
    cp.push(&target.control_path);
    exit.push(cp);
    exit.push("--".into());
    exit.push((&target.host).into());
    assert_eq!(cmd.run(&exit).code, Some(0));
    assert!(!session::master_alive(&*cmd, &target), "master closed");
}

/// Master + control socket dir for the listing test. The socket dir is a fresh
/// temp dir when its path is short enough, else the session's fallback.
fn connect(host: String) -> (Cleanup, Arc<dyn Cmd>, Established, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let target = SshTarget {
        host,
        port: None,
        control_path: session::control_path(Some(tmp.path()), uid),
    };
    session::ensure_control_dir(&target.control_path).unwrap();
    let dir = target.control_path.parent().unwrap().to_path_buf();
    let owned_dir = dir.starts_with(tmp.path());
    let cleanup = Cleanup {
        target: target.clone(),
        dir,
        owned_dir,
    };
    let cmd: Arc<dyn Cmd> = Arc::new(SystemCmd);
    session::start_master(&*cmd, &target).expect("ssh -fN");
    let est = session::establish(&cmd, "it", target, Some("/tmp")).expect("establish");
    (cleanup, cmd, est, tmp)
}

fn remote(cmd: &Arc<dyn Cmd>, target: &SshTarget, script: &[u8]) -> session::CmdOutput {
    cmd.run(&exec_argv(target, script))
}

/// Removes exactly the files the test created, then the (empty) directory.
struct RemoteTmp {
    cmd: Arc<dyn Cmd>,
    target: SshTarget,
    dir: String,
}

const FILES: [&str; 4] = ["normal.txt", "with space.txt", "한글 파일.txt", "link"];

impl Drop for RemoteTmp {
    fn drop(&mut self) {
        let q = |s: &str| String::from_utf8(sh_quote(s.as_bytes())).unwrap();
        let files: Vec<String> = FILES.iter().map(|f| q(f)).collect();
        let script = format!(
            "cd -- {d} && rm -f -- {f} && rmdir -- sub; cd / && rmdir -- {d}",
            d = q(&self.dir),
            f = files.join(" ")
        );
        let _ = remote(&self.cmd, &self.target, script.as_bytes());
    }
}

#[test]
#[ignore = "needs HDIR_SSH_HOST and a reachable ssh server"]
fn lists_real_remote_files_with_odd_names() {
    let host = std::env::var("HDIR_SSH_HOST").expect("set HDIR_SSH_HOST=user@host");
    let (_cleanup, cmd, est, _tmp) = connect(host);
    let target = est.session.target.clone();

    let made = remote(&cmd, &target, b"mktemp -d");
    assert_eq!(made.code, Some(0), "mktemp -d failed");
    let dir = String::from_utf8(made.stdout)
        .unwrap()
        .trim_end()
        .to_string();
    assert!(dir.starts_with('/') && !dir.contains('\n'), "{dir:?}");
    let _remote_tmp = RemoteTmp {
        cmd: Arc::clone(&cmd),
        target: target.clone(),
        dir: dir.clone(),
    };

    let q = |s: &str| String::from_utf8(sh_quote(s.as_bytes())).unwrap();
    let script = format!(
        "cd -- {} && printf hello > normal.txt && printf abc > {} && printf abcdefg > {} \
         && mkdir sub && ln -s sub link",
        q(&dir),
        q("with space.txt"),
        q("한글 파일.txt")
    );
    assert_eq!(remote(&cmd, &target, script.as_bytes()).code, Some(0));

    let (entries, skipped) = est
        .session
        .list(std::path::Path::new(&dir), true)
        .expect("list temp dir");
    assert_eq!(skipped, 0);
    let by = |n: &str| {
        entries
            .iter()
            .find(|e| e.name == n)
            .unwrap_or_else(|| panic!("{n} missing in {entries:?}"))
    };
    assert_eq!(entries[0].name, "..");
    assert_eq!(entries.len(), 1 + 5, "{entries:?}");
    assert_eq!(
        (by("normal.txt").kind.clone(), by("normal.txt").size),
        (EntryKind::File, 5)
    );
    assert_eq!(by("with space.txt").size, 3);
    assert_eq!(by("with space.txt").kind, EntryKind::File);
    assert_eq!(by("한글 파일.txt").size, 7);
    assert_eq!(by("sub").kind, EntryKind::Dir);
    assert_eq!(
        by("link").kind,
        EntryKind::Symlink {
            target_is_dir: true
        }
    );
}
