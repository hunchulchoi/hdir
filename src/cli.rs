//! Command-line arguments and the `--cwd-file` writer.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::{fs_ops, platform};

pub const USAGE: &str = "사용법: hdir [--cwd-file PATH] [START_DIR]\n\n\
옵션:\n  --cwd-file PATH  종료 시 활성 패널의 디렉터리를 PATH에 기록 (셸 통합용)\n  -h, --help       도움말\n  -V, --version    버전\n";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub cwd_file: Option<PathBuf>,
    pub start_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    Run(Options),
    Help,
    Version,
}

/// Parse the arguments after the program name.
pub fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Result<Cli, String> {
    let mut opts = Options::default();
    let mut only_positional = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if only_positional || !text.starts_with('-') || text == "-" {
            set_start(&mut opts, arg)?;
        } else if text == "--" {
            only_positional = true;
        } else if text == "-h" || text == "--help" {
            return Ok(Cli::Help);
        } else if text == "-V" || text == "--version" {
            return Ok(Cli::Version);
        } else if text == "--cwd-file" {
            let v = args.next().ok_or("--cwd-file: 경로가 필요함")?;
            set_cwd_file(&mut opts, v)?;
        } else if text.starts_with("--cwd-file=") {
            set_cwd_file(&mut opts, value_after_eq(&arg, "--cwd-file=".len()))?;
        } else {
            return Err(format!("알 수 없는 옵션: {text}"));
        }
    }
    Ok(Cli::Run(opts))
}

/// `arg` without its first `skip` bytes, keeping non-UTF-8 bytes on unix
/// (lossy on Windows, where the prefix is ASCII anyway).
fn value_after_eq(arg: &OsString, skip: usize) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        OsString::from_vec(arg.as_bytes()[skip..].to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(arg.to_string_lossy()[skip..].to_string())
    }
}

fn set_start(opts: &mut Options, v: OsString) -> Result<(), String> {
    if opts.start_dir.replace(PathBuf::from(v)).is_some() {
        return Err("시작 디렉터리는 하나만 지정할 수 있음".into());
    }
    Ok(())
}

fn set_cwd_file(opts: &mut Options, v: OsString) -> Result<(), String> {
    if v.is_empty() {
        return Err("--cwd-file: 경로가 필요함".into());
    }
    if opts.cwd_file.replace(PathBuf::from(v)).is_some() {
        return Err("--cwd-file은 한 번만 지정할 수 있음".into());
    }
    Ok(())
}

/// Write the cwd file atomically (temp file in the same directory, then
/// rename). `Some(dir)`: the raw bytes of `dir`, no newline. `None` (quit
/// without cd): an empty file.
///
/// The temp is created exclusively under a unique name (retrying on a
/// collision), and only a temp this call created is ever removed.
pub fn write_cwd_file(path: &Path, cwd: Option<&Path>) -> io::Result<()> {
    write_cwd_file_with(path, cwd, &mut || fs_ops::temp_sibling(path))
}

fn write_cwd_file_with(
    path: &Path,
    cwd: Option<&Path>,
    next_temp: &mut dyn FnMut() -> PathBuf,
) -> io::Result<()> {
    let (tmp, mut f) = create_temp(next_temp)?;
    let result = (|| {
        if let Some(dir) = cwd {
            f.write_all(&platform::os_bytes(dir.as_os_str()))?;
        }
        drop(f);
        platform::rename_replace(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn create_temp(next: &mut dyn FnMut() -> PathBuf) -> io::Result<(PathBuf, std::fs::File)> {
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = next();
        match platform::create_private_file(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "임시 파일 이름을 정하지 못함",
    ))
}

const TEMP_ATTEMPTS: usize = 100;

/// Directory hdir starts in: `arg` resolved to its final physical path first
/// (symlinks before any `..`), then that exact path is validated.
pub fn start_dir(arg: Option<&Path>) -> Result<PathBuf, String> {
    let Some(arg) = arg else {
        return Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    };
    crate::cmdline::canonical_dir(arg).map_err(|e| format!("{}: {e}", arg.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn run(a: &[&str]) -> Result<Cli, String> {
        parse_args(args(a))
    }

    fn opts(cwd_file: Option<&str>, start: Option<&str>) -> Result<Cli, String> {
        Ok(Cli::Run(Options {
            cwd_file: cwd_file.map(PathBuf::from),
            start_dir: start.map(PathBuf::from),
        }))
    }

    #[test]
    fn no_args_and_plain_start_dir() {
        assert_eq!(run(&[]), opts(None, None));
        assert_eq!(run(&["/tmp"]), opts(None, Some("/tmp")));
    }

    #[test]
    fn cwd_file_forms() {
        assert_eq!(run(&["--cwd-file", "/t/x"]), opts(Some("/t/x"), None));
        assert_eq!(run(&["--cwd-file=/t/x"]), opts(Some("/t/x"), None));
        assert_eq!(
            run(&["--cwd-file", "/t/x", "/d"]),
            opts(Some("/t/x"), Some("/d"))
        );
        assert_eq!(
            run(&["/d", "--cwd-file", "/t/x"]),
            opts(Some("/t/x"), Some("/d"))
        );
    }

    #[test]
    fn help_version_and_double_dash() {
        assert_eq!(run(&["--help"]), Ok(Cli::Help));
        assert_eq!(run(&["-h"]), Ok(Cli::Help));
        assert_eq!(run(&["--version"]), Ok(Cli::Version));
        assert_eq!(run(&["-V"]), Ok(Cli::Version));
        assert_eq!(run(&["--", "--weird"]), opts(None, Some("--weird")));
    }

    #[test]
    fn errors() {
        assert!(run(&["--cwd-file"]).unwrap_err().contains("--cwd-file"));
        assert!(run(&["--cwd-file="]).is_err());
        assert!(run(&["--bogus"]).unwrap_err().contains("--bogus"));
        assert!(run(&["/a", "/b"]).is_err(), "one start dir only");
        assert!(run(&["--cwd-file", "a", "--cwd-file", "b"]).is_err());
    }

    #[test]
    fn writes_exact_bytes_without_newline_and_leaves_no_temp() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("cwd");
        std::fs::write(&f, "old contents that are longer").unwrap();
        write_cwd_file(&f, Some(Path::new("/tmp/한글 dir"))).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), "/tmp/한글 dir".as_bytes());
        let names: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from("cwd")]);
    }

    #[test]
    fn no_cd_truncates_to_empty() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("cwd");
        std::fs::write(&f, "/stale").unwrap();
        write_cwd_file(&f, None).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"");
        let fresh = t.path().join("new");
        write_cwd_file(&fresh, None).unwrap();
        assert!(fresh.exists());
    }

    #[test]
    fn missing_directory_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("nope/cwd");
        assert!(write_cwd_file(&f, Some(Path::new("/x"))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_bytes_are_preserved() {
        use std::os::unix::ffi::OsStrExt;
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("cwd");
        let dir = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff\xfe"));
        write_cwd_file(&f, Some(dir)).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"/tmp/\xff\xfe");
    }

    #[test]
    fn temp_is_a_hidden_sibling_and_no_temp_is_left_behind() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("cwd");
        write_cwd_file(&f, None).unwrap();
        let names: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("cwd")]);
    }

    #[test]
    #[cfg(unix)]
    fn cwd_file_equals_form_keeps_non_utf8_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let raw = OsString::from_vec(b"--cwd-file=/t/\xff\xfe".to_vec());
        match parse_args([raw]) {
            Ok(Cli::Run(o)) => {
                assert_eq!(o.cwd_file.unwrap().as_os_str().as_bytes(), b"/t/\xff\xfe")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cwd_file_write_never_deletes_a_file_it_did_not_create() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("cwd");
        let foreign = fs_ops::temp_sibling(&target);
        fs::write(&foreign, "foreign").unwrap();
        let mut names = vec![d.path().join("fresh"), foreign.clone()];
        let mut next = || names.pop().unwrap();
        write_cwd_file_with(&target, Some(Path::new("/x/y")), &mut next).unwrap();
        assert!(!d.path().join("fresh").exists());
        assert_eq!(fs::read(&target).unwrap(), b"/x/y");
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign");
    }

    #[test]
    fn start_dir_resolves_symlink_before_dotdot_and_validates_the_result() {
        let d = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(d.path()).unwrap();
        // Path::join folds `..` textually on Windows verbatim paths: append raw.
        let raw = |b: &Path, rel: &str| PathBuf::from(format!("{}/{rel}", b.display()));
        fs::create_dir_all(base.join("real/inner")).unwrap();
        fs::create_dir(base.join("other")).unwrap();
        fs::write(base.join("real/file"), "").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("real/inner"), base.join("link")).unwrap();
            // physical: link -> real/inner, so link/.. is real
            assert_eq!(
                start_dir(Some(&raw(&base, "link/.."))),
                Ok(base.join("real"))
            );
        }
        assert_eq!(
            start_dir(Some(&raw(&base, "real/../other"))),
            Ok(base.join("other"))
        );
        assert!(start_dir(Some(&raw(&base, "missing/.."))).is_err());
        assert!(start_dir(Some(&base.join("real/file"))).is_err());
    }
}
