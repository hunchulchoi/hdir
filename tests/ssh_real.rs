//! Real ssh round trip. Ignored by default; run with
//! `HDIR_SSH_HOST=user@host cargo test --test ssh_real -- --ignored`
//! (key auth must work without prompts).

#![cfg(unix)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use hdir::remote::SshTarget;
use hdir::session::{self, Cmd, SystemCmd};

/// Closes the master and removes the socket dir even if an assertion fails.
struct Cleanup {
    target: SshTarget,
    dir: PathBuf,
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
        let _ = std::fs::remove_dir_all(&self.dir);
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
