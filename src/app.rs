//! Application state and key handling (no terminal IO).

use std::ffi::OsString;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::dialog::{self, Dialog, DialogResult};
use crate::fs_ops::{self, SystemTrash, Trash};
use crate::job::{ConflictAnswer, JobEvent, JobHandle, JobKind, JobOutcome, JobSpec, ProgressView};
use crate::loader::Loader;
use crate::panel::{AsyncEnter, EntryKind, LoadKind, LoadRequest, Panel};
use crate::ui::{self, View};
use crate::undo::{UndoOp, UndoStack};

/// Rows taken by header (1) + status (1) + fkey bar (1) + panel borders (2).
const CHROME_ROWS: u16 = 5;

/// Visible entry rows inside a panel for a terminal of `total_height` rows.
pub fn panel_rows(total_height: u16) -> usize {
    total_height.saturating_sub(CHROME_ROWS) as usize
}

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

pub struct App {
    pub running: bool,
    pub panels: [Panel; 2],
    pub active: usize,
    pub status: Option<String>,
    pub shell: String,
    pub dialog: Option<(Dialog, PendingAction)>,
    pub job: Option<RunningJob>,
    trash: Box<dyn Trash>,
    pub undo: UndoStack,
    /// Background directory reader; tests may swap in a gated one.
    pub loader: Loader,
    next_load_id: u64,
    page_height: usize,
}

impl Default for App {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd.clone(), cwd)
    }
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self::with_trash(left, right, Box::new(SystemTrash))
    }

    pub fn with_trash(left: PathBuf, right: PathBuf, trash: Box<dyn Trash>) -> Self {
        let shell = std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".to_string());
        Self {
            running: true,
            panels: [Panel::new(left), Panel::new(right)],
            active: 0,
            status: None,
            shell,
            dialog: None,
            job: None,
            trash,
            undo: UndoStack::default(),
            loader: Loader::default(),
            next_load_id: 0,
            page_height: 1,
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

    fn open_transfer(&mut self, kind: JobKind) {
        if self.job.is_some() {
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
        if self.job.is_some() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let dest_dir = resolve_dest(input.trim(), &self.active_panel().cwd);
        if !dest_dir.is_dir() {
            self.status = Some("오류: 대상 폴더 없음".into());
            return;
        }
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
        });
    }

    fn next_id(&mut self) -> u64 {
        self.next_load_id += 1;
        self.next_load_id
    }

    /// Hand a panel's pending load request to the background loader.
    fn dispatch(&mut self, idx: usize, req: LoadRequest) {
        let hidden = self.panels[idx].show_hidden;
        self.loader.request(req.id, req.dir, hidden);
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
            }
        }
        changed
    }

    /// Poll background loads and the job; returns true when the screen needs a redraw.
    pub fn tick(&mut self) -> bool {
        let loaded = self.drain_loads();
        self.tick_job() || loaded
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
                    let name = dest
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.dialog = Some((
                        Dialog::choice(
                            "덮어쓰기 확인",
                            vec![format!("이미 존재: {name}")],
                            ["Overwrite", "Skip", "Overwrite all", "Skip all", "Cancel"]
                                .map(String::from)
                                .to_vec(),
                        ),
                        PendingAction::Conflict,
                    ));
                }
                JobEvent::Finished { outcome } => finished = Some(outcome.clone()),
                _ => {}
            }
        }
        match finished {
            Some(outcome) => self.finish_job(outcome),
            None => {
                if !job.cancelling {
                    self.status = Some(job.view.status_line());
                }
            }
        }
        true
    }

    fn finish_job(&mut self, outcome: JobOutcome) {
        let Some(job) = self.job.take() else {
            return;
        };
        let n = job.sources.len();
        let status = match outcome {
            JobOutcome::Done => {
                if job.kind == JobKind::Move {
                    let pairs: Vec<(PathBuf, PathBuf)> = job
                        .sources
                        .iter()
                        .filter_map(|src| {
                            let dest = job.dest_dir.join(src.file_name()?);
                            (src.symlink_metadata().is_err() && dest.symlink_metadata().is_ok())
                                .then(|| (src.clone(), dest))
                        })
                        .collect();
                    if !pairs.is_empty() {
                        self.undo.push(UndoOp::Move { pairs });
                    }
                }
                self.active_panel_mut().selected.clear();
                match job.kind {
                    JobKind::Copy => format!("복사 완료: {n}개"),
                    JobKind::Move => format!("이동 완료: {n}개"),
                }
            }
            JobOutcome::Cancelled => "취소됨".to_string(),
            JobOutcome::Failed(msg) => format!("오류: {msg}"),
        };
        // A conflict dialog may still be open for a job that just ended.
        if matches!(self.dialog, Some((_, PendingAction::Conflict))) {
            self.dialog = None;
        }
        for i in 0..2 {
            self.reload_panel(i, None);
        }
        self.status = Some(status);
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

    fn handle_dialog_key(&mut self, key: KeyEvent) {
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
                let cwd = self.active_panel().cwd.clone();
                match fs_ops::make_dir(&cwd, &name) {
                    Ok(_) => {
                        self.undo.push(UndoOp::Mkdir {
                            path: cwd.join(&name),
                        });
                        self.after_fs_op(Some(&name), format!("폴더 생성: {name}"));
                    }
                    Err(e) => self.status = Some(format!("오류: {e}")),
                }
            }
            (PendingAction::Rename { from }, DialogResult::Submitted(name)) => {
                match fs_ops::rename(&from, &name) {
                    Ok(_) => {
                        let to = from.with_file_name(&name);
                        self.undo.push(UndoOp::Rename { from, to });
                        self.after_fs_op(Some(&name), format!("이름 변경: {name}"));
                    }
                    Err(e) => self.status = Some(format!("오류: {e}")),
                }
            }
            (PendingAction::Trash { paths }, DialogResult::Confirmed) => {
                match self.trash.trash(&paths) {
                    Ok(()) => {
                        let n = paths.len();
                        self.undo.push(UndoOp::Trash { count: n });
                        self.active_panel_mut().selected.clear();
                        self.after_fs_op(None, format!("휴지통: {n}개"));
                    }
                    Err(e) => self.status = Some(format!("오류: {e}")),
                }
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

    /// Ctrl+Z: reverse the latest undoable operation and reload both panels.
    fn undo(&mut self) {
        let status = match self.undo.undo() {
            Ok(msg) => msg,
            Err(e) => e,
        };
        for i in 0..2 {
            self.reload_panel(i, None);
        }
        self.status = Some(status);
    }

    /// Reload the active panel (cursor on `focus` if given) and the other panel
    /// when it shows the same directory, then set the status.
    fn after_fs_op(&mut self, focus: Option<&str>, status: String) {
        let same = self.other_panel().cwd == self.active_panel().cwd;
        if same {
            self.reload_panel(1 - self.active, None);
        }
        self.reload_panel(self.active, focus.map(OsString::from));
        self.status = Some(status);
    }

    /// Run a navigation action on the active panel, clearing stale status and
    /// surfacing any panel error afterwards.
    fn navigate(&mut self, f: impl FnOnce(&mut Panel)) {
        self.status = None;
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
        let view = View {
            panels: [&self.panels[0], &self.panels[1]],
            active: self.active,
            status: self.status.as_deref(),
            shell: &self.shell,
            os: std::env::consts::OS,
        };
        ui::draw(frame, &view);
        if let Some((dlg, _)) = &self.dialog {
            dialog::render(frame, frame.area(), dlg);
        }
    }
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

    #[derive(Clone, Default)]
    struct FakeTrash(Arc<Mutex<Vec<PathBuf>>>);
    impl crate::fs_ops::Trash for FakeTrash {
        fn trash(&self, paths: &[PathBuf]) -> std::io::Result<()> {
            self.0.lock().unwrap().extend(paths.iter().cloned());
            Ok(())
        }
    }

    fn fixture_trash() -> (TempDir, App, FakeTrash) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("adir")).unwrap();
        fs::write(root.join("f.txt"), "x").unwrap();
        fs::write(root.join("g.txt"), "x").unwrap();
        let fake = FakeTrash::default();
        let app = App::with_trash(
            root.to_path_buf(),
            root.to_path_buf(),
            Box::new(fake.clone()),
        );
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
    fn f7_invalid_name_sets_error() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::F(7));
        type_str(&mut app, "a/b");
        press(&mut app, KeyCode::Enter);
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
        let got = fake.0.lock().unwrap().clone();
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
        assert!(fake.0.lock().unwrap().is_empty());
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
        assert!(tmp.path().join("새폴더").is_dir());
        ctrl_z(&mut app);
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
        ctrl_z(&mut app);
        assert!(tmp.path().join("f.txt").exists());
        assert!(!tmp.path().join("h.txt").exists());
        assert!(app.panels[0].entries.iter().any(|e| e.name == "f.txt"));
    }

    #[test]
    fn ctrl_z_after_trash_explains() {
        let (_t, mut app, _f) = fixture_trash();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        press(&mut app, KeyCode::Char('y'));
        ctrl_z(&mut app);
        assert!(app.status.as_deref().unwrap().contains("복원"));
    }

    #[test]
    fn ctrl_z_empty_stack_status() {
        let (_t, mut app, _f) = fixture_trash();
        ctrl_z(&mut app);
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
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Enter); // Overwrite all
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
        assert_eq!(app.status.as_deref(), Some("취소됨"));
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
        assert!(app.job.is_none());
        assert_eq!(app.status.as_deref(), Some("오류: 대상 폴더 없음"));
        assert!(!tmp.path().join("src/nope").exists());
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
        assert_eq!(app.status.as_deref(), Some("취소됨"));
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
}
