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
use crate::remote::{Endpoint, RemoteOs, UNSAFE_DIR, safe_dirs_check, scp_argv, sh_quote};
use crate::remote_ops;
use crate::session::{CmdOutput, RemoteSession, first_line};
use crate::trash::TrashBackend;

/// Exit codes of the remote helper commands.
pub const NOT_THERE: i32 = 43;
pub const IS_DIR: i32 = 44;
pub const IS_OTHER: i32 = 45;
/// [`place_cmd`]: the item is in place but the staging dir could not go.
pub const STAGE_LEFT: i32 = 46;
/// [`rename_over_cmd`]: a real directory appeared at the target.
pub const DIR_APPEARED: i32 = 47;

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

/// Create a private (mode 700) staging dir in the physical `dest_dir`,
/// after checking that nobody else can swap entries in it or any parent
/// ([`safe_dirs_check`], exit [`UNSAFE_DIR`]). Prints the physical
/// directory and the staging path, each NUL-terminated.
pub fn stage_cmd(os: &RemoteOs, dest_dir: &[u8]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dest_dir),
        b" || exit 1; d0=$(pwd -P && echo x) || exit 1; d0=${d0%?x}; ",
        &safe_dirs_check(os, b"\"$d0\""),
        b"case $d0 in /) t=/",
        STAGE_PREFIX.as_bytes(),
        b"XXXXXX;; *) t=$d0/",
        STAGE_PREFIX.as_bytes(),
        b"XXXXXX;; esac; d=$(mktemp -d \"$t\") || exit 1; printf '%s\\0%s\\0' \"$d0\" \"$d\"",
    ])
}

/// The physical destination and staging path printed by [`stage_cmd`]. The
/// staging path must be a direct child of that directory with the staging
/// prefix (anything else is refused, so a confused reply can never become
/// an `rm -rf` target).
pub fn parse_stage(stdout: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut f = stdout.split(|&b| b == 0);
    let (dir, stage) = (f.next()?, f.next()?);
    if f.next() != Some(&b""[..]) || f.next().is_some() || dir.first() != Some(&b'/') {
        return None;
    }
    let prefix = cat(&[&into_dir(dir), STAGE_PREFIX.as_bytes()]);
    let rest = stage.strip_prefix(prefix.as_slice())?;
    (!rest.is_empty() && !rest.contains(&b'/')).then(|| (dir.to_vec(), stage.to_vec()))
}

/// Rename `from` onto `to` without ever following `to`: a symlink there
/// (even one to a directory) is replaced as a link, a real directory that
/// appeared meanwhile is refused ([`DIR_APPEARED`]).
pub fn rename_over_cmd(from: &[u8], to: &[u8]) -> Vec<u8> {
    let t = sh_quote(to);
    cat(&[
        b"if [ -L ",
        &t,
        b" ]; then if [ -d ",
        &t,
        b" ]; then rm -f -- ",
        &t,
        b" || exit 1; fi; elif [ -d ",
        &t,
        b" ]; then exit 47; fi; mv -f -- ",
        &sh_quote(from),
        b" ",
        &t,
    ])
}

/// Rename `stage/name` onto `dir/name` ([`rename_over_cmd`]), then remove
/// the empty staging dir ([`STAGE_LEFT`] when that fails).
pub fn place_cmd(stage: &[u8], name: &[u8], dir: &[u8]) -> Vec<u8> {
    let from = cat(&[&into_dir(stage), name]);
    let to = cat(&[&into_dir(dir), name]);
    cat(&[
        &rename_over_cmd(&from, &to),
        b" || exit $?; rmdir -- ",
        &sh_quote(stage),
        b" || exit 46",
    ])
}

/// `dev:inode` of `path` itself (not following a final symlink).
pub fn identity_cmd(os: &RemoteOs, path: &[u8]) -> Vec<u8> {
    let fmt: &[u8] = match os {
        RemoteOs::Bsd => b"stat -f '%d:%i' -- ",
        RemoteOs::Linux | RemoteOs::Other(_) => b"stat -c '%d:%i' -- ",
    };
    cat(&[fmt, &sh_quote(path)])
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

// ---- running ----

enum Route<'a> {
    Up(&'a RemoteSession),
    Down(&'a RemoteSession),
    SameHost(&'a RemoteSession),
    Between(&'a RemoteSession, &'a RemoteSession),
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
        (Some(s), Some(d)) => Route::Between(s, d),
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
    /// Session, physical destination dir, staging dir.
    Remote(&'a RemoteSession, Vec<u8>, Vec<u8>),
}

impl Stage<'_> {
    fn endpoint(&self) -> Endpoint<'_> {
        match self {
            Stage::Local(t) => Endpoint::Local(t.path()),
            Stage::Remote(s, _, p) => Endpoint::Remote(&s.target, p),
        }
    }
}

/// Shown when the destination (or a parent) could be tampered with.
pub const UNSAFE_MSG: &str = "다른 사용자가 바꿀 수 있는 폴더라 보내지 않음";

/// Refuse a local destination where someone else could swap the staging
/// dir: it or a parent is not owned by the user or root, or is writable by
/// others or by a group other than the user's own private group (gid =
/// uid), without being sticky.
#[cfg(unix)]
fn local_dest_safe(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(dir).map_err(|e| e.to_string())?;
    // SAFETY: plain id getters without preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    for a in real.ancestors() {
        let m = std::fs::metadata(a).map_err(|e| e.to_string())?;
        let mode = m.mode();
        let unsafe_dir = if m.uid() != uid && m.uid() != 0 {
            true
        } else if mode & 0o1000 != 0 {
            false
        } else {
            mode & 0o002 != 0 || (mode & 0o020 != 0 && !(m.gid() == gid && gid == uid))
        };
        if unsafe_dir {
            return Err(UNSAFE_MSG.into());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn local_dest_safe(_dir: &Path) -> Result<(), String> {
    Ok(())
}

fn make_stage<'a>(dst: &'a Side, dest_dir: &Path) -> Result<Stage<'a>, String> {
    match dst {
        Side::Local => {
            local_dest_safe(dest_dir)?;
            tempfile::Builder::new()
                .prefix(STAGE_PREFIX)
                .tempdir_in(dest_dir)
                .map(Stage::Local)
                .map_err(|e| format!("임시 폴더를 만들 수 없음: {e}"))
        }
        Side::Remote(s) => {
            let out = s
                .exec(&stage_cmd(&s.os, &b(dest_dir)))
                .map_err(|e| e.message())?;
            if out.code == Some(UNSAFE_DIR) {
                return Err(UNSAFE_MSG.into());
            }
            let out = ran(out).map_err(|e| format!("임시 폴더를 만들 수 없음: {e}"))?;
            parse_stage(&out.stdout)
                .map(|(dir, p)| Stage::Remote(s, dir, p))
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
            // rename(2) replaces a file or symlink at the target, never
            // following it; a real directory there makes it fail.
            std::fs::rename(t.path().join(name), dest_dir.join(name)).map_err(|e| e.to_string())?;
            let path = t.path().to_string_lossy().into_owned();
            Ok(t.close().err().map(|_| left_behind(&path)))
        }
        Stage::Remote(s, dir, p) => {
            let res = s.exec(&place_cmd(&p, &os_bytes(name), &dir));
            let out = res.map_err(|e| e.message())?;
            match out.code {
                Some(0) => Ok(None),
                Some(STAGE_LEFT) => Ok(Some(left_behind(&String::from_utf8_lossy(&p)))),
                c => {
                    let e = if c == Some(DIR_APPEARED) {
                        "대상에 폴더가 생겨 옮기지 않음".to_string()
                    } else {
                        code_err(&out)
                    };
                    match discard(Stage::Remote(s, dir, p)) {
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
        Stage::Remote(s, _, p) => {
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
        Route::Down(s) | Route::Between(s, _) => scp(s, Endpoint::Remote(&s.target, &sb)),
        Route::SameHost(s) => {
            let Endpoint::Remote(_, dir) = into else {
                return Err("임시 폴더가 원격이 아님".into());
            };
            let cmd = cp_cmd(&sb, dir);
            ran(s.exec_cancellable(&cmd, cancel).map_err(|e| e.message())?).map(drop)
        }
    }
}

/// `dev:inode` of `path` on its side (unix only locally); `None` when it
/// cannot be told.
fn identity(side: &Side, path: &Path) -> Option<String> {
    match side {
        Side::Local => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let m = std::fs::symlink_metadata(path).ok()?;
                Some(format!("{}:{}", m.dev(), m.ino()))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                None
            }
        }
        Side::Remote(s) => {
            let out = s.exec(&identity_cmd(&s.os, &b(path))).ok()?;
            let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (out.code == Some(0) && !id.is_empty()).then_some(id)
        }
    }
}

/// Symlink check of one item right before it is sent (the source may have
/// changed while a conflict was being answered).
fn has_link_now(spec: &TransferSpec, name: &OsString, cancel: &AtomicBool) -> Result<bool, String> {
    match &spec.src {
        Side::Local => Ok(scan_local(&spec.src_dir.join(name), cancel).is_none_or(|s| s.has_link)),
        Side::Remote(s) => {
            let n = os_bytes(name).into_owned();
            let out = s
                .exec_cancellable(&scan_cmd(&b(&spec.src_dir), &[&n]), cancel)
                .map_err(|e| e.message())?;
            let out = ran(out)?;
            parse_scan(&out.stdout, 1)
                .map(|v| v[0].has_link)
                .ok_or_else(|| "원격 항목을 확인할 수 없음".into())
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
    let mut overwrite = false;
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
                ConflictAction::Overwrite => overwrite = true,
                ConflictAction::Skip => return Step::Skipped(None),
                ConflictAction::Cancel => return Step::Stop(OpOutcome::Cancelled),
            }
        }
        Err(e) => return fail(e),
    }
    let is_move = spec.kind == JobKind::Move;
    if let (true, Route::SameHost(s)) = (is_move, route) {
        let cmd = rename_over_cmd(&b(&src), &b(&dest));
        return match s.exec_cancellable(&cmd, ctl.cancel_flag()) {
            Ok(o) if o.code == Some(0) => {
                r.overwritten += usize::from(overwrite);
                Step::Done
            }
            _ if ctl.is_cancelled() => Step::Stop(OpOutcome::Cancelled),
            Ok(o) if o.code == Some(DIR_APPEARED) => fail("대상에 폴더가 생겨 옮기지 않음".into()),
            Ok(o) => fail(code_err(&o)),
            Err(e) => fail(e.message()),
        };
    }
    if route.uses_scp() {
        match has_link_now(spec, name, ctl.cancel_flag()) {
            Ok(false) => {}
            _ if ctl.is_cancelled() => return Step::Stop(OpOutcome::Cancelled),
            Ok(true) => {
                return Step::Skipped(Some(format!(
                    "심볼릭 링크가 있어 건너뜀 (scp는 링크를 따라감): {}",
                    lossy(name)
                )));
            }
            Err(e) => return fail(e),
        }
    }
    // A move over an existing target: if both are the same file (one host
    // reached twice, ssh to localhost), placing the copy replaces the
    // source, and deleting "the source" would delete the only copy.
    let src_id = (is_move && overwrite).then(|| identity(&spec.src, &src));
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
    r.overwritten += usize::from(overwrite);
    if !is_move {
        return Step::Done;
    }
    match kind(&spec.dst, &dest) {
        Ok(Kind::Missing) => return fail("전송 후 대상이 없어 원본을 지우지 않음".into()),
        Err(e) => return fail(format!("전송 확인 실패, 원본을 지우지 않음 ({e})")),
        Ok(_) => {}
    }
    if let Some(before) = src_id
        && (before.is_none() || identity(&spec.src, &src) != before)
    {
        return Step::Partial(format!(
            "원본과 대상이 같은 파일일 수 있어 원본을 지우지 않음: {}",
            lossy(name)
        ));
    }
    match delete_source(spec, name) {
        Ok(()) => Step::Done,
        Err(e) => Step::Partial(format!("원본을 지우지 못함: {} ({e})", lossy(name))),
    }
}

/// Run the whole job (blocking; called on the job worker thread).
pub fn run(spec: &TransferSpec, ctl: &mut dyn TransferCtl) -> OpReport {
    let route = match route(spec) {
        Ok(r) => r,
        Err(e) => return OpReport::failed(e),
    };
    let same_target = match route {
        Route::SameHost(_) => true,
        Route::Between(a, b) => a.target.host == b.target.host && a.target.port == b.target.port,
        _ => false,
    };
    if same_target && spec.dest_dir == spec.src_dir {
        return OpReport::failed("같은 폴더로는 보낼 수 없음");
    }
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
        let st = String::from_utf8(stage_cmd(&RemoteOs::Linux, b"/dst/it's")).unwrap();
        assert!(
            st.starts_with(
                "cd -- '/dst/it'\\''s' || exit 1; d0=$(pwd -P && echo x) || exit 1; d0=${d0%?x}; me=$(id -u)"
            ),
            "{st}"
        );
        assert!(st.contains("c=\"$d0\"; while :; do chk \"$c\";"), "{st}");
        assert!(
            st.ends_with(
                "case $d0 in /) t=/.hdir-xfer.XXXXXX;; *) t=$d0/.hdir-xfer.XXXXXX;; esac; \
                 d=$(mktemp -d \"$t\") || exit 1; printf '%s\\0%s\\0' \"$d0\" \"$d\""
            ),
            "{st}"
        );
        let q = "'/d/a '\\''b'\\'''";
        assert_eq!(
            String::from_utf8(rename_over_cmd(b"/s/a", b"/d/a 'b'")).unwrap(),
            format!(
                "if [ -L {q} ]; then if [ -d {q} ]; then rm -f -- {q} || exit 1; fi; \
                 elif [ -d {q} ]; then exit 47; fi; mv -f -- '/s/a' {q}"
            )
        );
        let pl = String::from_utf8(place_cmd(b"/d/.hdir-xfer.1", b"x", b"/d")).unwrap();
        assert!(pl.starts_with("if [ -L '/d/x' ]"), "{pl}");
        assert!(
            pl.ends_with(
                "mv -f -- '/d/.hdir-xfer.1/x' '/d/x' || exit $?; rmdir -- '/d/.hdir-xfer.1' || exit 46"
            ),
            "{pl}"
        );
        assert_eq!(
            unstage_cmd(b"/d/.hdir-xfer.1"),
            b"rm -rf -- '/d/.hdir-xfer.1'".to_vec()
        );
        assert_eq!(
            cp_cmd(b"/a/x", b"/dst//"),
            b"cp -Rp -- '/a/x' '/dst/'".to_vec()
        );
        assert_eq!(
            identity_cmd(&RemoteOs::Linux, b"/a b"),
            b"stat -c '%d:%i' -- '/a b'".to_vec()
        );
        assert_eq!(
            identity_cmd(&RemoteOs::Bsd, b"/a"),
            b"stat -f '%d:%i' -- '/a'".to_vec()
        );
    }

    #[test]
    fn a_stage_path_must_be_a_fresh_child_of_the_reported_directory() {
        let pair = |d: &[u8], s: &[u8]| Some((d.to_vec(), s.to_vec()));
        assert_eq!(
            parse_stage(b"/real/dst\0/real/dst/.hdir-xfer.Ab12Cd\0"),
            pair(b"/real/dst", b"/real/dst/.hdir-xfer.Ab12Cd")
        );
        assert_eq!(
            parse_stage(b"/\0/.hdir-xfer.x\0"),
            pair(b"/", b"/.hdir-xfer.x")
        );
        for bad in [
            &b"/dst\0/etc\0"[..],
            b"/dst\0/dst/.hdir-xfer.\0",
            b"/dst\0/dst/.hdir-xfer.a/b\0",
            b"/dst\0/dst/sub/.hdir-xfer.a\0",
            b"/dst\0/dst/.hdir-xfer.a",
            b"/dst\0/dst/.hdir-xfer.a\0junk\0",
            b"dst\0dst/.hdir-xfer.a\0",
            b"",
        ] {
            assert_eq!(parse_stage(bad), None, "{bad:?}");
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

    /// A private directory (0700 whatever the umask) inside `t`.
    fn private_dir(t: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let d = t.join(name);
        fs::create_dir(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn stage_in(dest: &Path) -> (Vec<u8>, Vec<u8>, PathBuf) {
        let out = sh(&stage_cmd(&crate::testutil::local_os(), &b(dest)));
        assert!(out.status.success(), "{out:?}");
        let (dir, stage) = parse_stage(&out.stdout).expect("stage path");
        let p = PathBuf::from(String::from_utf8(stage.clone()).unwrap());
        (dir, stage, p)
    }

    #[test]
    fn real_staging_replaces_links_instead_of_writing_through_them() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        fs::write(t.path().join("outside"), "keep").unwrap();
        let outside_dir = private_dir(t.path(), "outside-dir");
        std::os::unix::fs::symlink(t.path().join("outside"), dest.join("it's")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, dest.join("to-dir")).unwrap();
        for name in ["it's", "to-dir"] {
            let (dir, stage, sp) = stage_in(&dest);
            assert_eq!(dir, b(&fs::canonicalize(&dest).unwrap()));
            assert_eq!(
                fs::metadata(&sp).unwrap().permissions().mode() & 0o777,
                0o700
            );
            fs::write(sp.join(name), "new").unwrap();
            let out = sh(&place_cmd(&stage, name.as_bytes(), &dir));
            assert!(out.status.success(), "{name}: {out:?}");
            let m = fs::symlink_metadata(dest.join(name)).unwrap();
            assert!(m.is_file(), "{name} is a plain file now");
            assert_eq!(fs::read(dest.join(name)).unwrap(), b"new");
            assert!(!sp.exists());
        }
        assert_eq!(fs::read(t.path().join("outside")).unwrap(), b"keep");
        assert_eq!(
            fs::read_dir(&outside_dir).unwrap().count(),
            0,
            "nothing moved into the linked dir"
        );
        // a real directory at the target is never moved into
        fs::create_dir(dest.join("real")).unwrap();
        let (dir, stage, sp) = stage_in(&dest);
        fs::write(sp.join("real"), "new").unwrap();
        assert_eq!(
            sh(&place_cmd(&stage, b"real", &dir)).status.code(),
            Some(DIR_APPEARED)
        );
        assert_eq!(fs::read_dir(dest.join("real")).unwrap().count(), 0);
        // a discarded stage goes with its content
        fs::create_dir(sp.join("half")).unwrap();
        fs::write(sp.join("half/x"), "x").unwrap();
        assert!(sh(&unstage_cmd(&stage)).status.success());
        assert!(!sp.exists());
    }

    #[test]
    fn real_staging_is_refused_where_others_can_swap_it() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let open = private_dir(t.path(), "open");
        let below = private_dir(&open, "below");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        for d in [&open, &below] {
            let out = sh(&stage_cmd(&crate::testutil::local_os(), &b(d)));
            assert_eq!(out.status.code(), Some(UNSAFE_DIR), "{d:?}");
            assert!(
                matches!(local_dest_safe(d), Err(m) if m == UNSAFE_MSG),
                "{d:?}"
            );
        }
        assert_eq!(fs::read_dir(&below).unwrap().count(), 0, "nothing created");
        // sticky (like /tmp) is fine
        fs::set_permissions(&open, fs::Permissions::from_mode(0o1777)).unwrap();
        let (_, stage, sp) = stage_in(&below);
        assert!(sh(&unstage_cmd(&stage)).status.success());
        assert!(!sp.exists());
        assert!(local_dest_safe(&below).is_ok());
    }

    #[test]
    fn real_identity_tells_files_apart_and_does_not_follow_links() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        fs::write(t.path().join("b"), "x").unwrap();
        std::os::unix::fs::symlink(t.path().join("a"), t.path().join("l")).unwrap();
        let id = |n: &str| {
            let out = sh(&identity_cmd(
                &crate::testutil::local_os(),
                &b(&t.path().join(n)),
            ));
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_ne!(id("a"), id("b"));
        assert_ne!(id("a"), id("l"));
        assert_eq!(Some(id("a")), identity(&Side::Local, &t.path().join("a")));
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
        /// Runs while a conflict is being "asked".
        on_ask: Option<Box<dyn FnMut()>>,
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
                on_ask: None,
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
            if let Some(f) = self.on_ask.as_mut() {
                f();
            }
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
        if c.starts_with("if [ -d '") || c.contains("; mv -f -- ") {
            return code(0); // destination dir check, rename into place
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
        if c.starts_with("cd -- ") && c.contains("du -sk") {
            // the up-front scan has every name, the re-check before sending one
            let list = c.split_once("for f in ").unwrap().1;
            let list = list.split_once("; do").unwrap().0;
            let n = list.matches("' '").count() + 1;
            let recs: Vec<&str> = scan.split_terminator('\0').collect();
            if recs.len() < 2 * n {
                return ok(scan);
            }
            let first: String = recs[..2 * n].iter().map(|r| format!("{r}\0")).collect();
            return ok(&first);
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
        let mv = String::from_utf8(rename_over_cmd(b"/srv/b", b"/dst/b")).unwrap();
        assert_eq!(long, vec![cp, mv]);
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
            if c.starts_with("if [ -L '") && !c.contains("; mv -f -- ") {
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

    #[test]
    fn a_move_onto_the_same_file_keeps_it() {
        // ssh to this very machine: the remote target is the local source.
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("a");
        fs::write(&src, "only copy").unwrap();
        let src2 = src.clone();
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("; mv -f -- ") {
                // renaming the copy into place replaces the source itself
                fs::remove_file(&src2).unwrap();
                fs::write(&src2, "only copy").unwrap();
                return code(0);
            }
            answer(c, &[], &["/dst/a"], "")
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
        let r = run(&s, &mut Ctl::answering(&[ConflictAnswer::Overwrite]));
        assert_eq!((r.outcome, r.partial, r.copied), (OpOutcome::Done, 1, 0));
        assert_eq!(
            r.warnings,
            vec!["원본과 대상이 같은 파일일 수 있어 원본을 지우지 않음: a".to_string()]
        );
        assert!(trash.trashed.lock().unwrap().is_empty());
        assert_eq!(fs::read(&src).unwrap(), b"only copy");
    }

    #[test]
    fn a_remote_move_over_a_target_checks_the_source_identity() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "old").unwrap();
        let ids = Arc::new(Mutex::new(vec!["1:1\n", "1:1\n"]));
        let i2 = Arc::clone(&ids);
        let f = FakeCmd::new(move |a| {
            if a[0] == "scp" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("stat -c '%d:%i' -- '/srv/a'") {
                return ok(i2.lock().unwrap().remove(0));
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
        let r = run(&s, &mut Ctl::answering(&[ConflictAnswer::Overwrite]));
        assert_eq!(
            (r.outcome, r.copied, r.overwritten),
            (OpOutcome::Done, 1, 1)
        );
        assert!(ids.lock().unwrap().is_empty(), "checked before and after");
        assert!(
            f.remote_cmds().iter().any(|c| c.contains("rm -rf")),
            "unchanged: deleted"
        );
    }

    #[test]
    fn the_same_folder_on_one_host_reached_twice_is_refused() {
        let fa = host(&[], &[], "");
        let fb = host(&[], &[], "");
        let twin = RemoteSession::new(
            "again".into(),
            crate::remote::SshTarget {
                host: "prod-web".into(),
                port: None,
                control_path: p("/tmp/hdir-1/cm-again"),
            },
            RemoteOs::Linux,
            crate::remote::ScpMode::Sftp,
            Arc::clone(&fb) as Arc<dyn crate::session::Cmd>,
        );
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&fa))),
            &p("/srv"),
            &["a"],
            Side::Remote(Arc::new(twin)),
            &p("/srv"),
        );
        let r = run(&s, &mut Ctl::new());
        assert!(matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("같은 폴더")));
        assert!(fa.log.lock().unwrap().is_empty() && fb.log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_source_turned_into_a_link_while_asking_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("a");
        fs::write(&src, "x").unwrap();
        let f = host(&[], &["/dst/a"], "");
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let mut c = Ctl::answering(&[ConflictAnswer::Overwrite]);
        c.on_ask = Some(Box::new(move || {
            fs::remove_file(&src).unwrap();
            std::os::unix::fs::symlink("/etc/hosts", &src).unwrap();
        }));
        let r = run(&s, &mut c);
        assert_eq!(
            (r.outcome, r.skipped, r.overwritten),
            (OpOutcome::Done, 1, 0)
        );
        assert!(r.warnings[0].contains("심볼릭 링크"), "{:?}", r.warnings);
        assert!(f.scp_calls().is_empty());
    }

    #[test]
    fn a_failed_rename_into_place_keeps_the_source_and_the_old_target() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("; mv -f -- ") {
                return fail(1, "mv: Permission denied\n");
            }
            answer(c, &[], &[], "")
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
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("a: 제자리로 옮기지 못함: mv: Permission denied".into())
        );
        assert!(trash.trashed.lock().unwrap().is_empty());
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert_eq!(f.remote_cmds().last(), Some(&unstage));
    }

    #[test]
    fn an_unsafe_destination_is_refused_before_sending() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "scp" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("mktemp -d") {
                return code(UNSAFE_DIR);
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
        assert_eq!(r.outcome, OpOutcome::Failed(format!("a: {UNSAFE_MSG}")));
        assert!(f.scp_calls().is_empty());
        // locally too
        use std::os::unix::fs::PermissionsExt;
        let open = tempfile::tempdir().unwrap();
        fs::set_permissions(open.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let f = host(&[], &[], "0\x000\x00");
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            open.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Failed(format!("a: {UNSAFE_MSG}")));
        assert!(f.scp_calls().is_empty());
    }
}
