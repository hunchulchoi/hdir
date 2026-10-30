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

/// Names the file operations test creates (and the cleanup removes by name).
const OPS_NAMES: [&str; 15] = [
    "mode600.txt",
    "hl-a",
    "hl-b",
    "up-link",
    "real-dir",
    "새 폴더 'q'",
    "moved dir",
    "t.txt",
    "bin.dat",
    "fifo",
    "broken",
    "edit me.txt",
    "target.txt",
    "ln-to-target",
    "exists.txt",
];

struct OpsTmp {
    cmd: Arc<dyn Cmd>,
    target: SshTarget,
    dir: String,
}

impl Drop for OpsTmp {
    fn drop(&mut self) {
        let q = |s: &str| String::from_utf8(sh_quote(s.as_bytes())).unwrap();
        let names: Vec<String> = OPS_NAMES.iter().map(|n| q(n)).collect();
        // Only the names this test created, relative to the (quoted) temp dir;
        // `&&` stops everything if the cd fails.
        let script = format!(
            "cd -- {d} && rm -rf -- {n} .hdir-edit.*; cd / && rmdir -- {d}",
            d = q(&self.dir),
            n = names.join(" ")
        );
        let _ = remote(&self.cmd, &self.target, script.as_bytes());
    }
}

#[test]
#[ignore = "needs HDIR_SSH_HOST and a reachable ssh server"]
fn remote_file_operations_round_trip() {
    use hdir::preview::{Limits, Preview};
    use hdir::remote_ops as ops;
    use std::ffi::OsString;
    use std::path::Path;

    let host = std::env::var("HDIR_SSH_HOST").expect("set HDIR_SSH_HOST=user@host");
    let (_cleanup, cmd, est, _tmp) = connect(host);
    let s = Arc::clone(&est.session);
    let target = s.target.clone();

    let made = remote(&cmd, &target, b"mktemp -d");
    assert_eq!(made.code, Some(0), "mktemp -d failed");
    let dir = String::from_utf8(made.stdout)
        .unwrap()
        .trim_end()
        .to_string();
    assert!(dir.starts_with('/') && !dir.contains('\n'), "{dir:?}");
    let _guard = OpsTmp {
        cmd: Arc::clone(&cmd),
        target: target.clone(),
        dir: dir.clone(),
    };
    let d = Path::new(&dir);
    let run = |script: &str| {
        let q = String::from_utf8(sh_quote(dir.as_bytes())).unwrap();
        let o = remote(&cmd, &target, format!("cd -- {q} && {script}").as_bytes());
        assert_eq!(
            o.code,
            Some(0),
            "{script}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };

    // mkdir (not -p): works once, then reports the existing name
    let made = ops::mkdir(&s, d, "새 폴더 'q'").expect("mkdir");
    assert_eq!(made, d.join("새 폴더 'q'"));
    assert!(
        ops::mkdir(&s, d, "새 폴더 'q'").is_err(),
        "mkdir must not be -p"
    );
    assert!(ops::mkdir(&s, d, "a/b").is_err());

    // rename: moves; refuses an existing target (file and dir alike)
    let moved = ops::rename(&s, &made, "moved dir").expect("rename");
    assert_eq!(moved, d.join("moved dir"));
    run("printf x > exists.txt && printf y > t.txt");
    let e = ops::rename(&s, &d.join("t.txt"), "exists.txt").unwrap_err();
    assert!(e.contains("이미 존재함"), "{e}");
    assert_eq!(run("cat exists.txt"), "x", "target untouched");
    let e = ops::rename(&s, &d.join("t.txt"), "moved dir").unwrap_err();
    assert!(e.contains("이미 존재함"), "{e}");
    assert_eq!(run("[ -f t.txt ] && echo still-there"), "still-there\n");

    // undo of mkdir = rmdir: only when empty
    run("mkdir tmpdir-undo && printf z > tmpdir-undo/f");
    assert!(ops::rmdir(&s, &d.join("tmpdir-undo")).is_err(), "not empty");
    run("rm -f -- tmpdir-undo/f && rmdir -- tmpdir-undo");
    ops::mkdir(&s, d, "tmpdir-undo").unwrap();
    ops::rmdir(&s, &d.join("tmpdir-undo")).expect("empty dir goes away");

    // preview: text / binary / special / broken link / dir
    run(
        "printf 'hello 한글\\nline2\\n' > t.txt && printf '\\000\\001\\002' > bin.dat \
         && mkfifo fifo && ln -s nowhere broken",
    );
    match ops::preview(&s, &d.join("t.txt"), Limits::PREVIEW) {
        Preview::Text { lines, truncated } => {
            assert_eq!(lines, vec!["hello 한글".to_string(), "line2".to_string()]);
            assert!(!truncated);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        ops::preview(&s, &d.join("bin.dat"), Limits::PREVIEW),
        Preview::Binary { size: 3, .. }
    ));
    assert_eq!(
        ops::preview(&s, &d.join("fifo"), Limits::PREVIEW),
        Preview::Binary {
            size: 0,
            kind: "특수 파일"
        }
    );
    assert_eq!(
        ops::preview(&s, &d.join("broken"), Limits::PREVIEW),
        Preview::Error("깨진 링크".into())
    );
    assert!(matches!(
        ops::preview(&s, &moved, Limits::PREVIEW),
        Preview::Info(m) if m.contains("항목 0개")
    ));
    assert!(ops::probe_text(&s, &d.join("fifo")).is_err());
    assert_eq!(ops::probe_text(&s, &d.join("t.txt")), Ok(true));
    assert_eq!(ops::probe_text(&s, &d.join("bin.dat")), Ok(false));

    // edit round trip: download -> change -> upload replaces atomically
    run("printf 'original' > 'edit me.txt' && chmod 640 'edit me.txt'");
    const MODE_OF: &str = "stat -c %a 'edit me.txt' 2>/dev/null || stat -f %Lp 'edit me.txt'";
    let mode_before = run(MODE_OF).trim().to_string();
    assert_eq!(mode_before, "640");
    let copy = ops::fetch(&s, &d.join("edit me.txt")).expect("download");
    assert_eq!(std::fs::read_to_string(&copy.local).unwrap(), "original");
    assert!(!copy.stamp.differs(&copy.local).unwrap());
    std::fs::write(&copy.local, "changed on the server").unwrap();
    assert!(copy.stamp.differs(&copy.local).unwrap());
    ops::upload_replace(&s, &copy.local, &d.join("edit me.txt"), &copy.remote).expect("upload");
    assert_eq!(run("cat 'edit me.txt'"), "changed on the server");
    assert_eq!(
        run("ls -A | grep -c hdir-edit || true").trim(),
        "0",
        "no temp left"
    );
    assert_eq!(
        run(MODE_OF).trim(),
        mode_before,
        "permissions survive the upload"
    );

    // mode 600 survives too
    run("printf 'secret' > mode600.txt && chmod 600 mode600.txt");
    let m = |f: &str| {
        run(&format!("stat -c %a {f} 2>/dev/null || stat -f %Lp {f}"))
            .trim()
            .to_string()
    };
    let before = m("mode600.txt");
    let copy = ops::fetch(&s, &d.join("mode600.txt")).unwrap();
    std::fs::write(&copy.local, "secret edited").unwrap();
    ops::upload_replace(&s, &copy.local, &d.join("mode600.txt"), &copy.remote).unwrap();
    assert_eq!(run("cat mode600.txt"), "secret edited");
    assert_eq!(m("mode600.txt"), before);
    assert_eq!(before, "600");

    // hard-linked file: rewritten in place, the link survives
    run("printf 'shared' > hl-a && ln hl-a hl-b");
    let nlink = |f: &str| {
        run(&format!("stat -c %h {f} 2>/dev/null || stat -f %l {f}"))
            .trim()
            .to_string()
    };
    assert_eq!(nlink("hl-a"), "2");
    let copy = ops::fetch(&s, &d.join("hl-a")).unwrap();
    std::fs::write(&copy.local, "shared edited").unwrap();
    ops::upload_replace(&s, &copy.local, &d.join("hl-a"), &copy.remote).unwrap();
    assert_eq!(nlink("hl-a"), "2", "hard link count kept");
    assert_eq!(run("cat hl-b"), "shared edited");

    // symlink: the link survives and the target changes (target in another
    // directory, mode kept, no temp left in either place)
    run(
        "mkdir real-dir && printf 'before' > real-dir/target.txt && chmod 640 real-dir/target.txt \
         && ln -s real-dir/target.txt ln-to-target",
    );
    let copy = ops::fetch(&s, &d.join("ln-to-target")).expect("download via link");
    std::fs::write(&copy.local, "after").unwrap();
    ops::upload_replace(&s, &copy.local, &d.join("ln-to-target"), &copy.remote).unwrap();
    assert_eq!(
        run("[ -L ln-to-target ] && cat real-dir/target.txt"),
        "after"
    );
    assert_eq!(m("real-dir/target.txt"), "640");
    assert_eq!(
        run("ls -A . real-dir | grep -c hdir-edit || true").trim(),
        "0",
        "no temp left"
    );

    // changed on the server while editing: not replaced, theirs kept
    run("printf 'v1' > race.txt");
    let copy = ops::fetch(&s, &d.join("race.txt")).unwrap();
    run("printf 'v2' > race.txt");
    std::fs::write(&copy.local, "mine").unwrap();
    let e = ops::upload_replace(&s, &copy.local, &d.join("race.txt"), &copy.remote).unwrap_err();
    assert!(e.contains("원격 파일이 바뀌어"), "{e}");
    assert_eq!(run("cat race.txt"), "v2");
    assert_eq!(run("ls -A | grep -c hdir-edit || true").trim(), "0");

    // a directory whose name ends in a newline vs. its twin without it
    // (`$n` = "tw\n"; a bare `$(printf ...)` would drop the newline)
    const NL: &str = "n=$(printf 'tw\\nx'); n=${n%x}; ";
    run(&format!(
        "{NL}mkdir -- \"$n\" tw && printf keep > tw/f && printf keep > \"$n/f\""
    ));
    let twin = d.join("tw\n");
    let copy = ops::fetch(&s, &twin.join("f")).unwrap();
    std::fs::write(&copy.local, "edited").unwrap();
    ops::upload_replace(&s, &copy.local, &twin.join("f"), &copy.remote).unwrap();
    assert_eq!(run(&format!("{NL}cat -- \"$n/f\"")), "edited");
    assert_eq!(run("cat tw/f"), "keep", "the twin is untouched");
    ops::delete(&s, &twin, &[OsString::from("f")]).unwrap();
    assert_eq!(run(&format!("{NL}[ -e \"$n/f\" ] || cat tw/f")), "keep");

    // failure path: a vanished directory leaves nothing behind and errors
    let gone = d.join("moved dir").join("nope").join("f.txt");
    assert!(ops::upload_replace(&s, &copy.local, &gone, &copy.remote).is_err());

    // cd resolution
    assert_eq!(ops::resolve_cd(&s, d, "moved dir").unwrap(), moved);
    assert_eq!(ops::resolve_cd(&s, &moved, "..").unwrap(), d);
    assert!(ops::resolve_cd(&s, d, "does-not-exist").is_err());

    // delete: weird names go; the remote home and root are refused
    let names: Vec<OsString> = ["moved dir", "exists.txt"].map(OsString::from).to_vec();
    ops::delete(&s, d, &names).expect("delete");
    assert_eq!(
        run("[ -e 'moved dir' ] || [ -e exists.txt ] || echo gone").trim(),
        "gone"
    );
    let home = String::from_utf8(remote(&cmd, &target, b"printf %s \"$HOME\"").stdout).unwrap();
    let home_path = Path::new(&home);
    if let (Some(parent), Some(name)) = (home_path.parent(), home_path.file_name()) {
        assert!(ops::delete(&s, parent, &[name.to_os_string()]).is_err());
        assert_eq!(
            run(&format!(
                "[ -d {} ] && echo kept",
                String::from_utf8(sh_quote(home.as_bytes())).unwrap()
            ))
            .trim(),
            "kept"
        );
    }
    // reaching the home's parent through a symlink in the panel cwd
    run("ln -s \"$(dirname \"$HOME\")\" up-link");
    if let Some(name) = home_path.file_name() {
        let via_link = d.join("up-link");
        let e = ops::delete(&s, &via_link, &[name.to_os_string()]).unwrap_err();
        assert!(e.contains("삭제 거부"), "{e}");
        assert_eq!(
            run("[ -d \"$HOME\" ] && echo kept").trim(),
            "kept",
            "home untouched"
        );
    }
    assert!(ops::delete(&s, Path::new("/"), &[OsString::from("..")]).is_err());
}
