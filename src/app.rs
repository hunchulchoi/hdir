//! Application state and key handling (no terminal IO).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::dialog::{self, Dialog, DialogResult};
use crate::fs_ops::{self};
use crate::fs_ops::{OpOutcome, OpReport};
use crate::job::{ConflictAnswer, JobEvent, JobHandle, JobKind, JobSpec, ProgressView};
use crate::loader::Loader;
use crate::panel::{AsyncEnter, EntryKind, LoadKind, LoadRequest, Panel};
use crate::theme::{self, Theme};
use crate::trash::{
    self, PurgeReport, RestoreResult, TrackedDelete, TrashBackend, TrashEntry, TrashView,
};
use crate::ui::{self, View};
use crate::undo::{UndoApplied, UndoOp, UndoStack};

/// Rows taken by header (1) + status (1) + fkey bar (1) + panel borders (2).
const CHROME_ROWS: u16 = 5;

/// Visible entry rows inside a panel for a terminal of `total_height` rows.
pub fn panel_rows(total_height: u16) -> usize {
    total_height.saturating_sub(CHROME_ROWS) as usize
}

/// Reverses one op off the UI thread.
pub type UndoRunner = Arc<dyn Fn(UndoOp) -> UndoApplied + Send + Sync>;

/// What a currently open dialog will do once answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAction {
    Mkdir,
    Rename {
        from: PathBuf,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    /// Destination prompt for F5 (copy) / F6 (move).
    Transfer {
        kind: JobKind,
        sources: Vec<PathBuf>,
    },
    /// Answer to a worker's overwrite question.
    Conflict,
    TrashPurge {
        panel: usize,
        entries: Vec<TrashEntry>,
    },
}

/// The single background copy/move job.
pub struct RunningJob {
    pub handle: JobHandle,
    pub view: ProgressView,
    pub kind: JobKind,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
    /// Esc was pressed; keep the "취소 중…" status until the worker finishes.
    pub cancelling: bool,
    /// Panel the job was started from, and its directory at that time.
    pub src_panel: usize,
    pub src_cwd: PathBuf,
}

/// What a background task is doing (and what to do when it ends).
enum BgKind {
    Trash {
        panel: usize,
        cwd: PathBuf,
    },
    Undo,
    Mkdir {
        panel: usize,
        parent: PathBuf,
        name: String,
    },
    Rename {
        panel: usize,
        from: PathBuf,
        name: String,
    },
    OpenOsTrash,
    TrashList {
        panel: usize,
    },
    TrashRestore {
        panel: usize,
    },
    TrashPurge {
        panel: usize,
    },
}

type TrashListing = Result<Vec<TrashEntry>, String>;

enum BgResult {
    Trash(TrackedDelete),
    TrashList(TrashListing),
    OpenOsTrash(Result<(), String>),
    /// Restore result, how many selected items had no known original
    /// location, plus the refreshed listing.
    TrashRestore(RestoreResult, usize, TrashListing),
    /// Purge result plus the refreshed listing.
    TrashPurge(PurgeReport, TrashListing),
    Undo(UndoApplied),
    Mkdir(std::io::Result<PathBuf>),
    Rename(std::io::Result<PathBuf>),
}

/// A short blocking filesystem operation (trash, undo) running on a worker
/// thread; polled from [`App::tick`].
pub struct BgTask {
    rx: Receiver<BgResult>,
    kind: BgKind,
}

impl BgTask {
    fn spawn(kind: BgKind, work: impl FnOnce() -> BgResult + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        Self { rx, kind }
    }
}

/// Expand a leading `~` and resolve relative paths against `cwd`.
fn resolve_dest(input: &str, cwd: &std::path::Path) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let expanded = if input == "~" {
        home().unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        home().map_or_else(|| PathBuf::from(input), |h| h.join(rest))
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

fn conflict_dialog(dest: &std::path::Path) -> Dialog {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Default to Skip so a stray Enter never destroys data.
    Dialog::choice_with_default(
        "덮어쓰기 확인",
        vec![format!("이미 존재: {name}")],
        ["Overwrite", "Skip", "Overwrite all", "Skip all", "Cancel"]
            .map(String::from)
            .to_vec(),
        1,
    )
}

pub struct App {
    pub running: bool,
    pub panels: [Panel; 2],
    pub active: usize,
    pub status: Option<String>,
    pub shell: String,
    pub dialog: Option<(Dialog, PendingAction)>,
    pub job: Option<RunningJob>,
    /// The single background trash/undo task.
    pub task: Option<BgTask>,
    /// A conflict question that arrived while another dialog was open.
    pending_conflict: Option<PathBuf>,
    trash: Arc<dyn TrashBackend>,
    /// Per panel: showing the trash instead of its directory.
    pub trash_views: [Option<TrashView>; 2],
    /// Runs one undo op on the worker; tests may swap in a gated one.
    pub undo_runner: UndoRunner,
    pub undo: UndoStack,
    /// Background directory reader; tests may swap in a gated one.
    pub loader: Loader,
    next_load_id: u64,
    page_height: usize,
    /// Effective theme for the current mode (after any color-depth fallback).
    pub theme: Theme,
}

impl Default for App {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd.clone(), cwd)
    }
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self::with_trash(left, right, trash::system_store())
    }

    pub fn with_trash(left: PathBuf, right: PathBuf, trash: Arc<dyn TrashBackend>) -> Self {
        let shell = std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_string());
        let undo_store = Arc::clone(&trash);
        Self {
            running: true,
            panels: [Panel::new(left), Panel::new(right)],
            active: 0,
            status: None,
            shell,
            dialog: None,
            job: None,
            task: None,
            pending_conflict: None,
            trash,
            trash_views: [None, None],
            undo_runner: Arc::new(move |op| UndoStack::apply_with_store(op, &*undo_store)),
            undo: UndoStack::default(),
            loader: Loader::default(),
            next_load_id: 0,
            page_height: 1,
            theme: theme::builtin("mdir").expect("mdir is built in"),
        }
    }

    pub fn active_panel(&self) -> &Panel {
        &self.panels[self.active]
    }

    pub fn active_panel_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    pub fn other_panel(&self) -> &Panel {
        &self.panels[1 - self.active]
    }

    /// Selected names (sorted) in the active panel, else the current entry
    /// unless it is the `..` entry.
    pub fn selected_or_current(&self) -> Vec<OsString> {
        let panel = self.active_panel();
        if !panel.selected.is_empty() {
            let mut names: Vec<OsString> = panel.selected.iter().cloned().collect();
            names.sort();
            return names;
        }
        match panel.current() {
            Some(e) if e.kind != EntryKind::Parent => vec![e.os_name.clone()],
            _ => Vec::new(),
        }
    }

    /// Handle a terminal event; returns true when the screen needs a redraw.
    pub fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key);
                true
            }
            Event::Resize(..) => true,
            _ => false,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return;
        }
        if self.trash_views[self.active].is_some() {
            self.handle_trash_key(key);
            return;
        }
        let page = self.page_height.max(1);
        match key.code {
            KeyCode::Esc => {
                if let Some(job) = self.job.as_mut() {
                    job.handle.cancel();
                    job.cancelling = true;
                    self.status = Some("취소 중…".into());
                }
            }
            KeyCode::F(5) => self.open_transfer(JobKind::Copy),
            KeyCode::F(6) if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.open_transfer(JobKind::Move)
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => self.undo(),
            KeyCode::F(7) => self.open_mkdir(),
            KeyCode::F(2) => self.open_rename(),
            KeyCode::F(6) if key.modifiers.contains(KeyModifiers::SHIFT) => self.open_rename(),
            KeyCode::F(8) if key.modifiers.contains(KeyModifiers::ALT) => self.open_trash_view(),
            KeyCode::F(8) => self.open_trash(),
            KeyCode::F(10) | KeyCode::Char('q') => self.running = false,
            KeyCode::Tab => self.active = 1 - self.active,
            KeyCode::Up => self.navigate(Panel::move_up),
            KeyCode::Down => self.navigate(Panel::move_down),
            KeyCode::PageUp => self.navigate(|p| p.page_up(page)),
            KeyCode::PageDown => self.navigate(|p| p.page_down(page)),
            KeyCode::Home => self.navigate(Panel::home),
            KeyCode::End => self.navigate(Panel::end),
            KeyCode::Backspace => {
                self.status = None;
                let id = self.next_id();
                if let Some(req) = self.active_panel_mut().begin_parent(id) {
                    self.dispatch(self.active, req);
                }
            }
            KeyCode::Char('.') => {
                self.status = None;
                let id = self.next_id();
                let req = self.active_panel_mut().begin_toggle_hidden(id);
                self.dispatch(self.active, req);
            }
            KeyCode::Enter => {
                self.status = None;
                let id = self.next_id();
                match self.active_panel_mut().begin_enter(id) {
                    AsyncEnter::Opened(path) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.status = Some(format!("열기: {name}"));
                    }
                    AsyncEnter::Load(req) => self.dispatch(self.active, req),
                    AsyncEnter::Nothing => self.show_panel_error(),
                }
            }
            KeyCode::Char(' ') | KeyCode::Insert => {
                self.status = None;
                let panel = self.active_panel_mut();
                if let Some(e) = panel.current()
                    && e.kind != EntryKind::Parent
                {
                    let name = e.os_name.clone();
                    if !panel.selected.remove(&name) {
                        panel.selected.insert(name);
                    }
                }
                self.active_panel_mut().move_down();
            }
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some() || self.task.is_some()
    }

    fn open_transfer(&mut self, kind: JobKind) {
        if self.busy() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let names = self.selected_or_current();
        if names.is_empty() {
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        let sources: Vec<PathBuf> = names.iter().map(|n| cwd.join(n)).collect();
        let title = match kind {
            JobKind::Copy => "복사",
            JobKind::Move => "이동",
        };
        let initial = self.other_panel().cwd.to_string_lossy().into_owned();
        self.status = None;
        self.dialog = Some((
            Dialog::input(title, "대상 경로:", &initial),
            PendingAction::Transfer { kind, sources },
        ));
    }

    fn start_job(&mut self, kind: JobKind, sources: Vec<PathBuf>, input: &str) {
        if self.busy() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        // Only reject empty input; surrounding spaces may be part of a real name.
        // Whether the folder exists is checked by the worker (no IO here).
        if input.is_empty() {
            self.status = Some("오류: 대상 폴더 없음".into());
            return;
        }
        let dest_dir = resolve_dest(input, &self.active_panel().cwd);
        let handle = JobHandle::spawn(JobSpec {
            kind,
            sources: sources.clone(),
            dest_dir: dest_dir.clone(),
        });
        self.job = Some(RunningJob {
            handle,
            view: ProgressView::new(kind),
            kind,
            sources,
            dest_dir,
            cancelling: false,
            src_panel: self.active,
            src_cwd: self.active_panel().cwd.clone(),
        });
    }

    fn next_id(&mut self) -> u64 {
        self.next_load_id += 1;
        self.next_load_id
    }

    /// Hand a panel's pending load request to the background loader.
    fn dispatch(&mut self, idx: usize, req: LoadRequest) {
        let hidden = self.panels[idx].show_hidden;
        self.loader.request(idx, req.id, req.dir, hidden);
    }

    /// Start an async reload of panel `idx` (cursor on `focus` if given).
    fn reload_panel(&mut self, idx: usize, focus: Option<OsString>) {
        let id = self.next_id();
        let req = self.panels[idx].begin_reload(id, focus);
        self.dispatch(idx, req);
    }

    /// Apply finished directory loads; returns true when anything changed.
    fn drain_loads(&mut self) -> bool {
        let mut changed = false;
        for res in self.loader.try_results() {
            let id = res.id;
            let Some(idx) = self
                .panels
                .iter()
                .position(|p| p.loading.as_ref().is_some_and(|r| r.id == id))
            else {
                continue; // superseded
            };
            let kind = self.panels[idx].loading.as_ref().map(|r| r.kind);
            if self.panels[idx].finish_load(res) {
                changed = true;
                if idx == self.active
                    && kind != Some(LoadKind::Reload)
                    && let Some(err) = self.panels[idx].error.clone()
                {
                    self.status = Some(err);
                }
                if std::mem::take(&mut self.panels[idx].reload_after_load) {
                    self.reload_panel(idx, None);
                }
            }
        }
        changed
    }

    /// Poll background loads and the job; returns true when the screen needs a redraw.
    pub fn tick(&mut self) -> bool {
        let loaded = self.drain_loads();
        let task = self.tick_task();
        self.tick_job() || loaded || task
    }

    fn tick_task(&mut self) -> bool {
        let Some(task) = self.task.as_ref() else {
            return false;
        };
        let result = match task.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => None,
        };
        let Some(task) = self.task.take() else {
            return false;
        };
        match (task.kind, result) {
            (BgKind::Trash { panel, cwd }, Some(BgResult::Trash(tracked))) => {
                let TrackedDelete {
                    results,
                    undo,
                    lost,
                } = tracked;
                let failed: Vec<&(PathBuf, Result<(), String>)> =
                    results.iter().filter(|(_, r)| r.is_err()).collect();
                let done: Vec<PathBuf> = results
                    .iter()
                    .filter(|(_, r)| r.is_ok())
                    .map(|(p, _)| p.clone())
                    .collect();
                let ok = done.len();
                let first = failed
                    .first()
                    .and_then(|(_, r)| r.as_ref().err())
                    .cloned()
                    .unwrap_or_default();
                let mut status = match failed.len() {
                    0 => format!("휴지통: {ok}개"),
                    _ if ok == 0 => format!("오류: {first}"),
                    n => format!("휴지통: {ok}개, 실패 {n}개: {first}"),
                };
                if lost > 0 {
                    status.push_str(" · 되돌리기 정보 없음");
                }
                if !undo.is_empty() {
                    self.undo.push(UndoOp::Trash { items: undo });
                }
                self.clear_selection(panel, &cwd, &done);
                self.reload_both();
                self.status = Some(status);
            }
            (
                BgKind::Mkdir {
                    panel,
                    parent,
                    name,
                },
                Some(BgResult::Mkdir(res)),
            ) => match res {
                Ok(path) => {
                    self.undo.push(UndoOp::Mkdir { path });
                    self.after_fs_op(panel, &parent, Some(&name), format!("폴더 생성: {name}"));
                }
                Err(e) => {
                    self.reload_both();
                    self.status = Some(format!("오류: {e}"));
                }
            },
            (BgKind::Rename { panel, from, name }, Some(BgResult::Rename(res))) => match res {
                Ok(to) => {
                    let parent = from.parent().map(PathBuf::from).unwrap_or_default();
                    self.undo.push(UndoOp::Rename { from, to });
                    self.after_fs_op(panel, &parent, Some(&name), format!("이름 변경: {name}"));
                }
                Err(e) => {
                    self.reload_both();
                    self.status = Some(format!("오류: {e}"));
                }
            },
            (BgKind::OpenOsTrash, Some(BgResult::OpenOsTrash(res))) => {
                self.status = Some(match res {
                    Ok(()) => "Finder 휴지통을 열었음 · 복원은 '되돌려 놓기'".to_string(),
                    Err(e) => format!("오류: {e}"),
                });
            }
            (BgKind::TrashList { panel }, Some(BgResult::TrashList(res))) => {
                self.status = match self.apply_trash_listing(panel, &res) {
                    Ok(()) => None,
                    Err(e) => Some(format!("오류: {e}")),
                };
            }
            (
                BgKind::TrashRestore { panel },
                Some(BgResult::TrashRestore(result, unknown, list)),
            ) => {
                self.finish_trash_restore(panel, result, unknown, list);
            }
            (BgKind::TrashPurge { panel }, Some(BgResult::TrashPurge(report, list))) => {
                let _ = self.apply_trash_listing_all(panel, &list);
                let skipped = report.skipped;
                self.status = Some(match report.error {
                    Some(e) => format!("오류: {e}"),
                    None if skipped > 0 => format!(
                        "영구 삭제: {}개, 휴지통 항목이 바뀌어 건너뜀 {skipped}개",
                        report.purged
                    ),
                    None => format!("영구 삭제: {}개", report.purged),
                });
            }
            (BgKind::Undo, Some(BgResult::Undo(done))) => {
                if let Some(retry) = done.retry {
                    self.undo.push(retry);
                }
                self.reload_both();
                self.status = Some(match done.message {
                    Ok(m) | Err(m) => m,
                });
            }
            _ => {
                self.reload_both();
                self.status = Some("오류: 작업이 중단됨".into());
            }
        }
        true
    }

    fn reload_both(&mut self) {
        for i in 0..2 {
            self.reload_after_op(i, None);
        }
    }

    /// Reload after a finished task/job. A panel that is already navigating
    /// (Enter/Parent) keeps its pending load: it reads the directory fresh anyway
    /// and a reload would supersede the navigation.
    fn reload_after_op(&mut self, idx: usize, focus: Option<OsString>) {
        let navigating = self.panels[idx]
            .loading
            .as_ref()
            .is_some_and(|r| r.kind != LoadKind::Reload);
        if navigating {
            // Reload as soon as the navigation lands so the task's changes show.
            self.panels[idx].reload_after_load = true;
        } else {
            self.reload_panel(idx, focus);
        }
    }

    /// Drop the selection of `paths` in panel `idx`, unless it moved elsewhere.
    fn clear_selection(&mut self, idx: usize, cwd: &std::path::Path, paths: &[PathBuf]) {
        let panel = &mut self.panels[idx];
        if panel.cwd != cwd {
            return;
        }
        for p in paths {
            if let Some(name) = p.file_name() {
                panel.selected.remove(name);
            }
        }
    }

    fn tick_job(&mut self) -> bool {
        let Some(job) = self.job.as_mut() else {
            return false;
        };
        let events = job.handle.try_events();
        if events.is_empty() {
            return false;
        }
        let mut finished = None;
        for ev in &events {
            job.view.apply(ev);
            match ev {
                JobEvent::Conflict { dest } => {
                    if self.dialog.is_some() {
                        // Never replace an open dialog; ask once it closes.
                        self.pending_conflict = Some(dest.clone());
                    } else {
                        self.dialog = Some((conflict_dialog(dest), PendingAction::Conflict));
                    }
                }
                JobEvent::Finished { report } => finished = Some(report.clone()),
                _ => {}
            }
        }
        match finished {
            Some(report) => self.finish_job(report),
            None => {
                if !job.cancelling {
                    self.status = Some(job.view.status_line());
                }
            }
        }
        true
    }

    fn finish_job(&mut self, report: OpReport) {
        let Some(job) = self.job.take() else {
            return;
        };
        self.pending_conflict = None;
        // Partial moves are undoable whatever the outcome.
        if job.kind == JobKind::Move
            && (!report.moved.is_empty() || !report.removed_dirs.is_empty())
        {
            self.undo.push(UndoOp::Move {
                pairs: report.moved.clone(),
                removed_dirs: report.removed_dirs.clone(),
            });
        }
        // Only sources fully copied / moved lose their selection; skipped, partial
        // and unprocessed ones stay selected.
        let processed: Vec<PathBuf> = report
            .done_sources
            .iter()
            .filter(|p| p.parent() == Some(job.src_cwd.as_path()))
            .cloned()
            .collect();
        self.clear_selection(job.src_panel, &job.src_cwd, &processed);
        let status = job_summary(job.kind, &report);
        // A conflict dialog may still be open for a job that just ended.
        if matches!(self.dialog, Some((_, PendingAction::Conflict))) {
            self.dialog = None;
        }
        self.reload_both();
        self.status = Some(status);
    }

    /// Open the queued conflict question once no other dialog is showing.
    fn open_pending_conflict(&mut self) {
        if self.dialog.is_some() {
            return;
        }
        if let Some(dest) = self.pending_conflict.take()
            && self.job.is_some()
        {
            self.dialog = Some((conflict_dialog(&dest), PendingAction::Conflict));
        }
    }

    fn open_mkdir(&mut self) {
        self.status = None;
        self.dialog = Some((
            Dialog::input("새 폴더", "폴더 이름:", ""),
            PendingAction::Mkdir,
        ));
    }

    fn open_rename(&mut self) {
        let panel = self.active_panel();
        let Some(e) = panel.current().filter(|e| e.kind != EntryKind::Parent) else {
            return;
        };
        let from = panel.cwd.join(&e.os_name);
        let name = e.os_name.to_string_lossy().into_owned();
        self.status = None;
        self.dialog = Some((
            Dialog::input("이름 변경", "새 이름:", &name),
            PendingAction::Rename { from },
        ));
    }

    fn open_trash(&mut self) {
        let names = self.selected_or_current();
        if names.is_empty() {
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        let paths: Vec<PathBuf> = names.iter().map(|n| cwd.join(n)).collect();
        let mut msg: Vec<String> = names
            .iter()
            .take(5)
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        if names.len() > 5 {
            msg.push(format!("외 {}개", names.len() - 5));
        }
        self.status = None;
        self.dialog = Some((
            Dialog::confirm("휴지통으로 이동", msg),
            PendingAction::Trash { paths },
        ));
    }

    // ---- trash view ----

    fn open_trash_view(&mut self) {
        if self.task.is_some() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        if !self.trash.supports_view() {
            // No in-panel view: the OS file manager shows (and restores from)
            // the trash. Spawning a process must not block the UI.
            let store = Arc::clone(&self.trash);
            self.task = Some(BgTask::spawn(BgKind::OpenOsTrash, move || {
                BgResult::OpenOsTrash(store.open_os_trash())
            }));
            self.status = Some("휴지통 여는 중…".into());
            return;
        }
        let idx = self.active;
        self.trash_views[idx] = Some(TrashView::new(self.panels[idx].cwd.clone()));
        let store = Arc::clone(&self.trash);
        self.task = Some(BgTask::spawn(BgKind::TrashList { panel: idx }, move || {
            BgResult::TrashList(store.list())
        }));
        self.status = Some("휴지통 읽는 중…".into());
    }

    fn close_trash_view(&mut self, idx: usize) {
        if self.trash_views[idx].take().is_some() {
            self.status = None;
            // Restores may have changed the directory underneath.
            self.reload_after_op(idx, None);
        }
    }

    fn handle_trash_key(&mut self, key: KeyEvent) {
        let idx = self.active;
        let page = self.page_height.max(1);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let moved = |app: &mut App, f: &dyn Fn(&mut TrashView)| {
            if app.task.is_none() {
                app.status = None;
            }
            if let Some(v) = app.trash_views[idx].as_mut() {
                f(v);
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.close_trash_view(idx),
            KeyCode::Tab => self.active = 1 - self.active,
            KeyCode::F(10) | KeyCode::Char('q') => self.running = false,
            // Already in the trash view.
            KeyCode::F(8) if alt => {}
            KeyCode::F(8) => self.open_trash_purge(),
            KeyCode::F(5) | KeyCode::Enter => self.open_trash_restore(),
            KeyCode::F(_) => self.status = Some("휴지통에서는 사용할 수 없음".into()),
            KeyCode::Up => moved(self, &TrashView::move_up),
            KeyCode::Down => moved(self, &TrashView::move_down),
            KeyCode::PageUp => moved(self, &|v| v.page_up(page)),
            KeyCode::PageDown => moved(self, &|v| v.page_down(page)),
            KeyCode::Home => moved(self, &TrashView::home),
            KeyCode::End => moved(self, &TrashView::end),
            KeyCode::Char(' ') | KeyCode::Insert => moved(self, &TrashView::toggle_current),
            _ => {}
        }
    }

    /// Entries a restore/purge in panel `idx` applies to.
    fn trash_targets(&self, idx: usize) -> Vec<TrashEntry> {
        self.trash_views[idx]
            .as_ref()
            .map(TrashView::targets)
            .unwrap_or_default()
    }

    fn open_trash_restore(&mut self) {
        if self.task.is_some() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let idx = self.active;
        let entries = self.trash_targets(idx);
        if entries.is_empty() {
            return;
        }
        // Items without a known original location cannot be restored.
        let (known, unknown): (Vec<TrashEntry>, Vec<TrashEntry>) = entries
            .into_iter()
            .partition(|e| e.original_path().is_some());
        if known.is_empty() {
            let names: Vec<String> = unknown.iter().take(3).map(TrashEntry::name).collect();
            self.status = Some(format!(
                "원래 위치를 알 수 없어 복원할 수 없음: {}",
                names.join(", ")
            ));
            return;
        }
        let skipped = unknown.len();
        let store = Arc::clone(&self.trash);
        self.task = Some(BgTask::spawn(
            BgKind::TrashRestore { panel: idx },
            move || {
                let result = trash::restore_verified(&*store, &known);
                BgResult::TrashRestore(result, skipped, store.list())
            },
        ));
        self.status = Some("복원 중…".into());
    }

    fn open_trash_purge(&mut self) {
        let idx = self.active;
        let entries = self.trash_targets(idx);
        if entries.is_empty() {
            return;
        }
        let mut msg: Vec<String> = entries.iter().take(5).map(TrashEntry::name).collect();
        if entries.len() > 5 {
            msg.push(format!("외 {}개", entries.len() - 5));
        }
        self.status = None;
        self.dialog = Some((
            Dialog::danger("영구 삭제", msg),
            PendingAction::TrashPurge {
                panel: idx,
                entries,
            },
        ));
    }

    /// Apply a fresh listing to the trash view of `panel` (if still open);
    /// returns the error message on failure.
    fn apply_trash_listing(&mut self, panel: usize, res: &TrashListing) -> Result<(), String> {
        let Some(view) = self.trash_views[panel].as_mut() else {
            return Ok(());
        };
        match res {
            Ok(entries) => {
                view.set_entries(entries.clone());
                Ok(())
            }
            Err(e) => {
                view.loading = false;
                view.error = Some(e.clone());
                Err(e.clone())
            }
        }
    }

    /// Apply one listing to every open trash view (a change in one panel
    /// affects the other); returns `panel`'s error.
    fn apply_trash_listing_all(&mut self, panel: usize, res: &TrashListing) -> Result<(), String> {
        let _ = self.apply_trash_listing(1 - panel, res);
        self.apply_trash_listing(panel, res)
    }

    fn finish_trash_restore(
        &mut self,
        panel: usize,
        result: RestoreResult,
        unknown: usize,
        list: TrashListing,
    ) {
        let _ = self.apply_trash_listing_all(panel, &list);
        let dirs: Vec<PathBuf> = result
            .restored
            .iter()
            .filter_map(|p| p.parent().map(PathBuf::from))
            .collect();
        for i in 0..2 {
            if dirs.contains(&self.panels[i].cwd) {
                self.reload_after_op(i, None);
            }
        }
        let mut status = result.summary();
        if unknown > 0 {
            status.push_str(&format!(" · 원래 위치를 알 수 없어 건너뜀: {unknown}개"));
        }
        self.status = Some(status);
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) {
        self.dispatch_dialog_key(key);
        self.open_pending_conflict();
    }

    fn dispatch_dialog_key(&mut self, key: KeyEvent) {
        let Some((dlg, _)) = self.dialog.as_mut() else {
            return;
        };
        let result = dlg.handle_key(key);
        if result == DialogResult::Pending {
            return;
        }
        let Some((_, action)) = self.dialog.take() else {
            return;
        };
        match (action, result) {
            (PendingAction::Mkdir, DialogResult::Submitted(name)) => {
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let parent = self.active_panel().cwd.clone();
                let (p, n) = (parent.clone(), name.clone());
                self.task = Some(BgTask::spawn(
                    BgKind::Mkdir {
                        panel: self.active,
                        parent,
                        name,
                    },
                    move || BgResult::Mkdir(fs_ops::make_dir(&p, &n)),
                ));
                self.status = Some("폴더 생성 중…".into());
            }
            (PendingAction::Rename { from }, DialogResult::Submitted(name)) => {
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let (f, n) = (from.clone(), name.clone());
                self.task = Some(BgTask::spawn(
                    BgKind::Rename {
                        panel: self.active,
                        from,
                        name,
                    },
                    move || BgResult::Rename(fs_ops::rename(&f, &n)),
                ));
                self.status = Some("이름 변경 중…".into());
            }
            (PendingAction::Trash { paths }, DialogResult::Confirmed) => {
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let trash = Arc::clone(&self.trash);
                self.task = Some(BgTask::spawn(
                    BgKind::Trash {
                        panel: self.active,
                        cwd: self.active_panel().cwd.clone(),
                    },
                    move || BgResult::Trash(trash::delete_tracked(&*trash, &paths)),
                ));
                self.status = Some("휴지통으로 이동 중…".into());
            }
            (PendingAction::TrashPurge { panel, entries }, DialogResult::Confirmed) => {
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let store = Arc::clone(&self.trash);
                self.task = Some(BgTask::spawn(BgKind::TrashPurge { panel }, move || {
                    BgResult::TrashPurge(purge_verified(&*store, &entries), store.list())
                }));
                self.status = Some("영구 삭제 중…".into());
            }
            (PendingAction::Transfer { kind, sources }, DialogResult::Submitted(input)) => {
                self.start_job(kind, sources, &input);
            }
            (PendingAction::Conflict, result) => {
                let answer = match result {
                    DialogResult::Chosen(0) => ConflictAnswer::Overwrite,
                    DialogResult::Chosen(1) => ConflictAnswer::Skip,
                    DialogResult::Chosen(2) => ConflictAnswer::OverwriteAll,
                    DialogResult::Chosen(3) => ConflictAnswer::SkipAll,
                    _ => ConflictAnswer::Cancel,
                };
                if let Some(job) = &self.job {
                    job.handle.answer(answer);
                }
            }
            _ => {}
        }
    }

    /// Ctrl+Z: pop the latest op here, reverse it on a worker; the result is
    /// applied (status, retry, reload) from [`App::tick`].
    fn undo(&mut self) {
        if self.busy() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let Some(op) = self.undo.pop() else {
            self.status = Some("되돌릴 작업 없음".into());
            return;
        };
        let runner = Arc::clone(&self.undo_runner);
        self.task = Some(BgTask::spawn(BgKind::Undo, move || {
            BgResult::Undo(runner(op))
        }));
        self.status = Some("되돌리는 중…".into());
    }

    /// Reload panel `idx` (cursor on `focus` if given) and the other panel when
    /// it shows the same directory `dir`, then set the status.
    fn after_fs_op(
        &mut self,
        idx: usize,
        dir: &std::path::Path,
        focus: Option<&str>,
        status: String,
    ) {
        if self.panels[1 - idx].cwd == dir {
            self.reload_after_op(1 - idx, None);
        }
        self.reload_after_op(idx, focus.map(OsString::from));
        self.status = Some(status);
    }

    /// Run a navigation action on the active panel, clearing stale status and
    /// surfacing any panel error afterwards.
    fn navigate(&mut self, f: impl FnOnce(&mut Panel)) {
        if self.task.is_none() {
            self.status = None;
        }
        f(self.active_panel_mut());
        self.show_panel_error();
    }

    fn show_panel_error(&mut self) {
        if let Some(err) = self.active_panel().error.clone() {
            self.status = Some(err);
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.page_height = panel_rows(frame.area().height).max(1);
        let rows = panel_rows(frame.area().height);
        for p in &mut self.panels {
            p.ensure_visible(rows);
        }
        for v in self.trash_views.iter_mut().flatten() {
            v.ensure_visible(rows);
        }
        let view = View {
            panels: [&self.panels[0], &self.panels[1]],
            active: self.active,
            status: self.status.as_deref(),
            shell: &self.shell,
            os: std::env::consts::OS,
            trash: [self.trash_views[0].as_ref(), self.trash_views[1].as_ref()],
            theme: &self.theme,
        };
        ui::draw(frame, &view);
        if let Some((dlg, _)) = &self.dialog {
            dialog::render_themed(frame, frame.area(), dlg, &self.theme);
        }
    }
}

/// Purge only the `wanted` items that a fresh listing still contains (matched
/// by id); the rest are skipped, never purged.
fn purge_verified(store: &dyn TrashBackend, wanted: &[TrashEntry]) -> PurgeReport {
    let fresh = match store.list() {
        Ok(f) => f,
        Err(e) => {
            return PurgeReport {
                error: Some(e),
                ..Default::default()
            };
        }
    };
    let (keep, skipped) = trash::match_fresh(wanted, &fresh);
    if keep.is_empty() {
        return PurgeReport {
            skipped,
            ..Default::default()
        };
    }
    match store.purge(&keep) {
        Ok(()) => PurgeReport {
            purged: keep.len(),
            skipped,
            error: None,
        },
        Err(e) => PurgeReport {
            skipped,
            error: Some(e),
            ..Default::default()
        },
    }
}

/// Status-bar text for a finished copy/move job.
fn job_summary(kind: JobKind, report: &OpReport) -> String {
    let n = report.copied;
    let mut status = match &report.outcome {
        OpOutcome::Done => {
            let mut s = match kind {
                JobKind::Copy => format!("복사 완료: {n}개"),
                JobKind::Move => format!("이동 완료: {n}개"),
            };
            if report.skipped > 0 {
                s.push_str(&format!(", 건너뜀 {}개", report.skipped));
            }
            if report.partial > 0 {
                s.push_str(&format!(", 일부만 이동 {}개 (원본 유지)", report.partial));
            }
            s
        }
        OpOutcome::Cancelled => format!("취소됨 (처리 {n}개)"),
        OpOutcome::Failed(msg) => format!("오류: {msg} (처리 {n}개)"),
    };
    if let Some(first) = report.warnings.first() {
        let shown: String = first.chars().take(60).collect();
        status.push_str(&format!(" · 경고 {}개: {shown}", report.warnings.len()));
    }
    if kind == JobKind::Move && report.overwritten > 0 {
        status.push_str(&format!(
            " · 덮어쓴 파일 {}개는 되돌릴 수 없음",
            report.overwritten
        ));
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::ffi::OsStr;
    use std::fs;
    use tempfile::TempDir;

    fn key(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(key(code, KeyEventKind::Press));
    }

    /// Layout: .., adir, bdir, f.txt (+ hidden .h). adir contains inner.txt.
    fn fixture() -> (TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("adir")).unwrap();
        fs::create_dir(root.join("bdir")).unwrap();
        fs::write(root.join("adir/inner.txt"), "x").unwrap();
        fs::write(root.join("f.txt"), "x").unwrap();
        fs::write(root.join(".h"), "x").unwrap();
        let app = App::new(root.to_path_buf(), root.to_path_buf());
        (tmp, app)
    }

    fn name_at(app: &App, i: usize) -> String {
        app.panels[i]
            .current()
            .map(|e| e.name.clone())
            .unwrap_or_default()
    }

    #[test]
    fn new_app_is_running() {
        let (_t, app) = fixture();
        assert!(app.running);
        assert_eq!(app.active, 0);
        assert!(!app.shell.is_empty());
    }

    #[test]
    fn f10_quits() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(10));
        assert!(!app.running);
    }

    #[test]
    fn q_quits() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.running);
    }

    #[test]
    fn repeat_is_handled() {
        let (_t, mut app) = fixture();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Repeat));
        assert!(!app.running);
    }

    #[test]
    fn other_keys_do_not_quit() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::F(1));
        press(&mut app, KeyCode::Enter);
        assert!(app.running);
    }

    #[test]
    fn release_events_are_ignored() {
        let (_t, mut app) = fixture();
        app.handle_key(key(KeyCode::Char('q'), KeyEventKind::Release));
        app.handle_key(key(KeyCode::F(10), KeyEventKind::Release));
        app.handle_key(key(KeyCode::Down, KeyEventKind::Release));
        app.handle_key(key(KeyCode::Tab, KeyEventKind::Release));
        assert!(app.running);
        assert_eq!(app.panels[0].cursor, 0);
        assert_eq!(app.active, 0);
    }

    #[test]
    fn tab_toggles_active_and_keys_affect_only_active() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[1].cursor, 1);
        assert_eq!(app.panels[0].cursor, 0);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 0);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[0].cursor, 2);
        assert_eq!(app.panels[1].cursor, 1);
    }

    #[test]
    fn active_and_other_panel_accessors() {
        let (_t, mut app) = fixture();
        app.panels[1].cursor = 2;
        assert_eq!(app.active_panel().cursor, 0);
        assert_eq!(app.other_panel().cursor, 2);
        app.active_panel_mut().cursor = 1;
        assert_eq!(app.panels[0].cursor, 1);
    }

    #[test]
    fn navigation_keys_move_cursor() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        assert_eq!(name_at(&app, 0), "adir");
        press(&mut app, KeyCode::Up);
        assert_eq!(name_at(&app, 0), "..");
        press(&mut app, KeyCode::End);
        assert_eq!(name_at(&app, 0), "f.txt");
        press(&mut app, KeyCode::Home);
        assert_eq!(app.panels[0].cursor, 0);
        app.page_height = 2;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.panels[0].cursor, 2);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.panels[0].cursor, 0);
    }

    #[test]
    fn enter_dir_and_backspace_returns_to_it() {
        let (tmp, mut app) = fixture();
        let root = fs::canonicalize(tmp.path()).unwrap();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root.join("adir"));
        assert_eq!(app.panels[1].cwd, root);
        press(&mut app, KeyCode::Backspace);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root);
        assert_eq!(name_at(&app, 0), "adir");
    }

    #[test]
    fn enter_file_sets_status_with_name() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Enter);
        assert!(app.status.as_deref().unwrap().contains("f.txt"));
    }

    #[test]
    fn navigation_clears_status() {
        let (_t, mut app) = fixture();
        app.status = Some("old".into());
        press(&mut app, KeyCode::Down);
        assert_eq!(app.status, None);
    }

    #[test]
    fn space_toggles_selection_and_advances() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panels[0].selected.contains(OsStr::new("adir")));
        assert_eq!(name_at(&app, 0), "bdir");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.panels[0].selected.contains(OsStr::new("adir")));
    }

    #[test]
    fn space_on_parent_does_not_select() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panels[0].selected.is_empty());
    }

    #[test]
    fn insert_selects_like_space() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Insert);
        assert!(app.panels[0].selected.contains(OsStr::new("adir")));
        assert_eq!(name_at(&app, 0), "bdir");
        let (_t2, mut app2) = fixture();
        press(&mut app2, KeyCode::Insert);
        assert!(app2.panels[0].selected.is_empty());
    }

    #[test]
    fn selected_or_current_variants() {
        let (_t, mut app) = fixture();
        assert!(app.selected_or_current().is_empty()); // on ..
        press(&mut app, KeyCode::End);
        assert_eq!(app.selected_or_current(), vec![OsString::from("f.txt")]);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' ')); // adir
        press(&mut app, KeyCode::Down); // skip bdir
        press(&mut app, KeyCode::Char(' ')); // f.txt
        press(&mut app, KeyCode::Home);
        assert_eq!(
            app.selected_or_current(),
            vec![OsString::from("adir"), OsString::from("f.txt")]
        );
    }

    #[test]
    fn hidden_toggle_drops_hidden_selection_from_selected_or_current() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('.')); // show hidden: .., adir, bdir, .h, f.txt
        settle(&mut app);
        for _ in 0..3 {
            press(&mut app, KeyCode::Down); // .h
        }
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.selected_or_current(), vec![OsString::from(".h")]);
        press(&mut app, KeyCode::Char('.')); // hide again
        settle(&mut app);
        assert!(app.active_panel().selected.is_empty());
        let cur = app.active_panel().current().unwrap().os_name.clone();
        assert_eq!(app.selected_or_current(), vec![cur]);
    }

    #[test]
    fn handle_event_reports_whether_redraw_needed() {
        let (_t, mut app) = fixture();
        assert!(app.handle_event(Event::Key(key(KeyCode::Down, KeyEventKind::Press))));
        assert!(app.handle_event(Event::Resize(80, 24)));
        assert!(!app.handle_event(Event::Key(key(KeyCode::Down, KeyEventKind::Release))));
        assert!(!app.handle_event(Event::FocusGained));
        assert_eq!(app.panels[0].cursor, 1);
    }

    #[test]
    fn dot_toggles_hidden_on_active_only() {
        let (_t, mut app) = fixture();
        let before = app.panels[0].show_hidden;
        press(&mut app, KeyCode::Char('.'));
        assert_ne!(app.panels[0].show_hidden, before);
        assert_eq!(app.panels[1].show_hidden, before);
    }

    #[test]
    fn panel_rows_matches_layout() {
        assert_eq!(panel_rows(24), 19);
        assert_eq!(panel_rows(3), 0);
        assert_eq!(panel_rows(0), 0);
    }

    #[test]
    fn draw_renders_both_panels_and_stores_page_height() {
        let (_t, mut app) = fixture();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.page_height, 19);
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("hdir"));
        let dir_name = app.panels[0]
            .cwd
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(text.matches(dir_name.as_str()).count() >= 2);
    }

    // ---- HDIR-8: mkdir / rename / trash ----

    use std::sync::{Arc, Mutex};

    use crate::trash::{FakeTrash, TrashBackend};

    type Fake = Arc<FakeTrash>;

    fn fixture_trash() -> (TempDir, App, Fake) {
        fixture_with(FakeTrash::new())
    }

    fn fixture_with(fake: FakeTrash) -> (TempDir, App, Fake) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("adir")).unwrap();
        fs::write(root.join("f.txt"), "x").unwrap();
        fs::write(root.join("g.txt"), "x").unwrap();
        let fake: Fake = Arc::new(fake);
        let app = App::with_trash(root.to_path_buf(), root.to_path_buf(), fake.clone());
        (tmp, app, fake)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn f7_creates_korean_dir_and_moves_cursor() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        assert!(app.dialog.is_some());
        type_str(&mut app, "새폴더");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(app.dialog.is_none());
        assert!(tmp.path().join("새폴더").is_dir());
        settle(&mut app);
        assert_eq!(name_at(&app, 0), "새폴더");
        assert_eq!(app.status.as_deref(), Some("폴더 생성: 새폴더"));
    }

    #[test]
    fn f7_esc_cancels() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "zz");
        press(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
        assert!(!tmp.path().join("zz").exists());
        assert_eq!(app.status, None);
    }

    #[test]
    fn f7_mkdir_runs_on_worker_then_focuses_new_dir() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "새폴더");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("폴더 생성 중…"));
        assert!(app.task.is_some());
        wait_idle(&mut app);
        settle(&mut app);
        assert!(tmp.path().join("새폴더").is_dir());
        assert_eq!(app.status.as_deref(), Some("폴더 생성: 새폴더"));
        assert_eq!(app.undo.len(), 1);
        assert_eq!(app.active_panel().current().unwrap().os_name, "새폴더");
    }

    #[test]
    fn f2_rename_runs_on_worker() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::F(2));
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "z.txt");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("이름 변경 중…"));
        assert!(app.task.is_some());
        wait_idle(&mut app);
        settle(&mut app);
        assert!(tmp.path().join("z.txt").exists());
        assert_eq!(app.undo.len(), 1);
        assert_eq!(app.status.as_deref(), Some("이름 변경: z.txt"));
    }

    #[test]
    fn mkdir_is_refused_while_another_task_runs() {
        let (tmp, mut app, _f) = fixture_trash();
        let (_tx, rx) = std::sync::mpsc::channel();
        app.task = Some(BgTask {
            rx,
            kind: BgKind::Undo,
        });
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "nope");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert!(!tmp.path().join("nope").exists());
    }

    #[test]
    fn f7_invalid_name_sets_error() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "a/b");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(app.dialog.is_none());
        assert!(app.status.as_deref().unwrap().starts_with("오류:"));
    }

    #[test]
    fn f2_renames_file() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::End); // g.txt
        press(&mut app, KeyCode::Up); // f.txt
        press(&mut app, KeyCode::F(2));
        match &app.dialog.as_ref().unwrap().0 {
            crate::dialog::Dialog::Input { value, .. } => assert_eq!(value, "f.txt"),
            d => panic!("{d:?}"),
        }
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "h.txt");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(!tmp.path().join("f.txt").exists());
        assert!(tmp.path().join("h.txt").exists());
        settle(&mut app);
        assert_eq!(name_at(&app, 0), "h.txt");
    }

    #[test]
    fn shift_f6_also_renames() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::Down);
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::F(6),
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ));
        assert!(app.dialog.is_some());
    }

    #[test]
    fn f2_on_parent_does_nothing() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(2));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn f2_rename_to_existing_errors() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::End); // g.txt
        press(&mut app, KeyCode::F(2));
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "f.txt");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(app.status.as_deref().unwrap().starts_with("오류:"));
        assert!(tmp.path().join("g.txt").exists());
    }

    #[test]
    fn f8_trashes_selection_after_confirm() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Char(' ')); // on .. : no-op select, moves to adir
        press(&mut app, KeyCode::Char(' ')); // adir
        press(&mut app, KeyCode::Char(' ')); // f.txt
        press(&mut app, KeyCode::F(8));
        assert!(app.dialog.is_some());
        press(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        wait_idle(&mut app);
        let got = fake.trashed.lock().unwrap().clone();
        assert_eq!(got, vec![root.join("adir"), root.join("f.txt")]);
        assert!(app.panels[0].selected.is_empty());
        assert_eq!(app.status.as_deref(), Some("휴지통: 2개"));
        let _ = tmp;
    }

    #[test]
    fn f8_cancel_with_n() {
        let (_t, mut app, fake) = fixture_trash();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Char('n'));
        assert!(app.dialog.is_none());
        assert!(fake.trashed.lock().unwrap().is_empty());
    }

    #[test]
    fn f8_on_parent_no_dialog() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(8));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn dialog_captures_keys() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.running);
        assert_eq!(app.panels[0].cursor, 0);
        assert!(app.dialog.is_some());
    }

    #[test]
    fn other_panel_same_cwd_reloads_after_mkdir() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "nd");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        settle(&mut app);
        assert!(app.panels[1].entries.iter().any(|e| e.name == "nd"));
    }

    #[test]
    fn draw_with_dialog_shows_title() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                text.push_str(sym);
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
        }
        assert!(text.contains("새 폴더"));
    }
    // ---- HDIR-10: undo ----

    fn ctrl_z(app: &mut App) {
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ));
    }

    #[test]
    fn ctrl_z_undoes_f7_mkdir_and_reloads() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "새폴더");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(tmp.path().join("새폴더").is_dir());
        settle(&mut app);
        assert!(app.panels[0].entries.iter().any(|e| e.name == "새폴더"));
        assert!(app.panels[1].entries.iter().any(|e| e.name == "새폴더"));
        ctrl_z(&mut app);
        wait_idle(&mut app);
        settle(&mut app);
        assert!(!tmp.path().join("새폴더").exists());
        assert!(app.panels[0].entries.iter().all(|e| e.name != "새폴더"));
        assert!(app.panels[1].entries.iter().all(|e| e.name != "새폴더"));
        assert_eq!(app.status.as_deref(), Some("되돌림: 폴더 생성 취소 새폴더"));
    }

    #[test]
    fn ctrl_z_undoes_f2_rename() {
        let (tmp, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Up); // f.txt
        press(&mut app, KeyCode::F(2));
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "h.txt");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        settle(&mut app);
        for i in 0..2 {
            assert!(app.panels[i].entries.iter().any(|e| e.name == "h.txt"));
        }
        ctrl_z(&mut app);
        wait_idle(&mut app);
        settle(&mut app);
        assert!(tmp.path().join("f.txt").exists());
        assert!(!tmp.path().join("h.txt").exists());
        for i in 0..2 {
            assert!(app.panels[i].entries.iter().any(|e| e.name == "f.txt"));
            assert!(app.panels[i].entries.iter().all(|e| e.name != "h.txt"));
        }
    }

    #[test]
    fn ctrl_z_empty_stack_status() {
        let (_t, mut app, _f) = fixture_trash();
        ctrl_z(&mut app);
        wait_idle(&mut app);
        assert_eq!(app.status.as_deref(), Some("되돌릴 작업 없음"));
    }

    // ---- HDIR-9: copy / move jobs ----

    /// left = src dir (a.txt, b.txt), right = empty dst dir.
    fn fixture_job() -> (TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&dst).unwrap();
        fs::write(src.join("a.txt"), "A").unwrap();
        fs::write(src.join("b.txt"), "B").unwrap();
        let app = App::new(src, dst);
        (tmp, app)
    }

    fn wait_job(app: &mut App) {
        let start = std::time::Instant::now();
        while app.job.is_some() {
            app.tick();
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "job timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Tick until neither a copy/move job nor a background task is running.
    fn wait_idle(app: &mut App) {
        let start = std::time::Instant::now();
        while app.job.is_some() || app.task.is_some() {
            app.tick();
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "idle timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn wait_dialog(app: &mut App) {
        let start = std::time::Instant::now();
        while app.dialog.is_none() {
            app.tick();
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "dialog timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn select_down(app: &mut App) {
        press(app, KeyCode::Down); // a.txt
    }

    #[test]
    fn f5_copies_to_other_panel() {
        let (tmp, mut app) = fixture_job();
        select_down(&mut app);
        press(&mut app, KeyCode::F(5));
        match &app.dialog.as_ref().unwrap().0 {
            Dialog::Input { value, .. } => {
                assert_eq!(value, &app.panels[1].cwd.to_string_lossy())
            }
            d => panic!("{d:?}"),
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.job.is_some());
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "A"
        );
        assert!(tmp.path().join("src/a.txt").exists());
        assert!(app.status.as_deref().unwrap().starts_with("복사 완료"));
        settle(&mut app);
        assert!(app.panels[1].entries.iter().any(|e| e.name == "a.txt"));
    }

    #[test]
    fn f5_on_parent_is_noop() {
        let (_t, mut app) = fixture_job();
        press(&mut app, KeyCode::F(5));
        assert!(app.dialog.is_none());
        press(&mut app, KeyCode::F(6));
        assert!(app.dialog.is_none());
    }

    #[test]
    fn f6_moves_and_undo_moves_back() {
        let (tmp, mut app) = fixture_job();
        select_down(&mut app);
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Enter);
        wait_job(&mut app);
        assert!(!tmp.path().join("src/a.txt").exists());
        assert!(tmp.path().join("dst/a.txt").exists());
        assert!(app.status.as_deref().unwrap().starts_with("이동 완료"));
        assert_eq!(app.undo.len(), 1);
        ctrl_z(&mut app);
        wait_idle(&mut app);
        assert!(tmp.path().join("src/a.txt").exists());
        assert!(!tmp.path().join("dst/a.txt").exists());
    }

    #[test]
    fn copy_selection_clears_selection() {
        let (_t, mut app) = fixture_job();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.active_panel().selected.len(), 2);
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Enter);
        wait_job(&mut app);
        assert!(app.panels[0].selected.is_empty());
        assert!(app.status.as_deref().unwrap().contains("2개"));
    }

    #[test]
    fn partial_copy_keeps_skipped_source_selected() {
        let (tmp, mut app) = fixture_job();
        fs::write(tmp.path().join("dst/b.txt"), "OLD").unwrap();
        app.panels[1].reload();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.active_panel().selected.len(), 2);
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Enter);
        wait_dialog(&mut app); // b.txt conflicts
        press(&mut app, KeyCode::Char('s'));
        wait_job(&mut app);
        let sel = &app.panels[0].selected;
        assert!(!sel.contains(OsStr::new("a.txt")));
        assert!(sel.contains(OsStr::new("b.txt")), "{sel:?}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/b.txt")).unwrap(),
            "OLD"
        );
    }

    fn start_conflict(app: &mut App, tmp: &TempDir) {
        fs::write(tmp.path().join("dst/a.txt"), "OLD").unwrap();
        app.panels[1].reload();
        select_down(app);
        press(app, KeyCode::F(5));
        press(app, KeyCode::Enter);
        wait_dialog(app);
    }

    #[test]
    fn conflict_dialog_skip_keeps_dest() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        match &app.dialog.as_ref().unwrap().0 {
            Dialog::Choice {
                title,
                message,
                options,
                ..
            } => {
                assert_eq!(title, "덮어쓰기 확인");
                assert_eq!(message, &vec!["이미 존재: a.txt".to_string()]);
                assert_eq!(options.len(), 5);
            }
            d => panic!("{d:?}"),
        }
        press(&mut app, KeyCode::Char('s'));
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn conflict_dialog_overwrite_replaces() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        press(&mut app, KeyCode::Char('o'));
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "A"
        );
    }

    #[test]
    fn overwrite_all_asks_only_once() {
        let (tmp, mut app) = fixture_job();
        fs::write(tmp.path().join("dst/a.txt"), "OLD").unwrap();
        fs::write(tmp.path().join("dst/b.txt"), "OLD").unwrap();
        app.panels[1].reload();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Enter);
        wait_dialog(&mut app);
        press(&mut app, KeyCode::Right); // default Skip -> Overwrite all
        press(&mut app, KeyCode::Enter);
        let mut dialogs = 0;
        let start = std::time::Instant::now();
        while app.job.is_some() {
            app.tick();
            if app.dialog.is_some() {
                dialogs += 1;
                press(&mut app, KeyCode::Esc);
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(dialogs, 0);
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "A"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/b.txt")).unwrap(),
            "B"
        );
    }

    #[test]
    fn esc_in_conflict_dialog_cancels() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        press(&mut app, KeyCode::Esc);
        wait_job(&mut app);
        assert_eq!(app.status.as_deref(), Some("취소됨 (처리 0개)"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn invalid_dest_sets_error_and_no_job() {
        let (tmp, mut app) = fixture_job();
        select_down(&mut app);
        press(&mut app, KeyCode::F(5));
        for _ in 0..500 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "nope");
        press(&mut app, KeyCode::Enter);
        // Validated on the worker, not the UI thread.
        assert!(app.job.is_some());
        wait_job(&mut app);
        assert!(app.status.as_deref().unwrap().contains("대상 폴더 없음"));
        assert!(app.status.as_deref().unwrap().starts_with("오류:"));
        assert!(!tmp.path().join("src/nope").exists());
        assert!(!tmp.path().join("dst/a.txt").exists());
    }

    #[test]
    fn relative_dest_resolves_against_active_cwd() {
        let (tmp, mut app) = fixture_job();
        fs::create_dir(tmp.path().join("src/sub")).unwrap();
        app.panels[0].reload();
        press(&mut app, KeyCode::End); // b.txt is last? sub sorts first
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down); // a.txt (dirs first: sub, a.txt)
        assert_eq!(name_at(&app, 0), "a.txt");
        press(&mut app, KeyCode::F(5));
        for _ in 0..500 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "sub");
        press(&mut app, KeyCode::Enter);
        wait_job(&mut app);
        assert!(tmp.path().join("src/sub/a.txt").exists());
    }

    #[test]
    fn second_f5_while_running_reports_busy() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        app.dialog = None; // job stays blocked on the conflict
        press(&mut app, KeyCode::F(5));
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert!(app.dialog.is_none());
        press(&mut app, KeyCode::F(6));
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        // Esc with no dialog cancels the running job.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.status.as_deref(), Some("취소 중…"));
        wait_job(&mut app);
        assert_eq!(app.status.as_deref(), Some("취소됨 (처리 0개)"));
    }

    #[test]
    fn panel_keys_work_while_job_runs() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        app.dialog = None;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[0].cursor, 2);
        press(&mut app, KeyCode::Esc);
        wait_job(&mut app);
    }

    #[test]
    fn tick_is_false_when_idle() {
        let (_t, mut app) = fixture_job();
        assert!(!app.tick());
        assert!(!app.tick());
    }

    // ---- async directory loading ----

    use std::time::{Duration, Instant};

    /// Tick until no panel is loading (5s timeout).
    fn settle(app: &mut App) {
        let start = Instant::now();
        while app.panels.iter().any(|p| p.loading.is_some()) {
            assert!(start.elapsed() < Duration::from_secs(5), "load timed out");
            app.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Replace the loader with one that blocks until the returned mutex guard
    /// is dropped by the test.
    fn gate_loader(app: &mut App) -> Arc<Mutex<()>> {
        let gate = Arc::new(Mutex::new(()));
        let g = Arc::clone(&gate);
        app.loader = crate::loader::Loader::with_reader(Arc::new(move |dir, hidden| {
            drop(g.lock().unwrap());
            crate::panel::read_listing(dir, hidden)
        }));
        gate
    }

    #[test]
    fn enter_dir_is_async_cwd_changes_after_tick() {
        let (tmp, mut app) = fixture();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.panels[0].cwd, root);
        assert!(app.panels[0].loading.is_some());
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root.join("adir"));
        assert_eq!(name_at(&app, 0), "..");
        assert!(app.panels[0].loading.is_none());
        let _ = tmp;
    }

    #[test]
    fn backspace_restores_focus_through_async_path() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down); // bdir
        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        press(&mut app, KeyCode::Backspace);
        assert!(app.panels[0].loading.is_some());
        settle(&mut app);
        assert_eq!(name_at(&app, 0), "bdir");
    }

    #[test]
    fn other_panel_and_old_listing_usable_while_loading() {
        let (_t, mut app) = fixture();
        let gate = gate_loader(&mut app);
        let guard = gate.lock().unwrap();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.panels[0].loading.is_some());
        assert_eq!(app.status, None);
        // Old listing still navigable.
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[0].cursor, 2);
        // Other panel fully usable.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.panels[1].cursor, 2);
        assert!(app.panels[1].loading.is_none());
        assert!(!app.tick());
        assert_eq!(app.panels[0].cwd, root);
        drop(guard);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root.join("adir"));
        assert_eq!(app.panels[1].cwd, root);
    }

    #[test]
    fn second_enter_supersedes_first() {
        let (_t, mut app) = fixture();
        let gate = gate_loader(&mut app);
        let guard = gate.lock().unwrap();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Enter);
        let first = app.panels[0].loading.as_ref().unwrap().id;
        press(&mut app, KeyCode::Down); // bdir
        press(&mut app, KeyCode::Enter);
        let second = app.panels[0].loading.as_ref().unwrap().id;
        assert_ne!(first, second);
        drop(guard);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root.join("bdir"));
    }

    #[test]
    fn toggle_hidden_is_async() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Char('.'));
        assert!(app.panels[0].loading.is_some());
        settle(&mut app);
        assert!(app.panels[0].entries.iter().any(|e| e.name == ".h"));
    }

    #[test]
    fn load_failure_shows_error_status_and_keeps_cwd() {
        let (tmp, mut app) = fixture();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down);
        let gate = gate_loader(&mut app);
        let guard = gate.lock().unwrap();
        press(&mut app, KeyCode::Enter);
        fs::remove_dir_all(tmp.path().join("adir")).unwrap();
        drop(guard);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, root);
        assert!(app.status.as_deref().is_some_and(|s| !s.is_empty()));
    }

    // ---- review fixes (phase A) ----

    #[test]
    fn conflict_dialog_defaults_to_skip() {
        let (tmp, mut app) = fixture_job();
        start_conflict(&mut app, &tmp);
        match &app.dialog.as_ref().unwrap().0 {
            Dialog::Choice { selected, .. } => assert_eq!(*selected, 1),
            d => panic!("{d:?}"),
        }
        press(&mut app, KeyCode::Enter); // bare Enter must be Skip
        wait_job(&mut app);
        assert_eq!(
            fs::read_to_string(tmp.path().join("dst/a.txt")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn conflict_event_does_not_replace_open_dialog() {
        let (tmp, mut app) = fixture_job();
        fs::write(tmp.path().join("dst/a.txt"), "OLD").unwrap();
        app.panels[1].reload();
        select_down(&mut app);
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(7)); // mkdir dialog while job runs
        let start = Instant::now();
        while app.pending_conflict.is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "conflict event never arrived"
            );
            app.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(matches!(
            app.dialog.as_ref().unwrap().0,
            Dialog::Input { .. }
        ));
        press(&mut app, KeyCode::Esc);
        match &app.dialog.as_ref().expect("conflict dialog queued").0 {
            Dialog::Choice { selected, .. } => assert_eq!(*selected, 1),
            d => panic!("{d:?}"),
        }
        press(&mut app, KeyCode::Esc);
        wait_job(&mut app);
    }

    #[cfg(unix)]
    #[test]
    fn dest_with_surrounding_spaces_is_not_trimmed() {
        let (tmp, mut app) = fixture_job();
        let odd = tmp.path().join(" sp ");
        fs::create_dir(&odd).unwrap();
        select_down(&mut app);
        press(&mut app, KeyCode::F(5));
        for _ in 0..500 {
            press(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, &odd.to_string_lossy());
        press(&mut app, KeyCode::Enter);
        assert!(app.job.is_some(), "status: {:?}", app.status);
        wait_job(&mut app);
        assert!(odd.join("a.txt").exists());
    }

    #[test]
    fn cancelled_move_keeps_partial_moves_undoable() {
        let (tmp, mut app) = fixture_job();
        fs::write(tmp.path().join("dst/b.txt"), "OLD").unwrap();
        app.panels[1].reload();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Enter);
        wait_dialog(&mut app); // a.txt moved, b.txt conflicts
        press(&mut app, KeyCode::Esc);
        wait_job(&mut app);
        assert!(tmp.path().join("dst/a.txt").exists());
        assert_eq!(app.undo.len(), 1);
        assert_eq!(app.status.as_deref(), Some("취소됨 (처리 1개)"));
        ctrl_z(&mut app);
        wait_idle(&mut app);
        assert!(tmp.path().join("src/a.txt").exists());
    }

    #[test]
    fn job_summary_reports_partial_moves() {
        let mut r = OpReport::failed("");
        r.outcome = OpOutcome::Done;
        r.partial = 1;
        let s = job_summary(JobKind::Move, &r);
        assert!(s.contains("일부만 이동 1개"), "{s}");
        r.partial = 0;
        assert!(!job_summary(JobKind::Move, &r).contains("일부만"));
    }

    // ---- round 3 part B: worker trash / undo, source panel, warnings ----

    /// Blocks until `released` is set (2s safety timeout so a regression fails
    /// instead of hanging).
    fn block_until(released: &std::sync::atomic::AtomicBool) {
        let start = Instant::now();
        while !released.load(std::sync::atomic::Ordering::SeqCst) {
            if start.elapsed() > Duration::from_secs(2) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn f8_trash_runs_on_worker_and_ui_stays_responsive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        for n in ["f.txt", "g.txt"] {
            fs::write(tmp.path().join(n), "x").unwrap();
        }
        let released = Arc::new(AtomicBool::new(false));
        let store = Arc::new(FakeTrash::new());
        *store.gate.lock().unwrap() = Some(Arc::clone(&released));
        let mut app = App::with_trash(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            store.clone(),
        );
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' ')); // f.txt selected, cursor g.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(app.status.as_deref(), Some("휴지통으로 이동 중…"));
        // UI keys still work while the worker is blocked.
        press(&mut app, KeyCode::Up);
        assert_eq!(app.panels[0].cursor, 1);
        assert_eq!(app.status.as_deref(), Some("휴지통으로 이동 중…"));
        press(&mut app, KeyCode::F(5));
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert!(app.dialog.is_none());
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert!(store.trashed.lock().unwrap().is_empty());
        released.store(true, Ordering::SeqCst);
        wait_idle(&mut app);
        assert_eq!(app.status.as_deref(), Some("휴지통: 1개"));
        assert_eq!(store.trashed.lock().unwrap().len(), 1);
        assert!(app.panels[0].selected.is_empty());
        assert_eq!(app.undo.len(), 1);
    }

    #[test]
    fn task_finish_does_not_clobber_in_flight_navigation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("adir")).unwrap();
        fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let trash = Arc::new(FakeTrash::new());
        *trash.gate.lock().unwrap() = Some(Arc::clone(&released));
        let root = tmp.path().to_path_buf();
        let mut app = App::with_trash(root.clone(), root.clone(), trash);
        // Entries: .., adir, f.txt. Trash f.txt in the background.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        assert!(app.task.is_some());
        // Meanwhile start a slow Enter on adir.
        let gate = gate_loader(&mut app);
        let guard = gate.lock().unwrap();
        press(&mut app, KeyCode::Up); // adir
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.panels[0].loading.as_ref().map(|r| r.kind),
            Some(LoadKind::Enter)
        );
        released.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while app.task.is_some() {
            assert!(start.elapsed() < Duration::from_secs(5), "task stuck");
            app.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            app.panels[0].loading.as_ref().map(|r| r.kind),
            Some(LoadKind::Enter),
            "task finish replaced the pending navigation"
        );
        drop(guard);
        settle(&mut app);
        assert_eq!(
            app.panels[0].cwd.canonicalize().unwrap(),
            root.join("adir").canonicalize().unwrap()
        );
    }

    #[test]
    fn task_finish_during_navigation_reloads_after_the_load() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("adir")).unwrap();
        fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let trash = Arc::new(FakeTrash::new());
        *trash.gate.lock().unwrap() = Some(Arc::clone(&released));
        let root = tmp.path().to_path_buf();
        let mut app = App::with_trash(root.clone(), root.clone(), trash);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        assert!(app.task.is_some());
        // Loader that lists first, then blocks: the listing predates the task's end.
        let listed = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Mutex::new(()));
        let (l, g) = (Arc::clone(&listed), Arc::clone(&gate));
        app.loader = crate::loader::Loader::with_reader(Arc::new(move |dir, hidden| {
            let res = crate::panel::read_listing(dir, hidden);
            l.store(true, Ordering::SeqCst);
            drop(g.lock().unwrap());
            res
        }));
        let guard = gate.lock().unwrap();
        press(&mut app, KeyCode::Up); // adir
        press(&mut app, KeyCode::Enter);
        let start = Instant::now();
        while !listed.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(5), "reader stuck");
            std::thread::sleep(Duration::from_millis(2));
        }
        released.store(true, Ordering::SeqCst);
        while app.task.is_some() {
            assert!(start.elapsed() < Duration::from_secs(5), "task stuck");
            app.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
        // The task's side effect lands after the navigation listing was read.
        fs::write(root.join("adir/new.txt"), "n").unwrap();
        drop(guard);
        settle(&mut app);
        assert_eq!(
            app.panels[0].cwd.canonicalize().unwrap(),
            root.join("adir").canonicalize().unwrap()
        );
        assert!(
            app.panels[0].entries.iter().any(|e| e.os_name == "new.txt"),
            "post-task change missing after navigation"
        );
    }

    #[test]
    fn f8_total_failure_is_reported_without_undo_entry() {
        let failing = Arc::new(FakeTrash::new());
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f.txt"), "x").unwrap();
        let mut app = App::with_trash(
            tmp.path().to_path_buf(),
            tmp.path().to_path_buf(),
            failing.clone(),
        );
        let key = app.panels[0].cwd.join("f.txt");
        failing
            .fail_paths
            .lock()
            .unwrap()
            .insert(key, "boom".into());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert_eq!(app.status.as_deref(), Some("오류: boom"));
        assert_eq!(app.undo.len(), 0);
    }

    #[test]
    fn f8_partial_failure_reports_counts_and_keeps_successes_undoable() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        let _ = &tmp;
        fake.fail_paths
            .lock()
            .unwrap()
            .insert(root.join("g.txt"), "denied".into());
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Down); // f.txt
        press(&mut app, KeyCode::Char(' ')); // select f.txt, cursor g.txt
        press(&mut app, KeyCode::Char(' ')); // select g.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert_eq!(app.status.as_deref(), Some("휴지통: 1개, 실패 1개: denied"));
        assert!(!root.join("f.txt").exists() && root.join("g.txt").exists());
        assert_eq!(app.undo.len(), 1);
        assert!(
            !app.panels[0].selected.contains(OsStr::new("f.txt")),
            "trashed one deselected"
        );
        assert!(app.panels[0].selected.contains(OsStr::new("g.txt")));
    }

    fn gated_undo_runner(released: Arc<std::sync::atomic::AtomicBool>) -> UndoRunner {
        Arc::new(move |op| {
            block_until(&released);
            UndoStack::apply(op)
        })
    }

    #[test]
    fn ctrl_z_runs_on_worker_second_ctrl_z_reports_busy() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tmp, mut app, _f) = fixture_trash();
        let dir = tmp.path().join("made");
        fs::create_dir(&dir).unwrap();
        app.undo.push(UndoOp::Mkdir { path: dir.clone() });
        let released = Arc::new(AtomicBool::new(false));
        app.undo_runner = gated_undo_runner(Arc::clone(&released));
        ctrl_z(&mut app);
        assert_eq!(app.status.as_deref(), Some("되돌리는 중…"));
        assert!(dir.exists(), "not reversed yet");
        assert_eq!(app.undo.len(), 0);
        press(&mut app, KeyCode::Down); // UI still responsive
        assert_eq!(app.panels[0].cursor, 1);
        ctrl_z(&mut app);
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        released.store(true, Ordering::SeqCst);
        wait_idle(&mut app);
        settle(&mut app);
        assert_eq!(app.status.as_deref(), Some("되돌림: 폴더 생성 취소 made"));
        assert!(!dir.exists());
    }

    #[test]
    fn failed_undo_pushes_retry_back_after_completion() {
        let (tmp, mut app, _f) = fixture_trash();
        let (o, n) = (tmp.path().join("o"), tmp.path().join("n"));
        fs::write(&n, "2").unwrap();
        fs::write(&o, "blocker").unwrap();
        app.undo.push(UndoOp::Move {
            pairs: vec![(o, n)],
            removed_dirs: vec![],
        });
        ctrl_z(&mut app);
        assert_eq!(app.undo.len(), 0);
        wait_idle(&mut app);
        assert_eq!(app.undo.len(), 1, "failed part kept for retry");
        assert!(app.status.as_deref().unwrap().contains("일부 실패"));
    }

    #[test]
    fn finished_job_clears_selection_of_its_source_panel_only() {
        let (tmp, mut app) = fixture_job();
        fs::write(tmp.path().join("dst/a.txt"), "OLD").unwrap();
        app.panels[1].reload();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' ')); // left: a.txt selected
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Down); // a.txt
        press(&mut app, KeyCode::F(5));
        press(&mut app, KeyCode::Enter);
        wait_dialog(&mut app); // blocked on conflict
        app.dialog = None;
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Down); // right: a.txt
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panels[1].selected.contains(OsStr::new("a.txt")));
        app.job
            .as_ref()
            .unwrap()
            .handle
            .answer(ConflictAnswer::OverwriteAll);
        wait_job(&mut app);
        assert!(app.panels[0].selected.is_empty(), "source panel cleared");
        assert!(
            app.panels[1].selected.contains(OsStr::new("a.txt")),
            "other panel's selection kept"
        );
    }

    #[test]
    fn job_summary_reports_warnings() {
        let mut r = OpReport::failed("");
        r.outcome = OpOutcome::Done;
        assert!(!job_summary(JobKind::Move, &r).contains("경고"));
        r.warnings = vec!["x: 빈 원본 폴더 삭제 실패".into(), "y".into()];
        let s = job_summary(JobKind::Move, &r);
        assert!(s.contains("경고 2개"), "{s}");
        assert!(s.contains("x: 빈 원본 폴더 삭제 실패"), "{s}");
    }

    // ---- OS trash: Alt+F8 view (Linux), Finder (macOS), undo ----

    use std::sync::atomic::Ordering;

    fn alt_f8(app: &mut App) {
        app.handle_key(KeyEvent::new_with_kind(
            KeyCode::F(8),
            KeyModifiers::ALT,
            KeyEventKind::Press,
        ));
    }

    /// Alt+F8 and wait for the listing.
    fn enter_trash(app: &mut App) {
        alt_f8(app);
        wait_idle(app);
    }

    fn trash_files(fake: &Fake, paths: &[PathBuf]) {
        for (_, r) in fake.delete(paths) {
            r.unwrap();
        }
    }

    fn view_names(app: &App, idx: usize) -> Vec<String> {
        app.trash_views[idx]
            .as_ref()
            .map(|v| v.entries.iter().map(|e| e.name()).collect())
            .unwrap_or_default()
    }

    fn dialog_title(app: &App) -> String {
        match &app.dialog {
            Some((
                Dialog::Input { title, .. }
                | Dialog::Confirm { title, .. }
                | Dialog::Choice { title, .. },
                _,
            )) => title.clone(),
            None => String::new(),
        }
    }

    fn screen_text(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // Skip the filler cell that follows each wide character.
        let mut out = String::new();
        let mut skip = false;
        for c in terminal.backend().buffer().content() {
            if std::mem::take(&mut skip) {
                continue;
            }
            out.push_str(c.symbol());
            skip = unicode_width::UnicodeWidthStr::width(c.symbol()) > 1;
        }
        out
    }

    #[test]
    fn alt_f8_enters_trash_view_loading_on_worker() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        trash_files(&fake, &[root.join("g.txt")]);
        alt_f8(&mut app);
        let view = app.trash_views[0].as_ref().expect("view opened at once");
        assert!(view.loading);
        assert!(app.trash_views[1].is_none());
        assert_eq!(app.status.as_deref(), Some("휴지통 읽는 중…"));
        wait_idle(&mut app);
        assert!(!app.trash_views[0].as_ref().unwrap().loading);
        assert_eq!(view_names(&app, 0), ["g.txt", "f.txt"], "newest first");
        assert_eq!(app.trash_views[0].as_ref().unwrap().prev_cwd, root);
        let text = screen_text(&mut app);
        assert!(text.contains("휴지통"));
        assert!(text.contains("g.txt") && text.contains("f.txt"));
        assert!(text.contains("Restore") && text.contains("Purge"));
        let _ = tmp;
    }

    #[test]
    fn trash_view_keys_move_cursor_and_space_selects() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for n in ["f.txt", "g.txt", "adir"] {
            trash_files(&fake, &[root.join(n)]);
        }
        enter_trash(&mut app);
        screen_text(&mut app); // sets the page height
        let cursor = |app: &App| app.trash_views[0].as_ref().unwrap().cursor;
        press(&mut app, KeyCode::Down);
        assert_eq!(cursor(&app), 1);
        press(&mut app, KeyCode::End);
        assert_eq!(cursor(&app), 2);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(cursor(&app), 0);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(cursor(&app), 2);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(cursor(&app), 1);
        assert_eq!(app.trash_views[0].as_ref().unwrap().selected.len(), 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(cursor(&app), 0);
        assert_eq!(app.panels[0].cursor, 0, "normal panel untouched");
    }

    #[test]
    fn enter_restores_current_entry_through_the_backend() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        enter_trash(&mut app);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("복원 중…"));
        wait_idle(&mut app);
        assert_eq!(
            *fake.restore_calls.lock().unwrap(),
            vec![vec![root.join("f.txt")]]
        );
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "x");
        assert_eq!(app.status.as_deref(), Some("복원: 1개"));
        assert!(view_names(&app, 0).is_empty(), "list refreshed");
        assert!(app.trash_views[0].is_some(), "still in trash view");
        // the other (normal) panel showing that directory was reloaded
        settle(&mut app);
        assert!(app.panels[1].entries.iter().any(|e| e.name == "f.txt"));
        let _ = tmp;
    }

    #[test]
    fn f5_restores_all_selected_entries() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for n in ["f.txt", "g.txt"] {
            trash_files(&fake, &[root.join(n)]);
        }
        enter_trash(&mut app);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::F(5));
        wait_idle(&mut app);
        assert!(root.join("f.txt").exists() && root.join("g.txt").exists());
        assert_eq!(app.status.as_deref(), Some("복원: 2개"));
        assert!(app.trash_views[0].as_ref().unwrap().selected.is_empty());
    }

    #[test]
    fn restore_skips_entries_changed_since_the_view_listed_them() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        enter_trash(&mut app);
        fake.bump_times();
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(!root.join("f.txt").exists());
        assert!(fake.restore_calls.lock().unwrap().is_empty());
        let st = app.status.clone().unwrap();
        assert!(st.contains("휴지통 항목이 바뀌어 건너뜀"), "{st}");
    }

    #[test]
    fn restore_collision_is_skipped_and_reported_without_overwriting() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        fs::write(root.join("f.txt"), "newer").unwrap();
        enter_trash(&mut app);
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(app.dialog.is_none(), "no rename/destination prompt");
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "newer");
        assert_eq!(
            app.status.as_deref(),
            Some(
                format!(
                    "원래 위치에 같은 이름이 있어 건너뜀: {}",
                    root.join("f.txt").display()
                )
                .as_str()
            )
        );
        assert_eq!(view_names(&app, 0), ["f.txt"], "item stays in the trash");
    }

    #[test]
    fn restore_of_unknown_original_location_is_refused() {
        let (_t, mut app, fake) = fixture_trash();
        fake.add_orphan("lost.txt");
        enter_trash(&mut app);
        press(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "no destination prompt");
        wait_idle(&mut app);
        assert!(fake.restore_calls.lock().unwrap().is_empty());
        let s = app.status.clone().unwrap();
        assert!(s.contains("원래 위치를 알 수 없어"), "{s}");
        assert!(s.contains("lost.txt"), "{s}");
        assert_eq!(view_names(&app, 0), ["lost.txt"]);
    }

    #[test]
    fn restore_with_unknown_items_restores_the_known_ones() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        fake.add_orphan("lost.txt");
        enter_trash(&mut app);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(root.join("f.txt").exists());
        let s = app.status.clone().unwrap();
        assert!(
            s.contains("복원: 1개") && s.contains("원래 위치를 알 수 없어"),
            "{s}"
        );
    }

    #[test]
    fn f8_in_trash_view_purges_after_danger_confirm() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for n in ["f.txt", "g.txt"] {
            trash_files(&fake, &[root.join(n)]);
        }
        enter_trash(&mut app);
        press(&mut app, KeyCode::F(8));
        assert_eq!(dialog_title(&app), "영구 삭제");
        let Some((
            Dialog::Confirm {
                message, danger, ..
            },
            _,
        )) = &app.dialog
        else {
            panic!("expected confirm dialog");
        };
        assert!(*danger);
        assert_eq!(message, &["g.txt"]);
        press(&mut app, KeyCode::Char('n'));
        assert!(app.dialog.is_none());
        assert_eq!(fake.count(), 2, "cancel keeps items");
        assert!(fake.purge_calls.lock().unwrap().is_empty());
        press(&mut app, KeyCode::Char(' ')); // select g.txt
        press(&mut app, KeyCode::Char(' ')); // select f.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("영구 삭제 중…"));
        wait_idle(&mut app);
        assert_eq!(fake.count(), 0);
        assert!(view_names(&app, 0).is_empty());
        assert_eq!(app.status.as_deref(), Some("영구 삭제: 2개"));
        assert!(!root.join("f.txt").exists());
    }

    #[test]
    fn purge_only_touches_the_selected_items() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for n in ["f.txt", "g.txt"] {
            trash_files(&fake, &[root.join(n)]);
        }
        enter_trash(&mut app);
        press(&mut app, KeyCode::Char(' ')); // g.txt only
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert_eq!(fake.names(), ["f.txt"]);
        assert_eq!(fake.purge_calls.lock().unwrap().len(), 1);
        assert_eq!(fake.purge_calls.lock().unwrap()[0].len(), 1);
    }

    #[test]
    fn purge_matches_a_fresh_listing_and_skips_vanished_items() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        enter_trash(&mut app);
        let stale = app.trash_views[0].as_ref().unwrap().entries[0].clone();
        // Behind the view's back the item is purged and a new one with the
        // same name takes its place (different id).
        fake.purge(std::slice::from_ref(&stale)).unwrap();
        fake.purge_calls.lock().unwrap().clear();
        fs::write(root.join("f.txt"), "newer").unwrap();
        trash_files(&fake, &[root.join("f.txt")]);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert_eq!(fake.count(), 1, "new item survives");
        assert!(
            fake.purge_calls.lock().unwrap().is_empty(),
            "nothing left to purge, so no backend call"
        );
        let s = app.status.clone().unwrap();
        assert!(s.contains("건너뜀 1개"), "{s}");
    }

    #[test]
    fn esc_and_backspace_return_to_previous_directory() {
        for key in [KeyCode::Esc, KeyCode::Backspace] {
            let (_t, mut app, _fake) = fixture_trash();
            let root = app.panels[0].cwd.clone();
            enter_trash(&mut app);
            press(&mut app, key);
            assert!(app.trash_views[0].is_none());
            assert_eq!(app.panels[0].cwd, root, "{key:?} did not navigate up");
            assert!(!screen_text(&mut app).contains("Restore"));
        }
    }

    #[test]
    fn other_function_keys_are_disabled_in_trash_view() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        enter_trash(&mut app);
        for n in [1, 2, 3, 4, 6, 7, 9] {
            app.status = None;
            press(&mut app, KeyCode::F(n));
            assert_eq!(
                app.status.as_deref(),
                Some("휴지통에서는 사용할 수 없음"),
                "F{n}"
            );
            assert!(app.dialog.is_none());
        }
        assert!(app.job.is_none() && app.task.is_none());
        alt_f8(&mut app); // already there: no reload, no change
        assert!(app.task.is_none());
    }

    #[test]
    fn f10_quits_from_trash_view_and_tab_switches_panels() {
        let (_t, mut app, _fake) = fixture_trash();
        enter_trash(&mut app);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Down); // normal panel navigation again
        assert_eq!(app.panels[1].cursor, 1);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::F(10));
        assert!(!app.running);
    }

    #[test]
    fn alt_f8_while_task_runs_reports_busy() {
        let (_t, mut app, fake) = fixture_trash();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *fake.gate.lock().unwrap() = Some(Arc::clone(&released));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        alt_f8(&mut app);
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert!(app.trash_views[0].is_none());
        released.store(true, Ordering::SeqCst);
        wait_idle(&mut app);
    }

    #[test]
    fn empty_trash_view_ignores_restore_and_purge() {
        let (_t, mut app, _fake) = fixture_trash();
        enter_trash(&mut app);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(8));
        assert!(app.dialog.is_none() && app.task.is_none());
    }

    #[test]
    fn trash_list_error_is_shown() {
        let (_t, mut app, fake) = fixture_trash();
        *fake.fail_list.lock().unwrap() = Some("휴지통 접근 불가".into());
        enter_trash(&mut app);
        assert_eq!(app.status.as_deref(), Some("오류: 휴지통 접근 불가"));
        let v = app.trash_views[0].as_ref().unwrap();
        assert!(!v.loading);
        assert_eq!(v.error.as_deref(), Some("휴지통 접근 불가"));
    }

    #[test]
    fn purge_in_one_panel_refreshes_the_other_trash_view() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for n in ["f.txt", "g.txt"] {
            trash_files(&fake, &[root.join(n)]);
        }
        enter_trash(&mut app);
        press(&mut app, KeyCode::Tab);
        enter_trash(&mut app);
        press(&mut app, KeyCode::Tab);
        assert_eq!(view_names(&app, 1).len(), 2);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(view_names(&app, 0).is_empty());
        assert!(view_names(&app, 1).is_empty(), "other panel is stale");
    }

    #[test]
    fn restore_in_one_panel_refreshes_the_other_trash_view() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        trash_files(&fake, &[root.join("f.txt")]);
        enter_trash(&mut app);
        press(&mut app, KeyCode::Tab);
        enter_trash(&mut app);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(view_names(&app, 1).is_empty(), "other panel is stale");
    }

    // ---- macOS mode: no in-panel view, Finder does the work ----

    #[test]
    fn mac_alt_f8_opens_the_os_trash_on_a_worker() {
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fake = FakeTrash::mac();
        *fake.open_gate.lock().unwrap() = Some(Arc::clone(&released));
        let (_t, mut app, fake) = fixture_with(fake);
        alt_f8(&mut app);
        assert!(app.task.is_some(), "open runs as a background task");
        assert_eq!(app.status.as_deref(), Some("휴지통 여는 중…"));
        assert_eq!(fake.open_calls.load(Ordering::SeqCst), 0);
        press(&mut app, KeyCode::Down); // UI stays responsive
        released.store(true, Ordering::SeqCst);
        wait_idle(&mut app);
        assert_eq!(fake.open_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.status.as_deref(),
            Some("Finder 휴지통을 열었음 · 복원은 '되돌려 놓기'")
        );
    }

    #[test]
    fn mac_alt_f8_opens_the_os_trash_instead_of_a_view() {
        let (_t, mut app, fake) = fixture_with(FakeTrash::mac());
        alt_f8(&mut app);
        wait_idle(&mut app);
        assert_eq!(fake.open_calls.load(Ordering::SeqCst), 1);
        assert!(app.trash_views[0].is_none());
        assert!(app.task.is_none());
        assert_eq!(fake.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            app.status.as_deref(),
            Some("Finder 휴지통을 열었음 · 복원은 '되돌려 놓기'")
        );
    }

    #[test]
    fn f8_then_ctrl_z_on_mac_opens_the_trash_and_consumes_the_op() {
        let (_t, mut app, fake) = fixture_with(FakeTrash::mac());
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down); // f.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert_eq!(app.undo.len(), 1);
        ctrl_z(&mut app);
        wait_idle(&mut app);
        assert_eq!(fake.open_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.status.as_deref(),
            Some("Finder 휴지통에서 '되돌려 놓기'로 복원하세요")
        );
        assert_eq!(app.undo.len(), 0, "op consumed");
        assert!(!root.join("f.txt").exists(), "hdir restores nothing itself");
        assert!(fake.restore_calls.lock().unwrap().is_empty());
    }

    // ---- Linux mode: Ctrl+Z after F8 ----

    #[test]
    fn f8_then_ctrl_z_restores_the_trashed_items_via_the_backend() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::Char(' ')); // select adir, cursor f.txt
        press(&mut app, KeyCode::Char(' ')); // select f.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Char('y'));
        wait_idle(&mut app);
        assert!(!root.join("f.txt").exists() && !root.join("adir").exists());
        assert_eq!(app.undo.len(), 1);
        assert_eq!(fake.count(), 2);
        ctrl_z(&mut app);
        wait_idle(&mut app);
        assert!(root.join("f.txt").exists() && root.join("adir").is_dir());
        assert_eq!(app.status.as_deref(), Some("되돌림: 휴지통 복원 2개"));
        assert_eq!(fake.count(), 0);
        assert_eq!(fake.open_calls.load(Ordering::SeqCst), 0);
        settle(&mut app);
        let _ = tmp;
    }

    #[test]
    fn f8_without_listing_warns_no_undo_info_and_pushes_no_undo_op() {
        let (_t, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        *fake.fail_list.lock().unwrap() = Some("busy".into());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down); // f.txt
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(!root.join("f.txt").exists(), "still trashed");
        let s = app.status.clone().unwrap();
        assert!(
            s.contains("휴지통: 1개") && s.contains("되돌리기 정보 없음"),
            "{s}"
        );
        assert_eq!(app.undo.len(), 0);
    }

    #[test]
    fn ctrl_z_after_trash_collision_reports_keeps_file_and_retry() {
        let (tmp, mut app, fake) = fixture_trash();
        let root = app.panels[0].cwd.clone();
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::F(8)); // g.txt
        press(&mut app, KeyCode::Char('y'));
        wait_idle(&mut app);
        fs::write(root.join("g.txt"), "new").unwrap();
        ctrl_z(&mut app);
        wait_idle(&mut app);
        let s = app.status.clone().unwrap();
        assert!(
            s.contains("원래 위치에 같은 이름이 있어 건너뜀")
                && s.contains(&root.join("g.txt").to_string_lossy().into_owned()),
            "{s}"
        );
        assert_eq!(fs::read_to_string(root.join("g.txt")).unwrap(), "new");
        assert_eq!(app.undo.len(), 1, "failed item kept for retry");
        assert_eq!(fake.count(), 1);
        let _ = tmp;
    }
}
