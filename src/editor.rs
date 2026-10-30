//! F4 external editor: build the command line (pure) and run it (injectable).

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

/// Result of one editor run; `Err` is a user-facing (Korean) message.
pub type EditResult = Result<(), String>;

/// Runs an argv (program first) to completion; tests inject fakes.
pub type EditorRunner = Arc<dyn Fn(&[OsString]) -> EditResult + Send + Sync>;

/// Split `s` with POSIX shell-words rules: whitespace separates, `\` escapes
/// outside quotes, `'...'` is literal, `"..."` honors `\"` `\\` `\$` `` \` ``.
pub fn split_command(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // A word has started (so `''` yields an empty argument).
    let mut started = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\\' => {
                cur.push(chars.next().ok_or("끝에 남은 역슬래시")?);
                started = true;
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("닫히지 않은 따옴표".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                            Some(e) => {
                                cur.push('\\');
                                cur.push(e);
                            }
                            None => return Err("닫히지 않은 따옴표".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("닫히지 않은 따옴표".into()),
                    }
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// `$VISUAL`, else `$EDITOR`, else `vi` (empty values are skipped), split into
/// words, with `path` appended as its own argv element.
pub fn editor_argv(
    visual: Option<&str>,
    editor: Option<&str>,
    path: &Path,
) -> Result<Vec<OsString>, String> {
    fn pick(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.trim().is_empty())
    }
    let (name, spec) = match (pick(visual), pick(editor)) {
        (Some(v), _) => ("VISUAL", v),
        (None, Some(e)) => ("EDITOR", e),
        (None, None) => ("EDITOR", "vi"),
    };
    let words = split_command(spec).map_err(|e| format!("${name} 해석 실패: {e}"))?;
    let mut argv: Vec<OsString> = words.into_iter().map(OsString::from).collect();
    argv.push(path.as_os_str().to_os_string());
    Ok(argv)
}

/// Spawn `argv[0]` with the rest as arguments and wait for it.
pub fn run_argv(argv: &[OsString]) -> EditResult {
    let (prog, args) = argv.split_first().ok_or("편집기 명령이 비어 있음")?;
    let status = std::process::Command::new(prog)
        .args(args)
        .status()
        .map_err(|e| format!("편집기 실행 실패: {} ({e})", prog.to_string_lossy()))?;
    if status.success() {
        Ok(())
    } else {
        match status.code() {
            Some(c) => Err(format!("편집기가 오류로 종료됨 (종료 코드 {c})")),
            None => Err("편집기가 신호로 종료됨".to_string()),
        }
    }
}

/// The real runner.
pub fn system_runner() -> EditorRunner {
    Arc::new(run_argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn words(s: &str) -> Vec<String> {
        split_command(s).unwrap()
    }

    #[test]
    fn splits_plain_words() {
        assert_eq!(words("code -w"), ["code", "-w"]);
        assert_eq!(words("  vim   -u  NONE "), ["vim", "-u", "NONE"]);
        assert!(words("").is_empty());
        assert!(words("   ").is_empty());
    }

    #[test]
    fn splits_quotes_and_escapes() {
        assert_eq!(words("'my editor' --x"), ["my editor", "--x"]);
        assert_eq!(words("vim \"a b\" c"), ["vim", "a b", "c"]);
        assert_eq!(words("a\\ b c"), ["a b", "c"]);
        assert_eq!(words("\"a\\\"b\" 'c\\d'"), ["a\"b", "c\\d"]);
        assert_eq!(
            words("\"x\\y\""),
            ["x\\y"],
            "backslash stays before ordinary chars"
        );
        assert_eq!(words("a'b c'd"), ["ab cd"], "adjacent pieces join");
        assert_eq!(words("a '' b"), ["a", "", "b"]);
        assert_eq!(words("에디터 \"한 글\""), ["에디터", "한 글"]);
    }

    #[test]
    fn rejects_unterminated_quotes_and_trailing_backslash() {
        assert!(split_command("vim 'abc").is_err());
        assert!(split_command("vim \"abc").is_err());
        assert!(split_command("vim abc\\").is_err());
    }

    #[test]
    fn argv_prefers_visual_then_editor_then_vi() {
        let p = PathBuf::from("/tmp/a.txt");
        let a = |v, e| editor_argv(v, e, &p).unwrap();
        assert_eq!(a(Some("nano"), Some("vim")), ["nano", "/tmp/a.txt"]);
        assert_eq!(a(None, Some("vim")), ["vim", "/tmp/a.txt"]);
        assert_eq!(a(Some(""), Some("vim")), ["vim", "/tmp/a.txt"]);
        assert_eq!(a(Some("  "), Some("vim")), ["vim", "/tmp/a.txt"]);
        assert_eq!(a(None, None), ["vi", "/tmp/a.txt"]);
        assert_eq!(a(Some(""), Some("")), ["vi", "/tmp/a.txt"]);
    }

    #[test]
    fn argv_splits_editor_with_flags_and_keeps_path_whole() {
        let p = PathBuf::from("/tmp/내 파일 'x' \"y\" $z.txt");
        let argv = editor_argv(Some("code -w"), None, &p).unwrap();
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], "code");
        assert_eq!(argv[1], "-w");
        assert_eq!(argv[2], p.as_os_str());
        let argv = editor_argv(Some("'/opt/my ed/bin/ed' --wait"), None, &p).unwrap();
        assert_eq!(argv[0], "/opt/my ed/bin/ed");
        assert_eq!(argv.len(), 3);
    }

    #[test]
    fn argv_reports_unparsable_variable() {
        let p = PathBuf::from("/tmp/a");
        let e = editor_argv(Some("vim 'x"), None, &p).unwrap_err();
        assert!(e.contains("VISUAL") || e.contains("EDITOR"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn run_argv_reports_exit_status_and_spawn_failure() {
        let ok = run_argv(&["true".into(), "ignored".into()]);
        assert_eq!(ok, Ok(()));
        let bad = run_argv(&["false".into()]).unwrap_err();
        assert!(bad.contains("종료 코드 1"), "{bad}");
        let missing = run_argv(&["/no/such/editor-xyz".into()]).unwrap_err();
        assert!(missing.contains("실행 실패"), "{missing}");
        assert!(run_argv(&[]).is_err());
    }
}
