//! The no-ControlMaster mode (what Windows uses) against a real server:
//! every command is its own login. Ignored by default; run with
//! `HDIR_SSH_HOST=user@host cargo test --test ssh_real_nomaster -- --ignored`
//! (key authentication must work without prompts).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hdir::fs_ops::{ConflictAction, OpControl, OpOutcome, Totals};
use hdir::job::JobKind;
use hdir::remote::{SshTarget, exec_argv, sh_quote, uses_master};
use hdir::session::{self, Cmd, SystemCmd};
use hdir::transfer::{Side, TransferCtl, TransferSpec, run};

struct Ctl(AtomicBool);

impl OpControl for Ctl {
    fn on_progress(&mut self, _: u64, _: &std::path::Path) {}
    fn is_cancelled(&self) -> bool {
        false
    }
    fn on_conflict(&mut self, _: &std::path::Path) -> ConflictAction {
        ConflictAction::Overwrite
    }
}

impl TransferCtl for Ctl {
    fn started(&mut self, _: Totals) {}
    fn cancel_flag(&self) -> &AtomicBool {
        &self.0
    }
}

/// Removes the fixed names this test makes, then the folder.
struct RemoteDir(Arc<dyn Cmd>, SshTarget, String);

impl Drop for RemoteDir {
    fn drop(&mut self) {
        let q = String::from_utf8(sh_quote(self.2.as_bytes())).unwrap();
        let s = format!("cd -- {q} && rm -f -- 'up.txt' 'down.txt'; cd / && rmdir -- {q}");
        let _ = self.0.run(&exec_argv(&self.1, s.as_bytes()));
    }
}

#[test]
#[ignore = "needs HDIR_SSH_HOST and a reachable ssh server"]
fn every_command_logs_in_on_its_own() {
    let host = std::env::var("HDIR_SSH_HOST").expect("set HDIR_SSH_HOST=user@host");
    let target = SshTarget {
        host,
        port: None,
        control_path: PathBuf::new(),
    };
    assert!(!uses_master(&target));
    let cmd: Arc<dyn Cmd> = Arc::new(SystemCmd);
    assert!(
        session::master_alive(&*cmd, &target),
        "a non-interactive login works"
    );
    let est = session::establish(&cmd, "it", target.clone(), Some("/tmp")).expect("establish");
    let s = Arc::clone(&est.session);
    let made = cmd.run(&exec_argv(&target, b"mktemp -d"));
    assert_eq!(made.code, Some(0));
    let dir = String::from_utf8(made.stdout)
        .unwrap()
        .trim_end()
        .to_string();
    assert!(dir.starts_with('/') && !dir.contains('\n'));
    let _cleanup = RemoteDir(Arc::clone(&cmd), target.clone(), dir.clone());
    let (entries, _) = s.list(std::path::Path::new(&dir), true).expect("list");
    assert_eq!(entries.len(), 1, "only .. in a fresh folder");

    let local = tempfile::tempdir().unwrap();
    std::fs::write(local.path().join("up.txt"), "from here").unwrap();
    let spec = |src: Side, src_dir: PathBuf, name: &str, dst: Side, dest: PathBuf| TransferSpec {
        kind: JobKind::Copy,
        src,
        src_dir,
        names: vec![OsString::from(name)],
        dst,
        dest_dir: dest,
        trash: hdir::trash::system_store(),
    };
    let ctl = || Ctl(AtomicBool::new(false));
    let r = run(
        &spec(
            Side::Local,
            local.path().into(),
            "up.txt",
            Side::Remote(Arc::clone(&s)),
            dir.clone().into(),
        ),
        &mut ctl(),
    );
    assert_eq!(
        (r.outcome, r.copied),
        (OpOutcome::Done, 1),
        "{:?}",
        r.warnings
    );
    let mk = format!(
        "cd -- {} && printf 'from there' > down.txt",
        String::from_utf8(sh_quote(dir.as_bytes())).unwrap()
    );
    assert_eq!(cmd.run(&exec_argv(&target, mk.as_bytes())).code, Some(0));
    let back = tempfile::tempdir().unwrap();
    let r = run(
        &spec(
            Side::Remote(Arc::clone(&s)),
            dir.clone().into(),
            "down.txt",
            Side::Local,
            back.path().into(),
        ),
        &mut ctl(),
    );
    assert_eq!(
        (r.outcome, r.copied),
        (OpOutcome::Done, 1),
        "{:?}",
        r.warnings
    );
    assert_eq!(
        std::fs::read(back.path().join("down.txt")).unwrap(),
        b"from there"
    );
    let up = s
        .exec(
            format!(
                "cat -- {}/up.txt",
                String::from_utf8(sh_quote(dir.as_bytes())).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
    assert_eq!(up.stdout, b"from here");
}
