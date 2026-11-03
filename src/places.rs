//! Favorite folders (kept in `favorites.toml`) and the folders visited in
//! this run (most recent first).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// How many visited folders the history keeps.
pub const HISTORY_CAP: usize = 50;

/// Most recent first, each folder once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    paths: VecDeque<PathBuf>,
}

impl History {
    /// Record a visit: `path` moves to the front.
    pub fn visit(&mut self, path: &Path) {
        self.paths.retain(|p| p != path);
        self.paths.push_front(path.to_path_buf());
        self.paths.truncate(HISTORY_CAP);
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.paths.iter().cloned().collect()
    }
}

/// Add `path` unless it is already there; `false` when it was.
pub fn add_favorite(list: &mut Vec<PathBuf>, path: &Path) -> bool {
    if list.iter().any(|p| p == path) {
        return false;
    }
    list.push(path.to_path_buf());
    true
}

pub fn favorites_file(dir: &Path) -> PathBuf {
    dir.join("favorites.toml")
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FavoritesFile {
    paths: Vec<String>,
}

/// The favorites in `text`; problems become warnings (an unreadable file
/// gives an empty list, never an error).
pub fn parse_favorites(text: &str) -> (Vec<PathBuf>, Vec<String>) {
    match toml::from_str::<FavoritesFile>(text) {
        Ok(f) => {
            let mut out = Vec::new();
            for p in f.paths {
                add_favorite(&mut out, Path::new(&p));
            }
            (out, Vec::new())
        }
        Err(e) => {
            let msg = e.to_string();
            let first = msg.lines().next().unwrap_or("parse error").to_string();
            (
                Vec::new(),
                vec![crate::tr!(
                    "favorites.toml 해석 실패: {first}",
                    "favorites.toml could not be read: {first}"
                )],
            )
        }
    }
}

/// The file text for `list`. A path that is not valid UTF-8 cannot be
/// written to TOML: an error names it.
pub fn favorites_toml(list: &[PathBuf]) -> Result<String, String> {
    let mut out = String::from("# hdir favorites (F11)\npaths = [\n");
    for p in list {
        let s = p.to_str().ok_or_else(|| {
            crate::tr!(
                "UTF-8이 아닌 경로는 저장할 수 없음: {}",
                "A path that is not UTF-8 cannot be saved: {}",
                p.to_string_lossy()
            )
        })?;
        out.push_str("  ");
        out.push_str(&toml::Value::String(s.to_string()).to_string());
        out.push_str(",\n");
    }
    out.push_str("]\n");
    Ok(out)
}

/// Read `<dir>/favorites.toml`. No file: an empty list, no warning.
pub fn load_favorites(dir: Option<&Path>) -> (Vec<PathBuf>, Vec<String>) {
    let Some(dir) = dir else {
        return (Vec::new(), Vec::new());
    };
    match std::fs::read_to_string(favorites_file(dir)) {
        Ok(t) => parse_favorites(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(e) => (
            Vec::new(),
            vec![crate::tr!(
                "favorites.toml 읽기 실패: {e}",
                "favorites.toml could not be read: {e}"
            )],
        ),
    }
}

/// Write the list to `<dir>/favorites.toml` (creating `dir`), replacing the
/// file atomically. Blocking: run off the UI thread.
pub fn save_favorites(dir: &Path, list: &[PathBuf]) -> Result<(), String> {
    let text = favorites_toml(list)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&favorites_file(dir), &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_latest_visit_once_and_is_capped() {
        let mut h = History::default();
        for p in ["/a", "/b", "/a", "/c"] {
            h.visit(Path::new(p));
        }
        assert_eq!(h.paths(), ["/c", "/a", "/b"].map(PathBuf::from));
        for i in 0..HISTORY_CAP + 10 {
            h.visit(&PathBuf::from(format!("/d{i}")));
        }
        assert_eq!(h.paths().len(), HISTORY_CAP);
        assert_eq!(
            h.paths()[0],
            PathBuf::from(format!("/d{}", HISTORY_CAP + 9))
        );
    }

    #[test]
    fn favorites_round_trip_with_korean_spaces_and_quotes() {
        let list: Vec<PathBuf> = ["/홈/내 문서", "/a \"q\" b", "/x\\y", "C:\\Users\\me"]
            .map(PathBuf::from)
            .to_vec();
        let text = favorites_toml(&list).unwrap();
        let (back, w) = parse_favorites(&text);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, list);
        assert_eq!(parse_favorites("").0, Vec::<PathBuf>::new());
    }

    #[test]
    fn duplicates_are_added_once_and_a_broken_file_warns() {
        let mut l = vec![PathBuf::from("/a")];
        assert!(!add_favorite(&mut l, Path::new("/a")));
        assert!(add_favorite(&mut l, Path::new("/b")));
        assert_eq!(l.len(), 2);
        let (got, _) = parse_favorites("paths = [\"/a\", \"/a\"]\n");
        assert_eq!(got.len(), 1);
        let (got, w) = parse_favorites("paths = [1, 2]\n");
        assert!(got.is_empty() && w.len() == 1, "{w:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_refused_by_name() {
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/x\xff"));
        assert!(favorites_toml(&[bad]).is_err());
    }

    #[test]
    fn save_and_load_through_the_config_dir() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("new-config");
        let list = vec![PathBuf::from("/srv/공유 폴더")];
        save_favorites(&dir, &list).unwrap();
        assert_eq!(load_favorites(Some(&dir)), (list, Vec::new()));
        assert_eq!(
            load_favorites(Some(&t.path().join("none"))),
            (Vec::new(), Vec::new())
        );
        assert_eq!(load_favorites(None), (Vec::new(), Vec::new()));
    }
}
