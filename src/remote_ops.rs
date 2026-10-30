//! File operations on a remote host over a [`RemoteSession`]: mkdir, rename,
//! delete, preview reads, and transfers for editing. Everything here blocks;
//! callers run it on a worker thread. Command construction lives in
//! [`crate::remote`]; these functions add validation and result mapping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::{os_bytes, os_from_bytes};
use crate::preview::{self, Limits, Preview};
use crate::remote::{
    DELETE_NO_CWD, DELETE_NO_HOME, DELETE_PROTECTED, PREVIEW_BROKEN, PREVIEW_DIR, PREVIEW_MISSING,
    PREVIEW_SPECIAL, RENAME_EXISTS, REPLACE_BACKUP_KEPT, REPLACE_CHANGED, REPLACE_GROUP,
    REPLACE_NOT_OWNER, REPLACE_RESTORED, REPLACE_SIZE, delete_cmd, mkdir_cmd, mktemp_cmd,
    preview_cmd, rename_cmd, replace_cmd, resolve_cmd, rm_file_cmd, rmdir_cmd, stamp_cmd,
};
use crate::remote::{Endpoint, scp_argv};
use crate::session::{CmdOutput, RemoteSession, cd_cmd, first_line};

/// Resolve `target` (as typed after `cd`) against `cwd` on the host.
pub fn resolve_cd(s: &RemoteSession, cwd: &Path, target: &str) -> Result<PathBuf, String> {
    let out = run_ok(s, &cd_cmd(&os_bytes(cwd.as_os_str()), target))?;
    let end = out.stdout.iter().position(|&b| b == 0).unwrap_or(0);
    if end == 0 || out.stdout[0] != b'/' {
        return Err("원격 경로를 알 수 없음".into());
    }
    Ok(PathBuf::from(os_from_bytes(out.stdout[..end].to_vec())))
}

/// Text shown for a FIFO, socket or device (same as the local Enter message).
pub const SPECIAL_FILE: &str = "특수 파일은 열 수 없음";

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

/// `cwd` as bytes and `names` as bytes, refusing anything that is not a
/// plain child name (empty, `.`, `..`, containing `/`) or a relative `cwd`.
fn delete_args(cwd: &Path, names: &[OsString]) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
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
        out.push(n);
    }
    Ok((base, out))
}

/// Permanently delete `names` in `cwd` with `rm -rf`. One remote script
/// resolves the home, `cwd` and every target physically and refuses (before
/// any `rm`) to touch `/`, the remote home or an ancestor of it, even when
/// reached through symlinks (see [`delete_cmd`]).
pub fn delete(s: &RemoteSession, cwd: &Path, names: &[OsString]) -> Result<(), String> {
    let (base, names) = delete_args(cwd, names)?;
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    let out = s.exec(&delete_cmd(&base, &refs)).map_err(|e| e.message())?;
    match out.code {
        Some(0) => Ok(()),
        Some(DELETE_PROTECTED) => {
            Err("삭제 거부: 원격 홈 디렉터리(또는 상위)는 지울 수 없음".into())
        }
        Some(DELETE_NO_HOME) => Err("삭제 거부: 원격 홈 디렉터리를 알 수 없음".into()),
        Some(DELETE_NO_CWD) => Err("삭제 거부: 원격 현재 경로를 확인할 수 없음".into()),
        c => Err(first_line(
            &out.stderr,
            &format!("종료 코드 {}", c.unwrap_or(-1)),
        )),
    }
}

/// Read the head of a remote file (or summarize a directory) like the local
/// preview does: at most `limits.max_bytes` (+3 so a split character can be
/// completed) cross the wire, then [`preview::classify_with`] decides.
pub fn preview(s: &RemoteSession, path: &Path, limits: Limits) -> Preview {
    let bytes = os_bytes(path.as_os_str());
    let out = match s.exec(&preview_cmd(&s.os, &bytes, limits.max_bytes + 3)) {
        Ok(o) => o,
        Err(e) => return Preview::Error(e.message()),
    };
    match out.code {
        Some(0) => {}
        Some(PREVIEW_DIR) => return remote_dir_info(s, path),
        Some(PREVIEW_SPECIAL) => {
            return Preview::Binary {
                size: 0,
                kind: "특수 파일",
            };
        }
        Some(PREVIEW_MISSING) => return Preview::Error("파일 없음".into()),
        Some(PREVIEW_BROKEN) => return Preview::Error("깨진 링크".into()),
        c => {
            return Preview::Error(first_line(
                &out.stderr,
                &format!("종료 코드 {}", c.unwrap_or(-1)),
            ));
        }
    }
    let nl = out.stdout.iter().position(|&b| b == b'\n');
    let size = nl
        .and_then(|i| std::str::from_utf8(&out.stdout[..i]).ok())
        .and_then(|t| t.trim().parse::<u64>().ok());
    let (Some(nl), Some(size)) = (nl, size) else {
        return Preview::Error("원격 응답을 해석할 수 없음".into());
    };
    preview::classify_with(&out.stdout[nl + 1..], size, limits)
}

fn remote_dir_info(s: &RemoteSession, path: &Path) -> Preview {
    match s.list(path, true) {
        // The listing starts with "..", which is not an entry of the directory.
        Ok((entries, _)) => {
            let n = entries.iter().filter(|e| e.name != "..").count();
            Preview::Info(format!("원격 디렉터리 · 항목 {n}개"))
        }
        Err(e) => Preview::Error(e),
    }
}

/// Is the remote file text (first 8 KiB)? Mirrors the local probe: special
/// files and errors are `Err`.
pub fn probe_text(s: &RemoteSession, path: &Path) -> Result<bool, String> {
    let limits = Limits {
        max_bytes: 8 * 1024,
        ..Limits::PREVIEW
    };
    match preview(s, path, limits) {
        Preview::Text { .. } | Preview::Empty => Ok(true),
        Preview::Binary {
            kind: "특수 파일",
        ..
        } => Err(SPECIAL_FILE.into()),
        Preview::Binary { .. } => Ok(false),
        Preview::Info(_) => Err("디렉터리".into()),
        Preview::Dir { .. } => Err("디렉터리".into()),
        Preview::Error(e) => Err(e),
    }
}

/// Content fingerprint of a local file, to tell whether an editor changed it.
/// Always the content hash: length and mtime can be restored by an editor or
/// a tool, so they never decide "unchanged" (an editor that re-saves identical
/// bytes is still not a change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    hash: u64,
}

fn hash_file(path: &Path) -> std::io::Result<u64> {
    use std::hash::Hasher;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = std::hash::DefaultHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.write(&buf[..n]);
    }
}

impl FileStamp {
    pub fn of(path: &Path) -> std::io::Result<FileStamp> {
        Ok(FileStamp {
            len: std::fs::metadata(path)?.len(),
            hash: hash_file(path)?,
        })
    }

    /// Did the content of `path` change since this stamp? Hashes the whole
    /// file (callers run this off the UI thread). A vanished file is an
    /// error.
    pub fn differs(&self, path: &Path) -> std::io::Result<bool> {
        let len = std::fs::metadata(path)?.len();
        Ok(len != self.len || hash_file(path)? != self.hash)
    }
}

/// A private local copy of a remote file.
#[derive(Debug)]
pub struct EditCopy {
    /// Owner-only (0700) temp directory holding the copy; removing it
    /// removes the copy.
    pub dir: tempfile::TempDir,
    pub local: PathBuf,
    pub stamp: FileStamp,
    /// [`remote_stamp`] of the remote file, taken before downloading it
    /// (empty unless fetched for editing).
    pub remote: Vec<u8>,
}

fn scp_run(s: &RemoteSession, argv: Result<Vec<OsString>, String>) -> Result<(), String> {
    let out = s.scp(&argv?).map_err(|e| e.message())?;
    if out.code == Some(0) {
        return Ok(());
    }
    Err(first_line(
        &out.stderr,
        &format!("scp 종료 코드 {}", out.code.unwrap_or(-1)),
    ))
}

/// Fingerprint of the remote file `path` (see [`stamp_cmd`]).
pub fn remote_stamp(s: &RemoteSession, path: &Path) -> Result<Vec<u8>, String> {
    let out = run_ok(s, &stamp_cmd(&s.os, &os_bytes(path.as_os_str())))?;
    if out.stdout.is_empty() {
        return Err("원격 파일 상태를 알 수 없음".into());
    }
    Ok(out.stdout)
}

/// Download `remote` into a fresh private temp dir (same file name, so
/// extensions keep working) and fingerprint it. `for_edit`: the remote file
/// is stamped first (so a change during the download also counts as a
/// change); a copy that is only opened skips that full read.
pub fn fetch(s: &RemoteSession, remote: &Path, for_edit: bool) -> Result<EditCopy, String> {
    let name = remote.file_name().ok_or("잘못된 경로")?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("hdir-remote-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-only, whatever the umask: the copy may be confidential.
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder
        .tempdir()
        .map_err(|e| format!("임시 폴더 생성 실패: {e}"))?;
    let remote_st = if for_edit {
        remote_stamp(s, remote)?
    } else {
        Vec::new()
    };
    let argv = scp_argv(
        &Endpoint::Remote(&s.target, &os_bytes(remote.as_os_str())),
        &Endpoint::Local(dir.path()),
        s.scp_mode,
    );
    scp_run(s, argv)?;
    let local = dir.path().join(name);
    let stamp = FileStamp::of(&local).map_err(|e| format!("내려받은 파일을 읽을 수 없음: {e}"))?;
    Ok(EditCopy {
        dir,
        local,
        stamp,
        remote: remote_st,
    })
}

/// A NUL-terminated absolute path printed by a remote command.
fn nul_path(out: &CmdOutput) -> Result<Vec<u8>, String> {
    let end = out.stdout.iter().position(|&b| b == 0).unwrap_or(0);
    if end == 0 || out.stdout[0] != b'/' {
        return Err("원격 경로를 알 수 없음".into());
    }
    Ok(out.stdout[..end].to_vec())
}

/// Upload `local` over `remote` without ever truncating it and keeping its
/// mode:
/// 1. resolve `remote` physically (a symlink is followed, so the link itself
///    survives and the temp lands next to the real file);
/// 2. `mktemp` creates an exclusive mode-600 `.hdir-edit.XXXXXX` there;
/// 3. scp onto that exact path;
/// 4. [`replace_cmd`] checks the original still matches `expect` (its
///    [`remote_stamp`] when it was downloaded), verifies the size, applies
///    the original's group and mode and
///    `mv`s it over (hard-linked files are rewritten in place instead).
///
/// On failure the remote temp file is removed and the original is untouched
/// (a failed in-place write of a hard-linked file is restored from a backup),
/// except when the original belongs to another user or restoring failed:
/// then the temp file (and the backup) stay, named in the error.
pub fn upload_replace(
    s: &RemoteSession,
    local: &Path,
    remote: &Path,
    expect: &[u8],
) -> Result<(), String> {
    let size = std::fs::metadata(local)
        .map_err(|e| format!("편집한 파일을 읽을 수 없음: {e}"))?
        .len();
    let phys = nul_path(&run_ok(s, &resolve_cmd(&os_bytes(remote.as_os_str())))?)?;
    let dir = phys
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(&b"/"[..], |i| if i == 0 { &b"/"[..] } else { &phys[..i] });
    let tmp_b = nul_path(&run_ok(s, &mktemp_cmd(dir))?)?;
    let argv = scp_argv(
        &Endpoint::Local(local),
        &Endpoint::Remote(&s.target, &tmp_b),
        s.scp_mode,
    );
    let tmp_s = String::from_utf8_lossy(&tmp_b).into_owned();
    // Errors that leave the remote temp file in place on purpose.
    let mut keep_tmp = false;
    let res = scp_run(s, argv).and_then(|()| {
        let out = s
            .exec(&replace_cmd(&s.os, &tmp_b, &phys, size, expect))
            .map_err(|e| e.message())?;
        match out.code {
            Some(0) => Ok(()),
            Some(REPLACE_NOT_OWNER) => {
                keep_tmp = true;
                Err(format!(
                    "원격 파일의 소유자가 달라 교체하지 않음 (원격 임시 파일: {tmp_s})"
                ))
            }
            Some(REPLACE_SIZE) => Err("업로드된 파일 크기가 맞지 않음".into()),
            Some(REPLACE_CHANGED) => Err("편집하는 동안 원격 파일이 바뀌어 교체하지 않음".into()),
            Some(REPLACE_GROUP) => Err("원격 파일의 그룹을 유지할 수 없어 교체하지 않음".into()),
            Some(REPLACE_RESTORED) => Err("원격 파일 쓰기 실패, 원본을 복원함".into()),
            Some(REPLACE_BACKUP_KEPT) => {
                keep_tmp = true;
                let bak = out.stdout.split(|&b| b == 0).next().unwrap_or_default();
                Err(format!(
                    "원격 파일 쓰기와 복원 모두 실패 (원본 백업: {}, 원격 임시 파일: {tmp_s})",
                    String::from_utf8_lossy(bak)
                ))
            }
            c => Err(first_line(
                &out.stderr,
                &format!("종료 코드 {}", c.unwrap_or(-1)),
            )),
        }
    });
    if res.is_err() && !keep_tmp {
        // Best effort: the session may be gone.
        let _ = s.exec(&rm_file_cmd(&tmp_b));
    }
    res
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

    #[test]
    fn delete_sends_one_script_with_quoted_names() {
        let f = FakeCmd::new(|_| ok(""));
        let s = session(Arc::clone(&f));
        let names: Vec<OsString> = ["a b", "it's", "-r", "x\ny", "한글"]
            .map(OsString::from)
            .to_vec();
        delete(&s, &p("/srv/data"), &names).unwrap();
        let refs: Vec<&[u8]> = vec![
            "a b".as_bytes(),
            "it's".as_bytes(),
            "-r".as_bytes(),
            "x\ny".as_bytes(),
            "한글".as_bytes(),
        ];
        let want = String::from_utf8(crate::remote::delete_cmd(b"/srv/data", &refs)).unwrap();
        assert_eq!(f.remote_cmds(), vec![want]);
    }

    #[test]
    fn delete_rejects_bad_arguments_without_running_anything() {
        let cases: Vec<(&str, Vec<&str>, &str)> = vec![
            ("/srv", vec![""], "empty name"),
            ("/srv", vec!["."], "dot"),
            ("/srv", vec![".."], "dotdot"),
            ("/srv", vec!["a/b"], "slash in name"),
            ("/srv", vec!["a", ".."], "dotdot among others"),
            ("/srv", vec![], "nothing selected"),
            ("srv", vec!["a"], "relative cwd"),
            ("", vec!["a"], "empty cwd"),
        ];
        for (cwd, names, why) in cases {
            let f = FakeCmd::new(|_| ok(""));
            let s = session(Arc::clone(&f));
            let names: Vec<OsString> = names.into_iter().map(OsString::from).collect();
            assert!(delete(&s, &p(cwd), &names).is_err(), "{why}");
            assert!(f.remote_cmds().is_empty(), "{why}: nothing may run");
        }
    }

    #[test]
    fn delete_maps_the_scripts_refusal_codes() {
        for (c, want) in [
            (42, "홈 디렉터리(또는 상위)"),
            (40, "홈 디렉터리를 알 수 없음"),
            (41, "현재 경로를 확인할 수 없음"),
        ] {
            let s = session(FakeCmd::new(move |_| code(c)));
            let e = delete(&s, &p("/srv"), &[OsString::from("a")]).unwrap_err();
            assert!(e.starts_with("삭제 거부") && e.contains(want), "{c}: {e}");
        }
    }

    #[test]
    fn delete_reports_rm_failure() {
        let f = FakeCmd::new(|_| fail(1, "rm: cannot remove '/srv/a': Permission denied\n"));
        let e = delete(&session(f), &p("/srv"), &[OsString::from("a")]).unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
    }

    // The real script, run by a local sh against a real directory tree.

    use crate::testutil::local_session;
    use std::os::unix::fs::symlink;

    /// root/a/home/u (= $HOME, with a file), root/work/{link -> a/home,
    /// up -> a, homelink -> a/home/u, plain}.
    fn home_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        let home = root.join("a/home/u");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("keep.txt"), "x").unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(work.join("plain")).unwrap();
        symlink(root.join("a/home"), work.join("link")).unwrap();
        symlink(root.join("a"), work.join("up")).unwrap();
        symlink(&home, work.join("homelink")).unwrap();
        (t, home, work)
    }

    #[test]
    fn delete_script_refuses_the_home_and_its_parents_reached_through_symlinks() {
        let (_t, home, work) = home_tree();
        let s = local_session(&home);
        let one = |n: &str| vec![OsString::from(n)];
        // home itself / its parent, named through a symlinked cwd
        for (cwd, name) in [
            (work.join("link"), "u"),
            (work.join("up"), "home"),
            (work.join("up/home"), "u"),
        ] {
            let e = delete(&s, &cwd, &one(name)).unwrap_err();
            assert!(e.starts_with("삭제 거부"), "{cwd:?}/{name}: {e}");
            assert!(home.join("keep.txt").exists(), "{cwd:?}/{name}");
        }
        // ... including among other, harmless names
        let names = vec![OsString::from("plain"), OsString::from("home")];
        assert!(delete(&s, &work.join("up"), &names).is_err());
        assert!(work.join("plain").exists(), "nothing is removed on refusal");
        assert!(home.join("keep.txt").exists());
    }

    #[test]
    fn delete_script_removes_a_symlink_as_a_link_and_children_of_the_home() {
        let (_t, home, work) = home_tree();
        let s = local_session(&home);
        // links to the home / its parent are only unlinked
        let names: Vec<OsString> = ["link", "up", "homelink"].map(OsString::from).to_vec();
        delete(&s, &work, &names).expect("links are removed as links");
        assert!(!work.join("link").exists() && !work.join("up").exists());
        assert!(std::fs::symlink_metadata(work.join("homelink")).is_err());
        assert!(home.join("keep.txt").exists(), "link targets untouched");
        // a child of the home is fine
        delete(&s, &home, &[OsString::from("keep.txt")]).unwrap();
        assert!(!home.join("keep.txt").exists());
        assert!(home.exists());
    }

    #[test]
    fn delete_script_refuses_without_a_resolvable_home_or_cwd() {
        let (_t, home, work) = home_tree();
        for bad_home in [PathBuf::new(), home.join("nope")] {
            let s = local_session(&bad_home);
            assert!(delete(&s, &work, &[OsString::from("plain")]).is_err());
            assert!(work.join("plain").exists(), "{bad_home:?}");
        }
        let s = local_session(&home);
        let e = delete(&s, &work.join("missing"), &[OsString::from("x")]).unwrap_err();
        assert!(e.starts_with("삭제 거부"), "{e}");
    }

    #[test]
    fn delete_script_handles_odd_names_and_the_root_directory() {
        let (_t, home, work) = home_tree();
        let s = local_session(&home);
        let odd = ["a b", "it's", "-r", "x\ny", "한글", "*"];
        for n in odd {
            std::fs::write(work.join(n), "1").unwrap();
        }
        std::fs::write(work.join("survivor"), "1").unwrap();
        let names: Vec<OsString> = odd.map(OsString::from).to_vec();
        delete(&s, &work, &names).unwrap();
        for n in odd {
            assert!(std::fs::symlink_metadata(work.join(n)).is_err(), "{n:?}");
        }
        assert!(work.join("survivor").exists(), "glob name did not expand");
        // "/" as cwd: the target is /name, never "//" or "/" itself
        let e = delete(
            &s,
            Path::new("/"),
            &[OsString::from("definitely-not-here-zz")],
        );
        assert!(e.is_ok(), "{e:?}");
    }

    /// work/a and work/"a\n", each holding `f` (the trailing newline must not
    /// be lost by `$(pwd -P)`).
    fn newline_twins() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(t.path()).unwrap().join("work");
        for d in ["a", "a\n"] {
            std::fs::create_dir_all(work.join(d)).unwrap();
            std::fs::write(work.join(d).join("f"), "keep").unwrap();
        }
        (t, work)
    }

    #[test]
    fn delete_script_keeps_a_trailing_newline_of_the_cwd() {
        let (_t, work) = newline_twins();
        let s = local_session(&work);
        delete(&s, &work.join("a\n"), &[OsString::from("f")]).unwrap();
        assert!(!work.join("a\n/f").exists(), "the named file is removed");
        assert!(
            work.join("a/f").exists(),
            "its twin without newline is kept"
        );
    }

    #[test]
    fn delete_script_refuses_a_home_reached_by_a_name_ending_in_newline() {
        let (_t, work) = newline_twins();
        // $HOME = work/"a\n": deleting work/"a\n" is refused, work/a is not
        // the home and may go
        let s = local_session(&work.join("a\n"));
        assert!(delete(&s, &work, &[OsString::from("a\n")]).is_err());
        assert!(work.join("a\n/f").exists());
        delete(&s, &work, &[OsString::from("a")]).unwrap();
        assert!(!work.join("a").exists());
    }

    // ---- HDIR-39: preview / probe ----

    use crate::preview::{Limits, Preview};

    /// Answers a preview command with `size\nbody`.
    fn serving(size: u64, body: &'static [u8]) -> Arc<FakeCmd> {
        FakeCmd::new(move |_| {
            let mut o = crate::session::CmdOutput {
                code: Some(0),
                ..Default::default()
            };
            o.stdout = format!("{size}\n").into_bytes();
            o.stdout.extend_from_slice(body);
            o
        })
    }

    #[test]
    fn preview_reads_head_and_classifies_text() {
        let f = serving(12, b"hello\nworld\n");
        let s = session(Arc::clone(&f));
        let got = preview(&s, &p("/srv/a b.txt"), Limits::PREVIEW);
        assert_eq!(
            got,
            Preview::Text {
                lines: vec!["hello".into(), "world".into()],
                truncated: false
            }
        );
        let want = crate::remote::preview_cmd(
            &crate::remote::RemoteOs::Linux,
            b"/srv/a b.txt",
            Limits::PREVIEW.max_bytes + 3,
        );
        assert_eq!(f.remote_cmds(), vec![String::from_utf8(want).unwrap()]);
    }

    #[test]
    fn preview_of_a_bigger_file_is_marked_truncated_and_viewer_reads_4_mib() {
        let f = serving(9_000_000, b"line\n");
        let s = session(Arc::clone(&f));
        match preview(&s, &p("/srv/big"), Limits::VIEWER) {
            Preview::Text { truncated, .. } => assert!(truncated),
            other => panic!("{other:?}"),
        }
        assert!(f.remote_cmds()[0].contains("head -c 4194307 -- '/srv/big'"));
    }

    #[test]
    fn preview_binary_and_empty() {
        let f = serving(4, b"\x00\x01\x02\x03");
        assert!(matches!(
            preview(&session(f), &p("/b"), Limits::PREVIEW),
            Preview::Binary { size: 4, .. }
        ));
        let f = serving(0, b"");
        assert_eq!(
            preview(&session(f), &p("/e"), Limits::PREVIEW),
            Preview::Empty
        );
    }

    #[test]
    fn preview_special_missing_broken_and_failures() {
        let by = |c: i32, err: &'static str| {
            let f = FakeCmd::new(move |_| fail(c, err));
            preview(&session(f), &p("/x"), Limits::PREVIEW)
        };
        assert_eq!(
            by(21, ""),
            Preview::Binary {
                size: 0,
                kind: "특수 파일"
            }
        );
        assert_eq!(by(22, ""), Preview::Error("파일 없음".into()));
        assert_eq!(by(23, ""), Preview::Error("깨진 링크".into()));
        assert_eq!(
            by(1, "wc: /x: Permission denied\n"),
            Preview::Error("wc: /x: Permission denied".into())
        );
        assert_eq!(
            by(255, ""),
            Preview::Error(crate::session::DISCONNECTED_MSG.into())
        );
    }

    #[test]
    fn preview_of_a_remote_dir_counts_entries() {
        let f = FakeCmd::new(|a| {
            let c = crate::testutil::remote_cmd(a).unwrap_or("");
            if c.starts_with("if [ -d") {
                code(20)
            } else {
                ok("f\t5\t1700000000.0\t644\t\ta\0d\t5\t1700000000.0\t755\t\tb\0")
            }
        });
        assert_eq!(
            preview(&session(f), &p("/srv/d"), Limits::PREVIEW),
            Preview::Info("원격 디렉터리 · 항목 2개".into())
        );
    }

    #[test]
    fn probe_text_follows_the_first_chunk() {
        assert_eq!(probe_text(&session(serving(3, b"abc")), &p("/t")), Ok(true));
        assert_eq!(probe_text(&session(serving(0, b"")), &p("/t")), Ok(true));
        assert_eq!(
            probe_text(&session(serving(2, b"\x00\x00")), &p("/t")),
            Ok(false)
        );
        let sp = FakeCmd::new(|_| code(21));
        assert_eq!(
            probe_text(&session(sp), &p("/t")),
            Err("특수 파일은 열 수 없음".into())
        );
        let missing = FakeCmd::new(|_| code(22));
        assert!(probe_text(&session(missing), &p("/t")).is_err());
        let f = serving(3, b"abc");
        let s = session(Arc::clone(&f));
        let _ = probe_text(&s, &p("/t"));
        assert!(f.remote_cmds()[0].contains("head -c 8195 -- '/t'"));
    }

    // ---- HDIR-40: download / upload / stamp ----

    use crate::remote::ScpMode;
    use crate::testutil::session_mode;
    use std::os::unix::fs::PermissionsExt;

    type Uploads = Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// What the fake answers to a stamp command.
    const STAMP: &[u8] = b"7 99 3 1234 3";

    /// Inverse of `sh_quote` for a single quoted word.
    fn unquote(q: &str) -> String {
        q.strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .unwrap()
            .replace("'\\''", "'")
    }

    /// An ssh/scp fake: downloads create `<dest dir>/<basename>` with
    /// `body`; uploads (destination starts with the host) are recorded with
    /// the local file's content at that moment. The resolve script answers
    /// `phys` (default: the path asked for), mktemp answers its template with
    /// `Ab12Cd`, and the replace script answers `mv_result`.
    fn scp_fake_at(
        body: &'static str,
        up_result: crate::session::CmdOutput,
        mv_result: crate::session::CmdOutput,
        phys: Option<&'static str>,
    ) -> (Arc<FakeCmd>, Uploads) {
        let uploads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let up = Arc::clone(&uploads);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                let (src, dst) = (&a[a.len() - 2], &a[a.len() - 1]);
                if dst.starts_with("prod-web:") {
                    let content = std::fs::read_to_string(src).unwrap_or_default();
                    up.lock().unwrap().push((dst.clone(), content));
                    return up_result.clone();
                }
                let name = src.rsplit('/').next().unwrap().trim_matches('\'');
                let name = name.replace("'\\''", "'");
                std::fs::write(std::path::Path::new(dst).join(name), body).unwrap();
                return ok("");
            }
            let c = crate::testutil::remote_cmd(a).unwrap_or("");
            if let Some(rest) = c.strip_prefix("p=") {
                let q = rest.split_once("; n=0\n").unwrap().0;
                return ok(&format!(
                    "{}\0",
                    phys.map_or_else(|| unquote(q), String::from)
                ));
            }
            if c.starts_with("f=") {
                return ok(std::str::from_utf8(STAMP).unwrap());
            }
            if let Some(rest) = c.strip_prefix("t=$(mktemp ") {
                let q = rest.split_once(") || exit 1").unwrap().0;
                return ok(&format!("{}\0", unquote(q).replace("XXXXXX", "Ab12Cd")));
            }
            if c.starts_with("t='") {
                return mv_result.clone();
            }
            ok("")
        });
        (f, uploads)
    }

    fn scp_fake(
        body: &'static str,
        up_result: crate::session::CmdOutput,
        mv_result: crate::session::CmdOutput,
    ) -> (Arc<FakeCmd>, Uploads) {
        scp_fake_at(body, up_result, mv_result, None)
    }

    #[test]
    fn fetch_downloads_into_a_private_temp_dir_and_stamps_it() {
        let (f, _) = scp_fake("orig\n", ok(""), ok(""));
        let s = session(Arc::clone(&f));
        let copy = fetch(&s, &p("/srv/it's a.txt"), true).unwrap();
        assert_eq!(copy.local, copy.dir.path().join("it's a.txt"));
        assert_eq!(std::fs::read_to_string(&copy.local).unwrap(), "orig\n");
        let mode = copy.dir.path().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let calls = f.scp_calls();
        assert_eq!(calls.len(), 1);
        let a = &calls[0];
        assert!(a.contains(&"BatchMode=yes".to_string()));
        assert_eq!(a[a.len() - 2], "prod-web:/srv/it's a.txt");
        assert_eq!(a[a.len() - 1], copy.dir.path().to_string_lossy());
        assert!(!copy.stamp.differs(&copy.local).unwrap());
    }

    #[test]
    fn fetch_in_legacy_scp_mode_quotes_the_remote_path() {
        let (f, _) = scp_fake("x", ok(""), ok(""));
        let s = session_mode(Arc::clone(&f), ScpMode::Legacy);
        fetch(&s, &p("/srv/it's a.txt"), true).unwrap();
        let a = &f.scp_calls()[0];
        assert_eq!(a[a.len() - 2], "prod-web:'/srv/it'\\''s a.txt'");
    }

    #[test]
    fn fetch_failures_are_errors() {
        let f = FakeCmd::new(|_| fail(1, "scp: /srv/x: No such file or directory\n"));
        let e = fetch(&session(f), &p("/srv/x"), true).unwrap_err();
        assert!(e.contains("No such file"), "{e}");
        let f = FakeCmd::new(|_| ok("")); // "succeeds" but nothing arrived
        assert!(fetch(&session(f), &p("/srv/x"), true).is_err());
        let f = FakeCmd::new(|_| code(255));
        let s = session(f);
        assert_eq!(
            fetch(&s, &p("/srv/x"), true).unwrap_err(),
            crate::session::DISCONNECTED_MSG
        );
        assert!(!s.is_alive());
    }

    #[test]
    fn fetch_stamps_the_remote_file_before_downloading_it() {
        let (f, _) = scp_fake("orig", ok(""), ok(""));
        let copy = fetch(&session(Arc::clone(&f)), &p("/srv/a"), true).unwrap();
        assert_eq!(copy.remote, STAMP);
        let log = f.log.lock().unwrap().clone();
        let stamp_at = log
            .iter()
            .position(|a| crate::testutil::remote_cmd(a).is_some_and(|c| c.starts_with("f=")))
            .unwrap();
        assert!(stamp_at < log.iter().position(|a| a[0] == "scp").unwrap());
        // no stamp, no download
        for answer in [fail(1, "stat: cannot stat\n"), ok("")] {
            let f = FakeCmd::new(move |_| answer.clone());
            assert!(fetch(&session(Arc::clone(&f)), &p("/srv/a"), true).is_err());
            assert!(f.scp_calls().is_empty());
        }
    }

    #[test]
    fn stamp_ignores_rewrites_of_identical_bytes() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, "abc").unwrap();
        let st = FileStamp::of(&f).unwrap();
        assert!(!st.differs(&f).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "abc").unwrap(); // newer mtime, same content
        assert!(!st.differs(&f).unwrap(), "touched but unchanged");
        std::fs::write(&f, "abd").unwrap(); // same length, other bytes
        assert!(st.differs(&f).unwrap());
        std::fs::write(&f, "abc").unwrap();
        assert!(!st.differs(&f).unwrap());
        std::fs::write(&f, "abcd").unwrap();
        assert!(st.differs(&f).unwrap());
        std::fs::remove_file(&f).unwrap();
        assert!(st.differs(&f).is_err());
    }

    #[test]
    fn stamp_sees_same_length_changes_even_when_the_mtime_is_restored() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, "abc").unwrap();
        let old = std::fs::metadata(&f).unwrap().modified().unwrap();
        let st = FileStamp::of(&f).unwrap();
        std::fs::write(&f, "xyz").unwrap(); // same length, other bytes
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap(); // ... and the old mtime put back
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), old);
        assert!(st.differs(&f).unwrap(), "content decides, not size+mtime");
    }

    #[test]
    fn upload_resolves_creates_an_exclusive_temp_then_scps_onto_it_and_replaces() {
        let (f, ups) = scp_fake("", ok(""), ok(""));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a b.txt");
        std::fs::write(&local, "new content").unwrap();
        upload_replace(&s, &local, &p("/srv/it's/a b.txt"), STAMP).unwrap();
        let ups = ups.lock().unwrap().clone();
        assert_eq!(
            ups,
            vec![(
                "prod-web:/srv/it's/.hdir-edit.Ab12Cd".to_string(),
                "new content".to_string()
            )],
            "scp goes onto the exact path mktemp returned"
        );
        use crate::remote::{mktemp_cmd, replace_cmd, resolve_cmd};
        let u = |v: Vec<u8>| String::from_utf8(v).unwrap();
        assert_eq!(
            f.remote_cmds(),
            vec![
                u(resolve_cmd(b"/srv/it's/a b.txt")),
                u(mktemp_cmd(b"/srv/it's")),
                u(replace_cmd(
                    &crate::remote::RemoteOs::Linux,
                    b"/srv/it's/.hdir-edit.Ab12Cd",
                    b"/srv/it's/a b.txt",
                    11,
                    STAMP,
                )),
            ]
        );
    }

    #[test]
    fn upload_through_a_symlink_puts_the_temp_next_to_the_real_file_and_replaces_that() {
        let (f, ups) = scp_fake_at("", ok(""), ok(""), Some("/data/real/target.txt"));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("l");
        std::fs::write(&local, "abc").unwrap();
        upload_replace(&s, &local, &p("/srv/link"), STAMP).unwrap();
        assert_eq!(
            ups.lock().unwrap()[0].0,
            "prod-web:/data/real/.hdir-edit.Ab12Cd"
        );
        let cmds = f.remote_cmds();
        assert!(
            cmds[1].contains("'/data/real/.hdir-edit.XXXXXX'"),
            "{}",
            cmds[1]
        );
        assert!(
            cmds[2].starts_with("t='/data/real/.hdir-edit.Ab12Cd'; o='/data/real/target.txt'"),
            "{}",
            cmds[2]
        );
        assert!(
            cmds.iter()
                .all(|c| !c.contains("cat --") || c.contains("-gt 1"))
        );
    }

    #[test]
    fn upload_into_the_root_directory_uses_a_root_template() {
        let (f, _) = scp_fake_at("", ok(""), ok(""), Some("/top.txt"));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("l");
        std::fs::write(&local, "abc").unwrap();
        upload_replace(&session(Arc::clone(&f)), &local, &p("/top.txt"), STAMP).unwrap();
        assert!(f.remote_cmds()[1].contains("'/.hdir-edit.XXXXXX'"));
    }

    #[test]
    fn upload_failures_clean_up_the_remote_temp_file_and_never_touch_the_original() {
        let rm = "rm -f -- '/srv/.hdir-edit.Ab12Cd'";
        let (f, _) = scp_fake(
            "",
            fail(1, "scp: write failed: No space left on device\n"),
            ok(""),
        );
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a");
        std::fs::write(&local, "x").unwrap();
        let e = upload_replace(&s, &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("No space left"), "{e}");
        let cmds = f.remote_cmds();
        assert_eq!(cmds.len(), 3, "resolve, mktemp, cleanup: no replace");
        assert_eq!(cmds[2], rm);

        let (f, _) = scp_fake("", ok(""), fail(1, "mv: cannot move: Permission denied\n"));
        let e = upload_replace(&session(Arc::clone(&f)), &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
        let cmds = f.remote_cmds();
        assert_eq!(cmds.len(), 4);
        assert_eq!(cmds[3], rm);

        // size mismatch after the upload
        let (f, _) = scp_fake("", ok(""), code(crate::remote::REPLACE_SIZE));
        let e = upload_replace(&session(Arc::clone(&f)), &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("크기"), "{e}");
        assert_eq!(f.remote_cmds()[3], rm);
    }

    #[test]
    fn upload_stops_before_scp_when_resolve_or_mktemp_fails() {
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a");
        std::fs::write(&local, "x").unwrap();
        for (starts, err) in [
            ("p=", "not a regular file"),
            ("t=$(mktemp", "Permission denied"),
        ] {
            let f = FakeCmd::new(move |a| {
                if crate::testutil::remote_cmd(a).is_some_and(|c| c.starts_with(starts)) {
                    fail(1, &format!("{err}\n"))
                } else if crate::testutil::remote_cmd(a).is_some_and(|c| c.starts_with("p=")) {
                    ok("/srv/a\0")
                } else {
                    ok("")
                }
            });
            let e =
                upload_replace(&session(Arc::clone(&f)), &local, &p("/srv/a"), STAMP).unwrap_err();
            assert!(e.contains(err), "{e}");
            assert!(f.scp_calls().is_empty(), "{starts}");
            assert!(f.remote_cmds().iter().all(|c| !c.starts_with("rm ")));
        }
        // garbage answers are refused too
        let f = FakeCmd::new(|_| ok("relative\0"));
        assert!(upload_replace(&session(f), &local, &p("/srv/a"), STAMP).is_err());
    }

    #[test]
    fn upload_to_a_file_of_another_owner_replaces_nothing_and_keeps_the_temp() {
        let (f, _) = scp_fake("", ok(""), code(crate::remote::REPLACE_NOT_OWNER));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a");
        std::fs::write(&local, "x").unwrap();
        let e = upload_replace(&session(Arc::clone(&f)), &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(
            e.contains("소유자") && e.contains("/srv/.hdir-edit.Ab12Cd"),
            "{e}"
        );
        assert_eq!(
            f.remote_cmds().len(),
            3,
            "no rm of the kept temp: {:?}",
            f.remote_cmds()
        );
    }

    // The real scripts, run by a local sh on real files.

    /// Upload over a remote file that did not change since now.
    fn upload_fresh(s: &RemoteSession, local: &Path, remote: &Path) -> Result<(), String> {
        let st = remote_stamp(s, remote).unwrap_or_default();
        upload_replace(s, local, remote, &st)
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn edited(t: &Path, content: &str) -> PathBuf {
        let l = t.join("edited-local");
        std::fs::write(&l, content).unwrap();
        l
    }

    fn no_temp_left(dir: &Path) {
        let left: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with(".hdir-edit"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn real_upload_preserves_the_mode_of_the_original() {
        for mode in [0o640, 0o600, 0o755, 0o444] {
            let t = tempfile::tempdir().unwrap();
            let dir = std::fs::canonicalize(t.path()).unwrap();
            let target = dir.join("it's a file.txt");
            std::fs::write(&target, "old").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
            let local = edited(&dir, "brand new");
            let s = local_session(&dir);
            upload_fresh(&s, &local, &target).unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "brand new");
            assert_eq!(mode_of(&target), mode, "{mode:o}");
            no_temp_left(&dir);
        }
    }

    #[test]
    fn real_upload_through_symlinks_keeps_the_links_and_writes_the_target() {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(root.join("links")).unwrap();
        let target = root.join("real/target.txt");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        // links/l2 -> l1 (relative) -> ../real/target.txt (relative)
        symlink("../real/target.txt", root.join("links/l1")).unwrap();
        symlink("l1", root.join("links/l2")).unwrap();
        let local = edited(&root, "through the link");
        let s = local_session(&root);
        upload_fresh(&s, &local, &root.join("links/l2")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "through the link"
        );
        for l in ["links/l1", "links/l2"] {
            assert!(
                std::fs::symlink_metadata(root.join(l))
                    .unwrap()
                    .is_symlink(),
                "{l}"
            );
        }
        assert_eq!(
            std::fs::read_link(root.join("links/l1")).unwrap(),
            Path::new("../real/target.txt")
        );
        assert_eq!(mode_of(&target), 0o640);
        no_temp_left(&root.join("real"));
        no_temp_left(&root.join("links"));
        // a dangling link is an error, nothing is created
        symlink("gone", root.join("links/dangling")).unwrap();
        assert!(upload_fresh(&s, &local, &root.join("links/dangling")).is_err());
        assert!(!root.join("links/gone").exists());
        no_temp_left(&root.join("links"));
    }

    #[test]
    fn real_upload_keeps_the_inode_of_a_hard_linked_file() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, "shared").unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        let ino = std::fs::metadata(&a).unwrap().ino();
        let local = edited(&dir, "edited shared");
        upload_fresh(&local_session(&dir), &local, &a).unwrap();
        assert_eq!(std::fs::metadata(&a).unwrap().nlink(), 2, "link count kept");
        assert_eq!(std::fs::metadata(&a).unwrap().ino(), ino);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "edited shared");
        no_temp_left(&dir);
    }

    #[test]
    fn real_upload_into_a_directory_ending_in_newline_writes_only_that_file() {
        let (_t, work) = newline_twins();
        let local = edited(work.parent().unwrap(), "edited");
        let s = local_session(&work);
        upload_fresh(&s, &local, &work.join("a\n/f")).unwrap();
        assert_eq!(
            std::fs::read_to_string(work.join("a\n/f")).unwrap(),
            "edited"
        );
        assert_eq!(std::fs::read_to_string(work.join("a/f")).unwrap(), "keep");
        no_temp_left(&work.join("a"));
        no_temp_left(&work.join("a\n"));
    }

    #[test]
    fn real_edit_refuses_to_replace_a_file_changed_on_the_remote_meanwhile() {
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let target = dir.join("it's.txt");
        std::fs::write(&target, "v1").unwrap();
        let s = local_session(&dir);
        let copy = fetch(&s, &target, true).unwrap();
        assert_eq!(std::fs::read_to_string(&copy.local).unwrap(), "v1");
        // someone else saves the remote file while we edit
        std::fs::write(&target, "v2").unwrap();
        std::fs::write(&copy.local, "mine").unwrap();
        let e = upload_replace(&s, &copy.local, &target, &copy.remote).unwrap_err();
        assert!(e.contains("원격 파일이 바뀌어"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "v2",
            "theirs kept"
        );
        assert_eq!(std::fs::read_to_string(&copy.local).unwrap(), "mine");
        no_temp_left(&dir);
    }

    #[test]
    fn real_edit_sees_a_same_size_change_with_the_mtime_restored() {
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let target = dir.join("f");
        std::fs::write(&target, "aaaa").unwrap();
        let s = local_session(&dir);
        let before = remote_stamp(&s, &target).unwrap();
        let mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::fs::write(&target, "bbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(remote_stamp(&s, &target).unwrap(), before);
        // a hard-linked file is checked too, before its backup is made
        std::fs::hard_link(&target, dir.join("g")).unwrap();
        let stale = remote_stamp(&s, &target).unwrap();
        std::fs::write(&target, "cccc").unwrap();
        let local = edited(&dir, "mine");
        assert!(upload_replace(&s, &local, &target, &stale).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("g")).unwrap(), "cccc");
        assert!(backups(&dir).is_empty());
        no_temp_left(&dir);
    }

    #[test]
    fn real_edit_of_an_unchanged_remote_file_through_a_symlink_replaces_it() {
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        std::fs::write(dir.join("real"), "v1").unwrap();
        symlink("real", dir.join("link")).unwrap();
        let s = local_session(&dir);
        let copy = fetch(&s, &dir.join("link"), true).unwrap();
        std::fs::write(&copy.local, "mine").unwrap();
        upload_replace(&s, &copy.local, &dir.join("link"), &copy.remote).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("real")).unwrap(), "mine");
    }

    #[test]
    fn real_replace_rechecks_a_hard_linked_file_after_backing_it_up() {
        // someone saves the original while its backup is being copied
        let (_t, dir, cmd) = hard_linked_with_fakes(&[(
            "cp",
            "/bin/cp \"$@\" || exit 1; printf other > \"$(dirname \"$0\")/../d/a\"",
        )]);
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_CHANGED));
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "other");
        assert!(backups(&dir).is_empty(), "backup removed");
    }

    #[test]
    fn real_replace_rechecks_right_before_the_mv() {
        // someone saves the original while the temp gets its mode
        let (_t, dir, cmd) = replace_with_fakes(
            &[(
                "chmod",
                "/bin/chmod \"$@\" || exit 1; printf other > \"$(dirname \"$0\")/../d/a\"",
            )],
            false,
        );
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_CHANGED));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "other");
    }

    /// Fake `stat` reporting group 999999 for `stat -c|-f %g`, so the group
    /// branch runs whatever groups the account has.
    const STAT_OTHER_GID: &str =
        "if [ \"$2\" = %g ]; then echo 999999; else exec /usr/bin/stat \"$@\"; fi";

    #[test]
    fn replace_script_group_branch_without_a_second_group() {
        let (_t, dir, cmd) =
            replace_with_fakes(&[("stat", STAT_OTHER_GID), ("chgrp", "exit 1")], false);
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_GROUP));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "shared");

        let rec = "echo \"$@\" > \"$(dirname \"$0\")/chgrp-args\"";
        let (_t, dir, cmd) = replace_with_fakes(&[("stat", STAT_OTHER_GID), ("chgrp", rec)], false);
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(
            out.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let args = std::fs::read_to_string(dir.parent().unwrap().join("bin/chgrp-args")).unwrap();
        use std::os::unix::fs::MetadataExt;
        let gid = std::fs::metadata(&dir).unwrap().gid();
        assert_eq!(
            args.trim(),
            format!("{gid} {}", dir.join(".hdir-edit.Hl1").display()),
            "chgrp to the original's group"
        );
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "edited!");
    }

    #[test]
    fn real_stamp_refuses_a_fifo_without_reading_it() {
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(dir.join("pipe"))
            .status()
            .unwrap();
        assert!(st.success());
        symlink("pipe", dir.join("to-pipe")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let d2 = dir.clone();
        std::thread::spawn(move || {
            let s = local_session(&d2);
            let r = ["pipe", "to-pipe"].map(|n| remote_stamp(&s, &d2.join(n)).is_err());
            let _ = tx.send(r);
        });
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| {
                // let a blocked reader go before failing
                for _ in 0..2 {
                    let _ = std::fs::OpenOptions::new()
                        .write(true)
                        .open(dir.join("pipe"));
                }
                panic!("stamping a FIFO must not block")
            });
        assert_eq!(r, [true, true]);
    }

    #[test]
    fn fetch_for_opening_only_skips_the_stamp() {
        let (f, _) = scp_fake("orig", ok(""), ok(""));
        let copy = fetch(&session(Arc::clone(&f)), &p("/srv/a"), false).unwrap();
        assert!(copy.remote.is_empty());
        assert!(!f.remote_cmds().iter().any(|c| c.starts_with("f=")));
    }

    #[test]
    fn upload_reports_a_concurrent_remote_change() {
        let (f, _) = scp_fake("", ok(""), code(crate::remote::REPLACE_CHANGED));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let e = upload_replace(&s, &edited(t.path(), "x"), &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("원격 파일이 바뀌어"), "{e}");
        assert!(
            f.remote_cmds().last().unwrap().starts_with("rm -f -- "),
            "temp removed"
        );
    }

    #[test]
    fn real_replace_script_refuses_a_temp_of_the_wrong_size() {
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let target = dir.join("f");
        std::fs::write(&target, "old").unwrap();
        let inner = local_session(&dir);
        // run the replace script directly with a wrong size
        let tmp = dir.join(".hdir-edit.Zz9");
        std::fs::write(&tmp, "four").unwrap();
        let cmd = crate::remote::replace_cmd(
            &inner.os,
            tmp.as_os_str().as_encoded_bytes(),
            target.as_os_str().as_encoded_bytes(),
            5,
            b"",
        );
        let out = inner.exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_SIZE));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    /// `dir/a` hard-linked as `dir/b` ("shared"), a verified temp holding
    /// "edited!", and a `bin` dir of fake tools put first on PATH.
    fn hard_linked_with_fakes(fakes: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        replace_with_fakes(fakes, true)
    }

    /// Like [`hard_linked_with_fakes`], `dir/b` only when `link`.
    fn replace_with_fakes(
        fakes: &[(&str, &str)],
        link: bool,
    ) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        let t = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        let dir = root.join("d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "shared").unwrap();
        if link {
            std::fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        }
        std::fs::write(dir.join(".hdir-edit.Hl1"), "edited!").unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in fakes {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = format!("PATH='{}':\"$PATH\"\n", bin.display()).into_bytes();
        cmd.extend(crate::remote::replace_cmd(
            &crate::testutil::local_os(),
            dir.join(".hdir-edit.Hl1").as_os_str().as_encoded_bytes(),
            dir.join("a").as_os_str().as_encoded_bytes(),
            7,
            &remote_stamp(&local_session(&dir), &dir.join("a")).unwrap(),
        ));
        (t, dir, cmd)
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".hdir-bak")
            })
            .collect()
    }

    #[test]
    fn real_replace_restores_a_hard_linked_file_when_the_in_place_write_fails() {
        use std::os::unix::fs::MetadataExt;
        // cat truncates the target, writes a little, then fails
        let (_t, dir, cmd) = hard_linked_with_fakes(&[("cat", "printf par; exit 1")]);
        let ino = std::fs::metadata(dir.join("a")).unwrap().ino();
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_RESTORED));
        for f in ["a", "b"] {
            assert_eq!(
                std::fs::read_to_string(dir.join(f)).unwrap(),
                "shared",
                "{f}"
            );
        }
        assert_eq!(std::fs::metadata(dir.join("a")).unwrap().ino(), ino);
        assert_eq!(std::fs::metadata(dir.join("a")).unwrap().nlink(), 2);
        assert!(backups(&dir).is_empty(), "backup removed after restoring");
    }

    #[test]
    fn real_replace_keeps_the_backup_when_restoring_fails_too() {
        // the first cp (backup) works, the second (restore) fails
        let mark = "\"$(dirname \"$0\")/used\"";
        let cp = format!("[ -e {mark} ] && exit 1; : > {mark}; exec /bin/cp \"$@\"");
        let (_t, dir, cmd) = hard_linked_with_fakes(&[("cat", "exit 1"), ("cp", &cp)]);
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_BACKUP_KEPT));
        let kept = backups(&dir);
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "shared");
        let printed = &out.stdout[..out.stdout.iter().position(|&b| b == 0).unwrap()];
        assert_eq!(printed, kept[0].as_os_str().as_encoded_bytes());
        assert_eq!(mode_of(&kept[0]) & 0o077, 0, "backup is private");
        assert!(
            dir.join(".hdir-edit.Hl1").exists(),
            "the edited temp is kept too"
        );
    }

    #[test]
    fn real_replace_touches_nothing_when_the_backup_cannot_be_made() {
        let (_t, dir, cmd) = hard_linked_with_fakes(&[("cp", "exit 1")]);
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_ne!(out.code, Some(0));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "shared");
        assert!(backups(&dir).is_empty());
    }

    #[test]
    fn upload_reports_a_restored_or_a_kept_backup_hard_link_write() {
        let (f, _) = scp_fake("", ok(""), code(crate::remote::REPLACE_RESTORED));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = edited(t.path(), "new");
        let e = upload_replace(&s, &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("원본을 복원"), "{e}");
        assert!(
            f.remote_cmds().last().unwrap().starts_with("rm -f -- "),
            "temp removed"
        );

        let mut kept = code(crate::remote::REPLACE_BACKUP_KEPT);
        kept.stdout = b"/srv/.hdir-bak.Qq1\0".to_vec();
        let (f, _) = scp_fake("", ok(""), kept);
        let s = session(Arc::clone(&f));
        let e = upload_replace(&s, &local, &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("/srv/.hdir-bak.Qq1"), "{e}");
        assert!(e.contains("/srv/.hdir-edit.Ab12Cd"), "{e}");
        assert!(
            !f.remote_cmds().iter().any(|c| c.starts_with("rm -f -- ")),
            "nothing is cleaned up"
        );
    }

    /// A group of the current user other than `not`, if any.
    fn other_group(not: u32) -> Option<u32> {
        let out = std::process::Command::new("id").arg("-G").output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .find(|&g| g != not)
    }

    #[test]
    fn real_upload_keeps_the_group_of_the_original() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap();
        let target = dir.join("secret");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let dir_gid = std::fs::metadata(&dir).unwrap().gid();
        let Some(g) = other_group(dir_gid) else {
            eprintln!("skipped: the user belongs to a single group");
            return;
        };
        let st = std::process::Command::new("chgrp")
            .arg(g.to_string())
            .arg(&target)
            .status()
            .unwrap();
        assert!(st.success());
        let local = edited(&dir, "new secret");
        upload_fresh(&local_session(&dir), &local, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new secret");
        assert_eq!(std::fs::metadata(&target).unwrap().gid(), g, "group kept");
        assert_eq!(mode_of(&target), 0o640, "mode kept after chgrp");
        no_temp_left(&dir);
    }

    #[test]
    fn real_replace_refuses_when_the_group_cannot_be_kept() {
        use std::os::unix::fs::MetadataExt;
        // the fake chgrp fails as for a group the user is not in
        let (_t, dir, cmd) = replace_with_fakes(&[("chgrp", "exit 1")], false);
        let gid = std::fs::metadata(dir.join("a")).unwrap().gid();
        let Some(g) = other_group(gid) else {
            eprintln!("skipped: the user belongs to a single group");
            return;
        };
        let st = std::process::Command::new("chgrp")
            .arg(g.to_string())
            .arg(dir.join("a"))
            .status()
            .unwrap();
        assert!(st.success());
        let out = local_session(&dir).exec(&cmd).unwrap();
        assert_eq!(out.code, Some(crate::remote::REPLACE_GROUP));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "shared");
        assert_eq!(std::fs::metadata(dir.join("a")).unwrap().gid(), g);
    }

    #[test]
    fn upload_reports_a_group_that_cannot_be_kept() {
        let (f, _) = scp_fake("", ok(""), code(crate::remote::REPLACE_GROUP));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let e = upload_replace(&s, &edited(t.path(), "x"), &p("/srv/a"), STAMP).unwrap_err();
        assert!(e.contains("그룹"), "{e}");
        assert!(
            f.remote_cmds().last().unwrap().starts_with("rm -f -- "),
            "temp removed"
        );
    }

    #[test]
    fn replace_script_refuses_a_file_owned_by_someone_else() {
        // Never point a test at a system file: check the script's owner guard
        // instead. A foreign-owned target can't be created without root.
        let cmd = crate::remote::replace_cmd(
            &crate::remote::RemoteOs::Linux,
            b"/tmp/x/.hdir-edit.Own1",
            b"/tmp/x/f",
            1,
            b"",
        );
        let script = String::from_utf8_lossy(&cmd);
        assert!(script.contains("!= \"$(id -u)\""), "{script}");
        assert!(
            script.contains(&format!("exit {}", crate::remote::REPLACE_NOT_OWNER)),
            "{script}"
        );
    }

    // ---- HDIR-41: cd ----

    #[test]
    fn resolve_cd_returns_the_pwd_printed_by_the_remote_shell() {
        let f = FakeCmd::new(|_| ok("/srv/data/sub\nx\0trailing"));
        let s = session(Arc::clone(&f));
        assert_eq!(
            resolve_cd(&s, &p("/srv/data"), "sub").unwrap(),
            p("/srv/data/sub\nx")
        );
        assert_eq!(
            f.remote_cmds(),
            vec!["cd -- '/srv/data' && cd -- 'sub' && printf '%s\\0' \"$PWD\"".to_string()]
        );
    }

    #[test]
    fn resolve_cd_errors() {
        let f = FakeCmd::new(|_| fail(1, "sh: 1: cd: can't cd to nope\nmore\n"));
        let e = resolve_cd(&session(f), &p("/srv"), "nope").unwrap_err();
        assert_eq!(e, "sh: 1: cd: can't cd to nope");
        for out in [ok(""), ok("\0"), ok("relative\0"), ok("/no/nul")] {
            let o = out.clone();
            let f = FakeCmd::new(move |_| o.clone());
            assert!(resolve_cd(&session(f), &p("/srv"), "x").is_err());
        }
        let f = FakeCmd::new(|_| code(255));
        assert_eq!(
            resolve_cd(&session(f), &p("/srv"), "x").unwrap_err(),
            crate::session::DISCONNECTED_MSG
        );
    }
}
