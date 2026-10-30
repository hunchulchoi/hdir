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
    port_args(t.port, "-p", &mut v);
    v.push(os("-fN"));
    v.push(os("--"));
    v.push(os(&t.host));
    v
}

/// `ssh -O check` argv (is the master alive?).
pub fn check_argv(t: &SshTarget) -> Vec<OsString> {
    let mut v = vec![os("ssh"), os("-O"), os("check"), os("-o"), control_opt(t)];
    port_args(t.port, "-p", &mut v);
    v.push(os("--"));
    v.push(os(&t.host));
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
stat -f '%HT%t%z%t%m%t%Lp%t' -- \"./$f\" && printf '%s\\0' \"$f\"; done",
            ])
        }
        _ => {
            let filter: &[u8] = if show_hidden { b"" } else { b" ! -name '.*'" };
            cat(&[
                b"cd -- ",
                &q,
                b" && find . -mindepth 1 -maxdepth 1",
                filter,
                b" -printf '%y\\t%s\\t%T@\\t%m\\t%P\\0'",
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
    let mut it = rec.splitn(5, |&b| b == b'\t');
    let kind = parse_kind(os, it.next()?)?;
    let size: u64 = std::str::from_utf8(it.next()?).ok()?.parse().ok()?;
    let mtime = parse_mtime(it.next()?);
    let mode = u32::from_str_radix(std::str::from_utf8(it.next()?).ok()?, 8).ok()?;
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

fn remote_arg(t: &SshTarget, path: &[u8]) -> Result<OsString, String> {
    if t.host.is_empty() || t.host.starts_with('-') {
        return Err(format!("invalid host: {:?}", t.host));
    }
    let mut b = t.host.as_bytes().to_vec();
    b.push(b':');
    b.extend_from_slice(&sh_quote(path));
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
pub fn scp_argv(src: &Endpoint, dst_dir: &Endpoint) -> Result<Vec<OsString>, String> {
    let mut v = vec![os("scp")];
    let (ctrl, port, a, b) = match (src, dst_dir) {
        (Endpoint::Local(_), Endpoint::Local(_)) => {
            return Err("local to local copy is not an scp route (use fs_ops)".into());
        }
        (Endpoint::Local(p), Endpoint::Remote(t, d)) => {
            (Some(*t), t.port, local_arg(p), remote_arg(t, d)?)
        }
        (Endpoint::Remote(t, s), Endpoint::Local(p)) => {
            (Some(*t), t.port, remote_arg(t, s)?, local_arg(p))
        }
        (Endpoint::Remote(ts, s), Endpoint::Remote(td, d)) => {
            if ts.port != td.port {
                return Err("remote to remote with different ports is unsupported".into());
            }
            v.push(os("-3"));
            // Only share ControlPath when both ends use the same master.
            let ctrl = (ts.control_path == td.control_path).then_some(*ts);
            (ctrl, ts.port, remote_arg(ts, s)?, remote_arg(td, d)?)
        }
    };
    v.push(os("-rp"));
    if let Some(t) = ctrl {
        v.push(os("-o"));
        v.push(control_opt(t));
    }
    port_args(port, "-P", &mut v);
    v.push(os("--"));
    v.push(a);
    v.push(b);
    Ok(v)
}

impl RemoteEntry {
    /// Convert for panel display. Symlink targets are unknown (`target_is_dir: false`);
    /// special files are shown as plain files.
    pub fn to_entry(&self) -> Entry {
        let kind = match self.kind {
            RemoteKind::Dir => EntryKind::Dir,
            RemoteKind::Symlink => EntryKind::Symlink {
                target_is_dir: false,
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
                "--",
                "prod",
                "ls 'a b'"
            ])
        );
        let a = exec_argv(&t(Some(2200)), b"x");
        assert_eq!(a[5], OsString::from("-p"));
        assert_eq!(a[6], OsString::from("2200"));
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
            "cd -- '/srv/a b' && find . -mindepth 1 -maxdepth 1 -printf '%y\\t%s\\t%T@\\t%m\\t%P\\0'"
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
        assert!(c.contains("stat -f '%HT%t%z%t%m%t%Lp%t' -- \"./$f\""));
        assert!(c.contains("printf '%s\\0' \"$f\""));
        assert!(c.ends_with("done"));
        let c = s(list_cmd(&RemoteOs::Bsd, b"/it's", false));
        assert!(c.contains("'/it'\\''s'"));
        assert!(c.contains("for f in *; do"));
        assert!(!c.contains(".[!.]*"));
    }

    #[test]
    fn parse_linux_sample() {
        let mut out = Vec::new();
        out.extend_from_slice(b"d\t4096\t1700000000.5000000000\t755\tsub dir\0");
        out.extend_from_slice(b"f\t12\t1700000001.0000000000\t644\t\xed\x95\x9c\xea\xb8\x80.txt\0");
        out.extend_from_slice(b"l\t5\t1700000002.25\t777\tlink\0");
        out.extend_from_slice(b"p\t0\t1700000003.0\t600\tfifo\0");
        out.extend_from_slice(b"s\t0\t1700000003.0\t600\tsock\0");
        out.extend_from_slice(b"c\t0\t1700000003.0\t600\tchr\0");
        out.extend_from_slice(b"f\t1\t1700000004.0\t644\ttab\tin\nname\0");
        out.extend_from_slice(b"f\t1\t1700000004.0\t644\tbad\xff\xfename\0");
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
        let out = b"Directory\t96\t1700000000\t755\td\0Regular File\t3\t1700000001\t644\tf f\0\
Symbolic Link\t4\t1700000002\t755\tl\0Fifo File\t0\t1700000003\t644\tq\0Socket\t0\t1\t644\ts\0\
Character Device\t0\t1\t600\tc\0Block Device\t0\t1\t600\tb\0";
        let (e, bad) = parse_listing(&RemoteOs::Bsd, out);
        assert_eq!(bad, 0);
        let k: Vec<_> = e.iter().map(|x| x.kind).collect();
        use RemoteKind::*;
        assert_eq!(
            k,
            vec![Dir, File, Symlink, Special, Special, Special, Special]
        );
        assert_eq!(e[1].name, OsString::from("f f"));
        assert_eq!(
            e[0].mtime,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(e[0].mode, 0o755);
    }

    #[test]
    fn parse_malformed_counted() {
        let out = b"f\t1\t1.0\t644\tok\0garbage\0f\tNaN\t1.0\t644\tx\0f\t1\t1.0\t999\tbadmode\0\
f\t1\t1.0\t644\t\0zz\t1\t1.0\t644\tn\0\0f\t2\t2.0\t600\tok2\0";
        let (e, bad) = parse_listing(&RemoteOs::Linux, out);
        assert_eq!(e.len(), 2);
        assert_eq!(bad, 6);
        assert_eq!(e[1].name, OsString::from("ok2"));
        let (e, bad) = parse_listing(&RemoteOs::Bsd, b"Weird\t1\t1\t644\tn\0");
        assert!(e.is_empty());
        assert_eq!(bad, 1);
        assert_eq!(parse_listing(&RemoteOs::Linux, b""), (vec![], 0));
    }

    #[test]
    fn to_entry_conversion() {
        let r = RemoteEntry {
            name: OsString::from("한글"),
            kind: RemoteKind::Symlink,
            size: 7,
            mtime: None,
            mode: 0o777,
        };
        let e = r.to_entry();
        assert_eq!(e.name, "한글");
        assert_eq!(e.os_name, OsString::from("한글"));
        assert_eq!(
            e.kind,
            EntryKind::Symlink {
                target_is_dir: false
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
    fn scp_routes() {
        let a = t(None);
        let up = scp_argv(
            &Endpoint::Local(Path::new("/l/f x")),
            &Endpoint::Remote(&a, b"/r/d"),
        )
        .unwrap();
        assert_eq!(
            up,
            v(&[
                "scp",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
                "--",
                "/l/f x",
                "prod:'/r/d'"
            ])
        );
        let b = t(Some(2222));
        let down = scp_argv(
            &Endpoint::Remote(&b, b"/r/it's"),
            &Endpoint::Local(Path::new("/l")),
        )
        .unwrap();
        assert_eq!(
            down,
            v(&[
                "scp",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
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
        let rr = scp_argv(&Endpoint::Remote(&a, b"/s"), &Endpoint::Remote(&c, b"/d")).unwrap();
        // distinct masters: no ControlPath (it would be applied to the wrong host)
        assert_eq!(
            rr,
            v(&["scp", "-3", "-rp", "--", "prod:'/s'", "other:'/d'"])
        );
        let same = scp_argv(&Endpoint::Remote(&a, b"/s"), &Endpoint::Remote(&a, b"/d")).unwrap();
        assert_eq!(
            same,
            v(&[
                "scp",
                "-3",
                "-rp",
                "-o",
                "ControlPath=/c/cm-1",
                "--",
                "prod:'/s'",
                "prod:'/d'"
            ])
        );
    }

    #[test]
    fn scp_errors_and_edge() {
        let a = t(None);
        assert!(
            scp_argv(
                &Endpoint::Local(Path::new("/a")),
                &Endpoint::Local(Path::new("/b"))
            )
            .is_err()
        );
        let mut bad = t(None);
        bad.host = "-oProxyCommand=x".into();
        assert!(
            scp_argv(
                &Endpoint::Local(Path::new("/a")),
                &Endpoint::Remote(&bad, b"/d")
            )
            .is_err()
        );
        let mut c = t(Some(1));
        c.host = "o".into();
        c.control_path = PathBuf::from("/c/cm-2");
        assert!(scp_argv(&Endpoint::Remote(&a, b"/s"), &Endpoint::Remote(&c, b"/d")).is_err());
        let r = scp_argv(
            &Endpoint::Local(Path::new("a:b")),
            &Endpoint::Remote(&a, b"/d"),
        )
        .unwrap();
        assert_eq!(r[r.len() - 2], OsString::from("./a:b"));
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
        )
        .unwrap();
        assert_eq!(up[up.len() - 2], OsString::from(r"C:\Users\x\f.txt"));
        let down = scp_argv(
            &Endpoint::Remote(&a, b"/r/f"),
            &Endpoint::Local(Path::new(r"D:\한글 dir")),
        )
        .unwrap();
        assert_eq!(down[down.len() - 1], OsString::from(r"D:\한글 dir"));
        // relative path with a colon in its first component is made explicit
        let rel = scp_argv(
            &Endpoint::Local(Path::new(r"a:b\f")),
            &Endpoint::Remote(&a, b"/d"),
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
        )
        .unwrap();
        assert_eq!(r[r.len() - 1], OsString::from("prod:'/r/한 글'\\''s'"));
    }
}
