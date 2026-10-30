//! File/directory preview logic (pure classification + IO entry point).
//! No rendering code lives here.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_LINES: usize = 200;
const DIR_LIMIT: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    Text {
        lines: Vec<String>,
        truncated: bool,
    },
    Binary {
        size: u64,
        kind: &'static str,
    },
    Dir {
        files: u64,
        dirs: u64,
        total_size: u64,
        truncated: bool,
        /// entries that could not be read (stat/readdir error)
        errors: u64,
    },
    Empty,
    Error(String),
}

/// Read caps for a preview (`PREVIEW` for the panel, `VIEWER` for F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

impl Limits {
    pub const PREVIEW: Limits = Limits {
        max_bytes: MAX_BYTES,
        max_lines: MAX_LINES,
    };
    pub const VIEWER: Limits = Limits {
        max_bytes: 4 * 1024 * 1024,
        max_lines: 100_000,
    };
}

pub fn preview_path(path: &Path) -> Preview {
    preview_path_with(path, Limits::PREVIEW)
}

pub fn preview_path_with(path: &Path, limits: Limits) -> Preview {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() {
                return Preview::Error("깨진 링크".to_string());
            }
            return Preview::Error(e.to_string());
        }
    };
    if meta.is_dir() {
        return match fs::read_dir(path) {
            Ok(rd) => summarize_dir(rd.map(|e| {
                let m = e?.metadata()?;
                Ok((m.is_dir(), m.len()))
            })),
            Err(e) => Preview::Error(e.to_string()),
        };
    }
    if !meta.is_file() {
        // fifo / socket / device: opening could block forever or have side effects
        return Preview::Binary {
            size: meta.len(),
            kind: "특수 파일",
        };
    }
    let mut buf = Vec::new();
    let read = fs::File::open(path)
        .and_then(|f| f.take((limits.max_bytes + 3) as u64).read_to_end(&mut buf));
    match read {
        Ok(_) => classify_with(&buf, meta.len(), limits),
        Err(e) => Preview::Error(e.to_string()),
    }
}

fn binary_kind(b: &[u8]) -> &'static str {
    const MACHO: [[u8; 4]; 5] = [
        [0xFE, 0xED, 0xFA, 0xCE],
        [0xFE, 0xED, 0xFA, 0xCF],
        [0xCE, 0xFA, 0xED, 0xFE],
        [0xCF, 0xFA, 0xED, 0xFE],
        [0xCA, 0xFE, 0xBA, 0xBE],
    ];
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "PNG 이미지"
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "JPEG 이미지"
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        "GIF 이미지"
    } else if b.starts_with(b"%PDF-") {
        "PDF 문서"
    } else if b.starts_with(b"PK\x03\x04") {
        "ZIP 압축"
    } else if b.starts_with(&[0x1f, 0x8b]) {
        "GZIP 압축"
    } else if b.starts_with(b"\x7fELF") {
        "ELF 실행파일"
    } else if MACHO.iter().any(|m| b.starts_with(m)) {
        "Mach-O 실행파일"
    } else {
        "바이너리"
    }
}

fn clean_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

pub fn classify(bytes: &[u8], file_size: u64) -> Preview {
    classify_with(bytes, file_size, Limits::PREVIEW)
}

pub fn classify_with(bytes: &[u8], file_size: u64, limits: Limits) -> Preview {
    if bytes.is_empty() {
        return if file_size == 0 {
            Preview::Empty
        } else {
            Preview::Binary {
                size: file_size,
                kind: "바이너리",
            }
        };
    }
    let kind = binary_kind(bytes);
    let binary = Preview::Binary {
        size: file_size,
        kind,
    };
    if kind != "바이너리" || bytes[..bytes.len().min(8192)].contains(&0) {
        return binary;
    }
    // Cut at limits.max_bytes, extending over continuation bytes of a split char.
    let mut end = bytes.len().min(limits.max_bytes);
    while end < bytes.len() && end < limits.max_bytes + 3 && (bytes[end] & 0xC0) == 0x80 {
        end += 1;
    }
    let mut truncated = (end as u64) < file_size || end < bytes.len();
    let mut slice = &bytes[..end];
    let decoded = match std::str::from_utf8(slice) {
        Ok(s) => s.to_string(),
        Err(e) if e.error_len().is_none() && truncated => {
            slice = &slice[..e.valid_up_to()];
            String::from_utf8_lossy(slice).into_owned()
        }
        Err(_) => {
            let lossy = String::from_utf8_lossy(slice);
            let bad = lossy.chars().filter(|&c| c == '\u{FFFD}').count();
            let total = lossy.chars().count();
            if bad * 100 > total {
                return binary;
            }
            lossy.into_owned()
        }
    };
    let mut lines: Vec<String> = decoded.lines().map(clean_line).collect();
    if lines.len() > limits.max_lines {
        lines.truncate(limits.max_lines);
        truncated = true;
    }
    Preview::Text { lines, truncated }
}

pub fn summarize_dir(mut entries: impl Iterator<Item = io::Result<(bool, u64)>>) -> Preview {
    let (mut files, mut dirs, mut total_size, mut errors) = (0u64, 0u64, 0u64, 0u64);
    for _ in 0..DIR_LIMIT {
        match entries.next() {
            None => {
                return Preview::Dir {
                    files,
                    dirs,
                    total_size,
                    truncated: false,
                    errors,
                };
            }
            Some(Ok((true, _))) => dirs += 1,
            Some(Ok((false, sz))) => {
                files += 1;
                total_size += sz;
            }
            Some(Err(_)) => errors += 1,
        }
    }
    Preview::Dir {
        files,
        dirs,
        total_size,
        truncated: entries.next().is_some(),
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(p: Preview) -> (Vec<String>, bool) {
        match p {
            Preview::Text { lines, truncated } => (lines, truncated),
            o => panic!("expected Text, got {o:?}"),
        }
    }

    #[test]
    fn viewer_limits_keep_more_than_the_preview_cap() {
        let big: String = (0..1000).map(|i| format!("line{i}\n")).collect();
        let (lines, truncated) = text(classify(big.as_bytes(), big.len() as u64));
        assert_eq!((lines.len(), truncated), (MAX_LINES, true));
        let (lines, truncated) = text(classify_with(
            big.as_bytes(),
            big.len() as u64,
            Limits::VIEWER,
        ));
        assert_eq!((lines.len(), truncated), (1000, false));
    }

    #[test]
    fn custom_limits_truncate_by_lines_and_bytes() {
        let small = Limits {
            max_bytes: 20,
            max_lines: 2,
        };
        let (lines, truncated) = text(classify_with(b"a\nb\nc\n", 6, small));
        assert_eq!((lines, truncated), (vec!["a".into(), "b".into()], true));
        let data = "x".repeat(100);
        let (lines, truncated) = text(classify_with(data.as_bytes(), 100, small));
        assert_eq!((lines[0].len(), truncated), (20, true));
    }

    #[test]
    fn preview_path_with_reads_past_the_preview_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("big.txt");
        let big: String = (0..5000).map(|i| format!("행{i}\n")).collect();
        std::fs::write(&f, &big).unwrap();
        let (lines, truncated) = text(preview_path_with(&f, Limits::VIEWER));
        assert_eq!((lines.len(), truncated), (5000, false));
        let (lines, truncated) = text(preview_path(&f));
        assert_eq!((lines.len(), truncated), (MAX_LINES, true));
    }

    #[test]
    fn text_crlf_tabs_control() {
        let b = b"a\r\nb\tc\r\nd\x01e\x1b\n";
        let (lines, t) = text(classify(b, b.len() as u64));
        assert_eq!(lines, vec!["a", "b    c", "d·e·"]);
        assert!(!t);
    }

    #[test]
    fn korean_split_at_boundary_is_not_binary() {
        let mut b = vec![b'a'; MAX_BYTES - 1];
        b.extend("한".as_bytes()); // crosses MAX_BYTES
        b.extend(b"tail");
        let size = b.len() as u64;
        let cut = &b[..MAX_BYTES + 3];
        let (lines, t) = text(classify(cut, size));
        assert!(t);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with('한'));
    }

    #[test]
    fn split_char_when_only_max_bytes_given() {
        let mut b = vec![b'a'; MAX_BYTES - 1];
        b.extend("한".as_bytes());
        let size = b.len() as u64 + 10;
        let (lines, t) = text(classify(&b[..MAX_BYTES], size));
        assert!(t);
        assert_eq!(lines[0].len(), MAX_BYTES - 1);
    }

    #[test]
    fn nul_is_binary() {
        assert_eq!(
            classify(b"abc\0def", 7),
            Preview::Binary {
                size: 7,
                kind: "바이너리"
            }
        );
    }

    #[test]
    fn magic_kinds() {
        let k = |b: &[u8]| match classify(b, b.len() as u64) {
            Preview::Binary { kind, .. } => kind,
            o => panic!("{o:?}"),
        };
        assert_eq!(k(b"\x89PNG\r\n\x1a\n\0\0"), "PNG 이미지");
        assert_eq!(k(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]), "JPEG 이미지");
        assert_eq!(k(b"GIF89a\0\0"), "GIF 이미지");
        assert_eq!(k(b"%PDF-1.7\n%text"), "PDF 문서");
        assert_eq!(k(b"PK\x03\x04\0\0"), "ZIP 압축");
        assert_eq!(k(&[0x1f, 0x8b, 8, 0]), "GZIP 압축");
        assert_eq!(k(b"\x7fELF\x02\x01"), "ELF 실행파일");
        assert_eq!(k(&[0xCF, 0xFA, 0xED, 0xFE, 7, 0]), "Mach-O 실행파일");
        assert_eq!(k(b"\0\0\0\0"), "바이너리");
    }

    #[test]
    fn lossy_threshold() {
        // 1 invalid byte in 1000 -> text
        let mut ok = vec![b'a'; 1000];
        ok[500] = 0xFF;
        let (lines, _) = text(classify(&ok, 1000));
        assert!(lines[0].contains('\u{FFFD}'));
        // 10 invalid bytes in 100 -> binary
        let mut bad = vec![b'a'; 100];
        for i in 0..10 {
            bad[i * 10] = 0xFF;
        }
        assert!(matches!(classify(&bad, 100), Preview::Binary { .. }));
    }

    #[test]
    fn many_lines_truncated() {
        let s: String = (0..300).map(|i| format!("l{i}\n")).collect();
        let (lines, t) = text(classify(s.as_bytes(), s.len() as u64));
        assert_eq!(lines.len(), MAX_LINES);
        assert!(t);
        assert_eq!(lines[0], "l0");
    }

    #[test]
    fn exactly_max_lines_not_truncated() {
        let s: String = (0..MAX_LINES).map(|i| format!("l{i}\n")).collect();
        let (lines, t) = text(classify(s.as_bytes(), s.len() as u64));
        assert_eq!(lines.len(), MAX_LINES);
        assert!(!t);
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(classify(b"", 0), Preview::Empty);
    }

    fn entries(files: usize, dirs: usize) -> impl Iterator<Item = io::Result<(bool, u64)>> {
        (0..files)
            .map(|_| Ok((false, 10)))
            .chain((0..dirs).map(|_| Ok((true, 4096))))
    }

    #[test]
    fn dir_summary_counts() {
        let mut v: Vec<io::Result<(bool, u64)>> = entries(3, 2).collect();
        v.push(Err(io::Error::other("x")));
        assert_eq!(
            summarize_dir(v.into_iter()),
            Preview::Dir {
                files: 3,
                dirs: 2,
                total_size: 30,
                truncated: false,
                errors: 1
            }
        );
    }

    #[test]
    fn dir_summary_truncates() {
        assert_eq!(
            summarize_dir(entries(10_001, 0)),
            Preview::Dir {
                files: 10_000,
                dirs: 0,
                total_size: 100_000,
                truncated: true,
                errors: 0
            }
        );
        assert!(matches!(
            summarize_dir(entries(10_000, 0)),
            Preview::Dir {
                truncated: false,
                ..
            }
        ));
    }

    #[test]
    fn path_text_empty_and_dir() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("한 글 'q'.txt");
        fs::write(&f, "hi\n").unwrap();
        assert_eq!(
            preview_path(&f),
            Preview::Text {
                lines: vec!["hi".into()],
                truncated: false
            }
        );
        let e = d.path().join("empty");
        fs::write(&e, "").unwrap();
        assert_eq!(preview_path(&e), Preview::Empty);
        assert!(matches!(
            preview_path(d.path()),
            Preview::Dir {
                files: 2,
                dirs: 0,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn path_symlink_and_broken_symlink() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("한 글 'q'.txt");
        fs::write(&f, "hi\n").unwrap();
        let e = d.path().join("empty");
        fs::write(&e, "").unwrap();
        let ln = d.path().join("ln");
        std::os::unix::fs::symlink(&f, &ln).unwrap();
        assert!(matches!(preview_path(&ln), Preview::Text { .. }));
        let bad = d.path().join("bad");
        std::os::unix::fs::symlink(d.path().join("nope"), &bad).unwrap();
        assert_eq!(preview_path(&bad), Preview::Error("깨진 링크".into()));
        assert!(matches!(
            preview_path(d.path()),
            Preview::Dir {
                files: 4,
                dirs: 0,
                ..
            }
        ));
    }

    #[test]
    fn big_file_truncated_via_path() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("big");
        fs::write(&f, "x".repeat(MAX_BYTES * 2)).unwrap();
        let (lines, t) = text(preview_path(&f));
        assert!(t);
        assert_eq!(lines[0].len(), MAX_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_is_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("secret");
        fs::write(&f, "x").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&f).is_ok() {
            return; // running as root
        }
        assert!(matches!(preview_path(&f), Preview::Error(_)));
    }

    #[cfg(unix)]
    fn mkfifo(p: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_special_and_never_opened() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("fifo");
        mkfifo(&f);
        // would block forever if opened (no writer)
        assert_eq!(
            preview_path(&f),
            Preview::Binary {
                size: 0,
                kind: "특수 파일"
            }
        );
        let ln = d.path().join("ln");
        std::os::unix::fs::symlink(&f, &ln).unwrap();
        assert!(matches!(
            preview_path(&ln),
            Preview::Binary {
                kind: "특수 파일",
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn char_device_is_special() {
        assert!(matches!(
            preview_path(Path::new("/dev/null")),
            Preview::Binary {
                kind: "특수 파일",
                ..
            }
        ));
    }

    #[test]
    fn dir_summary_error_count_when_truncated() {
        let it = (0..10_001).map(|i| {
            if i % 2 == 0 {
                Err(io::Error::other("x"))
            } else {
                Ok((false, 1))
            }
        });
        assert!(matches!(
            summarize_dir(it),
            Preview::Dir {
                truncated: true,
                errors: 5000,
                ..
            }
        ));
    }
}
