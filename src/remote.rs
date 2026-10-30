//! Remote (ssh/scp) support, pure part: OS detection, listing parser and
//! command-vector builders. Nothing here spawns processes.
//!
//! Decisions:
//! - Special files (fifo/socket/device) are `RemoteKind::Special`; `to_entry`
//!   maps them to `EntryKind::File` (panel has no special kind yet).
//! - `RemoteOs::Other` uses the GNU `find -printf` path.
//! - BSD listing prints the name via `printf '%s\0'` instead of `stat %N`
//!   (which appends ` -> target` for symlinks), so the record layout is the
//!   same as Linux: `kind \t size \t mtime \t perm \t name \0`.
//! - Symlinks carry `target_is_dir` (`find %Y` / `stat -L -f %HT`), so the
//!   record layout is `kind \t size \t mtime \t perm \t target-kind \t name \0`.
//! - scp: OpenSSH >= 9.0 speaks SFTP by default, where the remote path is not
//!   shell-interpreted (quotes would be literal) but glob characters in a
//!   *source* path are still expanded. See [`ScpMode`].
//! - ssh commands put `--` before the host; scp hosts starting with `-` are
//!   rejected.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::panel::{Entry, EntryKind};
use crate::platform::{os_bytes, os_from_bytes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOs {
    Linux,
    /// macOS / FreeBSD and friends (`stat -f`).
    Bsd,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
    pub control_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    Dir,
    File,
    Symlink,
    /// fifo, socket, char/block device, door, ...
    Special,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: OsString,
    pub kind: RemoteKind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
    /// Symlinks only: the target is a directory (false if unknown/dangling).
    pub target_is_dir: bool,
}

pub enum Endpoint<'a> {
    Local(&'a Path),
    Remote(&'a SshTarget, &'a [u8]),
}

/// Detect the remote OS from `uname -s` output (first line is used).
pub fn parse_uname(out: &str) -> RemoteOs {
    let name = out.lines().next().unwrap_or("").trim();
    match name {
        "Linux" => RemoteOs::Linux,
        "Darwin" | "FreeBSD" | "OpenBSD" | "NetBSD" | "DragonFly" => RemoteOs::Bsd,
        other => RemoteOs::Other(other.to_string()),
    }
}

/// POSIX single-quote raw bytes.
pub fn sh_quote(s: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(s.len() + 2);
    o.push(b'\'');
    for &b in s {
        if b == b'\'' {
            o.extend_from_slice(b"'\\''");
        } else {
            o.push(b);
        }
    }
    o.push(b'\'');
    o
}

fn os(s: &str) -> OsString {
    OsString::from(s)
}

fn control_opt(t: &SshTarget) -> OsString {
    let mut o = OsString::from("ControlPath=");
    o.push(&t.control_path);
    o
}

fn port_args(port: Option<u16>, flag: &str, v: &mut Vec<OsString>) {
    if let Some(p) = port {
        v.push(os(flag));
        v.push(OsString::from(p.to_string()));
    }
}

/// Options that keep a dead network from hanging ssh forever.
fn net_opts(v: &mut Vec<OsString>) {
    for o in [
        "ConnectTimeout=10",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=2",
    ] {
        v.push(os("-o"));
        v.push(os(o));
    }
}

/// `ssh` argv that starts the persistent master connection.
pub fn master_argv(t: &SshTarget) -> Vec<OsString> {
    let mut v = vec![
        os("ssh"),
        os("-o"),
        os("ControlMaster=yes"),
        os("-o"),
        os("ControlPersist=10m"),
        os("-o"),
        control_opt(t),
    ];
    net_opts(&mut v);
    port_args(t.port, "-p", &mut v);
    v.push(os("-fN"));
    v.push(os("--"));
    v.push(os(&t.host));
    v
}

/// `ssh -O check` argv (is the master alive?).
pub fn check_argv(t: &SshTarget) -> Vec<OsString> {
    let mut v = vec![os("ssh"), os("-O"), os("check"), os("-o"), control_opt(t)];
    net_opts(&mut v);
    port_args(t.port, "-p", &mut v);
    v.push(os("--"));
    v.push(os(&t.host));
    v
}

/// `ssh -t` argv that runs `cmd` in `cwd` on the host, on a terminal, over
/// the master. The command is wrapped in a brace group so a `;` or a trailing
/// comment in it cannot escape the `cd` guard. Unlike [`exec_argv`] there is
/// no `BatchMode`: the command itself may prompt.
pub fn remote_shell_argv(t: &SshTarget, cwd: &[u8], cmd: &str) -> Vec<OsString> {
    let mut v = vec![os("ssh"), os("-o"), control_opt(t)];
    net_opts(&mut v);
    port_args(t.port, "-p", &mut v);
    v.push(os("-t"));
    v.push(os("--"));
    v.push(os(&t.host));
    let remote = cat(&[
        b"cd -- ",
        &sh_quote(cwd),
        b" && {\n",
        cmd.as_bytes(),
        b"\n}",
    ]);
    v.push(os_from_bytes(remote));
    v
}

/// Run one remote command over the master; the command is a single argv item.
pub fn exec_argv(t: &SshTarget, remote_cmd: &[u8]) -> Vec<OsString> {
    let mut v = vec![
        os("ssh"),
        os("-o"),
        control_opt(t),
        os("-o"),
        os("BatchMode=yes"),
    ];
    net_opts(&mut v);
    port_args(t.port, "-p", &mut v);
    v.push(os("--"));
    v.push(os(&t.host));
    v.push(os_from_bytes(remote_cmd.to_vec()));
    v
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Remote shell command that prints NUL-terminated listing records.
pub fn list_cmd(os: &RemoteOs, dir: &[u8], show_hidden: bool) -> Vec<u8> {
    let q = sh_quote(dir);
    match os {
        RemoteOs::Bsd => {
            let globs: &[u8] = if show_hidden { b"* .[!.]* ..?*" } else { b"*" };
            cat(&[
                b"cd -- ",
                &q,
                b" && for f in ",
                globs,
                b"; do [ -e \"$f\" ] || [ -L \"$f\" ] || continue; \
t=$(stat -L -f %HT -- \"./$f\" 2>/dev/null) || t=; \
stat -n -f '%HT%t%z%t%m%t%Lp%t' -- \"./$f\" && printf '%s\\t%s\\0' \"$t\" \"$f\"; done",
            ])
        }
        _ => {
            let filter: &[u8] = if show_hidden { b"" } else { b" ! -name '.*'" };
            cat(&[
                b"cd -- ",
                &q,
                b" && find . -mindepth 1 -maxdepth 1",
                filter,
                b" -printf '%y\\t%s\\t%T@\\t%m\\t%Y\\t%P\\0'",
            ])
        }
    }
}

fn parse_kind(os: &RemoteOs, f: &[u8]) -> Option<RemoteKind> {
    use RemoteKind::*;
    match os {
        RemoteOs::Bsd => Some(match f {
            b"Directory" => Dir,
            b"Regular File" => File,
            b"Symbolic Link" => Symlink,
            b"Fifo File" | b"Socket" | b"Character Device" | b"Block Device" | b"Whiteout File" => {
                Special
            }
            _ => return None,
        }),
        _ => Some(match f {
            b"d" => Dir,
            b"f" => File,
            b"l" => Symlink,
            b"p" | b"s" | b"c" | b"b" | b"D" => Special,
            _ => return None,
        }),
    }
}

fn parse_mtime(f: &[u8]) -> Option<SystemTime> {
    let t = std::str::from_utf8(f).ok()?;
    let (secs, frac) = t.split_once('.').unwrap_or((t, ""));
    let secs: u64 = secs.parse().ok()?;
    let mut digits: String = frac.chars().take(9).collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    while digits.len() < 9 {
        digits.push('0');
    }
    let nanos: u32 = digits.parse().ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

fn parse_record(os: &RemoteOs, rec: &[u8]) -> Option<RemoteEntry> {
    let mut it = rec.splitn(6, |&b| b == b'\t');
    let kind = parse_kind(os, it.next()?)?;
    let size: u64 = std::str::from_utf8(it.next()?).ok()?.parse().ok()?;
    let mtime = parse_mtime(it.next()?);
    let mode = u32::from_str_radix(std::str::from_utf8(it.next()?).ok()?, 8).ok()?;
    let target_is_dir = matches!(it.next()?, b"d" | b"Directory") && kind == RemoteKind::Symlink;
    let name = it.next()?;
    if name.is_empty() || mode > 0o7777 {
        return None;
    }
    Some(RemoteEntry {
        name: os_from_bytes(name.to_vec()),
        kind,
        size,
        mtime,
        mode,
        target_is_dir,
    })
}

/// Parse NUL-separated records; returns entries and the malformed count.
pub fn parse_listing(os: &RemoteOs, out: &[u8]) -> (Vec<RemoteEntry>, usize) {
    let mut recs: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    if recs.last().is_some_and(|r| r.is_empty()) {
        recs.pop(); // terminator after the last record
    }
    let mut entries = Vec::new();
    let mut bad = 0;
    for r in recs {
        match parse_record(os, r) {
            Some(e) => entries.push(e),
            None => bad += 1,
        }
    }
    (entries, bad)
}

pub fn head_cmd(path: &[u8], max: usize) -> Vec<u8> {
    cat(&[format!("head -c {max} -- ").as_bytes(), &sh_quote(path)])
}

/// Exit codes of [`preview_cmd`] for things that are not regular files.
pub const PREVIEW_DIR: i32 = 20;
pub const PREVIEW_SPECIAL: i32 = 21;
pub const PREVIEW_MISSING: i32 = 22;
pub const PREVIEW_BROKEN: i32 = 23;

/// `stat` size flags of `os` (symlinks followed, so a link reports its
/// target's size like `head` reads it).
fn stat_size_args(os: &RemoteOs) -> &'static str {
    match os {
        RemoteOs::Bsd => "stat -L -f %z --",
        RemoteOs::Linux | RemoteOs::Other(_) => "stat -L -c %s --",
    }
}

/// One round trip for a preview: a directory exits [`PREVIEW_DIR`], other
/// non-regular files (fifo, device, socket: `head` could block) exit
/// [`PREVIEW_SPECIAL`]; a regular file prints its size (`stat`, not `wc -c`,
/// which would read the whole file) and a newline, then the first `max`
/// bytes (`head -c`). Symlinks are followed.
pub fn preview_cmd(os: &RemoteOs, path: &[u8], max: usize) -> Vec<u8> {
    let q = sh_quote(path);
    cat(&[
        b"if [ -d ",
        &q,
        format!(" ]; then exit {PREVIEW_DIR}; elif [ -f ").as_bytes(),
        &q,
        format!(" ]; then n=$({} ", stat_size_args(os)).as_bytes(),
        &q,
        b") || exit 1; printf '%s\\n' $n; ",
        &head_cmd(path, max),
        b"; elif [ -e ",
        &q,
        format!(" ]; then exit {PREVIEW_SPECIAL}; elif [ -L ").as_bytes(),
        &q,
        format!(" ]; then exit {PREVIEW_BROKEN}; else exit {PREVIEW_MISSING}; fi").as_bytes(),
    ])
}

pub fn mkdir_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[b"mkdir -- ", &sh_quote(path)])
}

pub fn mv_cmd(from: &[u8], to: &[u8], no_clobber: bool) -> Vec<u8> {
    let head: &[u8] = if no_clobber { b"mv -n -- " } else { b"mv -- " };
    cat(&[head, &sh_quote(from), b" ", &sh_quote(to)])
}

pub fn rm_cmd(paths: &[&[u8]]) -> Vec<u8> {
    let mut o = b"rm -rf --".to_vec();
    for p in paths {
        o.push(b' ');
        o.extend_from_slice(&sh_quote(p));
    }
    o
}

/// Exit code of [`rename_cmd`] when the target already exists.
pub const RENAME_EXISTS: i32 = 17;

/// Rename without replacing. The target is checked in the same remote command
/// as the move, and `mv -n` refuses to clobber a target that appears between
/// the check and the move. This is as close to an atomic no-replace rename as
/// plain POSIX tools get: there is still a tiny race window (and `mv -n` may
/// exit 0 without moving when it loses that race), unlike the local
/// `renameat2`/`link` based rename.
pub fn rename_cmd(from: &[u8], to: &[u8]) -> Vec<u8> {
    let q = sh_quote(to);
    cat(&[
        b"if [ -e ",
        &q,
        b" ] || [ -L ",
        &q,
        format!(" ]; then exit {RENAME_EXISTS}; fi; ").as_bytes(),
        b"mv -n -- ",
        &sh_quote(from),
        b" ",
        &q,
    ])
}

/// Exit codes of [`delete_cmd`].
pub const DELETE_NO_HOME: i32 = 40;
pub const DELETE_NO_CWD: i32 = 41;
pub const DELETE_PROTECTED: i32 = 42;

/// One script that deletes `names` (plain child names) of `cwd` with
/// `rm -rf --`, after checking on the host with *physical* paths (`pwd -P`)
/// that no target is `/`, the home directory or an ancestor of it. `cwd` is
/// resolved first, so reaching the home through a symlinked directory is
/// caught; a target that is itself a symlink is removed as a link, so its
/// parent (the physical cwd) is what gets resolved. Nothing is removed when
/// the home or cwd cannot be resolved ([`DELETE_NO_HOME`], [`DELETE_NO_CWD`])
/// or a target is protected ([`DELETE_PROTECTED`]). Every `$(pwd -P)` is
/// followed by a sentinel (`echo x`, stripped with `${v%?x}`) so a path
/// ending in a newline keeps it.
pub fn delete_cmd(cwd: &[u8], names: &[&[u8]]) -> Vec<u8> {
    let list = names
        .iter()
        .map(|n| sh_quote(n))
        .collect::<Vec<_>>()
        .join(&b' ');
    cat(&[
        b"[ -n \"$HOME\" ] || exit 40\n",
        b"h=$(cd -- \"$HOME\" 2>/dev/null && pwd -P && echo x) || exit 40; h=${h%?x}\n",
        b"c=$(cd -- ",
        &sh_quote(cwd),
        b" && pwd -P && echo x) || exit 41; c=${c%?x}\n",
        b"for n in ",
        &list,
        b"; do\n",
        b"  case $c in /) t=/$n;; *) t=$c/$n;; esac\n",
        b"  [ \"$t\" != / ] || exit 42\n",
        b"  case \"$h/\" in \"$t\"/*) echo 'refusing to delete the home directory or a parent of it' >&2; exit 42;; esac\n",
        b"done\n",
        b"cd -- \"$c\" && rm -rf -- ",
        &list,
    ])
}

/// Prints the physical path of `path` with its final symlinks followed
/// (portable `readlink -f`: a manual loop, 40 levels at most), NUL-terminated;
/// fails unless it names a regular file. Like [`delete_cmd`], command
/// substitutions carry a sentinel so trailing newlines survive.
pub fn resolve_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[
        b"p=",
        &sh_quote(path),
        b"; n=0\n\
          while [ -L \"$p\" ]; do\n  \
          n=$((n+1)); [ \"$n\" -le 40 ] || { echo 'too many levels of symbolic links' >&2; exit 1; }\n  \
          l=$(readlink -- \"$p\"; echo x); l=${l%x}; l=${l%?}\n  \
          case $l in /*) p=$l;; *) p=${p%/*}/$l;; esac\n\
          done\n\
          d=${p%/*}; [ -n \"$d\" ] || d=/; b=${p##*/}\n\
          d=$(cd -- \"$d\" && pwd -P && echo x) || exit 1; d=${d%?x}\n\
          if [ \"$d\" = / ]; then p=/$b; else p=$d/$b; fi\n\
          [ -f \"$p\" ] || { echo 'not a regular file' >&2; exit 1; }\n\
          printf '%s\\0' \"$p\"",
    ])
}

/// Creates an exclusive (mode 600) temp file `<dir>/.hdir-edit.XXXXXX` with
/// `mktemp` and prints its path, NUL-terminated.
pub fn mktemp_cmd(dir: &[u8]) -> Vec<u8> {
    let mut template = dir.to_vec();
    while template.len() > 1 && template.ends_with(b"/") {
        template.pop();
    }
    if template != b"/" {
        template.push(b'/');
    }
    template.extend_from_slice(b".hdir-edit.XXXXXX");
    cat(&[
        b"t=$(mktemp ",
        &sh_quote(&template),
        b") || exit 1; printf '%s\\0' \"$t\"",
    ])
}

/// Exit codes of [`replace_cmd`]. On [`REPLACE_NOT_OWNER`] the temp file is
/// left in place.
pub const REPLACE_SIZE: i32 = 31;
pub const REPLACE_NOT_OWNER: i32 = 32;
/// The in-place write of a hard-linked file failed and the original content
/// was copied back from the backup.
pub const REPLACE_RESTORED: i32 = 33;
/// The in-place write failed and so did restoring: the backup (path printed,
/// NUL-terminated) and the temp file are kept.
pub const REPLACE_BACKUP_KEPT: i32 = 34;
/// The original's group could not be given to the temp file (not a member
/// of it): nothing is replaced.
pub const REPLACE_GROUP: i32 = 35;
/// The original changed on the remote since it was stamped (see
/// [`stamp_cmd`]): nothing is replaced.
pub const REPLACE_CHANGED: i32 = 36;

/// Shell that sets `s` (the [`stat_stamp_args`] fields) and `k`
/// (`cksum` of the content) for the file named by `$var`; fails without
/// reading anything unless it is a regular file (a FIFO would block), and
/// when the `stat` fields changed while the checksum ran.
fn stamp_vars(os: &RemoteOs, var: &str) -> String {
    let st = stat_stamp_args(os);
    format!(
        "[ -f \"${var}\" ] && s=$({st} \"${var}\") && k=$(cksum < \"${var}\") && [ \"$({st} \"${var}\")\" = \"$s\" ]"
    )
}

/// `stat` fields `inode mtime ctime size mode uid gid` (symlinks followed)
/// of `os`. ctime cannot be set back by a user, so a rewrite that restores
/// the mtime still shows; mode and owner changes count as changes too.
fn stat_stamp_args(os: &RemoteOs) -> &'static str {
    match os {
        RemoteOs::Bsd => "stat -L -f '%i %m %c %z %Lp %u %g' --",
        RemoteOs::Linux | RemoteOs::Other(_) => "stat -L -c '%i %Y %Z %s %a %u %g' --",
    }
}

/// Number of fields printed by [`stat_stamp_args`].
const STAT_STAMP_FIELDS: usize = 7;

/// Prints a fingerprint of the file at `path` (inode, mtime, ctime, size,
/// mode, owner and a content checksum; symlinks followed), to tell later
/// whether it changed.
pub fn stamp_cmd(os: &RemoteOs, path: &[u8]) -> Vec<u8> {
    cat(&[
        b"f=",
        &sh_quote(path),
        format!(
            "; {} || {{ echo 'not a readable regular file' >&2; exit 1; }}; printf '%s %s' \"$s\" \"$k\"",
            stamp_vars(os, "f")
        )
        .as_bytes(),
    ])
}

/// `stat` printing the group id, of `os`.
fn stat_gid_args(os: &RemoteOs) -> &'static str {
    match os {
        RemoteOs::Bsd => "stat -f %g --",
        RemoteOs::Linux | RemoteOs::Other(_) => "stat -c %g --",
    }
}

/// `stat` fields `mode nlink uid gid` of `os`.
fn stat_meta_args(os: &RemoteOs) -> &'static str {
    match os {
        RemoteOs::Bsd => "stat -f '%Lp %l %u %g' --",
        RemoteOs::Linux | RemoteOs::Other(_) => "stat -c '%a %h %u %g' --",
    }
}

/// Put the uploaded temp file `tmp` (`size` bytes) in place of the physical
/// regular file `orig`, keeping its mode:
/// - `orig` must still match `expect` (its [`stamp_cmd`] output when it was
///   downloaded), checked right before writing (after the backup / the
///   temp's group; the temp stays mode 600 until the check passes); else
///   [`REPLACE_CHANGED`] and someone else's newer content is kept;
/// - the temp's size must match, else [`REPLACE_SIZE`];
/// - when the owner is not the current user (and we are not root) nothing is
///   replaced ([`REPLACE_NOT_OWNER`]): ownership cannot be kept;
/// - the original's mode is applied to the temp, then `mv` replaces it
///   atomically (a failed upload never truncates the original);
/// - a file with several hard links keeps its inode: it is first copied to a
///   private backup `.hdir-bak.XXXXXX` next to it (no backup, no write), then
///   the verified temp content is written in place with `cat`. If that write
///   fails the backup is copied back ([`REPLACE_RESTORED`]); if restoring
///   fails too, the backup and the temp stay ([`REPLACE_BACKUP_KEPT`], the
///   backup path printed NUL-terminated). An original whose `stat` fields
///   already differ from `expect` is refused before any backup is made.
///
/// - before the `mv`, the temp gets the original's group (`chgrp`, then the
///   mode again, as `chgrp` may clear setgid); when that is not allowed
///   nothing is replaced ([`REPLACE_GROUP`]) rather than exposing a
///   group-restricted file to another group.
pub fn replace_cmd(os: &RemoteOs, tmp: &[u8], orig: &[u8], size: u64, expect: &[u8]) -> Vec<u8> {
    let (t, o) = (sh_quote(tmp), sh_quote(orig));
    cat(&[
        b"t=",
        &t,
        b"; o=",
        &o,
        format!(
            "\n[ \"$(wc -c < \"$t\" | tr -d ' ')\" = {size} ] || exit {REPLACE_SIZE}\n"
        )
        .as_bytes(),
        format!("m=$({} \"$o\") || exit 1; set -- $m\n", stat_meta_args(os)).as_bytes(),
        format!(
            "if [ \"$3\" != \"$(id -u)\" ]; then [ \"$(id -u)\" = 0 ] && chown \"$3:$4\" \"$t\" || exit {REPLACE_NOT_OWNER}; fi\n"
        )
        .as_bytes(),
        format!("same() {{ {} && [ \"$s $k\" = ", stamp_vars(os, "o")).as_bytes(),
        &sh_quote(expect),
        b" ]; }\n",
        format!(
            "changed() {{ echo 'changed on the remote since it was downloaded' >&2; exit {REPLACE_CHANGED}; }}\n"
        )
        .as_bytes(),
        format!("pre() {{ [ \"$({} \"$o\")\" = ", stat_stamp_args(os)).as_bytes(),
        &sh_quote(
            &expect
                .split(|&b| b == b' ')
                .take(STAT_STAMP_FIELDS)
                .collect::<Vec<_>>()
                .join(&b' '),
        ),
        b" ]; }\n",
        b"if [ \"$2\" -gt 1 ]; then\n  \
          pre || changed\n  \
          b=$(mktemp \"${o%/*}/.hdir-bak.XXXXXX\") || exit 1\n  \
          cp -- \"$o\" \"$b\" || { rm -f -- \"$b\"; exit 1; }\n  \
          same || { rm -f -- \"$b\"; changed; }\n  \
          if cat -- \"$t\" > \"$o\"; then rm -f -- \"$t\" \"$b\"; exit 0; fi\n  \
          echo 'in-place write failed' >&2\n",
        format!(
            "  if cp -- \"$b\" \"$o\"; then rm -f -- \"$b\"; exit {REPLACE_RESTORED}; fi\n  \
             printf '%s\\0' \"$b\"; exit {REPLACE_BACKUP_KEPT}\n"
        )
        .as_bytes(),
        format!(
            "else\n  g=$({} \"$t\") || exit 1\n  \
             if [ \"$g\" != \"$4\" ]; then chgrp \"$4\" \"$t\" || exit {REPLACE_GROUP}; fi\n  \
             same || changed\n  \
             chmod \"$1\" \"$t\" || exit 1\n  \
             mv -- \"$t\" \"$o\"\nfi",
            stat_gid_args(os)
        )
        .as_bytes(),
    ])
}

/// `rm -f` of one file (cleanup of a remote temp file).
pub fn rm_file_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[b"rm -f -- ", &sh_quote(path)])
}

/// `rmdir` (fails unless the directory is empty): undo of a remote mkdir.
pub fn rmdir_cmd(path: &[u8]) -> Vec<u8> {
    cat(&[b"rmdir -- ", &sh_quote(path)])
}

/// How the local `scp` interprets remote paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScpMode {
    /// OpenSSH >= 9.0: SFTP protocol, remote paths are not shell-interpreted.
    Sftp,
    /// Older scp: the remote path goes through the remote shell.
    Legacy,
}

/// Decide the scp mode from the local `ssh -V` output (e.g.
/// `OpenSSH_9.6p1 Ubuntu-...`, `OpenSSH_for_Windows_9.5p2`). Unparsable
/// output is assumed to be a modern (sftp) scp.
pub fn scp_mode_from_version(ssh_v_output: &str) -> ScpMode {
    let major = ssh_v_output.split("OpenSSH_").nth(1).and_then(|r| {
        let r = r.strip_prefix("for_Windows_").unwrap_or(r);
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    match major {
        Some(m) if m < 9 => ScpMode::Legacy,
        _ => ScpMode::Sftp,
    }
}

/// Backslash-escape the glob metacharacters `* ? [ ] \` of an SFTP *source*
/// path. Verified against OpenSSH 10.4 client / 9.6 server: unescaped they
/// expand (`star*` also fetched `starX`), `\\` is needed for a literal
/// backslash, and `{}`, `~`, `%`, `$`, quotes and spaces are literal.
/// Destination paths are not globbed and must stay raw.
pub fn sftp_glob_escape(path: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(path.len());
    for &b in path {
        if matches!(b, b'*' | b'?' | b'[' | b']' | b'\\') {
            o.push(b'\\');
        }
        o.push(b);
    }
    o
}

fn remote_arg(
    t: &SshTarget,
    path: &[u8],
    mode: ScpMode,
    is_source: bool,
) -> Result<OsString, String> {
    if t.host.is_empty() || t.host.starts_with('-') {
        return Err(format!("invalid host: {:?}", t.host));
    }
    let mut b = t.host.as_bytes().to_vec();
    b.push(b':');
    match (mode, is_source) {
        (ScpMode::Legacy, _) => b.extend_from_slice(&sh_quote(path)),
        (ScpMode::Sftp, true) => b.extend_from_slice(&sftp_glob_escape(path)),
        (ScpMode::Sftp, false) => b.extend_from_slice(path),
    }
    Ok(os_from_bytes(b))
}

/// Local path as an scp argument (kept native, never converted to POSIX).
///
/// scp treats the part before the first `:` as a host, so a relative path whose
/// first component contains `:` gets a `./` (unix) or `.\` (windows) prefix.
/// Windows absolute paths (`C:\Users\x\f.txt`) are passed unchanged: OpenSSH
/// for Windows (Win32-OpenSSH `scp`) recognizes a single-letter drive prefix
/// as a local path, so no rewriting is needed (and `\\?\` forms are avoided
/// because they would confuse the same check).
fn local_arg(p: &Path) -> OsString {
    let os = p.as_os_str();
    if p.is_absolute() {
        return os.to_os_string();
    }
    let b = os_bytes(os);
    let first = b
        .split(|&c| c == b'/' || (cfg!(windows) && c == b'\\'))
        .next()
        .unwrap_or(&b);
    if first.contains(&b':') {
        let mut o = if cfg!(windows) {
            b".\\".to_vec()
        } else {
            b"./".to_vec()
        };
        o.extend_from_slice(&b);
        os_from_bytes(o)
    } else {
        os.to_os_string()
    }
}

/// scp argv copying `src` into directory `dst_dir`.
pub fn scp_argv(
    src: &Endpoint,
    dst_dir: &Endpoint,
    mode: ScpMode,
) -> Result<Vec<OsString>, String> {
    let mut v = vec![os("scp")];
    let (ctrl, port, a, b) = match (src, dst_dir) {
        (Endpoint::Local(_), Endpoint::Local(_)) => {
            return Err("local to local copy is not an scp route (use fs_ops)".into());
        }
        (Endpoint::Local(p), Endpoint::Remote(t, d)) => (
            Some(*t),
            t.port,
            local_arg(p),
            remote_arg(t, d, mode, false)?,
        ),
        (Endpoint::Remote(t, s), Endpoint::Local(p)) => (
            Some(*t),
            t.port,
            remote_arg(t, s, mode, true)?,
            local_arg(p),
        ),
        (Endpoint::Remote(ts, s), Endpoint::Remote(td, d)) => {
            if ts.port != td.port {
                return Err("remote to remote with different ports is unsupported".into());
            }
            v.push(os("-3"));
            // Only share ControlPath when both ends use the same master.
            let ctrl = (ts.control_path == td.control_path).then_some(*ts);
            (
                ctrl,
                ts.port,
                remote_arg(ts, s, mode, true)?,
                remote_arg(td, d, mode, false)?,
            )
        }
    };
    v.push(os("-rp"));
    if let Some(t) = ctrl {
        v.push(os("-o"));
        v.push(control_opt(t));
    }
    // Never prompt (the TUI owns the terminal) and never hang on a dead link.
    v.push(os("-o"));
    v.push(os("BatchMode=yes"));
    net_opts(&mut v);
    port_args(port, "-P", &mut v);
    v.push(os("--"));
    v.push(a);
    v.push(b);
    Ok(v)
}

impl RemoteEntry {
    /// Convert for panel display. Special files are shown as plain files.
    pub fn to_entry(&self) -> Entry {
        let kind = match self.kind {
            RemoteKind::Dir => EntryKind::Dir,
            RemoteKind::Symlink => EntryKind::Symlink {
                target_is_dir: self.target_is_dir,
            },
            RemoteKind::File | RemoteKind::Special => EntryKind::File,
        };
        Entry {
            name: self.name.to_string_lossy().into_owned(),
            os_name: self.name.clone(),
            kind,
            size: self.size,
            modified: self.mtime,
            mode: self.mode,
            hidden: os_bytes(&self.name).first() == Some(&b'.'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(port: Option<u16>) -> SshTarget {
        SshTarget {
            host: "prod".into(),
            port,
            control_path: PathBuf::from("/c/cm-1"),
        }
    }
    fn v(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }
    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn uname_variants() {
        assert_eq!(parse_uname("Linux\n"), RemoteOs::Linux);
        assert_eq!(parse_uname("Darwin\n"), RemoteOs::Bsd);
        assert_eq!(parse_uname("FreeBSD"), RemoteOs::Bsd);
        assert_eq!(parse_uname("  OpenBSD \n"), RemoteOs::Bsd);
        assert_eq!(parse_uname("SunOS\n"), RemoteOs::Other("SunOS".into()));
        assert_eq!(parse_uname(""), RemoteOs::Other(String::new()));
        assert_eq!(parse_uname("Linux\nextra"), RemoteOs::Linux);
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote(b"abc"), b"'abc'");
        assert_eq!(sh_quote(b""), b"''");
        assert_eq!(sh_quote(b"it's"), b"'it'\\''s'");
        assert_eq!(sh_quote(b"a\nb"), b"'a\nb'");
        assert_eq!(sh_quote(b"\xff\xfe x"), b"'\xff\xfe x'");
        assert_eq!(sh_quote(b"'"), b"''\\'''");
    }

    #[test]
    fn ssh_argvs() {
        assert_eq!(
            master_argv(&t(None)),
            v(&[
                "ssh",
                "-o",
                "ControlMaster=yes",
                "-o",
                "ControlPersist=10m",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "-fN",
                "--",
                "prod"
            ])
        );
        assert_eq!(
            master_argv(&t(Some(2222))),
            v(&[
                "ssh",
                "-o",
                "ControlMaster=yes",
                "-o",
                "ControlPersist=10m",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "-p",
                "2222",
                "-fN",
                "--",
                "prod"
            ])
        );
        assert_eq!(
            check_argv(&t(Some(22))),
            v(&[
                "ssh",
                "-O",
                "check",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "-p",
                "22",
                "--",
                "prod"
            ])
        );
        assert_eq!(
            exec_argv(&t(None), b"ls 'a b'"),
            v(&[
                "ssh",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "prod",
                "ls 'a b'"
            ])
        );
        let a = exec_argv(&t(Some(2200)), b"x");
        assert_eq!(a[11], OsString::from("-p"));
        assert_eq!(a[12], OsString::from("2200"));
        assert_eq!(a.last().unwrap(), &OsString::from("x"));
    }

    #[cfg(unix)]
    #[test]
    fn exec_argv_keeps_raw_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let a = exec_argv(&t(None), b"cat \xff");
        assert_eq!(a.last().unwrap().as_bytes(), b"cat \xff");
    }

    #[cfg(windows)]
    #[test]
    fn exec_argv_non_utf8_is_lossy_on_windows() {
        let a = exec_argv(&t(None), b"cat \xff");
        assert_eq!(a.last().unwrap(), &OsString::from("cat \u{fffd}"));
    }

    #[test]
    fn list_cmd_linux() {
        let c = s(list_cmd(&RemoteOs::Linux, b"/srv/a b", true));
        assert_eq!(
            c,
            "cd -- '/srv/a b' && find . -mindepth 1 -maxdepth 1 -printf '%y\\t%s\\t%T@\\t%m\\t%Y\\t%P\\0'"
        );
        let c = s(list_cmd(&RemoteOs::Linux, b"/x'y\nz", false));
        assert!(c.starts_with("cd -- '/x'\\''y\nz' && find ."));
        assert!(c.contains("! -name '.*'"));
        assert_eq!(
            list_cmd(&RemoteOs::Other("SunOS".into()), b"/", true),
            list_cmd(&RemoteOs::Linux, b"/", true)
        );
    }

    #[test]
    fn list_cmd_bsd() {
        let c = s(list_cmd(&RemoteOs::Bsd, b"/Users/a b", true));
        assert!(c.starts_with("cd -- '/Users/a b' && for f in * .[!.]* ..?*; do "));
        assert!(c.contains("[ -e \"$f\" ] || [ -L \"$f\" ] || continue;"));
        assert!(c.contains("t=$(stat -L -f %HT -- \"./$f\" 2>/dev/null) || t=;"));
        assert!(c.contains("stat -n -f '%HT%t%z%t%m%t%Lp%t' -- \"./$f\""));
        assert!(c.contains("printf '%s\\t%s\\0' \"$t\" \"$f\""));
        assert!(c.ends_with("done"));
        let c = s(list_cmd(&RemoteOs::Bsd, b"/it's", false));
        assert!(c.contains("'/it'\\''s'"));
        assert!(c.contains("for f in *; do"));
        assert!(!c.contains(".[!.]*"));
    }

    #[test]
    fn parse_linux_sample() {
        let mut out = Vec::new();
        out.extend_from_slice(b"d\t4096\t1700000000.5000000000\t755\td\tsub dir\0");
        out.extend_from_slice(
            b"f\t12\t1700000001.0000000000\t644\tf\t\xed\x95\x9c\xea\xb8\x80.txt\0",
        );
        out.extend_from_slice(b"l\t5\t1700000002.25\t777\td\tlink\0");
        out.extend_from_slice(b"p\t0\t1700000003.0\t600\tp\tfifo\0");
        out.extend_from_slice(b"s\t0\t1700000003.0\t600\ts\tsock\0");
        out.extend_from_slice(b"c\t0\t1700000003.0\t600\tc\tchr\0");
        out.extend_from_slice(b"f\t1\t1700000004.0\t644\tf\ttab\tin\nname\0");
        out.extend_from_slice(b"f\t1\t1700000004.0\t644\tf\tbad\xff\xfename\0");
        let (e, bad) = parse_listing(&RemoteOs::Linux, &out);
        assert_eq!(bad, 0);
        assert_eq!(e.len(), 8);
        assert_eq!(e[0].name, OsString::from("sub dir"));
        assert_eq!(e[0].kind, RemoteKind::Dir);
        assert_eq!(e[0].size, 4096);
        assert_eq!(e[0].mode, 0o755);
        assert_eq!(
            e[0].mtime,
            Some(UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000))
        );
        assert_eq!(e[1].name, OsString::from("한글.txt"));
        assert_eq!(e[1].kind, RemoteKind::File);
        assert_eq!(e[2].kind, RemoteKind::Symlink);
        assert!(e[2].target_is_dir);
        assert!(!e[1].target_is_dir);
        assert_eq!(
            e[2].mtime,
            Some(UNIX_EPOCH + Duration::new(1_700_000_002, 250_000_000))
        );
        assert_eq!(e[3].kind, RemoteKind::Special);
        assert_eq!(e[4].kind, RemoteKind::Special);
        assert_eq!(e[5].kind, RemoteKind::Special);
        assert_eq!(&*os_bytes(&e[6].name), b"tab\tin\nname");
        #[cfg(unix)]
        assert_eq!(&*os_bytes(&e[7].name), b"bad\xff\xfename");
        #[cfg(windows)]
        assert_eq!(e[7].name, OsString::from("bad\u{fffd}\u{fffd}name"));
    }

    #[test]
    fn parse_bsd_sample() {
        let out = b"Directory\t96\t1700000000\t755\tDirectory\td\0Regular File\t3\t1700000001\t644\tRegular File\tf f\0\
Symbolic Link\t4\t1700000002\t755\tDirectory\tl\0Fifo File\t0\t1700000003\t644\tFifo File\tq\0Socket\t0\t1\t644\tSocket\ts\0\
Character Device\t0\t1\t600\tCharacter Device\tc\0Block Device\t0\t1\t600\tBlock Device\tb\0\
Symbolic Link\t4\t1\t755\t\tbroken\0Symbolic Link\t4\t1\t755\tRegular File\tlf\0";
        let (e, bad) = parse_listing(&RemoteOs::Bsd, out);
        assert_eq!(bad, 0);
        let k: Vec<_> = e.iter().map(|x| x.kind).collect();
        use RemoteKind::*;
        assert_eq!(
            k,
            vec![
                Dir, File, Symlink, Special, Special, Special, Special, Symlink, Symlink
            ]
        );
        assert!(e[2].target_is_dir);
        assert!(!e[7].target_is_dir, "broken link: target unknown");
        assert!(!e[8].target_is_dir);
        assert_eq!(e[1].name, OsString::from("f f"));
        assert_eq!(
            e[0].mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(e[0].mode, 0o755);
    }

    #[test]
    fn parse_malformed_counted() {
        let out =
            b"f\t1\t1.0\t644\tf\tok\0garbage\0f\tNaN\t1.0\t644\tf\tx\0f\t1\t1.0\t999\tf\tbadmode\0\
f\t1\t1.0\t644\tf\t\0zz\t1\t1.0\t644\tf\tn\0\0f\t2\t2.0\t600\tf\tok2\0f\t1\t1.0\t644\told-layout\0";
        let (e, bad) = parse_listing(&RemoteOs::Linux, out);
        assert_eq!(e.len(), 2);
        assert_eq!(bad, 7);
        assert_eq!(e[1].name, OsString::from("ok2"));
        let (e, bad) = parse_listing(&RemoteOs::Bsd, b"Weird\t1\t1\t644\t\tn\0");
        assert!(e.is_empty());
        assert_eq!(bad, 1);
        assert_eq!(parse_listing(&RemoteOs::Linux, b""), (vec![], 0));
    }

    /// Fixture for the real-shell listing tests.
    #[cfg(unix)]
    fn list_fixture(d: &Path) {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;
        std::fs::write(d.join("plain"), "abc").unwrap();
        // independent of the umask
        std::fs::set_permissions(d.join("plain"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(d.join(".hid"), "").unwrap();
        std::fs::write(d.join("한글\t탭\n개행 'q'"), "xy").unwrap();
        std::fs::create_dir(d.join("sub dir")).unwrap();
        symlink("sub dir", d.join("lnd")).unwrap();
        symlink("plain", d.join("lnf")).unwrap();
        symlink("nope", d.join("bad")).unwrap();
    }

    #[cfg(unix)]
    fn run_list(os: &RemoteOs, show_hidden: bool) -> (Vec<RemoteEntry>, usize, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        list_fixture(d.path());
        let cmd = list_cmd(os, os_bytes(d.path().as_os_str()).as_ref(), show_hidden);
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(os_from_bytes(cmd))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (e, bad) = parse_listing(os, &out.stdout);
        (e, bad, d)
    }

    #[cfg(unix)]
    fn check_real_listing(os: &RemoteOs) {
        let (mut e, bad, _d) = run_list(os, true);
        assert_eq!(bad, 0);
        assert_eq!(e.len(), 7);
        e.sort_by(|a, b| a.name.cmp(&b.name));
        let get = |n: &str| e.iter().find(|x| x.name == n).unwrap();
        let p = get("plain");
        assert_eq!((p.kind, p.size, p.mode), (RemoteKind::File, 3, 0o644));
        assert!(p.mtime.is_some());
        let k = get("한글\t탭\n개행 'q'");
        assert_eq!((k.kind, k.size), (RemoteKind::File, 2));
        assert_eq!(get(".hid").size, 0);
        let sd = get("sub dir");
        assert_eq!(sd.kind, RemoteKind::Dir);
        assert!(!sd.target_is_dir);
        let ld = get("lnd");
        assert_eq!(ld.kind, RemoteKind::Symlink);
        assert!(ld.target_is_dir, "symlink to dir");
        let lf = get("lnf");
        assert_eq!(lf.kind, RemoteKind::Symlink);
        assert!(!lf.target_is_dir, "symlink to file");
        let b = get("bad");
        assert_eq!(b.kind, RemoteKind::Symlink);
        assert!(!b.target_is_dir, "dangling symlink");
        let (visible, bad, _d) = run_list(os, false);
        assert_eq!(bad, 0);
        assert_eq!(visible.len(), 6);
        assert!(visible.iter().all(|x| x.name != ".hid"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bsd_list_cmd_real_stat() {
        check_real_listing(&RemoteOs::Bsd);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_list_cmd_real_find() {
        check_real_listing(&RemoteOs::Linux);
    }

    #[test]
    fn to_entry_conversion() {
        let r = RemoteEntry {
            name: OsString::from("한글"),
            kind: RemoteKind::Symlink,
            size: 7,
            mtime: None,
            mode: 0o777,
            target_is_dir: true,
        };
        let e = r.to_entry();
        assert_eq!(e.name, "한글");
        assert_eq!(e.os_name, OsString::from("한글"));
        assert_eq!(
            e.kind,
            EntryKind::Symlink {
                target_is_dir: true
            }
        );
        assert_eq!(e.size, 7);
        assert!(!e.hidden);
        let mut r2 = r.clone();
        r2.kind = RemoteKind::Special;
        r2.name = OsString::from(".x");
        let e = r2.to_entry();
        assert_eq!(e.kind, EntryKind::File);
        assert!(e.hidden);
        r2.kind = RemoteKind::Dir;
        assert_eq!(r2.to_entry().kind, EntryKind::Dir);
    }

    #[test]
    fn simple_cmds() {
        assert_eq!(s(head_cmd(b"/a b", 65536)), "head -c 65536 -- '/a b'");
        assert_eq!(s(mkdir_cmd(b"/a'b")), "mkdir -- '/a'\\''b'");
        assert_eq!(s(mv_cmd(b"a", b"b c", false)), "mv -- 'a' 'b c'");
        assert_eq!(s(mv_cmd(b"a", b"b", true)), "mv -n -- 'a' 'b'");
        assert_eq!(s(rm_cmd(&[b"a", b"b\nc"])), "rm -rf -- 'a' 'b\nc'");
    }

    #[test]
    fn rename_cmd_checks_the_target_in_the_same_command() {
        assert_eq!(
            s(rename_cmd(b"/d/a b", b"/d/it's")),
            "if [ -e '/d/it'\\''s' ] || [ -L '/d/it'\\''s' ]; then exit 17; fi; \
             mv -n -- '/d/a b' '/d/it'\\''s'"
        );
        // leading dash and newline stay inside the quotes
        let c = s(rename_cmd(b"/d/-a", b"/d/b\nc"));
        assert!(c.ends_with("mv -n -- '/d/-a' '/d/b\nc'"), "{c}");
        assert_eq!(RENAME_EXISTS, 17);
    }

    #[test]
    fn preview_cmd_classifies_then_prints_size_and_head() {
        assert_eq!(
            s(preview_cmd(&RemoteOs::Linux, b"/d/it's -x", 65539)),
            "if [ -d '/d/it'\\''s -x' ]; then exit 20; \
             elif [ -f '/d/it'\\''s -x' ]; then n=$(stat -L -c %s -- '/d/it'\\''s -x') || exit 1; \
             printf '%s\\n' $n; head -c 65539 -- '/d/it'\\''s -x'; \
             elif [ -e '/d/it'\\''s -x' ]; then exit 21; \
             elif [ -L '/d/it'\\''s -x' ]; then exit 23; \
             else exit 22; fi"
        );
        assert_eq!(
            (
                PREVIEW_DIR,
                PREVIEW_SPECIAL,
                PREVIEW_MISSING,
                PREVIEW_BROKEN
            ),
            (20, 21, 22, 23)
        );
    }

    #[test]
    fn preview_size_comes_from_stat_per_os_never_wc() {
        let l = s(preview_cmd(&RemoteOs::Linux, b"/a", 10));
        let b = s(preview_cmd(&RemoteOs::Bsd, b"/a", 10));
        assert!(l.contains("n=$(stat -L -c %s -- '/a')"), "{l}");
        assert!(b.contains("n=$(stat -L -f %z -- '/a')"), "{b}");
        assert!(!l.contains("wc -c") && !b.contains("wc -c"));
    }

    #[test]
    fn delete_cmd_resolves_physical_paths_and_checks_before_rm() {
        let c = s(delete_cmd(b"/srv/it's", &[b"a b", b"-r", b"x\ny"]));
        assert!(
            c.contains(
                "h=$(cd -- \"$HOME\" 2>/dev/null && pwd -P && echo x) || exit 40; h=${h%?x}"
            ),
            "{c}"
        );
        assert!(
            c.contains("c=$(cd -- '/srv/it'\\''s' && pwd -P && echo x) || exit 41; c=${c%?x}"),
            "{c}"
        );
        assert!(c.contains("for n in 'a b' '-r' 'x\ny'; do"), "{c}");
        assert!(c.contains("case \"$h/\" in \"$t\"/*)"), "{c}");
        assert!(c.contains("exit 42"), "{c}");
        assert!(
            c.ends_with("cd -- \"$c\" && rm -rf -- 'a b' '-r' 'x\ny'"),
            "{c}"
        );
        let rm = c.find("rm -rf").unwrap();
        assert!(c.find("exit 42").unwrap() < rm && c.find("exit 41").unwrap() < rm);
        assert_eq!(
            (DELETE_NO_HOME, DELETE_NO_CWD, DELETE_PROTECTED),
            (40, 41, 42)
        );
    }

    #[test]
    fn stamp_cmd_prints_inode_mtime_size_and_checksum_following_links() {
        assert_eq!(
            s(stamp_cmd(&RemoteOs::Linux, b"/d/it's")),
            "f='/d/it'\\''s'; [ -f \"$f\" ] && s=$(stat -L -c '%i %Y %Z %s %a %u %g' -- \"$f\") && k=$(cksum < \"$f\") && [ \"$(stat -L -c '%i %Y %Z %s %a %u %g' -- \"$f\")\" = \"$s\" ] || { echo 'not a readable regular file' >&2; exit 1; }; printf '%s %s' \"$s\" \"$k\""
        );
        assert!(
            s(stamp_cmd(&RemoteOs::Bsd, b"/x"))
                .contains("stat -L -f '%i %m %c %z %Lp %u %g' -- \"$f\"")
        );
    }

    #[test]
    fn resolve_and_mktemp_cmds() {
        let c = s(resolve_cmd(b"/d/it's"));
        assert!(c.starts_with("p='/d/it'\\''s'; n=0\n"), "{c}");
        assert!(c.contains("while [ -L \"$p\" ]"), "{c}");
        assert!(c.contains("readlink -- \"$p\""), "{c}");
        assert!(
            c.contains("d=$(cd -- \"$d\" && pwd -P && echo x) || exit 1; d=${d%?x}"),
            "{c}"
        );
        assert!(c.ends_with("printf '%s\\0' \"$p\""), "{c}");
        assert_eq!(
            s(mktemp_cmd(b"/srv/it's")),
            "t=$(mktemp '/srv/it'\\''s/.hdir-edit.XXXXXX') || exit 1; printf '%s\\0' \"$t\""
        );
        assert_eq!(
            s(mktemp_cmd(b"/")),
            "t=$(mktemp '/.hdir-edit.XXXXXX') || exit 1; printf '%s\\0' \"$t\""
        );
    }

    #[test]
    fn replace_cmd_checks_size_owner_applies_mode_then_moves_or_rewrites_in_place() {
        let l = s(replace_cmd(
            &RemoteOs::Linux,
            b"/d/.hdir-edit.AB",
            b"/d/it's",
            42,
            b"7 99 98 42 640 501 20 1234 42",
        ));
        assert!(
            l.starts_with("t='/d/.hdir-edit.AB'; o='/d/it'\\''s'\n"),
            "{l}"
        );
        assert!(
            l.contains("[ \"$(wc -c < \"$t\" | tr -d ' ')\" = 42 ] || exit 31"),
            "{l}"
        );
        assert!(
            l.contains("m=$(stat -c '%a %h %u %g' -- \"$o\") || exit 1; set -- $m"),
            "{l}"
        );
        assert!(l.contains("exit 32"), "{l}");
        assert!(l.contains("chmod \"$1\" \"$t\" || exit 1\n"), "{l}");
        assert!(l.contains("mv -- \"$t\" \"$o\"\nfi"), "{l}");
        assert!(l.contains("if [ \"$2\" -gt 1 ]; then\n"), "{l}");
        assert!(
            l.contains("b=$(mktemp \"${o%/*}/.hdir-bak.XXXXXX\") || exit 1"),
            "{l}"
        );
        let (bak, write) = (
            l.find("cp -- \"$o\" \"$b\"").unwrap(),
            l.find("cat --").unwrap(),
        );
        assert!(bak < write, "backup before the in-place write");
        assert!(
            l.contains("cp -- \"$b\" \"$o\"; then rm -f -- \"$b\"; exit 33"),
            "{l}"
        );
        assert!(l.contains("printf '%s\\0' \"$b\"; exit 34"), "{l}");
        assert!(l.find("exit 31").unwrap() < l.find("cat --").unwrap());
        assert!(
            l.contains(
                "same() { [ -f \"$o\" ] && s=$(stat -L -c '%i %Y %Z %s %a %u %g' -- \"$o\") && k=$(cksum < \"$o\") && [ \"$(stat -L -c '%i %Y %Z %s %a %u %g' -- \"$o\")\" = \"$s\" ] && [ \"$s $k\" = '7 99 98 42 640 501 20 1234 42' ]; }\n"
            ),
            "{l}"
        );
        assert!(l.contains("exit 36; }\n"), "{l}");
        assert!(
            l.contains("pre() { [ \"$(stat -L -c '%i %Y %Z %s %a %u %g' -- \"$o\")\" = '7 99 98 42 640 501 20' ]; }\n"),
            "{l}"
        );
        assert!(l.find("pre || changed").unwrap() < l.find("b=$(mktemp").unwrap());
        // checked right before each write: after the backup, after chgrp/chmod
        let backup = l.find("cp -- \"$o\" \"$b\"").unwrap();
        let recheck_ln = l.find("same || { rm -f -- \"$b\"; changed; }").unwrap();
        assert!(backup < recheck_ln && recheck_ln < l.find("cat --").unwrap());
        let recheck_mv = l.find("same || changed\n").unwrap();
        assert!(l.find("chgrp").unwrap() < recheck_mv);
        assert!(
            recheck_mv < l.find("chmod \"$1\"").unwrap(),
            "private until checked"
        );
        assert!(l.find("chmod \"$1\"").unwrap() < l.find("mv --").unwrap());
        assert!(l.contains("g=$(stat -c %g -- \"$t\") || exit 1"), "{l}");
        assert!(
            l.contains("if [ \"$g\" != \"$4\" ]; then chgrp \"$4\" \"$t\" || exit 35; fi"),
            "{l}"
        );
        let (grp, mode, mv) = (
            l.find("chgrp").unwrap(),
            l.find("chmod \"$1\"").unwrap(),
            l.find("mv --").unwrap(),
        );
        assert!(grp < mode && mode < mv, "chgrp, then chmod, then mv");
        assert!(!l.contains("[ -L"), "no write-through-link branch any more");
        let b = s(replace_cmd(&RemoteOs::Bsd, b"/t", b"/o", 1, b"x"));
        assert!(b.contains("stat -f '%Lp %l %u %g' -- \"$o\""), "{b}");
        assert!(b.contains("g=$(stat -f %g -- \"$t\")"), "{b}");
        assert!(
            b.contains("s=$(stat -L -f '%i %m %c %z %Lp %u %g' -- \"$o\")"),
            "{b}"
        );
        assert_eq!(
            (
                REPLACE_SIZE,
                REPLACE_NOT_OWNER,
                REPLACE_RESTORED,
                REPLACE_BACKUP_KEPT
            ),
            (31, 32, 33, 34)
        );
        assert_eq!(s(rm_file_cmd(b"/d/-x y")), "rm -f -- '/d/-x y'");
    }

    #[test]
    fn remote_shell_argv_runs_the_command_in_the_cwd_on_a_tty_over_the_master() {
        assert_eq!(
            remote_shell_argv(&t(Some(2222)), b"/srv/it's d", "ls -l\necho 한글"),
            v(&[
                "ssh",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "-p",
                "2222",
                "-t",
                "--",
                "prod",
                "cd -- '/srv/it'\\''s d' && {\nls -l\necho 한글\n}"
            ])
        );
        // no BatchMode: the command itself may prompt on the tty
        let a = remote_shell_argv(&t(None), b"/", "top");
        assert!(!a.contains(&OsString::from("BatchMode=yes")));
        assert!(!a.contains(&OsString::from("-p")));
    }

    #[test]
    fn rmdir_cmd_quotes() {
        assert_eq!(s(rmdir_cmd(b"/d/-x y")), "rmdir -- '/d/-x y'");
    }

    #[test]
    fn scp_routes_legacy_quoted() {
        let a = t(None);
        let up = scp_argv(
            &Endpoint::Local(Path::new("/l/f x")),
            &Endpoint::Remote(&a, b"/r/d"),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(
            up,
            v(&[
                "scp",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "/l/f x",
                "prod:'/r/d'"
            ])
        );
        let b = t(Some(2222));
        let down = scp_argv(
            &Endpoint::Remote(&b, b"/r/it's"),
            &Endpoint::Local(Path::new("/l")),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(
            down,
            v(&[
                "scp",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "-P",
                "2222",
                "--",
                "prod:'/r/it'\\''s'",
                "/l"
            ])
        );
        let mut c = t(None);
        c.host = "other".into();
        c.control_path = PathBuf::from("/c/cm-2");
        let rr = scp_argv(
            &Endpoint::Remote(&a, b"/s"),
            &Endpoint::Remote(&c, b"/d"),
            ScpMode::Legacy,
        )
        .unwrap();
        // distinct masters: no ControlPath (it would be applied to the wrong host)
        assert_eq!(
            rr,
            v(&[
                "scp",
                "-3",
                "-rp",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "prod:'/s'",
                "other:'/d'"
            ])
        );
        let same = scp_argv(
            &Endpoint::Remote(&a, b"/s"),
            &Endpoint::Remote(&a, b"/d"),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(
            same,
            v(&[
                "scp",
                "-3",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=2",
                "--",
                "prod:'/s'",
                "prod:'/d'"
            ])
        );
    }

    #[test]
    fn scp_mode_detection() {
        use ScpMode::*;
        let m = scp_mode_from_version;
        assert_eq!(
            m("OpenSSH_9.6p1 Ubuntu-3ubuntu13.20, OpenSSL 3.0.13\n"),
            Sftp
        );
        assert_eq!(m("OpenSSH_10.4p1, OpenSSL 3.6.4"), Sftp);
        assert_eq!(m("OpenSSH_9.0p1"), Sftp);
        assert_eq!(m("OpenSSH_8.9p1 Ubuntu"), Legacy);
        assert_eq!(m("OpenSSH_7.4p1, OpenSSL 1.0.2k-fips"), Legacy);
        assert_eq!(m("OpenSSH_for_Windows_9.5p2, LibreSSL 3.8.2"), Sftp);
        assert_eq!(m("OpenSSH_for_Windows_8.1p1, LibreSSL 3.0.2"), Legacy);
        // unparsable output: assume a modern (sftp) scp
        assert_eq!(m(""), Sftp);
        assert_eq!(m("dropbear v2022.83"), Sftp);
    }

    #[test]
    fn sftp_glob_escape_rules() {
        assert_eq!(sftp_glob_escape(b"/a b/it's$x"), b"/a b/it's$x");
        assert_eq!(sftp_glob_escape(b"star*"), b"star\\*");
        assert_eq!(sftp_glob_escape(b"q?[a]\\x"), b"q\\?\\[a\\]\\\\x");
        // braces / tilde / percent are not glob syntax for sftp scp (verified)
        assert_eq!(sftp_glob_escape(b"{a,b}~%"), b"{a,b}~%");
        assert_eq!(
            sftp_glob_escape("한글 파일".as_bytes()),
            "한글 파일".as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn scp_sftp_source_escaped_dest_raw() {
        let a = t(None);
        let up = scp_argv(
            &Endpoint::Local(Path::new("/l/f x")),
            &Endpoint::Remote(&a, b"/r/it's d*r"),
            ScpMode::Sftp,
        )
        .unwrap();
        // destination of an upload is not globbed by sftp: raw, no quotes
        assert_eq!(up[up.len() - 1], OsString::from("prod:/r/it's d*r"));
        let down = scp_argv(
            &Endpoint::Remote(&a, b"/r/it's *.txt"),
            &Endpoint::Local(Path::new("/l")),
            ScpMode::Sftp,
        )
        .unwrap();
        assert_eq!(down[down.len() - 2], OsString::from("prod:/r/it's \\*.txt"));
        let rr = scp_argv(
            &Endpoint::Remote(&a, b"/s[1]"),
            &Endpoint::Remote(&a, b"/d[1]"),
            ScpMode::Sftp,
        )
        .unwrap();
        assert_eq!(rr[rr.len() - 2], OsString::from("prod:/s\\[1\\]"));
        assert_eq!(rr[rr.len() - 1], OsString::from("prod:/d[1]"));
        assert!(!rr.contains(&OsString::from("-O")));
    }

    #[test]
    fn scp_errors_and_edge() {
        let a = t(None);
        assert!(
            scp_argv(
                &Endpoint::Local(Path::new("/a")),
                &Endpoint::Local(Path::new("/b")),
                ScpMode::Sftp
            )
            .is_err()
        );
        let mut bad = t(None);
        bad.host = "-oProxyCommand=x".into();
        assert!(
            scp_argv(
                &Endpoint::Local(Path::new("/a")),
                &Endpoint::Remote(&bad, b"/d"),
                ScpMode::Sftp
            )
            .is_err()
        );
        let mut c = t(Some(1));
        c.host = "o".into();
        c.control_path = PathBuf::from("/c/cm-2");
        assert!(
            scp_argv(
                &Endpoint::Remote(&a, b"/s"),
                &Endpoint::Remote(&c, b"/d"),
                ScpMode::Sftp
            )
            .is_err()
        );
        let r = scp_argv(
            &Endpoint::Local(Path::new("a:b")),
            &Endpoint::Remote(&a, b"/d"),
            ScpMode::Sftp,
        )
        .unwrap();
        #[cfg(unix)]
        assert_eq!(r[r.len() - 2], OsString::from("./a:b"));
        #[cfg(windows)]
        assert_eq!(r[r.len() - 2], OsString::from(r".\a:b"));
    }

    #[cfg(windows)]
    #[test]
    fn scp_windows_local_paths_stay_local() {
        let a = t(None);
        // absolute drive paths are passed natively (Win32-OpenSSH scp treats a
        // single-letter drive prefix as local, not as host "C")
        let up = scp_argv(
            &Endpoint::Local(Path::new(r"C:\Users\x\f.txt")),
            &Endpoint::Remote(&a, b"/r/d"),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(up[up.len() - 2], OsString::from(r"C:\Users\x\f.txt"));
        let down = scp_argv(
            &Endpoint::Remote(&a, b"/r/f"),
            &Endpoint::Local(Path::new(r"D:\한글 dir")),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(down[down.len() - 1], OsString::from(r"D:\한글 dir"));
        // relative path with a colon in its first component is made explicit
        let rel = scp_argv(
            &Endpoint::Local(Path::new(r"a:b\f")),
            &Endpoint::Remote(&a, b"/d"),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(rel[rel.len() - 2], OsString::from(r".\a:b\f"));
        // remote side stays POSIX-quoted
        assert_eq!(up[up.len() - 1], OsString::from("prod:'/r/d'"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_remote_quote_uses_utf8_of_local_names() {
        let a = t(None);
        let r = scp_argv(
            &Endpoint::Local(Path::new(r"C:\x")),
            &Endpoint::Remote(&a, "/r/한 글's".as_bytes()),
            ScpMode::Legacy,
        )
        .unwrap();
        assert_eq!(r[r.len() - 1], OsString::from("prod:'/r/한 글'\\''s'"));
    }

    /// Real round trip against a server (manual):
    /// `HDIR_SCP_HOST=user@host cargo test --lib scp_real -- --ignored`.
    /// Uploads and downloads files with hostile names using the built argv.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn scp_real_round_trip() {
        use std::process::Command;
        let Ok(host) = std::env::var("HDIR_SCP_HOST") else {
            return;
        };
        let ver = Command::new("ssh").arg("-V").output().unwrap();
        let mode = scp_mode_from_version(&String::from_utf8_lossy(&ver.stderr));
        let tgt = SshTarget {
            host,
            port: None,
            control_path: PathBuf::from("/nonexistent/hdir-cm"),
        };
        let run = |argv: Vec<OsString>| {
            // ControlPath points nowhere: ssh just opens a normal connection
            let o = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
            assert!(
                o.status.success(),
                "{argv:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        let sh = |cmd: &[u8]| {
            Command::new("ssh")
                .arg("--")
                .arg(&tgt.host)
                .arg(os_from_bytes(cmd.to_vec()))
                .output()
                .unwrap()
        };
        let base = b"/tmp/hdir-scp-test";
        let rdir = b"/tmp/hdir-scp-test/d ir's $x*";
        assert!(
            sh(&cat(&[b"mkdir -p -- ", &sh_quote(rdir)]))
                .status
                .success()
        );
        let names = [
            "a b",
            "it's",
            "dollar$HOME",
            "star*",
            "q?m",
            "br[ack]et",
            "back\\slash",
            "한글 파일",
            "-dash",
        ];
        let local = tempfile::tempdir().unwrap();
        // decoys that a wrongly-expanded glob would also fetch
        let decoys = ["starX", "q.m", "back-slash"];
        for n in names.iter().chain(decoys.iter()) {
            std::fs::write(local.path().join(n), n).unwrap();
        }
        for n in names.iter().chain(decoys.iter()) {
            run(scp_argv(
                &Endpoint::Local(&local.path().join(n)),
                &Endpoint::Remote(&tgt, rdir),
                mode,
            )
            .unwrap());
        }
        let back = tempfile::tempdir().unwrap();
        for n in names {
            let mut rp = rdir.to_vec();
            rp.push(b'/');
            rp.extend_from_slice(n.as_bytes());
            run(scp_argv(
                &Endpoint::Remote(&tgt, &rp),
                &Endpoint::Local(back.path()),
                mode,
            )
            .unwrap());
            assert_eq!(std::fs::read_to_string(back.path().join(n)).unwrap(), n);
        }
        // exactly the names we sent exist remotely, and nothing was fetched twice
        let got = sh(&cat(&[b"ls -1A -- ", &sh_quote(rdir)])).stdout;
        let mut have: Vec<String> = String::from_utf8(got)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        have.sort();
        let mut want: Vec<String> = names
            .iter()
            .chain(decoys.iter())
            .map(|s| s.to_string())
            .collect();
        want.sort();
        assert_eq!(have, want);
        assert_eq!(std::fs::read_dir(back.path()).unwrap().count(), names.len());
        sh(&cat(&[b"rm -rf -- ", &sh_quote(base)]));
    }
}
