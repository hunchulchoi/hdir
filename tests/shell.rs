//! The shell wrappers (`shell/hdir.*`) against a fake `hdir` that writes a
//! chosen directory into the `--cwd-file`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const FAKE_HDIR: &str = r#"#!/bin/sh
# usage: hdir --cwd-file FILE [args...]
[ "$1" = "--cwd-file" ] || exit 64
file=$2
shift 2
for a in "$@"; do printf '%s\0' "$a"; done > "$FAKE_ARGS"
if [ -n "${FAKE_DIR+x}" ]; then printf '%s' "$FAKE_DIR" > "$file"; fi
exit "${FAKE_RC:-0}"
"#;

struct Env {
    _root: tempfile::TempDir,
    bin: PathBuf,
    tmp: PathBuf,
    start: PathBuf,
    args: PathBuf,
}

fn env() -> Env {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let (bin, tmp, start) = (base.join("bin"), base.join("tmp"), base.join("start"));
    for d in [&bin, &tmp, &start] {
        fs::create_dir(d).unwrap();
    }
    let fake = bin.join("hdir");
    fs::write(&fake, FAKE_HDIR).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    Env {
        args: base.join("args"),
        _root: root,
        bin,
        tmp,
        start,
    }
}

fn script(shell: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("shell/hdir.{shell}"))
}

fn available(shell: &str) -> bool {
    Command::new(shell)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Source the wrapper, call `h` with `args`, then print `rc=<status>` and `pwd`.
fn run(e: &Env, shell: &str, fake_dir: Option<&str>, rc: i32, args: &str) -> (String, String) {
    let src = script(shell);
    let code = match shell {
        "fish" => format!(
            "source '{}'; h {args}; echo rc=$status; printf '%s' $PWD",
            src.display()
        ),
        _ => format!(
            "source '{}'; h {args}; echo rc=$?; printf '%s' \"$PWD\"",
            src.display()
        ),
    };
    let path = format!(
        "{}:{}",
        e.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new(shell);
    c.args(["-c", &code])
        .current_dir(&e.start)
        .env("PATH", path)
        .env("TMPDIR", &e.tmp)
        .env("FAKE_ARGS", &e.args)
        .env("FAKE_RC", rc.to_string())
        .env_remove("FAKE_DIR");
    if let Some(d) = fake_dir {
        c.env("FAKE_DIR", d);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{shell}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let (rc, pwd) = text.split_once('\n').unwrap_or((&text, ""));
    (rc.to_string(), pwd.to_string())
}

fn shells() -> Vec<&'static str> {
    ["bash", "zsh", "fish"]
        .into_iter()
        .filter(|s| script(s).exists() || *s != "fish")
        .filter(|s| available(s))
        .collect()
}

fn leftovers(e: &Env) -> usize {
    fs::read_dir(&e.tmp).unwrap().count()
}

#[test]
fn at_least_bash_is_tested() {
    assert!(shells().contains(&"bash"), "bash is required for this test");
}

#[test]
fn h_changes_to_the_directory_written_by_hdir() {
    for sh in shells() {
        let e = env();
        let target = e.start.parent().unwrap().join("대상 폴더 x");
        fs::create_dir(&target).unwrap();
        let (rc, pwd) = run(&e, sh, Some(target.to_str().unwrap()), 0, "");
        assert_eq!(
            (rc.as_str(), pwd.as_str()),
            ("rc=0", target.to_str().unwrap()),
            "{sh}"
        );
        assert_eq!(leftovers(&e), 0, "{sh}: temp file removed");
    }
}

#[test]
fn h_stays_when_file_is_empty_missing_dir_or_same_dir() {
    for sh in shells() {
        let e = env();
        let start = e.start.to_str().unwrap().to_string();
        let gone = e.start.join("gone");
        for fake in [Some(""), Some(gone.to_str().unwrap()), Some(&start[..])] {
            let (_, pwd) = run(&e, sh, fake, 0, "");
            assert_eq!(pwd, start, "{sh} {fake:?}");
        }
        let (_, pwd) = run(&e, sh, None, 0, "");
        assert_eq!(pwd, start, "{sh}: no write at all");
        assert_eq!(leftovers(&e), 0, "{sh}");
    }
}

#[test]
fn h_passes_arguments_and_keeps_the_exit_code() {
    for sh in shells() {
        let e = env();
        let (rc, _) = run(&e, sh, None, 3, "'my dir' --flag");
        assert_eq!(rc, "rc=3", "{sh}");
        let raw = fs::read(&e.args).unwrap();
        let got: Vec<&[u8]> = raw.split(|&b| b == 0).collect();
        // NUL-terminated records: exactly two args plus the empty tail.
        assert_eq!(got, [&b"my dir"[..], b"--flag", b""], "{sh}");
    }
}

#[test]
fn h_handles_a_leading_dash_directory_name() {
    for sh in shells() {
        let e = env();
        let target = e.start.join("-dash");
        fs::create_dir(&target).unwrap();
        // Relative on purpose: `cd -- -dash` must not read it as an option.
        let (_, pwd) = run(&e, sh, Some("-dash"), 0, "");
        assert_eq!(pwd, target.to_str().unwrap(), "{sh}");
    }
}

#[test]
fn h_keeps_trailing_newlines_in_the_directory_name() {
    for sh in shells() {
        let e = env();
        let target = e.start.join("nl\n");
        fs::create_dir(&target).unwrap();
        let plain = e.start.join("nl");
        fs::create_dir(&plain).unwrap();
        let (_, pwd) = run(&e, sh, Some(target.to_str().unwrap()), 0, "");
        assert_eq!(pwd, target.to_str().unwrap(), "{sh}: trailing newline kept");
        assert_eq!(leftovers(&e), 0, "{sh}");
    }
}
