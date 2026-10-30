//! File operations on a remote host over a [`RemoteSession`]: mkdir, rename,
//! delete, preview reads, and transfers for editing. Everything here blocks;
//! callers run it on a worker thread. Command construction lives in
//! [`crate::remote`]; these functions add validation and result mapping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::{os_bytes, os_from_bytes};
use crate::preview::{self, Limits, Preview};
use crate::remote::{Endpoint, scp_argv};
use crate::remote::{
    PREVIEW_BROKEN, PREVIEW_DIR, PREVIEW_MISSING, PREVIEW_SPECIAL, RENAME_EXISTS, home_cmd,
    mkdir_cmd, preview_cmd, rename_cmd, replace_cmd, rm_cmd, rm_file_cmd, rmdir_cmd,
};
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

/// Read the head of a remote file (or summarize a directory) like the local
/// preview does: at most `limits.max_bytes` (+3 so a split character can be
/// completed) cross the wire, then [`preview::classify_with`] decides.
pub fn preview(s: &RemoteSession, path: &Path, limits: Limits) -> Preview {
    let bytes = os_bytes(path.as_os_str());
    let out = match s.exec(&preview_cmd(&bytes, limits.max_bytes + 3)) {
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
/// Length and mtime give a cheap "unchanged" answer; when either differs the
/// content hash decides (an editor that re-saves identical bytes is not a
/// change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    mtime: Option<std::time::SystemTime>,
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
        let meta = std::fs::metadata(path)?;
        Ok(FileStamp {
            len: meta.len(),
            mtime: meta.modified().ok(),
            hash: hash_file(path)?,
        })
    }

    /// Did the content of `path` change since this stamp? A vanished file is
    /// an error.
    pub fn differs(&self, path: &Path) -> std::io::Result<bool> {
        let meta = std::fs::metadata(path)?;
        if meta.len() == self.len && meta.modified().ok() == self.mtime && self.mtime.is_some() {
            return Ok(false);
        }
        Ok(meta.len() != self.len || hash_file(path)? != self.hash)
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

/// Download `remote` into a fresh private temp dir (same file name, so
/// extensions keep working) and fingerprint it.
pub fn fetch(s: &RemoteSession, remote: &Path) -> Result<EditCopy, String> {
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
    let argv = scp_argv(
        &Endpoint::Remote(&s.target, &os_bytes(remote.as_os_str())),
        &Endpoint::Local(dir.path()),
        s.scp_mode,
    );
    scp_run(s, argv)?;
    let local = dir.path().join(name);
    let stamp = FileStamp::of(&local).map_err(|e| format!("내려받은 파일을 읽을 수 없음: {e}"))?;
    Ok(EditCopy { dir, local, stamp })
}

/// Unique-enough suffix for remote temp file names.
pub fn edit_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos:x}", std::process::id())
}

/// Upload `local` over `remote` without ever truncating it: scp to
/// `.hdir-edit-<token>` next to the original, then replace by `mv` (see
/// [`replace_cmd`]). On any failure the remote temp file is removed and the
/// original is untouched.
pub fn upload_replace(
    s: &RemoteSession,
    local: &Path,
    remote: &Path,
    token: &str,
) -> Result<(), String> {
    let tmp = remote.with_file_name(format!(".hdir-edit-{token}"));
    let tmp_b = os_bytes(tmp.as_os_str()).into_owned();
    let argv = scp_argv(
        &Endpoint::Local(local),
        &Endpoint::Remote(&s.target, &tmp_b),
        s.scp_mode,
    );
    let res = scp_run(s, argv)
        .and_then(|()| run_ok(s, &replace_cmd(&tmp_b, &os_bytes(remote.as_os_str()))).map(|_| ()));
    if res.is_err() {
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
        let want = crate::remote::preview_cmd(b"/srv/a b.txt", Limits::PREVIEW.max_bytes + 3);
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

    /// An scp fake: downloads create `<dest dir>/<basename>` with `body`;
    /// uploads (destination starts with the host) are recorded with the
    /// local file's content at that moment.
    fn scp_fake(
        body: &'static str,
        up_result: crate::session::CmdOutput,
        mv_result: crate::session::CmdOutput,
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
            if c.starts_with("if [ -L") {
                mv_result.clone()
            } else {
                ok("")
            }
        });
        (f, uploads)
    }

    #[test]
    fn fetch_downloads_into_a_private_temp_dir_and_stamps_it() {
        let (f, _) = scp_fake("orig\n", ok(""), ok(""));
        let s = session(Arc::clone(&f));
        let copy = fetch(&s, &p("/srv/it's a.txt")).unwrap();
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
        fetch(&s, &p("/srv/it's a.txt")).unwrap();
        let a = &f.scp_calls()[0];
        assert_eq!(a[a.len() - 2], "prod-web:'/srv/it'\\''s a.txt'");
    }

    #[test]
    fn fetch_failures_are_errors() {
        let f = FakeCmd::new(|_| fail(1, "scp: /srv/x: No such file or directory\n"));
        let e = fetch(&session(f), &p("/srv/x")).unwrap_err();
        assert!(e.contains("No such file"), "{e}");
        let f = FakeCmd::new(|_| ok("")); // "succeeds" but nothing arrived
        assert!(fetch(&session(f), &p("/srv/x")).is_err());
        let f = FakeCmd::new(|_| code(255));
        let s = session(f);
        assert_eq!(
            fetch(&s, &p("/srv/x")).unwrap_err(),
            crate::session::DISCONNECTED_MSG
        );
        assert!(!s.is_alive());
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
        std::fs::write(&f, "abcd").unwrap();
        assert!(st.differs(&f).unwrap());
        std::fs::remove_file(&f).unwrap();
        assert!(st.differs(&f).is_err());
    }

    #[test]
    fn upload_goes_to_a_temp_name_next_to_the_original_then_replaces_it() {
        let (f, ups) = scp_fake("", ok(""), ok(""));
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a b.txt");
        std::fs::write(&local, "new content").unwrap();
        upload_replace(&s, &local, &p("/srv/it's/a b.txt"), "T0K3N").unwrap();
        let ups = ups.lock().unwrap().clone();
        assert_eq!(
            ups,
            vec![(
                "prod-web:/srv/it's/.hdir-edit-T0K3N".to_string(),
                "new content".to_string()
            )]
        );
        let want = crate::remote::replace_cmd(b"/srv/it's/.hdir-edit-T0K3N", b"/srv/it's/a b.txt");
        assert_eq!(f.remote_cmds(), vec![String::from_utf8(want).unwrap()]);
    }

    #[test]
    fn upload_failures_clean_up_the_remote_temp_file_and_never_touch_the_original() {
        let (f, _) = scp_fake(
            "",
            fail(1, "scp: write failed: No space left on device\n"),
            ok(""),
        );
        let s = session(Arc::clone(&f));
        let t = tempfile::tempdir().unwrap();
        let local = t.path().join("a");
        std::fs::write(&local, "x").unwrap();
        let e = upload_replace(&s, &local, &p("/srv/a"), "TK").unwrap_err();
        assert!(e.contains("No space left"), "{e}");
        assert_eq!(
            f.remote_cmds(),
            vec!["rm -f -- '/srv/.hdir-edit-TK'".to_string()]
        );

        let (f, _) = scp_fake("", ok(""), fail(1, "mv: cannot move: Permission denied\n"));
        let s = session(Arc::clone(&f));
        let e = upload_replace(&s, &local, &p("/srv/a"), "TK").unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
        let cmds = f.remote_cmds();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[1], "rm -f -- '/srv/.hdir-edit-TK'");
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
