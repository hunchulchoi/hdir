//! Copy / move when at least one side is a remote panel (M8).
//!
//! Routes (local to local stays in [`crate::fs_ops`]):
//! - local → remote and remote → local: `scp -rp`;
//! - two panels on the same server (same session): `cp -Rp` / `mv` run on
//!   the server, nothing goes through this machine;
//! - two different servers: `scp -3`.
//!
//! Items are handled one at a time: the destination is checked first (an
//! existing name asks through the usual conflict dialog), then the item is
//! transferred, and for a move the copy is checked before the source goes
//! (local sources to the trash, remote ones through
//! [`remote_ops::delete`] with its home / `/` protections). Progress is per
//! item, sized up front (`du -sk` on the host, a walk locally). Esc kills
//! the running scp; whatever it left behind is named in the result.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fs_ops::{self, ConflictAction, OpControl, OpOutcome, OpReport, Totals};
use crate::job::JobKind;
use crate::platform::os_bytes;
use crate::remote::{Endpoint, scp_argv, sh_quote};
use crate::remote_ops;
use crate::session::{CmdOutput, RemoteSession, first_line};
use crate::trash::TrashBackend;

/// Exit code of [`exists_cmd`] / [`is_dir_cmd`] when the answer is "no".
pub const NOT_THERE: i32 = 43;

/// Where one side of a transfer lives.
#[derive(Clone)]
pub enum Side {
    Local,
    Remote(Arc<RemoteSession>),
}

impl Side {
    fn session(&self) -> Option<&RemoteSession> {
        match self {
            Side::Local => None,
            Side::Remote(s) => Some(s),
        }
    }
}

pub struct TransferSpec {
    pub kind: JobKind,
    pub src: Side,
    pub src_dir: PathBuf,
    /// Plain child names of `src_dir`.
    pub names: Vec<OsString>,
    pub dst: Side,
    pub dest_dir: PathBuf,
    /// Where moved local sources go.
    pub trash: Arc<dyn TrashBackend>,
}

/// What the transfer needs from the job worker beyond [`OpControl`].
pub trait TransferCtl: OpControl {
    fn started(&mut self, totals: Totals);
    fn cancel_flag(&self) -> &AtomicBool;
}

// ---- command construction ----

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Exit 0 when `path` exists (a dangling symlink counts), [`NOT_THERE`] when not.
pub fn exists_cmd(path: &[u8]) -> Vec<u8> {
    let q = sh_quote(path);
    cat(&[
        b"if [ -e ",
        &q,
        b" ] || [ -L ",
        &q,
        b" ]; then exit 0; fi; exit 43",
    ])
}

/// Exit 0 when `path` is a directory, [`NOT_THERE`] when not.
pub fn is_dir_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[
        b"if [ -d ",
        &sh_quote(path),
        b" ]; then exit 0; fi; exit 43",
    ])
}

/// Size in KiB of each of `names` in `dir`, printed NUL-terminated in order
/// (`0` when unknown). Only the number before the first tab is kept, so a
/// name with a newline cannot shift the list.
pub fn du_cmd(dir: &[u8], names: &[&[u8]]) -> Vec<u8> {
    let list = names
        .iter()
        .map(|n| sh_quote(n))
        .collect::<Vec<_>>()
        .join(&b' ');
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; for f in ",
        &list,
        b"; do k=$(du -sk -- \"$f\" 2>/dev/null | head -n 1 | cut -f 1); printf '%s\\0' \"${k:-0}\"; done",
    ])
}

/// Bytes per name from [`du_cmd`] output (`n` entries, unknown ones 0).
pub fn parse_du(stdout: &[u8], n: usize) -> Vec<u64> {
    let mut v: Vec<u64> = stdout
        .split(|&b| b == 0)
        .take(n)
        .map(|f| {
            std::str::from_utf8(f)
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .map_or(0, |k| k.saturating_mul(1024))
        })
        .collect();
    v.resize(n, 0);
    v
}

/// `dir` with exactly one trailing `/`, so `cp`/`mv` always treat it as the
/// directory to put things *into*.
fn into_dir(dir: &[u8]) -> Vec<u8> {
    let mut d = dir.to_vec();
    while d.len() > 1 && d.ends_with(b"/") {
        d.pop();
    }
    if d != b"/" {
        d.push(b'/');
    }
    d
}

/// Copy `src` into `dest_dir` on the same host.
pub fn cp_cmd(src: &[u8], dest_dir: &[u8]) -> Vec<u8> {
    cat(&[
        b"cp -Rp -- ",
        &sh_quote(src),
        b" ",
        &sh_quote(&into_dir(dest_dir)),
    ])
}

/// Move `src` into `dest_dir` on the same host.
pub fn mv_cmd(src: &[u8], dest_dir: &[u8]) -> Vec<u8> {
    cat(&[
        b"mv -f -- ",
        &sh_quote(src),
        b" ",
        &sh_quote(&into_dir(dest_dir)),
    ])
}

// ---- running ----

enum Route<'a> {
    Up(&'a RemoteSession),
    Down(&'a RemoteSession),
    SameHost(&'a RemoteSession),
    Between(&'a RemoteSession, &'a RemoteSession),
}

fn route(spec: &TransferSpec) -> Result<Route<'_>, String> {
    Ok(match (spec.src.session(), spec.dst.session()) {
        (None, None) => return Err("로컬 간 복사는 원격 전송이 아님".into()),
        (None, Some(d)) => Route::Up(d),
        (Some(s), None) => Route::Down(s),
        (Some(s), Some(d)) if std::ptr::eq(s, d) => Route::SameHost(s),
        (Some(s), Some(d)) => Route::Between(s, d),
    })
}

fn b(p: &Path) -> Vec<u8> {
    os_bytes(p.as_os_str()).into_owned()
}

/// Exit-code check of a yes/no command.
fn yes_no(s: &RemoteSession, cmd: &[u8]) -> Result<bool, String> {
    let out = s.exec(cmd).map_err(|e| e.message())?;
    match out.code {
        Some(0) => Ok(true),
        Some(NOT_THERE) => Ok(false),
        c => Err(first_line(
            &out.stderr,
            &format!("종료 코드 {}", c.unwrap_or(-1)),
        )),
    }
}

fn exists(side: &Side, path: &Path) -> Result<bool, String> {
    match side {
        Side::Local => match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.to_string()),
        },
        Side::Remote(s) => yes_no(s, &exists_cmd(&b(path))),
    }
}

fn is_dir(side: &Side, path: &Path) -> Result<bool, String> {
    match side {
        Side::Local => Ok(path.is_dir()),
        Side::Remote(s) => yes_no(s, &is_dir_cmd(&b(path))),
    }
}

/// Size of every item (unknown sizes are 0); `None` when cancelled.
fn sizes(spec: &TransferSpec, cancel: &AtomicBool) -> Option<Vec<u64>> {
    match &spec.src {
        Side::Local => {
            let stop = || cancel.load(Ordering::Relaxed);
            let mut v = Vec::with_capacity(spec.names.len());
            for n in &spec.names {
                match fs_ops::measure_cancellable(&[spec.src_dir.join(n)], &stop) {
                    Ok(Some(t)) => v.push(t.bytes),
                    Ok(None) => return None,
                    Err(_) => v.push(0),
                }
            }
            Some(v)
        }
        Side::Remote(s) => {
            let names: Vec<Vec<u8>> = spec
                .names
                .iter()
                .map(|n| os_bytes(n).into_owned())
                .collect();
            let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
            let n = names.len();
            Some(match s.exec(&du_cmd(&b(&spec.src_dir), &refs)) {
                Ok(out) if out.code == Some(0) => parse_du(&out.stdout, n),
                _ => vec![0; n],
            })
        }
    }
}

fn ran(out: CmdOutput) -> Result<(), String> {
    match out.code {
        Some(0) => Ok(()),
        c => Err(first_line(
            &out.stderr,
            &format!("종료 코드 {}", c.unwrap_or(-1)),
        )),
    }
}

/// Copy (or, on one host, move) `src` into `dest_dir`.
fn transfer_one(
    route: &Route,
    move_on_host: bool,
    src: &Path,
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (sb, db) = (b(src), b(dest_dir));
    let scp = |s: &RemoteSession, from: Endpoint, to: Endpoint| {
        let argv = scp_argv(&from, &to, s.scp_mode)?;
        ran(s.scp_cancellable(&argv, cancel).map_err(|e| e.message())?)
    };
    match route {
        Route::Up(d) => scp(d, Endpoint::Local(src), Endpoint::Remote(&d.target, &db)),
        Route::Down(s) => scp(
            s,
            Endpoint::Remote(&s.target, &sb),
            Endpoint::Local(dest_dir),
        ),
        Route::Between(s, d) => scp(
            s,
            Endpoint::Remote(&s.target, &sb),
            Endpoint::Remote(&d.target, &db),
        ),
        Route::SameHost(s) => {
            let cmd = if move_on_host {
                mv_cmd(&sb, &db)
            } else {
                cp_cmd(&sb, &db)
            };
            ran(s.exec_cancellable(&cmd, cancel).map_err(|e| e.message())?)
        }
    }
}

/// Remove a moved source once its copy is in place.
fn delete_source(spec: &TransferSpec, name: &OsString) -> Result<(), String> {
    match &spec.src {
        Side::Local => {
            let src = spec.src_dir.join(name);
            match spec.trash.delete(std::slice::from_ref(&src)).pop() {
                Some((_, r)) => r,
                None => Err("휴지통 이동 결과 없음".into()),
            }
        }
        Side::Remote(s) => remote_ops::delete(s, &spec.src_dir, std::slice::from_ref(name)),
    }
}

fn same_place(spec: &TransferSpec) -> bool {
    match (spec.src.session(), spec.dst.session()) {
        (Some(s), Some(d)) => std::ptr::eq(s, d),
        _ => false,
    }
}

fn report(outcome: OpOutcome) -> OpReport {
    OpReport {
        outcome,
        ..OpReport::failed("")
    }
}

/// Run the whole job (blocking; called on the job worker thread).
pub fn run(spec: &TransferSpec, ctl: &mut dyn TransferCtl) -> OpReport {
    let route = match route(spec) {
        Ok(r) => r,
        Err(e) => return OpReport::failed(e),
    };
    if same_place(spec) {
        if spec.dest_dir == spec.src_dir {
            return OpReport::failed("같은 폴더로는 보낼 수 없음");
        }
        if spec
            .names
            .iter()
            .any(|n| spec.dest_dir.starts_with(spec.src_dir.join(n)))
        {
            return OpReport::failed("폴더를 그 안으로 보낼 수 없음");
        }
    }
    match is_dir(&spec.dst, &spec.dest_dir) {
        Ok(true) => {}
        Ok(false) => return OpReport::failed("대상 폴더 없음"),
        Err(e) => return OpReport::failed(e),
    }
    let Some(sizes) = sizes(spec, ctl.cancel_flag()) else {
        return report(OpOutcome::Cancelled);
    };
    ctl.started(Totals {
        files: spec.names.len() as u64,
        bytes: sizes.iter().sum(),
    });
    let mut r = report(OpOutcome::Done);
    let is_move = spec.kind == JobKind::Move;
    for (name, &size) in spec.names.iter().zip(&sizes) {
        if ctl.is_cancelled() {
            r.outcome = OpOutcome::Cancelled;
            break;
        }
        let src = spec.src_dir.join(name);
        let dest = spec.dest_dir.join(name);
        ctl.on_progress(0, &src);
        match exists(&spec.dst, &dest) {
            Ok(false) => {}
            Ok(true) => match ctl.on_conflict(&dest) {
                ConflictAction::Overwrite => r.overwritten += 1,
                ConflictAction::Skip => {
                    r.skipped += 1;
                    ctl.on_item_done(&src);
                    continue;
                }
                ConflictAction::Cancel => {
                    r.outcome = OpOutcome::Cancelled;
                    break;
                }
            },
            Err(e) => {
                r.outcome = OpOutcome::Failed(e);
                break;
            }
        }
        let on_host = is_move && matches!(route, Route::SameHost(_));
        if let Err(e) = transfer_one(&route, on_host, &src, &spec.dest_dir, ctl.cancel_flag()) {
            if ctl.is_cancelled() {
                r.outcome = OpOutcome::Cancelled;
                r.warnings.push(format!(
                    "부분 파일이 남았을 수 있음: {}",
                    dest.to_string_lossy()
                ));
            } else {
                r.outcome = OpOutcome::Failed(format!("{}: {e}", name.to_string_lossy()));
            }
            break;
        }
        if is_move && !on_host {
            match exists(&spec.dst, &dest) {
                Ok(true) => {}
                Ok(false) => {
                    r.outcome = OpOutcome::Failed(format!(
                        "{}: 전송 후 대상이 없어 원본을 지우지 않음",
                        name.to_string_lossy()
                    ));
                    break;
                }
                Err(e) => {
                    r.outcome = OpOutcome::Failed(format!(
                        "{}: 전송 확인 실패, 원본을 지우지 않음 ({e})",
                        name.to_string_lossy()
                    ));
                    break;
                }
            }
            if let Err(e) = delete_source(spec, name) {
                r.partial += 1;
                r.warnings.push(format!(
                    "원본을 지우지 못함: {} ({e})",
                    name.to_string_lossy()
                ));
                ctl.on_progress(size, &src);
                ctl.on_item_done(&src);
                continue;
            }
        }
        r.copied += 1;
        r.done_sources.push(src.clone());
        ctl.on_progress(size, &src);
        ctl.on_item_done(&src);
    }
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::job::ConflictAnswer;
    use crate::job::ConflictPolicy;
    use crate::session::CANCELLED_MSG;
    use crate::testutil::{FakeCmd, code, fail, ok, remote_cmd, session};
    use crate::trash::FakeTrash;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ---- command construction ----

    #[test]
    fn yes_no_commands_quote_the_path() {
        assert_eq!(
            exists_cmd(b"/a b/it's"),
            b"if [ -e '/a b/it'\\''s' ] || [ -L '/a b/it'\\''s' ]; then exit 0; fi; exit 43"
                .to_vec()
        );
        assert_eq!(
            is_dir_cmd(b"/srv/x"),
            b"if [ -d '/srv/x' ]; then exit 0; fi; exit 43".to_vec()
        );
    }

    #[test]
    fn cp_and_mv_always_target_the_directory() {
        assert_eq!(
            cp_cmd(b"/a/x y", b"/dst"),
            b"cp -Rp -- '/a/x y' '/dst/'".to_vec()
        );
        assert_eq!(
            cp_cmd(b"/a/x", b"/dst//"),
            b"cp -Rp -- '/a/x' '/dst/'".to_vec()
        );
        assert_eq!(mv_cmd(b"/a/-x", b"/"), b"mv -f -- '/a/-x' '/'".to_vec());
    }

    #[test]
    fn du_output_is_parsed_in_order_and_padded() {
        assert_eq!(parse_du(b"1\x002\x00", 2), vec![1024, 2048]);
        assert_eq!(parse_du(b"x\x00", 3), vec![0, 0, 0]);
        assert_eq!(parse_du(b"", 1), vec![0]);
    }

    fn sh(cmd: &[u8]) -> std::process::Output {
        use std::os::unix::ffi::OsStrExt;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(std::ffi::OsStr::from_bytes(cmd))
            .output()
            .unwrap()
    }

    #[test]
    fn real_du_keeps_one_entry_per_name_even_with_newlines() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a\nb"), vec![0u8; 5000]).unwrap();
        std::fs::create_dir(t.path().join("-d")).unwrap();
        std::fs::write(t.path().join("-d/f"), vec![1u8; 9000]).unwrap();
        let names: [&[u8]; 3] = [b"a\nb", b"-d", b"missing"];
        let out = sh(&du_cmd(&b(t.path()), &names));
        assert!(out.status.success(), "{out:?}");
        let v = parse_du(&out.stdout, 3);
        assert!(v[0] >= 4096 && v[1] >= 8192, "{v:?}");
        assert_eq!(v[2], 0);
    }

    #[test]
    fn real_yes_no_commands_answer_with_exit_codes() {
        let t = tempfile::tempdir().unwrap();
        let link = t.path().join("dangling");
        std::os::unix::fs::symlink("/nonexistent/x", &link).unwrap();
        assert_eq!(sh(&exists_cmd(&b(&link))).status.code(), Some(0));
        let gone = t.path().join("gone 'q'");
        assert_eq!(sh(&exists_cmd(&b(&gone))).status.code(), Some(NOT_THERE));
        assert_eq!(sh(&is_dir_cmd(&b(t.path()))).status.code(), Some(0));
        assert_eq!(sh(&is_dir_cmd(&b(&link))).status.code(), Some(NOT_THERE));
    }

    #[test]
    fn real_cp_and_mv_put_items_inside_the_directory() {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("src"), t.path().join("dst"));
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::write(src.join("d/f"), "x").unwrap();
        std::fs::write(src.join("it's"), "y").unwrap();
        std::fs::create_dir_all(dst.join("d")).unwrap(); // existing dir: merge
        assert!(sh(&cp_cmd(&b(&src.join("d")), &b(&dst))).status.success());
        assert_eq!(std::fs::read(dst.join("d/f")).unwrap(), b"x");
        assert!(!dst.join("d/d").exists());
        assert!(
            sh(&mv_cmd(&b(&src.join("it's")), &b(&dst)))
                .status
                .success()
        );
        assert_eq!(std::fs::read(dst.join("it's")).unwrap(), b"y");
        assert!(!src.join("it's").exists());
    }

    // ---- running ----

    struct Ctl {
        cancel: Arc<AtomicBool>,
        answers: VecDeque<ConflictAnswer>,
        policy: ConflictPolicy,
        conflicts: Vec<PathBuf>,
        totals: Option<Totals>,
        bytes: u64,
        items: Vec<PathBuf>,
    }

    impl Ctl {
        fn new() -> Self {
            Self {
                cancel: Arc::new(AtomicBool::new(false)),
                answers: VecDeque::new(),
                policy: ConflictPolicy::default(),
                conflicts: vec![],
                totals: None,
                bytes: 0,
                items: vec![],
            }
        }
        fn answering(a: &[ConflictAnswer]) -> Self {
            let mut c = Self::new();
            c.answers = a.iter().copied().collect();
            c
        }
    }

    impl OpControl for Ctl {
        fn on_progress(&mut self, delta: u64, _current: &Path) {
            self.bytes += delta;
        }
        fn on_item_done(&mut self, src: &Path) {
            self.items.push(src.to_path_buf());
        }
        fn is_cancelled(&self) -> bool {
            self.cancel.load(Ordering::SeqCst)
        }
        fn on_conflict(&mut self, dest: &Path) -> ConflictAction {
            self.conflicts.push(dest.to_path_buf());
            if let Some(a) = self.policy.remembered() {
                return a;
            }
            let a = self.answers.pop_front().unwrap_or(ConflictAnswer::Cancel);
            self.policy.decide(a)
        }
    }

    impl TransferCtl for Ctl {
        fn started(&mut self, totals: Totals) {
            self.totals = Some(totals);
        }
        fn cancel_flag(&self) -> &AtomicBool {
            &self.cancel
        }
    }

    fn names(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn spec(
        kind: JobKind,
        src: Side,
        src_dir: &Path,
        n: &[&str],
        dst: Side,
        dest: &Path,
    ) -> TransferSpec {
        TransferSpec {
            kind,
            src,
            src_dir: src_dir.to_path_buf(),
            names: names(n),
            dst,
            dest_dir: dest.to_path_buf(),
            trash: Arc::new(FakeTrash::new()),
        }
    }

    /// A host where every path in `there` exists and `/dst` is a directory;
    /// scp / cp / mv succeed and the first `du` answers `du`.
    fn host(there: &'static [&'static str], du: &'static str) -> Arc<FakeCmd> {
        FakeCmd::new(move |a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("if [ -d '/dst") || c.starts_with("if [ -d '/srv") {
                return code(0);
            }
            if c.starts_with("if [ -e ") {
                let hit = there
                    .iter()
                    .any(|t| c.starts_with(&format!("if [ -e '{t}' ]")));
                return code(if hit { 0 } else { NOT_THERE });
            }
            if c.starts_with("cd -- ") {
                return ok(du);
            }
            ok("")
        })
    }

    fn scp_pairs(f: &FakeCmd) -> Vec<(String, String)> {
        f.scp_calls()
            .iter()
            .map(|a| (a[a.len() - 2].clone(), a[a.len() - 1].clone()))
            .collect()
    }

    #[test]
    fn upload_sends_each_item_with_scp_and_counts_local_sizes() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "12345").unwrap();
        std::fs::create_dir(t.path().join("d")).unwrap();
        std::fs::write(t.path().join("d/x"), "123").unwrap();
        let f = host(&[], "");
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a", "d"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = Ctl::new();
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.done_sources.len()), (2, 2));
        assert_eq!(c.totals, Some(Totals { files: 2, bytes: 8 }));
        assert_eq!(c.bytes, 8);
        let a = t.path().join("a").to_string_lossy().into_owned();
        let d = t.path().join("d").to_string_lossy().into_owned();
        assert_eq!(
            scp_pairs(&f),
            vec![(a, "prod-web:/dst".into()), (d, "prod-web:/dst".into())]
        );
        assert!(f.scp_calls()[0].contains(&"-rp".to_string()));
    }

    #[test]
    fn download_sizes_with_du_and_scps_into_the_local_dir() {
        let t = tempfile::tempdir().unwrap();
        let f = host(&[], "1\x002\x00");
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a b", "c"],
            Side::Local,
            t.path(),
        );
        let mut c = Ctl::new();
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!(
            c.totals,
            Some(Totals {
                files: 2,
                bytes: 3072
            })
        );
        let dir = t.path().to_string_lossy().into_owned();
        assert_eq!(
            scp_pairs(&f),
            vec![
                ("prod-web:/srv/a b".into(), dir.clone()),
                ("prod-web:/srv/c".into(), dir)
            ]
        );
        assert!(
            f.remote_cmds()
                .iter()
                .any(|c| c.starts_with("cd -- '/srv' || exit 1; for f in 'a b' 'c';"))
        );
    }

    #[test]
    fn same_server_copies_and_moves_on_the_server() {
        let f = host(&[], "");
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Copy,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["a"],
            Side::Remote(Arc::clone(&sess)),
            &p("/dst"),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.copied), (OpOutcome::Done, 1));
        assert!(f.scp_calls().is_empty());
        let long = f.long_cmds();
        assert_eq!(
            long,
            vec!["cp -Rp -- '/srv/a' '/dst/'", "mv -f -- '/srv/b' '/dst/'"]
        );
        // a move on one host is a rename: nothing is deleted afterwards
        assert!(!f.remote_cmds().iter().any(|c| c.contains("rm -rf")));
    }

    #[test]
    fn two_servers_go_through_scp_3() {
        let fs = host(&[], "");
        let fd = host(&[], "");
        let src = session(Arc::clone(&fs));
        let dst = RemoteSession::new(
            "other".into(),
            crate::remote::SshTarget {
                host: "other-host".into(),
                port: None,
                control_path: p("/tmp/hdir-1/cm-other"),
            },
            crate::remote::RemoteOs::Linux,
            crate::remote::ScpMode::Sftp,
            Arc::clone(&fd) as Arc<dyn crate::session::Cmd>,
        );
        let s = spec(
            JobKind::Copy,
            Side::Remote(src),
            &p("/srv"),
            &["a"],
            Side::Remote(Arc::new(dst)),
            &p("/dst"),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let calls = fs.scp_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&"-3".to_string()), "{calls:?}");
        assert_eq!(
            scp_pairs(&fs),
            vec![("prod-web:/srv/a".into(), "other-host:/dst".into())]
        );
    }

    #[test]
    fn existing_names_ask_and_follow_the_answer() {
        let f = host(&[], "");
        let t = tempfile::tempdir().unwrap();
        for n in ["a", "b", "c"] {
            std::fs::write(t.path().join(n), "old").unwrap();
        }
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a", "b", "c", "new"],
            Side::Local,
            t.path(),
        );
        let mut c = Ctl::answering(&[
            ConflictAnswer::Skip,
            ConflictAnswer::Overwrite,
            ConflictAnswer::SkipAll,
        ]);
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.skipped, r.overwritten), (2, 2, 1));
        assert_eq!(c.conflicts.len(), 3);
        let sent: Vec<String> = scp_pairs(&f).into_iter().map(|(a, _)| a).collect();
        assert_eq!(sent, vec!["prod-web:/srv/b", "prod-web:/srv/new"]);
        assert_eq!(c.items.len(), 4, "skipped items still advance progress");
    }

    #[test]
    fn a_remote_conflict_cancelled_stops_before_any_transfer() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "x").unwrap();
        let f = host(&["/dst/a"], "");
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = Ctl::answering(&[ConflictAnswer::Cancel]);
        assert_eq!(run(&s, &mut c).outcome, OpOutcome::Cancelled);
        assert_eq!(c.conflicts, vec![p("/dst/a")]);
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn a_missing_destination_folder_fails_before_anything_runs() {
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                ok("")
            } else {
                code(NOT_THERE)
            }
        });
        let t = tempfile::tempdir().unwrap();
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/nope"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Failed("대상 폴더 없음".into()));
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn the_same_folder_or_a_folder_into_itself_is_refused_on_one_host() {
        let f = host(&[], "");
        let sess = session(Arc::clone(&f));
        let one = |dest: &str| {
            let s = spec(
                JobKind::Move,
                Side::Remote(Arc::clone(&sess)),
                &p("/srv"),
                &["d"],
                Side::Remote(Arc::clone(&sess)),
                &p(dest),
            );
            run(&s, &mut Ctl::new()).outcome
        };
        assert!(matches!(one("/srv"), OpOutcome::Failed(m) if m.contains("같은 폴더")));
        assert!(matches!(one("/srv/d/sub"), OpOutcome::Failed(m) if m.contains("그 안으로")));
        assert!(
            f.log.lock().unwrap().is_empty(),
            "refused before any command"
        );
        // "/srv/dd" is not inside "/srv/d"
        assert_eq!(one("/srv/dd"), OpOutcome::Done);
    }

    #[test]
    fn a_failed_scp_stops_the_job_with_its_error() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return fail(1, "scp: /dst/a: Permission denied\n");
            }
            let c = remote_cmd(a).unwrap_or_default();
            code(if c.starts_with("if [ -d") {
                0
            } else {
                NOT_THERE
            })
        });
        let s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a", "b"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("a: scp: /dst/a: Permission denied".into())
        );
        assert_eq!(f.scp_calls().len(), 1);
        assert_eq!(r.copied, 0);
    }

    #[test]
    fn a_dropped_connection_is_reported_as_such() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return code(255);
            }
            let c = remote_cmd(a).unwrap_or_default();
            code(if c.starts_with("if [ -d") {
                0
            } else {
                NOT_THERE
            })
        });
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(Arc::clone(&sess)),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert!(
            matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("연결 끊김")),
            "{:?}",
            r.outcome
        );
        assert!(!sess.is_alive());
    }

    #[test]
    fn esc_during_scp_stops_and_names_the_partial_target() {
        let t = tempfile::tempdir().unwrap();
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                flag.store(true, Ordering::SeqCst); // Esc while it runs
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: CANCELLED_MSG.as_bytes().to_vec(),
                };
            }
            let cm = remote_cmd(a).unwrap_or_default();
            code(if cm.starts_with("if [ -d") {
                0
            } else {
                NOT_THERE
            })
        });
        let s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a", "b"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert_eq!(
            r.warnings,
            vec!["부분 파일이 남았을 수 있음: /dst/a".to_string()]
        );
        assert_eq!(f.scp_calls().len(), 1, "b never starts");
        assert_eq!(f.transfers.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_cancel_before_the_next_item_stops_cleanly() {
        let t = tempfile::tempdir().unwrap();
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                flag.store(true, Ordering::SeqCst); // Esc right after a finished
                return ok("");
            }
            let cm = remote_cmd(a).unwrap_or_default();
            code(if cm.starts_with("if [ -d") {
                0
            } else {
                NOT_THERE
            })
        });
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a", "b"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!((r.outcome, r.copied), (OpOutcome::Cancelled, 1));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn moving_up_trashes_the_local_source_after_checking_the_copy() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "x").unwrap();
        let seen = Arc::new(Mutex::new(0));
        let s2 = Arc::clone(&seen);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("if [ -d") {
                return code(0);
            }
            // first -e: before (missing), second: after the copy (there)
            let mut n = s2.lock().unwrap();
            *n += 1;
            code(if *n == 1 { NOT_THERE } else { 0 })
        });
        let trash = Arc::new(FakeTrash::new());
        let mut s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        s.trash = Arc::clone(&trash) as Arc<dyn TrashBackend>;
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.copied), (OpOutcome::Done, 1));
        assert_eq!(*trash.trashed.lock().unwrap(), vec![t.path().join("a")]);
        assert!(r.moved.is_empty(), "remote moves are not undoable");
    }

    #[test]
    fn a_move_whose_copy_is_missing_keeps_the_source() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            code(if c.starts_with("if [ -d") {
                0
            } else {
                NOT_THERE
            })
        });
        let trash = Arc::new(FakeTrash::new());
        let mut s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        s.trash = Arc::clone(&trash) as Arc<dyn TrashBackend>;
        let r = run(&s, &mut Ctl::new());
        assert!(matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("원본을 지우지 않음")));
        assert!(trash.trashed.lock().unwrap().is_empty());
    }

    #[test]
    fn moving_down_deletes_the_remote_source_with_the_protected_delete() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("cd -- ") {
                return ok("0\x00");
            }
            ok("")
        });
        let tp = t.path().to_path_buf();
        // the scp fake does not create the file; do it so the check passes
        std::fs::write(tp.join("a"), "x").unwrap();
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            &tp,
        );
        let mut c = Ctl::answering(&[ConflictAnswer::Overwrite]);
        let r = run(&s, &mut c);
        assert_eq!((r.outcome, r.copied), (OpOutcome::Done, 1));
        let rm: Vec<String> = f
            .remote_cmds()
            .into_iter()
            .filter(|c| c.contains("rm -rf"))
            .collect();
        assert_eq!(rm.len(), 1, "{rm:?}");
        assert!(rm[0].contains("'a'"), "{}", rm[0]);
    }

    #[test]
    fn a_source_that_cannot_be_deleted_is_a_partial_move_with_a_warning() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("if [ -d") {
                return code(0);
            }
            code(0) // destination always there (conflict answered Overwrite)
        });
        let trash = Arc::new(FakeTrash::new());
        trash
            .fail_paths
            .lock()
            .unwrap()
            .insert(t.path().join("a"), "busy".into());
        let mut s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        s.trash = Arc::clone(&trash) as Arc<dyn TrashBackend>;
        let r = run(&s, &mut Ctl::answering(&[ConflictAnswer::Overwrite]));
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.partial), (0, 1));
        assert!(r.done_sources.is_empty(), "the source stays selected");
        assert_eq!(r.warnings, vec!["원본을 지우지 못함: a (busy)".to_string()]);
    }
}
