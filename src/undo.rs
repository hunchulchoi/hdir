//! Undo stack for reversible file operations (no terminal IO).

use std::fs;
use std::path::{Path, PathBuf};

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
    },
    Trash {
        count: usize,
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

    /// Pop and reverse the latest op. The op is consumed, even on error, except
    /// that failed pairs of a move are pushed back for a retry. Existing files
    /// are never overwritten.
    pub fn undo(&mut self) -> Result<String, String> {
        let op = self.ops.pop().ok_or("되돌릴 작업 없음")?;
        match op {
            UndoOp::Mkdir { path } => {
                fs::remove_dir(&path).map_err(|e| format!("되돌릴 수 없음: {e}"))?;
                Ok(format!("되돌림: 폴더 생성 취소 {}", base(&path)))
            }
            UndoOp::Rename { from, to } => {
                restore(&to, &from)?;
                Ok(format!("되돌림: 이름 변경 {} → {}", base(&to), base(&from)))
            }
            UndoOp::Move { pairs } => {
                let total = pairs.len();
                let mut errors = Vec::new();
                let mut failed = Vec::new();
                for (orig, new) in pairs {
                    if let Err(e) = restore(&new, &orig) {
                        errors.push(e);
                        failed.push((orig, new));
                    }
                }
                if errors.is_empty() {
                    Ok(format!("되돌림: 이동 {total}개"))
                } else {
                    let msg = format!(
                        "되돌림 일부 실패 ({}/{total}): {}",
                        total - errors.len(),
                        errors[0]
                    );
                    // Keep only the failed pairs so the user can retry.
                    self.push(UndoOp::Move { pairs: failed });
                    Err(msg)
                }
            }
            UndoOp::Trash { .. } => Err("휴지통 항목은 Finder/휴지통에서 복원하세요".into()),
        }
    }
}

fn base(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Move `current` back to `target`, refusing to overwrite.
fn restore(current: &Path, target: &Path) -> Result<(), String> {
    if target.symlink_metadata().is_ok() {
        return Err(format!(
            "되돌릴 수 없음: {} 이미 존재",
            target.to_string_lossy()
        ));
    }
    // A merged-away source directory may be gone; recreate its parent chain.
    if let Some(parent) = target.parent().filter(|p| !p.exists()) {
        fs::create_dir_all(parent).map_err(|e| format!("되돌릴 수 없음: {e}"))?;
    }
    // Falls back to copy + remove across devices; never overwrites.
    crate::fs_ops::move_path_no_overwrite(current, target)
        .map_err(|e| format!("되돌릴 수 없음: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        });
        let e = u.undo().unwrap_err();
        assert!(e.contains("1/2"), "{e}");
        // Failed pair is kept on the stack so the user can retry.
        assert_eq!(u.len(), 1);
        assert_eq!(
            u.ops[0],
            UndoOp::Move {
                pairs: vec![(o2.clone(), n2.clone())]
            }
        );
        assert!(o1.exists());
        assert_eq!(fs::read_to_string(&o2).unwrap(), "blocker");
        assert!(n2.exists());
    }

    #[test]
    fn trash_is_not_undoable() {
        let mut u = UndoStack::default();
        u.push(UndoOp::Trash { count: 2 });
        assert_eq!(
            u.undo().unwrap_err(),
            "휴지통 항목은 Finder/휴지통에서 복원하세요"
        );
        assert!(u.is_empty());
    }

    #[test]
    fn cap_drops_oldest() {
        let mut u = UndoStack::default();
        for i in 0..60 {
            u.push(UndoOp::Trash { count: i });
        }
        assert_eq!(u.len(), 50);
        assert_eq!(u.ops[0], UndoOp::Trash { count: 10 });
    }

    #[test]
    fn undo_is_lifo_and_empty_errs() {
        let mut u = UndoStack::default();
        assert!(u.undo().is_err());
        u.push(UndoOp::Trash { count: 1 });
        u.push(UndoOp::Trash { count: 2 });
        let _ = u.undo();
        assert_eq!(u.len(), 1);
        assert_eq!(u.ops[0], UndoOp::Trash { count: 1 });
    }
}
