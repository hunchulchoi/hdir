//! Undo stack for reversible file operations (no terminal IO).

use std::fs;
use std::path::{Path, PathBuf};

use crate::trash::{OnConflict, TrashEntry, TrashStore};

const CAP: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOp {
    Mkdir {
        path: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// `(original, new)` pairs.
    Move {
        pairs: Vec<(PathBuf, PathBuf)>,
        /// Emptied source directories that were removed by a merge.
        removed_dirs: Vec<PathBuf>,
    },
    /// Items sent to the trash; undo restores them to their original paths.
    Trash {
        entries: Vec<TrashEntry>,
    },
}

#[derive(Debug)]
pub struct UndoStack {
    ops: Vec<UndoOp>,
    cap: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self {
            ops: Vec::new(),
            cap: CAP,
        }
    }
}

impl UndoStack {
    pub fn push(&mut self, op: UndoOp) {
        self.ops.push(op);
        if self.ops.len() > self.cap {
            self.ops.remove(0);
        }
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Remove and return the latest op without touching the filesystem.
    pub fn pop(&mut self) -> Option<UndoOp> {
        self.ops.pop()
    }

    /// Pop and reverse the latest op. The op is consumed, even on error, except
    /// that failed parts of a move are pushed back for a retry. Existing files
    /// are never overwritten.
    pub fn undo(&mut self) -> Result<String, String> {
        let op = self.pop().ok_or("되돌릴 작업 없음")?;
        let UndoApplied { message, retry } = Self::apply(op);
        if let Some(r) = retry {
            self.push(r);
        }
        message
    }

    /// Reverse `op` on the filesystem without touching any stack (usable from a
    /// worker thread). `retry` holds what failed and may be pushed back.
    pub fn apply(op: UndoOp) -> UndoApplied {
        match op {
            UndoOp::Trash { .. } => Self::apply_with_store(op, &*crate::trash::system_store()),
            op => apply_with(op, &|d| fs::create_dir_all(d), None),
        }
    }

    /// Like [`apply`](Self::apply) with an explicit trash backend.
    pub fn apply_with_store(op: UndoOp, store: &dyn TrashStore) -> UndoApplied {
        apply_with(op, &|d| fs::create_dir_all(d), Some(store))
    }
}

/// Outcome of [`UndoStack::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoApplied {
    pub message: Result<String, String>,
    /// Remaining work after a partial failure, to push back for a retry.
    pub retry: Option<UndoOp>,
}

fn apply_with(
    op: UndoOp,
    create_dir: &dyn Fn(&Path) -> std::io::Result<()>,
    store: Option<&dyn TrashStore>,
) -> UndoApplied {
    let done = |message| UndoApplied {
        message,
        retry: None,
    };
    match op {
        UndoOp::Mkdir { path } => done(
            fs::remove_dir(&path)
                .map(|()| format!("되돌림: 폴더 생성 취소 {}", base(&path)))
                .map_err(|e| format!("되돌릴 수 없음: {e}")),
        ),
        UndoOp::Rename { from, to } => done(
            restore(&to, &from)
                .map(|()| format!("되돌림: 이름 변경 {} → {}", base(&to), base(&from)))
                .map_err(Restore::into_msg),
        ),
        UndoOp::Move {
            pairs,
            removed_dirs,
        } => {
            let total = pairs.len() + removed_dirs.len();
            let mut errors = Vec::new();
            let mut failed_dirs = Vec::new();
            let mut failed_pairs = Vec::new();
            // Merged-away source directories first, so they exist again.
            for dir in removed_dirs {
                if let Err(e) = create_dir(&dir) {
                    errors.push(format!("되돌릴 수 없음: {e}"));
                    failed_dirs.push(dir);
                }
            }
            for (orig, new) in pairs {
                match restore(&new, &orig) {
                    Ok(()) => {}
                    Err(Restore::Failed(e)) => {
                        errors.push(e);
                        failed_pairs.push((orig, new));
                    }
                    // Restored but the old place is not clean: a retry would
                    // only say "already exists", so do not keep the pair.
                    Err(Restore::Dirty(e)) => errors.push(e),
                }
            }
            if errors.is_empty() {
                return done(Ok(format!("되돌림: 이동 {total}개")));
            }
            let ok = total.saturating_sub(errors.len());
            let msg = format!("되돌림 일부 실패 ({ok}/{total}): {}", errors[0]);
            // Keep only what failed so the user can retry.
            let retry =
                (!failed_pairs.is_empty() || !failed_dirs.is_empty()).then_some(UndoOp::Move {
                    pairs: failed_pairs,
                    removed_dirs: failed_dirs,
                });
            UndoApplied {
                message: Err(msg),
                retry,
            }
        }
        UndoOp::Trash { entries } => {
            let Some(store) = store else {
                return done(Err("휴지통을 사용할 수 없음".into()));
            };
            undo_trash(store, entries)
        }
    }
}

/// Restore trashed items to their original paths (never overwriting).
fn undo_trash(store: &dyn TrashStore, entries: Vec<TrashEntry>) -> UndoApplied {
    let jobs = crate::trash::build_jobs(&entries, None);
    let total = jobs.len();
    let report = crate::trash::restore_entries(store, &jobs, OnConflict::Report);
    let ok = report.restored.len();
    if ok == total {
        let mut msg = format!("되돌림: 휴지통 복원 {ok}개");
        if let Some(w) = report.warnings.first() {
            msg.push_str(&format!(" · {w}"));
        }
        return UndoApplied {
            message: Ok(msg),
            retry: None,
        };
    }
    let first = if let Some((e, dest)) = report.conflicts.first() {
        format!("{}: {} 이미 존재", e.name, dest.to_string_lossy())
    } else {
        report
            .failed
            .first()
            .map(|f| f.message.clone())
            .unwrap_or_default()
    };
    // Keep what a retry could still fix: conflicts and transient failures.
    let mut retry_entries: Vec<TrashEntry> = report.conflicts.into_iter().map(|(e, _)| e).collect();
    retry_entries.extend(
        report
            .failed
            .into_iter()
            .filter(|f| f.retry)
            .map(|f| f.entry),
    );
    UndoApplied {
        message: Err(format!("되돌림 일부 실패 ({ok}/{total}): {first}")),
        retry: (!retry_entries.is_empty()).then_some(UndoOp::Trash {
            entries: retry_entries,
        }),
    }
}

fn base(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

enum Restore {
    Failed(String),
    /// Copied back, but the old location could not be fully removed.
    Dirty(String),
}

impl Restore {
    fn into_msg(self) -> String {
        match self {
            Restore::Failed(m) | Restore::Dirty(m) => m,
        }
    }
}

/// Move `current` back to `target`, refusing to overwrite.
fn restore(current: &Path, target: &Path) -> Result<(), Restore> {
    let fail = |m: String| Restore::Failed(m);
    if target.symlink_metadata().is_ok() {
        return Err(fail(format!(
            "되돌릴 수 없음: {} 이미 존재",
            target.to_string_lossy()
        )));
    }
    // A merged-away source directory may be gone; recreate its parent chain.
    if let Some(parent) = target.parent().filter(|p| !p.exists()) {
        fs::create_dir_all(parent).map_err(|e| fail(format!("되돌릴 수 없음: {e}")))?;
    }
    // Falls back to copy + remove across devices; never overwrites.
    crate::fs_ops::move_path_no_overwrite(current, target).map_err(|e| {
        match e
            .get_ref()
            .and_then(|x| x.downcast_ref::<crate::fs_ops::CleanupFailed>())
        {
            Some(c) => Restore::Dirty(format!(
                "되돌림: 복사는 됐지만 원래 위치 정리 실패: {}",
                c.0.to_string_lossy()
            )),
            None => fail(format!("되돌릴 수 없음: {e}")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash::FakeTrashStore;

    fn assert_send<T: Send>() {}

    #[test]
    fn undo_op_and_applied_are_send() {
        assert_send::<UndoOp>();
        assert_send::<UndoApplied>();
    }

    #[test]
    fn apply_reverses_without_a_stack_and_pop_is_lifo() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::write(&b, "1").unwrap();
        let r = UndoStack::apply(UndoOp::Rename {
            from: a.clone(),
            to: b.clone(),
        });
        assert_eq!(r.message.unwrap(), "되돌림: 이름 변경 b → a");
        assert!(r.retry.is_none() && a.exists());
        let mut u = UndoStack::default();
        assert!(u.pop().is_none());
        u.push(trash_op(1));
        u.push(trash_op(2));
        assert_eq!(u.pop(), Some(trash_op(2)));
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn apply_partial_failure_returns_retry_op() {
        let t = tempfile::tempdir().unwrap();
        let (o, n) = (t.path().join("o"), t.path().join("n"));
        fs::write(&n, "2").unwrap();
        fs::write(&o, "blocker").unwrap();
        let r = UndoStack::apply(UndoOp::Move {
            pairs: vec![(o.clone(), n.clone())],
            removed_dirs: vec![],
        });
        assert!(r.message.is_err());
        assert_eq!(
            r.retry,
            Some(UndoOp::Move {
                pairs: vec![(o, n)],
                removed_dirs: vec![]
            })
        );
    }

    #[test]
    fn dir_and_pair_failures_together_do_not_underflow() {
        let t = tempfile::tempdir().unwrap();
        let (o, n) = (t.path().join("o"), t.path().join("n"));
        fs::write(&n, "2").unwrap();
        fs::write(&o, "blocker").unwrap();
        let d1 = t.path().join("d1");
        let d2 = t.path().join("d2");
        let deny = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let r = apply_with(
            UndoOp::Move {
                pairs: vec![(o, n)],
                removed_dirs: vec![d1, d2],
            },
            &deny,
            None,
        );
        let e = r.message.unwrap_err();
        assert!(e.contains("(0/3)"), "{e}");
    }

    #[test]
    fn failed_removed_dirs_are_kept_for_retry() {
        let t = tempfile::tempdir().unwrap();
        let (good, bad) = (t.path().join("good"), t.path().join("bad"));
        let selective = |d: &Path| {
            if d.ends_with("bad") {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                fs::create_dir_all(d)
            }
        };
        let r = apply_with(
            UndoOp::Move {
                pairs: vec![],
                removed_dirs: vec![good.clone(), bad.clone()],
            },
            &selective,
            None,
        );
        assert!(good.is_dir());
        assert!(r.message.unwrap_err().contains("(1/2)"));
        assert_eq!(
            r.retry,
            Some(UndoOp::Move {
                pairs: vec![],
                removed_dirs: vec![bad]
            })
        );
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn mkdir_undo_removes_empty_dir() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("새폴더");
        fs::create_dir(&d).unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Mkdir { path: d.clone() });
        let msg = u.undo().unwrap();
        assert_eq!(msg, "되돌림: 폴더 생성 취소 새폴더");
        assert!(!d.exists());
        assert!(u.is_empty());
    }

    #[test]
    fn mkdir_undo_non_empty_errors_and_drops_op() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("d");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("x"), "1").unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Mkdir { path: d.clone() });
        assert!(u.undo().is_err());
        assert!(d.join("x").exists());
        assert!(u.is_empty());
    }

    #[test]
    fn rename_undo_restores() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::write(&b, "1").unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Rename {
            from: a.clone(),
            to: b.clone(),
        });
        assert_eq!(u.undo().unwrap(), "되돌림: 이름 변경 b → a");
        assert!(a.exists() && !b.exists());
    }

    #[test]
    fn rename_undo_never_overwrites() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::write(&a, "old").unwrap();
        fs::write(&b, "new").unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Rename {
            from: a.clone(),
            to: b.clone(),
        });
        let e = u.undo().unwrap_err();
        assert_eq!(e, format!("되돌릴 수 없음: {} 이미 존재", s(&a)));
        assert_eq!(fs::read_to_string(&a).unwrap(), "old");
        assert_eq!(fs::read_to_string(&b).unwrap(), "new");
        assert!(u.is_empty());
    }

    #[test]
    fn move_undo_moves_back() {
        let t = tempfile::tempdir().unwrap();
        let (o1, n1) = (t.path().join("o1"), t.path().join("n1"));
        let (o2, n2) = (t.path().join("o2"), t.path().join("n2"));
        fs::write(&n1, "1").unwrap();
        fs::write(&n2, "2").unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Move {
            pairs: vec![(o1.clone(), n1.clone()), (o2.clone(), n2.clone())],
            removed_dirs: vec![],
        });
        assert_eq!(u.undo().unwrap(), "되돌림: 이동 2개");
        assert!(o1.exists() && o2.exists() && !n1.exists() && !n2.exists());
    }

    #[test]
    fn move_undo_partial_failure_reports_error() {
        let t = tempfile::tempdir().unwrap();
        let (o1, n1) = (t.path().join("o1"), t.path().join("n1"));
        let (o2, n2) = (t.path().join("o2"), t.path().join("n2"));
        fs::write(&n1, "1").unwrap();
        fs::write(&n2, "2").unwrap();
        fs::write(&o2, "blocker").unwrap();
        let mut u = UndoStack::default();
        u.push(UndoOp::Move {
            pairs: vec![(o1.clone(), n1.clone()), (o2.clone(), n2.clone())],
            removed_dirs: vec![],
        });
        let e = u.undo().unwrap_err();
        assert!(e.contains("1/2"), "{e}");
        // Failed pair is kept on the stack so the user can retry.
        assert_eq!(u.len(), 1);
        assert_eq!(
            u.ops[0],
            UndoOp::Move {
                pairs: vec![(o2.clone(), n2.clone())],
                removed_dirs: vec![]
            }
        );
        assert!(o1.exists());
        assert_eq!(fs::read_to_string(&o2).unwrap(), "blocker");
        assert!(n2.exists());
    }

    /// A trash op with nothing to restore (undo is a no-op success).
    fn trash_op(i: usize) -> UndoOp {
        UndoOp::Trash {
            entries: vec![TrashEntry {
                id: format!("/nonexistent/hdir-undo-test/{i}").into(),
                name: format!("n{i}"),
                os_name: format!("n{i}").into(),
                original_parent: None,
                deleted_at: Some(i as i64),
                size: None,
                is_dir: false,
            }],
        }
    }

    fn trash_files(store: &FakeTrashStore, paths: &[&Path]) -> UndoOp {
        let paths: Vec<PathBuf> = paths.iter().map(|p| p.to_path_buf()).collect();
        UndoOp::Trash {
            entries: store.trash(&paths).unwrap().entries,
        }
    }

    #[test]
    fn undo_trash_restores_items_to_original_paths() {
        let store = FakeTrashStore::new();
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a.txt"), t.path().join("한글 b"));
        fs::write(&a, "A").unwrap();
        fs::create_dir(&b).unwrap();
        let op = trash_files(&store, &[&a, &b]);
        assert!(!a.exists() && !b.exists());
        let r = UndoStack::apply_with_store(op, &store);
        assert_eq!(r.message.unwrap(), "되돌림: 휴지통 복원 2개");
        assert!(r.retry.is_none());
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert!(b.is_dir());
        assert!(store.list().unwrap().entries.is_empty());
    }

    #[test]
    fn undo_trash_conflict_never_overwrites_and_keeps_item_for_retry() {
        let store = FakeTrashStore::new();
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a.txt"), t.path().join("b.txt"));
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();
        let op = trash_files(&store, &[&a, &b]);
        fs::write(&b, "new b").unwrap();
        let r = UndoStack::apply_with_store(op, &store);
        let msg = r.message.unwrap_err();
        assert!(msg.contains("일부 실패 (1/2)"), "{msg}");
        assert!(msg.contains("b.txt"), "{msg}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "new b");
        let Some(UndoOp::Trash { entries }) = r.retry else {
            panic!("expected retry");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b.txt");
        // retry works once the blocker is gone
        fs::remove_file(&b).unwrap();
        let r = UndoStack::apply_with_store(UndoOp::Trash { entries }, &store);
        assert!(r.message.is_ok() && r.retry.is_none());
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
    }

    #[test]
    fn undo_trash_item_gone_from_trash_is_reported_without_retry() {
        let store = FakeTrashStore::new();
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.txt");
        fs::write(&a, "A").unwrap();
        let op = trash_files(&store, &[&a]);
        let UndoOp::Trash { entries } = &op else {
            unreachable!()
        };
        store.purge(entries).unwrap();
        let r = UndoStack::apply_with_store(op, &store);
        assert!(r.message.is_err());
        assert!(r.retry.is_none());
    }

    #[test]
    fn cap_drops_oldest() {
        let mut u = UndoStack::default();
        for i in 0..60 {
            u.push(trash_op(i));
        }
        assert_eq!(u.len(), 50);
        assert_eq!(u.ops[0], trash_op(10));
    }

    #[test]
    fn undo_is_lifo_and_empty_errs() {
        let mut u = UndoStack::default();
        assert!(u.undo().is_err());
        u.push(trash_op(1));
        u.push(trash_op(2));
        let _ = u.undo();
        assert_eq!(u.len(), 1);
        assert_eq!(u.ops[0], trash_op(1));
    }

    #[test]
    fn move_undo_recreates_removed_empty_dirs() {
        let t = tempfile::tempdir().unwrap();
        let gone = t.path().join("gone");
        let nested = t.path().join("a/b");
        let mut u = UndoStack::default();
        u.push(UndoOp::Move {
            pairs: vec![],
            removed_dirs: vec![gone.clone(), nested.clone()],
        });
        assert!(u.undo().is_ok());
        assert!(gone.is_dir() && nested.is_dir());
        assert!(u.is_empty());
    }
}
