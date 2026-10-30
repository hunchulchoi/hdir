//! Background directory loading: keeps blocking `read_dir` + metadata off the UI thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::panel::{Entry, read_listing};

/// Listing plus the number of items skipped due to errors, or an error message.
pub type Listing = Result<(Vec<Entry>, usize), String>;

/// Reads one directory; replaceable so tests can slow it down deterministically.
pub type Reader = Arc<dyn Fn(&Path, bool) -> Listing + Send + Sync>;

#[derive(Debug)]
pub struct LoadResult {
    pub id: u64,
    pub dir: PathBuf,
    pub result: Listing,
}

pub struct Loader {
    reader: Reader,
    tx: Sender<LoadResult>,
    rx: Mutex<Receiver<LoadResult>>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::with_reader(Arc::new(read_listing))
    }
}

impl Loader {
    pub fn with_reader(reader: Reader) -> Self {
        let (tx, rx) = channel();
        Self {
            reader,
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// Start loading `dir` on its own thread (so one hung mount cannot delay
    /// other requests). The result is tagged with `id`.
    pub fn request(&self, id: u64, dir: PathBuf, show_hidden: bool) {
        let reader = Arc::clone(&self.reader);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = reader(&dir, show_hidden);
            let _ = tx.send(LoadResult { id, dir, result });
        });
    }

    /// Collect all finished results without blocking.
    pub fn try_results(&self) -> Vec<LoadResult> {
        let Ok(rx) = self.rx.lock() else {
            return Vec::new();
        };
        rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(loader: &Loader, n: usize) -> Vec<LoadResult> {
        let start = Instant::now();
        let mut out = Vec::new();
        while out.len() < n && start.elapsed() < Duration::from_secs(5) {
            out.extend(loader.try_results());
            std::thread::sleep(Duration::from_millis(2));
        }
        out
    }

    #[test]
    fn result_arrives_for_tempdir() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("한글 file.txt"), "x").unwrap();
        let loader = Loader::default();
        loader.request(7, t.path().to_path_buf(), false);
        let got = wait_for(&loader, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 7);
        assert_eq!(got[0].dir, t.path());
        let (entries, skipped) = got[0].result.as_ref().unwrap();
        assert_eq!(*skipped, 0);
        assert!(entries.iter().any(|e| e.name == "한글 file.txt"));
    }

    #[test]
    fn missing_dir_yields_error() {
        let t = tempfile::tempdir().unwrap();
        let loader = Loader::default();
        loader.request(1, t.path().join("nope"), false);
        let got = wait_for(&loader, 1);
        assert!(got[0].result.is_err());
    }

    #[test]
    fn results_keep_their_ids() {
        let t = tempfile::tempdir().unwrap();
        let loader = Loader::default();
        loader.request(1, t.path().to_path_buf(), false);
        loader.request(2, t.path().to_path_buf(), true);
        let mut ids: Vec<u64> = wait_for(&loader, 2).iter().map(|r| r.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }
}
