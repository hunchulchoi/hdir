//! Command-line arguments and the `--cwd-file` writer.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::platform;

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
        } else if let Some(v) = text.strip_prefix("--cwd-file=") {
            set_cwd_file(&mut opts, OsString::from(v))?;
        } else {
            return Err(format!("알 수 없는 옵션: {text}"));
        }
    }
    Ok(Cli::Run(opts))
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
pub fn write_cwd_file(path: &Path, cwd: Option<&Path>) -> io::Result<()> {
    let tmp = temp_path(path);
    let result = (|| {
        let mut f = platform::create_private_file(&tmp)?;
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

/// Hidden sibling of `path` (same directory, so the rename stays atomic).
fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = format!(".{name}.{}.tmp", std::process::id());
    path.with_file_name(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn temp_path_is_a_hidden_sibling() {
        let a = temp_path(Path::new("/a/b/cwd"));
        assert_eq!(a.parent(), Some(Path::new("/a/b")));
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".cwd.")
        );
        assert_eq!(temp_path(Path::new("rel")).parent(), Some(Path::new("")));
    }
}
