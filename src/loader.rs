//! Background directory loading: keeps blocking `read_dir` + metadata off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::panel::Entry;
use crate::vfs::{LocalVfs, Vfs};

/// Global cap on concurrent reader threads.
pub const MAX_READERS: usize = 4;

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

struct Req {
    id: u64,
    dir: PathBuf,
    hidden: bool,
    /// Where to read; `None` = the loader's default reader (local).
    vfs: Option<Arc<dyn Vfs>>,
}

#[derive(Default)]
struct Slot {
    in_flight: bool,
    /// Latest request waiting for the in-flight read; older ones are dropped.
    pending: Option<Req>,
}

#[derive(Default)]
struct State {
    slots: HashMap<usize, Slot>,
    running: usize,
}

struct Inner {
    reader: Reader,
    tx: Sender<LoadResult>,
    state: Mutex<State>,
}

/// Reads directories off the UI thread: per key (panel) at most one read is in
/// flight plus one pending (latest wins), and at most [`MAX_READERS`] overall.
pub struct Loader {
    inner: Arc<Inner>,
    rx: Mutex<Receiver<LoadResult>>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::with_reader(Arc::new(|d, h| LocalVfs.list(d, h)))
    }
}

impl Loader {
    pub fn with_reader(reader: Reader) -> Self {
        let (tx, rx) = channel();
        Self {
            inner: Arc::new(Inner {
                reader,
                tx,
                state: Mutex::new(State::default()),
            }),
            rx: Mutex::new(rx),
        }
    }

    /// Queue loading `dir` for panel `key`; the result is tagged with `id`. A
    /// pending request of the same key that has not started yet is replaced.
    pub fn request(&self, key: usize, id: u64, dir: PathBuf, show_hidden: bool) {
        self.request_vfs(key, id, dir, show_hidden, None);
    }

    /// Like [`Loader::request`], but reads through `vfs` when given.
    pub fn request_vfs(
        &self,
        key: usize,
        id: u64,
        dir: PathBuf,
        show_hidden: bool,
        vfs: Option<Arc<dyn Vfs>>,
    ) {
        if let Ok(mut st) = self.inner.state.lock() {
            st.slots.entry(key).or_default().pending = Some(Req {
                id,
                dir,
                hidden: show_hidden,
                vfs,
            });
        }
        pump(&self.inner);
    }

    /// Collect all finished results without blocking.
    pub fn try_results(&self) -> Vec<LoadResult> {
        let Ok(rx) = self.rx.lock() else {
            return Vec::new();
        };
        rx.try_iter().collect()
    }
}

/// Start pending requests while reader slots are free.
fn pump(inner: &Arc<Inner>) {
    let mut to_run = Vec::new();
    if let Ok(mut st) = inner.state.lock() {
        let keys: Vec<usize> = st.slots.keys().copied().collect();
        for key in keys {
            if st.running >= MAX_READERS {
                break;
            }
            let slot = st.slots.get_mut(&key).expect("key just listed");
            if slot.in_flight {
                continue;
            }
            if let Some(req) = slot.pending.take() {
                slot.in_flight = true;
                st.running += 1;
                to_run.push((key, req));
            }
        }
    }
    for (key, req) in to_run {
        let inner = Arc::clone(inner);
        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &req.vfs {
                    Some(vfs) => vfs.list(&req.dir, req.hidden),
                    None => (inner.reader)(&req.dir, req.hidden),
                }))
                .unwrap_or_else(|_| Err("디렉터리 읽기 실패".to_string()));
            let _ = inner.tx.send(LoadResult {
                id: req.id,
                dir: req.dir,
                result,
            });
            if let Ok(mut st) = inner.state.lock() {
                st.running = st.running.saturating_sub(1);
                if let Some(slot) = st.slots.get_mut(&key) {
                    slot.in_flight = false;
                }
            }
            pump(&inner);
        });
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
        loader.request(0, 7, t.path().to_path_buf(), false);
        let got = wait_for(&loader, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, 7);
        assert_eq!(got[0].dir, t.path());
        let (entries, skipped) = got[0].result.as_ref().unwrap();
        assert_eq!(*skipped, 0);
        assert!(entries.iter().any(|e| e.name == "한글 file.txt"));
    }

    struct FakeVfs;
    impl Vfs for FakeVfs {
        fn list(&self, dir: &Path, hidden: bool) -> Listing {
            Err(format!("fake:{}:{hidden}", dir.display()))
        }
        fn display_path(&self, dir: &Path) -> String {
            dir.display().to_string()
        }
    }

    #[test]
    fn request_vfs_reads_through_the_given_vfs() {
        let loader = Loader::default();
        loader.request_vfs(0, 7, PathBuf::from("/x"), true, Some(Arc::new(FakeVfs)));
        let got = wait_for(&loader, 1);
        assert_eq!(got[0].id, 7);
        assert_eq!(got[0].result.as_ref().unwrap_err(), "fake:/x:true");
    }

    #[test]
    fn missing_dir_yields_error() {
        let t = tempfile::tempdir().unwrap();
        let loader = Loader::default();
        loader.request(0, 1, t.path().join("nope"), false);
        let got = wait_for(&loader, 1);
        assert!(got[0].result.is_err());
    }

    #[test]
    fn results_keep_their_ids() {
        let t = tempfile::tempdir().unwrap();
        let loader = Loader::default();
        loader.request(0, 1, t.path().to_path_buf(), false);
        loader.request(1, 2, t.path().to_path_buf(), true);
        let mut ids: Vec<u64> = wait_for(&loader, 2).iter().map(|r| r.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Reader that counts invocations, reports each entry on `entered`, tracks the
    /// peak number of simultaneous reads, and blocks until the gate is released.
    struct Probe {
        count: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        entered: std::sync::mpsc::Receiver<()>,
    }

    fn gated(gate: Arc<Mutex<()>>) -> (Loader, Probe) {
        let (count, peak, live) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        let (tx, entered) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let probe = Probe {
            count: Arc::clone(&count),
            peak: Arc::clone(&peak),
            entered,
        };
        let loader = Loader::with_reader(Arc::new(move |dir, hidden| {
            count.fetch_add(1, Ordering::SeqCst);
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            let _ = tx.lock().unwrap().send(());
            drop(gate.lock().unwrap());
            live.fetch_sub(1, Ordering::SeqCst);
            crate::panel::read_listing(dir, hidden)
        }));
        (loader, probe)
    }

    impl Probe {
        /// Block until `n` more readers have entered (5s timeout).
        fn wait_entered(&self, n: usize) {
            for _ in 0..n {
                self.entered
                    .recv_timeout(Duration::from_secs(5))
                    .expect("reader never entered");
            }
        }
    }

    #[test]
    fn rapid_requests_for_one_panel_run_at_most_one_plus_latest() {
        let t = tempfile::tempdir().unwrap();
        let gate = Arc::new(Mutex::new(()));
        let (loader, probe) = gated(Arc::clone(&gate));
        let guard = gate.lock().unwrap();
        loader.request(0, 1, t.path().to_path_buf(), false);
        probe.wait_entered(1); // the first read is in flight and blocked
        for id in 2..=10 {
            loader.request(0, id, t.path().to_path_buf(), false);
        }
        assert_eq!(
            probe.count.load(Ordering::SeqCst),
            1,
            "one in flight, rest pending"
        );
        drop(guard);
        let mut ids = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            ids.extend(loader.try_results().iter().map(|r| r.id));
            if ids.contains(&10) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(ids.last(), Some(&10), "latest request wins: {ids:?}");
        assert_eq!(
            probe.count.load(Ordering::SeqCst),
            2,
            "superseded requests never ran"
        );
    }

    #[test]
    fn global_reader_thread_cap() {
        let t = tempfile::tempdir().unwrap();
        let gate = Arc::new(Mutex::new(()));
        let (loader, probe) = gated(Arc::clone(&gate));
        let guard = gate.lock().unwrap();
        for key in 0..8 {
            loader.request(key, key as u64 + 1, t.path().to_path_buf(), false);
        }
        probe.wait_entered(MAX_READERS); // all four slots are provably busy
        assert_eq!(probe.peak.load(Ordering::SeqCst), MAX_READERS);
        drop(guard);
        assert_eq!(wait_for(&loader, 8).len(), 8, "queued panels still load");
        assert_eq!(
            probe.peak.load(Ordering::SeqCst),
            MAX_READERS,
            "never above the cap"
        );
    }
}
