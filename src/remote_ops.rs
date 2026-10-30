//! File operations on a remote host over a [`RemoteSession`]: mkdir, rename,
//! delete, preview reads, and transfers for editing. Everything here blocks;
//! callers run it on a worker thread. Command construction lives in
//! [`crate::remote`]; these functions add validation and result mapping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::os_bytes;
use crate::remote::{RENAME_EXISTS, home_cmd, mkdir_cmd, rename_cmd, rm_cmd, rmdir_cmd};
use crate::session::{CmdOutput, RemoteSession, first_line};

/// A name that is safe to join onto a remote directory.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err("잘못된 이름".into());
    }
    Ok(())
}

/// Run `cmd`; a non-zero exit becomes the first stderr line.
fn run_ok(s: &RemoteSession, cmd: &[u8]) -> Result<CmdOutput, String> {
    let out = s.exec(cmd).map_err(|e| e.message())?;
    if out.code == Some(0) {
        return Ok(out);
    }
    Err(first_line(
        &out.stderr,
        &format!("종료 코드 {}", out.code.unwrap_or(-1)),
    ))
}

/// `mkdir -- parent/name` (never `-p`: an existing name is an error).
pub fn mkdir(s: &RemoteSession, parent: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    let path = parent.join(name);
    run_ok(s, &mkdir_cmd(&os_bytes(path.as_os_str())))?;
    Ok(path)
}

/// Rename `from` to `new_name` in the same directory without replacing.
pub fn rename(s: &RemoteSession, from: &Path, new_name: &str) -> Result<PathBuf, String> {
    validate_name(new_name)?;
    let to = from.with_file_name(new_name);
    let cmd = rename_cmd(&os_bytes(from.as_os_str()), &os_bytes(to.as_os_str()));
    let out = s.exec(&cmd).map_err(|e| e.message())?;
    match out.code {
        Some(0) => Ok(to),
        Some(RENAME_EXISTS) => Err(format!("이미 존재함: {new_name}")),
        c => Err(first_line(
            &out.stderr,
            &format!("종료 코드 {}", c.unwrap_or(-1)),
        )),
    }
}

/// `rmdir` (only an empty directory goes away).
pub fn rmdir(s: &RemoteSession, path: &Path) -> Result<(), String> {
    run_ok(s, &rmdir_cmd(&os_bytes(path.as_os_str()))).map(|_| ())
}

/// Absolute paths of `names` in `cwd`, refusing anything that is not a plain
/// child name (empty, `.`, `..`, containing `/`) or a relative `cwd`.
fn delete_paths(cwd: &Path, names: &[OsString]) -> Result<Vec<Vec<u8>>, String> {
    let base = os_bytes(cwd.as_os_str()).into_owned();
    if base.first() != Some(&b'/') {
        return Err("삭제 거부: 절대 경로가 아님".into());
    }
    if names.is_empty() {
        return Err("삭제할 항목 없음".into());
    }
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let n = os_bytes(n).into_owned();
        if n.is_empty() || n == b"." || n == b".." || n.contains(&b'/') || n.contains(&0) {
            return Err(format!(
                "삭제 거부: 잘못된 이름 {:?}",
                String::from_utf8_lossy(&n)
            ));
        }
        let mut path = base.clone();
        while path.len() > 1 && path.ends_with(b"/") {
            path.pop();
        }
        if path != b"/" {
            path.push(b'/');
        }
        path.extend_from_slice(&n);
        out.push(path);
    }
    Ok(out)
}

/// The remote `$HOME` (absolute, non-empty), or an error.
fn remote_home(s: &RemoteSession) -> Result<Vec<u8>, String> {
    let out = run_ok(s, &home_cmd())?;
    let end = out.stdout.iter().position(|&b| b == 0).unwrap_or(0);
    let home = &out.stdout[..end];
    if home.first() != Some(&b'/') {
        return Err("삭제 거부: 원격 홈 디렉터리를 알 수 없음".into());
    }
    Ok(home.to_vec())
}

/// Permanently delete `names` in `cwd` with `rm -rf`. Refuses to touch the
/// remote home directory or any of its ancestors (home is looked up first; if
/// that fails nothing is deleted).
pub fn delete(s: &RemoteSession, cwd: &Path, names: &[OsString]) -> Result<(), String> {
    let paths = delete_paths(cwd, names)?;
    let home = remote_home(s)?;
    for p in &paths {
        let ancestor =
            home == *p || (home.starts_with(p) && home.get(p.len()) == Some(&b'/')) || p == b"/";
        if ancestor {
            return Err("삭제 거부: 원격 홈 디렉터리(또는 상위)는 지울 수 없음".into());
        }
    }
    let refs: Vec<&[u8]> = paths.iter().map(Vec::as_slice).collect();
    run_ok(s, &rm_cmd(&refs)).map(|_| ())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::{FakeCmd, code, fail, ok, session};
    use std::sync::Arc;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn names_are_validated() {
        for bad in ["", ".", "..", "a/b", "a\0b", "/"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        for good in [
            "a b",
            "it's",
            "-x",
            "한글",
            "line\nbreak",
            "\"q\"",
            ".hidden",
        ] {
            assert!(validate_name(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn mkdir_runs_a_quoted_plain_mkdir() {
        let f = FakeCmd::new(|_| ok(""));
        let s = session(Arc::clone(&f));
        let got = mkdir(&s, &p("/srv/it's dir"), "한글 \"x\"\n-lead").unwrap();
        assert_eq!(got, p("/srv/it's dir/한글 \"x\"\n-lead"));
        assert_eq!(
            f.remote_cmds(),
            vec!["mkdir -- '/srv/it'\\''s dir/한글 \"x\"\n-lead'".to_string()]
        );
    }

    #[test]
    fn mkdir_failure_carries_stderr_and_bad_names_never_reach_ssh() {
        let f = FakeCmd::new(|_| fail(1, "mkdir: cannot create directory 'x': File exists\n"));
        let s = session(Arc::clone(&f));
        let e = mkdir(&s, &p("/srv"), "x").unwrap_err();
        assert!(e.contains("File exists"), "{e}");
        assert!(mkdir(&s, &p("/srv"), "a/b").is_err());
        assert_eq!(f.remote_cmds().len(), 1);
    }

    #[test]
    fn mkdir_on_a_dead_session_reports_the_reconnect_hint() {
        let f = FakeCmd::new(|_| code(255));
        let s = session(f);
        assert_eq!(
            mkdir(&s, &p("/srv"), "x").unwrap_err(),
            crate::session::DISCONNECTED_MSG
        );
    }

    #[test]
    fn rename_checks_target_and_moves_without_clobbering() {
        let f = FakeCmd::new(|_| ok(""));
        let s = session(Arc::clone(&f));
        let to = rename(&s, &p("/srv/old name"), "new'name").unwrap();
        assert_eq!(to, p("/srv/new'name"));
        assert_eq!(
            f.remote_cmds(),
            vec![
                String::from_utf8(crate::remote::rename_cmd(
                    b"/srv/old name",
                    b"/srv/new'name"
                ))
                .unwrap()
            ]
        );
    }

    #[test]
    fn rename_onto_an_existing_name_is_an_error() {
        let f = FakeCmd::new(|_| code(crate::remote::RENAME_EXISTS));
        let s = session(f);
        assert_eq!(rename(&s, &p("/srv/a"), "b").unwrap_err(), "이미 존재함: b");
        assert!(rename(&s, &p("/srv/a"), "../b").is_err());
    }

    #[test]
    fn rmdir_runs_plain_rmdir() {
        let f = FakeCmd::new(|_| ok(""));
        let s = session(Arc::clone(&f));
        rmdir(&s, &p("/srv/-d")).unwrap();
        assert_eq!(f.remote_cmds(), vec!["rmdir -- '/srv/-d'".to_string()]);
        let f = FakeCmd::new(|_| fail(1, "rmdir: failed: Directory not empty\n"));
        let s = session(f);
        assert!(rmdir(&s, &p("/srv/d")).unwrap_err().contains("not empty"));
    }

    /// Answers the HOME lookup, everything else succeeds.
    fn with_home(home: &'static str) -> Arc<FakeCmd> {
        FakeCmd::new(move |a| {
            let c = crate::testutil::remote_cmd(a).unwrap_or("");
            if c.contains("$HOME") {
                ok(&format!("{home}\0"))
            } else {
                ok("")
            }
        })
    }

    fn only_rm(f: &FakeCmd) -> Vec<String> {
        f.remote_cmds()
            .into_iter()
            .filter(|c| c.starts_with("rm "))
            .collect()
    }

    #[test]
    fn delete_removes_quoted_absolute_paths() {
        let f = with_home("/home/choi");
        let s = session(Arc::clone(&f));
        let names: Vec<OsString> = ["a b", "it's", "-r", "x\ny", "한글"]
            .map(OsString::from)
            .to_vec();
        delete(&s, &p("/srv/data"), &names).unwrap();
        assert_eq!(
            only_rm(&f),
            vec![
                "rm -rf -- '/srv/data/a b' '/srv/data/it'\\''s' '/srv/data/-r' \
                 '/srv/data/x\ny' '/srv/data/한글'"
                    .to_string()
            ]
        );
    }

    #[test]
    fn delete_at_the_root_builds_single_slash_paths() {
        let f = with_home("/home/choi");
        let s = session(Arc::clone(&f));
        delete(&s, &p("/"), &[OsString::from("tmp")]).unwrap();
        assert_eq!(only_rm(&f), vec!["rm -rf -- '/tmp'".to_string()]);
    }

    #[test]
    fn delete_refuses_dangerous_targets_without_running_rm() {
        let cases: Vec<(&str, Vec<&str>, &str)> = vec![
            ("/srv", vec![""], "empty name"),
            ("/srv", vec!["."], "dot"),
            ("/srv", vec![".."], "dotdot"),
            ("/srv", vec!["a/b"], "slash in name"),
            ("/srv", vec!["a", ".."], "dotdot among others"),
            ("/srv", vec![], "nothing selected"),
            ("srv", vec!["a"], "relative cwd"),
            ("", vec!["a"], "empty cwd"),
            ("/home", vec!["choi"], "the remote home itself"),
            ("/", vec!["home"], "an ancestor of the remote home"),
            ("/home/choi", vec!["x", "../choi"], "dotdot to home"),
        ];
        for (cwd, names, why) in cases {
            let f = with_home("/home/choi");
            let s = session(Arc::clone(&f));
            let names: Vec<OsString> = names.into_iter().map(OsString::from).collect();
            assert!(delete(&s, &p(cwd), &names).is_err(), "{why}");
            assert!(only_rm(&f).is_empty(), "{why}: rm must not run");
        }
    }

    #[test]
    fn delete_is_refused_when_home_cannot_be_determined() {
        for out in [fail(1, "boom\n"), ok(""), ok("\0"), ok("relative\0")] {
            let o = out.clone();
            let f = FakeCmd::new(move |a| {
                if crate::testutil::remote_cmd(a).is_some_and(|c| c.contains("$HOME")) {
                    o.clone()
                } else {
                    ok("")
                }
            });
            let s = session(Arc::clone(&f));
            assert!(delete(&s, &p("/srv"), &[OsString::from("a")]).is_err());
            assert!(only_rm(&f).is_empty());
        }
    }

    #[test]
    fn delete_reports_rm_failure() {
        let f = FakeCmd::new(|a| {
            let c = crate::testutil::remote_cmd(a).unwrap_or("");
            if c.contains("$HOME") {
                ok("/home/choi\0")
            } else {
                fail(1, "rm: cannot remove '/srv/a': Permission denied\n")
            }
        });
        let s = session(f);
        let e = delete(&s, &p("/srv"), &[OsString::from("a")]).unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
    }
}
