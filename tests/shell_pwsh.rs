//! `shell/hdir.ps1` against a fake `hdir` script that writes a chosen folder
//! into the `--cwd-file`. Runs wherever `pwsh` is installed (the Windows
//! gate); skipped elsewhere.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FAKE_HDIR: &str = r#"
if ($args[0] -ne '--cwd-file') { exit 64 }
[System.IO.File]::WriteAllText($env:FAKE_ARGS, ($args[2..($args.Count)] -join '|'))
if ($null -ne $env:FAKE_DIR) {
    [System.IO.File]::WriteAllText($args[1], $env:FAKE_DIR, (New-Object System.Text.UTF8Encoding $false))
}
exit [int]$env:FAKE_RC
"#;

/// Whether pwsh runs here. On Windows it must: the Windows gate is where
/// this integration is checked, so a missing pwsh fails instead of skipping.
fn pwsh() -> bool {
    let ok = Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok || !cfg!(windows), "pwsh must run on Windows");
    ok
}

struct Env {
    _root: tempfile::TempDir,
    bin: PathBuf,
    start: PathBuf,
    args: PathBuf,
}

fn env() -> Env {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let base = PathBuf::from(base.to_string_lossy().trim_start_matches(r"\\?\"));
    let (bin, start) = (base.join("bin"), base.join("start"));
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&start).unwrap();
    fs::write(bin.join("hdir.ps1"), FAKE_HDIR).unwrap();
    Env {
        args: base.join("args"),
        _root: root,
        bin,
        start,
    }
}

/// Dot-source the wrapper, call `h` with `args`; returns (rc, location).
fn run(e: &Env, fake_dir: Option<&str>, rc: i32, args: &str) -> (String, String) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("shell/hdir.ps1");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!(
        "{}{sep}{}",
        e.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let code = format!(
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding = [Text.Encoding]::UTF8; \
         . '{}'; h {args}; \
         Write-Output (\"rc=\" + $LASTEXITCODE); Write-Output (Get-Location).ProviderPath",
        src.display()
    );
    let mut c = Command::new("pwsh");
    c.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &code,
    ])
    .current_dir(&e.start)
    .env("PATH", path)
    .env("FAKE_ARGS", &e.args)
    .env("FAKE_RC", rc.to_string());
    match fake_dir {
        Some(d) => c.env("FAKE_DIR", d),
        None => c.env_remove("FAKE_DIR"),
    };
    let out = c.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let rc = lines.next().unwrap_or_default().to_string();
    let loc = lines.next().unwrap_or_default().to_string();
    (rc, loc)
}

#[test]
fn h_changes_to_the_folder_hdir_reports() {
    if !pwsh() {
        eprintln!("skipped: no pwsh");
        return;
    }
    let e = env();
    let target = e.start.parent().unwrap().join("한글 it's 폴더");
    fs::create_dir(&target).unwrap();
    let (rc, loc) = run(&e, Some(&target.to_string_lossy()), 0, "-x 'a b'");
    assert_eq!(
        (rc.as_str(), loc.as_str()),
        ("rc=0", &*target.to_string_lossy())
    );
    assert_eq!(
        fs::read_to_string(&e.args).unwrap(),
        "-x|a b",
        "arguments pass through"
    );
}

#[test]
fn h_stays_put_without_a_folder_and_keeps_the_exit_code() {
    if !pwsh() {
        eprintln!("skipped: no pwsh");
        return;
    }
    let e = env();
    let here = e.start.to_string_lossy().into_owned();
    // quit without cd: an empty file
    assert_eq!(run(&e, Some(""), 0, ""), ("rc=0".into(), here.clone()));
    // a folder that does not exist
    let gone = e.start.join("gone").to_string_lossy().into_owned();
    assert_eq!(run(&e, Some(&gone), 0, ""), ("rc=0".into(), here.clone()));
    // hdir failed: its exit code comes back
    assert_eq!(run(&e, None, 3, ""), ("rc=3".into(), here));
}
