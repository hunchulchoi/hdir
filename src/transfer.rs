//! Copy / move when at least one side is a remote panel (M8).
//!
//! Routes (local to local stays in [`crate::fs_ops`]):
//! - local → remote, remote → local and server → server: a tar stream
//!   (`tar -c | ssh host tar -x`, or `ssh a tar -c | ssh b tar -x`), each ssh
//!   through its own session's ControlMaster. Links travel as links and are
//!   never followed, so nothing outside the selection can be read;
//! - two panels on the same server (same session): `cp -Rp` / a rename run
//!   on the server, nothing goes through this machine.
//!
//! Every item lands in a fresh private staging directory inside the
//! (physical, checked) destination (`.hdir-xfer.XXXXXX`) and is renamed into
//! place only once it arrived in full. So an interrupted transfer (Esc, lost
//! link, full disk) never touches an existing target, and an existing
//! symlink at the target is replaced as a link instead of being written
//! through. A name that was free when checked is placed without replacing
//! anything (a file appearing meanwhile is kept). Esc kills both ends of the
//! stream and the staging directory is removed.
//!
//! Refused:
//! - a destination where someone else could swap the staging dir (the same
//!   parent-by-parent check remote saves use);
//! - a conflict where either side is a directory (skipped): merging cannot
//!   be atomic;
//! - a folder sent into itself (compared by device and inode).
//!
//! A move deletes the source only when the copy is in place and the
//! source's whole tree (identity, size, nanosecond mtime / ctime of every
//! entry) is unchanged since before sending (so a file changed meanwhile,
//! or the same file reached through two connections, stays). Local sources
//! go to the trash; remote ones are first set aside in a private
//! `.hdir-del.*` dir, checked again there and deleted through
//! [`remote_ops::delete`] with its home / `/` protections. On one server a
//! move is a rename. Progress is per item, sized up front (`du -sk` on the
//! host, a walk locally).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::fs_ops::{ConflictAction, OpControl, OpOutcome, OpReport, Totals};
use crate::job::JobKind;
use crate::platform::os_bytes;
use crate::remote::{RemoteOs, UNSAFE_DIR, exec_argv, safe_dirs_check, sh_quote};
use crate::remote_ops;
use crate::session::{CmdOutput, RemoteSession, SessionError, first_line};
use crate::trash::TrashBackend;

/// Exit codes of the remote helper commands.
pub const NOT_THERE: i32 = 43;
pub const IS_DIR: i32 = 44;
pub const IS_OTHER: i32 = 45;
/// [`place_cmd`]: the item is in place but the staging dir could not go.
pub const STAGE_LEFT: i32 = 46;
/// [`place_cmd`]: a real directory appeared at an approved target.
pub const DIR_APPEARED: i32 = 47;
/// [`place_cmd`]: something appeared at a target that was free.
pub const APPEARED: i32 = 48;
/// [`place_cmd`]: a free target, but no hard links to place a file safely.
pub const NO_SAFE_RENAME: i32 = 49;
/// [`place_cmd`]: something took an approved target's place while the old
/// item was set aside; both stay in the stage.
pub const ASIDE_LEFT: i32 = 50;

/// Name prefix of staging directories.
pub const STAGE_PREFIX: &str = ".hdir-xfer.";

/// Shown when the destination (or a parent) could be tampered with.
pub fn unsafe_msg() -> &'static str {
    crate::i18n::t(
        "다른 사용자가 바꿀 수 있는 폴더라 보내지 않음",
        "Not sent: a folder other users can change",
    )
}

fn special_msg() -> &'static str {
    crate::i18n::t("특수 파일은 옮기지 않음", "Special files are not moved")
}

fn appeared_msg() -> &'static str {
    crate::i18n::t(
        "대상에 같은 이름이 새로 생겨 건너뜀",
        "Skipped: the same name just appeared in the destination",
    )
}

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

/// For each of `names` in `dir`: its size in KiB, NUL-terminated. Only the
/// number before the first tab of `du` is kept, so a name with a newline
/// cannot shift the list.
pub fn scan_cmd(dir: &[u8], names: &[&[u8]]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; for f in ",
        &word_list(names),
        b"; do k=$(du -sk -- \"$f\" 2>/dev/null | head -n 1 | cut -f 1); \
          printf '%s\\0' \"${k:-0}\"; done",
    ])
}

/// Bytes per name from [`scan_cmd`] output; `None` unless it holds exactly
/// `n` records (unknown sizes count as 0).
pub fn parse_scan(stdout: &[u8], n: usize) -> Option<Vec<u64>> {
    if n == 0 {
        return stdout.is_empty().then(Vec::new);
    }
    let body = stdout.strip_suffix(b"\0")?;
    let fields: Vec<&[u8]> = body.split(|&b| b == 0).collect();
    (fields.len() == n).then(|| {
        fields
            .iter()
            .map(|f| {
                std::str::from_utf8(f)
                    .ok()
                    .and_then(|t| t.trim().parse::<u64>().ok())
                    .map_or(0, |k| k.saturating_mul(1024))
            })
            .collect()
    })
}

/// Remote side of a transfer stream: a tar of `dir/name` on stdout. Links
/// are stored as links, never followed; `./` keeps a leading `-` from being
/// an option.
///
/// `id` is the `dev:inode` the folder had when the job started: right after
/// the `cd`, the folder now current must still be that one ([`SRC_MOVED`]
/// otherwise), and tar reads relative to it, so swapping the folder for a
/// link can never make it read elsewhere.
pub fn tar_out_cmd(os: &RemoteOs, dir: &[u8], name: &[u8], id: &[u8], acls: bool) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; i=$(",
        dir_id_fmt(os),
        b".) || exit 1; [ \"$i\" = ",
        &sh_quote(id),
        b" ] || exit 51; ",
        tar_word(acls),
        b" -cf - ./",
        &sh_quote(name),
    ])
}

/// `tar`, with `--acls` when POSIX ACLs travel too.
fn tar_word(acls: bool) -> &'static [u8] {
    if acls { b"tar --acls" } else { b"tar" }
}

/// Exit 0 when this side's tar can store and restore POSIX ACLs.
pub const TAR_ACLS_PROBE: &[u8] = b"tar --acls -cf /dev/null -T /dev/null >/dev/null 2>&1";

/// Prints how many entries in `dir/name` (links not followed) carry an ACL.
/// `ls` marks them with `+` after the mode, except macOS, which shows `@`
/// instead when there are xattrs too: there `ls -le` lists the ACL entries
/// and each ACL's first one (` 0: `) is counted. A name with a newline can
/// only add a false match, which errs on the safe side.
pub fn acl_scan_cmd(os: &RemoteOs, dir: &[u8], name: &[u8]) -> Vec<u8> {
    let q = sh_quote(name);
    let plus: &[&[u8]] = &[
        b"n=$(find ./",
        &q,
        b" ! -type l -exec ls -dn -- {} + | grep -c '^[^ ]\\{10\\}+')",
    ];
    let mut v = cat(&[b"cd -- ", &sh_quote(dir), b" || exit 1; "]);
    if matches!(os, RemoteOs::Bsd) {
        v.extend_from_slice(b"if [ \"$(uname)\" = Darwin ]; then n=$(find ./");
        v.extend_from_slice(&q);
        v.extend_from_slice(b" ! -type l -exec ls -lden -- {} + | grep -c '^ 0: '); else ");
        v.extend(cat(plus));
        v.extend_from_slice(b"; fi");
    } else {
        v.extend(cat(plus));
    }
    v.extend_from_slice(b"; echo $((n))");
    v
}

/// Removes group and other permissions from everything in `stage/name`:
/// used when ACLs that limited them could not travel.
pub fn restrict_cmd(stage: &[u8], name: &[u8]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(stage),
        b" || exit 1; find ./",
        &sh_quote(name),
        b" ! -type l -exec chmod go-rwx {} +",
    ])
}

/// What to do about POSIX ACLs in a tar transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclPlan {
    /// The source has none: plain tar.
    None,
    /// Both tars handle them: `--acls` on both ends.
    Keep,
    /// They cannot travel: strip group and other permissions on arrival, so
    /// nobody the ACL kept out gets in through the plain mode bits.
    Restrict,
}

pub fn acl_plan(has_acls: bool, sender_ok: bool, receiver_ok: bool) -> AclPlan {
    match (has_acls, sender_ok && receiver_ok) {
        (false, _) => AclPlan::None,
        (true, true) => AclPlan::Keep,
        (true, false) => AclPlan::Restrict,
    }
}

/// Exit code of a tar sender whose source folder is no longer the one the
/// job started with.
pub const SRC_MOVED: i32 = 51;

/// `stat` printing `dev:inode` of the path that follows.
fn dir_id_fmt(os: &RemoteOs) -> &'static [u8] {
    match os {
        RemoteOs::Bsd => b"stat -f %d:%i -- ",
        RemoteOs::Linux | RemoteOs::Other(_) => b"stat -c %d:%i -- ",
    }
}

/// `dev:inode` of the folder `dir` itself (followed), on its side.
fn src_dir_id(side: &Side, dir: &Path) -> Result<String, String> {
    let err = |e: String| {
        crate::tr!(
            "원본 폴더를 확인할 수 없음: {e}",
            "Cannot check the source folder: {e}"
        )
    };
    match side {
        Side::Local => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let m = std::fs::metadata(dir).map_err(|e| err(e.to_string()))?;
                Ok(format!("{}:{}", m.dev(), m.ino()))
            }
            #[cfg(not(unix))]
            {
                let _ = dir;
                Ok(String::new())
            }
        }
        Side::Remote(s) => {
            let cmd = cat(&[
                b"cd -- ",
                &sh_quote(&b(dir)),
                b" && ",
                dir_id_fmt(&s.os),
                b".",
            ]);
            let out = ran(s.exec(&cmd).map_err(|e| err(e.message()))?).map_err(err)?;
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if id.is_empty() {
                return Err(err(crate::i18n::t("빈 응답", "empty response").into()));
            }
            Ok(id)
        }
    }
}

/// Remote side of a transfer stream: unpack stdin into `stage`, keeping
/// modes (`-p`) but not the sender's owners (`-o`).
pub fn tar_in_cmd(stage: &[u8], acls: bool) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(stage),
        b" || exit 1; ",
        tar_word(acls),
        b" -xpof - || exit 1; ",
        STRIP_SETID,
    ])
}

/// Clears setuid / setgid on every regular file under `.`: kept modes must
/// not turn a received program into a privilege for whoever unpacked it.
const STRIP_SETID: &[u8] =
    b"find . -type f \\( -perm -4000 -o -perm -2000 \\) -exec chmod ug-s {} +";

/// [`STRIP_SETID`] for a local stage (links are never followed).
#[cfg(unix)]
fn strip_setid(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(q) = todo.pop() {
        let m = std::fs::symlink_metadata(&q)?;
        if m.is_dir() {
            for e in std::fs::read_dir(&q)? {
                todo.push(e?.path());
            }
        } else if m.is_file() && m.permissions().mode() & 0o6000 != 0 {
            let mode = m.permissions().mode() & !0o6000;
            std::fs::set_permissions(&q, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn strip_setid(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Exit 0 when `a` and `b` are on the same filesystem (device), else not.
pub fn same_fs_cmd(os: &RemoteOs, a: &[u8], b: &[u8]) -> Vec<u8> {
    let st: &[u8] = match os {
        RemoteOs::Bsd => b"stat -f %d -- ",
        RemoteOs::Linux | RemoteOs::Other(_) => b"stat -c %d -- ",
    };
    cat(&[
        b"x=$(",
        st,
        &sh_quote(a),
        b") && y=$(",
        st,
        &sh_quote(b),
        b") && [ -n \"$x\" ] && [ \"$x\" = \"$y\" ]",
    ])
}

/// Local `tar` writing `dir/name` to stdout. On macOS without AppleDouble
/// files or extended attributes, which other tars would warn about.
///
/// Like [`tar_out_cmd`], a shell first enters `dir`, checks it is still the
/// folder `id` names and then runs tar there; the paths travel as arguments
/// (`$1` folder, `$2` id, `$3` name), never inside the script.
pub fn local_tar_out(dir: &Path, name: &std::ffi::OsStr, id: &str, acls: bool) -> Vec<OsString> {
    if cfg!(windows) {
        // no POSIX shell: Windows' own tar.exe, path based like local
        // copies there
        let _ = id;
        let mut item = OsString::from("./");
        item.push(name);
        let mut v: Vec<OsString> = ["tar", "-cf", "-", "-C"].map(OsString::from).to_vec();
        v.push(dir.as_os_str().to_os_string());
        v.push(item);
        return v;
    }
    let (stat, tar) = if cfg!(target_os = "linux") {
        (
            "stat -c %d:%i -- .",
            if acls { "tar --acls" } else { "tar" },
        )
    } else if cfg!(target_os = "macos") {
        (
            "stat -f %d:%i -- .",
            "COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs",
        )
    } else {
        ("stat -f %d:%i -- .", "tar")
    };
    let script = format!(
        "cd -- \"$1\" || exit 1; i=$({stat}) || exit 1; \
         [ \"$i\" = \"$2\" ] || {{ echo '원본 폴더가 바뀌어 보내지 않음' >&2; exit 51; }}; \
         exec env {tar} -cf - \"./$3\""
    );
    let mut v: Vec<OsString> = ["sh", "-c"].map(OsString::from).to_vec();
    v.push(script.into());
    v.push("sh".into());
    v.push(dir.as_os_str().to_os_string());
    v.push(id.into());
    v.push(name.to_os_string());
    v
}

/// Unpack the package `pkg` (a tar uploaded into `stage`) there, then drop
/// it and strip setuid / setgid like [`tar_in_cmd`].
pub fn unpack_cmd(stage: &[u8], pkg: &[u8]) -> Vec<u8> {
    let p = sh_quote(pkg);
    cat(&[
        b"cd -- ",
        &sh_quote(stage),
        b" || exit 1; tar -xpof ",
        &p,
        b" || exit 1; rm -f -- ",
        &p,
        b" || exit 1; ",
        STRIP_SETID,
    ])
}

/// Name of the package uploaded next to the item: never the item's own.
fn pkg_name(item: &[u8]) -> &'static [u8] {
    if item == b"o.tar" { b"p.tar" } else { b"o.tar" }
}

/// Windows upload (its ssh.exe hangs when fed data on stdin): tar the item
/// into a local package, scp the package into the stage, unpack it there.
fn send_up_packaged(
    d: &RemoteSession,
    src_dir: &Path,
    name: &OsString,
    stage: &[u8],
    cancel: &AtomicBool,
) -> Result<(), String> {
    let pkg = tempfile::Builder::new()
        .prefix("hdir-pkg-")
        .suffix(".tar")
        .tempfile()
        .map_err(|e| {
            crate::tr!(
                "임시 파일을 만들 수 없음: {e}",
                "Cannot create temp file: {e}"
            )
        })?;
    let mut item = OsString::from("./");
    item.push(name);
    let mut tar: Vec<OsString> = ["tar", "-cf"].map(OsString::from).to_vec();
    tar.push(pkg.path().as_os_str().to_os_string());
    tar.push("-C".into());
    tar.push(src_dir.as_os_str().to_os_string());
    tar.push(item);
    ran(d.run_local(&tar, cancel))?;
    let remote_pkg = cat(&[&into_dir(stage), pkg_name(&os_bytes(name))]);
    let argv = crate::remote::scp_argv(
        &crate::remote::Endpoint::Local(pkg.path()),
        &crate::remote::Endpoint::Remote(&d.target, &remote_pkg),
        d.scp_mode,
    )?;
    ran(d.scp_cancellable(&argv, cancel).map_err(|e| e.message())?)?;
    let cmd = unpack_cmd(stage, pkg_name(&os_bytes(name)));
    ran(d.exec_cancellable(&cmd, cancel).map_err(|e| e.message())?).map(drop)
}

/// Local `tar` unpacking stdin into `stage`.
pub fn local_tar_in(stage: &Path, acls: bool) -> Vec<OsString> {
    let mut v: Vec<OsString> = ["tar"].map(OsString::from).to_vec();
    if acls {
        v.push("--acls".into());
    }
    v.extend(["-xpof", "-", "-C"].map(OsString::from));
    v.push(stage.as_os_str().to_os_string());
    v
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
    // Without GNU -T, placing cannot be made race-free against names other
    // users create, so a sticky (shared) destination is refused there.
    let sticky: &[u8] = if matches!(os, RemoteOs::Linux) {
        b""
    } else {
        b"k=$(find \"$d0\" -prune -perm -1000) || exit 1; [ -z \"$k\" ] || exit 37; "
    };
    cat(&[
        b"cd -- ",
        &sh_quote(dest_dir),
        b" || exit 1; d0=$(pwd -P && echo x) || exit 1; d0=${d0%?x}; ",
        sticky,
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

/// Rename `from` onto `to`, never following `to`, then remove the (empty)
/// staging dir `stage` ([`STAGE_LEFT`] when that fails).
///
/// `replace` (the user approved overwriting): a symlink at `to`, even one to
/// a directory, is set aside in the stage and replaced as a link (and put
/// back if the rename fails); a real directory there is refused
/// ([`DIR_APPEARED`]).
///
/// Not `replace` (`to` was free when checked): anything there now is kept
/// ([`APPEARED`]). A regular file is placed with `ln` (which never
/// replaces) and the staged name removed; where hard links do not work it
/// is refused ([`NO_SAFE_RENAME`]) rather than raced. Other kinds are
/// renamed after a last check (rename(2) cannot replace a file or a
/// non-empty directory with a directory).
pub fn place_cmd(os: &RemoteOs, from: &[u8], to: &[u8], stage: &[u8], replace: bool) -> Vec<u8> {
    let (f, t, st) = (sh_quote(from), sh_quote(to), sh_quote(stage));
    // The set-aside link gets a one-letter name that differs from the
    // staged item's own name, so they can never collide.
    let base = from.rsplit(|&c| c == b'/').next().unwrap_or_default();
    let spare: &[u8] = if base == b"o" { b"p" } else { b"o" };
    let aside = sh_quote(&cat(&[&into_dir(stage), spare]));
    let inside = sh_quote(&cat(&[&into_dir(to), base]));
    let there: &[u8] = b"if [ -e $T ] || [ -L $T ]; then exit 48; fi; ";
    let linux = matches!(os, RemoteOs::Linux);
    let body: Vec<u8> = match (replace, linux) {
        // GNU -T: the target is never taken as a directory to move into, so
        // a link there (even to a directory) is replaced in one rename.
        (true, true) => b"if [ -d $T ] && [ ! -L $T ]; then exit 47; fi; \
                          mv -fT -- $F $T || exit 1; "
            .to_vec(),
        // No -T: whatever is at the target is first set aside in the stage
        // (a fresh name there), then the item goes in with -n, which never
        // writes over anything. Whether it landed on the name itself (and
        // not in a folder or a link to one that took the name meanwhile) is
        // checked by inode; if not, it is taken back and the set-aside item
        // stays in the stage ([`ASIDE_LEFT`]) rather than being lost.
        (true, false) => b"set -- $(ls -di -- $F) || exit 1; i=$1; s=; \
              if [ -e $T ] || [ -L $T ]; then \
              if [ -d $T ] && [ ! -L $T ]; then exit 47; fi; \
              mv -n -- $T $A || exit 1; \
              if [ -d $A ] && [ ! -L $A ]; then mv -n -- $A $T; exit 47; fi; \
              if [ -e $T ] || [ -L $T ]; then mv -n -- $A $T; exit 1; fi; s=1; fi; \
              mv -n -- $F $T || { if [ -n \"$s\" ]; then \
              if [ -e $T ] || [ -L $T ]; then exit 50; fi; mv -n -- $A $T; \
              if [ -e $A ] || [ -L $A ]; then exit 50; fi; fi; exit 1; }; \
              set -- $(ls -di -- $T 2>/dev/null) x; \
              if [ \"$1\" != \"$i\" ] || [ -e $F ] || [ -L $F ]; then \
              if [ ! -e $F ] && [ ! -L $F ]; then mv -n -- $I $F; fi; \
              if [ -n \"$s\" ]; then exit 50; fi; exit 48; fi; \
              if [ -n \"$s\" ]; then rm -f -- $A; fi; "
            .to_vec(),
        // Nothing is ever renamed over a name that may have been taken: a
        // file is hard-linked, a symlink re-created (both fail when the name
        // exists), a directory renamed with -T (which cannot replace a file,
        // a link or a non-empty directory).
        (false, true) => cat(&[
            there,
            b"if [ -L $F ]; then l=$(readlink -- $F && echo x) || exit 1; l=${l%?x}; \
              if ln -sT -- \"$l\" $T 2>/dev/null; then rm -f -- $F; else ",
            there,
            b"exit 1; fi; \
              elif [ -f $F ]; then \
              if ln -T -- $F $T 2>/dev/null; then rm -f -- $F; else ",
            there,
            b"exit 49; fi; \
              elif [ -d $F ]; then mv -nT -- $F $T 2>/dev/null || { ",
            there,
            b"exit 1; }; \
              if [ -e $F ] || [ -L $F ]; then exit 48; fi; \
              else exit 49; fi; ",
        ]),
        // Without -T, `ln` and `mv` go into a directory that appeared
        // meanwhile: that is detected afterwards (`-ef`, the link's own
        // target, the inode), undone and refused; `ln -n` never follows a
        // link that appeared there.
        (false, false) => cat(&[
            there,
            b"if [ -L $F ]; then l=$(readlink -- $F && echo x) || exit 1; l=${l%?x}; \
              if ln -sn -- \"$l\" $T 2>/dev/null; then \
              r=$(readlink -- $T 2>/dev/null && echo x); \
              if [ -L $T ] && [ \"${r%?x}\" = \"$l\" ]; then rm -f -- $F; else \
              b=${l##*/}; if [ -n \"$b\" ]; then \
              q=$(readlink -- $T/\"$b\" 2>/dev/null && echo x); \
              if [ -L $T/\"$b\" ] && [ \"${q%?x}\" = \"$l\" ]; then rm -f -- $T/\"$b\"; fi; fi; \
              exit 48; fi; else ",
            there,
            b"exit 1; fi; \
              elif [ -f $F ]; then \
              if ln -- $F $T 2>/dev/null; then \
              if [ $T -ef $F ]; then rm -f -- $F; else rm -f -- $I; exit 48; fi; else ",
            there,
            b"exit 49; fi; \
              elif [ -d $F ]; then set -- $(ls -di -- $F) || exit 1; i=$1; \
              mv -- $F $T || exit 1; set -- $(ls -di -- $T) || exit 1; \
              if [ \"$1\" != \"$i\" ]; then mv -- $I $F; exit 48; fi; \
              else exit 49; fi; ",
        ]),
    };
    let body = fill(
        &body,
        &[(b"$T", &t), (b"$F", &f), (b"$A", &aside), (b"$I", &inside)],
    );
    cat(&[&body, b"rmdir -- ", &st, b" || exit 46"])
}

/// `text` with every slot replaced by its bytes, in one pass (a path that
/// itself contains `$F` is never substituted again).
fn fill(text: &[u8], slots: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut o = Vec::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < text.len() {
        for (slot, with) in slots {
            if text[i..].starts_with(slot) {
                o.extend_from_slice(with);
                i += slot.len();
                continue 'outer;
            }
        }
        o.push(text[i]);
        i += 1;
    }
    o
}

/// A checksum over `dev:inode:size:mtime:ctime:name` (nanoseconds) of
/// `dir/name` and everything under it, links not followed: any change to
/// the tree (content, a rename, a new file) changes it.
pub fn stamp_cmd(os: &RemoteOs, dir: &[u8], name: &[u8]) -> Vec<u8> {
    // The top entry leaves out its ctime: setting it aside before deletion
    // (a rename) changes that, and nothing else.
    let (top, fmt): (&[u8], &[u8]) = match os {
        RemoteOs::Bsd => (
            b"stat -f '%d:%i:%z:%Fm:%N'",
            b"stat -f '%d:%i:%z:%Fm:%Fc:%N'",
        ),
        RemoteOs::Linux | RemoteOs::Other(_) => (
            b"stat -c '%d:%i:%s:%.9Y:%n'",
            b"stat -c '%d:%i:%s:%.9Y:%.9Z:%n'",
        ),
    };
    let n = cat(&[b"./", &sh_quote(name)]);
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; c=$(",
        ctime_fmt(os),
        &n,
        b") || exit 1; t=$(",
        top,
        b" -- ",
        &n,
        b") || exit 1; s=$(find ",
        &n,
        b" -mindepth 1 -exec ",
        fmt,
        b" {} +) || exit 1; printf '%s ' \"$c\"; \
          printf '%s\\n%s\\n' \"$t\" \"$s\" | LC_ALL=C sort | cksum",
    ])
}

/// Print how many entries the tree at `dir/name` has (itself included,
/// links not followed); names with newlines count once.
pub fn count_cmd(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let n = cat(&[b"./", &sh_quote(name)]);
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; [ -e ",
        &n,
        b" ] || [ -L ",
        &n,
        b" ] || exit 1; n=$(find ",
        &n,
        b" -exec sh -c 'for f do printf .; done' sh {} + | wc -c) || exit 1; echo $((n))",
    ])
}

/// `stat` printing the ctime (nanoseconds) of the path that follows.
fn ctime_fmt(os: &RemoteOs) -> &'static [u8] {
    match os {
        RemoteOs::Bsd => b"stat -f %Fc -- ",
        RemoteOs::Linux | RemoteOs::Other(_) => b"stat -c %.9Z -- ",
    }
}

/// [`quarantine_cmd`]: the item changed since it was fingerprinted.
pub const SOURCE_CHANGED: i32 = 50;

/// Prefix of the dir a remote source is set aside in before deletion.
pub const QUARANTINE_PREFIX: &str = ".hdir-del.";

/// Move `dir/name` into a fresh private `dir/.hdir-del.XXXXXX` (printed,
/// NUL-terminated, relative to `dir`). The name inside stays `name`.
///
/// Refused when someone else could swap that dir before it is deleted from
/// (the physical `dir` and its parents fail [`safe_dirs_check`]:
/// [`UNSAFE_DIR`]), and when the item's ctime is no longer `ctime` (its
/// fingerprint's top entry, which the rename itself would change:
/// [`SOURCE_CHANGED`]), and for the home, or a directory containing it
/// ([`crate::remote::DELETE_PROTECTED`]), before anything moves.
pub fn quarantine_cmd(os: &RemoteOs, dir: &[u8], name: &[u8], ctime: &[u8]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; d0=$(pwd -P && echo x) || exit 1; d0=${d0%?x}; ",
        &safe_dirs_check(os, b"\"$d0\""),
        b"c=$(",
        ctime_fmt(os),
        b"./",
        &sh_quote(name),
        b") || exit 1; [ \"$c\" = ",
        &sh_quote(ctime),
        b" ] || exit 50; ",
        // never set aside the home, `/` or a parent of the home: the delete
        // would refuse it and leave it in the quarantine
        b"h=$(cd -- \"$HOME\" && pwd -P && echo x) || exit 42; h=${h%?x}; \
          if [ -d ./",
        &sh_quote(name),
        b" ] && [ ! -L ./",
        &sh_quote(name),
        b" ]; then p=$(cd -- ./",
        &sh_quote(name),
        b" && pwd -P && echo x) || exit 1; p=${p%?x}; \
          case \"$h/\" in \"$p\"/*) exit 42;; esac; fi; q=$(mktemp -d ",
        QUARANTINE_PREFIX.as_bytes(),
        b"XXXXXX) || exit 1; mv -- ./",
        &sh_quote(name),
        b" \"$q/\" || { rmdir -- \"$q\"; exit 1; }; printf '%s\\0' \"$q\"",
    ])
}

/// The dir name printed by [`quarantine_cmd`]: the prefix plus something,
/// no `/` (never anything else, so it cannot point elsewhere).
pub fn parse_quarantine(stdout: &[u8]) -> Option<Vec<u8>> {
    let end = stdout.iter().position(|&b| b == 0)?;
    let q = &stdout[..end];
    let rest = q.strip_prefix(QUARANTINE_PREFIX.as_bytes())?;
    (!rest.is_empty() && !rest.contains(&b'/') && stdout.len() == end + 1).then(|| q.to_vec())
}

/// After a failed same-server move into `stage`: put `stage/name` back to
/// `src` without replacing anything there ([`place_cmd`], so [`APPEARED`]
/// when `src` was taken meanwhile and the stage then stays), or just remove
/// the stage when the item never got there.
pub fn unmove_cmd(os: &RemoteOs, stage: &[u8], name: &[u8], src: &[u8]) -> Vec<u8> {
    let staged = cat(&[&into_dir(stage), name]);
    let q = sh_quote(&staged);
    cat(&[
        b"if [ -e ",
        &q,
        b" ] || [ -L ",
        &q,
        b" ]; then ",
        &place_cmd(os, &staged, src, stage, false),
        b"; else rmdir -- ",
        &sh_quote(stage),
        b"; fi",
    ])
}

fn id_fmt(os: &RemoteOs) -> &'static [u8] {
    match os {
        RemoteOs::Bsd => b"stat -f '%d:%i' -- ",
        RemoteOs::Linux | RemoteOs::Other(_) => b"stat -c '%d:%i' -- ",
    }
}

/// `dev:inode` of the physical `dir` and of every parent up to `/`, one
/// per line.
pub fn chain_cmd(os: &RemoteOs, dir: &[u8]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(dir),
        b" || exit 1; c=$(pwd -P && echo x) || exit 1; c=${c%?x}; while :; do ",
        id_fmt(os),
        b"\"$c\" || exit 1; [ \"$c\" = / ] && break; c=${c%/*}; [ -n \"$c\" ] || c=/; done",
    ])
}

/// `dev:inode` of `path` when it is a real directory, else nothing.
pub fn dir_id_cmd(os: &RemoteOs, path: &[u8]) -> Vec<u8> {
    let q = sh_quote(path);
    cat(&[
        b"if [ -d ",
        &q,
        b" ] && [ ! -L ",
        &q,
        b" ]; then ",
        id_fmt(os),
        &q,
        b"; fi",
    ])
}

/// Remove a staging dir and whatever a failed transfer left in it.
pub fn unstage_cmd(stage: &[u8]) -> Vec<u8> {
    cat(&[b"rm -rf -- ", &sh_quote(stage)])
}

/// Copy `src` into `dir` on the same host.
pub fn cp_cmd(os: &RemoteOs, src_dir: &[u8], name: &[u8], id: &[u8], dir: &[u8]) -> Vec<u8> {
    cat(&[
        &in_src_dir(os, src_dir, id),
        b"cp -Rp -- ./",
        &sh_quote(name),
        b" ",
        &sh_quote(&into_dir(dir)),
    ])
}

/// Enter `src_dir` and make sure it is still the folder `id` names
/// ([`SRC_MOVED`]), so what follows works relative to exactly that folder.
fn in_src_dir(os: &RemoteOs, src_dir: &[u8], id: &[u8]) -> Vec<u8> {
    cat(&[
        b"cd -- ",
        &sh_quote(src_dir),
        b" || exit 1; i=$(",
        dir_id_fmt(os),
        b".) || exit 1; [ \"$i\" = ",
        &sh_quote(id),
        b" ] || exit 51; ",
    ])
}

/// [`host_mv_cmd`]: not a file, folder or link (a FIFO, a device).
pub const SPECIAL: i32 = 52;

/// Rename `src_dir/name` to `into` on the same server, relative to the
/// checked source folder; special files are refused ([`SPECIAL`]) before
/// anything moves, as the place step could not put them anywhere.
pub fn host_mv_cmd(os: &RemoteOs, src_dir: &[u8], name: &[u8], id: &[u8], into: &[u8]) -> Vec<u8> {
    let n = cat(&[b"./", &sh_quote(name)]);
    cat(&[
        &in_src_dir(os, src_dir, id),
        b"if [ -L ",
        &n,
        b" ] || [ -f ",
        &n,
        b" ] || [ -d ",
        &n,
        b" ]; then :; else exit 52; fi; mv -- ",
        &n,
        b" ",
        &sh_quote(into),
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
        (None, None) => {
            return Err(crate::tr!(
                "로컬 간 복사는 원격 전송이 아님",
                "Copying between local folders is not a remote transfer"
            ));
        }
        (None, Some(d)) => Route::Up(d),
        (Some(s), None) => Route::Down(s),
        (Some(s), Some(d)) if std::ptr::eq(s, d) => Route::SameHost(s),
        (Some(s), Some(d)) => Route::Between(s, d),
    })
}

/// A remote path as bytes (see [`crate::platform::remote_bytes`]).
fn b(p: &Path) -> Vec<u8> {
    crate::platform::remote_bytes(p.as_os_str()).into_owned()
}

fn code_err(out: &CmdOutput) -> String {
    first_line(
        &out.stderr,
        &crate::tr!("종료 코드 {}", "exit code {}", out.code.unwrap_or(-1)),
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

/// Check the destination folder. A local one is resolved and checked for
/// safety here, once; the physical path is what staging and placing use.
fn dest_dir_ok(side: &Side, dir: &Path) -> Result<Option<PathBuf>, String> {
    match side {
        Side::Local => {
            if !dir.is_dir() {
                return Err(crate::tr!("대상 폴더 없음", "Destination folder missing"));
            }
            let real = std::fs::canonicalize(dir).map_err(|e| e.to_string())?;
            local_dest_safe(&real)?;
            Ok(Some(real))
        }
        Side::Remote(s) => {
            let out = s.exec(&is_dir_cmd(&b(dir))).map_err(|e| e.message())?;
            match out.code {
                Some(0) => Ok(None),
                Some(NOT_THERE) => Err(crate::tr!("대상 폴더 없음", "Destination folder missing")),
                _ => Err(code_err(&out)),
            }
        }
    }
}

/// The same parent-by-parent check remote saves use ([`safe_dirs_check`]:
/// owner, write-granting ACLs, others / non-private group write unless
/// sticky), run with the local `sh` on the physical path `real`.
#[cfg(unix)]
fn local_dest_safe(real: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let os = if cfg!(target_os = "linux") {
        RemoteOs::Linux
    } else {
        RemoteOs::Bsd
    };
    let script = cat(&[&safe_dirs_check(&os, &sh_quote(&b(real))), b"exit 0"]);
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(std::ffi::OsStr::from_bytes(&script))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            crate::tr!(
                "대상 폴더를 확인할 수 없음: {e}",
                "Cannot check the destination folder: {e}"
            )
        })?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(UNSAFE_DIR) => Err(unsafe_msg().into()),
        _ => Err(crate::tr!(
            "대상 폴더를 확인할 수 없음: {}",
            "Cannot check the destination folder: {}",
            first_line(&out.stderr, crate::i18n::t("검사 실패", "check failed"))
        )),
    }
}

#[cfg(not(unix))]
fn local_dest_safe(_real: &Path) -> Result<(), String> {
    Ok(())
}

/// `dev:inode` of the destination and all its parents, on its side.
fn dest_chain(side: &Side, dir: &Path) -> Result<Vec<String>, String> {
    let err = |e: String| {
        crate::tr!(
            "대상 폴더를 확인할 수 없음: {e}",
            "Cannot check the destination folder: {e}"
        )
    };
    match side {
        Side::Local => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let real = std::fs::canonicalize(dir).map_err(|e| err(e.to_string()))?;
                real.ancestors()
                    .map(|a| {
                        std::fs::metadata(a)
                            .map(|m| format!("{}:{}", m.dev(), m.ino()))
                            .map_err(|e| err(e.to_string()))
                    })
                    .collect()
            }
            #[cfg(not(unix))]
            {
                let _ = dir;
                Ok(Vec::new())
            }
        }
        Side::Remote(s) => {
            let out = s
                .exec(&chain_cmd(&s.os, &b(dir)))
                .map_err(|e| err(e.message()))?;
            let out = ran(out).map_err(err)?;
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect())
        }
    }
}

/// `dev:inode` of `path` if it is a real directory.
fn dir_id(side: &Side, path: &Path) -> Result<Option<String>, String> {
    match side {
        Side::Local => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(std::fs::symlink_metadata(path)
                    .ok()
                    .filter(|m| m.is_dir())
                    .map(|m| format!("{}:{}", m.dev(), m.ino())))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Ok(None)
            }
        }
        Side::Remote(s) => {
            let out = ran(s
                .exec(&dir_id_cmd(&s.os, &b(path)))
                .map_err(|e| e.message())?)?;
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok((!id.is_empty()).then_some(id))
        }
    }
}

/// Size of a local item; unreadable parts count as 0. `None` when
/// cancelled.
fn scan_local(p: &Path, stop: &AtomicBool) -> Option<u64> {
    let mut bytes = 0;
    let mut todo = vec![p.to_path_buf()];
    while let Some(q) = todo.pop() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(m) = std::fs::symlink_metadata(&q) else {
            continue;
        };
        if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&q) {
                todo.extend(rd.flatten().map(|e| e.path()));
            }
        } else if m.is_file() {
            bytes += m.len();
        }
    }
    Some(bytes)
}

/// Size of every item; `Ok(None)` when cancelled. A same-server move is a
/// rename, so nothing is read for it.
fn scan(
    spec: &TransferSpec,
    route: &Route,
    cancel: &AtomicBool,
) -> Result<Option<Vec<u64>>, String> {
    let n = spec.names.len();
    if spec.kind == JobKind::Move && matches!(route, Route::SameHost(_)) {
        return Ok(Some(vec![0; n]));
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
                None => Err(crate::tr!(
                    "원격 항목을 확인할 수 없음",
                    "Cannot check the remote items"
                )),
            }
        }
    }
}

/// A staging directory inside the destination.
enum Stage<'a> {
    /// The staging dir and the physical destination dir.
    Local(tempfile::TempDir, PathBuf),
    /// Session, physical destination dir, staging dir.
    Remote(&'a RemoteSession, Vec<u8>, Vec<u8>),
}

/// `real`: the checked physical local destination from [`dest_dir_ok`].
fn make_stage<'a>(
    dst: &'a Side,
    dest_dir: &Path,
    real: Option<&Path>,
) -> Result<Stage<'a>, String> {
    match dst {
        Side::Local => {
            let real = real
                .ok_or_else(|| {
                    crate::tr!(
                        "대상 폴더를 확인하지 않음",
                        "Destination folder not checked"
                    )
                })?
                .to_path_buf();
            tempfile::Builder::new()
                .prefix(STAGE_PREFIX)
                .tempdir_in(&real)
                .map(|t| Stage::Local(t, real))
                .map_err(|e| {
                    crate::tr!(
                        "임시 폴더를 만들 수 없음: {e}",
                        "Cannot create temp folder: {e}"
                    )
                })
        }
        Side::Remote(s) => {
            let out = s
                .exec(&stage_cmd(&s.os, &b(dest_dir)))
                .map_err(|e| e.message())?;
            if out.code == Some(UNSAFE_DIR) {
                return Err(unsafe_msg().into());
            }
            let out = ran(out).map_err(|e| {
                crate::tr!(
                    "임시 폴더를 만들 수 없음: {e}",
                    "Cannot create temp folder: {e}"
                )
            })?;
            parse_stage(&out.stdout)
                .map(|(dir, p)| Stage::Remote(s, dir, p))
                .ok_or_else(|| {
                    crate::tr!(
                        "임시 폴더 경로를 알 수 없음",
                        "Cannot determine the temp folder path"
                    )
                })
        }
    }
}

fn left_behind(path: &str) -> String {
    crate::tr!(
        "임시 폴더가 남음: {path}",
        "Temp folder left behind: {path}"
    )
}

/// How placing went.
#[derive(Debug)]
enum Placed {
    /// In place; maybe with a warning (the stage stayed).
    Done(Option<String>),
    /// The target was taken meanwhile; nothing was replaced.
    Taken,
}

/// Put the staged item onto `dest_dir/name`; see [`place_cmd`] for what
/// `replace` allows. The stage is removed either way, except that when the
/// item cannot be placed and `restore` is given (a same-server move: the
/// staged item *is* the source), it is first moved back there; if that
/// fails too, the stage is kept and named.
fn place(
    stage: Stage,
    name: &OsString,
    replace: bool,
    restore: Option<&Path>,
) -> Result<Placed, String> {
    match stage {
        Stage::Local(t, real) => {
            let (src, to) = (t.path().join(name), real.join(name));
            // rename(2) replaces a file or symlink at the target and never
            // follows it (on Windows also a read-only file, once approved);
            // the no-replace form keeps anything there.
            let res = if replace {
                crate::platform::rename_replace(&src, &to)
            } else {
                crate::fs_ops::rename_noreplace(&src, &to)
            };
            let path = t.path().to_string_lossy().into_owned();
            match res {
                Ok(()) => Ok(Placed::Done(t.close().err().map(|_| left_behind(&path)))),
                Err(e) => {
                    let taken = e.kind() == std::io::ErrorKind::AlreadyExists;
                    let left = t.close().err().map(|_| left_behind(&path));
                    match (taken, left) {
                        (true, None) => Ok(Placed::Taken),
                        (_, Some(w)) => Err(format!("{e} · {w}")),
                        (false, None) => Err(e.to_string()),
                    }
                }
            }
        }
        Stage::Remote(s, dir, p) => {
            let to = cat(&[&into_dir(&dir), &os_bytes(name)]);
            let from = cat(&[&into_dir(&p), &os_bytes(name)]);
            let out = match s.exec(&place_cmd(&s.os, &from, &to, &p, replace)) {
                Ok(o) => o,
                // A same-server move: the stage holds the only copy. Try to
                // put it back (the connection may be gone) and name it.
                Err(e) if restore.is_some() => {
                    return Err(unmove_or_name(s, &p, name, restore, e.message()));
                }
                Err(e) => {
                    let left = discard(Stage::Remote(s, dir, p));
                    return Err(match left {
                        Some(w) => format!("{} · {w}", e.message()),
                        None => e.message(),
                    });
                }
            };
            match out.code {
                Some(0) => return Ok(Placed::Done(None)),
                Some(STAGE_LEFT) => {
                    return Ok(Placed::Done(Some(left_behind(&String::from_utf8_lossy(
                        &p,
                    )))));
                }
                _ => {}
            }
            let e = match out.code {
                Some(APPEARED) => appeared_msg().to_string(),
                Some(DIR_APPEARED) => crate::i18n::t(
                    "대상에 폴더가 생겨 옮기지 않음",
                    "Not moved: a folder appeared in the destination",
                )
                .to_string(),
                Some(NO_SAFE_RENAME) => crate::i18n::t(
                    "이 파일시스템은 안전한 이름 변경을 지원하지 않음",
                    "This filesystem does not support safe renaming",
                )
                .to_string(),
                // both the copy and the item it was replacing are in the
                // stage: never discard it
                Some(ASIDE_LEFT) => {
                    return Err(crate::tr!(
                        "대상에 다른 항목이 생겨 옮기지 않음 · 받은 항목과 원래 항목이 임시 폴더에 남음: {}",
                        "Not moved: something took the destination's place · the copy and the original stayed in the temp folder: {}",
                        String::from_utf8_lossy(&p)
                    ));
                }
                _ => code_err(&out),
            };
            if restore.is_some() {
                // put back without replacing; the stage goes with it
                let m = unmove_or_name(s, &p, name, restore, e.clone());
                return if out.code == Some(APPEARED) && m == e {
                    Ok(Placed::Taken)
                } else {
                    Err(m)
                };
            }
            let left = discard(Stage::Remote(s, dir, p));
            match (out.code, left) {
                (Some(APPEARED), None) => Ok(Placed::Taken),
                (_, Some(w)) => Err(format!("{e} · {w}")),
                (_, None) => Err(e),
            }
        }
    }
}

/// Put a same-server move's staged source back ([`unmove_cmd`]); the
/// error `e`, plus where the item stayed when that did not work.
fn unmove_or_name(
    s: &RemoteSession,
    stage: &[u8],
    name: &OsString,
    back: Option<&Path>,
    e: String,
) -> String {
    let Some(back) = back else {
        return e;
    };
    let cmd = unmove_cmd(&s.os, stage, &os_bytes(name), &b(back));
    match s.exec(&cmd) {
        Ok(o) if o.code == Some(0) => e,
        _ => crate::tr!(
            "{e} · 옮기던 항목이 임시 폴더에 남음: {}",
            "{e} · The item being moved stayed in the temp folder: {}",
            String::from_utf8_lossy(&cat(&[&into_dir(stage), &os_bytes(name)]))
        ),
    }
}

/// Remove a staging dir after a failed transfer; a warning when it stays.
fn discard(stage: Stage) -> Option<String> {
    let (ok, path) = match stage {
        Stage::Local(t, _) => {
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

/// Whether `dir/name` on the sending side (`None`: local) carries ACLs
/// (POSIX, or macOS / BSD ones, which can deny what the mode bits allow).
/// A check that fails counts as yes.
fn has_acls(
    side: Option<&RemoteSession>,
    dir: &Path,
    name: &OsString,
    cancel: &AtomicBool,
) -> bool {
    match side {
        None => crate::platform::tree_has_posix_acl(&dir.join(name), cancel),
        Some(s) => {
            match s.exec_cancellable(&acl_scan_cmd(&s.os, &b(dir), &os_bytes(name)), cancel) {
                Ok(o) if o.code == Some(0) => String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u64>()
                    .map_or(true, |n| n > 0),
                _ => true,
            }
        }
    }
}

/// Whether this side's tar can carry POSIX ACLs (`None`: local). Asked
/// once per connection, and once per run for the local tar.
fn tar_acls_ok(side: Option<&RemoteSession>) -> bool {
    static LOCAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    match side {
        None => *LOCAL.get_or_init(|| {
            cfg!(target_os = "linux")
                && std::process::Command::new("sh")
                    .arg("-c")
                    .arg(std::ffi::OsStr::new(
                        std::str::from_utf8(TAR_ACLS_PROBE).unwrap_or("false"),
                    ))
                    .stdin(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|st| st.success())
        }),
        Some(s) => s.tar_acls(|| {
            matches!(s.os, RemoteOs::Linux)
                && matches!(s.exec(TAR_ACLS_PROBE), Ok(o) if o.code == Some(0))
        }),
    }
}

/// [`restrict_cmd`] on the received item in `stage`.
fn restrict(stage: &Stage, name: &OsString) -> Result<(), String> {
    match stage {
        Stage::Local(t, _) => {
            crate::platform::strip_group_other(&t.path().join(name)).map_err(|e| e.to_string())
        }
        Stage::Remote(s, _, p) => ran(s
            .exec(&restrict_cmd(p, &os_bytes(name)))
            .map_err(|e| e.message())?)
        .map(drop),
    }
}

/// Copy `src_dir/name` into `stage` over the route: a tar stream between
/// the two sides (links stay links), or `cp -Rp` on one server (which
/// keeps ACLs itself). `Ok(Some(warning))` when ACLs could not travel and
/// the copy was made private instead.
fn send(
    route: &Route,
    src_dir: &Path,
    src_id: &str,
    name: &OsString,
    stage: &Stage,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let (n, sd) = (os_bytes(name).into_owned(), b(src_dir));
    let plan = match route {
        Route::Up(d) => Some((None, Some(*d))),
        Route::Down(s) => Some((Some(*s), None)),
        Route::Between(s, d) => Some((Some(*s), Some(*d))),
        Route::SameHost(_) => None,
    }
    .map_or(AclPlan::None, |(from, to)| {
        if has_acls(from, src_dir, name, cancel) {
            acl_plan(true, tar_acls_ok(from), tar_acls_ok(to))
        } else {
            AclPlan::None
        }
    });
    let acls = plan == AclPlan::Keep;
    let remote_in = |d: &RemoteSession| match stage {
        Stage::Remote(_, _, p) => Ok(exec_argv(&d.target, &tar_in_cmd(p, acls))),
        Stage::Local(..) => Err(crate::tr!(
            "임시 폴더가 원격이 아님",
            "temp folder is not remote"
        )),
    };
    let local_in = || match stage {
        Stage::Local(t, _) => Ok(local_tar_in(t.path(), acls)),
        Stage::Remote(..) => Err(crate::tr!(
            "임시 폴더가 로컬이 아님",
            "temp folder is not local"
        )),
    };
    let remote_out = |s: &RemoteSession| {
        exec_argv(
            &s.target,
            &tar_out_cmd(&s.os, &sd, &n, src_id.as_bytes(), acls),
        )
    };
    if cfg!(windows) {
        match (route, stage) {
            (Route::Up(d), Stage::Remote(_, _, p)) => {
                return send_up_packaged(d, src_dir, name, p, cancel).map(|()| None);
            }
            (Route::Between(..), _) => {
                return Err(crate::tr!(
                    "Windows에서는 서버 간 직접 전송을 지원하지 않음: 로컬을 거쳐 복사하세요",
                    "Direct server-to-server transfer is not supported on Windows: copy via a local folder"
                ));
            }
            _ => {}
        }
    }
    // (runner, from argv, the ssh session behind `from`, to argv, ... `to`)
    let (runner, from, from_s, to, to_s) = match route {
        Route::Up(d) => (
            *d,
            local_tar_out(src_dir, name, src_id, acls),
            None,
            remote_in(d)?,
            Some(*d),
        ),
        Route::Down(s) => (*s, remote_out(s), Some(*s), local_in()?, None),
        Route::Between(s, d) => (*s, remote_out(s), Some(*s), remote_in(d)?, Some(*d)),
        Route::SameHost(s) => {
            let Stage::Remote(_, _, p) = stage else {
                return Err(crate::tr!(
                    "임시 폴더가 원격이 아님",
                    "temp folder is not remote"
                ));
            };
            let cmd = cp_cmd(&s.os, &b(src_dir), &os_bytes(name), src_id.as_bytes(), p);
            let out = s.exec_cancellable(&cmd, cancel).map_err(|e| e.message())?;
            if out.code == Some(SRC_MOVED) {
                return Err(crate::tr!(
                    "원본 폴더가 바뀌어 보내지 않음",
                    "Not sent: the source folder changed"
                ));
            }
            return ran(out).map(|_| None);
        }
    };
    let (oa, ob) = runner.pipe(&from, &to, cancel).map_err(|e| e.message())?;
    // ssh exits 255 when the connection itself failed
    for (o, s) in [(&oa, from_s), (&ob, to_s)] {
        if let (Some(255), Some(s)) = (o.code, s) {
            s.set_alive(false);
            return Err(SessionError::Disconnected.message());
        }
    }
    if oa.code == Some(SRC_MOVED) {
        return Err(crate::tr!(
            "원본 폴더가 바뀌어 보내지 않음",
            "Not sent: the source folder changed"
        ));
    }
    match (oa.code, ob.code) {
        (Some(0), Some(0)) => {
            if let Stage::Local(t, _) = stage {
                strip_setid(t.path()).map_err(|e| {
                    crate::tr!(
                        "받은 파일 권한 정리 실패: {e}",
                        "Cannot fix permissions of received files: {e}"
                    )
                })?;
            }
            if plan != AclPlan::Restrict {
                return Ok(None);
            }
            restrict(stage, name).map_err(|e| {
                crate::tr!(
                    "ACL을 옮길 수 없는데 받은 항목의 권한을 줄이지 못함: {e}",
                    "ACLs could not travel and the received item could not be made private: {e}"
                )
            })?;
            Ok(Some(if cfg!(windows) {
                crate::tr!(
                    "ACL을 옮길 수 없음, 받은 폴더의 Windows 권한을 따름: {}",
                    "ACLs could not travel, the destination folder's Windows permissions apply: {}",
                    lossy(name)
                )
            } else {
                crate::tr!(
                    "ACL을 옮길 수 없어 그룹·기타 권한을 뺐음: {}",
                    "ACLs could not travel, group and other permissions removed: {}",
                    lossy(name)
                )
            }))
        }
        // the side that failed first; a reader that died leaves the writer
        // with a broken pipe, and a writer that died leaves a short archive
        (Some(0), _) => Err(code_err(&ob)),
        (_, Some(0)) => Err(code_err(&oa)),
        _ if !oa.stderr.is_empty() => Err(code_err(&oa)),
        _ => Err(code_err(&ob)),
    }
}

/// Fingerprint of `dir/name` and its whole tree on its side (see
/// [`stamp_cmd`]; unix only locally); `None` when it cannot be told.
fn stamp(side: &Side, dir: &Path, name: &OsString, cancel: &AtomicBool) -> Option<String> {
    match side {
        Side::Local => local_stamp(&dir.join(name), cancel),
        Side::Remote(s) => {
            let out = s
                .exec_cancellable(&stamp_cmd(&s.os, &b(dir), &os_bytes(name)), cancel)
                .ok()?;
            let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (out.code == Some(0) && !id.is_empty()).then_some(id)
        }
    }
}

/// Entries in the local tree at `path` (itself included, links not
/// followed); `None` when it cannot be read or on Esc.
fn local_count(path: &Path, cancel: &AtomicBool) -> Option<u64> {
    let mut n = 0;
    let mut todo = vec![path.to_path_buf()];
    while let Some(q) = todo.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        n += 1;
        if std::fs::symlink_metadata(&q).ok()?.is_dir() {
            for e in std::fs::read_dir(&q).ok()? {
                todo.push(e.ok()?.path());
            }
        }
    }
    Some(n)
}

/// Entries in the tree at `dir/name` on its side ([`local_count`],
/// [`count_cmd`]); `None` when it cannot be told.
fn count(side: &Side, dir: &Path, name: &OsString, cancel: &AtomicBool) -> Option<u64> {
    match side {
        Side::Local => local_count(&dir.join(name), cancel),
        Side::Remote(s) => {
            let out = s
                .exec_cancellable(&count_cmd(&b(dir), &os_bytes(name)), cancel)
                .ok()?;
            (out.code == Some(0)).then_some(())?;
            String::from_utf8(out.stdout).ok()?.trim().parse().ok()
        }
    }
}

#[cfg(unix)]
fn local_stamp(path: &Path, cancel: &AtomicBool) -> Option<String> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::MetadataExt;
    let mut all = Vec::new();
    let mut top_ctime = String::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(q) = todo.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let m = std::fs::symlink_metadata(&q).ok()?;
        if m.is_dir() {
            for e in std::fs::read_dir(&q).ok()? {
                todo.push(e.ok()?.path());
            }
        }
        // like the remote stamp, the top entry's ctime is kept apart
        let top = q == path;
        if top {
            top_ctime = format!("{}.{:09}", m.ctime(), m.ctime_nsec());
        }
        all.push((
            q.strip_prefix(path.parent().unwrap_or(path))
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            [m.dev(), m.ino(), m.len()],
            [
                m.mtime(),
                m.mtime_nsec(),
                if top { 0 } else { m.ctime() },
                if top { 0 } else { m.ctime_nsec() },
            ],
        ));
    }
    all.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    all.hash(&mut h);
    Some(format!("{top_ctime} {:016x}:{}", h.finish(), all.len()))
}

/// Hash of a file's content (Windows move fingerprints); `None` when it
/// cannot be read or the job was cancelled.
#[cfg(not(unix))]
fn content_hash(p: &Path, cancel: &AtomicBool) -> Option<u64> {
    use std::hash::Hasher;
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(h.finish());
        }
        h.write(&buf[..n]);
    }
}

/// Windows: no inodes or ctime; the tree's relative paths, kinds, sizes,
/// modification times (100 ns) and file contents.
#[cfg(not(unix))]
fn local_stamp(path: &Path, cancel: &AtomicBool) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut all = Vec::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(q) = todo.pop() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let m = std::fs::symlink_metadata(&q).ok()?;
        if m.is_dir() {
            for e in std::fs::read_dir(&q).ok()? {
                todo.push(e.ok()?.path());
            }
        }
        let mtime = m
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let rel = q.strip_prefix(path).ok()?.to_string_lossy().into_owned();
        // no ctime here: the content itself is hashed, so a same-size edit
        // with its mtime put back still shows
        let content = if m.is_file() {
            content_hash(&q, cancel)?
        } else {
            0
        };
        all.push((rel, m.is_dir(), m.len(), mtime, content));
    }
    all.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    all.hash(&mut h);
    Some(format!("- {:016x}:{}", h.finish(), all.len()))
}

/// Remove a moved source once its copy is in place.
fn delete_source(spec: &TransferSpec, name: &OsString) -> Result<(), String> {
    match &spec.src {
        Side::Local => {
            let src = spec.src_dir.join(name);
            match spec.trash.delete(std::slice::from_ref(&src)).pop() {
                Some((_, r)) => r,
                None => Err(crate::tr!(
                    "휴지통 이동 결과 없음",
                    "No result from moving to trash"
                )),
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

/// Transfer one item. `real`: the checked local destination, if local.
#[allow(clippy::too_many_arguments)]
fn item(
    spec: &TransferSpec,
    route: &Route,
    real: Option<&Path>,
    chain: &[String],
    src_id: &str,
    name: &OsString,
    ctl: &mut dyn TransferCtl,
    r: &mut OpReport,
) -> Step {
    let fail = |m: String| Step::Stop(OpOutcome::Failed(format!("{}: {m}", lossy(name))));
    let src = spec.src_dir.join(name);
    let dest = spec.dest_dir.join(name);
    let mut overwrite = false;
    match kind(&spec.dst, &dest) {
        Ok(Kind::Missing) => {}
        Ok(dk) => {
            let sk = match kind(&spec.src, &src) {
                Ok(k) => k,
                Err(e) => return fail(e),
            };
            if dk == Kind::Dir || sk == Kind::Dir {
                return Step::Skipped(Some(crate::tr!(
                    "폴더는 덮어쓸 수 없어 건너뜀: {}",
                    "Skipped: a folder cannot be overwritten: {}",
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
    // A folder sent into itself (one machine seen through two names, ssh
    // to localhost): the copy would keep copying its own stage.
    match dir_id(&spec.src, &src) {
        Ok(Some(id)) if chain.contains(&id) => {
            return fail(crate::tr!(
                "폴더를 그 안으로 보낼 수 없음",
                "Cannot send a folder into itself"
            ));
        }
        Ok(_) => {}
        Err(e) => return fail(e),
    }
    let stage = match make_stage(&spec.dst, &spec.dest_dir, real) {
        Ok(st) => st,
        Err(e) => return fail(e),
    };
    // A move on one server is a rename, but only within one filesystem:
    // across them `mv` copies and deletes, unchecked, so that case takes the
    // copy, check and delete path of the other routes.
    let on_host_move = is_move
        && match (route, &stage) {
            (Route::SameHost(s), Stage::Remote(_, _, p)) => {
                let cmd = same_fs_cmd(&s.os, &b(&src), p);
                matches!(s.exec(&cmd), Ok(o) if o.code == Some(0))
            }
            _ => false,
        };
    // A move deletes the source only if it is still what was sent: not
    // changed meanwhile, and not replaced by the copy (the same file reached
    // through two connections, ssh to localhost).
    let before = (is_move && !on_host_move)
        .then(|| stamp(&spec.src, &spec.src_dir, name, ctl.cancel_flag()));
    if ctl.is_cancelled() {
        r.warnings.extend(discard(stage));
        return Step::Stop(OpOutcome::Cancelled);
    }
    // On one server a move first renames the source into the stage (a copy
    // there when it crosses filesystems), then places it like any item, so
    // an existing target is never written over in place.
    let sent = if on_host_move {
        let Stage::Remote(s, _, p) = &stage else {
            return fail(crate::tr!(
                "임시 폴더가 원격이 아님",
                "temp folder is not remote"
            ));
        };
        let into = cat(&[&into_dir(p), &os_bytes(name)]);
        let cmd = host_mv_cmd(
            &s.os,
            &b(&spec.src_dir),
            &os_bytes(name),
            src_id.as_bytes(),
            &into,
        );
        // Not cancellable: a killed client could leave the server's mv
        // running while the stage is cleaned up.
        match s.exec_long(&cmd).map_err(|e| e.message()) {
            Ok(o) if o.code == Some(SPECIAL) => Err(special_msg().to_string()),
            Ok(o) if o.code == Some(SRC_MOVED) => Err(crate::tr!(
                "원본 폴더가 바뀌어 보내지 않음",
                "Not sent: the source folder changed"
            )),
            Ok(o) => ran(o).map(|_| None),
            Err(e) => Err(e),
        }
    } else {
        send(
            route,
            &spec.src_dir,
            src_id,
            name,
            &stage,
            ctl.cancel_flag(),
        )
    };
    let sent = sent.map(|w| r.warnings.extend(w));
    if let Err(e) = sent {
        let cancelled = ctl.is_cancelled();
        r.warnings.extend(match (on_host_move, stage) {
            // the stage may hold the source itself: put it back, never rm -rf
            (true, Stage::Remote(s, _, p)) => {
                let cmd = unmove_cmd(&s.os, &p, &os_bytes(name), &b(&src));
                (!matches!(s.exec(&cmd), Ok(o) if o.code == Some(0))).then(|| {
                    crate::tr!(
                        "옮기던 항목이 임시 폴더에 남았을 수 있음: {}",
                        "The item being moved may have stayed in the temp folder: {}",
                        String::from_utf8_lossy(&p)
                    )
                })
            }
            (_, stage) => discard(stage),
        });
        return if cancelled {
            Step::Stop(OpOutcome::Cancelled)
        } else if e == special_msg() {
            Step::Skipped(Some(format!("{}: {}", special_msg(), lossy(name))))
        } else {
            fail(e)
        };
    }
    let restore = on_host_move.then_some(src.as_path());
    // Esc during a same-server move (its rename cannot be interrupted):
    // nothing is placed and the source goes back. An item that arrived in
    // full by any other route is still placed.
    if on_host_move && ctl.is_cancelled() {
        r.warnings.extend(match (restore, stage) {
            (Some(back), Stage::Remote(s, _, p)) => {
                let cmd = unmove_cmd(&s.os, &p, &os_bytes(name), &b(back));
                (!matches!(s.exec(&cmd), Ok(o) if o.code == Some(0))).then(|| {
                    crate::tr!(
                        "옮기던 항목이 임시 폴더에 남았을 수 있음: {}",
                        "The item being moved may have stayed in the temp folder: {}",
                        String::from_utf8_lossy(&p)
                    )
                })
            }
            (_, stage) => discard(stage),
        });
        return Step::Stop(OpOutcome::Cancelled);
    }
    match place(stage, name, overwrite, restore) {
        Ok(Placed::Done(w)) => r.warnings.extend(w),
        Ok(Placed::Taken) => {
            return Step::Skipped(Some(format!("{}: {}", appeared_msg(), lossy(name))));
        }
        Err(e) => {
            return fail(crate::tr!(
                "제자리로 옮기지 못함: {e}",
                "Cannot move into place: {e}"
            ));
        }
    }
    r.overwritten += usize::from(overwrite);
    let Some(before) = before else {
        return Step::Done;
    };
    match kind(&spec.dst, &dest) {
        Ok(Kind::Missing) => {
            return fail(crate::tr!(
                "전송 후 대상이 없어 원본을 지우지 않음",
                "Source kept: the destination is missing after the transfer"
            ));
        }
        Err(e) => {
            return fail(crate::tr!(
                "전송 확인 실패, 원본을 지우지 않음 ({e})",
                "Transfer check failed, source kept ({e})"
            ));
        }
        Ok(_) => {}
    }
    // A remote source is deleted for good, so it is first set aside in a
    // private dir next to it: whatever is checked below is exactly what
    // gets deleted, even if someone recreates the name meanwhile.
    // Names that differ only in case (`a`, `A`) become one on a disk that
    // ignores case: then fewer entries arrived than were sent. (A change to
    // the source meanwhile is caught by its fingerprint below.)
    let sent = count(&spec.src, &spec.src_dir, name, ctl.cancel_flag());
    let got = count(&spec.dst, &spec.dest_dir, name, ctl.cancel_flag());
    if ctl.is_cancelled() {
        return Step::Partial(crate::tr!(
            "취소되어 원본을 지우지 않음: {}",
            "Cancelled, source kept: {}",
            lossy(name)
        ));
    }
    if sent.is_none() || got != sent {
        return Step::Partial(crate::tr!(
            "받은 항목 수를 원본과 맞춰 보지 못해(대소문자만 다른 이름 등) 원본을 지우지 않음: {}",
            "Source kept: cannot match the received item count to the source (names differing only in case?): {}",
            lossy(name)
        ));
    }
    if let Side::Remote(s) = &spec.src {
        return delete_remote_source(s, spec, name, before, ctl);
    }
    let after = stamp(&spec.src, &spec.src_dir, name, ctl.cancel_flag());
    if ctl.is_cancelled() {
        return Step::Partial(crate::tr!(
            "취소되어 원본을 지우지 않음: {}",
            "Cancelled, source kept: {}",
            lossy(name)
        ));
    }
    if before.is_none() || after != before {
        return Step::Partial(changed_msg(name));
    }
    match delete_source(spec, name) {
        Ok(()) => Step::Done,
        Err(e) => Step::Partial(crate::tr!(
            "원본을 지우지 못함: {} ({e})",
            "Cannot delete the source: {} ({e})",
            lossy(name)
        )),
    }
}

/// Move a quarantined source back to where it was, never replacing
/// anything there; `why`, plus where it stayed when that did not work.
fn put_back(
    s: &RemoteSession,
    spec: &TransferSpec,
    qdir: &Path,
    name: &OsString,
    why: String,
) -> String {
    let back = place_cmd(
        &s.os,
        &b(&qdir.join(name)),
        &b(&spec.src_dir.join(name)),
        &b(qdir),
        false,
    );
    match s.exec(&back) {
        Ok(o) if o.code == Some(0) => why,
        _ => crate::tr!(
            "{why} · 원본이 격리 폴더에 남음: {}",
            "{why} · The source stayed in the quarantine folder: {}",
            qdir.join(name).to_string_lossy()
        ),
    }
}

fn changed_msg(name: &OsString) -> String {
    crate::tr!(
        "원본이 전송 중 바뀌었거나 대상과 같은 파일이라 원본을 지우지 않음: {}",
        "Source kept: it changed during the transfer or is the same file as the target: {}",
        lossy(name)
    )
}

/// Move `src_dir/name` into a fresh `.hdir-del.*` dir, re-check its
/// fingerprint there and delete it only if it still matches `before`;
/// otherwise put it back without replacing anything.
fn delete_remote_source(
    s: &RemoteSession,
    spec: &TransferSpec,
    name: &OsString,
    before: Option<String>,
    ctl: &mut dyn TransferCtl,
) -> Step {
    let keep = Step::Partial;
    // "<top ctime> <tree checksum>": the ctime is checked by the quarantine
    // itself, right before the rename that changes it.
    let Some((c0, t0)) = before.as_deref().and_then(|b| b.split_once(' ')) else {
        return keep(changed_msg(name));
    };
    let cmd = quarantine_cmd(&s.os, &b(&spec.src_dir), &os_bytes(name), c0.as_bytes());
    let out = match s.exec(&cmd) {
        Ok(o) if o.code == Some(0) => o,
        Ok(o) if o.code == Some(SOURCE_CHANGED) => return keep(changed_msg(name)),
        Ok(o) if o.code == Some(crate::remote::DELETE_PROTECTED) => {
            return keep(crate::tr!(
                "삭제 거부: 홈 디렉터리 또는 그 상위라 원본을 지우지 않음: {}",
                "Delete refused: the source is the home directory or above, kept: {}",
                lossy(name)
            ));
        }
        Ok(o) if o.code == Some(UNSAFE_DIR) => {
            return keep(crate::tr!(
                "다른 사용자가 바꿀 수 있는 폴더라 원본을 지우지 않음: {}",
                "Source kept: a folder other users can change: {}",
                lossy(name)
            ));
        }
        Ok(o) => {
            return keep(crate::tr!(
                "원본을 지우지 못함: {} ({})",
                "Cannot delete the source: {} ({})",
                lossy(name),
                code_err(&o)
            ));
        }
        Err(e) => {
            return keep(crate::tr!(
                "원본을 지우지 못함: {} ({})",
                "Cannot delete the source: {} ({})",
                lossy(name),
                e.message()
            ));
        }
    };
    let Some(q) = parse_quarantine(&out.stdout) else {
        return keep(crate::tr!(
            "원본을 지우지 못함: {} (격리 폴더를 알 수 없음)",
            "Cannot delete the source: {} (quarantine folder unknown)",
            lossy(name)
        ));
    };
    let qdir = spec.src_dir.join(crate::platform::os_from_bytes(q));
    let after = stamp(&spec.src, &qdir, name, ctl.cancel_flag());
    let tree = after
        .as_deref()
        .and_then(|a| a.split_once(' '))
        .map(|(_, t)| t);
    if ctl.is_cancelled() || tree != Some(t0) {
        let why = if ctl.is_cancelled() {
            crate::tr!(
                "취소되어 원본을 지우지 않음: {}",
                "Cancelled, source kept: {}",
                lossy(name)
            )
        } else {
            changed_msg(name)
        };
        return keep(put_back(s, spec, &qdir, name, why));
    }
    if let Err(e) = remote_ops::delete(s, &qdir, std::slice::from_ref(name)) {
        let why = crate::tr!(
            "원본을 지우지 못함: {} ({e})",
            "Cannot delete the source: {} ({e})",
            lossy(name)
        );
        return keep(put_back(s, spec, &qdir, name, why));
    }
    // The item is gone; an empty dir left behind is only cosmetic.
    let _ = remote_ops::rmdir(s, &qdir);
    Step::Done
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
        return OpReport::failed(crate::tr!(
            "같은 폴더로는 보낼 수 없음",
            "Cannot send into the same folder"
        ));
    }
    if same_target
        && spec
            .names
            .iter()
            .any(|n| spec.dest_dir.starts_with(spec.src_dir.join(n)))
    {
        return OpReport::failed(crate::tr!(
            "폴더를 그 안으로 보낼 수 없음",
            "Cannot send a folder into itself"
        ));
    }
    let real = match dest_dir_ok(&spec.dst, &spec.dest_dir) {
        Ok(r) => r,
        Err(e) => return OpReport::failed(e),
    };
    // Senders check the source folder is still this one before reading.
    let src_id = match src_dir_id(&spec.src, &spec.src_dir) {
        Ok(i) => i,
        Err(e) => return OpReport::failed(e),
    };
    let chain = match dest_chain(&spec.dst, &spec.dest_dir) {
        Ok(c) => c,
        Err(e) => return OpReport::failed(e),
    };
    let scanned = match scan(spec, &route, ctl.cancel_flag()) {
        Ok(Some(v)) => v,
        Ok(None) => return report(OpOutcome::Cancelled),
        Err(e) => return OpReport::failed(e),
    };
    ctl.started(Totals {
        files: spec.names.len() as u64,
        bytes: scanned.iter().sum(),
    });
    let mut r = report(OpOutcome::Done);
    for (name, &sc) in spec.names.iter().zip(&scanned) {
        if ctl.is_cancelled() {
            r.outcome = OpOutcome::Cancelled;
            break;
        }
        let src = spec.src_dir.join(name);
        ctl.on_progress(0, &src);
        match item(
            spec,
            &route,
            real.as_deref(),
            &chain,
            &src_id,
            name,
            ctl,
            &mut r,
        ) {
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
        ctl.on_progress(sc, &src);
        ctl.on_item_done(&src);
    }
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::job::{ConflictAnswer, ConflictPolicy};
    use crate::session::cancelled_msg;
    use crate::testutil::{FakeCmd, code, fail, fake_mktemp, ok, pipe_ends, remote_cmd, session};
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
        let pl = String::from_utf8(place_cmd(&RemoteOs::Bsd, b"/d/S/x", b"/d/x", b"/d/S", true))
            .unwrap();
        for part in [
            "set -- $(ls -di -- '/d/S/x') || exit 1; i=$1; s=; ",
            "if [ -d '/d/x' ] && [ ! -L '/d/x' ]; then exit 47; fi; mv -n -- '/d/x' '/d/S/o' || exit 1;",
            "mv -n -- '/d/S/x' '/d/x' || {",
            "set -- $(ls -di -- '/d/x' 2>/dev/null) x;",
            "mv -n -- '/d/x/x' '/d/S/x'",
            "if [ -n \"$s\" ]; then exit 50; fi; exit 48; fi;",
            "if [ -n \"$s\" ]; then rm -f -- '/d/S/o'; fi; rmdir -- '/d/S' || exit 46",
        ] {
            assert!(pl.contains(part), "missing {part}\nin {pl}");
        }
        assert!(!pl.contains("mv -f"), "never an overwriting mv: {pl}");
        let pl = String::from_utf8(place_cmd(
            &RemoteOs::Bsd,
            b"/d/S/x",
            b"/d/x",
            b"/d/S",
            false,
        ))
        .unwrap();
        for part in [
            "if [ -e '/d/x' ] || [ -L '/d/x' ]; then exit 48; fi; ",
            "if ln -sn -- \"$l\" '/d/x' 2>/dev/null; then \
             r=$(readlink -- '/d/x' 2>/dev/null && echo x); \
             if [ -L '/d/x' ] && [ \"${r%?x}\" = \"$l\" ]; then rm -f -- '/d/S/x'; else \
             b=${l##*/}; if [ -n \"$b\" ]; then",
            "if [ -L '/d/x'/\"$b\" ] && [ \"${q%?x}\" = \"$l\" ]; then rm -f -- '/d/x'/\"$b\"; fi; fi; \
             exit 48; fi;",
            "if ln -- '/d/S/x' '/d/x' 2>/dev/null; then \
             if [ '/d/x' -ef '/d/S/x' ]; then rm -f -- '/d/S/x'; else rm -f -- '/d/x/x'; exit 48; fi;",
            "elif [ -d '/d/S/x' ]; then set -- $(ls -di -- '/d/S/x') || exit 1; i=$1; \
             mv -- '/d/S/x' '/d/x' || exit 1;",
            "else exit 49; fi; rmdir",
            "if [ \"$1\" != \"$i\" ]; then mv -- '/d/x/x' '/d/S/x'; exit 48; fi;",
        ] {
            assert!(pl.contains(part), "missing {part}\nin {pl}");
        }
        assert!(pl.ends_with("rmdir -- '/d/S' || exit 46"), "{pl}");
        // GNU: -T never takes the target as a directory to go into
        let pl = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            b"/d/S/x",
            b"/d/x",
            b"/d/S",
            true,
        ));
        assert_eq!(
            pl.unwrap(),
            "if [ -d '/d/x' ] && [ ! -L '/d/x' ]; then exit 47; fi; \
             mv -fT -- '/d/S/x' '/d/x' || exit 1; rmdir -- '/d/S' || exit 46"
        );
        let pl = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            b"/d/S/x",
            b"/d/x",
            b"/d/S",
            false,
        ));
        let pl = pl.unwrap();
        for part in [
            "if ln -sT -- \"$l\" '/d/x' 2>/dev/null; then rm -f -- '/d/S/x';",
            "if ln -T -- '/d/S/x' '/d/x' 2>/dev/null; then rm -f -- '/d/S/x';",
            "elif [ -d '/d/S/x' ]; then mv -nT -- '/d/S/x' '/d/x' 2>/dev/null || { \
             if [ -e '/d/x' ] || [ -L '/d/x' ]; then exit 48; fi; exit 1; }; \
             if [ -e '/d/S/x' ] || [ -L '/d/S/x' ]; then exit 48; fi; else exit 49; fi; \
             rmdir -- '/d/S' || exit 46",
        ] {
            assert!(pl.contains(part), "missing {part}\nin {pl}");
        }
        let um = String::from_utf8(unmove_cmd(&RemoteOs::Linux, b"/d/S", b"b", b"/s/b")).unwrap();
        assert!(
            um.starts_with("if [ -e '/d/S/b' ] || [ -L '/d/S/b' ]; then if [ -e '/s/b' ]"),
            "{um}"
        );
        assert!(um.ends_with("; else rmdir -- '/d/S'; fi"), "{um}");
        // paths are substituted once: a name containing a slot stays as is
        let odd = String::from_utf8(place_cmd(
            &RemoteOs::Bsd,
            b"/s/a$Tb",
            b"/d/$F",
            b"/d/S",
            false,
        ))
        .unwrap();
        assert!(
            odd.contains("'/s/a$Tb'") && odd.contains("'/d/$F'"),
            "{odd}"
        );
        assert_eq!(
            unstage_cmd(b"/d/.hdir-xfer.1"),
            b"rm -rf -- '/d/.hdir-xfer.1'".to_vec()
        );
        assert_eq!(
            cp_cmd(&RemoteOs::Linux, b"/a", b"x", b"5:6", b"/dst//"),
            b"cd -- '/a' || exit 1; i=$(stat -c %d:%i -- .) || exit 1; \
              [ \"$i\" = '5:6' ] || exit 51; cp -Rp -- ./'x' '/dst/'"
                .to_vec()
        );
        let mv =
            String::from_utf8(host_mv_cmd(&RemoteOs::Linux, b"/a", b"x", b"5:6", b"/S/x")).unwrap();
        assert!(
            mv.ends_with("if [ -L ./'x' ] || [ -f ./'x' ] || [ -d ./'x' ]; then :; else exit 52; fi; mv -- ./'x' '/S/x'"),
            "{mv}"
        );
        assert_eq!(
            String::from_utf8(stamp_cmd(&RemoteOs::Linux, b"/s", b"a b")).unwrap(),
            "cd -- '/s' || exit 1; c=$(stat -c %.9Z -- ./'a b') || exit 1; \
             t=$(stat -c '%d:%i:%s:%.9Y:%n' -- ./'a b') || exit 1; \
             s=$(find ./'a b' -mindepth 1 -exec stat -c '%d:%i:%s:%.9Y:%.9Z:%n' {} +) || exit 1; \
             printf '%s ' \"$c\"; printf '%s\\n%s\\n' \"$t\" \"$s\" | LC_ALL=C sort | cksum"
        );
        let bsd = String::from_utf8(stamp_cmd(&RemoteOs::Bsd, b"/s", b"a")).unwrap();
        assert!(
            bsd.contains("-exec stat -f '%d:%i:%z:%Fm:%Fc:%N' {} +"),
            "{bsd}"
        );
        // the set-aside name never equals the staged item's name
        let o = String::from_utf8(place_cmd(&RemoteOs::Bsd, b"/d/S/o", b"/d/o", b"/d/S", true))
            .unwrap();
        assert!(o.contains("'/d/S/p'") && !o.contains("'/d/S/o' ||"), "{o}");
        assert_eq!(
            tar_out_cmd(&RemoteOs::Linux, b"/s it's", b"-x", b"5:6", false),
            b"cd -- '/s it'\\''s' || exit 1; i=$(stat -c %d:%i -- .) || exit 1; \
              [ \"$i\" = '5:6' ] || exit 51; tar -cf - ./'-x'"
                .to_vec()
        );
        assert_eq!(
            tar_in_cmd(b"/d/.hdir-xfer.1", false),
            b"cd -- '/d/.hdir-xfer.1' || exit 1; tar -xpof - || exit 1; \
              find . -type f \\( -perm -4000 -o -perm -2000 \\) -exec chmod ug-s {} +"
                .to_vec()
        );
        // the paths are arguments, never part of the script
        let out = local_tar_out(
            Path::new("/s 'x'"),
            std::ffi::OsStr::new("-x"),
            "5:6",
            false,
        );
        let tail: Vec<&str> = out[3..].iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(tail, ["sh", "/s 'x'", "5:6", "-x"]);
        assert!(!out[2].to_str().unwrap().contains("/s"), "{:?}", out[2]);
        assert_eq!(
            local_tar_in(Path::new("/st"), false),
            ["tar", "-xpof", "-", "-C", "/st"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn acl_commands_carry_acls_or_strip_group_and_other() {
        let out = tar_out_cmd(&RemoteOs::Linux, b"/s", b"a", b"5:6", true);
        assert!(
            out.ends_with(b"tar --acls -cf - ./'a'"),
            "{}",
            lossy_b(&out)
        );
        let inn = tar_in_cmd(b"/d/S", true);
        assert!(
            inn.starts_with(b"cd -- '/d/S' || exit 1; tar --acls -xpof - || exit 1; "),
            "{}",
            lossy_b(&inn)
        );
        assert_eq!(
            local_tar_in(Path::new("/st"), true),
            ["tar", "--acls", "-xpof", "-", "-C", "/st"]
                .map(OsString::from)
                .to_vec()
        );
        let lo = local_tar_out(Path::new("/s"), std::ffi::OsStr::new("a"), "5:6", true);
        if cfg!(target_os = "linux") {
            assert!(
                lo[2].to_str().unwrap().contains("tar --acls -cf -"),
                "{lo:?}"
            );
        }
        let bsd = String::from_utf8(acl_scan_cmd(&RemoteOs::Bsd, b"/s", b"a")).unwrap();
        assert!(
            bsd.contains("if [ \"$(uname)\" = Darwin ]; then n=$(find ./'a' ! -type l -exec ls -lden -- {} + | grep -c '^ 0: ')"),
            "{bsd}"
        );
        assert!(
            bsd.contains("else n=$(find ./'a' ! -type l -exec ls -dn -- {} +"),
            "{bsd}"
        );
        assert_eq!(
            acl_scan_cmd(&RemoteOs::Linux, b"/s it's", b"-a"),
            b"cd -- '/s it'\\''s' || exit 1; \
              n=$(find ./'-a' ! -type l -exec ls -dn -- {} + | grep -c '^[^ ]\\{10\\}+'); \
              echo $((n))"
                .to_vec()
        );
        assert_eq!(
            restrict_cmd(b"/d/S", b"-a"),
            b"cd -- '/d/S' || exit 1; find ./'-a' ! -type l -exec chmod go-rwx {} +".to_vec()
        );
        assert_eq!(
            TAR_ACLS_PROBE,
            b"tar --acls -cf /dev/null -T /dev/null >/dev/null 2>&1"
        );
    }

    #[test]
    fn acl_plan_keeps_acls_only_when_both_tars_can() {
        assert_eq!(acl_plan(false, false, false), AclPlan::None);
        assert_eq!(acl_plan(false, true, true), AclPlan::None);
        assert_eq!(acl_plan(true, true, true), AclPlan::Keep);
        assert_eq!(acl_plan(true, true, false), AclPlan::Restrict);
        assert_eq!(acl_plan(true, false, true), AclPlan::Restrict);
    }

    fn lossy_b(v: &[u8]) -> String {
        String::from_utf8_lossy(v).into_owned()
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
        assert_eq!(parse_scan(b"1\x002\x00", 2), Some(vec![1024, 2048]));
        assert_eq!(parse_scan(b"x\x00", 1), Some(vec![0]));
        assert_eq!(parse_scan(b"1\x00", 2), None);
        assert_eq!(parse_scan(b"1", 1), None);
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
    fn real_scan_sizes_items_with_odd_names() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a\nb"), vec![0u8; 5000]).unwrap();
        fs::create_dir(t.path().join("-d")).unwrap();
        fs::write(t.path().join("-d/f"), vec![1u8; 9000]).unwrap();
        let names: [&[u8]; 3] = [b"a\nb", b"-d", b"missing"];
        let out = sh(&scan_cmd(&b(t.path()), &names));
        assert!(out.status.success(), "{out:?}");
        let v = parse_scan(&out.stdout, 3).expect("parsed");
        assert!(v[0] >= 4096 && v[1] >= 8192, "{v:?}");
        assert_eq!(v[2], 0);
    }

    #[test]
    fn real_restrict_strips_group_and_other_but_not_through_links() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("-d x");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("f"), "x").unwrap();
        fs::set_permissions(d.join("f"), fs::Permissions::from_mode(0o664)).unwrap();
        fs::write(t.path().join("outside"), "o").unwrap();
        fs::set_permissions(t.path().join("outside"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), d.join("l")).unwrap();
        let out = sh(&restrict_cmd(&b(t.path()), b"-d x"));
        assert!(out.status.success(), "{out:?}");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d.join("f")), 0o600);
        assert_eq!(mode(&d) & 0o077, 0);
        assert_eq!(mode(&t.path().join("outside")), 0o644);
    }

    #[test]
    fn real_acl_scan_counts_nothing_in_a_plain_tree() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("-d")).unwrap();
        fs::write(t.path().join("-d/f\nx"), "x").unwrap();
        let out = sh(&acl_scan_cmd(
            &crate::testutil::local_os(),
            &b(t.path()),
            b"-d",
        ));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0", "{out:?}");
    }

    /// Linux with ACL support: the scan sees an ACL deep inside, and a
    /// `--acls` stream carries it while a plain one drops it.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_acls_are_found_and_carried_by_tar() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        fs::create_dir_all(src.join("d/e")).unwrap();
        fs::write(src.join("d/e/f"), "x").unwrap();
        if !crate::platform::try_test_acl(&src.join("d/e/f")) || !tar_acls_ok(None) {
            eprintln!("no ACL support here, skipped");
            return;
        }
        let out = sh(&acl_scan_cmd(&crate::testutil::local_os(), &b(&src), b"d"));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1", "{out:?}");
        let id = src_dir_id(&Side::Local, &src).unwrap();
        let no = AtomicBool::new(false);
        for acls in [true, false] {
            let stage = private_dir(t.path(), if acls { "with" } else { "without" });
            let (oa, ob) = crate::session::run_pipe_cancellable(
                &local_tar_out(&src, std::ffi::OsStr::new("d"), &id, acls),
                &local_tar_in(&stage, acls),
                std::time::Duration::from_secs(30),
                &no,
            );
            assert_eq!((oa.code, ob.code), (Some(0), Some(0)), "{oa:?} {ob:?}");
            assert_eq!(
                crate::platform::tree_has_posix_acl(&stage.join("d"), &no),
                acls,
                "acls={acls}"
            );
            if acls {
                // the same entries: the group stays shut out
                assert_eq!(
                    crate::platform::test_acl_of(&stage.join("d/e/f")),
                    Some(crate::platform::test_acl_value(65534))
                );
            }
        }
    }

    #[test]
    fn real_a_tar_stream_keeps_links_as_links_and_odd_names() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        let stage = private_dir(t.path(), "stage");
        let d = src.join("-d it's");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/f\nx"), "data").unwrap();
        std::os::unix::fs::symlink(t.path().join("secret"), d.join("ln")).unwrap();
        fs::write(t.path().join("secret"), "never sent").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.join("sub/f\nx"), fs::Permissions::from_mode(0o640)).unwrap();
        let from: Vec<OsString> = vec!["sh".into(), "-c".into(), {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(&tar_out_cmd(
                &crate::testutil::local_os(),
                &b(&src),
                b"-d it's",
                src_dir_id(&Side::Local, &src).unwrap().as_bytes(),
                false,
            ))
            .to_os_string()
        }];
        let no = AtomicBool::new(false);
        let (oa, ob) = crate::session::run_pipe_cancellable(
            &from,
            &local_tar_in(&stage, false),
            std::time::Duration::from_secs(30),
            &no,
        );
        assert_eq!((oa.code, ob.code), (Some(0), Some(0)), "{oa:?} {ob:?}");
        let got = stage.join("-d it's");
        assert_eq!(fs::read(got.join("sub/f\nx")).unwrap(), b"data");
        assert_eq!(
            fs::metadata(got.join("sub/f\nx"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_link(got.join("ln")).unwrap(),
            t.path().join("secret")
        );
        // and the other way: local tar out, shell tar in
        let back = private_dir(t.path(), "back");
        let to: Vec<OsString> = vec!["sh".into(), "-c".into(), {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(&tar_in_cmd(&b(&back), false)).to_os_string()
        }];
        let (oa, ob) = crate::session::run_pipe_cancellable(
            &local_tar_out(
                &src,
                std::ffi::OsStr::new("-d it's"),
                &src_dir_id(&Side::Local, &src).unwrap(),
                false,
            ),
            &to,
            std::time::Duration::from_secs(30),
            &no,
        );
        assert_eq!((oa.code, ob.code), (Some(0), Some(0)), "{oa:?} {ob:?}");
        assert!(
            fs::symlink_metadata(back.join("-d it's/ln"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_dir(&back).unwrap().count(),
            1,
            "no AppleDouble files"
        );
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
    fn real_placing_replaces_links_instead_of_writing_through_them() {
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
            let to = cat(&[&dir, b"/", name.as_bytes()]);
            let from = cat(&[&stage, b"/", name.as_bytes()]);
            let out = sh(&place_cmd(
                &crate::testutil::local_os(),
                &from,
                &to,
                &stage,
                true,
            ));
            assert!(out.status.success(), "{name}: {out:?}");
            let m = fs::symlink_metadata(dest.join(name)).unwrap();
            assert!(m.is_file(), "{name} is a plain file now");
            assert_eq!(fs::read(dest.join(name)).unwrap(), b"new");
            assert!(!sp.exists());
        }
        assert_eq!(fs::read(t.path().join("outside")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&outside_dir).unwrap().count(), 0);
        // a real directory at the target is never moved into
        fs::create_dir(dest.join("real")).unwrap();
        let (dir, stage, sp) = stage_in(&dest);
        fs::write(sp.join("real"), "new").unwrap();
        let to = cat(&[&dir, b"/real"]);
        let from = cat(&[&stage, b"/real"]);
        assert_eq!(
            sh(&place_cmd(
                &crate::testutil::local_os(),
                &from,
                &to,
                &stage,
                true
            ))
            .status
            .code(),
            Some(DIR_APPEARED)
        );
        assert_eq!(fs::read_dir(dest.join("real")).unwrap().count(), 0);
        // a discarded stage goes with its content
        fs::create_dir(sp.join("half")).unwrap();
        fs::write(sp.join("half/x"), "x").unwrap();
        assert!(sh(&unstage_cmd(&stage)).status.success());
        assert!(!sp.exists());
    }

    /// `script` run by `sh` with a `mv` shim first on PATH: just before the
    /// `mv` whose last two arguments are `from` and `to`, it plants `plant`
    /// (a folder holding `victim`, or a link to `outside`) at `to`, as a
    /// racing user would.
    fn sh_with_planting_mv(script: &[u8], t: &Path, plant: &str) -> std::process::Output {
        sh_with_planting("mv", script, t, plant)
    }

    /// [`sh_with_planting_mv`] for any command `cmd` (`mv`, `ln`).
    fn sh_with_planting(cmd: &str, script: &[u8], t: &Path, plant: &str) -> std::process::Output {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let bin = t.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let real = [format!("/bin/{cmd}"), format!("/usr/bin/{cmd}")]
            .into_iter()
            .find(|m| Path::new(m).exists())
            .unwrap();
        let shim = format!(
            "#!/bin/sh\n\
             for a do to=$a; done\n\
             if [ \"$to\" = \"$PLANT_AT\" ] && [ ! -e \"$PLANT_AT.done\" ] && \
             [ \"$(eval echo \\\"\\${{$(($#-1))}}\\\")\" = \"$PLANT_FROM\" ]; then\n\
               : > \"$PLANT_AT.done\"; rm -f \"$PLANT_AT\"\n\
               case $PLANT in\n\
                 dir) mkdir \"$PLANT_AT\" && printf victim > \"$PLANT_AT/$PLANT_NAME\";;\n\
                 link) ln -s \"$PLANT_OUTSIDE\" \"$PLANT_AT\";;\n\
               esac\n\
             fi\n\
             exec {real} \"$@\"\n"
        );
        fs::write(bin.join(cmd), shim).unwrap();
        fs::set_permissions(bin.join(cmd), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::process::Command::new("sh")
            .arg("-c")
            .arg(std::ffi::OsStr::from_bytes(script))
            .env("PATH", path)
            .env("PLANT", plant)
            .output()
            .unwrap()
    }

    /// BSD `mv` has no -T: an approved overwrite must never go into a
    /// folder (or a link to one) that took the target's place meanwhile, and
    /// never lose the file it was replacing.
    #[test]
    fn real_bsd_overwrite_never_lands_in_a_folder_that_appeared() {
        for plant in ["dir", "link"] {
            let t = tempfile::tempdir().unwrap();
            let dest = private_dir(t.path(), "dest");
            let outside = private_dir(t.path(), "outside");
            fs::write(outside.join("a.txt"), "victim").unwrap();
            fs::write(dest.join("a.txt"), "old").unwrap();
            let (dir, stage, sp) = stage_in(&dest);
            fs::write(sp.join("a.txt"), "new").unwrap();
            let to = cat(&[&dir, b"/a.txt"]);
            let from = cat(&[&stage, b"/a.txt"]);
            let script = place_cmd(&RemoteOs::Bsd, &from, &to, &stage, true);
            let script = cat(&[
                b"PLANT_AT=",
                &sh_quote(&to),
                b"; PLANT_FROM=",
                &sh_quote(&from),
                b"; PLANT_NAME=a.txt; PLANT_OUTSIDE=",
                &sh_quote(&b(&outside)),
                b"; export PLANT_AT PLANT_FROM PLANT_NAME PLANT_OUTSIDE; ",
                &script,
            ]);
            let out = sh_with_planting_mv(&script, t.path(), plant);
            assert_ne!(out.status.code(), Some(0), "{plant}: {out:?}");
            assert_eq!(
                fs::read(outside.join("a.txt")).unwrap(),
                b"victim",
                "{plant}"
            );
            if plant == "dir" {
                assert_eq!(fs::read(dest.join("a.txt/a.txt")).unwrap(), b"victim");
            }
            // the new copy and the replaced file both survive in the stage
            let kept: Vec<Vec<u8>> = fs::read_dir(&sp)
                .unwrap()
                .flatten()
                .map(|e| fs::read(e.path()).unwrap_or_default())
                .collect();
            assert!(kept.contains(&b"new".to_vec()), "{plant}: {kept:?}");
            assert!(kept.contains(&b"old".to_vec()), "{plant}: {kept:?}");
            assert_eq!(out.status.code(), Some(ASIDE_LEFT), "{plant}: {out:?}");
        }
    }

    #[test]
    fn a_stage_holding_the_replaced_item_is_never_discarded() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "new").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
                return code(ASIDE_LEFT);
            }
            answer(c, &[], &["/dst/a"], "0\x000\x00")
        });
        let s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::answering(&[ConflictAnswer::Overwrite]));
        assert!(
            matches!(&r.outcome, OpOutcome::Failed(m) if m.contains(STAGE)),
            "{:?}",
            r.outcome
        );
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert!(!f.remote_cmds().contains(&unstage), "{:?}", f.remote_cmds());
        assert_eq!(
            fs::read(t.path().join("a")).unwrap(),
            b"new",
            "move source kept"
        );
    }

    /// A link placed on a free name while a folder takes that name: the
    /// link `ln` made inside the folder is removed again.
    #[test]
    fn real_bsd_link_placing_leaves_nothing_in_a_folder_that_appeared() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        std::os::unix::fs::symlink("target-x", sp.join("a")).unwrap();
        let to = cat(&[&dir, b"/a"]);
        let from = cat(&[&stage, b"/a"]);
        let script = cat(&[
            b"PLANT_AT=",
            &sh_quote(&to),
            b"; PLANT_FROM=target-x; PLANT_NAME=a; export PLANT_AT PLANT_FROM PLANT_NAME; ",
            &place_cmd(&RemoteOs::Bsd, &from, &to, &stage, false),
        ]);
        let out = sh_with_planting("ln", &script, t.path(), "dir");
        assert_eq!(out.status.code(), Some(APPEARED), "{out:?}");
        let inside: Vec<String> = fs::read_dir(dest.join("a"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(inside, ["a"], "only the folder's own file");
        assert_eq!(fs::read(dest.join("a/a")).unwrap(), b"victim");
        assert_eq!(fs::read_link(sp.join("a")).unwrap(), Path::new("target-x"));
    }

    #[test]
    fn real_bsd_overwrite_replaces_files_and_links_in_place() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let outside = private_dir(t.path(), "outside");
        fs::write(dest.join("f"), "old").unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("l")).unwrap();
        for name in ["f", "l", "new-name"] {
            let (dir, stage, sp) = stage_in(&dest);
            fs::write(sp.join(name), "new").unwrap();
            let to = cat(&[&dir, b"/", name.as_bytes()]);
            let from = cat(&[&stage, b"/", name.as_bytes()]);
            let out = sh(&place_cmd(&RemoteOs::Bsd, &from, &to, &stage, true));
            assert!(out.status.success(), "{name}: {out:?}");
            assert!(fs::symlink_metadata(dest.join(name)).unwrap().is_file());
            assert_eq!(fs::read(dest.join(name)).unwrap(), b"new");
            assert!(!sp.exists(), "{name}: stage gone");
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn real_placing_puts_a_dir_link_back_when_the_rename_fails() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let outside_dir = private_dir(t.path(), "outside-dir");
        std::os::unix::fs::symlink(&outside_dir, dest.join("l")).unwrap();
        let (dir, stage, sp) = stage_in(&dest);
        let to = cat(&[&dir, b"/l"]);
        let missing = cat(&[&stage, b"/l"]); // nothing staged: mv fails
        assert_eq!(
            sh(&place_cmd(
                &crate::testutil::local_os(),
                &missing,
                &to,
                &stage,
                true
            ))
            .status
            .code(),
            Some(1)
        );
        assert_eq!(fs::read_link(dest.join("l")).unwrap(), outside_dir);
        assert_eq!(fs::read_dir(&sp).unwrap().count(), 0, "nothing left aside");
    }

    #[test]
    fn real_placing_onto_a_free_name_never_replaces_what_appeared() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        // a file goes in with ln; a directory with mv
        for (name, is_dir) in [("f $F", false), ("d", true)] {
            let (dir, stage, sp) = stage_in(&dest);
            if is_dir {
                fs::create_dir(sp.join(name)).unwrap();
                fs::write(sp.join(name).join("x"), "x").unwrap();
            } else {
                fs::write(sp.join(name), "new").unwrap();
            }
            let to = cat(&[&dir, b"/", name.as_bytes()]);
            let from = cat(&[&stage, b"/", name.as_bytes()]);
            let out = sh(&place_cmd(
                &crate::testutil::local_os(),
                &from,
                &to,
                &stage,
                false,
            ));
            assert!(out.status.success(), "{name}: {out:?}");
            assert!(dest.join(name).exists() && !sp.exists(), "{name}");
        }
        // something appeared meanwhile: kept, refused with APPEARED
        for appeared_dir in [false, true] {
            let name = "taken";
            let _ = fs::remove_file(dest.join(name));
            let _ = fs::remove_dir(dest.join(name));
            if appeared_dir {
                fs::create_dir(dest.join(name)).unwrap();
            } else {
                fs::write(dest.join(name), "theirs").unwrap();
            }
            let (dir, stage, sp) = stage_in(&dest);
            fs::write(sp.join(name), "mine").unwrap();
            let to = cat(&[&dir, b"/", name.as_bytes()]);
            let from = cat(&[&stage, b"/", name.as_bytes()]);
            assert_eq!(
                sh(&place_cmd(
                    &crate::testutil::local_os(),
                    &from,
                    &to,
                    &stage,
                    false
                ))
                .status
                .code(),
                Some(APPEARED)
            );
            if appeared_dir {
                assert_eq!(fs::read_dir(dest.join(name)).unwrap().count(), 0);
            } else {
                assert_eq!(fs::read(dest.join(name)).unwrap(), b"theirs");
            }
            assert!(sh(&unstage_cmd(&stage)).status.success());
        }
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
                matches!(local_dest_safe(d), Err(m) if m == unsafe_msg()),
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
    fn real_stamps_see_any_change_in_the_tree() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/f"), "abc").unwrap();
        fs::write(t.path().join("b"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", d.join("ln")).unwrap();
        let remote = |n: &str| {
            let out = sh(&stamp_cmd(
                &crate::testutil::local_os(),
                &b(t.path()),
                n.as_bytes(),
            ));
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let local = |n: &str| {
            stamp(
                &Side::Local,
                t.path(),
                &OsString::from(n),
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let (r0, l0) = (remote("d"), local("d"));
        assert_eq!(
            (remote("d"), local("d")),
            (r0.clone(), l0.clone()),
            "stable"
        );
        assert_ne!(remote("b"), r0);
        // same size, same second: a deep content change still shows (ctime ns)
        fs::write(d.join("sub/f"), "xyz").unwrap();
        let (r1, l1) = (remote("d"), local("d"));
        assert_ne!(r1, r0, "remote sees a deep change");
        assert_ne!(l1, l0, "local sees a deep change");
        fs::write(d.join("sub/new"), "").unwrap();
        assert_ne!(remote("d"), r1);
        assert_ne!(local("d"), l1);
        assert!(
            stamp(
                &Side::Local,
                t.path(),
                &OsString::from("gone"),
                &AtomicBool::new(false)
            )
            .is_none()
        );
        assert!(
            !sh(&stamp_cmd(
                &crate::testutil::local_os(),
                &b(t.path()),
                b"gone"
            ))
            .status
            .success()
        );
    }

    #[test]
    fn real_placing_a_file_without_hard_links_is_refused_not_raced() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        fs::write(sp.join("f"), "new").unwrap();
        let to = cat(&[&dir, b"/f"]);
        let from = cat(&[&stage, b"/f"]);
        // `ln` fails (no hard links here); a racing writer is behind `mv`
        let script = cat(&[
            b"ln() { return 1; }; mv() { echo theirs > ",
            &sh_quote(&to),
            b"; command mv \"$@\"; }; ",
            &place_cmd(&crate::testutil::local_os(), &from, &to, &stage, false),
        ]);
        assert_eq!(sh(&script).status.code(), Some(NO_SAFE_RENAME));
        assert!(!dest.join("f").exists(), "nothing placed, nothing replaced");
        assert_eq!(
            fs::read(sp.join("f")).unwrap(),
            b"new",
            "the copy stays staged"
        );
    }

    #[test]
    fn real_same_host_copy_into_a_stage_keeps_links_as_links() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        fs::create_dir_all(src.join("d")).unwrap();
        std::os::unix::fs::symlink("/etc", src.join("d/ln")).unwrap();
        let st = t.path().join("st");
        fs::create_dir(&st).unwrap();
        let id = src_dir_id(&Side::Local, &src).unwrap();
        let os = crate::testutil::local_os();
        assert!(
            sh(&cp_cmd(&os, &b(&src), b"d", id.as_bytes(), &b(&st)))
                .status
                .success()
        );
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
    /// A fake transfer stream: unpacking into a local stage writes `new`.
    fn fake_scp(a: &[String]) -> CmdOutput {
        let (src, dst) = pipe_ends(a);
        if !dst.starts_with("prod-web:") && !dst.starts_with("other-host:") {
            let name = Path::new(src.split_once(':').map_or(src.as_str(), |x| x.1))
                .file_name()
                .unwrap()
                .to_owned();
            fs::write(Path::new(&dst).join(name), "new").unwrap();
        }
        ok("")
    }

    /// Remote answers: `dirs` / `files` exist (as directory / file), the
    /// destination folders `/dst` and `/srv` exist, the scan answers `scan`.
    fn answer(c: &str, dirs: &[&str], files: &[&str], scan: &str) -> CmdOutput {
        if c.starts_with("if [ -d '") || c.ends_with("|| exit 46") {
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
        if let Some(o) = crate::testutil::fake_quarantine(c) {
            return o;
        }
        if c.ends_with("&& stat -c %d:%i -- .") {
            return ok("5:6\n"); // the source folder's dev:inode
        }
        if c.contains("| cksum") {
            return ok("1 2\n");
        }
        if c.contains("ls -dn --") {
            return ok("0\n"); // no ACLs
        }
        if c.contains("for f do printf .") {
            return ok("1\n"); // the entry count
        }
        if c.starts_with("cd -- ") && c.contains("du -sk") {
            // the up-front scan has every name, the re-check before sending one
            let list = c.split_once("for f in ").unwrap().1;
            let list = list.split_once("; do").unwrap().0;
            let n = list.matches("' '").count() + 1;
            let recs: Vec<&str> = scan.split_terminator('\0').collect();
            if recs.len() < n {
                return ok(scan);
            }
            let first: String = recs[..n].iter().map(|r| format!("{r}\0")).collect();
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
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            answer(remote_cmd(a).unwrap_or_default(), dirs, files, scan)
        })
    }

    fn scp_pairs(f: &FakeCmd) -> Vec<(String, String)> {
        f.pipe_calls().iter().map(|a| pipe_ends(a)).collect()
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
        let from = format!("{STAGE}/a");
        let place = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            from.as_bytes(),
            b"/dst/a",
            STAGE.as_bytes(),
            false,
        ));
        let place = place.unwrap();
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
            if a[0] == "pipe" {
                let dst = &a[a.len() - 1];
                fs::write(Path::new(dst).join("a"), "ne").unwrap(); // half done
                flag.store(true, Ordering::SeqCst);
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: cancelled_msg().as_bytes().to_vec(),
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
        assert_eq!(f.pipe_calls().len(), 1, "b never starts");
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
            if a[0] == "pipe" {
                flag.store(true, Ordering::SeqCst);
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: cancelled_msg().as_bytes().to_vec(),
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
            if a[0] == "pipe" {
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
    fn items_with_links_travel_as_links() {
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
        assert_eq!((r.outcome, r.copied, r.skipped), (OpOutcome::Done, 2, 0));
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(f.pipe_calls().len(), 2);
    }

    #[test]
    fn a_remote_scan_that_cannot_be_read_stops_before_sending() {
        // two records for three names
        let f = host(&[], &[], "1\x002\x00");
        let t = tempfile::tempdir().unwrap();
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a", "b", "c"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("원격 항목을 확인할 수 없음".into())
        );
        assert!(f.pipe_calls().is_empty());
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
        assert!(f.pipe_calls().is_empty());
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
        assert!(f.pipe_calls().is_empty());
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
        assert!(f.pipe_calls().is_empty());
        let long: Vec<String> = f
            .long_cmds()
            .into_iter()
            .filter(|c| !c.contains("du -sk"))
            .collect();
        let cp = String::from_utf8(cp_cmd(
            &RemoteOs::Linux,
            b"/srv",
            b"a",
            b"5:6",
            STAGE.as_bytes(),
        ));
        let into = format!("{STAGE}/b");
        let into_stage = String::from_utf8(host_mv_cmd(
            &RemoteOs::Linux,
            b"/srv",
            b"b",
            b"5:6",
            into.as_bytes(),
        ));
        let (cp, into_stage) = (cp.unwrap(), into_stage.unwrap());
        assert_eq!(
            long,
            vec![cp, into_stage],
            "a move on one host is a rename, not a copy"
        );
        let moved: Vec<String> = f.remote_cmds()[before..].to_vec();
        let from = format!("{STAGE}/b");
        let mv = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            from.as_bytes(),
            b"/dst/b",
            STAGE.as_bytes(),
            false,
        ));
        assert!(moved.contains(&mv.unwrap()), "{moved:?}");
        assert!(!moved.iter().any(|c| c.contains("du -sk")), "{moved:?}");
        assert!(!moved.iter().any(|c| c.contains("rm -rf")), "{moved:?}");
    }

    /// A server whose `/srv` items carry ACLs (`acl_scan` answers that) and
    /// whose tar answers the `--acls` probe with `probe`.
    fn acl_host(acl_scan: &'static str, probe: i32) -> Arc<FakeCmd> {
        FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("ls -dn --") {
                return ok(acl_scan);
            }
            if c.as_bytes() == TAR_ACLS_PROBE {
                return code(probe);
            }
            answer(c, &[], &[], "0\x000\x00")
        })
    }

    fn to_other(f: &Arc<FakeCmd>) -> Side {
        Side::Remote(Arc::new(RemoteSession::new(
            "other".into(),
            crate::remote::SshTarget {
                host: "other-host".into(),
                port: None,
                control_path: p("/tmp/hdir-1/cm-other"),
            },
            crate::remote::RemoteOs::Linux,
            crate::remote::ScpMode::Sftp,
            Arc::clone(f) as Arc<dyn crate::session::Cmd>,
        )))
    }

    #[test]
    fn acls_travel_between_two_servers_whose_tars_can_carry_them() {
        let (from, to) = (acl_host("2\n", 0), acl_host("0\n", 0));
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a"],
            to_other(&to),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Done);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let joined = from.pipe_calls()[0].join(" ");
        assert!(joined.contains("tar --acls -cf -"), "{joined}");
        assert!(joined.contains("tar --acls -xpof -"), "{joined}");
        assert!(!to.remote_cmds().iter().any(|c| c.contains("go-rwx")));
    }

    #[test]
    fn each_server_is_asked_about_tar_acls_once_per_connection() {
        let (from, to) = (acl_host("1\n", 0), acl_host("0\n", 0));
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a", "b"],
            to_other(&to),
            &p("/dst"),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let probe = String::from_utf8(TAR_ACLS_PROBE.to_vec()).unwrap();
        for f in [&from, &to] {
            let n = f.remote_cmds().iter().filter(|c| **c == probe).count();
            assert_eq!(n, 1, "{:?}", f.remote_cmds());
        }
    }

    #[test]
    fn without_acl_support_on_the_receiver_the_copy_is_made_private() {
        let (from, to) = (acl_host("1\n", 0), acl_host("0\n", 2));
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a"],
            to_other(&to),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Done);
        let joined = from.pipe_calls()[0].join(" ");
        assert!(!joined.contains("--acls"), "{joined}");
        let restrict = String::from_utf8(restrict_cmd(STAGE.as_bytes(), b"a")).unwrap();
        assert!(
            to.remote_cmds().contains(&restrict),
            "{:?}",
            to.remote_cmds()
        );
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("ACL"), "{:?}", r.warnings);
    }

    #[test]
    fn a_copy_that_cannot_be_made_private_is_never_placed_or_moved() {
        for kind in [JobKind::Copy, JobKind::Move] {
            let from = acl_host("1\n", 0);
            let to = FakeCmd::new(|a| {
                let c = remote_cmd(a).unwrap_or_default();
                if c.contains("chmod go-rwx") {
                    return code(1);
                }
                if c.as_bytes() == TAR_ACLS_PROBE {
                    return code(2);
                }
                answer(c, &[], &[], "0\x000\x00")
            });
            let s = spec(
                kind,
                Side::Remote(session(Arc::clone(&from))),
                &p("/srv"),
                &["a"],
                to_other(&to),
                &p("/dst"),
            );
            let r = run(&s, &mut Ctl::new());
            assert!(
                matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("ACL")),
                "{kind:?} {:?}",
                r.outcome
            );
            let cmds = to.remote_cmds();
            assert!(
                !cmds.iter().any(|c| c.ends_with("|| exit 46")),
                "never placed: {cmds:?}"
            );
            let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
            assert!(cmds.contains(&unstage), "stage dropped: {cmds:?}");
            assert!(
                !from.remote_cmds().iter().any(|c| c.contains(".hdir-del.")),
                "{kind:?}: source kept: {:?}",
                from.remote_cmds()
            );
        }
    }

    #[test]
    fn a_source_without_acls_or_a_failed_scan() {
        // none: plain tar, no probes
        let (from, to) = (acl_host("0\n", 0), acl_host("0\n", 0));
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a"],
            to_other(&to),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(!from.pipe_calls()[0].join(" ").contains("--acls"));
        let probe = String::from_utf8(TAR_ACLS_PROBE.to_vec()).unwrap();
        assert!(!to.remote_cmds().contains(&probe));
        // a scan that answers nonsense counts as ACLs present
        let (from, to) = (acl_host("", 0), acl_host("0\n", 0));
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a"],
            to_other(&to),
            &p("/dst"),
        );
        run(&s, &mut Ctl::new());
        assert!(from.pipe_calls()[0].join(" ").contains("tar --acls"));
    }

    #[cfg(unix)]
    #[test]
    fn a_download_with_acls_is_kept_or_made_private_by_the_local_tar() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let from = acl_host("1\n", 0);
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&from))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(r.outcome, OpOutcome::Done);
        let joined = from.pipe_calls()[0].join(" ");
        let mode = fs::metadata(t.path().join("a"))
            .unwrap()
            .permissions()
            .mode();
        if tar_acls_ok(None) {
            assert!(joined.contains("tar --acls -cf -"), "{joined}");
            assert!(joined.contains("--acls -xpof"), "{joined}");
            assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        } else {
            assert!(!joined.contains("--acls"), "{joined}");
            assert_eq!(mode & 0o077, 0, "{mode:o}");
            assert!(r.warnings[0].contains("ACL"), "{:?}", r.warnings);
        }
    }

    #[test]
    fn two_servers_stream_through_their_own_masters_into_the_stage() {
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
        let calls = fs_.pipe_calls();
        // each end goes through its own session's master
        let joined = calls[0].join(" ");
        assert!(
            joined.contains("ControlPath=/tmp/hdir-1/cm-%C -o BatchMode=yes"),
            "{joined}"
        );
        assert!(
            joined.contains("ControlPath=/tmp/hdir-1/cm-other"),
            "{joined}"
        );
        assert_eq!(
            scp_pairs(&fs_),
            vec![("prod-web:/srv/a".into(), format!("other-host:{STAGE}"))]
        );
        let from = format!("{STAGE}/a");
        let place = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            from.as_bytes(),
            b"/dst/a",
            STAGE.as_bytes(),
            false,
        ));
        let place = place.unwrap();
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
            if a[0] == "pipe" {
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
        assert!(f.pipe_calls().is_empty());
        assert!(!f.remote_cmds().iter().any(|c| c.contains("rm -rf")));
    }

    #[test]
    fn a_failed_scp_stops_the_job_and_removes_the_stage() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
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
        assert_eq!((f.pipe_calls().len(), r.copied), (1, 0));
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert_eq!(f.remote_cmds().last(), Some(&unstage));
    }

    #[test]
    fn a_dropped_connection_is_reported_as_such() {
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
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
            if a[0] == "pipe" {
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

    /// Remote `[ -L` answers: missing until the item was placed, then a file.
    fn appears_after_scp() -> Arc<FakeCmd> {
        let sent = Arc::new(AtomicBool::new(false));
        FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
                sent.store(true, Ordering::SeqCst); // in place only now
                return code(0);
            }
            if c.starts_with("if [ -L '") && !c.ends_with("|| exit 46") {
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
            if a[0] == "pipe" {
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
            if a[0] == "pipe" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
                // renaming the (already written) copy into place replaces
                // the source itself, like the real `mv` would
                let staged = src2.with_file_name("staged");
                fs::write(&staged, "only copy").unwrap();
                fs::rename(&staged, &src2).unwrap();
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
            vec![
                "원본이 전송 중 바뀌었거나 대상과 같은 파일이라 원본을 지우지 않음: a".to_string()
            ]
        );
        assert!(trash.trashed.lock().unwrap().is_empty());
        assert_eq!(fs::read(&src).unwrap(), b"only copy");
    }

    #[test]
    fn a_remote_move_over_a_target_checks_the_source_identity() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "old").unwrap();
        let ids = Arc::new(Mutex::new(vec!["1 1\n", "1 1\n"]));
        let i2 = Arc::clone(&ids);
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("| cksum") {
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
    fn a_failed_rename_into_place_keeps_the_source_and_the_old_target() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
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
            if a[0] == "pipe" {
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
        assert_eq!(r.outcome, OpOutcome::Failed(format!("a: {}", unsafe_msg())));
        assert!(f.pipe_calls().is_empty());
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
        assert_eq!(
            r.outcome,
            OpOutcome::Failed(unsafe_msg().into()),
            "checked once, up front"
        );
        assert!(f.pipe_calls().is_empty());
    }

    #[test]
    fn a_local_file_that_appears_during_the_download_is_kept() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().to_path_buf();
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                fs::write(dest.join("a"), "theirs").unwrap(); // someone else
                return fake_scp(a);
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "0\x000\x00")
        });
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let mut c = Ctl::new();
        let r = run(&s, &mut c);
        assert_eq!((r.outcome, r.copied, r.skipped), (OpOutcome::Done, 0, 1));
        assert!(c.conflicts.is_empty(), "it was free when checked");
        assert_eq!(r.warnings, vec![format!("{}: a", appeared_msg())]);
        assert_eq!(fs::read(t.path().join("a")).unwrap(), b"theirs");
        assert_eq!(stages_in(t.path()), 0);
    }

    #[test]
    fn a_remote_name_that_appears_during_the_upload_is_kept() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
                return code(APPEARED);
            }
            answer(c, &[], &[], "")
        });
        let s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["a"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.skipped), (OpOutcome::Done, 1));
        assert_eq!(r.warnings, vec![format!("{}: a", appeared_msg())]);
        assert!(t.path().join("a").exists(), "source kept");
        let unstage = String::from_utf8(unstage_cmd(STAGE.as_bytes())).unwrap();
        assert_eq!(f.remote_cmds().last(), Some(&unstage));
    }

    #[test]
    fn a_source_changed_during_a_move_is_kept() {
        let t = tempfile::tempdir().unwrap();
        // "<top ctime> <tree checksum>": the tree changed after it was sent
        let stamps = Arc::new(Mutex::new(vec!["7.1 11 2\n", "8.2 99 2\n"]));
        let s2 = Arc::clone(&stamps);
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("| cksum") {
                return ok(s2.lock().unwrap().remove(0));
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
        assert_eq!((r.outcome, r.partial), (OpOutcome::Done, 1));
        assert!(r.warnings[0].contains("바뀌었거나"), "{:?}", r.warnings);
        assert!(stamps.lock().unwrap().is_empty(), "both fingerprints taken");
        let cmds = f.remote_cmds();
        assert!(!cmds.iter().any(|c| c.contains("rm -rf")), "source kept");
        assert!(
            cmds.iter().any(|c| c.starts_with("if [ -e '/srv/a' ]")),
            "put back from the quarantine: {cmds:?}"
        );
        assert_eq!(
            fs::read(t.path().join("a")).unwrap(),
            b"new",
            "copy in place"
        );
    }

    /// A remote that answers the entry count with `n` and the rest like
    /// [`answer`].
    fn counting(n: &'static str) -> Arc<FakeCmd> {
        counting_with(n, &[])
    }

    fn counting_with(n: &'static str, dirs: &'static [&'static str]) -> Arc<FakeCmd> {
        FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("ls -dn --") {
                return ok("0\n"); // no ACLs
            }
            if c.contains("for f do printf .") {
                return ok(n);
            }
            answer(c, dirs, &[], "0\x000\x00")
        })
    }

    #[test]
    fn a_move_that_arrives_with_fewer_entries_keeps_the_remote_source() {
        // `a` and `A` in one remote folder land as one file on a disk that
        // ignores case: the remote tree has 3 entries, 1 arrived.
        let t = tempfile::tempdir().unwrap();
        let f = counting("3\n");
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.partial), (OpOutcome::Done, 1));
        assert!(r.warnings[0].contains("대소문자"), "{:?}", r.warnings);
        let cmds = f.remote_cmds();
        assert!(!cmds.iter().any(|c| c.contains("rm -rf")), "source kept");
        assert!(
            !cmds.iter().any(|c| c.contains(QUARANTINE_PREFIX)),
            "not even set aside: {cmds:?}"
        );
        assert_eq!(fs::read(t.path().join("a")).unwrap(), b"new");
        // a count that cannot be told keeps it too
        let t = tempfile::tempdir().unwrap();
        let f = counting("");
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        assert_eq!(run(&s, &mut Ctl::new()).partial, 1);
        // the same move with every entry there goes ahead
        let t = tempfile::tempdir().unwrap();
        let f = counting("1\n");
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.partial),
            (OpOutcome::Done, 0),
            "{:?}",
            r.warnings
        );
        assert!(f.remote_cmds().iter().any(|c| c.contains("rm -rf")));
    }

    #[test]
    fn an_upload_that_arrives_with_fewer_entries_keeps_the_local_source() {
        // a remote disk that ignores case (macOS) merges `a` and `A`
        fn up(n: &'static str) -> Arc<FakeCmd> {
            let sent = Arc::new(AtomicBool::new(false));
            FakeCmd::new(move |a| {
                if a[0] == "pipe" {
                    sent.store(true, Ordering::SeqCst);
                    return ok("");
                }
                let c = remote_cmd(a).unwrap_or_default();
                if c.contains("ls -dn --") {
                    return ok("0\n"); // no ACLs
                }
                if c.contains("for f do printf .") {
                    return ok(n);
                }
                if c.starts_with("if [ -L '") && !c.ends_with("|| exit 46") {
                    // there once it was sent
                    return code(if sent.load(Ordering::SeqCst) {
                        IS_DIR
                    } else {
                        NOT_THERE
                    });
                }
                answer(c, &[], &[], "")
            })
        }
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        fs::write(t.path().join("d/a"), "1").unwrap();
        fs::write(t.path().join("d/b"), "2").unwrap();
        let s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["d"],
            Side::Remote(session(up("2\n"))),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.partial),
            (OpOutcome::Done, 1),
            "{:?}",
            r.warnings
        );
        assert!(r.warnings[0].contains("대소문자"), "{:?}", r.warnings);
        assert!(t.path().join("d/b").exists(), "source kept");
        let trash = Arc::new(FakeTrash::new());
        let mut s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["d"],
            Side::Remote(session(up("3\n"))),
            &p("/dst"),
        );
        s.trash = Arc::clone(&trash) as Arc<dyn TrashBackend>;
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.partial),
            (OpOutcome::Done, 0),
            "{:?}",
            r.warnings
        );
        assert_eq!(trash.trashed.lock().unwrap().len(), 1, "moved to the trash");
    }

    #[test]
    fn real_count_has_every_entry_once() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("d it's/sub")).unwrap();
        fs::write(t.path().join("d it's/sub/f"), "abc").unwrap();
        fs::write(t.path().join("d it's/two\nlines"), "x").unwrap();
        let out = sh(&count_cmd(&b(t.path()), b"d it's"));
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "4\n");
        let no = AtomicBool::new(false);
        assert_eq!(local_count(&t.path().join("d it's"), &no), Some(4));
        assert!(!sh(&count_cmd(&b(t.path()), b"gone")).status.success());
    }

    #[test]
    fn a_local_destination_reached_through_a_link_is_used_by_its_real_path() {
        let t = tempfile::tempdir().unwrap();
        let real = private_dir(t.path(), "real");
        std::os::unix::fs::symlink(&real, t.path().join("via")).unwrap();
        let f = host(&[], &[], "0\x000\x00");
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            &t.path().join("via"),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let staged = PathBuf::from(&scp_pairs(&f)[0].1);
        assert_eq!(
            staged.parent(),
            Some(fs::canonicalize(&real).unwrap().as_path())
        );
        assert_eq!(fs::read(real.join("a")).unwrap(), b"new");
    }

    #[test]
    fn a_folder_changed_deep_inside_during_a_move_is_kept() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("d/sub")).unwrap();
        fs::write(t.path().join("d/sub/f"), "abc").unwrap();
        let inner = t.path().join("d/sub/f");
        let sent = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&sent);
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                fs::write(&inner, "xyz").unwrap(); // same size, edited meanwhile
                s2.store(true, Ordering::SeqCst);
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("ls -dn --") {
                return ok("0\n"); // no ACLs
            }
            if c.contains("for f do printf .") {
                return ok("3\n"); // all arrived
            }
            if c.starts_with("if [ -L '") && !c.ends_with("|| exit 46") {
                return code(if s2.load(Ordering::SeqCst) {
                    IS_DIR
                } else {
                    NOT_THERE
                });
            }
            answer(c, &[], &[], "")
        });
        let trash = Arc::new(FakeTrash::new());
        let mut s = spec(
            JobKind::Move,
            Side::Local,
            t.path(),
            &["d"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/dst"),
        );
        s.trash = Arc::clone(&trash) as Arc<dyn TrashBackend>;
        let r = run(&s, &mut Ctl::new());
        assert!(sent.load(Ordering::SeqCst));
        assert_eq!((r.outcome, r.partial), (OpOutcome::Done, 1));
        assert!(r.warnings[0].contains("바뀌었거나"), "{:?}", r.warnings);
        assert!(trash.trashed.lock().unwrap().is_empty());
        assert_eq!(fs::read(t.path().join("d/sub/f")).unwrap(), b"xyz");
    }

    #[test]
    fn real_a_folder_appearing_right_before_ln_gets_nothing_put_inside() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        fs::write(sp.join("f"), "mine").unwrap();
        let to = cat(&[&dir, b"/f"]);
        let from = cat(&[&stage, b"/f"]);
        // someone makes `f` a directory between the last check and `ln`
        let script = cat(&[
            b"ln() { mkdir -- ",
            &sh_quote(&to),
            b"; command ln \"$@\"; }; ",
            &place_cmd(&crate::testutil::local_os(), &from, &to, &stage, false),
        ]);
        assert_eq!(sh(&script).status.code(), Some(APPEARED));
        assert_eq!(
            fs::read_dir(dest.join("f")).unwrap().count(),
            0,
            "nothing inside"
        );
        assert_eq!(fs::read(sp.join("f")).unwrap(), b"mine");
    }

    #[test]
    fn a_failed_same_server_move_puts_the_source_back_without_replacing() {
        let f = FakeCmd::new(|a| {
            let c = remote_cmd(a).unwrap_or_default();
            if c.ends_with("|| exit 46") {
                return code(APPEARED);
            }
            answer(c, &[], &[], "")
        });
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!((r.outcome, r.skipped), (OpOutcome::Done, 1));
        let cmds = f.remote_cmds();
        let back = String::from_utf8(unmove_cmd(
            &RemoteOs::Linux,
            STAGE.as_bytes(),
            b"b",
            b"/srv/b",
        ));
        assert!(
            cmds.contains(&back.unwrap()),
            "put back without replacing: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.starts_with("rm -rf")),
            "the stage holds the source: {cmds:?}"
        );
    }

    #[test]
    fn a_same_server_move_that_fails_never_removes_the_stage_recursively() {
        let f = FakeCmd::new(|a| {
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("; mv -- ./'b' ") {
                return code(255); // connection lost mid-move
            }
            answer(c, &[], &[], "")
        });
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert!(matches!(r.outcome, OpOutcome::Failed(_)), "{:?}", r.outcome);
        assert!(!f.remote_cmds().iter().any(|c| c.starts_with("rm -rf")));
        assert!(
            r.warnings[0].contains("임시 폴더에 남았을 수 있음"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn esc_while_checking_the_source_before_a_move_stops_it() {
        let t = tempfile::tempdir().unwrap();
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return ok("");
            }
            let cm = remote_cmd(a).unwrap_or_default();
            if cm.contains("| cksum") {
                flag.store(true, Ordering::SeqCst);
                return CmdOutput {
                    code: None,
                    stdout: vec![],
                    stderr: cancelled_msg().as_bytes().to_vec(),
                };
            }
            answer(cm, &[], &[], "0\x000\x00")
        });
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        assert!(f.pipe_calls().is_empty());
        assert!(
            f.cancellable
                .lock()
                .unwrap()
                .iter()
                .any(|a| remote_cmd(a).is_some_and(|c| c.contains("| cksum"))),
            "the source check goes through the cancellable runner"
        );
    }

    #[test]
    fn a_folder_into_itself_is_refused_on_one_host_reached_twice() {
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
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&fa))),
            &p("/srv"),
            &["d"],
            Side::Remote(Arc::new(twin)),
            &p("/srv/d/inner"),
        );
        let r = run(&s, &mut Ctl::new());
        assert!(matches!(&r.outcome, OpOutcome::Failed(m) if m.contains("그 안으로")));
    }

    #[test]
    fn real_a_staged_link_is_recreated_and_never_replaces_a_file() {
        for os in [RemoteOs::Bsd, crate::testutil::local_os()] {
            let t = tempfile::tempdir().unwrap();
            let dest = private_dir(t.path(), "dest");
            let (dir, stage, sp) = stage_in(&dest);
            std::os::unix::fs::symlink("target\nwith newline", sp.join("ln")).unwrap();
            let to = cat(&[&dir, b"/ln"]);
            let from = cat(&[&stage, b"/ln"]);
            let out = sh(&place_cmd(&os, &from, &to, &stage, false));
            assert!(out.status.success(), "{os:?} {out:?}");
            assert_eq!(
                fs::read_link(dest.join("ln")).unwrap(),
                PathBuf::from("target\nwith newline")
            );
            assert!(!sp.exists());
            // a file took the name meanwhile: kept
            let (dir, stage, sp) = stage_in(&dest);
            std::os::unix::fs::symlink("x", sp.join("f")).unwrap();
            let to = cat(&[&dir, b"/f"]);
            let from = cat(&[&stage, b"/f"]);
            let script = cat(&[
                // past the first check: the file appears right before ln
                b"ln() { printf theirs > ",
                &sh_quote(&to),
                b"; command ln \"$@\"; }; ",
                &place_cmd(&os, &from, &to, &stage, false)[..],
            ]);
            assert_eq!(sh(&script).status.code(), Some(APPEARED), "{os:?}");
            assert_eq!(fs::read(dest.join("f")).unwrap(), b"theirs");
        }
    }

    #[test]
    fn real_a_folder_never_ends_up_inside_one_that_appeared() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        fs::create_dir(sp.join("d")).unwrap();
        fs::write(sp.join("d/x"), "mine").unwrap();
        let to = cat(&[&dir, b"/d"]);
        let from = cat(&[&stage, b"/d"]);
        // without -T: `d` appears as a directory right before the rename
        let script = cat(&[
            b"m=0; mv() { if [ $m = 0 ]; then m=1; mkdir -- ",
            &sh_quote(&to),
            b"; fi; command mv \"$@\"; }; ",
            &place_cmd(&RemoteOs::Bsd, &from, &to, &stage, false),
        ]);
        assert_eq!(sh(&script).status.code(), Some(APPEARED));
        assert_eq!(
            fs::read_dir(dest.join("d")).unwrap().count(),
            0,
            "theirs, empty"
        );
        assert_eq!(
            fs::read(sp.join("d/x")).unwrap(),
            b"mine",
            "ours back in the stage"
        );
    }

    #[test]
    fn real_putting_a_moved_source_back_never_replaces_a_new_one() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let src_dir = private_dir(t.path(), "src");
        let (_, stage, sp) = stage_in(&dest);
        fs::write(sp.join("b"), "moved").unwrap();
        let src = b(&src_dir.join("b"));
        fs::write(src_dir.join("b"), "new one").unwrap();
        let os = crate::testutil::local_os();
        assert_eq!(
            sh(&unmove_cmd(&os, &stage, b"b", &src)).status.code(),
            Some(APPEARED)
        );
        assert_eq!(fs::read(src_dir.join("b")).unwrap(), b"new one");
        assert_eq!(
            fs::read(sp.join("b")).unwrap(),
            b"moved",
            "kept in the stage"
        );
        // free again: it goes back and the stage goes
        fs::remove_file(src_dir.join("b")).unwrap();
        assert!(sh(&unmove_cmd(&os, &stage, b"b", &src)).status.success());
        assert_eq!(fs::read(src_dir.join("b")).unwrap(), b"moved");
        assert!(!sp.exists());
        // nothing staged: only the stage goes
        let (_, stage, sp) = stage_in(&dest);
        assert!(sh(&unmove_cmd(&os, &stage, b"b", &src)).status.success());
        assert!(!sp.exists());
    }

    #[test]
    fn real_a_folder_on_this_os_is_never_placed_over_one_that_appeared() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        fs::create_dir(sp.join("d")).unwrap();
        fs::write(sp.join("d/x"), "mine").unwrap();
        let to = cat(&[&dir, b"/d"]);
        let from = cat(&[&stage, b"/d"]);
        let script = cat(&[
            b"m=0; mv() { if [ $m = 0 ]; then m=1; mkdir -- ",
            &sh_quote(&to),
            b"; fi; command mv \"$@\"; }; ",
            &place_cmd(&crate::testutil::local_os(), &from, &to, &stage, false),
        ]);
        assert_eq!(sh(&script).status.code(), Some(APPEARED));
        assert_eq!(
            fs::read_dir(dest.join("d")).unwrap().count(),
            0,
            "theirs, empty"
        );
        assert_eq!(fs::read(sp.join("d/x")).unwrap(), b"mine");
    }

    #[test]
    fn real_special_files_are_not_placed_without_a_safe_rename() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let (dir, stage, sp) = stage_in(&dest);
        let fifo = sp.join("p");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let to = cat(&[&dir, b"/p"]);
        let from = cat(&[&stage, b"/p"]);
        for os in [RemoteOs::Bsd, RemoteOs::Linux] {
            assert_eq!(
                sh(&place_cmd(&os, &from, &to, &stage, false)).status.code(),
                Some(NO_SAFE_RENAME),
                "{os:?}"
            );
        }
        assert!(!dest.join("p").exists());
    }

    #[test]
    fn real_a_shared_sticky_destination_is_refused_without_gnu_tools() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let shared = private_dir(t.path(), "shared");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1700)).unwrap();
        let out = sh(&stage_cmd(&RemoteOs::Bsd, &b(&shared)));
        assert_eq!(out.status.code(), Some(UNSAFE_DIR));
        assert_eq!(fs::read_dir(&shared).unwrap().count(), 0);
        if cfg!(target_os = "linux") {
            let out = sh(&stage_cmd(&RemoteOs::Linux, &b(&shared)));
            assert!(out.status.success(), "-T makes sticky dirs fine: {out:?}");
        }
    }

    #[test]
    fn a_folder_sent_into_itself_through_another_name_is_refused() {
        // "ssh localhost": the remote destination is inside the local source
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        fs::create_dir_all(d.join("inner")).unwrap();
        let id = {
            use std::os::unix::fs::MetadataExt;
            let m = fs::metadata(&d).unwrap();
            format!("{}:{}", m.dev(), m.ino())
        };
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return ok("");
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("; c=$(pwd -P && echo x)") {
                return ok(&format!("7:7\n{id}\n2:2\n"));
            }
            answer(c, &[], &[], "")
        });
        let s = spec(
            JobKind::Copy,
            Side::Local,
            t.path(),
            &["d"],
            Side::Remote(session(Arc::clone(&f))),
            &p("/alias/d/inner"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            r.outcome,
            OpOutcome::Failed("d: 폴더를 그 안으로 보낼 수 없음".into())
        );
        assert!(f.pipe_calls().is_empty());
    }

    #[test]
    fn real_destination_parents_and_folder_ids_line_up() {
        let t = tempfile::tempdir().unwrap();
        let inner = t.path().join("d/inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(t.path().join("f"), "x").unwrap();
        let os = crate::testutil::local_os();
        let out = sh(&chain_cmd(&os, &b(&inner)));
        assert!(out.status.success(), "{out:?}");
        let remote: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(remote, dest_chain(&Side::Local, &inner).unwrap());
        let out = sh(&dir_id_cmd(&os, &b(&t.path().join("d"))));
        let did = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert!(remote.contains(&did), "d is a parent of d/inner");
        assert_eq!(
            Some(did),
            dir_id(&Side::Local, &t.path().join("d")).unwrap()
        );
        assert!(
            sh(&dir_id_cmd(&os, &b(&t.path().join("f"))))
                .stdout
                .is_empty()
        );
        assert_eq!(dir_id(&Side::Local, &t.path().join("f")).unwrap(), None);
    }

    #[test]
    fn a_quarantine_reply_must_be_a_plain_prefixed_name() {
        assert_eq!(
            parse_quarantine(b".hdir-del.Ab12\0"),
            Some(b".hdir-del.Ab12".to_vec())
        );
        for bad in [
            &b".hdir-del.\0"[..],
            b"../x\0",
            b".hdir-del.a/b\0",
            b"/tmp/.hdir-del.a\0",
            b".hdir-del.a",
            b".hdir-del.a\0junk",
            b"",
        ] {
            assert_eq!(parse_quarantine(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn real_a_set_aside_source_keeps_its_fingerprint_until_it_changes() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        fs::create_dir_all(src.join("d it's/sub")).unwrap();
        fs::write(src.join("d it's/sub/f"), "abc").unwrap();
        let os = crate::testutil::local_os();
        let name = OsString::from("d it's");
        let remote = |dir: &Path| {
            let out = sh(&stamp_cmd(&os, &b(dir), b"d it's"));
            assert!(out.status.success(), "{out:?}");
            let s = String::from_utf8(out.stdout).unwrap();
            let (c, t) = s.trim().split_once(' ').unwrap();
            (c.to_string(), t.to_string())
        };
        let tree = |s: String| s.split_once(' ').unwrap().1.to_string();
        let no = AtomicBool::new(false);
        let (c0, t0) = remote(&src);
        let l0 = stamp(&Side::Local, &src, &name, &no).unwrap();
        // a stale ctime (the item changed): refused, nothing moved
        let out = sh(&quarantine_cmd(&os, &b(&src), b"d it's", b"1.000000000"));
        assert_eq!(out.status.code(), Some(SOURCE_CHANGED));
        assert!(src.join("d it's").exists());
        let out = sh(&quarantine_cmd(&os, &b(&src), b"d it's", c0.as_bytes()));
        assert!(out.status.success(), "{out:?}");
        let q = parse_quarantine(&out.stdout).expect("quarantine dir");
        let qdir = src.join(String::from_utf8(q).unwrap());
        assert!(!src.join("d it's").exists() && qdir.join("d it's/sub/f").exists());
        assert_eq!(remote(&qdir).1, t0, "a rename keeps the tree part");
        assert_eq!(
            tree(stamp(&Side::Local, &qdir, &name, &no).unwrap()),
            tree(l0),
            "locally too"
        );
        fs::write(qdir.join("d it's/sub/f"), "xyz").unwrap();
        assert_ne!(remote(&qdir).1, t0, "a change below still shows");
        // a missing source: nothing is created, the command fails
        let out = sh(&quarantine_cmd(&os, &b(&src), b"gone", b"0"));
        assert!(!out.status.success());
        assert_eq!(
            fs::read_dir(&src).unwrap().count(),
            1,
            "only the first quarantine dir"
        );
    }

    #[test]
    fn real_a_top_file_edited_with_its_mtime_restored_is_caught() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        let f = src.join("f");
        fs::write(&f, "abc").unwrap();
        let os = crate::testutil::local_os();
        let no = AtomicBool::new(false);
        let name = OsString::from("f");
        let l0 = stamp(&Side::Local, &src, &name, &no).unwrap();
        let out = sh(&stamp_cmd(&os, &b(&src), b"f"));
        let c0 = String::from_utf8(out.stdout)
            .unwrap()
            .split_once(' ')
            .unwrap()
            .0
            .to_string();
        let mtime = fs::metadata(&f).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&f, "xyz").unwrap(); // same size
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(
            stamp(&Side::Local, &src, &name, &no).unwrap(),
            l0,
            "local sees it"
        );
        let out = sh(&quarantine_cmd(&os, &b(&src), b"f", c0.as_bytes()));
        assert_eq!(out.status.code(), Some(SOURCE_CHANGED), "remote refuses it");
        assert!(f.exists());
    }

    #[test]
    fn real_a_source_folder_others_can_swap_is_not_deleted_from() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let open = private_dir(t.path(), "open");
        fs::write(open.join("f"), "x").unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let os = crate::testutil::local_os();
        let out = sh(&stamp_cmd(&os, &b(&open), b"f"));
        let c0 = String::from_utf8(out.stdout)
            .unwrap()
            .split_once(' ')
            .unwrap()
            .0
            .to_string();
        let out = sh(&quarantine_cmd(&os, &b(&open), b"f", c0.as_bytes()));
        assert_eq!(out.status.code(), Some(UNSAFE_DIR));
        assert!(open.join("f").exists());
        assert_eq!(fs::read_dir(&open).unwrap().count(), 1);
    }

    #[test]
    fn a_remote_source_is_deleted_from_its_quarantine_only() {
        let t = tempfile::tempdir().unwrap();
        let f = host(&[], &[], "0\x000\x00");
        let s = spec(
            JobKind::Move,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["a"],
            Side::Local,
            t.path(),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.copied),
            (OpOutcome::Done, 1),
            "{:?}",
            r.warnings
        );
        let cmds = f.remote_cmds();
        let qi = cmds
            .iter()
            .position(|c| {
                c.starts_with("cd -- '/srv' || exit 1; d0=") && c.contains("mktemp -d .hdir-del.")
            })
            .expect("set aside first");
        let stamps: Vec<usize> = (0..cmds.len())
            .filter(|&i| cmds[i].contains("| cksum"))
            .collect();
        assert_eq!(stamps.len(), 2);
        assert!(stamps[1] > qi && cmds[stamps[1]].starts_with("cd -- '/srv/.hdir-del.Q1'"));
        let rm = cmds
            .iter()
            .position(|c| c.contains("rm -rf"))
            .expect("deleted");
        assert!(rm > stamps[1]);
        assert!(cmds[rm].contains("/srv/.hdir-del.Q1"), "{}", cmds[rm]);
        assert!(cmds.iter().any(|c| c == "rmdir -- '/srv/.hdir-del.Q1'"));
    }

    #[test]
    fn a_remote_source_changed_by_the_time_it_is_set_aside_goes_back() {
        let t = tempfile::tempdir().unwrap();
        let stamps = Arc::new(Mutex::new(vec!["1 2\n", "9 9\n"]));
        let s2 = Arc::clone(&stamps);
        let f = FakeCmd::new(move |a| {
            if a[0] == "pipe" {
                return fake_scp(a);
            }
            let c = remote_cmd(a).unwrap_or_default();
            if c.contains("| cksum") {
                return ok(s2.lock().unwrap().remove(0));
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
        assert_eq!((r.outcome, r.partial), (OpOutcome::Done, 1));
        let back = String::from_utf8(place_cmd(
            &RemoteOs::Linux,
            b"/srv/.hdir-del.Q1/a",
            b"/srv/a",
            b"/srv/.hdir-del.Q1",
            false,
        ))
        .unwrap();
        let cmds = f.remote_cmds();
        assert!(cmds.contains(&back), "put back without replacing: {cmds:?}");
        assert!(
            !cmds.iter().any(|c| c.contains("rm -rf")),
            "nothing deleted"
        );
    }

    #[test]
    fn real_the_home_or_a_parent_of_it_is_never_set_aside() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        fs::create_dir_all(src.join("top/me")).unwrap();
        fs::create_dir(src.join("other")).unwrap();
        let os = crate::testutil::local_os();
        let home = fs::canonicalize(src.join("top/me")).unwrap();
        let run_q = |name: &[u8]| {
            let st = sh(&stamp_cmd(&os, &b(&src), name));
            let c0 = String::from_utf8(st.stdout)
                .unwrap()
                .split_once(' ')
                .unwrap()
                .0
                .to_string();
            let script = cat(&[
                b"HOME=",
                &sh_quote(&b(&home)),
                b"; export HOME; ",
                &quarantine_cmd(&os, &b(&src), name, c0.as_bytes()),
            ]);
            sh(&script)
        };
        let protected = crate::remote::DELETE_PROTECTED;
        assert_eq!(
            run_q(b"top").status.code(),
            Some(protected),
            "a parent of the home"
        );
        assert_eq!(
            run_q(b"top/me").status.code(),
            Some(protected),
            "the home itself"
        );
        assert!(src.join("top/me").is_dir(), "nothing moved");
        assert!(
            run_q(b"other").status.success(),
            "an unrelated folder is fine"
        );
    }

    #[test]
    fn a_source_whose_delete_is_refused_goes_back_or_is_named() {
        for put_back_ok in [true, false] {
            let t = tempfile::tempdir().unwrap();
            let f = FakeCmd::new(move |a| {
                if a[0] == "pipe" {
                    return fake_scp(a);
                }
                let c = remote_cmd(a).unwrap_or_default();
                if c.contains("rm -rf") {
                    return fail(1, "rm: busy\n");
                }
                if c.starts_with("if [ -e '/srv/a' ]") {
                    return code(if put_back_ok { 0 } else { APPEARED });
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
            assert_eq!((r.outcome, r.partial), (OpOutcome::Done, 1));
            let w = &r.warnings[0];
            assert!(w.starts_with("원본을 지우지 못함: a"), "{w}");
            assert_eq!(
                w.contains("원본이 격리 폴더에 남음: /srv/.hdir-del.Q1/a"),
                !put_back_ok,
                "{w}"
            );
            let cmds = f.remote_cmds();
            assert!(
                cmds.iter().any(|c| c.starts_with("if [ -e '/srv/a' ]")),
                "{cmds:?}"
            );
        }
    }

    #[test]
    fn real_received_programs_lose_setuid_and_setgid() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        fs::create_dir(src.join("d")).unwrap();
        fs::write(src.join("d/prog"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("d/prog"), fs::Permissions::from_mode(0o4755)).unwrap();
        fs::write(src.join("d/plain"), "x").unwrap();
        fs::set_permissions(src.join("d/plain"), fs::Permissions::from_mode(0o640)).unwrap();
        if fs::metadata(src.join("d/prog"))
            .unwrap()
            .permissions()
            .mode()
            & 0o4000
            == 0
        {
            return; // this filesystem drops setuid itself
        }
        let no = AtomicBool::new(false);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        // shell side (remote receiver)
        let shell_in = private_dir(t.path(), "shell-in");
        let to: Vec<OsString> = vec!["sh".into(), "-c".into(), {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(&tar_in_cmd(&b(&shell_in), false)).to_os_string()
        }];
        let (oa, ob) = crate::session::run_pipe_cancellable(
            &local_tar_out(
                &src,
                std::ffi::OsStr::new("d"),
                &src_dir_id(&Side::Local, &src).unwrap(),
                false,
            ),
            &to,
            std::time::Duration::from_secs(30),
            &no,
        );
        assert_eq!((oa.code, ob.code), (Some(0), Some(0)), "{oa:?} {ob:?}");
        assert_eq!(mode(&shell_in.join("d/prog")), 0o755);
        assert_eq!(mode(&shell_in.join("d/plain")), 0o640, "other modes kept");
        // local receiver
        let local_in = private_dir(t.path(), "local-in");
        let (oa, ob) = crate::session::run_pipe_cancellable(
            &local_tar_out(
                &src,
                std::ffi::OsStr::new("d"),
                &src_dir_id(&Side::Local, &src).unwrap(),
                false,
            ),
            &local_tar_in(&local_in, false),
            std::time::Duration::from_secs(30),
            &no,
        );
        assert_eq!((oa.code, ob.code), (Some(0), Some(0)));
        strip_setid(&local_in).unwrap();
        assert_eq!(mode(&local_in.join("d/prog")), 0o755);
    }

    #[test]
    fn a_same_server_move_across_filesystems_is_copied_checked_and_deleted() {
        let placed = Arc::new(AtomicBool::new(false));
        let p2 = Arc::clone(&placed);
        let f = FakeCmd::new(move |a| {
            let c = remote_cmd(a).unwrap_or_default();
            if c.starts_with("x=$(stat") {
                return code(1); // different devices
            }
            if c.ends_with("|| exit 46") {
                p2.store(true, Ordering::SeqCst);
                return code(0);
            }
            if c.starts_with("if [ -L '/dst/b'") && p2.load(Ordering::SeqCst) {
                return code(IS_OTHER);
            }
            answer(c, &[], &[], "0\x00")
        });
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let r = run(&s, &mut Ctl::new());
        assert_eq!(
            (r.outcome, r.copied),
            (OpOutcome::Done, 1),
            "{:?}",
            r.warnings
        );
        let cmds = f.remote_cmds();
        assert!(
            !cmds
                .iter()
                .any(|c| c.contains("else exit 52; fi; mv -- ./'b' ")),
            "no cross-filesystem mv: {cmds:?}"
        );
        let cp = cp_cmd(&RemoteOs::Linux, b"/srv", b"b", b"5:6", STAGE.as_bytes());
        assert!(cmds.contains(&String::from_utf8(cp).unwrap()), "{cmds:?}");
        let q = cmds
            .iter()
            .position(|c| c.contains("mktemp -d .hdir-del."))
            .expect("set aside");
        let rm = cmds
            .iter()
            .position(|c| c.contains("rm -rf"))
            .expect("deleted");
        assert!(q < rm);
    }

    #[test]
    fn a_failing_sender_keeps_source_and_target_and_drops_the_stage() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "old").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
                return fail(2, "from: tar: ./a: Cannot open: Permission denied\n");
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "0\x00")
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
            r.outcome,
            OpOutcome::Failed("a: from: tar: ./a: Cannot open: Permission denied".into())
        );
        assert_eq!(fs::read(t.path().join("a")).unwrap(), b"old");
        assert_eq!(stages_in(t.path()), 0);
        assert!(
            !f.remote_cmds()
                .iter()
                .any(|c| c.contains("rm -rf") || c.contains(".hdir-del."))
        );
    }

    #[test]
    fn a_download_through_run_strips_setuid_and_setgid() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "pipe" {
                let (_, dst) = pipe_ends(a);
                let d = Path::new(&dst).join("d");
                fs::create_dir(&d).unwrap();
                for (n, m) in [
                    ("u", 0o4755),
                    ("g", 0o2755),
                    ("ug", 0o6750),
                    ("plain", 0o640),
                ] {
                    fs::write(d.join(n), "x").unwrap();
                    fs::set_permissions(d.join(n), fs::Permissions::from_mode(m)).unwrap();
                }
                return ok("");
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "0\x00")
        });
        let s = spec(
            JobKind::Copy,
            Side::Remote(session(Arc::clone(&f))),
            &p("/srv"),
            &["d"],
            Side::Local,
            t.path(),
        );
        assert_eq!(run(&s, &mut Ctl::new()).outcome, OpOutcome::Done);
        let mode = |n: &str| {
            fs::metadata(t.path().join("d").join(n))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        if mode("plain") != 0o640 {
            return;
        }
        assert_eq!(
            [mode("u"), mode("g"), mode("ug"), mode("plain")],
            [0o755, 0o755, 0o750, 0o640]
        );
    }

    #[test]
    fn esc_during_a_same_server_move_puts_the_source_back() {
        let c = Ctl::new();
        let flag = Arc::clone(&c.cancel);
        let f = FakeCmd::new(move |a| {
            let cm = remote_cmd(a).unwrap_or_default();
            if cm.contains("; mv -- ./'b' ") {
                flag.store(true, Ordering::SeqCst); // Esc while it ran
                return ok("");
            }
            answer(cm, &[], &[], "")
        });
        let sess = session(Arc::clone(&f));
        let s = spec(
            JobKind::Move,
            Side::Remote(Arc::clone(&sess)),
            &p("/srv"),
            &["b"],
            Side::Remote(sess),
            &p("/dst"),
        );
        let mut c = c;
        let r = run(&s, &mut c);
        assert_eq!(r.outcome, OpOutcome::Cancelled);
        let cmds = f.remote_cmds();
        let back = String::from_utf8(unmove_cmd(
            &RemoteOs::Linux,
            STAGE.as_bytes(),
            b"b",
            b"/srv/b",
        ));
        assert!(cmds.contains(&back.unwrap()), "{cmds:?}");
        assert!(
            !cmds
                .iter()
                .any(|c| c.ends_with("|| exit 46") && c.contains("'/dst/b'")),
            "not placed"
        );
    }

    #[test]
    fn real_a_source_folder_swapped_after_the_start_sends_nothing() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        let secret = private_dir(t.path(), "secret");
        fs::write(src.join("f"), "mine").unwrap();
        fs::write(secret.join("f"), "SECRET").unwrap();
        let id = src_dir_id(&Side::Local, &src).unwrap();
        // the folder becomes a link to another folder with the same names
        fs::rename(&src, t.path().join("src.old")).unwrap();
        std::os::unix::fs::symlink(&secret, &src).unwrap();
        let no = AtomicBool::new(false);
        let sink: Vec<OsString> = vec!["sh".into(), "-c".into(), "cat".into()];
        let (oa, ob) = crate::session::run_pipe_cancellable(
            &local_tar_out(&src, std::ffi::OsStr::new("f"), &id, false),
            &sink,
            std::time::Duration::from_secs(30),
            &no,
        );
        assert_eq!(oa.code, Some(SRC_MOVED), "{oa:?}");
        assert!(ob.stdout.is_empty(), "nothing read");
        // the remote sender checks the same way
        let out = sh(&tar_out_cmd(
            &crate::testutil::local_os(),
            &b(&src),
            b"f",
            id.as_bytes(),
            false,
        ));
        assert_eq!(out.status.code(), Some(SRC_MOVED));
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn real_same_server_copy_and_move_check_the_source_folder_and_kinds() {
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        let st = private_dir(t.path(), "stage");
        let secret = private_dir(t.path(), "secret");
        fs::write(src.join("f"), "mine").unwrap();
        fs::write(secret.join("f"), "SECRET").unwrap();
        let fifo = std::ffi::CString::new(src.join("p").to_str().unwrap()).unwrap();
        // SAFETY: valid path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let os = crate::testutil::local_os();
        let id = src_dir_id(&Side::Local, &src).unwrap();
        // a special file never moves
        let into = b(&st.join("p"));
        let out = sh(&host_mv_cmd(&os, &b(&src), b"p", id.as_bytes(), &into));
        assert_eq!(out.status.code(), Some(SPECIAL));
        assert!(src.join("p").exists() && !st.join("p").exists());
        // the folder swapped for a link: nothing copied or moved
        fs::rename(&src, t.path().join("src.old")).unwrap();
        std::os::unix::fs::symlink(&secret, &src).unwrap();
        let out = sh(&cp_cmd(&os, &b(&src), b"f", id.as_bytes(), &b(&st)));
        assert_eq!(out.status.code(), Some(SRC_MOVED));
        let into = b(&st.join("f"));
        let out = sh(&host_mv_cmd(&os, &b(&src), b"f", id.as_bytes(), &into));
        assert_eq!(out.status.code(), Some(SRC_MOVED));
        assert_eq!(fs::read_dir(&st).unwrap().count(), 0);
        assert_eq!(fs::read(secret.join("f")).unwrap(), b"SECRET");
    }

    #[test]
    fn a_packaged_upload_tars_locally_then_scps_then_unpacks() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("a"), "x").unwrap();
        let f = FakeCmd::new(|a| {
            if a[0] == "tar" {
                return ok("");
            }
            answer(remote_cmd(a).unwrap_or_default(), &[], &[], "")
        });
        let s = session(Arc::clone(&f));
        let no = AtomicBool::new(false);
        send_up_packaged(&s, t.path(), &OsString::from("a"), STAGE.as_bytes(), &no).unwrap();
        let log = f.log.lock().unwrap().clone();
        assert_eq!(log[0][..2], ["tar", "-cf"]);
        assert_eq!(
            log[0][3..],
            [
                "-C".to_string(),
                t.path().to_string_lossy().into(),
                "./a".into()
            ]
        );
        assert_eq!(log[1][0], "scp");
        assert_eq!(log[1].last().unwrap(), &format!("prod-web:{STAGE}/o.tar"));
        let unpack = String::from_utf8(unpack_cmd(STAGE.as_bytes(), b"o.tar")).unwrap();
        assert_eq!(remote_cmd(&log[2]), Some(unpack.as_str()));
        assert_eq!(pkg_name(b"o.tar"), b"p.tar", "never the item's own name");
    }

    #[test]
    fn real_unpacking_a_package_keeps_links_drops_the_package_and_setuid() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let src = private_dir(t.path(), "src");
        let stage = private_dir(t.path(), "stage");
        fs::create_dir(src.join("d")).unwrap();
        fs::write(src.join("d/prog"), "x").unwrap();
        fs::set_permissions(src.join("d/prog"), fs::Permissions::from_mode(0o4755)).unwrap();
        std::os::unix::fs::symlink("/etc", src.join("d/ln")).unwrap();
        let pkg = stage.join("o.tar");
        let out = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&pkg)
            .arg("-C")
            .arg(&src)
            .arg("./d")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(sh(&unpack_cmd(&b(&stage), b"o.tar")).status.success());
        assert!(!pkg.exists());
        assert!(
            fs::symlink_metadata(stage.join("d/ln"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let mode = fs::metadata(stage.join("d/prog"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode & 0o6000, 0, "{mode:o}");
    }

    #[test]
    fn real_a_link_appearing_as_a_folder_link_gets_nothing_inside_it() {
        let t = tempfile::tempdir().unwrap();
        let dest = private_dir(t.path(), "dest");
        let elsewhere = private_dir(t.path(), "elsewhere");
        let (dir, stage, sp) = stage_in(&dest);
        std::os::unix::fs::symlink("target", sp.join("ln")).unwrap();
        let to = cat(&[&dir, b"/ln"]);
        let from = cat(&[&stage, b"/ln"]);
        // right before ln, the name becomes a link to another folder
        let script = cat(&[
            b"ln() { command ln -s ",
            &sh_quote(&b(&elsewhere)),
            b" ",
            &sh_quote(&to),
            b"; command ln \"$@\"; }; ",
            &place_cmd(&RemoteOs::Bsd, &from, &to, &stage, false),
        ]);
        assert_eq!(sh(&script).status.code(), Some(APPEARED));
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing inside"
        );
        assert_eq!(
            fs::read_link(dest.join("ln")).unwrap(),
            elsewhere,
            "theirs kept"
        );
        assert!(sp.join("ln").exists() || fs::symlink_metadata(sp.join("ln")).is_ok());
    }
}

#[cfg(all(test, windows))]
mod windows_stamp_tests {
    use super::*;

    #[test]
    fn a_same_size_edit_with_its_mtime_put_back_is_seen() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, "abc").unwrap();
        let no = AtomicBool::new(false);
        let name = OsString::from("f");
        let before = stamp(&Side::Local, t.path(), &name, &no).unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        std::fs::write(&f, "xyz").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_ne!(stamp(&Side::Local, t.path(), &name, &no).unwrap(), before);
    }
}

#[cfg(all(test, windows))]
mod windows_place_tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_approved_overwrite_replaces_a_read_only_file() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let old = dest.join("a.txt");
        fs::write(&old, "old").unwrap();
        let mut ro = fs::metadata(&old).unwrap().permissions();
        ro.set_readonly(true);
        fs::set_permissions(&old, ro).unwrap();
        let stage = tempfile::Builder::new()
            .prefix(STAGE_PREFIX)
            .tempdir_in(&dest)
            .unwrap();
        fs::write(stage.path().join("a.txt"), "new").unwrap();
        let r = place(
            Stage::Local(stage, dest.clone()),
            &"a.txt".into(),
            true,
            None,
        );
        assert!(matches!(r, Ok(Placed::Done(None))), "{r:?}");
        assert_eq!(fs::read(&old).unwrap(), b"new");
    }

    #[test]
    fn transfer_messages_in_english() {
        crate::i18n::with_lang(crate::i18n::Lang::En, || {
            assert_eq!(unsafe_msg(), "Not sent: a folder other users can change");
            assert_eq!(left_behind("/t/x y"), "Temp folder left behind: /t/x y");
            let name = OsString::from("한글 'a'.txt");
            assert_eq!(
                changed_msg(&name),
                "Source kept: it changed during the transfer or is the same file as the target: 한글 'a'.txt"
            );
            let d = tempfile::tempdir().unwrap();
            let gone = d.path().join("none");
            assert_eq!(
                dest_dir_ok(&Side::Local, &gone).unwrap_err(),
                "Destination folder missing"
            );
        });
        // the default language is still Korean
        assert_eq!(left_behind("/t"), "임시 폴더가 남음: /t");
    }
}
