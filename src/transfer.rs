//! Copy / move when at least one side is a remote panel (M8).
//!
//! Routes (local to local stays in [`crate::fs_ops`]):
//! - local → remote and remote → local: `scp -rp`;
//! - two panels on the same server (same session): `cp -Rp` / `mv` run on
//!   the server, nothing goes through this machine;
//! - two different servers: `scp -3`.
//!
//! Every item lands in a fresh private staging directory inside the
//! destination (`.hdir-xfer.XXXXXX`) and is renamed into place only once it
//! arrived in full. So an interrupted transfer (Esc, lost link, full disk)
//! never touches an existing target, and an existing symlink at the target
//! is replaced as a link instead of being written through. Esc kills the
//! running scp and the staging directory is removed.
//!
//! Refused (skipped with a warning):
//! - over scp, an item that is or contains a symlink: scp follows links,
//!   which would copy data from outside the selection (`secret -> ~/.ssh`);
//! - a conflict where either side is a directory: merging cannot be atomic.
//!
//! A move checks the copy is in place before the source goes (local sources
//! to the trash, remote ones through [`remote_ops::delete`] with its home /
//! `/` protections); on one server it is a plain `mv`. Progress is per item,
//! sized up front (`du -sk` on the host, a walk locally).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fs_ops::{ConflictAction, OpControl, OpOutcome, OpReport, Totals};
use crate::job::JobKind;
use crate::platform::os_bytes;
use crate::remote::{Endpoint, scp_argv, sh_quote};
use crate::remote_ops;
use crate::session::{CmdOutput, RemoteSession, first_line};
use crate::trash::TrashBackend;

/// Exit codes of the remote helper commands.
pub const NOT_THERE: i32 = 43;
pub const IS_DIR: i32 = 44;
pub const IS_OTHER: i32 = 45;
/// [`place_cmd`]: the item is in place but the staging dir could not go.
pub const STAGE_LEFT: i32 = 46;

/// Name prefix of staging directories.
pub const STAGE_PREFIX: &str = ".hdir-xfer.";

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

/// What is at `path`, without following a final symlink: exit
/// [`NOT_THERE`], [`IS_DIR`] or [`IS_OTHER`] (file, symlink, special).
pub fn kind_cmd(path: &[u8]) -> Vec<u8> {
    let q = sh_quote(path);
    cat(&[
        b"if [ -L ",
        &q,
        b" ]; then exit 45; elif [ -d ",
        &q,
        b" ]; then exit 44; elif [ -e ",
        &q,
        b" ]; then exit 45; fi; exit 43",
    ])
}

/// Exit 0 when `path` is a directory (a symlink to one counts),
/// [`NOT_THERE`] when not.
pub fn is_dir_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[
        b"if [ -d ",
        &sh_quote(path),
        b" ]; then exit 0; fi; exit 43",
    ])
}

fn word_list(names: &[&[u8]]) -> Vec<u8> {
    names
        .iter()
        .map(|n| sh_quote(n))
        .collect::<Vec<_>>()
        .join(&b' ')
}

/// For each of `names` in `dir`: its size in KiB and whether it is or
/// contains a symlink (`1`/`0`), each NUL-terminated. Only the number before
/// the first tab of `du` is kept, so a name with a newline cannot shift the
/// list; `find` gets `./name` so a leading `-` is never an option.
pub fn scan_cmd(dir: &[u8], names: &[&[u8]]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; for f in ",
        &word_list(names),
        b"; do k=$(du -sk -- \"$f\" 2>/dev/null | head -n 1 | cut -f 1); l=0; \
          [ -z \"$(find \"./$f\" -type l 2>/dev/null | head -c 1)\" ] || l=1; \
          printf '%s\\0%s\\0' \"${k:-0}\" \"$l\"; done",
    ])
}

/// What [`scan_cmd`] (or a local walk) found about one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scanned {
    pub bytes: u64,
    pub has_link: bool,
}

/// Parse [`scan_cmd`] output; `None` unless it holds exactly `n` records.
pub fn parse_scan(stdout: &[u8], n: usize) -> Option<Vec<Scanned>> {
    if n == 0 {
        return stdout.is_empty().then(Vec::new);
    }
    let body = stdout.strip_suffix(b"\0")?;
    let fields: Vec<&[u8]> = body.split(|&b| b == 0).collect();
    if fields.len() != 2 * n {
        return None;
    }
    fields
        .chunks(2)
        .map(|c| {
            let kib = std::str::from_utf8(c[0])
                .ok()?
                .trim()
                .parse::<u64>()
                .unwrap_or(0);
            let has_link = match c[1] {
                b"0" => false,
                b"1" => true,
                _ => return None,
            };
            Some(Scanned {
                bytes: kib.saturating_mul(1024),
                has_link,
            })
        })
        .collect()
}

/// `dir` with exactly one trailing `/`.
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

/// Create a private (mode 700) staging dir in `dest_dir`, printed
/// NUL-terminated.
pub fn stage_cmd(dest_dir: &[u8]) -> Vec<u8> {
    let template = cat(&[&into_dir(dest_dir), STAGE_PREFIX.as_bytes(), b"XXXXXX"]);
    cat(&[
        b"d=$(mktemp -d ",
        &sh_quote(&template),
        b") || exit 1; printf '%s\\0' \"$d\"",
    ])
}

/// The staging path printed by [`stage_cmd`], checked to be a direct child
/// of `dest_dir` with the staging prefix (anything else is refused, so a
/// confused reply can never become an `rm -rf` target).
pub fn parse_stage(stdout: &[u8], dest_dir: &[u8]) -> Option<Vec<u8>> {
    let end = stdout.iter().position(|&b| b == 0)?;
    let path = &stdout[..end];
    let prefix = cat(&[&into_dir(dest_dir), STAGE_PREFIX.as_bytes()]);
    let rest = path.strip_prefix(prefix.as_slice())?;
    (!rest.is_empty() && !rest.contains(&b'/')).then(|| path.to_vec())
}

/// Rename `stage/name` onto `dest_dir/name` (a file or link there is
/// replaced, never followed), then remove the empty staging dir.
pub fn place_cmd(stage: &[u8], name: &[u8], dest_dir: &[u8]) -> Vec<u8> {
    let from = cat(&[&into_dir(stage), name]);
    let to = cat(&[&into_dir(dest_dir), name]);
    cat(&[
        b"mv -f -- ",
        &sh_quote(&from),
        b" ",
        &sh_quote(&to),
        b" || exit 1; rmdir -- ",
        &sh_quote(stage),
        b" || exit 46",
    ])
}

/// Remove a staging dir and whatever a failed transfer left in it.
pub fn unstage_cmd(stage: &[u8]) -> Vec<u8> {
    cat(&[b"rm -rf -- ", &sh_quote(stage)])
}

/// Copy `src` into `dir` on the same host.
pub fn cp_cmd(src: &[u8], dir: &[u8]) -> Vec<u8> {
    cat(&[
        b"cp -Rp -- ",
        &sh_quote(src),
        b" ",
        &sh_quote(&into_dir(dir)),
    ])
}

/// Move `src` into `dest_dir` on the same host (a rename when it can be).
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
    Between(&'a RemoteSession),
}

impl Route<'_> {
    fn uses_scp(&self) -> bool {
        !matches!(self, Route::SameHost(_))
    }
}

fn route(spec: &TransferSpec) -> Result<Route<'_>, String> {
    Ok(match (spec.src.session(), spec.dst.session()) {
        (None, None) => return Err("로컬 간 복사는 원격 전송이 아님".into()),
        (None, Some(d)) => Route::Up(d),
        (Some(s), None) => Route::Down(s),
        (Some(s), Some(d)) if std::ptr::eq(s, d) => Route::SameHost(s),
        (Some(s), Some(_)) => Route::Between(s),
    })
}

fn b(p: &Path) -> Vec<u8> {
    os_bytes(p.as_os_str()).into_owned()
}

fn code_err(out: &CmdOutput) -> String {
    first_line(
        &out.stderr,
        &format!("종료 코드 {}", out.code.unwrap_or(-1)),
    )
}

fn ran(out: CmdOutput) -> Result<CmdOutput, String> {
    if out.code == Some(0) {
        Ok(out)
    } else {
        Err(code_err(&out))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Missing,
    Dir,
    Other,
}

fn kind(side: &Side, path: &Path) -> Result<Kind, String> {
    match side {
        Side::Local => match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => Ok(Kind::Dir),
            Ok(_) => Ok(Kind::Other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Kind::Missing),
            Err(e) => Err(e.to_string()),
        },
        Side::Remote(s) => {
            let out = s.exec(&kind_cmd(&b(path))).map_err(|e| e.message())?;
            match out.code {
                Some(NOT_THERE) => Ok(Kind::Missing),
                Some(IS_DIR) => Ok(Kind::Dir),
                Some(IS_OTHER) => Ok(Kind::Other),
                _ => Err(code_err(&out)),
            }
        }
    }
}

fn dest_dir_ok(side: &Side, dir: &Path) -> Result<(), String> {
    let there = match side {
        Side::Local => dir.is_dir(),
        Side::Remote(s) => {
            let out = s.exec(&is_dir_cmd(&b(dir))).map_err(|e| e.message())?;
            match out.code {
                Some(0) => true,
                Some(NOT_THERE) => false,
                _ => return Err(code_err(&out)),
            }
        }
    };
    if there {
        Ok(())
    } else {
        Err("대상 폴더 없음".into())
    }
}

/// Size and symlink check of a local item; unreadable parts count as 0
/// bytes (scp cannot read them either). `None` when cancelled.
fn scan_local(p: &Path, stop: &AtomicBool) -> Option<Scanned> {
    let mut s = Scanned::default();
    let mut todo = vec![p.to_path_buf()];
    while let Some(q) = todo.pop() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(m) = std::fs::symlink_metadata(&q) else {
            continue;
        };
        if m.file_type().is_symlink() {
            s.has_link = true;
        } else if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&q) {
                todo.extend(rd.flatten().map(|e| e.path()));
            }
        } else {
            s.bytes += m.len();
        }
    }
    Some(s)
}

/// Size and symlink check of every item; `Ok(None)` when cancelled. A
/// same-server move is a rename, so nothing is read for it.
fn scan(
    spec: &TransferSpec,
    route: &Route,
    cancel: &AtomicBool,
) -> Result<Option<Vec<Scanned>>, String> {
    let n = spec.names.len();
    if spec.kind == JobKind::Move && !route.uses_scp() {
        return Ok(Some(vec![Scanned::default(); n]));
    }
    match &spec.src {
        Side::Local => Ok(spec
            .names
            .iter()
            .map(|name| scan_local(&spec.src_dir.join(name), cancel))
            .collect()),
        Side::Remote(s) => {
            let names: Vec<Vec<u8>> = spec
                .names
                .iter()
                .map(|n| os_bytes(n).into_owned())
                .collect();
            let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
            let res = s.exec_cancellable(&scan_cmd(&b(&spec.src_dir), &refs), cancel);
            if cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let out = ran(res.map_err(|e| e.message())?)?;
            match parse_scan(&out.stdout, n) {
                Some(v) => Ok(Some(v)),
                None => Err("원격 항목을 확인할 수 없음".into()),
            }
        }
    }
}

/// A staging directory inside the destination.
enum Stage<'a> {
    Local(tempfile::TempDir),
    Remote(&'a RemoteSession, Vec<u8>),
}

impl Stage<'_> {
    fn endpoint(&self) -> Endpoint<'_> {
        match self {
            Stage::Local(t) => Endpoint::Local(t.path()),
            Stage::Remote(s, p) => Endpoint::Remote(&s.target, p),
        }
    }
}

fn make_stage<'a>(dst: &'a Side, dest_dir: &Path) -> Result<Stage<'a>, String> {
    match dst {
        Side::Local => tempfile::Builder::new()
            .prefix(STAGE_PREFIX)
            .tempdir_in(dest_dir)
            .map(Stage::Local)
            .map_err(|e| format!("임시 폴더를 만들 수 없음: {e}")),
        Side::Remote(s) => {
            let db = b(dest_dir);
            let out = ran(s.exec(&stage_cmd(&db)).map_err(|e| e.message())?)
                .map_err(|e| format!("임시 폴더를 만들 수 없음: {e}"))?;
            parse_stage(&out.stdout, &db)
                .map(|p| Stage::Remote(s, p))
                .ok_or_else(|| "임시 폴더 경로를 알 수 없음".into())
        }
    }
}

fn left_behind(path: &str) -> String {
    format!("임시 폴더가 남음: {path}")
}

/// Rename the staged item into place. `Ok(Some(warning))` when it is in
/// place but the staging dir stayed. On error the stage is discarded.
fn place(stage: Stage, name: &OsString, dest_dir: &Path) -> Result<Option<String>, String> {
    match stage {
        Stage::Local(t) => {
            std::fs::rename(t.path().join(name), dest_dir.join(name)).map_err(|e| e.to_string())?;
            let path = t.path().to_string_lossy().into_owned();
            Ok(t.close().err().map(|_| left_behind(&path)))
        }
        Stage::Remote(s, p) => {
            let res = s.exec(&place_cmd(&p, &os_bytes(name), &b(dest_dir)));
            let out = res.map_err(|e| e.message())?;
            match out.code {
                Some(0) => Ok(None),
                Some(STAGE_LEFT) => Ok(Some(left_behind(&String::from_utf8_lossy(&p)))),
                _ => {
                    let e = code_err(&out);
                    match discard(Stage::Remote(s, p)) {
                        Some(w) => Err(format!("{e} · {w}")),
                        None => Err(e),
                    }
                }
            }
        }
    }
}

/// Remove a staging dir after a failed transfer; a warning when it stays.
fn discard(stage: Stage) -> Option<String> {
    let (ok, path) = match stage {
        Stage::Local(t) => {
            let p = t.path().to_string_lossy().into_owned();
            (t.close().is_ok(), p)
        }
        Stage::Remote(s, p) => {
            let ok = matches!(s.exec(&unstage_cmd(&p)), Ok(o) if o.code == Some(0));
            (ok, String::from_utf8_lossy(&p).into_owned())
        }
    };
    (!ok).then(|| left_behind(&path))
}

/// Copy `src` into `into` (a staging dir) over the route.
fn send(route: &Route, src: &Path, into: Endpoint, cancel: &AtomicBool) -> Result<(), String> {
    let sb = b(src);
    let scp = |s: &RemoteSession, from: Endpoint| {
        let argv = scp_argv(&from, &into, s.scp_mode)?;
        ran(s.scp_cancellable(&argv, cancel).map_err(|e| e.message())?).map(drop)
    };
    match route {
        Route::Up(d) => scp(d, Endpoint::Local(src)),
        Route::Down(s) | Route::Between(s) => scp(s, Endpoint::Remote(&s.target, &sb)),
        Route::SameHost(s) => {
            let Endpoint::Remote(_, dir) = into else {
                return Err("임시 폴더가 원격이 아님".into());
            };
            let cmd = cp_cmd(&sb, dir);
            ran(s.exec_cancellable(&cmd, cancel).map_err(|e| e.message())?).map(drop)
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

fn report(outcome: OpOutcome) -> OpReport {
    OpReport {
        outcome,
        ..OpReport::failed("")
    }
}

/// How one item ended.
enum Step {
    Done,
    Skipped(Option<String>),
    /// The copy is in place but the source stayed; the warning says why.
    Partial(String),
    Stop(OpOutcome),
}

fn lossy(n: &OsString) -> String {
    n.to_string_lossy().into_owned()
}

/// Transfer one item.
fn item(
    spec: &TransferSpec,
    route: &Route,
    name: &OsString,
    scanned: Scanned,
    ctl: &mut dyn TransferCtl,
    r: &mut OpReport,
) -> Step {
    let fail = |m: String| Step::Stop(OpOutcome::Failed(format!("{}: {m}", lossy(name))));
    let src = spec.src_dir.join(name);
    let dest = spec.dest_dir.join(name);
    if route.uses_scp() && scanned.has_link {
        return Step::Skipped(Some(format!(
            "심볼릭 링크가 있어 건너뜀 (scp는 링크를 따라감): {}",
            lossy(name)
        )));
    }
    match kind(&spec.dst, &dest) {
        Ok(Kind::Missing) => {}
        Ok(dk) => {
            let sk = match kind(&spec.src, &src) {
                Ok(k) => k,
                Err(e) => return fail(e),
            };
            if dk == Kind::Dir || sk == Kind::Dir {
                return Step::Skipped(Some(format!(
                    "폴더는 덮어쓸 수 없어 건너뜀: {}",
                    lossy(name)
                )));
            }
            match ctl.on_conflict(&dest) {
                ConflictAction::Overwrite => r.overwritten += 1,
                ConflictAction::Skip => return Step::Skipped(None),
                ConflictAction::Cancel => return Step::Stop(OpOutcome::Cancelled),
            }
        }
        Err(e) => return fail(e),
    }
    let is_move = spec.kind == JobKind::Move;
    if let (true, Route::SameHost(s)) = (is_move, route) {
        let cmd = mv_cmd(&b(&src), &b(&spec.dest_dir));
        return match s.exec_cancellable(&cmd, ctl.cancel_flag()) {
            Ok(o) if o.code == Some(0) => Step::Done,
            _ if ctl.is_cancelled() => Step::Stop(OpOutcome::Cancelled),
            Ok(o) => fail(code_err(&o)),
            Err(e) => fail(e.message()),
        };
    }
    let stage = match make_stage(&spec.dst, &spec.dest_dir) {
        Ok(st) => st,
        Err(e) => return fail(e),
    };
    if let Err(e) = send(route, &src, stage.endpoint(), ctl.cancel_flag()) {
        let cancelled = ctl.is_cancelled();
        r.warnings.extend(discard(stage));
        return if cancelled {
            Step::Stop(OpOutcome::Cancelled)
        } else {
            fail(e)
        };
    }
    match place(stage, name, &spec.dest_dir) {
        Ok(w) => r.warnings.extend(w),
        Err(e) => return fail(format!("제자리로 옮기지 못함: {e}")),
    }
    if !is_move {
        return Step::Done;
    }
    match kind(&spec.dst, &dest) {
        Ok(Kind::Missing) => fail("전송 후 대상이 없어 원본을 지우지 않음".into()),
        Err(e) => fail(format!("전송 확인 실패, 원본을 지우지 않음 ({e})")),
        Ok(_) => match delete_source(spec, name) {
            Ok(()) => Step::Done,
            Err(e) => Step::Partial(format!("원본을 지우지 못함: {} ({e})", lossy(name))),
        },
    }
}

/// Run the whole job (blocking; called on the job worker thread).
pub fn run(spec: &TransferSpec, ctl: &mut dyn TransferCtl) -> OpReport {
    let route = match route(spec) {
        Ok(r) => r,
        Err(e) => return OpReport::failed(e),
    };
    if matches!(route, Route::SameHost(_)) {
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
    if let Err(e) = dest_dir_ok(&spec.dst, &spec.dest_dir) {
        return OpReport::failed(e);
    }
    let scanned = match scan(spec, &route, ctl.cancel_flag()) {
        Ok(Some(v)) => v,
        Ok(None) => return report(OpOutcome::Cancelled),
        Err(e) => return OpReport::failed(e),
    };
    ctl.started(Totals {
        files: spec.names.len() as u64,
        bytes: scanned.iter().map(|s| s.bytes).sum(),
    });
    let mut r = report(OpOutcome::Done);
    for (name, &sc) in spec.names.iter().zip(&scanned) {
        if ctl.is_cancelled() {
            r.outcome = OpOutcome::Cancelled;
            break;
        }
        let src = spec.src_dir.join(name);
        ctl.on_progress(0, &src);
        match item(spec, &route, name, sc, ctl, &mut r) {
            Step::Done => {
                r.copied += 1;
                r.done_sources.push(src.clone());
            }
            Step::Skipped(w) => {
                r.skipped += 1;
                r.warnings.extend(w);
            }
            Step::Partial(w) => {
                r.partial += 1;
                r.warnings.push(w);
            }
            Step::Stop(o) => {
                r.outcome = o;
                break;
            }
        }
        ctl.on_progress(sc.bytes, &src);
        ctl.on_item_done(&src);
    }
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::job::{ConflictAnswer, ConflictPolicy};
    use crate::session::CANCELLED_MSG;
    use crate::testutil::{FakeCmd, code, fail, fake_mktemp, ok, remote_cmd, session};
    use crate::trash::FakeTrash;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex;

    // ---- command construction ----

    #[test]
    fn kind_and_dir_checks_quote_the_path() {
        assert_eq!(
            kind_cmd(b"/a b/it's"),
            b"if [ -L '/a b/it'\\''s' ]; then exit 45; elif [ -d '/a b/it'\\''s' ]; then exit 44; \
              elif [ -e '/a b/it'\\''s' ]; then exit 45; fi; exit 43"
                .to_vec()
        );
        assert_eq!(
            is_dir_cmd(b"/srv/x"),
            b"if [ -d '/srv/x' ]; then exit 0; fi; exit 43".to_vec()
        );
    }

    #[test]
    fn staging_commands_stay_inside_the_destination() {
        assert_eq!(
            stage_cmd(b"/dst/"),
            b"d=$(mktemp -d '/dst/.hdir-xfer.XXXXXX') || exit 1; printf '%s\\0' \"$d\"".to_vec()
        );
        assert_eq!(
            stage_cmd(b"/"),
            b"d=$(mktemp -d '/.hdir-xfer.XXXXXX') || exit 1; printf '%s\\0' \"$d\"".to_vec()
        );
        assert_eq!(
            place_cmd(b"/d/.hdir-xfer.1", b"a 'b'", b"/d"),
            b"mv -f -- '/d/.hdir-xfer.1/a '\\''b'\\''' '/d/a '\\''b'\\''' || exit 1; \
              rmdir -- '/d/.hdir-xfer.1' || exit 46"
                .to_vec()
        );
        assert_eq!(
            unstage_cmd(b"/d/.hdir-xfer.1"),
            b"rm -rf -- '/d/.hdir-xfer.1'".to_vec()
        );
        assert_eq!(
            cp_cmd(b"/a/x", b"/dst//"),
            b"cp -Rp -- '/a/x' '/dst/'".to_vec()
        );
        assert_eq!(mv_cmd(b"/a/-x", b"/"), b"mv -f -- '/a/-x' '/'".to_vec());
    }

    #[test]
    fn a_stage_path_must_be_a_fresh_child_of_the_destination() {
        let ok = |s: &[u8], d: &[u8]| parse_stage(s, d);
        assert_eq!(
            ok(b"/dst/.hdir-xfer.Ab12Cd\0", b"/dst"),
            Some(b"/dst/.hdir-xfer.Ab12Cd".to_vec())
        );
        assert_eq!(
            ok(b"/.hdir-xfer.x\0", b"/"),
            Some(b"/.hdir-xfer.x".to_vec())
        );
        for bad in [
            &b"/etc\0"[..],
            b"/dst/.hdir-xfer.\0",
            b"/dst/.hdir-xfer.a/b\0",
            b"/dst/sub/.hdir-xfer.a\0",
            b"/dst/.hdir-xfer.a",
            b"",
        ] {
            assert_eq!(ok(bad, b"/dst"), None, "{bad:?}");
        }
    }

    #[test]
    fn scan_output_needs_one_record_per_name() {
        assert_eq!(
            parse_scan(b"1\x000\x002\x001\x00", 2),
            Some(vec![
                Scanned {
                    bytes: 1024,
                    has_link: false
                },
                Scanned {
                    bytes: 2048,
                    has_link: true
                }
            ])
        );
        assert_eq!(parse_scan(b"x\x000\x00", 1).unwrap()[0].bytes, 0);
        assert_eq!(parse_scan(b"1\x000\x00", 2), None);
        assert_eq!(parse_scan(b"1\x00y\x00", 1), None);
        assert_eq!(parse_scan(b"1\x000", 1), None);
        assert_eq!(parse_scan(b"", 0), Some(vec![]));
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
    fn real_scan_finds_sizes_and_links_with_odd_names() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a\nb"), vec![0u8; 5000]).unwrap();
        fs::create_dir(t.path().join("-d")).unwrap();
        fs::write(t.path().join("-d/f"), vec![1u8; 9000]).unwrap();
        std::os::unix::fs::symlink("/etc", t.path().join("-d/ln")).unwrap();
        std::os::unix::fs::symlink("a\nb", t.path().join("top")).unwrap();
        let names: [&[u8]; 4] = [b"a\nb", b"-d", b"missing", b"top"];
        let out = sh(&scan_cmd(&b(t.path()), &names));
        assert!(out.status.success(), "{out:?}");
        let v = parse_scan(&out.stdout, 4).expect("parsed");
        assert!(v[0].bytes >= 4096 && !v[0].has_link, "{v:?}");
        assert!(v[1].bytes >= 8192 && v[1].has_link, "{v:?}");
        assert_eq!(v[2], Scanned::default());
        assert!(v[3].has_link, "a top-level link counts");
    }

    #[test]
    fn real_kind_check_does_not_follow_links() {
        let t = tempfile::tempdir().unwrap();
        let code_of = |p: &Path| sh(&kind_cmd(&b(p))).status.code();
        std::os::unix::fs::symlink("/nonexistent/x", t.path().join("dangling")).unwrap();
        std::os::unix::fs::symlink(t.path(), t.path().join("to-dir")).unwrap();
        fs::write(t.path().join("f"), "x").unwrap();
        assert_eq!(code_of(&t.path().join("dangling")), Some(IS_OTHER));
        assert_eq!(code_of(&t.path().join("to-dir")), Some(IS_OTHER));
        assert_eq!(code_of(&t.path().join("f")), Some(IS_OTHER));
        assert_eq!(code_of(t.path()), Some(IS_DIR));
        assert_eq!(code_of(&t.path().join("gone 'q'")), Some(NOT_THERE));
        assert_eq!(
            sh(&is_dir_cmd(&b(&t.path().join("to-dir")))).status.code(),
            Some(0)
        );
    }

    #[test]
    fn real_staging_replaces_a_link_instead_of_writing_through_it() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(t.path().join("outside"), "keep").unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), dest.join("it's")).unwrap();
        let out = sh(&stage_cmd(&b(&dest)));
        let stage = parse_stage(&out.stdout, &b(&dest)).expect("stage path");
        let stage_p = PathBuf::from(std::ffi::OsString::from(
            String::from_utf8(stage.clone()).unwrap(),
        ));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&stage_p).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(stage_p.join("it's"), "new").unwrap();
        assert!(sh(&place_cmd(&stage, b"it's", &b(&dest))).status.success());
        assert_eq!(fs::read(dest.join("it's")).unwrap(), b"new");
        assert!(
            !fs::symlink_metadata(dest.join("it's"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(t.path().join("outside")).unwrap(), b"keep");
        assert!(!stage_p.exists());
        // a discarded stage goes with its content
        let out = sh(&stage_cmd(&b(&dest)));
        let stage = parse_stage(&out.stdout, &b(&dest)).unwrap();
        let p = PathBuf::from(String::from_utf8(stage.clone()).unwrap());
        fs::create_dir(p.join("half")).unwrap();
        fs::write(p.join("half/x"), "x").unwrap();
        assert!(sh(&unstage_cmd(&stage)).status.success());
        assert!(!p.exists());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
    }

    #[test]
    fn real_same_host_copy_into_a_stage_keeps_links_as_links() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        fs::create_dir_all(src.join("d")).unwrap();
        std::os::unix::fs::symlink("/etc", src.join("d/ln")).unwrap();
        let st = t.path().join("st");
        fs::create_dir(&st).unwrap();
        assert!(sh(&cp_cmd(&b(&src.join("d")), &b(&st))).status.success());
        let m = fs::symlink_metadata(st.join("d/ln")).unwrap();
        assert!(m.file_type().is_symlink());
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
            Self::answering(&[])
        }
        fn answering(a: &[ConflictAnswer]) -> Self {
            Self {
                cancel: Arc::new(AtomicBool::new(false)),
                answers: a.iter().copied().collect(),
                policy: ConflictPolicy::default(),
                conflicts: vec![],
                totals: None,
                bytes: 0,
                items: vec![],
            }
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
            names: n.iter().map(OsString::from).collect(),
            dst,
            dest_dir: dest.to_path_buf(),
            trash: Arc::new(FakeTrash::new()),
        }
    }

    /// The single-quoted path right after `prefix` in `c`.
    fn quoted_after<'a>(c: &'a str, prefix: &str) -> Option<&'a str> {
        c.strip_prefix(prefix)?.split_once('\'').map(|(p, _)| p)
    }

    /// A fake scp: a download writes `new` into the local target dir.
    fn fake_scp(a: &[String]) -> CmdOutput {
        let (src, dst) = (&a[a.len() - 2], &a[a.len() - 1]);
        if !dst.starts_with("prod-web:") && !dst.starts_with("other-host:") {
            let name = Path::new(src.split_once(':').map_or(src.as_str(), |x| x.1))
                .file_name()
                .unwrap()
                .to_owned();
            fs::write(Path::new(dst).join(name), "new").unwrap();
        }
        ok("")
    }

    /// Remote answers: `dirs` / `files` exist (as directory / file), the
    /// destination folders `/dst` and `/srv` exist, the scan answers `scan`.
    fn answer(c: &str, dirs: &[&str], files: &[&str], scan: &str) -> CmdOutput {
        if c.starts_with("if [ -d '") {
            return code(0);
        }
        if let Some(path) = quoted_after(c, "if [ -L '") {
            return code(if dirs.contains(&path) {
                IS_DIR
            } else if files.contains(&path) {
                IS_OTHER
            } else {
                NOT_THERE
            });
        }
        if let Some(o) = fake_mktemp(c) {
            return o;
        }
        if c.starts_with("cd -- ") {
            return ok(scan);
        }
        ok("")
    }

    fn host(
        dirs: &'static [&'static str],
        files: &'static [&'static str],
        scan: &'static str,
    ) -> Arc<FakeCmd> {
        FakeCmd::new(move |a| {
            if a[0] == "scp" {
                return fake_scp(a);
            }
            answer(remote_cmd(a).unwrap_or_default(), dirs, files, scan)
        })
    }

    fn scp_pairs(f: &FakeCmd) -> Vec<(String, String)> {
        f.scp_calls()
            .iter()
            .map(|a| (a[a.len() - 2].clone(), a[a.len() - 1].clone()))
            .collect()
    }

    fn stages_in(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGE_PREFIX))
            .count()
    }

    const STAGE: &str = "/dst/.hdir-xfer.ABC123";

    #[test]
    fn upload_goes_through_a_stage_and_is_renamed_into_place() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "12345").unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("d/x"), "123").unwrap();
        let f = host(&[], &[], "");
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
        let to = format!("prod-web:{STAGE}");
        assert_eq!(scp_pairs(&f), vec![(a, to.clone()), (d, to)]);
        assert!(f.scp_calls()[0].contains(&"-rp".to_string()));
        let place = String::from_utf8(place_cmd(STAGE.as_bytes(), b"a", b"/dst")).unwrap();
        assert!(f.remote_cmds().contains(&place), "{:?}", f.remote_cmds());
    }

    #[test]
    fn download_replaces_an_existing_link_and_never_writes_through_it() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fs::write(t.path().join("outside"), "keep").unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), dest.join("a")).unwrap();
        let f = host(&[], &[], "1\x000\x00");
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            &dest,
        );
        let mut c = Ctl::answering(&[ConflictAnswer::Overwrite]);
        let r = run(&s, &mut c);
        assert_eq!(
            (r.outcome, r.copied, r.overwritten),
            (OpOutcome::Done, 1, 1)
        );
        assert_eq!(
            c.totals,
            Some(Totals {
                files: 1,
                bytes: 1024
            })
        );
        assert_eq!(fs::read(dest.join("a")).unwrap(), b"new");
        assert!(
            !fs::symlink_metadata(dest.join("a"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(t.path().join("outside")).unwrap(), b"keep");
        assert_eq!(stages_in(&dest), 0);
        assert!(
            scp_pairs(&f)[0].1.contains("/.hdir-xfer."),
            "{:?}",
            scp_pairs(&f)
        );
    }

    #[test]
    fn esc_during_a_download_keeps_the_old_file_and_leaves_no_stage() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "old").unwrap();
        let c = Ctl::answering(&[ConflictAnswer::Overwrite]);
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                let dst = &a[a.len() - 1];
                fs::write(Path::new(dst).join("a"), "ne").unwrap(); // half done
                flag.store(true, Ordering::SeqCst);
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: CANCELLED_MSG.as_bytes().to_vec(),
                };
            }
            answer(
                remote_cmd(a).unwrap_or_default(),
                &[],
                &[],
                "0\x000\x000\x000\x00",
            )
        });
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a", "b"],
            Side::Local,
            t.path(),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(fs::read(t.path().join("a")).unwrap(), b"old");
        assert_eq!(stages_in(t.path()), 0);
        assert_eq!(f.scp_calls().len(), 1, "b never starts");
        assert!(
            !f.remote_cmds().iter().any(|c| c.contains("rm -rf")),
            "source kept"
        );
    }

    #[test]
    fn esc_during_an_upload_removes_the_remote_stage() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                flag.store(true, Ordering::SeqCst);
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: CANCELLED_MSG.as_bytes().to_vec(),
                };
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "")
        });
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert_eq!(f.remote_cmds().last(), Some(&unstage));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn a_stage_that_cannot_be_removed_is_named() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return fail(1, "scp: write failed\n");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("rm -rf") {
                return fail(1, "busy\n");
            }
            answer(c, &[], &[], "")
        });
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Failed("a: scp: write failed".into()));
        assert_eq!(r.warnings, vec![format!("임시 폴더가 남음: {STAGE}")]);
    }

    #[test]
    fn items_with_links_are_skipped_over_scp() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        std::os::unix::fs::symlink("/etc", t.path().join("d/secret")).unwrap();
        fs::write(t.path().join("ok"), "x").unwrap();
        let f = host(&[], &[], "");
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["d", "ok"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.copied, r.skipped), (OpOutcome::Done, 1, 1));
        assert_eq!(
            r.warnings,
            vec!["심볼릭 링크가 있어 건너뜀 (scp는 링크를 따라감): d".to_string()]
        );
        assert_eq!(f.scp_calls().len(), 1);
        // the same for a remote source the scan reports a link in
        let f = host(&[], &[], "0\x001\x00");
        let back = tempfile::tempdir().unwrap();
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["d"],
            Side::Local,
            back.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.copied, r.skipped), (0, 1));
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn a_remote_scan_that_cannot_be_read_stops_before_sending() {
        let f = host(&[], &[], "garbage");
        let t = tempfile::tempdir().unwrap();
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("원격 항목을 확인할 수 없음".into())
        );
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn folder_conflicts_are_skipped_without_asking() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("f"), "x").unwrap();
        let f = host(&["/dst/d", "/dst/f"], &[], "");
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["d", "f"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = Ctl::new();
        let r = run(&s, &mut c);
        assert_eq!((r.outcome, r.skipped), (OpOutcome::Done, 2));
        assert!(c.conflicts.is_empty());
        assert_eq!(
            r.warnings,
            vec![
                "폴더는 덮어쓸 수 없어 건너뜀: d".to_string(),
                "폴더는 덮어쓸 수 없어 건너뜀: f".to_string()
            ]
        );
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn file_conflicts_ask_and_follow_the_answer() {
        let t = tempfile::tempdir().unwrap();
        for n in ["a", "b", "c"] {
            fs::write(t.path().join(n), "old").unwrap();
        }
        let f = host(&[], &[], "0\x000\x000\x000\x000\x000\x000\x000\x00");
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
        let read = |n: &str| fs::read_to_string(t.path().join(n)).unwrap();
        assert_eq!(
            (read("a"), read("b"), read("c"), read("new")),
            ("old".into(), "new".into(), "old".into(), "new".into())
        );
        assert_eq!(c.items.len(), 4, "skipped items still advance progress");
    }

    #[test]
    fn a_conflict_cancelled_stops_before_any_transfer() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = host(&[], &["/dst/a"], "");
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
    fn same_server_copy_stages_and_move_is_one_rename_without_a_scan() {
        let f = host(&[], &[], "0\x001\x00");
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Copy,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["a"],
            Side::Remote(Arc::clone(&sess)),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.copied),
            (OpOutcome::Done, 1),
            "links are fine for cp"
        );
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let before = f.remote_cmds().len();
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.copied), (OpOutcome::Done, 1));
        assert!(f.scp_calls().is_empty());
        let long: Vec<String> = f
            .long_cmds()
            .into_iter()
            .filter(|c| !c.starts_with("cd -- "))
            .collect();
        let cp = format!("cp -Rp -- '/srv/a' '{STAGE}/'");
        assert_eq!(long, vec![cp, "mv -f -- '/srv/b' '/dst/'".to_string()]);
        let moved: Vec<String> = f.remote_cmds()[before..].to_vec();
        assert!(!moved.iter().any(|c| c.contains("du -sk")), "{moved:?}");
        assert!(!moved.iter().any(|c| c.contains("rm -rf")), "{moved:?}");
    }

    #[test]
    fn two_servers_go_through_scp_3_into_the_destination_stage() {
        let fs_ = host(&[], &[], "0\x000\x00");
        let fd = host(&[], &[], "");
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
            Side::Remote(session(Arc::clone(&fs_))),
            &p("/srv"),
            &["a"],
            Side::Remote(Arc::new(dst)),
            &p("/dst"),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let calls = fs_.scp_calls();
        assert!(calls[0].contains(&"-3".to_string()), "{calls:?}");
        assert_eq!(
            scp_pairs(&fs_),
            vec![("prod-web:/srv/a".into(), format!("other-host:{STAGE}"))]
        );
        let place = String::from_utf8(place_cmd(STAGE.as_bytes(), b"a", b"/dst")).unwrap();
        assert!(fd.remote_cmds().contains(&place));
    }

    #[test]
    fn a_missing_destination_folder_fails_before_anything_runs() {
        let f = FakeCmd::new(|_| code(NOT_THERE));
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
        assert_eq!(f.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_same_folder_or_a_folder_into_itself_is_refused_on_one_host() {
        let f = host(&[], &[], "");
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
        assert_eq!(one("/srv/dd"), OpOutcome::Done);
    }

    #[test]
    fn a_stage_reply_outside_the_destination_is_never_used() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("mktemp -d") {
                return ok("/etc\0");
            }
            answer(c, &[], &[], "")
        });
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("a: 임시 폴더 경로를 알 수 없음".into())
        );
        assert!(f.scp_calls().is_empty());
        assert!(!f.remote_cmds().iter().any(|c| c.contains("rm -rf")));
    }

    #[test]
    fn a_failed_scp_stops_the_job_and_removes_the_stage() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return fail(1, "scp: /dst/a: Permission denied\n");
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "")
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
        assert_eq!((f.scp_calls().len(), r.copied), (1, 0));
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert_eq!(f.remote_cmds().last(), Some(&unstage));
    }

    #[test]
    fn a_dropped_connection_is_reported_as_such() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return code(255);
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "")
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
    fn a_cancel_after_one_item_stops_before_the_next() {
        let t = tempfile::tempdir().unwrap();
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                flag.store(true, Ordering::SeqCst); // Esc right after it finished
                return ok("");
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "")
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

    /// Remote `[ -L` answers: missing until `scp` ran, then a file.
    fn appears_after_scp() -> Arc<FakeCmd> {
        let sent = Arc::new(AtomicBool::new(false));
        FakeCmd::new(move |a| {
            if a[0] == "scp" {
                sent.store(true, Ordering::SeqCst);
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("if [ -L '") {
                return code(if sent.load(Ordering::SeqCst) {
                    IS_OTHER
                } else {
                    NOT_THERE
                });
            }
            answer(c, &[], &[], "")
        })
    }

    #[test]
    fn moving_up_trashes_the_local_source_only_after_the_copy_is_in_place() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = appears_after_scp();
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
        let f = host(&[], &[], "");
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
        assert!(
            matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("원본을 지우지 않음")),
            "{:?}",
            r.outcome
        );
        assert!(trash.trashed.lock().unwrap().is_empty());
    }

    #[test]
    fn moving_down_deletes_the_remote_source_after_the_copy_landed() {
        let t = tempfile::tempdir().unwrap();
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let o2 = Arc::clone(&order);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                o2.lock().unwrap().push("scp".into());
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("rm -rf") {
                o2.lock().unwrap().push("rm".into());
            }
            answer(c, &[], &[], "0\x000\x00")
        });
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.copied), (OpOutcome::Done, 1));
        assert_eq!(fs::read(t.path().join("a")).unwrap(), b"new");
        assert_eq!(*order.lock().unwrap(), vec!["scp", "rm"]);
        let rm: Vec<String> = f
            .remote_cmds()
            .into_iter()
            .filter(|c| c.contains("rm -rf"))
            .collect();
        assert!(rm[0].contains("'a'"), "{}", rm[0]);
    }

    #[test]
    fn a_source_that_cannot_be_deleted_is_a_partial_move_with_a_warning() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = appears_after_scp();
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
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Done);
        assert_eq!((r.copied, r.partial), (0, 1));
        assert!(r.done_sources.is_empty(), "the source stays selected");
        assert_eq!(r.warnings, vec!["원본을 지우지 못함: a (busy)".to_string()]);
    }
}
