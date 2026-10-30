//! Background copy/move worker so the UI thread never blocks.

use crate::fs_ops::{self, ConflictAction, OpControl, OpOutcome, OpReport};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
}

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub kind: JobKind,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Started {
        totals: fs_ops::Totals,
    },
    Progress {
        files_done: u64,
        bytes_done: u64,
        current: PathBuf,
    },
    /// Worker is blocked until [`JobHandle::answer`] (or cancel).
    Conflict {
        dest: PathBuf,
    },
    /// The report's `outcome` says how the job ended.
    Finished {
        report: OpReport,
    },
}

pub type JobOutcome = OpOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAnswer {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
    Cancel,
}

/// Remembers "all" answers for the rest of a job.
#[derive(Debug, Default)]
pub struct ConflictPolicy {
    remembered: Option<ConflictAction>,
}

impl ConflictPolicy {
    pub fn decide(&mut self, answer: ConflictAnswer) -> ConflictAction {
        match answer {
            ConflictAnswer::Overwrite => ConflictAction::Overwrite,
            ConflictAnswer::Skip => ConflictAction::Skip,
            ConflictAnswer::Cancel => ConflictAction::Cancel,
            ConflictAnswer::OverwriteAll => {
                self.remembered = Some(ConflictAction::Overwrite);
                ConflictAction::Overwrite
            }
            ConflictAnswer::SkipAll => {
                self.remembered = Some(ConflictAction::Skip);
                ConflictAction::Skip
            }
        }
    }

    pub fn remembered(&self) -> Option<ConflictAction> {
        self.remembered
    }
}

struct Worker {
    events: Sender<JobEvent>,
    answers: Receiver<ConflictAnswer>,
    cancel: Arc<AtomicBool>,
    policy: ConflictPolicy,
    files_done: u64,
    bytes_done: u64,
    /// Move jobs report progress per top-level item instead of per file.
    by_items: bool,
    current: PathBuf,
    last_sent: Instant,
    /// Test hook: invoked after every copied chunk, on the worker thread.
    on_chunk: Option<ChunkHook>,
}

/// Callback run on the worker thread after each copied chunk (used to make tests deterministic).
pub type ChunkHook = Arc<dyn Fn(&Path) + Send + Sync>;

impl Worker {
    /// Send at most one progress event per [`PROGRESS_INTERVAL`]; `run` always sends the final state.
    fn send_progress_throttled(&mut self) {
        if self.last_sent.elapsed() >= PROGRESS_INTERVAL {
            self.send_progress();
        }
    }

    fn send_progress(&mut self) {
        self.last_sent = Instant::now();
        let _ = self.events.send(JobEvent::Progress {
            files_done: self.files_done,
            bytes_done: self.bytes_done,
            current: self.current.clone(),
        });
    }
}

impl OpControl for Worker {
    fn on_progress(&mut self, delta: u64, current: &Path) {
        if let Some(hook) = &self.on_chunk {
            hook(current);
        }
        let first = self.bytes_done == 0;
        self.bytes_done += delta;
        if self.current != current {
            self.current = current.to_path_buf();
        }
        if first {
            self.send_progress();
        } else {
            self.send_progress_throttled();
        }
    }

    fn on_file_done(&mut self, path: &Path) {
        if !self.by_items {
            self.files_done += 1;
        }
        self.current = path.to_path_buf();
        self.send_progress_throttled();
    }

    fn on_item_done(&mut self, src: &Path) {
        if self.by_items {
            self.files_done += 1;
            self.current = src.to_path_buf();
            self.send_progress_throttled();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn on_conflict(&mut self, dest: &Path) -> ConflictAction {
        if let Some(a) = self.policy.remembered() {
            return a;
        }
        let _ = self.events.send(JobEvent::Conflict {
            dest: dest.to_path_buf(),
        });
        loop {
            match self.answers.recv_timeout(POLL) {
                Ok(a) => return self.policy.decide(a),
                Err(RecvTimeoutError::Timeout) => {
                    if self.is_cancelled() {
                        return ConflictAction::Cancel;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return ConflictAction::Cancel,
            }
        }
    }
}

fn run(spec: &JobSpec, w: &mut Worker) -> OpReport {
    let totals = match spec.kind {
        // Moves are renames: progress by top-level item, no deep walk.
        JobKind::Move => fs_ops::Totals {
            files: spec.sources.len() as u64,
            bytes: 0,
        },
        JobKind::Copy => {
            let cancel = Arc::clone(&w.cancel);
            let stop = move || cancel.load(Ordering::Relaxed);
            match fs_ops::measure_cancellable(&spec.sources, &stop) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    return OpReport {
                        outcome: OpOutcome::Cancelled,
                        ..OpReport::failed("")
                    };
                }
                Err(e) => return OpReport::failed(e.to_string()),
            }
        }
    };
    let _ = w.events.send(JobEvent::Started { totals });
    let report = match spec.kind {
        JobKind::Copy => fs_ops::copy_items(&spec.sources, &spec.dest_dir, w),
        JobKind::Move => fs_ops::move_items(&spec.sources, &spec.dest_dir, w),
    };
    match report.outcome {
        OpOutcome::Done => {
            // Renames report no per-byte progress; show completion.
            w.files_done = w.files_done.max(totals.files);
            w.bytes_done = w.bytes_done.max(totals.bytes);
            w.send_progress();
        }
        OpOutcome::Cancelled | OpOutcome::Failed(_) => w.send_progress(),
    }
    report
}

pub struct JobHandle {
    events: Receiver<JobEvent>,
    answers: Sender<ConflictAnswer>,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl JobHandle {
    pub fn spawn(spec: JobSpec) -> JobHandle {
        Self::spawn_with(spec, None)
    }

    /// Like [`spawn`](Self::spawn) with an optional per-chunk hook (for deterministic tests).
    pub fn spawn_with(spec: JobSpec, on_chunk: Option<ChunkHook>) -> JobHandle {
        let (etx, erx) = mpsc::channel();
        let (atx, arx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut worker = Worker {
            events: etx.clone(),
            answers: arx,
            cancel: Arc::clone(&cancel),
            policy: ConflictPolicy::default(),
            files_done: 0,
            bytes_done: 0,
            by_items: spec.kind == JobKind::Move,
            current: PathBuf::new(),
            last_sent: Instant::now(),
            on_chunk,
        };
        let thread = std::thread::spawn(move || {
            let report = catch_unwind(AssertUnwindSafe(|| run(&spec, &mut worker)))
                .unwrap_or_else(|_| OpReport::failed("worker panicked"));
            let _ = etx.send(JobEvent::Finished { report });
        });
        JobHandle {
            events: erx,
            answers: atx,
            cancel,
            thread,
        }
    }

    /// Non-blocking drain of pending events.
    pub fn try_events(&self) -> Vec<JobEvent> {
        self.events.try_iter().collect()
    }

    pub fn answer(&self, a: ConflictAnswer) {
        let _ = self.answers.send(a);
    }

    /// Sets the cancel flag; a pending conflict is released as Cancel.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.answers.send(ConflictAnswer::Cancel);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

#[derive(Debug, Clone)]
pub struct ProgressView {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: Option<PathBuf>,
    pub kind: JobKind,
}

impl ProgressView {
    pub fn new(kind: JobKind) -> Self {
        Self {
            files_done: 0,
            files_total: 0,
            bytes_done: 0,
            bytes_total: 0,
            current: None,
            kind,
        }
    }

    pub fn apply(&mut self, ev: &JobEvent) {
        match ev {
            JobEvent::Started { totals } => {
                self.files_total = totals.files;
                self.bytes_total = totals.bytes;
            }
            JobEvent::Progress {
                files_done,
                bytes_done,
                current,
            } => {
                self.files_done = *files_done;
                self.bytes_done = *bytes_done;
                self.current = Some(current.clone());
            }
            JobEvent::Conflict { .. } | JobEvent::Finished { .. } => {}
        }
    }

    /// Byte-based when there are bytes, otherwise file-count based; clamped to 100.
    pub fn percent(&self) -> u8 {
        let (done, total) = if self.bytes_total > 0 {
            (self.bytes_done, self.bytes_total)
        } else {
            (self.files_done, self.files_total)
        };
        if total == 0 {
            return 0;
        }
        (u128::from(done) * 100 / u128::from(total)).min(100) as u8
    }

    pub fn status_line(&self) -> String {
        let verb = match self.kind {
            JobKind::Copy => "복사 중",
            JobKind::Move => "이동 중",
        };
        let mut s = format!(
            "{verb} {}/{} · {}%",
            self.files_done,
            self.files_total,
            self.percent()
        );
        if let Some(name) = self
            .current
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy())
        {
            s.push_str(" · ");
            s.push_str(&name);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn wait_events(h: &JobHandle, stop: impl Fn(&[JobEvent]) -> bool) -> Vec<JobEvent> {
        let mut all = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            all.extend(h.try_events());
            if stop(&all) {
                return all;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("timeout; events so far: {all:?}");
    }

    fn finished(ev: &[JobEvent]) -> bool {
        ev.iter().any(|e| matches!(e, JobEvent::Finished { .. }))
    }

    fn outcome(ev: &[JobEvent]) -> JobOutcome {
        ev.iter()
            .find_map(|e| match e {
                JobEvent::Finished { report } => Some(report.outcome.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn count_conflicts(ev: &[JobEvent]) -> usize {
        ev.iter()
            .filter(|e| matches!(e, JobEvent::Conflict { .. }))
            .count()
    }

    /// Run job; answer every Conflict with `ans`; return all events.
    fn run_answering(spec: JobSpec, ans: ConflictAnswer) -> Vec<JobEvent> {
        let h = JobHandle::spawn(spec);
        let mut all = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            for e in h.try_events() {
                if matches!(e, JobEvent::Conflict { .. }) {
                    h.answer(ans);
                }
                all.push(e);
            }
            if finished(&all) {
                return all;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timeout: {all:?}");
    }

    fn spec(kind: JobKind, sources: Vec<PathBuf>, dest: &Path) -> JobSpec {
        JobSpec {
            kind,
            sources,
            dest_dir: dest.to_path_buf(),
        }
    }

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (s, d) = (t.path().join("s"), t.path().join("d"));
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        (t, s, d)
    }

    #[test]
    fn copy_job_end_to_end() {
        let (_t, s, d) = dirs();
        fs::write(s.join("a.txt"), "aaa").unwrap();
        fs::write(s.join("b.txt"), "bb").unwrap();
        let h = JobHandle::spawn(spec(
            JobKind::Copy,
            vec![s.join("a.txt"), s.join("b.txt")],
            &d,
        ));
        let ev = wait_events(&h, finished);
        assert!(
            matches!(ev[0], JobEvent::Started { totals } if totals.files == 2 && totals.bytes == 5)
        );
        assert!(ev.iter().any(|e| matches!(e, JobEvent::Progress { .. })));
        assert_eq!(outcome(&ev), JobOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "aaa");
        assert!(s.join("a.txt").exists());
        let start = Instant::now();
        while !h.is_finished() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.is_finished());
    }

    #[test]
    fn move_job_end_to_end() {
        let (_t, s, d) = dirs();
        fs::write(s.join("a.txt"), "aaa").unwrap();
        let ev = run_answering(
            spec(JobKind::Move, vec![s.join("a.txt")], &d),
            ConflictAnswer::Skip,
        );
        assert_eq!(outcome(&ev), JobOutcome::Done);
        assert!(!s.join("a.txt").exists());
        assert_eq!(fs::read_to_string(d.join("a.txt")).unwrap(), "aaa");
    }

    fn conflict_setup(n: usize) -> (tempfile::TempDir, Vec<PathBuf>, PathBuf) {
        let (t, s, d) = dirs();
        let mut srcs = Vec::new();
        for i in 0..n {
            let name = format!("f{i}.txt");
            fs::write(s.join(&name), "new").unwrap();
            fs::write(d.join(&name), "old").unwrap();
            srcs.push(s.join(&name));
        }
        (t, srcs, d)
    }

    #[test]
    fn conflict_skip_and_overwrite() {
        let (_t, srcs, d) = conflict_setup(1);
        let ev = run_answering(spec(JobKind::Copy, srcs.clone(), &d), ConflictAnswer::Skip);
        assert_eq!(count_conflicts(&ev), 1);
        assert_eq!(outcome(&ev), JobOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("f0.txt")).unwrap(), "old");
        let ev = run_answering(spec(JobKind::Copy, srcs, &d), ConflictAnswer::Overwrite);
        assert_eq!(outcome(&ev), JobOutcome::Done);
        assert_eq!(fs::read_to_string(d.join("f0.txt")).unwrap(), "new");
    }

    #[test]
    fn overwrite_all_asked_once() {
        let (_t, srcs, d) = conflict_setup(3);
        let ev = run_answering(spec(JobKind::Copy, srcs, &d), ConflictAnswer::OverwriteAll);
        assert_eq!(count_conflicts(&ev), 1);
        assert_eq!(outcome(&ev), JobOutcome::Done);
        for i in 0..3 {
            assert_eq!(
                fs::read_to_string(d.join(format!("f{i}.txt"))).unwrap(),
                "new"
            );
        }
    }

    #[test]
    fn skip_all_asked_once() {
        let (_t, srcs, d) = conflict_setup(3);
        let ev = run_answering(spec(JobKind::Copy, srcs, &d), ConflictAnswer::SkipAll);
        assert_eq!(count_conflicts(&ev), 1);
        for i in 0..3 {
            assert_eq!(
                fs::read_to_string(d.join(format!("f{i}.txt"))).unwrap(),
                "old"
            );
        }
    }

    #[test]
    fn cancel_mid_copy_is_deterministic() {
        use std::sync::Mutex;
        use std::sync::atomic::AtomicUsize;
        let (_t, s, d) = dirs();
        fs::write(s.join("big.bin"), vec![7u8; 4 << 20]).unwrap();
        let (reached_tx, reached_rx) = mpsc::channel::<()>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = Mutex::new(gate_rx);
        let calls = AtomicUsize::new(0);
        let hook: ChunkHook = Arc::new(move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                reached_tx.send(()).unwrap();
                let _ = gate_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        });
        let h = JobHandle::spawn_with(spec(JobKind::Copy, vec![s.join("big.bin")], &d), Some(hook));
        // Worker is provably inside the copy (first 1 MiB chunk written, 3 MiB left).
        reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        h.cancel();
        gate_tx.send(()).unwrap();
        let ev = wait_events(&h, finished);
        assert_eq!(outcome(&ev), JobOutcome::Cancelled);
        assert!(!d.join("big.bin").exists());
        assert!(s.join("big.bin").exists());
        let max_done = ev
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { bytes_done, .. } => Some(*bytes_done),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        assert!(max_done < 4 << 20, "copied everything: {max_done}");
    }

    #[test]
    fn cancel_while_waiting_on_conflict() {
        let (_t, srcs, d) = conflict_setup(2);
        let h = JobHandle::spawn(spec(JobKind::Copy, srcs, &d));
        let mut ev = wait_events(&h, |e| count_conflicts(e) > 0);
        h.cancel();
        while !finished(&ev) {
            ev.extend(wait_events(&h, |_| true));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(outcome(&ev), JobOutcome::Cancelled);
        assert_eq!(fs::read_to_string(d.join("f0.txt")).unwrap(), "old");
    }

    #[test]
    fn missing_source_fails() {
        let (t, _s, d) = dirs();
        let h = JobHandle::spawn(spec(JobKind::Copy, vec![t.path().join("nope")], &d));
        let ev = wait_events(&h, finished);
        assert!(matches!(outcome(&ev), JobOutcome::Failed(m) if !m.is_empty()));
    }

    #[test]
    fn policy_basics() {
        let mut p = ConflictPolicy::default();
        assert_eq!(p.remembered(), None);
        assert_eq!(
            p.decide(ConflictAnswer::Overwrite),
            ConflictAction::Overwrite
        );
        assert_eq!(p.remembered(), None);
        assert_eq!(p.decide(ConflictAnswer::Skip), ConflictAction::Skip);
        assert_eq!(p.remembered(), None);
        assert_eq!(p.decide(ConflictAnswer::Cancel), ConflictAction::Cancel);
        assert_eq!(p.remembered(), None);
    }

    #[test]
    fn policy_remembers_all() {
        let mut p = ConflictPolicy::default();
        assert_eq!(
            p.decide(ConflictAnswer::OverwriteAll),
            ConflictAction::Overwrite
        );
        assert_eq!(p.remembered(), Some(ConflictAction::Overwrite));
        let mut p = ConflictPolicy::default();
        assert_eq!(p.decide(ConflictAnswer::SkipAll), ConflictAction::Skip);
        assert_eq!(p.remembered(), Some(ConflictAction::Skip));
    }

    #[test]
    fn progress_view_apply_and_percent() {
        let mut v = ProgressView::new(JobKind::Copy);
        assert_eq!(v.percent(), 0);
        v.apply(&JobEvent::Started {
            totals: fs_ops::Totals {
                files: 12,
                bytes: 200,
            },
        });
        assert_eq!((v.files_total, v.bytes_total), (12, 200));
        v.apply(&JobEvent::Progress {
            files_done: 3,
            bytes_done: 90,
            current: PathBuf::from("/x/name.txt"),
        });
        assert_eq!(v.percent(), 45);
        assert_eq!(v.status_line(), "복사 중 3/12 · 45% · name.txt");
        v.apply(&JobEvent::Progress {
            files_done: 12,
            bytes_done: 999,
            current: PathBuf::from("/x/z"),
        });
        assert_eq!(v.percent(), 100);
    }

    #[test]
    fn progress_view_zero_total_and_move() {
        let mut v = ProgressView::new(JobKind::Move);
        v.apply(&JobEvent::Started {
            totals: fs_ops::Totals { files: 2, bytes: 0 },
        });
        assert_eq!(v.percent(), 0);
        v.apply(&JobEvent::Progress {
            files_done: 2,
            bytes_done: 0,
            current: PathBuf::from("q"),
        });
        assert_eq!(v.percent(), 100);
        assert!(v.status_line().starts_with("이동 중 2/2 · 100%"));
    }

    #[test]
    fn move_job_skips_deep_measure() {
        let (_t, s, d) = dirs();
        fs::create_dir_all(s.join("t/u")).unwrap();
        fs::write(s.join("t/u/f"), "abc").unwrap();
        fs::write(s.join("g"), "g").unwrap();
        let h = JobHandle::spawn(spec(JobKind::Move, vec![s.join("t"), s.join("g")], &d));
        let ev = wait_events(&h, finished);
        assert!(
            matches!(ev[0], JobEvent::Started { totals } if totals.files == 2 && totals.bytes == 0),
            "{:?}",
            ev[0]
        );
        assert_eq!(outcome(&ev), JobOutcome::Done);
    }

    #[test]
    fn finished_event_carries_report() {
        let (_t, s, d) = dirs();
        fs::write(s.join("a"), "A").unwrap();
        let h = JobHandle::spawn(spec(JobKind::Move, vec![s.join("a")], &d));
        let ev = wait_events(&h, finished);
        let report = ev
            .iter()
            .find_map(|e| match e {
                JobEvent::Finished { report } => Some(report.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(report.moved, vec![(s.join("a"), d.join("a"))]);
    }

    #[test]
    fn many_tiny_files_coalesce_progress_events() {
        let (_t, s, d) = dirs();
        let tree = s.join("tree");
        fs::create_dir(&tree).unwrap();
        for i in 0..500 {
            fs::write(tree.join(format!("f{i}")), "x").unwrap();
        }
        let ev = run_answering(spec(JobKind::Copy, vec![tree], &d), ConflictAnswer::Cancel);
        assert_eq!(outcome(&ev), JobOutcome::Done);
        let progress: Vec<u64> = ev
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { files_done, .. } => Some(*files_done),
                _ => None,
            })
            .collect();
        assert!(progress.len() < 100, "{} progress events", progress.len());
        assert_eq!(progress.last(), Some(&500));
    }
}
