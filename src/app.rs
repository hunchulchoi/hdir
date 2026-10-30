//! Application state and key handling (no terminal IO).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::assoc::{self, AssocRule, DefaultAction, RunMode};
use crate::cmdline::{self, CdTarget, CmdEvent, CmdLine, History, LocationCmd};
use crate::config::{self, Mode, Settings};
use crate::dialog::{self, Dialog, DialogResult};
use crate::editor::{self, EditResult, EditorRunner};
use crate::fs_ops::{self};
use crate::fs_ops::{OpOutcome, OpReport};
use crate::job::{ConflictAnswer, JobEvent, JobHandle, JobKind, JobSpec, ProgressView};
use crate::keymap::{Action, Context};
use crate::launcher::{self, SharedLauncher};
use crate::loader::Loader;
use crate::menu::{self, Menu, MenuEvent};
use crate::panel::{AsyncEnter, EntryKind, LoadKind, LoadRequest, Panel};
use crate::platform::os_bytes;
use crate::preview::{self, Preview};
use crate::remote::{self, SshTarget};
use crate::remote_ops::{self, EditCopy};
use crate::screen::{self, TextScreen};
use crate::servers::{self, Server, SshHost};
use crate::session::{self, Cmd, DISCONNECTED_MSG, Established, RemoteSession, SystemCmd};
use crate::theme::{self, Theme};
use crate::trash::{
    self, PurgeReport, RestoreResult, TrackedDelete, TrashBackend, TrashEntry, TrashView,
};
use crate::ui::{self, PreviewBody, PreviewView, View};
use crate::undo::{UndoApplied, UndoOp, UndoStack};
use crate::vfs::Location;
use crate::viewer::{self, Viewer};

pub use crate::ui::panel_rows;

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
    /// Permanent delete on a remote host (no trash there).
    RemoteDelete {
        panel: usize,
        session: Arc<RemoteSession>,
        cwd: PathBuf,
        names: Vec<OsString>,
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

/// Shown for every file operation a remote panel does not support yet.
const REMOTE_UNSUPPORTED: &str = "원격: 다음 단계에서 지원";

/// What a connect worker should establish.
#[derive(Clone)]
enum ConnectKind {
    New {
        server: Server,
        path: Option<String>,
    },
    Reconnect(Arc<RemoteSession>),
}

/// One connect attempt for a panel; `attempt` identifies the latest attempt.
#[derive(Clone)]
struct ConnectJob {
    idx: usize,
    attempt: u64,
    kind: ConnectKind,
    target: SshTarget,
}

enum ConnectDone {
    Established(Established),
    Revived,
}

enum ConnectMsg {
    /// The master is not running: ssh must run on the real terminal.
    NeedAuth(ConnectJob),
    Done {
        idx: usize,
        attempt: u64,
        result: Result<ConnectDone, String>,
    },
}

/// What an open F9 menu is choosing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuKind {
    Main,
    /// Theme names, parallel to the menu items.
    Theme(Vec<String>),
    /// Open-with choices for `path`, parallel to the menu items.
    OpenWith {
        path: PathBuf,
        choices: Vec<OpenChoice>,
        /// Set when `path` is on a remote host.
        session: Option<Arc<RemoteSession>>,
    },
    /// Alt+F1/F2 location picker for `panel`: item 0 is local, then `servers`.
    Location {
        panel: usize,
        servers: Vec<Server>,
    },
}

/// One entry of the open-with menu.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenChoice {
    Rule(AssocRule),
    System,
    Viewer,
}

/// The open popup menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenMenu {
    pub menu: Menu,
    kind: MenuKind,
}

/// Main menu entries (label prefixes are matched by tests).
const MENU_THEME: usize = 0;
const MENU_HIDDEN: usize = 1;
const MENU_KEYS: usize = 2;
const MENU_RELOAD: usize = 3;
const MENU_WARNINGS: usize = 4;
const MENU_OPEN_WITH: usize = 5;

/// Decides off the UI thread whether a file is text (first chunk only).
pub type TextProbe = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;

/// Status shown when Enter lands on a FIFO, socket or device.
const SPECIAL_FILE: &str = remote_ops::SPECIAL_FILE;

/// Reads the first 8 KiB of `path` and classifies it like the preview does.
/// Errors (vanished, unreadable, dangling symlink, special file) carry the
/// user-facing reason; special files are refused before any blocking open.
pub fn probe_text(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    // `metadata` follows symlinks, so a link to a FIFO is refused as well.
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file() {
        return Err(SPECIAL_FILE.into());
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Second guard: never block in open() if the file turns into a FIFO.
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = opts.open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(8 * 1024)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(matches!(
        preview::classify(&buf, meta.len()),
        Preview::Text { .. } | Preview::Empty
    ))
}

/// An association run waiting for the main loop to suspend the TUI.
struct RunJob {
    argv: Vec<OsString>,
    cwd: PathBuf,
    mode: RunMode,
    panel: usize,
    name: OsString,
    /// What the user configured, for error messages.
    label: String,
    /// Downloaded copy of a remote file; removed when the run is over.
    temp: Option<tempfile::TempDir>,
}

/// What to do with a remote file once it is downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchPurpose {
    Edit,
    Rule(AssocRule),
    System,
}

/// An F4 edit of a remote file: the downloaded copy waits here while the
/// editor runs, then decides whether to upload.
struct RemoteEditCtx {
    session: Arc<RemoteSession>,
    remote: PathBuf,
    copy: EditCopy,
}

/// An upload of an edited remote file running on a worker.
struct Upload {
    rx: Receiver<UploadDone>,
    name: String,
    session: Arc<RemoteSession>,
    remote: PathBuf,
    /// The edited local copy (kept whenever the upload does not complete).
    local: PathBuf,
    /// Set when the app quits without waiting any longer: the worker then
    /// keeps the local copy whatever happens.
    abandon: Arc<AtomicBool>,
    /// The editor exited cleanly (shown as "편집 완료 (변경 없음)").
    edited: bool,
    /// What to show when the content turns out unchanged after an editor error.
    editor_status: String,
}

struct UploadDone {
    result: Result<(), String>,
    /// The content did not change: nothing was uploaded.
    unchanged: bool,
    /// Where the local copy was kept after a failure.
    kept: Option<PathBuf>,
}

/// What [`App::finish_uploads`] found at quit time.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UploadReport {
    /// Lines for stderr: where every unsent edit was kept and where it was
    /// meant to go.
    pub notes: Vec<String>,
    /// Some upload did not complete.
    pub failed: bool,
}

/// Uploads an edited copy over a remote file (`local`, `remote`); tests
/// inject slow or failing fakes.
pub type Uploader = Arc<dyn Fn(&RemoteSession, &Path, &Path) -> Result<(), String> + Send + Sync>;

/// A command line command waiting for the main loop to suspend the TUI.
struct ShellJob {
    cmd: String,
    cwd: PathBuf,
    /// Set for a remote panel: the command runs there over `ssh -t`.
    session: Option<Arc<RemoteSession>>,
}

/// An in-flight text-or-not decision for a file without a rule.
struct ProbeLoad {
    rx: Receiver<Result<bool, String>>,
    req: ProbeReq,
}

/// One text-probe request with the UI state it was made in; a result is only
/// applied while `id` is still the newest generation and that state holds.
struct ProbeReq {
    id: u64,
    path: PathBuf,
    active: usize,
    cwd: PathBuf,
    /// The host the file lives on (`None`: local).
    session: Option<Arc<RemoteSession>>,
}

/// Computes one preview off the UI thread; tests may swap in a gated fake.
pub type PreviewRunner = Arc<dyn Fn(&std::path::Path) -> Preview + Send + Sync>;

/// Reads the F3 viewer's file off the UI thread; tests may swap in a gated fake.
pub type ViewerLoader = Arc<dyn Fn(&std::path::Path) -> Preview + Send + Sync>;

/// What the Dev preview shows: a path and, for remote panels, the session
/// that can read it (the same path on another host is a different target).
#[derive(Debug, Clone)]
pub struct PreviewTarget {
    pub path: PathBuf,
    pub session: Option<Arc<RemoteSession>>,
}

impl PartialEq for PreviewTarget {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && match (&self.session, &other.session) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

/// Dev-mode preview bookkeeping: debounce, latest-wins ids, last result.
pub struct PreviewState {
    /// Target under the cursor that the preview should show.
    wanted: Option<PreviewTarget>,
    wanted_at: Instant,
    /// A request for `wanted` has been started (or is done).
    requested: bool,
    /// A worker is running; at most one at a time. The newest wanted path
    /// waits (as the single pending request) until its result arrives.
    in_flight: bool,
    /// Results with another id are stale and dropped.
    latest_id: u64,
    shown: Option<(PreviewTarget, Preview)>,
    tx: Sender<(u64, PreviewTarget, Preview)>,
    rx: Receiver<(u64, PreviewTarget, Preview)>,
}

impl PreviewState {
    fn shown_for_wanted(&self) -> Option<&Preview> {
        match (&self.wanted, &self.shown) {
            (Some(w), Some((p, pv))) if w == p => Some(pv),
            _ => None,
        }
    }

    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            wanted: None,
            wanted_at: Instant::now(),
            requested: false,
            in_flight: false,
            latest_id: 0,
            shown: None,
            tx,
            rx,
        }
    }
}

/// An F4 edit waiting for the main loop to suspend the TUI and run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditJob {
    pub argv: Vec<OsString>,
    pub panel: usize,
    pub name: OsString,
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
    RemoteMkdir {
        panel: usize,
        parent: PathBuf,
        name: String,
        session: Arc<RemoteSession>,
    },
    RemoteRename {
        panel: usize,
        from: PathBuf,
        name: String,
        session: Arc<RemoteSession>,
    },
    RemoteDelete {
        panel: usize,
        cwd: PathBuf,
        paths: Vec<PathBuf>,
    },
    /// Download a remote file to a private temp dir for `purpose`.
    RemoteFetch {
        panel: usize,
        session: Arc<RemoteSession>,
        remote: PathBuf,
        purpose: FetchPurpose,
    },
    /// Resolve a remote `cd` on the server.
    RemoteCd {
        panel: usize,
        session: Arc<RemoteSession>,
    },
    OpenOsTrash,
    /// Re-read config.toml, themes, keymap and assoc.
    ConfigReload,
    /// Write `theme.<mode> = name` to config.toml.
    ThemeSave {
        mode: Mode,
        name: String,
    },
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
    Settings(Box<Settings>),
    ThemeSaved(Result<(), String>),
    /// Restore result, how many selected items had no known original
    /// location, plus the refreshed listing.
    TrashRestore(RestoreResult, usize, TrashListing),
    /// Purge result plus the refreshed listing.
    TrashPurge(PurgeReport, TrashListing),
    Undo(UndoApplied),
    Mkdir(std::io::Result<PathBuf>),
    Rename(std::io::Result<PathBuf>),
    RemoteMkdir(Result<PathBuf, String>),
    RemoteRename(Result<PathBuf, String>),
    RemoteDelete(Result<(), String>),
    RemoteFetch(Result<EditCopy, String>),
    RemoteCd(Result<PathBuf, String>),
}

/// The F3 viewer's own load slot (independent of the shared [`BgTask`]).
/// Closing the viewer or starting another load drops the receiver, so a late
/// result of an abandoned load has nowhere to go.
struct ViewerLoad {
    rx: Receiver<Preview>,
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
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

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
    pub mode: Mode,
    /// Directory holding `config.toml` and `themes/`; `None` = defaults only.
    pub config_dir: Option<PathBuf>,
    /// Config-derived state (keymap, themes, assoc, warnings).
    pub settings: Settings,
    /// Terminal supports 24-bit color; otherwise themes are mapped to 256.
    truecolor: bool,
    /// A full-screen text page (help, key settings, warnings) over the panels.
    pub screen: Option<TextScreen>,
    /// `($VISUAL, $EDITOR)` as read at startup; tests inject values.
    pub editor_vars: (Option<String>, Option<String>),
    /// Runs the editor argv; tests may swap in a fake.
    pub editor_runner: EditorRunner,
    pending_edit: Option<EditJob>,
    /// The downloaded copy behind `pending_edit` when the file is remote.
    pending_remote_edit: Option<RemoteEditCtx>,
    /// Uploads of edited remote files in flight.
    uploads: Vec<Upload>,
    /// Uploads an edited copy; tests may swap in a fake.
    pub uploader: Uploader,
    /// Local copies of edits whose upload failed, for the report at quit.
    failed_copies: Vec<String>,
    /// Downloaded copies a background program may still be reading; they
    /// are removed with the app.
    keep_temp: Vec<tempfile::TempDir>,
    pub preview: PreviewState,
    pub preview_runner: PreviewRunner,
    /// Wait this long after the cursor stops before computing a preview.
    pub preview_debounce: Duration,
    /// The F3 file viewer (full screen).
    pub viewer: Option<Viewer>,
    pub viewer_loader: ViewerLoader,
    viewer_load: Option<ViewerLoad>,
    /// The session the open viewer's file lives on (`None`: a local file).
    viewer_session: Option<Arc<RemoteSession>>,
    /// The F9 popup menu (or theme picker).
    pub menu: Option<OpenMenu>,
    /// Starts association programs; tests inject a fake.
    pub launcher: SharedLauncher,
    /// Text detection for Enter on a file without a rule; tests inject fakes.
    pub text_probe: TextProbe,
    pending_run: Option<RunJob>,
    /// The open `:` command line.
    pub cmdline: Option<CmdLine>,
    pub cmd_history: History,
    pending_shell: Option<ShellJob>,
    /// Shell used for `-c` commands (`$SHELL`, else `/bin/sh`).
    pub shell_path: OsString,
    /// Target of a bare `cd` / `cd ~`.
    pub home_dir: Option<PathBuf>,
    /// Per panel: where it was before its last directory change (`cd -`).
    prev_dirs: [Option<PathBuf>; 2],
    /// Whether leaving now should sync the shell's cwd (false: quit without cd).
    pub cd_on_exit: bool,
    /// The single probe worker in flight, if any.
    probe: Option<ProbeLoad>,
    /// A newer request waiting for the running probe to finish.
    probe_next: Option<ProbeReq>,
    probe_gen: u64,
    /// Results of background spawns, shown by `tick`.
    spawn_tx: Sender<String>,
    spawn_rx: Receiver<String>,
    /// Runs ssh; tests inject a fake.
    pub cmd: Arc<dyn Cmd>,
    /// ssh ControlPath template (`cm-%C` inside a private directory).
    pub control_path: PathBuf,
    /// Hosts from `~/.ssh/config`, merged with `[[servers]]` for the picker.
    pub ssh_hosts: Vec<SshHost>,
    /// `~/.ssh/config` to read in `configure`.
    pub ssh_config_path: Option<PathBuf>,
    /// Per panel: the local directory to return to with `:local`.
    local_cwds: [Option<PathBuf>; 2],
    /// Per panel: generation of the connect attempt in flight.
    connecting: [Option<u64>; 2],
    connect_gen: u64,
    connect_tx: Sender<ConnectMsg>,
    connect_rx: Receiver<ConnectMsg>,
    /// Connects that need the terminal for ssh's prompts, in request order.
    pending_master: VecDeque<ConnectJob>,
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
        let (spawn_tx, spawn_rx) = mpsc::channel();
        let (connect_tx, connect_rx) = mpsc::channel();
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
            mode: Mode::Classic,
            config_dir: None,
            settings: config::load_settings(None),
            truecolor: true,
            screen: None,
            editor_vars: (std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok()),
            editor_runner: editor::system_runner(),
            pending_edit: None,
            pending_remote_edit: None,
            uploads: Vec::new(),
            uploader: Arc::new(remote_ops::upload_replace),
            failed_copies: Vec::new(),
            keep_temp: Vec::new(),
            preview: PreviewState::new(),
            preview_runner: Arc::new(preview::preview_path),
            preview_debounce: Duration::from_millis(80),
            viewer: None,
            viewer_loader: Arc::new(|p| preview::preview_path_with(p, preview::Limits::VIEWER)),
            viewer_load: None,
            viewer_session: None,
            menu: None,
            launcher: launcher::system_launcher(),
            text_probe: Arc::new(probe_text),
            pending_run: None,
            cmdline: None,
            cmd_history: History::default(),
            pending_shell: None,
            shell_path: std::env::var_os("SHELL")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| OsString::from("/bin/sh")),
            home_dir: dirs::home_dir(),
            prev_dirs: [None, None],
            cd_on_exit: true,
            probe: None,
            probe_next: None,
            probe_gen: 0,
            spawn_tx,
            spawn_rx,
            cmd: Arc::new(SystemCmd),
            control_path: session::control_path(dirs::cache_dir().as_deref(), current_uid()),
            ssh_hosts: Vec::new(),
            ssh_config_path: None,
            local_cwds: [None, None],
            connecting: [None, None],
            connect_gen: 0,
            connect_tx,
            connect_rx,
            pending_master: VecDeque::new(),
        }
    }

    /// Load the config from `config_dir` (synchronous file IO, deliberately:
    /// it runs once before the first draw, when there is no UI to block; later
    /// reloads and theme saves use the background runner) and adopt it. `colorterm` is the `COLORTERM` value, injected so
    /// tests do not depend on the environment. Warnings from every source go to
    /// the status line (first one plus a count).
    pub fn configure(&mut self, config_dir: Option<PathBuf>, colorterm: Option<&str>) {
        self.config_dir = config_dir;
        self.truecolor = theme::supports_truecolor(colorterm);
        self.reload_settings();
        if let Some(path) = self.ssh_config_path.clone() {
            let (hosts, warnings) = servers::load_ssh_config(&path);
            self.ssh_hosts = hosts;
            self.settings.warnings.extend(warnings);
        }
        if let Some(s) = config::warning_summary(&self.settings.warnings) {
            self.status = Some(s);
        }
    }

    /// Re-read config, themes, keymap and assoc from `config_dir`.
    /// Synchronous: only for startup, before the first draw.
    fn reload_settings(&mut self) {
        self.settings = config::load_settings(self.config_dir.as_deref());
        self.apply_theme();
    }

    /// Re-read config, themes, keymap and assoc on the background runner; the
    /// result is applied from [`App::tick`].
    pub fn reload_config(&mut self) {
        if self.task.is_some() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let dir = self.config_dir.clone();
        self.task = Some(BgTask::spawn(BgKind::ConfigReload, move || {
            BgResult::Settings(Box::new(config::load_settings(dir.as_deref())))
        }));
        self.status = Some("설정 읽는 중…".into());
    }

    fn close_hint(&self) -> String {
        let keys: Vec<String> = self
            .settings
            .keymap
            .keys_for(Context::Viewer, Action::Quit)
            .iter()
            .map(ToString::to_string)
            .collect();
        format!("{}: 닫기", keys.join("/"))
    }

    fn open_screen(&mut self, title: &str, lines: Vec<String>) {
        self.status = None;
        self.invalidate_probe();
        self.screen = Some(TextScreen::new(title, lines, self.close_hint()));
    }

    /// F1: every binding of the current keymap.
    pub fn open_help(&mut self) {
        let lines = screen::help_lines(&self.settings.keymap);
        self.open_screen("도움말", lines);
    }

    /// Config file location plus the current bindings.
    pub fn open_key_settings(&mut self) {
        let path = self.config_dir.as_deref().map(config::config_file);
        let exists = path.as_deref().is_some_and(std::path::Path::is_file);
        let preset = self
            .settings
            .config
            .keys
            .preset
            .clone()
            .unwrap_or_else(|| "mdir".to_string());
        let lines =
            screen::key_settings_lines(path.as_deref(), exists, &preset, &self.settings.keymap);
        self.open_screen("키 설정", lines);
    }

    pub fn open_warnings(&mut self) {
        let lines = if self.settings.warnings.is_empty() {
            vec!["경고 없음".to_string()]
        } else {
            self.settings.warnings.clone()
        };
        self.open_screen("설정 경고", lines);
    }

    pub fn open_main_menu(&mut self) {
        let mut warnings = "설정 경고 보기".to_string();
        if !self.settings.warnings.is_empty() {
            warnings.push_str(&format!(" ({})", self.settings.warnings.len()));
        }
        let mut items: Vec<String> = [
            "테마 선택",
            "숨김 파일 보기 토글",
            "키 설정 보기",
            "설정 다시 읽기",
        ]
        .map(String::from)
        .to_vec();
        items.push(warnings);
        items.push("연결 프로그램 선택".to_string());
        self.status = None;
        self.menu = Some(OpenMenu {
            menu: Menu::new("메뉴", items, 0),
            kind: MenuKind::Main,
        });
    }

    fn open_theme_menu(&mut self) {
        let mut names: Vec<String> = theme::builtin_names()
            .iter()
            .map(|n| n.to_string())
            .collect();
        for t in &self.settings.user_themes {
            if !names.contains(&t.name) {
                names.push(t.name.clone());
            }
        }
        let current = self.settings.config.theme.name_for(self.mode);
        let cursor = names.iter().position(|n| n == current).unwrap_or(0);
        self.menu = Some(OpenMenu {
            menu: Menu::new(
                &format!("테마 선택 ({})", self.mode.key()),
                names.clone(),
                cursor,
            ),
            kind: MenuKind::Theme(names),
        });
    }

    fn preview_theme(&mut self, name: &str) {
        let (t, _) = theme::resolve(name, &self.settings.user_themes);
        self.theme = if self.truecolor { t } else { t.to_256() };
    }

    /// Adopt `name` for the current mode and write it to config.toml.
    fn commit_theme(&mut self, name: &str) {
        if self.task.is_some() {
            // Leave the picker's preview and keep the saved choice.
            self.apply_theme();
            self.status = Some("작업 진행 중".into());
            return;
        }
        let (t, _) = theme::resolve(name, &self.settings.user_themes);
        match self.mode {
            Mode::Classic => {
                self.settings.config.theme.classic = name.to_string();
                self.settings.classic = t;
            }
            Mode::Dev => {
                self.settings.config.theme.dev = name.to_string();
                self.settings.dev = t;
            }
        }
        self.apply_theme();
        let Some(dir) = self.config_dir.clone() else {
            self.status = Some("테마 저장 실패: 설정 디렉터리를 알 수 없음".into());
            return;
        };
        let (mode, owned) = (self.mode, name.to_string());
        self.task = Some(BgTask::spawn(
            BgKind::ThemeSave {
                mode,
                name: owned.clone(),
            },
            move || BgResult::ThemeSaved(config::save_theme(&dir, mode, &owned)),
        ));
        self.status = Some("테마 저장 중…".into());
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        let Some(action) = self.settings.keymap.lookup(Context::Menu, &key) else {
            return;
        };
        let Some(open) = self.menu.as_mut() else {
            return;
        };
        let event = open.menu.apply(action);
        let theme_names = match &open.kind {
            MenuKind::Theme(names) => Some(names.clone()),
            MenuKind::Main => None,
            MenuKind::Location { panel, servers } => {
                let (panel, servers) = (*panel, servers.clone());
                match event {
                    MenuEvent::Chosen(i) => {
                        self.menu = None;
                        match i.checked_sub(1).and_then(|n| servers.get(n)) {
                            None => self.switch_local(panel),
                            Some(sv) => self.connect_server(panel, sv.clone(), None),
                        }
                    }
                    MenuEvent::Cancelled => self.menu = None,
                    _ => {}
                }
                return;
            }
            MenuKind::OpenWith {
                path,
                choices,
                session,
            } => {
                let (path, choices, session) = (path.clone(), choices.clone(), session.clone());
                match event {
                    MenuEvent::Chosen(i) => {
                        self.menu = None;
                        if let Some(c) = choices.get(i) {
                            self.run_choice(c, path, session);
                        }
                    }
                    MenuEvent::Cancelled => self.menu = None,
                    _ => {}
                }
                return;
            }
        };
        match (event, theme_names) {
            (MenuEvent::Moved, Some(names)) => {
                let cursor = self.menu.as_ref().map_or(0, |m| m.menu.cursor);
                if let Some(n) = names.get(cursor) {
                    self.preview_theme(n);
                }
            }
            (MenuEvent::Cancelled, kind) => {
                self.menu = None;
                if kind.is_some() {
                    self.apply_theme();
                }
            }
            (MenuEvent::Chosen(i), Some(names)) => {
                self.menu = None;
                if let Some(n) = names.get(i) {
                    self.commit_theme(n);
                }
            }
            (MenuEvent::Chosen(i), None) => {
                self.menu = None;
                match i {
                    MENU_THEME => self.open_theme_menu(),
                    MENU_HIDDEN => self.panel_action(Action::ToggleHidden),
                    MENU_KEYS => self.open_key_settings(),
                    MENU_RELOAD => self.reload_config(),
                    MENU_WARNINGS => self.open_warnings(),
                    MENU_OPEN_WITH => self.open_with_menu(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_screen_key(&mut self, key: KeyEvent) {
        let Some(action) = self.settings.keymap.lookup(Context::Viewer, &key) else {
            return;
        };
        if self.screen.as_mut().is_some_and(|s| s.apply(action)) {
            self.screen = None;
        }
    }

    /// Switch layout mode; the theme follows the per-mode setting.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.apply_theme();
    }

    fn apply_theme(&mut self) {
        let base = self.settings.theme_for(self.mode);
        self.theme = if self.truecolor {
            base.clone()
        } else {
            base.to_256()
        };
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
        self.handle_key_inner(key);
        // Any overlay opened by this key supersedes a pending text probe.
        if self.dialog.is_some()
            || self.screen.is_some()
            || self.viewer.is_some()
            || self.menu.is_some()
        {
            self.invalidate_probe();
        }
    }

    fn handle_key_inner(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.cmdline.is_some() {
            self.handle_cmdline_key(key);
            return;
        }
        if self.dialog.is_some() {
            self.handle_dialog_key(key);
            return;
        }
        if self.screen.is_some() {
            self.handle_screen_key(key);
            return;
        }
        if self.viewer.is_some() {
            self.handle_viewer_key(key);
            return;
        }
        if self.menu.is_some() {
            self.handle_menu_key(key);
            return;
        }
        if self.trash_views[self.active].is_some() {
            self.handle_trash_key(key);
            return;
        }
        let Some(action) = self.settings.keymap.lookup(Context::Panel, &key) else {
            return;
        };
        self.panel_action(action);
    }

    /// Stop the app; `cd` = the shell should follow the active panel.
    fn quit(&mut self, cd: bool) {
        self.cd_on_exit = cd;
        self.running = false;
    }

    /// Directory the shell should `cd` to on exit, or `None` (quit without cd).
    pub fn exit_cwd(&self) -> Option<&Path> {
        if !self.cd_on_exit {
            return None;
        }
        if self.active_panel().location.is_remote() {
            // The shell cannot cd to a remote path: use the last local dir.
            return self.local_cwds[self.active].as_deref();
        }
        Some(self.active_panel().cwd.as_path())
    }

    fn handle_cmdline_key(&mut self, key: KeyEvent) {
        let Some(line) = self.cmdline.as_mut() else {
            return;
        };
        match line.key(key, &self.cmd_history) {
            CmdEvent::Pending => {}
            CmdEvent::Cancel => self.cmdline = None,
            CmdEvent::Run(cmd) => {
                self.cmdline = None;
                self.run_command_line(cmd.trim());
            }
        }
    }

    fn run_command_line(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        self.cmd_history.push(cmd);
        self.status = None;
        if let Some(lc) = cmdline::parse_location_cmd(cmd) {
            match lc {
                LocationCmd::Local => self.switch_local(self.active),
                LocationCmd::Ssh { host, path } => self.connect_by_name(self.active, &host, path),
                LocationCmd::SshUsage => self.status = Some("사용법: ssh 호스트[:경로]".into()),
            }
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        if let Some(session) = self.active_panel().location.session().cloned() {
            self.run_remote_command_line(session, cwd, cmd);
            return;
        }
        if let Some(target) = cmdline::parse_cd(cmd, self.home_dir.as_deref()) {
            self.change_dir_to(&cwd, &target);
            return;
        }
        if cfg!(windows) {
            self.status = Some("셸 명령: Windows 지원 예정".into());
            return;
        }
        self.pending_shell = Some(ShellJob {
            cmd: cmd.to_string(),
            cwd,
            session: None,
        });
    }

    /// A command line on a remote panel: `cd` is resolved on the server
    /// (never leaves the TUI), everything else runs on `ssh -t`.
    fn run_remote_command_line(&mut self, session: Arc<RemoteSession>, cwd: PathBuf, cmd: &str) {
        if !session.is_alive() {
            self.status = Some(DISCONNECTED_MSG.into());
            return;
        }
        // "~" stands for the remote home; `~/x` becomes `~/x` again.
        match cmdline::parse_cd(cmd, Some(Path::new("~"))) {
            None => {
                self.pending_shell = Some(ShellJob {
                    cmd: cmd.to_string(),
                    cwd,
                    session: Some(session),
                });
            }
            Some(target) => {
                let text = match target {
                    CdTarget::Home => "~".to_string(),
                    CdTarget::Prev => match &self.prev_dirs[self.active] {
                        Some(p) => p.to_string_lossy().into_owned(),
                        None => {
                            self.status = Some("이전 디렉터리 없음".into());
                            return;
                        }
                    },
                    CdTarget::Path(p) => p.to_string_lossy().into_owned(),
                };
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let sess = Arc::clone(&session);
                self.task = Some(BgTask::spawn(
                    BgKind::RemoteCd {
                        panel: self.active,
                        session,
                    },
                    move || BgResult::RemoteCd(remote_ops::resolve_cd(&sess, &cwd, &text)),
                ));
                self.status = Some("이동 중…".into());
            }
        }
    }

    fn change_dir_to(&mut self, cwd: &Path, target: &CdTarget) {
        let prev = self.prev_dirs[self.active].as_deref();
        match cmdline::resolve_cd(cwd, target, self.home_dir.as_deref(), prev) {
            Ok(dir) => {
                self.invalidate_probe();
                let id = self.next_id();
                let req = self
                    .active_panel_mut()
                    .begin_change_dir(id, dir, None, LoadKind::Enter);
                self.dispatch(self.active, req);
            }
            Err(e) => self.status = Some(e),
        }
    }

    // ---- ssh panels (HDIR-35/43/44) ----

    /// `[[servers]]` merged with `~/.ssh/config` aliases (picker order).
    fn servers(&self) -> Vec<Server> {
        servers::merge(&self.ssh_hosts, &self.settings.config.servers).0
    }

    /// Alt+F1/F2: choose local or a server for panel `idx`.
    fn open_location_menu(&mut self, idx: usize) {
        let servers = self.servers();
        let mut items = vec!["로컬".to_string()];
        items.extend(servers.iter().map(|s| {
            // ssh aliases show where they point (user@hostname) when known.
            let detail = self
                .ssh_hosts
                .iter()
                .find(|h| s.from_ssh_config && h.alias == s.host)
                .and_then(|h| {
                    let host = h.hostname.as_deref()?;
                    Some(match &h.user {
                        Some(u) => format!("{u}@{host}"),
                        None => host.to_string(),
                    })
                })
                .unwrap_or_else(|| s.host.clone());
            if detail == s.name {
                s.name.clone()
            } else {
                format!("{}  ({detail})", s.name)
            }
        }));
        let title = if idx == 0 {
            "왼쪽 패널"
        } else {
            "오른쪽 패널"
        };
        self.status = None;
        self.menu = Some(OpenMenu {
            menu: Menu::new(title, items, 0),
            kind: MenuKind::Location {
                panel: idx,
                servers,
            },
        });
    }

    /// `:ssh host[:path]`: a known server name, else an ssh destination as is.
    fn connect_by_name(&mut self, idx: usize, host: &str, path: Option<String>) {
        let server = self
            .servers()
            .into_iter()
            .find(|s| s.name == host)
            .unwrap_or_else(|| Server {
                name: host.to_string(),
                host: host.to_string(),
                port: None,
                start_path: None,
                theme: None,
                from_ssh_config: false,
            });
        self.connect_server(idx, server, path);
    }

    fn connect_server(&mut self, idx: usize, server: Server, path: Option<String>) {
        let path = path.or_else(|| server.start_path.clone());
        let target = SshTarget {
            host: server.host.clone(),
            port: server.port,
            control_path: self.control_path.clone(),
        };
        self.start_connect(idx, ConnectKind::New { server, path }, target);
    }

    fn start_connect(&mut self, idx: usize, kind: ConnectKind, target: SshTarget) {
        if cfg!(windows) {
            self.status = Some("SSH: Windows 지원 예정".into());
            return;
        }
        self.connect_gen += 1;
        let job = ConnectJob {
            idx,
            attempt: self.connect_gen,
            kind,
            target,
        };
        self.connecting[idx] = Some(job.attempt);
        self.pending_master.retain(|j| j.idx != idx);
        self.status = Some(format!("연결 중… {}", job.target.host));
        self.spawn_connect(job, false);
    }

    /// Run the non-interactive part of a connect on a worker. `authed`: the
    /// master was just started on the terminal, skip the liveness check.
    fn spawn_connect(&self, job: ConnectJob, authed: bool) {
        let cmd = Arc::clone(&self.cmd);
        let tx = self.connect_tx.clone();
        std::thread::spawn(move || {
            let (idx, attempt) = (job.idx, job.attempt);
            if !authed {
                if let Err(e) = session::ensure_control_dir(&job.target.control_path) {
                    let result = Err(format!("소켓 디렉터리: {e}"));
                    let _ = tx.send(ConnectMsg::Done {
                        idx,
                        attempt,
                        result,
                    });
                    return;
                }
                if !session::master_alive(&*cmd, &job.target) {
                    let _ = tx.send(ConnectMsg::NeedAuth(job));
                    return;
                }
            }
            let result = match &job.kind {
                ConnectKind::New { server, path } => {
                    session::establish(&cmd, &server.name, job.target.clone(), path.as_deref())
                        .map(ConnectDone::Established)
                }
                ConnectKind::Reconnect(s) => {
                    if s.revive() {
                        Ok(ConnectDone::Revived)
                    } else {
                        Err("연결 끊김".to_string())
                    }
                }
            };
            let _ = tx.send(ConnectMsg::Done {
                idx,
                attempt,
                result,
            });
        });
    }

    /// Is a connect in flight (including one waiting for the terminal)?
    pub fn is_connecting(&self) -> bool {
        self.connecting.iter().any(Option::is_some)
    }

    pub fn has_pending_master(&self) -> bool {
        !self.pending_master.is_empty()
    }

    /// Run the queued `ssh -fN` on the real terminal (password / host key
    /// prompts): `suspend` leaves the TUI, runs the closure, and re-enters.
    pub fn run_master(
        &mut self,
        suspend: impl FnOnce(&mut dyn FnMut() -> EditResult) -> std::io::Result<EditResult>,
    ) {
        let job = loop {
            let Some(job) = self.pending_master.pop_front() else {
                return;
            };
            if self.connecting[job.idx] == Some(job.attempt) {
                break job;
            } // else superseded
        };
        let cmd = Arc::clone(&self.cmd);
        let mut result: EditResult = Err("실행되지 않음".into());
        let outcome = suspend(&mut || {
            result = session::start_master(&*cmd, &job.target);
            result.clone()
        });
        match (outcome, result) {
            (Err(e), _) => {
                self.connecting[job.idx] = None;
                self.status = Some(format!("터미널 복귀 실패: {e}"));
            }
            (Ok(_), Ok(())) => {
                self.status = Some(format!("연결 중… {}", job.target.host));
                self.spawn_connect(job, true);
            }
            (Ok(_), Err(e)) => {
                self.connecting[job.idx] = None;
                self.status = Some(format!("연결 실패: {e}"));
            }
        }
    }

    /// Apply finished connect attempts; returns true when anything changed.
    fn tick_connect(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.connect_rx.try_recv() {
            match msg {
                ConnectMsg::NeedAuth(job) => {
                    if self.connecting[job.idx] == Some(job.attempt) {
                        self.status = Some(format!("인증 필요: {}", job.target.host));
                        self.pending_master.retain(|j| j.idx != job.idx);
                        // Workers answer in any order; ask in request order.
                        let at = self
                            .pending_master
                            .iter()
                            .position(|j| j.attempt > job.attempt)
                            .unwrap_or(self.pending_master.len());
                        self.pending_master.insert(at, job);
                        changed = true;
                    }
                }
                ConnectMsg::Done {
                    idx,
                    attempt,
                    result,
                } => {
                    if self.connecting[idx] != Some(attempt) {
                        continue;
                    }
                    self.connecting[idx] = None;
                    changed = true;
                    match result {
                        Ok(ConnectDone::Established(e)) => self.attach_remote(idx, e),
                        Ok(ConnectDone::Revived) => {
                            self.status = Some("재연결됨".into());
                            self.reload_remote_panels();
                        }
                        Err(e) => self.status = Some(format!("연결 실패: {e}")),
                    }
                }
            }
        }
        changed
    }

    fn attach_remote(&mut self, idx: usize, est: Established) {
        if !self.panels[idx].location.is_remote() {
            self.local_cwds[idx] = Some(self.panels[idx].cwd.clone());
        }
        self.trash_views[idx] = None;
        self.prev_dirs[idx] = None;
        self.invalidate_probe();
        self.status = Some(format!("{} 연결됨", est.session.name));
        self.panels[idx].set_location(Location::Remote(est.session), est.start);
        self.active = idx;
        self.reload_panel(idx, None);
    }

    /// Reload every panel showing a remote location (after a reconnect).
    fn reload_remote_panels(&mut self) {
        for i in 0..2 {
            if self.panels[i].location.is_remote() {
                self.reload_panel(i, None);
            }
        }
    }

    /// `:local` / picking "로컬": back to the last local directory.
    fn switch_local(&mut self, idx: usize) {
        self.connecting[idx] = None;
        self.pending_master.retain(|j| j.idx != idx);
        if !self.panels[idx].location.is_remote() {
            self.status = Some("이미 로컬 패널".into());
            return;
        }
        let cwd = self.local_cwds[idx]
            .take()
            .or_else(|| self.home_dir.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        self.invalidate_probe();
        self.prev_dirs[idx] = None;
        self.panels[idx].set_location(Location::Local, cwd);
        self.status = None;
        self.reload_panel(idx, None);
    }

    /// Theme used to draw panel `idx`: remote panels follow their server's
    /// theme rule, everything else the current mode's theme.
    pub fn panel_theme(&self, idx: usize) -> Theme {
        let Some(sess) = self.panels[idx].location.session() else {
            return self.theme.clone();
        };
        let server = Server {
            name: sess.name.clone(),
            host: sess.target.host.clone(),
            port: sess.target.port,
            start_path: None,
            theme: None,
            from_ssh_config: false,
        };
        let global = self.settings.config.theme.name_for(self.mode);
        let name = servers::theme_for(&server, &self.settings.config.servers, global);
        if name == global {
            return self.theme.clone();
        }
        let (t, _) = theme::resolve(&name, &self.settings.user_themes);
        if self.truecolor { t } else { t.to_256() }
    }

    pub fn has_pending_shell(&self) -> bool {
        self.pending_shell.is_some()
    }

    /// Run the queued command line command: `suspend` leaves the TUI, calls
    /// the closure (shell, then the exit code and a key prompt) and re-enters;
    /// then both panels reload and the exit code goes to the status line.
    pub fn run_shell(
        &mut self,
        suspend: impl FnOnce(&mut dyn FnMut() -> EditResult) -> std::io::Result<EditResult>,
    ) {
        let Some(job) = self.pending_shell.take() else {
            return;
        };
        let launcher = Arc::clone(&self.launcher);
        // A remote command runs through the local `ssh`; the local process
        // has no meaningful cwd of its own.
        let (argv, run_cwd) = match &job.session {
            Some(s) => (
                remote::remote_shell_argv(&s.target, &os_bytes(job.cwd.as_os_str()), &job.cmd),
                PathBuf::from("."),
            ),
            None => (
                vec![
                    self.shell_path.clone(),
                    OsString::from("-c"),
                    OsString::from(&job.cmd),
                ],
                job.cwd.clone(),
            ),
        };
        let mut result: Result<i32, String> = Err("실행되지 않음".into());
        let outcome = suspend(&mut || {
            let r = launcher.run_shell(&argv, &run_cwd);
            if let Ok(code) = &r {
                launcher.wait_key_after(*code);
            }
            let out = r.as_ref().map(|_| ()).map_err(Clone::clone);
            result = r;
            out
        });
        self.reload_both();
        self.status = Some(match (outcome, result) {
            (Err(e), _) => format!("터미널 복귀 실패: {e}"),
            (Ok(_), Ok(code)) => format!("종료 코드 {code}: {}", job.cmd),
            (Ok(_), Err(e)) => format!("오류: {}: {e}", job.cmd),
        });
    }

    /// Run a panel-context action.
    fn panel_action(&mut self, action: Action) {
        let page = self.page_height.max(1);
        let remote_here = self.active_panel().location.is_remote();
        let remote_any = remote_here || self.other_panel().location.is_remote();
        if action == Action::TrashView && remote_here {
            self.status = Some("원격: 휴지통 없음".into());
            return;
        }
        let blocked = match action {
            Action::Copy | Action::Move => remote_any,
            _ => false,
        };
        if blocked {
            self.status = Some(REMOTE_UNSUPPORTED.into());
            return;
        }
        match action {
            Action::CancelJob => {
                if let Some(job) = self.job.as_mut() {
                    job.handle.cancel();
                    job.cancelling = true;
                    self.status = Some("취소 중…".into());
                }
            }
            Action::Copy => self.open_transfer(JobKind::Copy),
            Action::Move => self.open_transfer(JobKind::Move),
            Action::Undo => self.undo(),
            Action::Mkdir => self.open_mkdir(),
            Action::Rename => self.open_rename(),
            Action::TrashView => self.open_trash_view(),
            Action::Delete => self.open_trash(),
            Action::Quit | Action::QuitNoCd => self.quit(action == Action::Quit),
            Action::SwitchPanel => {
                self.invalidate_probe();
                self.active = 1 - self.active;
            }
            Action::CursorUp => self.navigate(Panel::move_up),
            Action::CursorDown => self.navigate(Panel::move_down),
            Action::PageUp => self.navigate(|p| p.page_up(page)),
            Action::PageDown => self.navigate(|p| p.page_down(page)),
            Action::Home => self.navigate(Panel::home),
            Action::End => self.navigate(Panel::end),
            Action::Parent => {
                self.status = None;
                self.invalidate_probe();
                let id = self.next_id();
                if let Some(req) = self.active_panel_mut().begin_parent(id) {
                    self.dispatch(self.active, req);
                }
            }
            Action::ToggleHidden => {
                self.status = None;
                let id = self.next_id();
                let req = self.active_panel_mut().begin_toggle_hidden(id);
                self.dispatch(self.active, req);
            }
            Action::Enter => {
                self.status = None;
                if let Some(sess) = self.active_panel().location.session()
                    && !sess.is_alive()
                {
                    let sess = Arc::clone(sess);
                    let target = sess.target.clone();
                    self.start_connect(self.active, ConnectKind::Reconnect(sess), target);
                    return;
                }
                let id = self.next_id();
                match self.active_panel_mut().begin_enter(id) {
                    AsyncEnter::Opened(path) => self.activate_file(path),
                    AsyncEnter::Load(req) => {
                        self.invalidate_probe();
                        self.dispatch(self.active, req);
                    }
                    AsyncEnter::Nothing => self.show_panel_error(),
                }
            }
            Action::ToggleSelect => {
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
            Action::ToggleMode => {
                self.set_mode(self.mode.toggled());
                self.status = Some(format!("모드: {:?}", self.mode));
            }
            Action::View => self.open_viewer(),
            Action::Edit => self.open_editor(),
            Action::CommandLine => {
                self.status = None;
                self.cmdline = Some(CmdLine::default());
            }
            Action::ConnectLeft => self.open_location_menu(0),
            Action::ConnectRight => self.open_location_menu(1),
            Action::OpenWith => self.open_with_menu(),
            Action::Menu => self.open_main_menu(),
            Action::Help => self.open_help(),
            Action::Confirm
            | Action::Cancel
            | Action::ScrollLeft
            | Action::ScrollRight
            | Action::Search
            | Action::SearchNext => {}
        }
    }

    /// F4: queue an edit of the current file; the main loop runs it through
    /// [`App::run_edit`] (it needs the terminal handle).
    fn open_editor(&mut self) {
        let panel = self.active_panel();
        let Some(e) = panel.current() else {
            return;
        };
        let is_dir = match e.kind {
            EntryKind::Parent | EntryKind::Dir => true,
            EntryKind::Symlink { target_is_dir } => target_is_dir,
            EntryKind::File => false,
        };
        if is_dir {
            return;
        }
        let path = panel.cwd.join(&e.os_name);
        match panel.location.session().cloned() {
            Some(session) => self.start_remote_fetch(session, path, FetchPurpose::Edit),
            None => self.queue_edit(&path),
        }
    }

    /// Download `remote` on a worker; what happens next depends on `purpose`
    /// (see [`App::tick_task`]).
    fn start_remote_fetch(
        &mut self,
        session: Arc<RemoteSession>,
        remote: PathBuf,
        purpose: FetchPurpose,
    ) {
        if self.busy() {
            self.status = Some("작업 진행 중".into());
            return;
        }
        let (r, sess) = (remote.clone(), Arc::clone(&session));
        self.task = Some(BgTask::spawn(
            BgKind::RemoteFetch {
                panel: self.active,
                session,
                remote,
                purpose,
            },
            move || BgResult::RemoteFetch(remote_ops::fetch(&sess, &r)),
        ));
        self.status = Some("내려받는 중…".into());
    }

    /// The download for an F4 edit is done: queue the editor on the copy.
    fn begin_remote_edit(
        &mut self,
        session: Arc<RemoteSession>,
        remote: PathBuf,
        copy: EditCopy,
        panel: usize,
    ) {
        let Some(name) = remote.file_name().map(OsString::from) else {
            return;
        };
        let (visual, editor) = &self.editor_vars;
        match editor::editor_argv(visual.as_deref(), editor.as_deref(), &copy.local) {
            Ok(argv) => {
                self.status = None;
                self.pending_edit = Some(EditJob { argv, panel, name });
                self.pending_remote_edit = Some(RemoteEditCtx {
                    session,
                    remote,
                    copy,
                });
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// Queue an edit of `path` (a file in the active panel's directory).
    fn queue_edit(&mut self, path: &std::path::Path) {
        if cfg!(windows) {
            self.status = Some("편집: Windows 지원 예정".into());
            return;
        }
        let Some(name) = path.file_name().map(OsString::from) else {
            return;
        };
        let (visual, editor) = &self.editor_vars;
        match editor::editor_argv(visual.as_deref(), editor.as_deref(), path) {
            Ok(argv) => {
                self.status = None;
                self.pending_edit = Some(EditJob {
                    argv,
                    panel: self.active,
                    name,
                });
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// The finished preview for the entry under the cursor, if any.
    pub fn preview_shown(&self) -> Option<&Preview> {
        self.preview.shown_for_wanted()
    }

    /// Dev mode wants a preview that has not arrived yet.
    pub fn preview_pending(&self) -> bool {
        self.mode == Mode::Dev && self.preview.wanted.is_some() && self.preview_shown().is_none()
    }

    /// Path the Dev preview should show: the active panel's cursor entry.
    fn preview_target(&self) -> Option<PreviewTarget> {
        if self.mode != Mode::Dev
            || self.viewer.is_some()
            || self.trash_views[self.active].is_some()
        {
            return None;
        }
        let panel = self.active_panel();
        if panel.loading.is_some() {
            // Navigating: keep what we had instead of flickering.
            return self.preview.wanted.clone();
        }
        match panel.current() {
            Some(e) if e.kind != EntryKind::Parent => Some(PreviewTarget {
                path: panel.cwd.join(&e.os_name),
                session: panel.location.session().cloned(),
            }),
            _ => None,
        }
    }

    /// Drop in-flight results and recompute for the current target (the old
    /// result stays on screen until the new one lands).
    fn preview_refresh(&mut self) {
        self.preview.latest_id += 1;
        self.preview.requested = false;
        self.preview.wanted_at = Instant::now();
    }

    /// Track the cursor, dispatch a debounced worker for the latest position
    /// and apply fresh results; returns true when a result changed the screen.
    fn tick_preview(&mut self) -> bool {
        let target = self.preview_target();
        let st = &mut self.preview;
        if target != st.wanted {
            st.wanted = target;
            st.wanted_at = Instant::now();
            st.requested = false;
            st.latest_id += 1; // anything in flight is now stale
            if st.wanted.is_none() {
                st.shown = None;
            }
        }
        let mut changed = false;
        while let Ok((id, path, result)) = st.rx.try_recv() {
            st.in_flight = false;
            if id == st.latest_id {
                st.shown = Some((path, result));
                changed = true;
            }
        }
        if let Some(target) = st.wanted.clone()
            && !st.requested
            && !st.in_flight
            && st.wanted_at.elapsed() >= self.preview_debounce
        {
            st.requested = true;
            st.in_flight = true;
            let (id, tx) = (st.latest_id, st.tx.clone());
            let runner = Arc::clone(&self.preview_runner);
            std::thread::spawn(move || {
                // A panicking runner must still report back or the single
                // slot would stay occupied forever.
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match &target.session {
                            Some(sess) => {
                                remote_ops::preview(sess, &target.path, preview::Limits::PREVIEW)
                            }
                            None => runner(&target.path),
                        }
                    }))
                    .unwrap_or_else(|_| Preview::Error("미리보기 실패".into()));
                let _ = tx.send((id, target, result));
            });
        }
        changed
    }

    pub fn has_pending_edit(&self) -> bool {
        self.pending_edit.is_some()
    }

    /// Run the queued edit: `suspend` leaves the TUI, calls the closure (which
    /// runs the editor) and re-enters; then the panels reload with the cursor
    /// kept on the edited file.
    pub fn run_edit(
        &mut self,
        suspend: impl FnOnce(&mut dyn FnMut() -> EditResult) -> std::io::Result<EditResult>,
    ) {
        let Some(job) = self.pending_edit.take() else {
            return;
        };
        let runner = Arc::clone(&self.editor_runner);
        let argv = job.argv;
        let remote = self.pending_remote_edit.take();
        let outcome = suspend(&mut || runner(&argv));
        let cwd = self.panels[job.panel].cwd.clone();
        if self.panels[1 - job.panel].cwd == cwd {
            self.reload_after_op(1 - job.panel, None);
        }
        let name = job.name.to_string_lossy().into_owned();
        if let Some(path) = self.viewer.as_ref().map(|v| v.path.clone()) {
            self.start_viewer_load(path); // show what the editor saved
        }
        self.reload_after_op(job.panel, Some(job.name));
        let edited = outcome.as_ref().is_ok_and(Result::is_ok);
        let mut status = match outcome {
            Ok(Ok(())) => format!("편집 완료: {name}"),
            Ok(Err(msg)) => msg,
            Err(e) => format!("터미널 복귀 실패: {e}"),
        };
        if let Some(ctx) = remote {
            // Even after an editor error the user may have saved: a worker
            // hashes the copy and uploads it if the content changed.
            let editor_status = std::mem::replace(&mut status, format!("변경 확인 중…: {name}"));
            self.start_upload(ctx, name, edited, editor_status);
        }
        self.status = Some(status);
    }

    /// On a worker: hash the edited copy and, if the content changed, upload
    /// it. The temp dir goes away after a successful upload (or no change)
    /// and is kept (and reported) after a failure or an abandoned wait.
    fn start_upload(
        &mut self,
        ctx: RemoteEditCtx,
        name: String,
        edited: bool,
        editor_status: String,
    ) {
        let RemoteEditCtx {
            session,
            remote,
            copy,
        } = ctx;
        let EditCopy { dir, local, stamp } = copy;
        let (tx, rx) = mpsc::channel();
        let abandon = Arc::new(AtomicBool::new(false));
        let (s2, r2, l2) = (Arc::clone(&session), remote.clone(), local.clone());
        let (up, ab) = (Arc::clone(&self.uploader), Arc::clone(&abandon));
        std::thread::spawn(move || {
            let (result, unchanged) = match stamp.differs(&l2) {
                Ok(false) => (Ok(()), true),
                Ok(true) => (up(&s2, &l2, &r2), false),
                Err(e) => (Err(format!("편집한 파일을 읽을 수 없음: {e}")), false),
            };
            let kept = (result.is_err() || ab.load(Ordering::SeqCst)).then(|| {
                let _ = dir.keep();
                l2.clone()
            });
            let _ = tx.send(UploadDone {
                result,
                unchanged,
                kept,
            });
        });
        self.uploads.push(Upload {
            rx,
            name,
            session,
            remote,
            local,
            abandon,
            edited,
            editor_status,
        });
    }

    pub fn has_uploads(&self) -> bool {
        !self.uploads.is_empty()
    }

    /// Apply finished uploads; returns true when the screen changed.
    fn tick_uploads(&mut self) -> bool {
        let mut done = Vec::new();
        let mut pending = Vec::new();
        for u in std::mem::take(&mut self.uploads) {
            match u.rx.try_recv() {
                Ok(d) => done.push((u, d)),
                Err(TryRecvError::Empty) => pending.push(u),
                Err(TryRecvError::Disconnected) => {
                    let d = UploadDone {
                        result: Err("업로드 중단됨".into()),
                        unchanged: false,
                        kept: Some(u.local.clone()),
                    };
                    done.push((u, d));
                }
            }
        }
        self.uploads = pending;
        let changed = !done.is_empty();
        for (u, d) in done {
            let Upload {
                name,
                session,
                remote,
                edited,
                editor_status,
                ..
            } = u;
            self.status = Some(match &d.result {
                Ok(()) if d.unchanged && edited => format!("편집 완료: {name} (변경 없음)"),
                Ok(()) if d.unchanged => editor_status,
                Ok(()) => format!("편집 완료: {name} (업로드됨)"),
                Err(e) => {
                    let kept = d
                        .kept
                        .as_ref()
                        .map(|k| format!(" · 임시 복사본: {}", k.display()))
                        .unwrap_or_default();
                    if let Some(k) = &d.kept {
                        self.failed_copies.push(format!(
                            "업로드 실패: {name}: {e}\n  편집한 복사본: {}\n  원격 대상: {}:{}",
                            k.display(),
                            session.target.host,
                            remote.display()
                        ));
                    }
                    format!("업로드 실패: {name}: {e}{kept}")
                }
            });
            let dir = remote.parent().map(Path::to_path_buf).unwrap_or_default();
            for i in 0..2 {
                let same = self.panels[i]
                    .location
                    .session()
                    .is_some_and(|s| Arc::ptr_eq(s, &session));
                if same && self.panels[i].cwd == dir {
                    self.reload_after_op(i, None);
                }
            }
            let viewing = self.viewer.as_ref().is_some_and(|v| v.path == remote)
                && self
                    .viewer_session
                    .as_ref()
                    .is_some_and(|s| Arc::ptr_eq(s, &session));
            if viewing && d.result.is_ok() {
                self.start_viewer_load(remote);
            }
        }
        changed
    }

    /// Wait (bounded) for uploads still running when the app quits. Every
    /// edit that did not reach the server (failed earlier, failed now, or
    /// still running when the wait expires) keeps its local copy; the report
    /// names the copy and the remote destination so the caller can print them
    /// once the terminal is restored.
    pub fn finish_uploads(&mut self, timeout: Duration) -> UploadReport {
        let deadline = Instant::now() + timeout;
        while self.has_uploads() && Instant::now() < deadline {
            self.tick_uploads();
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut notes = std::mem::take(&mut self.failed_copies);
        for u in &self.uploads {
            u.abandon.store(true, Ordering::SeqCst);
            notes.push(format!(
                "업로드가 끝나지 않음: {}\n  편집한 복사본: {}\n  원격 대상: {}:{}",
                u.name,
                u.local.display(),
                u.session.target.host,
                u.remote.display()
            ));
        }
        UploadReport {
            failed: !notes.is_empty(),
            notes,
        }
    }

    /// The quit path of `main`: wait for uploads, print the report to `err`
    /// (call after the terminal is restored); `false` means an edit was not
    /// uploaded and the process should exit non-zero.
    pub fn quit_uploads(&mut self, timeout: Duration, err: &mut dyn std::io::Write) -> bool {
        let report = self.finish_uploads(timeout);
        for note in &report.notes {
            let _ = writeln!(err, "hdir: {note}");
        }
        !report.failed
    }

    /// F3: view the current file (directories and `..` do nothing).
    fn open_viewer(&mut self) {
        if let Some(path) = self.current_file() {
            match self.active_panel().location.session().cloned() {
                Some(session) => self.view_remote(session, path),
                None => self.view_path(path),
            }
        }
    }

    /// The active panel's cursor entry when it is a file (not `..`/a dir).
    fn current_file(&self) -> Option<PathBuf> {
        let panel = self.active_panel();
        let e = panel.current()?;
        let is_dir = match e.kind {
            EntryKind::Parent | EntryKind::Dir => true,
            EntryKind::Symlink { target_is_dir } => target_is_dir,
            EntryKind::File => false,
        };
        (!is_dir).then(|| panel.cwd.join(&e.os_name))
    }

    fn view_path(&mut self, path: PathBuf) {
        self.status = None;
        self.viewer_session = None;
        self.viewer = Some(Viewer::loading(path.clone()));
        self.start_viewer_load(path);
    }

    /// F3 on a remote file: read its head over ssh.
    fn view_remote(&mut self, session: Arc<RemoteSession>, path: PathBuf) {
        self.status = None;
        self.viewer_session = Some(session);
        self.viewer = Some(Viewer::loading(path.clone()));
        self.start_viewer_load(path);
    }

    /// Read `path` for the open viewer on a worker; replaces (abandons) any
    /// load still running.
    fn start_viewer_load(&mut self, path: PathBuf) {
        let (tx, rx) = mpsc::channel();
        let loader = Arc::clone(&self.viewer_loader);
        let session = self.viewer_session.clone();
        std::thread::spawn(move || {
            let preview = match session {
                Some(s) => remote_ops::preview(&s, &path, preview::Limits::VIEWER),
                None => loader(&path),
            };
            let _ = tx.send(preview);
        });
        self.viewer_load = Some(ViewerLoad { rx });
    }

    fn close_viewer(&mut self) {
        self.viewer = None;
        self.viewer_session = None;
        self.viewer_load = None; // abandon: the worker's send just fails
    }

    /// Apply a finished viewer load; returns true when the screen changed.
    fn tick_viewer_load(&mut self) -> bool {
        let Some(load) = self.viewer_load.as_ref() else {
            return false;
        };
        let preview = match load.rx.try_recv() {
            Ok(p) => p,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Preview::Error("읽기 중단됨".into()),
        };
        self.viewer_load = None;
        match self.viewer.as_mut() {
            Some(v) => {
                v.set_preview(preview);
                true
            }
            None => false,
        }
    }

    // ---- extension associations (HDIR-28/29) ----

    /// Enter on a file: its rule, else text -> viewer / other -> system opener.
    fn activate_file(&mut self, path: PathBuf) {
        let Some(name) = path.file_name() else {
            return;
        };
        let session = self.active_panel().location.session().cloned();
        if let Some(rule) = self.settings.assoc.find(name).cloned() {
            match session {
                Some(s) => self.start_remote_fetch(s, path, FetchPurpose::Rule(rule)),
                None => {
                    self.run_rule(&rule, path);
                }
            }
            return;
        }
        self.probe_gen += 1;
        let req = ProbeReq {
            id: self.probe_gen,
            path,
            active: self.active,
            cwd: self.active_panel().cwd.clone(),
            session,
        };
        if self.probe.is_some() {
            // One probe at a time: the newest request replaces any waiting one.
            self.probe_next = Some(req);
        } else {
            self.start_probe(req);
        }
    }

    /// Drop every pending text probe's result (a newer action took over).
    fn invalidate_probe(&mut self) {
        self.probe_gen += 1;
        self.probe_next = None;
    }

    fn start_probe(&mut self, req: ProbeReq) {
        let (tx, rx) = mpsc::channel();
        let probe = Arc::clone(&self.text_probe);
        let target = req.path.clone();
        let session = req.session.clone();
        let spawned = std::thread::Builder::new()
            .name("hdir-probe".into())
            .spawn(move || {
                let _ = tx.send(match session {
                    Some(s) => remote_ops::probe_text(&s, &target),
                    None => probe(&target),
                });
            });
        match spawned {
            Ok(_) => self.probe = Some(ProbeLoad { rx, req }),
            Err(e) => self.status = Some(format!("오류: 파일 형식 확인 실패: {e}")),
        }
    }

    /// Whether the UI is still where `req` was made.
    fn probe_state_holds(&self, req: &ProbeReq) -> bool {
        req.id == self.probe_gen
            && self.dialog.is_none()
            && self.screen.is_none()
            && self.viewer.is_none()
            && self.menu.is_none()
            && self.active == req.active
            && self.panels[req.active].cwd == req.cwd
            && match (self.panels[req.active].location.session(), &req.session) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }

    /// Apply a finished text probe; true when the screen changed.
    fn tick_probe(&mut self) -> bool {
        let Some(load) = self.probe.as_ref() else {
            return false;
        };
        let result = match load.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("파일 형식 확인 실패".into()),
        };
        let Some(load) = self.probe.take() else {
            return false;
        };
        let mut changed = false;
        if self.probe_state_holds(&load.req) {
            changed = true;
            let path = load.req.path;
            let session = load.req.session;
            match result {
                Ok(is_text) => match (assoc::default_action(is_text), session) {
                    (DefaultAction::View, None) => self.view_path(path),
                    (DefaultAction::View, Some(s)) => self.view_remote(s, path),
                    (DefaultAction::OpenWithSystem, None) => {
                        self.run_system(path);
                    }
                    (DefaultAction::OpenWithSystem, Some(s)) => {
                        self.start_remote_fetch(s, path, FetchPurpose::System);
                    }
                },
                Err(e) if e == SPECIAL_FILE => self.status = Some(e),
                Err(e) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    self.status = Some(format!("오류: {name}: {e}"));
                }
            }
        }
        if let Some(next) = self.probe_next.take()
            && next.id == self.probe_gen
        {
            self.start_probe(next);
            changed = true;
        }
        changed
    }

    /// Returns whether a program was queued or started.
    fn run_rule(&mut self, rule: &AssocRule, path: PathBuf) -> bool {
        let cwd = self.run_cwd(&path);
        match assoc::plan_run(rule, &path, &cwd, &self.shell) {
            Ok((argv, mode)) => self.launch(argv, mode, rule.cmd.clone(), &path, cwd),
            Err(e) => {
                self.status = Some(e);
                false
            }
        }
    }

    fn run_system(&mut self, path: PathBuf) -> bool {
        let cwd = self.run_cwd(&path);
        match assoc::system_opener_argv(&path) {
            Ok(argv) => self.launch(
                argv,
                RunMode::Background,
                assoc::system_opener().into(),
                &path,
                cwd,
            ),
            Err(e) => {
                self.status = Some(e);
                false
            }
        }
    }

    /// Programs run in the directory holding the file (the active panel's cwd).
    fn run_cwd(&self, path: &Path) -> PathBuf {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| self.active_panel().cwd.clone(), Path::to_path_buf)
    }

    fn launch(
        &mut self,
        argv: Vec<OsString>,
        mode: RunMode,
        label: String,
        path: &Path,
        cwd: PathBuf,
    ) -> bool {
        let name = path.file_name().map(OsString::from).unwrap_or_default();
        if mode == RunMode::Background {
            // spawn() can be slow (PATH search, a hung mount): keep it off
            // the UI thread and report through `tick`.
            let launcher = Arc::clone(&self.launcher);
            let tx = self.spawn_tx.clone();
            let shown = name.to_string_lossy().into_owned();
            let err_label = label.clone();
            let spawned = std::thread::Builder::new()
                .name("hdir-spawn".into())
                .spawn(move || {
                    let msg = match launcher.spawn_detached(&argv, &cwd) {
                        Ok(()) => format!("실행: {shown}"),
                        Err(e) => format!("오류: {err_label}: {e}"),
                    };
                    let _ = tx.send(msg);
                });
            if let Err(e) = spawned {
                self.status = Some(format!("오류: {label}: {e}"));
                return false;
            }
            return true;
        }
        self.status = None;
        self.pending_run = Some(RunJob {
            argv,
            cwd,
            mode,
            panel: self.active,
            name,
            label,
            temp: None,
        });
        true
    }

    fn run_choice(
        &mut self,
        choice: &OpenChoice,
        path: PathBuf,
        session: Option<Arc<RemoteSession>>,
    ) {
        match (choice, session) {
            (OpenChoice::Rule(rule), None) => {
                self.run_rule(rule, path);
            }
            (OpenChoice::Rule(rule), Some(s)) => {
                self.start_remote_fetch(s, path, FetchPurpose::Rule(rule.clone()));
            }
            (OpenChoice::System, None) => {
                self.run_system(path);
            }
            (OpenChoice::System, Some(s)) => {
                self.start_remote_fetch(s, path, FetchPurpose::System);
            }
            (OpenChoice::Viewer, None) => self.view_path(path),
            (OpenChoice::Viewer, Some(s)) => self.view_remote(s, path),
        }
    }

    /// Shift+Enter / Ctrl+O / F9 item: choose how to open the current file.
    fn open_with_menu(&mut self) {
        self.status = None;
        self.invalidate_probe();
        let Some(path) = self.current_file() else {
            self.status = Some("파일을 선택하세요".into());
            return;
        };
        let mut items = Vec::new();
        let mut choices = Vec::new();
        if let Some(name) = path.file_name() {
            for rule in self.settings.assoc.candidates(name) {
                items.push(rule.name.clone().unwrap_or_else(|| rule.cmd.clone()));
                choices.push(OpenChoice::Rule(rule.clone()));
            }
        }
        items.push("시스템 기본 프로그램".into());
        choices.push(OpenChoice::System);
        items.push("뷰어(F3)".into());
        choices.push(OpenChoice::Viewer);
        self.menu = Some(OpenMenu {
            menu: Menu::new("연결 프로그램 선택", items, 0),
            kind: MenuKind::OpenWith {
                path,
                choices,
                session: self.active_panel().location.session().cloned(),
            },
        });
    }

    pub fn has_pending_run(&self) -> bool {
        self.pending_run.is_some()
    }

    /// Run the queued association program: `suspend` leaves the TUI, calls the
    /// closure (which runs the program, and for `wait` rules reads a key) and
    /// re-enters; then the panels showing that directory reload.
    pub fn run_pending(
        &mut self,
        suspend: impl FnOnce(&mut dyn FnMut() -> EditResult) -> std::io::Result<EditResult>,
    ) {
        let Some(job) = self.pending_run.take() else {
            return;
        };
        let launcher = Arc::clone(&self.launcher);
        let wait = job.mode == RunMode::Wait;
        let outcome = suspend(&mut || {
            let r = launcher.run(&job.argv, &job.cwd);
            if wait {
                launcher.wait_key();
            }
            r
        });
        for i in 0..2 {
            if self.panels[i].cwd == job.cwd {
                let focus = (i == job.panel).then(|| job.name.clone());
                self.reload_after_op(i, focus);
            }
        }
        let name = job.name.to_string_lossy();
        self.status = Some(match outcome {
            Ok(Ok(())) => format!("실행 완료: {name}"),
            Ok(Err(e)) => format!("오류: {}: {e}", job.label),
            Err(e) => format!("터미널 복귀 실패: {e}"),
        });
    }

    /// F1 inside the viewer: help for the viewer keys only.
    fn open_viewer_help(&mut self) {
        let lines = screen::help_lines_for(&self.settings.keymap, &[Context::Viewer]);
        self.open_screen("도움말 (뷰어)", lines);
    }

    fn handle_viewer_key(&mut self, key: KeyEvent) {
        let Some(v) = self.viewer.as_mut() else {
            return;
        };
        if v.input.is_some() {
            v.input_key(key);
            return;
        }
        let Some(action) = self.settings.keymap.lookup(Context::Viewer, &key) else {
            return;
        };
        match action {
            Action::Edit if self.viewer_session.is_some() => {
                let path = v.path.clone();
                if let Some(session) = self.viewer_session.clone() {
                    self.start_remote_fetch(session, path, FetchPurpose::Edit);
                }
            }
            Action::Edit => {
                let path = v.path.clone();
                self.queue_edit(&path);
            }
            Action::Help => self.open_viewer_help(),
            _ if v.apply(action) => self.close_viewer(),
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
        // Local panels use the loader's default reader; remote ones list over ssh.
        let vfs = self.panels[idx]
            .location
            .is_remote()
            .then(|| self.panels[idx].location.vfs());
        self.loader.request_vfs(idx, req.id, req.dir, hidden, vfs);
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
            let before = self.panels[idx].cwd.clone();
            if self.panels[idx].finish_load(res) {
                changed = true;
                if self.panels[idx].cwd != before {
                    self.prev_dirs[idx] = Some(before);
                }
                self.preview_refresh();
                if idx == self.active
                    && kind != Some(LoadKind::Reload)
                    && let Some(err) = self.panels[idx].error.clone()
                {
                    self.status = Some(err);
                }
                if idx == self.active
                    && self.panels[idx]
                        .location
                        .session()
                        .is_some_and(|s| !s.is_alive())
                {
                    self.status = Some(DISCONNECTED_MSG.into());
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
        let loaded = self.drain_loads() | self.tick_connect();
        let task =
            self.tick_task() | self.tick_viewer_load() | self.tick_probe() | self.tick_uploads();
        let spawned = self.tick_spawn();
        let preview = self.tick_preview();
        self.tick_job() || loaded || task || preview || spawned
    }

    /// Show the newest finished background-spawn result.
    fn tick_spawn(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.spawn_rx.try_recv() {
            self.status = Some(msg);
            changed = true;
        }
        changed
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
            (
                BgKind::RemoteMkdir {
                    panel,
                    parent,
                    name,
                    session,
                },
                Some(BgResult::RemoteMkdir(res)),
            ) => match res {
                Ok(path) => {
                    self.undo.push(UndoOp::RemoteMkdir { session, path });
                    self.after_fs_op(panel, &parent, Some(&name), format!("폴더 생성: {name}"));
                }
                Err(e) => {
                    self.reload_both();
                    self.status = Some(format!("오류: {e}"));
                }
            },
            (
                BgKind::RemoteRename {
                    panel,
                    from,
                    name,
                    session,
                },
                Some(BgResult::RemoteRename(res)),
            ) => match res {
                Ok(to) => {
                    let parent = from.parent().map(PathBuf::from).unwrap_or_default();
                    self.undo.push(UndoOp::RemoteRename { session, from, to });
                    self.after_fs_op(panel, &parent, Some(&name), format!("이름 변경: {name}"));
                }
                Err(e) => {
                    self.reload_both();
                    self.status = Some(format!("오류: {e}"));
                }
            },
            (BgKind::RemoteDelete { panel, cwd, paths }, Some(BgResult::RemoteDelete(res))) => {
                // Even a failed rm -rf may have removed some of the items.
                self.clear_selection(panel, &cwd, &paths);
                self.reload_both();
                self.status = Some(match res {
                    Ok(()) => format!("영구 삭제: {}개 (원격 삭제는 되돌릴 수 없음)", paths.len()),
                    Err(e) => format!("오류: {e}"),
                });
            }
            (
                BgKind::RemoteFetch {
                    panel,
                    session,
                    remote,
                    purpose,
                },
                Some(BgResult::RemoteFetch(res)),
            ) => self.finish_fetch(panel, session, remote, purpose, res),
            (BgKind::RemoteCd { panel, session }, Some(BgResult::RemoteCd(res))) => {
                let same = self.panels[panel]
                    .location
                    .session()
                    .is_some_and(|s| Arc::ptr_eq(s, &session));
                match res {
                    Ok(dir) if same => {
                        self.status = None;
                        self.invalidate_probe();
                        let id = self.next_id();
                        let req =
                            self.panels[panel].begin_change_dir(id, dir, None, LoadKind::Enter);
                        self.dispatch(panel, req);
                    }
                    Ok(_) => self.status = None, // the panel left that host meanwhile
                    Err(e) => self.status = Some(e),
                }
            }
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
            (BgKind::ConfigReload, Some(BgResult::Settings(settings))) => {
                self.settings = *settings;
                self.apply_theme();
                self.status = Some(
                    config::warning_summary(&self.settings.warnings)
                        .unwrap_or_else(|| "설정을 다시 읽었음".to_string()),
                );
            }
            (BgKind::ThemeSave { mode, name }, Some(BgResult::ThemeSaved(res))) => {
                self.status = Some(match res {
                    Ok(()) => format!("테마 저장: {name} ({})", mode.key()),
                    Err(e) => format!("테마 저장 실패: {e}"),
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

    /// A remote download finished: edit it, or run the program chosen for it.
    fn finish_fetch(
        &mut self,
        panel: usize,
        session: Arc<RemoteSession>,
        remote: PathBuf,
        purpose: FetchPurpose,
        res: Result<EditCopy, String>,
    ) {
        let copy = match res {
            Ok(c) => c,
            Err(e) => {
                self.status = Some(format!("오류: {e}"));
                return;
            }
        };
        match purpose {
            FetchPurpose::Edit => self.begin_remote_edit(session, remote, copy, panel),
            FetchPurpose::Rule(rule) => {
                let EditCopy { dir, local, .. } = copy;
                let queued = self.run_rule(&rule, local);
                self.adopt_temp(dir, queued);
            }
            FetchPurpose::System => {
                let EditCopy { dir, local, .. } = copy;
                let queued = self.run_system(local);
                self.adopt_temp(dir, queued);
            }
        }
    }

    /// Tie a downloaded copy's lifetime to the program started on it: a
    /// foreground run removes it afterwards, a background opener keeps it
    /// until the app exits, a failed start removes it right away.
    fn adopt_temp(&mut self, dir: tempfile::TempDir, started: bool) {
        if !started {
            return;
        }
        match self.pending_run.as_mut() {
            Some(job) => job.temp = Some(dir),
            None => self.keep_temp.push(dir),
        }
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
        if let Some(session) = self.active_panel().location.session().cloned() {
            // No trash on a remote host: a red permanent-delete confirmation.
            let mut msg: Vec<String> = names
                .iter()
                .take(5)
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            if names.len() > 5 {
                msg.push(format!("외 {}개", names.len() - 5));
            }
            let title = format!("영구 삭제 (원격: {})", session.name);
            self.status = None;
            self.dialog = Some((
                Dialog::danger(&title, msg),
                PendingAction::RemoteDelete {
                    panel: self.active,
                    session,
                    cwd,
                    names,
                },
            ));
            return;
        }
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
        let moved = |app: &mut App, f: &dyn Fn(&mut TrashView)| {
            if app.task.is_none() {
                app.status = None;
            }
            if let Some(v) = app.trash_views[idx].as_mut() {
                f(v);
            }
        };
        let Some(action) = self.settings.keymap.lookup(Context::Trash, &key) else {
            if matches!(key.code, KeyCode::F(_)) {
                self.status = Some("휴지통에서는 사용할 수 없음".into());
            }
            return;
        };
        match action {
            Action::Cancel | Action::Parent => self.close_trash_view(idx),
            Action::SwitchPanel => self.active = 1 - self.active,
            Action::Quit | Action::QuitNoCd => self.quit(action == Action::Quit),
            // Already in the trash view.
            Action::TrashView => {}
            Action::Delete => self.open_trash_purge(),
            Action::Copy | Action::Enter => self.open_trash_restore(),
            Action::CursorUp => moved(self, &TrashView::move_up),
            Action::CursorDown => moved(self, &TrashView::move_down),
            Action::PageUp => moved(self, &|v| v.page_up(page)),
            Action::PageDown => moved(self, &|v| v.page_down(page)),
            Action::Home => moved(self, &TrashView::home),
            Action::End => moved(self, &TrashView::end),
            Action::ToggleSelect => moved(self, &TrashView::toggle_current),
            _ => {
                if matches!(key.code, KeyCode::F(_)) {
                    self.status = Some("휴지통에서는 사용할 수 없음".into());
                }
            }
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

    /// Map a key through the dialog context: bound confirm/cancel keys become
    /// Enter/Esc. In text inputs, plain characters are always text.
    fn dialog_key(&self, key: KeyEvent) -> KeyEvent {
        let Some((dlg, _)) = self.dialog.as_ref() else {
            return key;
        };
        let plain_char = matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if matches!(dlg, Dialog::Input { .. }) && plain_char {
            return key;
        }
        let code = match self.settings.keymap.lookup(Context::Dialog, &key) {
            Some(Action::Confirm) => KeyCode::Enter,
            Some(Action::Cancel) => KeyCode::Esc,
            _ => return key,
        };
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, key.kind)
    }

    fn dispatch_dialog_key(&mut self, key: KeyEvent) {
        let key = self.dialog_key(key);
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
                if let Some(session) = self.active_panel().location.session().cloned() {
                    let sess = Arc::clone(&session);
                    self.task = Some(BgTask::spawn(
                        BgKind::RemoteMkdir {
                            panel: self.active,
                            parent,
                            name,
                            session,
                        },
                        move || BgResult::RemoteMkdir(remote_ops::mkdir(&sess, &p, &n)),
                    ));
                    self.status = Some("폴더 생성 중…".into());
                    return;
                }
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
                if let Some(session) = self.active_panel().location.session().cloned() {
                    let sess = Arc::clone(&session);
                    self.task = Some(BgTask::spawn(
                        BgKind::RemoteRename {
                            panel: self.active,
                            from,
                            name,
                            session,
                        },
                        move || BgResult::RemoteRename(remote_ops::rename(&sess, &f, &n)),
                    ));
                    self.status = Some("이름 변경 중…".into());
                    return;
                }
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
            (
                PendingAction::RemoteDelete {
                    panel,
                    session,
                    cwd,
                    names,
                },
                DialogResult::Confirmed,
            ) => {
                if self.busy() {
                    self.status = Some("작업 진행 중".into());
                    return;
                }
                let paths: Vec<PathBuf> = names.iter().map(|n| cwd.join(n)).collect();
                let dir = cwd.clone();
                self.task = Some(BgTask::spawn(
                    BgKind::RemoteDelete { panel, cwd, paths },
                    move || BgResult::RemoteDelete(remote_ops::delete(&session, &dir, &names)),
                ));
                self.status = Some("영구 삭제 중…".into());
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
        let same_source = self.panels[idx].location.vfs().source_key()
            == self.panels[1 - idx].location.vfs().source_key();
        if same_source && self.panels[1 - idx].cwd == dir {
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
        let rows = panel_rows(self.mode, frame.area().height);
        self.page_height = rows.max(1);
        for p in &mut self.panels {
            p.ensure_visible(rows);
        }
        for v in self.trash_views.iter_mut().flatten() {
            v.ensure_visible(rows);
        }
        let bar_ctx = if self.trash_views[self.active].is_some() {
            Context::Trash
        } else {
            Context::Panel
        };
        let fkeys = ui::fkey_bar(&self.settings.keymap, bar_ctx);
        let preview_view = (self.mode == Mode::Dev).then(|| {
            let title = self
                .preview
                .wanted
                .as_ref()
                .and_then(|t| t.path.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let body = match (&self.preview.wanted, self.preview.shown_for_wanted()) {
                (None, _) => PreviewBody::Idle,
                (Some(_), Some(p)) => PreviewBody::Ready(p),
                (Some(_), None) => PreviewBody::Loading,
            };
            PreviewView { title, body }
        });
        let themes = [self.panel_theme(0), self.panel_theme(1)];
        let view = View {
            panels: [&self.panels[0], &self.panels[1]],
            active: self.active,
            status: self.status.as_deref(),
            shell: &self.shell,
            assoc: &self.settings.assoc,
            os: std::env::consts::OS,
            mode: self.mode,
            preview: preview_view,
            trash: [self.trash_views[0].as_ref(), self.trash_views[1].as_ref()],
            theme: &themes[self.active],
            panel_themes: [&themes[0], &themes[1]],
            fkeys: &fkeys,
            cmdline: self.cmdline.as_ref().map(|c| (c.value.as_str(), c.cursor)),
        };
        let viewer_hint = format!("{} · /: 검색 · n: 다음", self.close_hint());
        if let Some(scr) = self.screen.as_mut() {
            screen::render(frame, scr, &self.theme);
        } else if let Some(v) = self.viewer.as_mut() {
            let hint = &viewer_hint;
            viewer::render(frame, v, &self.theme, hint);
        } else {
            ui::draw(frame, &view);
        }
        if let Some(open) = &self.menu {
            menu::render(frame, frame.area(), &open.menu, &self.theme);
        }
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

    // ---- HDIR-17/25: config wiring ----

    fn config_with(text: &str) -> TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("config.toml"), text).unwrap();
        d
    }

    fn configured(text: &str, colorterm: Option<&str>) -> (TempDir, App) {
        let (t, mut app) = fixture();
        let cfg = config_with(text);
        app.configure(Some(cfg.path().to_path_buf()), colorterm);
        (t, app)
        // `cfg` lives only as long as the files are read at configure time
    }

    #[test]
    fn configure_applies_theme_for_current_mode() {
        let (_t, mut app) = configured(
            "[theme]\nclassic = \"dark\"\ndev = \"light\"\n",
            Some("truecolor"),
        );
        assert_eq!(app.mode, crate::config::Mode::Classic);
        assert_eq!(app.theme, theme::builtin("dark").unwrap());
        app.set_mode(crate::config::Mode::Dev);
        assert_eq!(app.theme, theme::builtin("light").unwrap());
        app.set_mode(crate::config::Mode::Classic);
        assert_eq!(app.theme.name, "dark");
    }

    #[test]
    fn without_truecolor_theme_is_mapped_to_256() {
        let (_t, app) = configured("", Some("256color"));
        assert_eq!(app.theme, theme::builtin("mdir").unwrap().to_256());
        assert!(matches!(app.theme.bg, ratatui::style::Color::Indexed(_)));
        let (_t, app) = configured("", None);
        assert!(matches!(app.theme.bg, ratatui::style::Color::Indexed(_)));
        let (_t, app) = configured("", Some("24bit"));
        assert!(matches!(app.theme.bg, ratatui::style::Color::Rgb(..)));
    }

    #[test]
    fn startup_status_shows_first_warning_and_count() {
        let (_t, app) = configured(
            "[theme]\nclassic = \"nope\"\n[keys.bind]\nfrob = \"F5\"\n",
            Some("truecolor"),
        );
        let st = app.status.clone().unwrap();
        assert!(st.starts_with("설정 경고: "), "{st}");
        assert!(st.contains("(외 1개)"), "{st}");
        assert_eq!(app.settings.warnings.len(), 2);
        assert_eq!(app.theme.name, "mdir", "unknown theme falls back");
    }

    #[test]
    fn startup_without_config_has_no_status_or_warning() {
        let (_t, mut app) = fixture();
        let empty = tempfile::tempdir().unwrap();
        app.configure(Some(empty.path().to_path_buf()), Some("truecolor"));
        assert!(app.status.is_none());
        assert!(app.settings.warnings.is_empty());
        assert_eq!(app.config_dir.as_deref(), Some(empty.path()));
    }

    #[test]
    fn broken_config_falls_back_to_defaults_with_warning() {
        let (_t, app) = configured("[theme\n", Some("truecolor"));
        assert_eq!(app.theme.name, "mdir");
        assert!(app.status.as_deref().unwrap().contains("config.toml"));
    }

    // ---- HDIR-19: keymap-driven dispatch ----

    fn press_mods(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.handle_key(KeyEvent::new_with_kind(code, mods, KeyEventKind::Press));
    }

    #[test]
    fn user_remap_copy_to_c_moves_copy_off_f5() {
        let (_t, mut app) = configured("[keys.bind]\ncopy = [\"c\"]\n", Some("truecolor"));
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::F(5));
        assert!(app.dialog.is_none(), "F5 must no longer copy");
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(
            app.dialog,
            Some((
                _,
                PendingAction::Transfer {
                    kind: JobKind::Copy,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn empty_binding_disables_the_action() {
        let (_t, mut app) = fixture();
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::F(3));
        assert!(app.viewer.is_some(), "default: view is bound");
        let (_t, mut app) = configured("[keys.bind]\nview = []\n", Some("truecolor"));
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::F(3));
        assert!(
            app.viewer.is_none() && app.status.is_none(),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn alt_digit_fallbacks_and_shift_f6_come_from_the_keymap() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::Down);
        press_mods(&mut app, KeyCode::Char('5'), KeyModifiers::ALT);
        assert!(matches!(
            app.dialog,
            Some((
                _,
                PendingAction::Transfer {
                    kind: JobKind::Copy,
                    ..
                }
            ))
        ));
        press(&mut app, KeyCode::Esc);
        press_mods(&mut app, KeyCode::F(6), KeyModifiers::SHIFT);
        assert!(matches!(
            app.dialog,
            Some((_, PendingAction::Rename { .. }))
        ));
    }

    #[test]
    fn vim_preset_navigates_with_jk_and_quits_with_q() {
        let (_t, mut app) = configured("[keys]\npreset = \"vim\"\n", Some("truecolor"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.panels[0].cursor, 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.panels[0].cursor, 0);
    }

    #[test]
    fn quit_no_cd_and_toggle_mode_actions() {
        let (_t, mut app) = fixture();
        press_mods(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, Mode::Dev);
        press_mods(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, Mode::Classic);
        press_mods(&mut app, KeyCode::F(10), KeyModifiers::SHIFT);
        assert!(!app.running);
    }

    #[test]
    fn dialog_confirm_and_cancel_keys_are_remappable() {
        let (_t, mut app) = configured(
            "[keys.dialog]\nconfirm = [\"Enter\", \"Ctrl+Y\"]\ncancel = [\"Esc\", \"Ctrl+G\"]\n",
            Some("truecolor"),
        );
        press(&mut app, KeyCode::Down); // adir
        press(&mut app, KeyCode::F(8));
        assert!(matches!(app.dialog, Some((Dialog::Confirm { .. }, _))));
        press_mods(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(app.dialog.is_none(), "Ctrl+G cancels");
        assert!(app.task.is_none());
        press(&mut app, KeyCode::F(8));
        press_mods(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert!(app.dialog.is_none());
        assert!(app.task.is_some(), "Ctrl+Y confirmed the trash move");
    }

    #[test]
    fn text_input_keys_stay_raw_even_if_bound_in_dialog_context() {
        let (_t, mut app) = configured(
            "[keys.dialog]\ncancel = [\"Esc\", \"q\"]\n",
            Some("truecolor"),
        );
        press(&mut app, KeyCode::F(7));
        press(&mut app, KeyCode::Char('q'));
        match &app.dialog {
            Some((Dialog::Input { value, .. }, _)) => assert_eq!(value, "q"),
            other => panic!("input dialog must stay open: {other:?}"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn trash_view_keys_come_from_the_trash_context() {
        let (_t, mut app, _f) = fixture_trash();
        let cfg = config_with("[keys.trash]\ncancel = [\"x\"]\n");
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        enter_trash(&mut app);
        press(&mut app, KeyCode::Esc);
        assert!(app.trash_views[0].is_some(), "Esc no longer closes");
        press(&mut app, KeyCode::Char('x'));
        assert!(app.trash_views[0].is_none());
    }

    // ---- HDIR-20: help, key settings, reload ----

    fn draw_text(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    // wide glyphs are followed by a filler cell
                    x += if unicode_width::UnicodeWidthStr::width(sym) > 1 {
                        2
                    } else {
                        1
                    };
                }
                row
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn page_text(app: &App) -> String {
        app.screen.as_ref().unwrap().lines.join("\n")
    }

    #[test]
    fn f1_opens_the_help_screen_and_esc_closes_it() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(1));
        assert!(app.screen.is_some());
        let text = draw_text(&mut app, 80, 24);
        assert!(text.contains("도움말"), "{text}");
        assert!(text.contains("F5, Alt+5"), "{text}");
        press(&mut app, KeyCode::Esc);
        assert!(app.screen.is_none());
        assert!(app.running);
    }

    #[test]
    fn help_screen_takes_all_keys_and_scrolls() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(1));
        draw_text(&mut app, 80, 10);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.screen.as_ref().unwrap().scroll, 1);
        assert_eq!(app.panels[0].cursor, 0, "panel must not move");
        press(&mut app, KeyCode::F(5));
        assert!(app.dialog.is_none(), "no copy dialog from help");
        press(&mut app, KeyCode::End);
        assert!(app.screen.as_ref().unwrap().scroll > 1);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.screen.is_none());
        assert!(app.running, "q only closes the help");
    }

    #[test]
    fn help_is_generated_from_the_current_keymap() {
        let (_t, mut app) = configured("[keys.bind]\ncopy = [\"c\"]\n", Some("truecolor"));
        press(&mut app, KeyCode::F(1));
        let text = page_text(&app);
        let copy = text.lines().find(|l| l.contains("복사")).unwrap();
        assert!(copy.contains('c') && !copy.contains("F5"), "{copy}");
    }

    #[test]
    fn key_settings_view_shows_path_and_bindings() {
        let cfg = config_with("[keys.bind]\ncopy = [\"c\"]\n");
        let (_t, mut app) = fixture();
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        app.open_key_settings();
        let text = page_text(&app);
        assert!(
            text.contains(&cfg.path().join("config.toml").display().to_string()),
            "{text}"
        );
        assert!(text.contains("키 프리셋: mdir"));
        assert!(text.lines().any(|l| l.contains("복사") && l.contains('c')));
        press(&mut app, KeyCode::Esc);
        assert!(app.screen.is_none());
    }

    #[test]
    fn reload_applies_theme_keys_and_assoc_immediately() {
        let cfg = config_with("");
        let (_t, mut app) = fixture();
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        assert_eq!(app.theme.name, "mdir");
        fs::write(
            cfg.path().join("config.toml"),
            "[theme]\nclassic = \"dark\"\n[keys.bind]\ncopy = [\"c\"]\n[[assoc]]\next = [\"md\"]\ncmd = \"cat {}\"\n",
        )
        .unwrap();
        press(&mut app, KeyCode::Down);
        app.reload_config();
        settle_task(&mut app);
        assert_eq!(app.theme.name, "dark");
        assert_eq!(app.status.as_deref(), Some("설정을 다시 읽었음"));
        press(&mut app, KeyCode::F(5));
        assert!(app.dialog.is_none());
        press(&mut app, KeyCode::Char('c'));
        assert!(app.dialog.is_some());
        assert!(
            app.settings
                .assoc
                .find(std::ffi::OsStr::new("a.md"))
                .is_some()
        );
    }

    #[test]
    fn reload_keeps_mode_and_reports_warnings() {
        let cfg = config_with("");
        let (_t, mut app) = fixture();
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        app.set_mode(Mode::Dev);
        fs::write(
            cfg.path().join("config.toml"),
            "[theme]\ndev = \"light\"\nclassic = \"nope\"\n[keys.bind]\nfrob = \"F5\"\n",
        )
        .unwrap();
        app.reload_config();
        settle_task(&mut app);
        assert_eq!(app.mode, Mode::Dev);
        assert_eq!(app.theme.name, "light");
        let st = app.status.clone().unwrap();
        assert!(
            st.starts_with("설정 경고: ") && st.contains("(외 1개)"),
            "{st}"
        );
    }

    #[test]
    fn warnings_view_lists_every_warning() {
        let cfg = config_with("[theme]\nclassic = \"nope\"\n[keys.bind]\nfrob = \"F5\"\n");
        let (_t, mut app) = fixture();
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        app.open_warnings();
        let text = page_text(&app);
        assert!(text.contains("nope") && text.contains("frob"), "{text}");
        let (_t, mut app) = fixture();
        app.open_warnings();
        assert!(page_text(&app).contains("경고 없음"));
    }

    // ---- HDIR-16 (F9 menu) / HDIR-24: theme selection ----

    /// App with a live config dir (kept alive by the returned guard).
    fn app_with_dir(text: &str) -> (TempDir, TempDir, App) {
        let (t, mut app) = fixture();
        let cfg = config_with(text);
        app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
        (t, cfg, app)
    }

    fn menu_items(app: &App) -> Vec<String> {
        app.menu.as_ref().unwrap().menu.items.clone()
    }

    fn pick(app: &mut App, label: &str) {
        let idx = menu_items(app)
            .iter()
            .position(|i| i.starts_with(label))
            .unwrap_or_else(|| panic!("no item {label}: {:?}", menu_items(app)));
        let cur = app.menu.as_ref().unwrap().menu.cursor;
        for _ in 0..cur {
            press(app, KeyCode::Up);
        }
        for _ in 0..idx {
            press(app, KeyCode::Down);
        }
        press(app, KeyCode::Enter);
    }

    #[test]
    fn f9_opens_the_menu_with_the_listed_items_and_esc_or_f9_closes() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(9));
        assert_eq!(
            menu_items(&app),
            [
                "테마 선택",
                "숨김 파일 보기 토글",
                "키 설정 보기",
                "설정 다시 읽기",
                "설정 경고 보기",
                "연결 프로그램 선택"
            ]
        );
        let text = draw_text(&mut app, 80, 24);
        assert!(
            text.contains("테마 선택") && text.contains("설정 경고 보기"),
            "{text}"
        );
        press(&mut app, KeyCode::Esc);
        assert!(app.menu.is_none());
        press(&mut app, KeyCode::F(9));
        press(&mut app, KeyCode::F(9));
        assert!(app.menu.is_none(), "F9 toggles the menu");
    }

    #[test]
    fn menu_captures_navigation_keys() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(9));
        press(&mut app, KeyCode::Down);
        assert_eq!(app.menu.as_ref().unwrap().menu.cursor, 1);
        assert_eq!(app.panels[0].cursor, 0);
        press(&mut app, KeyCode::F(5));
        assert!(app.dialog.is_none());
        assert!(app.menu.is_some());
    }

    #[test]
    fn menu_warning_item_shows_count() {
        let (_t, _c, mut app) = app_with_dir("[theme]\nclassic = \"nope\"\n");
        press(&mut app, KeyCode::F(9));
        assert_eq!(menu_items(&app)[4], "설정 경고 보기 (1)");
    }

    #[test]
    fn menu_toggle_hidden_item_toggles_and_closes() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(9));
        pick(&mut app, "숨김 파일");
        assert!(app.menu.is_none());
        settle(&mut app);
        assert!(app.panels[0].entries.iter().any(|e| e.name == ".h"));
    }

    #[test]
    fn menu_key_settings_and_warnings_open_pages() {
        let (_t, _c, mut app) = app_with_dir("[theme]\nclassic = \"nope\"\n");
        press(&mut app, KeyCode::F(9));
        pick(&mut app, "키 설정 보기");
        assert!(app.menu.is_none());
        assert!(page_text(&app).contains("설정 파일:"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::F(9));
        pick(&mut app, "설정 경고 보기");
        assert!(page_text(&app).contains("nope"));
    }

    #[test]
    fn menu_reload_item_reloads_and_closes() {
        let (_t, cfg, mut app) = app_with_dir("");
        fs::write(
            cfg.path().join("config.toml"),
            "[theme]\nclassic = \"dark\"\n",
        )
        .unwrap();
        press(&mut app, KeyCode::F(9));
        pick(&mut app, "설정 다시 읽기");
        settle_task(&mut app);
        assert!(app.menu.is_none());
        assert_eq!(app.theme.name, "dark");
        assert_eq!(app.status.as_deref(), Some("설정을 다시 읽었음"));
    }

    fn open_theme_menu(app: &mut App) {
        press(app, KeyCode::F(9));
        pick(app, "테마 선택");
    }

    #[test]
    fn theme_menu_lists_builtin_then_user_themes_with_cursor_on_current() {
        let (_t, cfg, mut app) = app_with_dir("[theme]\nclassic = \"dark\"\n");
        fs::create_dir(cfg.path().join("themes")).unwrap();
        fs::write(cfg.path().join("themes/mine.toml"), "base = \"dark\"\n").unwrap();
        fs::write(
            cfg.path().join("themes/dark.toml"),
            "[colors]\nbg = \"#010203\"\n",
        )
        .unwrap();
        app.reload_config();
        settle_task(&mut app);
        open_theme_menu(&mut app);
        let items = menu_items(&app);
        assert_eq!(
            items,
            ["mdir", "mdir-red", "mdir-green", "dark", "light", "mine"],
            "built-ins first, user themes after; same name is listed once"
        );
        assert_eq!(app.menu.as_ref().unwrap().menu.cursor, 3, "current theme");
    }

    #[test]
    fn theme_cursor_move_previews_and_esc_restores() {
        let (_t, cfg, mut app) = app_with_dir("");
        let before = fs::read_to_string(cfg.path().join("config.toml")).unwrap();
        open_theme_menu(&mut app);
        assert_eq!(app.theme.name, "mdir");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.theme.name, "mdir-red");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.theme.name, "mdir-green");
        press(&mut app, KeyCode::Esc);
        assert!(app.menu.is_none());
        assert_eq!(app.theme.name, "mdir", "Esc restores the previous theme");
        assert_eq!(app.settings.config.theme.classic, "mdir");
        assert_eq!(
            fs::read_to_string(cfg.path().join("config.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn theme_enter_saves_for_the_current_mode_preserving_the_file() {
        let (_t, cfg, mut app) = app_with_dir("# mine\n[keys]\npreset = \"mdir\"\n");
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down); // dark
        press(&mut app, KeyCode::Enter);
        settle_task(&mut app);
        assert!(app.menu.is_none());
        assert_eq!(app.theme.name, "dark");
        assert_eq!(app.settings.config.theme.classic, "dark");
        assert_eq!(app.settings.config.theme.dev, "mdir");
        let saved = fs::read_to_string(cfg.path().join("config.toml")).unwrap();
        assert!(
            saved.starts_with("# mine\n[keys]\npreset = \"mdir\"\n"),
            "{saved}"
        );
        assert!(saved.contains("[theme]\nclassic = \"dark\""), "{saved}");
        assert!(app.status.as_deref().unwrap().contains("dark"));
        // survives a reload from disk
        app.reload_config();
        settle_task(&mut app);
        assert_eq!(app.theme.name, "dark");
    }

    #[test]
    fn theme_save_targets_dev_key_in_dev_mode_and_keeps_classic() {
        let (_t, cfg, mut app) = app_with_dir("[theme]\nclassic = \"light\"\n");
        app.set_mode(Mode::Dev);
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // mdir-red
        settle_task(&mut app);
        assert_eq!(app.theme.name, "mdir-red");
        app.set_mode(Mode::Classic);
        assert_eq!(app.theme.name, "light", "classic untouched");
        let (c, w) = config::load_config(Some(cfg.path()));
        assert!(w.is_empty());
        assert_eq!(
            (c.theme.classic.as_str(), c.theme.dev.as_str()),
            ("light", "mdir-red")
        );
    }

    #[test]
    fn theme_preview_honors_256_color_fallback() {
        let (_t, mut app) = fixture();
        app.configure(None, Some("256color"));
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.theme, theme::builtin("mdir-red").unwrap().to_256());
    }

    #[test]
    fn reload_config_runs_on_the_background_runner() {
        let (_t, cfg, mut app) = app_with_dir("");
        fs::write(
            cfg.path().join("config.toml"),
            "[theme]\nclassic = \"dark\"\n",
        )
        .unwrap();
        app.reload_config();
        assert!(app.task.is_some(), "IO runs on a worker");
        assert_eq!(app.status.as_deref(), Some("설정 읽는 중…"));
        assert_eq!(app.theme.name, "mdir", "applied only from tick()");
        settle_task(&mut app);
        assert_eq!(app.theme.name, "dark");
        assert_eq!(app.status.as_deref(), Some("설정을 다시 읽었음"));
    }

    #[test]
    fn theme_commit_saves_on_the_background_runner() {
        let (_t, cfg, mut app) = app_with_dir("");
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(app.task.is_some(), "save runs on a worker");
        assert_eq!(app.status.as_deref(), Some("테마 저장 중…"));
        assert_eq!(app.theme.name, "mdir-red", "applied in memory at once");
        settle_task(&mut app);
        assert_eq!(app.status.as_deref(), Some("테마 저장: mdir-red (classic)"));
        let (c, _) = config::load_config(Some(cfg.path()));
        assert_eq!(c.theme.classic, "mdir-red");
    }

    #[test]
    fn theme_preview_does_no_io_and_starts_no_task() {
        let (_t, _cfg, mut app) = app_with_dir("");
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert!(app.task.is_none());
        assert_eq!(app.theme.name, "mdir-green");
    }

    #[test]
    fn config_io_is_refused_while_another_task_runs() {
        let (_t, cfg, mut app) = app_with_dir("");
        let (_tx, rx) = std::sync::mpsc::channel();
        app.task = Some(BgTask {
            rx,
            kind: BgKind::Undo,
        });
        fs::write(
            cfg.path().join("config.toml"),
            "[theme]\nclassic = \"dark\"\n",
        )
        .unwrap();
        app.reload_config();
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert_eq!(app.theme.name, "mdir");
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
        assert_eq!(app.theme.name, "mdir", "nothing half-applied");
        assert_eq!(app.settings.config.theme.classic, "mdir");
    }

    #[test]
    fn theme_save_failure_is_reported_and_theme_stays_for_the_session() {
        let (_t, mut app) = fixture(); // no config dir
        open_theme_menu(&mut app);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        settle_task(&mut app);
        assert!(
            app.status.as_deref().unwrap().contains("저장 실패"),
            "{:?}",
            app.status
        );
        assert_eq!(app.theme.name, "mdir-red");
    }

    #[test]
    fn menu_keys_are_remappable() {
        let (_t, _c, mut app) = app_with_dir("[keys.menu]\ncancel = [\"x\"]\n");
        press(&mut app, KeyCode::F(9));
        press(&mut app, KeyCode::Esc);
        assert!(app.menu.is_some(), "Esc was rebound away");
        press(&mut app, KeyCode::Char('x'));
        assert!(app.menu.is_none());
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

    /// Tick until the background task (if any) has been applied (5s timeout).
    fn settle_task(app: &mut App) {
        let start = Instant::now();
        while app.task.is_some() {
            assert!(start.elapsed() < Duration::from_secs(5), "task timed out");
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
    // ---- HDIR-13: F3 viewer ----

    fn cursor_to(app: &mut App, name: &str) {
        let i = app
            .active_panel()
            .entries
            .iter()
            .position(|e| e.name == name)
            .unwrap();
        app.active_panel_mut().cursor = i;
    }

    fn open_view(app: &mut App, name: &str) {
        cursor_to(app, name);
        press(app, KeyCode::F(3));
    }

    fn draw_app(app: &mut App, w: u16, h: u16) -> Vec<String> {
        use unicode_width::UnicodeWidthStr;
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x += sym.width().max(1) as u16;
                }
                row
            })
            .collect()
    }

    #[test]
    fn f3_opens_viewer_and_loads_text_on_the_worker() {
        let (t, mut app) = fixture();
        fs::write(t.path().join("f.txt"), "하나\ntwo\n\tthree\n").unwrap();
        open_view(&mut app, "f.txt");
        let v = app.viewer.as_ref().unwrap();
        assert_eq!(v.path.file_name().unwrap(), "f.txt");
        assert_eq!(v.content, crate::viewer::Content::Loading);
        assert!(app.task.is_none(), "own slot, not the shared BgTask");
        assert!(app.viewer_load.is_some(), "load runs on a worker");
        settle_view(&mut app);
        match &app.viewer.as_ref().unwrap().content {
            crate::viewer::Content::Text { lines, truncated } => {
                assert_eq!(lines[..2], ["하나", "two"]);
                assert!(!truncated);
            }
            c => panic!("{c:?}"),
        }
        let rows = draw_app(&mut app, 60, 10);
        assert!(
            rows[0].contains("f.txt") && rows[0].contains("1/3"),
            "{rows:?}"
        );
        assert!(rows[1].starts_with("하나"));
    }

    #[test]
    fn f3_on_dir_or_parent_does_nothing() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(3)); // ".."
        assert!(app.viewer.is_none() && app.task.is_none() && app.status.is_none());
        open_view(&mut app, "adir");
        assert!(app.viewer.is_none() && app.task.is_none() && app.status.is_none());
    }

    #[test]
    fn viewer_swallows_panel_keys_and_closes_with_q_esc_f3_f10() {
        for close in [
            KeyCode::Char('q'),
            KeyCode::Esc,
            KeyCode::F(3),
            KeyCode::F(10),
        ] {
            let (_t, mut app) = fixture();
            open_view(&mut app, "f.txt");
            settle_view(&mut app);
            press(&mut app, KeyCode::Tab);
            assert_eq!(app.active, 0, "Tab goes to the viewer, not the panels");
            press(&mut app, close);
            assert!(app.viewer.is_none() && app.running, "{close:?}");
        }
    }

    #[test]
    fn viewer_scrolls_searches_and_hscrolls_through_the_keymap() {
        let (t, mut app) = fixture();
        let body: String = (0..50).map(|i| format!("line{i}\n")).collect();
        fs::write(t.path().join("f.txt"), body).unwrap();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        draw_app(&mut app, 40, 12); // 10 body rows
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 10);
        press(&mut app, KeyCode::End);
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 40);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Char('/'));
        for c in "line1".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let rows = draw_app(&mut app, 40, 12);
        assert!(rows[11].contains("/line1"), "{:?}", rows[11]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.viewer.as_ref().unwrap().match_line, Some(1));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.viewer.as_ref().unwrap().match_line, Some(10));
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Left);
        assert!(app.viewer.is_some());
    }

    #[test]
    fn typing_q_in_search_input_does_not_close_the_viewer() {
        let (_t, mut app) = fixture();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.viewer.is_some());
        assert_eq!(app.viewer.as_ref().unwrap().input.as_deref(), Some("q"));
        press(&mut app, KeyCode::Esc);
        assert!(app.viewer.is_some(), "Esc only closes the input line");
        press(&mut app, KeyCode::Esc);
        assert!(app.viewer.is_none());
    }

    #[test]
    fn closing_the_viewer_while_loading_drops_the_result() {
        let (_t, mut app) = fixture();
        open_view(&mut app, "f.txt");
        press(&mut app, KeyCode::Char('q'));
        assert!(app.viewer.is_none());
        settle_view(&mut app);
        assert!(app.viewer.is_none());
    }

    #[test]
    fn binary_file_shows_kind_and_size_summary() {
        let (t, mut app) = fixture();
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0u8; 2000]);
        fs::write(t.path().join("f.txt"), &bytes).unwrap();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        let rows = draw_app(&mut app, 60, 8);
        assert!(
            rows.join("").contains("바이너리 파일 (PNG 이미지, 2.0K)"),
            "{rows:?}"
        );
    }

    #[test]
    fn viewer_uses_the_larger_cap_and_marks_truncation() {
        let (t, mut app) = fixture();
        let many: String = (0..120_000).map(|_| "x\n").collect();
        fs::write(t.path().join("f.txt"), many).unwrap();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        match &app.viewer.as_ref().unwrap().content {
            crate::viewer::Content::Text { lines, truncated } => {
                assert_eq!(lines.len(), 100_000);
                assert!(truncated);
            }
            c => panic!("{c:?}"),
        }
        let rows = draw_app(&mut app, 70, 6);
        assert!(
            rows[0].contains("잘림") && rows[0].contains("/100000"),
            "{:?}",
            rows[0]
        );
    }

    fn settle_view(app: &mut App) {
        let start = Instant::now();
        loop {
            app.tick();
            match &app.viewer {
                Some(v) if v.content == crate::viewer::Content::Loading => {}
                _ => return,
            }
            assert!(start.elapsed() < Duration::from_secs(5), "viewer timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn text_of(v: &crate::viewer::Viewer) -> String {
        match &v.content {
            crate::viewer::Content::Text { lines, .. } => lines.join("\n"),
            c => format!("{c:?}"),
        }
    }

    /// Loader that blocks on `f.txt` until the returned guard is dropped.
    fn gate_viewer_loader(app: &mut App) -> (Arc<Mutex<()>>, Arc<Mutex<Vec<String>>>) {
        let gate = Arc::new(Mutex::new(()));
        let g = Arc::clone(&gate);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c = Arc::clone(&calls);
        app.viewer_loader = Arc::new(move |p: &std::path::Path| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            c.lock().unwrap().push(name.clone());
            if name == "f.txt" {
                drop(g.lock().unwrap());
            }
            Preview::Text {
                lines: vec![format!("loaded {name}")],
                truncated: false,
            }
        });
        (gate, calls)
    }

    #[test]
    fn closing_a_slow_viewer_load_frees_other_tasks_at_once() {
        let (t, mut app) = fixture();
        let (gate, _calls) = gate_viewer_loader(&mut app);
        let guard = gate.lock().unwrap();
        open_view(&mut app, "f.txt");
        press(&mut app, KeyCode::Esc);
        assert!(app.viewer.is_none());
        press(&mut app, KeyCode::F(7));
        assert!(app.dialog.is_some(), "mkdir dialog opens: {:?}", app.status);
        assert_ne!(app.status.as_deref(), Some("작업 진행 중"));
        type_str(&mut app, "newdir");
        press(&mut app, KeyCode::Enter);
        wait_idle(&mut app);
        assert!(t.path().join("newdir").is_dir());
        drop(guard);
        std::thread::sleep(Duration::from_millis(30));
        app.tick();
        assert!(app.viewer.is_none(), "late result is ignored");
    }

    #[test]
    fn a_late_result_of_an_abandoned_load_never_reaches_a_newer_viewer() {
        let (t, mut app) = fixture();
        fs::write(t.path().join("other.txt"), "x").unwrap();
        app.reload_panel(0, None);
        settle(&mut app);
        let (gate, _calls) = gate_viewer_loader(&mut app);
        let guard = gate.lock().unwrap();
        open_view(&mut app, "f.txt");
        press(&mut app, KeyCode::Esc);
        open_view(&mut app, "other.txt");
        settle_view(&mut app);
        assert_eq!(text_of(app.viewer.as_ref().unwrap()), "loaded other.txt");
        drop(guard);
        std::thread::sleep(Duration::from_millis(30));
        app.tick();
        assert_eq!(text_of(app.viewer.as_ref().unwrap()), "loaded other.txt");
    }

    #[test]
    fn viewer_opens_even_while_another_task_runs() {
        let (_t, mut app) = fixture();
        app.reload_config();
        open_view(&mut app, "f.txt");
        assert!(app.viewer.is_some());
        settle_view(&mut app);
        assert!(matches!(
            app.viewer.as_ref().unwrap().content,
            crate::viewer::Content::Text { .. }
        ));
    }

    #[test]
    fn f1_in_viewer_shows_viewer_help_over_it_and_esc_returns() {
        let (_t, mut app) = fixture();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        press(&mut app, KeyCode::F(1));
        let scr = app.screen.as_ref().expect("help opened");
        let all = scr.lines.join("\n");
        assert!(all.contains("[뷰어 / 도움말]"), "{all}");
        assert!(
            !all.contains("[패널]") && !all.contains("[휴지통]"),
            "{all}"
        );
        let rows = draw_app(&mut app, 80, 20);
        assert!(rows.join("").contains("뷰어 / 도움말"), "{rows:?}");
        press(&mut app, KeyCode::Esc);
        assert!(app.screen.is_none());
        assert!(app.viewer.is_some(), "back in the viewer");
    }

    #[cfg(unix)]
    #[test]
    fn f4_in_viewer_edits_the_viewed_file_then_reloads_the_view() {
        let (t, mut app) = fixture();
        let log = Log::default();
        let l = Arc::clone(&log);
        app.editor_runner = Arc::new(move |argv: &[OsString]| {
            let path = std::path::PathBuf::from(argv.last().unwrap());
            l.lock().unwrap().push(path.to_string_lossy().into_owned());
            fs::write(path, "edited body").unwrap();
            Ok(())
        });
        app.editor_vars = (None, Some("vim".into()));
        fs::write(t.path().join("f.txt"), "old body").unwrap();
        open_view(&mut app, "f.txt");
        settle_view(&mut app);
        assert_eq!(text_of(app.viewer.as_ref().unwrap()), "old body");
        press(&mut app, KeyCode::F(4));
        assert!(app.has_pending_edit());
        assert!(app.viewer.is_some());
        run_logged(&mut app, &log);
        assert!(log.lock().unwrap().iter().any(|e| e.ends_with("/f.txt")));
        let start = Instant::now();
        while text_of(app.viewer.as_ref().unwrap()) != "edited body" {
            app.tick();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "view not reloaded"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    // ---- HDIR-14: F4 editor ----

    #[cfg(unix)]
    type Log = Arc<Mutex<Vec<String>>>;

    #[cfg(unix)]
    /// Fake editor that logs its argv and creates `new.txt` next to the file.
    fn fake_editor(app: &mut App, log: &Log, result: EditResult) {
        let log = Arc::clone(log);
        app.editor_runner = Arc::new(move |argv: &[OsString]| {
            let path = std::path::PathBuf::from(argv.last().unwrap());
            log.lock().unwrap().push(format!(
                "edit {}",
                argv.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("|")
            ));
            fs::write(path.with_file_name("new.txt"), "n").unwrap();
            result.clone()
        });
    }

    #[cfg(unix)]
    fn run_logged(app: &mut App, log: &Log) {
        let l = Arc::clone(log);
        app.run_edit(move |f| {
            l.lock().unwrap().push("suspend".into());
            let r = f();
            l.lock().unwrap().push("resume".into());
            Ok(r)
        });
    }

    #[cfg(unix)]
    #[test]
    fn f4_only_queues_the_edit_without_running_the_editor() {
        let (_t, mut app) = fixture();
        let log = Log::default();
        fake_editor(&mut app, &log, Ok(()));
        app.editor_vars = (None, Some("vim".into()));
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        assert!(app.has_pending_edit());
        assert!(log.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn run_edit_suspends_runs_editor_inside_then_reloads_keeping_the_cursor() {
        let (t, mut app) = fixture();
        let log = Log::default();
        fake_editor(&mut app, &log, Ok(()));
        app.editor_vars = (Some("code -w".into()), Some("ignored".into()));
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        run_logged(&mut app, &log);
        let got = log.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], "suspend");
        assert!(
            got[1].starts_with("edit code|-w|") && got[1].ends_with("/f.txt"),
            "{got:?}"
        );
        assert_eq!(got[2], "resume");
        assert!(!app.has_pending_edit());
        settle(&mut app);
        for i in 0..2 {
            let names: Vec<_> = app.panels[i]
                .entries
                .iter()
                .map(|e| e.name.clone())
                .collect();
            assert!(
                names.contains(&"new.txt".to_string()),
                "panel {i} reloaded: {names:?}"
            );
        }
        assert_eq!(name_at(&app, 0), "f.txt", "cursor stays on the edited file");
        assert_eq!(app.status.as_deref(), Some("편집 완료: f.txt"));
        drop(t);
    }

    #[cfg(unix)]
    #[test]
    fn failed_editor_reports_error_and_still_reloads() {
        let (_t, mut app) = fixture();
        let log = Log::default();
        fake_editor(
            &mut app,
            &log,
            Err("편집기가 오류로 종료됨 (종료 코드 1)".into()),
        );
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        run_logged(&mut app, &log);
        settle(&mut app);
        assert!(app.status.as_deref().unwrap().contains("종료 코드 1"));
        assert!(app.panels[0].entries.iter().any(|e| e.name == "new.txt"));
        assert_eq!(name_at(&app, 0), "f.txt");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_suspend_failure_is_reported() {
        let (_t, mut app) = fixture();
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        app.run_edit(|_| Err(std::io::Error::other("tty gone")));
        assert!(app.status.as_deref().unwrap().contains("tty gone"));
        assert!(!app.has_pending_edit());
    }

    #[test]
    fn f4_on_dir_or_parent_queues_nothing() {
        let (_t, mut app) = fixture();
        press(&mut app, KeyCode::F(4));
        cursor_to(&mut app, "adir");
        press(&mut app, KeyCode::F(4));
        assert!(!app.has_pending_edit() && app.status.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unparsable_editor_variable_is_a_status_error() {
        let (_t, mut app) = fixture();
        app.editor_vars = (Some("vim 'x".into()), None);
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        assert!(!app.has_pending_edit());
        assert!(app.status.as_deref().unwrap().contains("VISUAL"));
    }

    #[cfg(windows)]
    #[test]
    fn f4_on_windows_says_support_is_coming() {
        let (_t, mut app) = fixture();
        cursor_to(&mut app, "f.txt");
        press(&mut app, KeyCode::F(4));
        assert!(!app.has_pending_edit());
        assert!(app.status.as_deref().unwrap().contains("Windows 지원 예정"));
    }
    // ---- HDIR-16: modes ----

    #[test]
    fn ctrl_t_switches_layout_and_page_height() {
        let (_t, mut app) = fixture();
        let rows = draw_app(&mut app, 100, 30);
        assert_eq!(app.page_height, 25);
        assert!(!rows.iter().any(|r| r.starts_with("> ")));
        press_mods(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, crate::config::Mode::Dev);
        let rows = draw_app(&mut app, 100, 30);
        assert_eq!(app.page_height, 16, "panels are shorter in Dev");
        assert!(rows[27].starts_with("> "), "{:?}", rows[27]);
        assert!(rows.iter().any(|r| r.contains("미리보기")));
        press_mods(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let rows = draw_app(&mut app, 100, 30);
        assert_eq!(app.page_height, 25);
        assert!(!rows.iter().any(|r| r.starts_with("> ")));
    }

    #[test]
    fn page_down_follows_the_dev_page_height() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..80 {
            fs::write(tmp.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());
        settle(&mut app);
        press_mods(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        draw_app(&mut app, 100, 30);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.panels[0].cursor, 16);
    }
    // ---- HDIR-15: preview panel ----

    fn dev_app() -> (TempDir, App) {
        let (t, mut app) = fixture();
        app.set_mode(Mode::Dev);
        app.preview_debounce = Duration::ZERO;
        (t, app)
    }

    fn settle_preview(app: &mut App) {
        let start = Instant::now();
        loop {
            app.tick();
            if !app.preview_pending() {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "preview timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn counting_runner(app: &mut App) -> Log2 {
        let log = Log2::default();
        let l = Arc::clone(&log);
        app.preview_runner = Arc::new(move |p: &std::path::Path| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            l.lock().unwrap().push(name.clone());
            Preview::Text {
                lines: vec![format!("content of {name}")],
                truncated: false,
            }
        });
        log
    }

    type Log2 = Arc<Mutex<Vec<String>>>;

    #[test]
    fn dev_preview_shows_the_cursor_entry_text() {
        let (t, mut app) = dev_app();
        fs::write(t.path().join("f.txt"), "hello preview\n둘째 줄\n").unwrap();
        cursor_to(&mut app, "f.txt");
        app.tick();
        assert!(app.preview_pending(), "computed on a worker, not inline");
        let rows = draw_app(&mut app, 100, 30);
        assert!(rows.iter().any(|r| r.contains("읽는 중")), "{rows:?}");
        settle_preview(&mut app);
        let rows = draw_app(&mut app, 100, 30);
        let pv: Vec<_> = rows[19..27].iter().map(|r| r.replace(' ', "")).collect();
        assert!(pv[0].contains("미리보기:f.txt"), "{pv:?}");
        assert!(pv[1].contains("hellopreview"), "{pv:?}");
        assert!(pv[2].contains("둘째줄"), "{pv:?}");
    }

    #[test]
    fn classic_mode_computes_no_preview() {
        let (_t, mut app) = fixture();
        let log = counting_runner(&mut app);
        app.preview_debounce = Duration::ZERO;
        cursor_to(&mut app, "f.txt");
        for _ in 0..5 {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(log.lock().unwrap().is_empty());
        assert!(!app.preview_pending());
    }

    #[test]
    fn debounce_computes_only_the_latest_cursor_position() {
        let (_t, mut app) = dev_app();
        let log = counting_runner(&mut app);
        app.preview_debounce = Duration::from_millis(60);
        cursor_to(&mut app, "adir");
        app.tick();
        cursor_to(&mut app, "bdir");
        app.tick();
        cursor_to(&mut app, "f.txt");
        app.tick();
        assert!(
            log.lock().unwrap().is_empty(),
            "nothing before the debounce"
        );
        std::thread::sleep(Duration::from_millis(90));
        settle_preview(&mut app);
        assert_eq!(*log.lock().unwrap(), ["f.txt"]);
    }

    #[test]
    fn stale_results_are_dropped_by_id() {
        let (_t, mut app) = dev_app();
        let gate = Arc::new(Mutex::new(()));
        let guard = gate.lock().unwrap();
        let g = Arc::clone(&gate);
        let log = Log2::default();
        let l = Arc::clone(&log);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let done_tx = Mutex::new(done_tx);
        app.preview_runner = Arc::new(move |p: &std::path::Path| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            l.lock().unwrap().push(name.clone());
            if name == "f.txt" {
                drop(g.lock().unwrap());
            }
            let result = Preview::Text {
                lines: vec![format!("content of {name}")],
                truncated: false,
            };
            if name == "f.txt" {
                // The worker is about to send its (now stale) result.
                done_tx.lock().unwrap().send(()).unwrap();
            }
            result
        });
        cursor_to(&mut app, "f.txt");
        app.tick(); // slow request for f.txt is now blocked
        cursor_to(&mut app, "bdir");
        app.tick();
        drop(guard); // the stale f.txt result lands now
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("f.txt worker finished and sent its result");
        // bdir's worker can only start once the stale f.txt result has been
        // received (and dropped): wait for that.
        let start = Instant::now();
        while !log.lock().unwrap().contains(&"bdir".to_string()) {
            app.tick();
            assert!(start.elapsed() < Duration::from_secs(5), "bdir never ran");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.preview.shown.is_none(),
            "stale f.txt result must not be shown: {:?}",
            app.preview.shown
        );
        settle_preview(&mut app);
        assert!(
            matches!(app.preview_shown(), Some(Preview::Text { lines, .. }) if lines[0] == "content of bdir"),
            "{:?}",
            app.preview_shown()
        );
    }

    #[test]
    fn rapid_cursor_moves_keep_one_read_in_flight_and_the_latest_pending() {
        let (_t, mut app) = dev_app();
        let gate = Arc::new(Mutex::new(()));
        let guard = gate.lock().unwrap();
        let g = Arc::clone(&gate);
        let log = Log2::default();
        let l = Arc::clone(&log);
        app.preview_runner = Arc::new(move |p: &std::path::Path| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            l.lock().unwrap().push(name.clone());
            drop(g.lock().unwrap());
            Preview::Text {
                lines: vec![format!("content of {name}")],
                truncated: false,
            }
        });
        let names = ["adir", "bdir", "f.txt"];
        for i in 0..10 {
            cursor_to(&mut app, names[i % 3]); // last move: names[0] = adir
            app.tick();
        }
        std::thread::sleep(Duration::from_millis(60));
        app.tick();
        assert!(
            log.lock().unwrap().len() <= 1,
            "one read in flight: {:?}",
            log.lock().unwrap()
        );
        drop(guard);
        settle_preview(&mut app);
        assert!(
            log.lock().unwrap().len() <= 2,
            "in flight + latest pending only: {:?}",
            log.lock().unwrap()
        );
        assert_eq!(log.lock().unwrap().last().unwrap(), "adir");
        assert!(
            matches!(app.preview_shown(), Some(Preview::Text { lines, .. }) if lines[0] == "content of adir"),
            "{:?}",
            app.preview_shown()
        );
    }

    #[test]
    fn old_result_is_not_shown_for_a_different_entry() {
        let (_t, mut app) = dev_app();
        counting_runner(&mut app);
        cursor_to(&mut app, "f.txt");
        settle_preview(&mut app);
        assert!(app.preview_shown().is_some());
        cursor_to(&mut app, "adir");
        app.tick();
        assert!(app.preview_shown().is_none(), "no stale content for adir");
        let rows = draw_app(&mut app, 100, 30);
        assert!(!rows.join("").contains("content of f.txt"));
        settle_preview(&mut app);
        let rows = draw_app(&mut app, 100, 30);
        assert!(rows.join("").contains("content of adir"));
        assert!(!rows.join("").contains("content of f.txt"));
    }

    #[test]
    fn dir_entry_shows_a_summary_including_unreadable_count() {
        let (_t, mut app) = dev_app();
        cursor_to(&mut app, "adir");
        settle_preview(&mut app);
        let rows = draw_app(&mut app, 100, 30);
        assert!(
            rows.iter().any(|r| r.contains("폴더 0개, 파일 1개")),
            "{rows:?}"
        );
        app.preview_runner = Arc::new(|_| Preview::Dir {
            files: 1,
            dirs: 1,
            total_size: 0,
            truncated: false,
            errors: 3,
        });
        cursor_to(&mut app, "bdir");
        settle_preview(&mut app);
        let rows = draw_app(&mut app, 100, 30);
        assert!(rows.iter().any(|r| r.contains("읽지 못함 3개")), "{rows:?}");
    }

    #[test]
    fn parent_entry_has_nothing_to_preview() {
        let (_t, mut app) = dev_app();
        let log = counting_runner(&mut app);
        for _ in 0..3 {
            app.tick();
        }
        assert!(log.lock().unwrap().is_empty(), "cursor starts on '..'");
        assert!(!app.preview_pending());
        assert!(app.preview_shown().is_none());
    }

    #[test]
    fn opening_the_trash_view_clears_the_preview_and_computes_no_more() {
        let (_t, mut app, _fake) = fixture_trash();
        app.set_mode(Mode::Dev);
        app.preview_debounce = Duration::ZERO;
        let log = counting_runner(&mut app);
        cursor_to(&mut app, "f.txt");
        settle_preview(&mut app);
        assert_eq!(*log.lock().unwrap(), ["f.txt"]);
        let rows = draw_app(&mut app, 100, 30);
        assert!(rows.join("").contains("content of f.txt"), "{rows:?}");

        alt_f8(&mut app);
        assert!(app.trash_views[0].is_some(), "trash view really opened");
        wait_idle(&mut app);
        for _ in 0..5 {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.trash_views[0].is_some());
        assert!(!app.preview_pending());
        assert!(app.preview_shown().is_none());
        assert!(app.preview.shown.is_none(), "cleared, not just hidden");
        let rows = draw_app(&mut app, 100, 30);
        assert!(!rows.join("").contains("content of f.txt"), "{rows:?}");
        assert_eq!(*log.lock().unwrap(), ["f.txt"], "no new runner call");
    }

    #[test]
    fn preview_refreshes_after_the_panel_reloads() {
        let (t, mut app) = dev_app();
        fs::write(t.path().join("f.txt"), "version1").unwrap();
        cursor_to(&mut app, "f.txt");
        settle_preview(&mut app);
        let rows = draw_app(&mut app, 100, 30);
        assert!(rows.join("").contains("version1"));
        fs::write(t.path().join("f.txt"), "version2").unwrap();
        app.reload_panel(0, Some(OsString::from("f.txt")));
        settle(&mut app);
        settle_preview(&mut app);
        let start = Instant::now();
        loop {
            let rows = draw_app(&mut app, 100, 30);
            if rows.join("").contains("version2") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "{rows:?}");
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // ---- HDIR-26/27/28/29: extension associations ----

    mod assoc_run {
        use super::*;
        use crate::launcher::Launcher;
        use std::path::Path;

        #[derive(Default)]
        struct Fake {
            log: Arc<Mutex<Vec<String>>>,
            fail_run: Mutex<Option<String>>,
            fail_spawn: Mutex<Option<String>>,
            spawn_delay: Mutex<Option<Duration>>,
        }

        fn line(kind: &str, argv: &[OsString], cwd: &Path) -> String {
            let a: Vec<_> = argv.iter().map(|a| a.to_string_lossy()).collect();
            format!("{kind} {} @{}", a.join("|"), cwd.display())
        }

        impl Launcher for Fake {
            fn run(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
                self.log.lock().unwrap().push(line("run", argv, cwd));
                self.fail_run.lock().unwrap().clone().map_or(Ok(()), Err)
            }
            fn spawn_detached(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
                let delay = *self.spawn_delay.lock().unwrap();
                if let Some(d) = delay {
                    std::thread::sleep(d);
                }
                self.log.lock().unwrap().push(line("spawn", argv, cwd));
                self.fail_spawn.lock().unwrap().clone().map_or(Ok(()), Err)
            }
            fn wait_key(&self) {
                self.log.lock().unwrap().push("wait_key".into());
            }
        }

        #[cfg_attr(windows, allow(dead_code))]
        struct Env {
            _t: TempDir,
            _cfg: TempDir,
            app: App,
            log: Arc<Mutex<Vec<String>>>,
            fake: Arc<Fake>,
        }

        fn env(rules: &str) -> Env {
            let (t, mut app) = fixture();
            let cfg = config_with(rules);
            app.configure(Some(cfg.path().to_path_buf()), Some("truecolor"));
            let fake = Arc::new(Fake::default());
            let log = Arc::clone(&fake.log);
            app.launcher = fake.clone();
            cursor_to(&mut app, "f.txt");
            Env {
                _t: t,
                _cfg: cfg,
                app,
                log,
                fake,
            }
        }

        const CAT: &str = "[[assoc]]\next = [\"txt\"]\ncmd = \"cat {}\"\n";

        #[cfg_attr(windows, allow(dead_code))]
        fn run_pending_logged(e: &mut Env) {
            let l = Arc::clone(&e.log);
            e.app.run_pending(move |f| {
                l.lock().unwrap().push("suspend".into());
                let r = f();
                l.lock().unwrap().push("resume".into());
                Ok(r)
            });
        }

        fn log(e: &Env) -> Vec<String> {
            e.log.lock().unwrap().clone()
        }

        fn tick_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
            let start = Instant::now();
            while !done(app) {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "timed out: {what}"
                );
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[cfg(unix)]
        #[test]
        fn enter_runs_a_foreground_rule_suspended_in_the_panel_cwd() {
            let mut e = env(CAT);
            press(&mut e.app, KeyCode::Enter);
            assert!(e.app.has_pending_run(), "queued for the main loop");
            assert!(log(&e).is_empty(), "nothing runs before the suspend");
            run_pending_logged(&mut e);
            let got = log(&e);
            assert_eq!(got.len(), 3, "{got:?}");
            assert_eq!(got[0], "suspend");
            let cwd = e.app.panels[0].cwd.display().to_string();
            assert!(
                got[1].starts_with("run ")
                    && got[1].contains("|-c|cat \"$1\"|hdir|")
                    && got[1].contains(&format!("{cwd}/f.txt|{cwd}|f @{cwd}")),
                "{got:?}"
            );
            assert_eq!(got[2], "resume");
            assert!(!e.app.has_pending_run());
            assert_eq!(e.app.status.as_deref(), Some("실행 완료: f.txt"));
        }

        #[cfg(unix)]
        #[test]
        fn wait_rule_reads_a_key_inside_the_suspension() {
            let mut e = env(&format!("{CAT}wait = true\n"));
            press(&mut e.app, KeyCode::Enter);
            run_pending_logged(&mut e);
            let got = log(&e);
            assert_eq!(got.len(), 4, "{got:?}");
            assert_eq!(got[0], "suspend");
            assert!(got[1].starts_with("run "));
            assert_eq!(got[2], "wait_key");
            assert_eq!(got[3], "resume");
        }

        #[cfg(unix)]
        #[test]
        fn background_rule_spawns_at_once_and_never_suspends() {
            let mut e = env(&format!("{CAT}background = true\n"));
            press(&mut e.app, KeyCode::Enter);
            assert!(!e.app.has_pending_run());
            tick_until(&mut e.app, "status", |a| {
                a.status.as_deref() == Some("실행: f.txt")
            });
            let got = log(&e);
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].starts_with("spawn ") && got[0].contains("cat \"$1\""));
        }

        #[cfg(unix)]
        #[test]
        fn foreground_failure_and_spawn_failure_report_cmd_and_reason() {
            let mut e = env(CAT);
            *e.fake.fail_run.lock().unwrap() = Some("종료 코드 1".into());
            press(&mut e.app, KeyCode::Enter);
            run_pending_logged(&mut e);
            assert_eq!(e.app.status.as_deref(), Some("오류: cat {}: 종료 코드 1"));
            let mut e = env(&format!("{CAT}background = true\n"));
            *e.fake.fail_spawn.lock().unwrap() = Some("boom".into());
            press(&mut e.app, KeyCode::Enter);
            tick_until(&mut e.app, "error", |a| {
                a.status.as_deref() == Some("오류: cat {}: boom")
            });
        }

        #[cfg(unix)]
        #[test]
        fn terminal_failure_on_resume_is_reported() {
            let mut e = env(CAT);
            press(&mut e.app, KeyCode::Enter);
            e.app
                .run_pending(|_| Err(std::io::Error::other("tty gone")));
            assert!(e.app.status.as_deref().unwrap().contains("tty gone"));
        }

        #[test]
        fn no_rule_text_opens_the_viewer_without_launching() {
            let mut e = env("");
            e.app.text_probe = Arc::new(|_| Ok(true));
            press(&mut e.app, KeyCode::Enter);
            tick_until(&mut e.app, "viewer", |a| a.viewer.is_some());
            assert!(log(&e).is_empty() && !e.app.has_pending_run());
        }

        #[test]
        fn real_probe_treats_utf8_as_text_and_nul_bytes_as_binary() {
            let mut e = env("");
            press(&mut e.app, KeyCode::Enter); // f.txt = "x"
            tick_until(&mut e.app, "viewer", |a| a.viewer.is_some());

            let mut e = env("");
            let root = e.app.panels[0].cwd.clone();
            fs::write(root.join("blob.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
            e.app.reload_panel(0, None);
            settle(&mut e.app);
            cursor_to(&mut e.app, "blob.bin");
            press(&mut e.app, KeyCode::Enter);
            let l = Arc::clone(&e.log);
            tick_until(&mut e.app, "spawn", move |a| {
                !l.lock().unwrap().is_empty() || a.status.as_deref() == Some("Windows 지원 예정")
            });
            assert!(e.app.viewer.is_none());
        }

        #[cfg(unix)]
        #[test]
        fn no_rule_binary_goes_to_the_system_opener_in_the_background() {
            let mut e = env("");
            e.app.text_probe = Arc::new(|_| Ok(false));
            press(&mut e.app, KeyCode::Enter);
            let l = Arc::clone(&e.log);
            tick_until(&mut e.app, "spawn", move |a| {
                !l.lock().unwrap().is_empty() && a.status.is_some()
            });
            let got = log(&e);
            let cwd = e.app.panels[0].cwd.display().to_string();
            assert_eq!(
                got,
                vec![format!(
                    "spawn {}|{cwd}/f.txt @{cwd}",
                    crate::assoc::system_opener()
                )]
            );
            assert!(e.app.viewer.is_none() && !e.app.has_pending_run());
            assert_eq!(e.app.status.as_deref(), Some("실행: f.txt"));
        }

        #[cfg(unix)]
        #[test]
        fn system_opener_spawn_failure_reports_error() {
            let mut e = env("");
            e.app.text_probe = Arc::new(|_| Ok(false));
            *e.fake.fail_spawn.lock().unwrap() = Some("no such file".into());
            press(&mut e.app, KeyCode::Enter);
            let want = format!("오류: {}: no such file", crate::assoc::system_opener());
            tick_until(&mut e.app, "error", |a| a.status.as_deref() == Some(&want));
        }

        #[test]
        fn text_probe_does_not_block_the_ui_thread() {
            let mut e = env("");
            let (tx, rx) = mpsc::channel::<()>();
            let rx = Mutex::new(rx);
            e.app.text_probe = Arc::new(move |_| {
                let _ = rx.lock().unwrap().recv();
                Ok(true)
            });
            press(&mut e.app, KeyCode::Enter); // returns while the probe waits
            assert!(e.app.viewer.is_none());
            tx.send(()).unwrap();
            tick_until(&mut e.app, "viewer", |a| a.viewer.is_some());
        }

        #[test]
        fn enter_on_directories_never_launches() {
            let mut e = env(CAT);
            cursor_to(&mut e.app, "adir");
            press(&mut e.app, KeyCode::Enter);
            settle(&mut e.app);
            assert!(log(&e).is_empty() && !e.app.has_pending_run());
        }

        #[cfg(windows)]
        #[test]
        fn windows_rule_run_shows_the_planned_message() {
            let mut e = env(CAT);
            press(&mut e.app, KeyCode::Enter);
            assert_eq!(e.app.status.as_deref(), Some("Windows 지원 예정"));
            assert!(!e.app.has_pending_run());
        }

        #[cfg(windows)]
        #[test]
        fn windows_system_opener_shows_the_planned_message() {
            let mut e = env("");
            e.app.text_probe = Arc::new(|_| Ok(false));
            press(&mut e.app, KeyCode::Enter);
            tick_until(&mut e.app, "msg", |a| {
                a.status.as_deref() == Some("Windows 지원 예정")
            });
        }

        // ---- review fixes: stale probes, probe errors, special files, bg spawn ----

        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AO};

        #[cfg(unix)]
        fn add_file(e: &mut Env, name: &str) {
            let root = e.app.panels[0].cwd.clone();
            fs::write(root.join(name), "y").unwrap();
            e.app.reload_panel(0, None);
            settle(&mut e.app);
        }

        fn quiet_ticks(app: &mut App) {
            for _ in 0..40 {
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// A probe that blocks for `f.txt` until `open` is set (text), and
        /// answers `Ok(false)` at once for any other file.
        fn gated_probe(open: &Arc<AtomicBool>, calls: &Arc<AtomicUsize>) -> TextProbe {
            let (open, calls) = (Arc::clone(open), Arc::clone(calls));
            Arc::new(move |p| {
                calls.fetch_add(1, AO::SeqCst);
                if p.file_name().is_some_and(|n| n == "f.txt") {
                    while !open.load(AO::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return Ok(true);
                }
                Ok(false)
            })
        }

        #[cfg(unix)]
        #[test]
        fn stale_probe_is_ignored_after_a_newer_enter() {
            let mut e = env("");
            add_file(&mut e, "g.txt");
            let (open, calls) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
            );
            e.app.text_probe = gated_probe(&open, &calls);
            cursor_to(&mut e.app, "f.txt");
            press(&mut e.app, KeyCode::Enter); // A pending
            cursor_to(&mut e.app, "g.txt");
            press(&mut e.app, KeyCode::Enter); // B supersedes A
            open.store(true, AO::SeqCst); // release A
            let l = Arc::clone(&e.log);
            tick_until(&mut e.app, "B spawn", move |_| {
                !l.lock().unwrap().is_empty()
            });
            quiet_ticks(&mut e.app);
            assert!(e.app.viewer.is_none(), "A's text result must be dropped");
            let got = log(&e);
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].contains("g.txt"), "{got:?}");
        }

        #[test]
        fn stale_probe_is_ignored_after_a_screen_opened_and_closed() {
            let mut e = env("");
            let (open, calls) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
            );
            e.app.text_probe = gated_probe(&open, &calls);
            press(&mut e.app, KeyCode::Enter); // A pending
            press(&mut e.app, KeyCode::F(1));
            assert!(e.app.screen.is_some());
            press(&mut e.app, KeyCode::Esc);
            assert!(e.app.screen.is_none());
            open.store(true, AO::SeqCst);
            quiet_ticks(&mut e.app);
            assert!(e.app.viewer.is_none() && log(&e).is_empty());
        }

        #[test]
        fn stale_probe_is_ignored_after_tab() {
            let mut e = env("");
            let (open, calls) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
            );
            e.app.text_probe = gated_probe(&open, &calls);
            press(&mut e.app, KeyCode::Enter);
            press(&mut e.app, KeyCode::Tab);
            press(&mut e.app, KeyCode::Tab);
            open.store(true, AO::SeqCst);
            quiet_ticks(&mut e.app);
            assert!(e.app.viewer.is_none() && log(&e).is_empty());
        }

        fn status_starts(e: &mut Env, prefix: &str) {
            let prefix = prefix.to_string();
            tick_until(&mut e.app, "probe error", move |a| {
                a.status.as_deref().is_some_and(|s| s.starts_with(&prefix))
            });
            assert!(e.app.viewer.is_none() && log(e).is_empty());
            assert!(!e.app.has_pending_run());
        }

        #[test]
        fn vanished_file_reports_an_error_instead_of_opening() {
            let mut e = env("");
            let root = e.app.panels[0].cwd.clone();
            fs::remove_file(root.join("f.txt")).unwrap();
            press(&mut e.app, KeyCode::Enter);
            status_starts(&mut e, "오류: f.txt: ");
        }

        #[cfg(unix)]
        #[test]
        fn unreadable_file_reports_an_error_instead_of_opening() {
            use std::os::unix::fs::PermissionsExt;
            let mut e = env("");
            let root = e.app.panels[0].cwd.clone();
            let f = root.join("f.txt");
            fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::File::open(&f).is_ok() {
                return; // running as root: cannot make it unreadable
            }
            press(&mut e.app, KeyCode::Enter);
            status_starts(&mut e, "오류: f.txt: ");
        }

        #[cfg(unix)]
        #[test]
        fn broken_symlink_reports_an_error_instead_of_opening() {
            let mut e = env("");
            let root = e.app.panels[0].cwd.clone();
            std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling.txt")).unwrap();
            e.app.reload_panel(0, None);
            settle(&mut e.app);
            cursor_to(&mut e.app, "dangling.txt");
            press(&mut e.app, KeyCode::Enter);
            status_starts(&mut e, "오류: dangling.txt: ");
        }

        #[cfg(unix)]
        #[test]
        fn fifo_is_rejected_without_a_blocking_read() {
            let mut e = env("");
            let root = e.app.panels[0].cwd.clone();
            let fifo = root.join("pipe");
            let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            e.app.reload_panel(0, None);
            settle(&mut e.app);
            cursor_to(&mut e.app, "pipe");
            press(&mut e.app, KeyCode::Enter);
            tick_until(&mut e.app, "special", |a| {
                a.status.as_deref() == Some("특수 파일은 열 수 없음")
            });
            assert!(e.app.viewer.is_none() && log(&e).is_empty());
        }

        #[test]
        fn probe_spam_keeps_one_in_flight_and_at_most_two_runs() {
            let mut e = env("");
            let (open, calls) = (
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicUsize::new(0)),
            );
            e.app.text_probe = gated_probe(&open, &calls);
            for _ in 0..10 {
                press(&mut e.app, KeyCode::Enter);
            }
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(calls.load(AO::SeqCst), 1, "one probe in flight");
            open.store(true, AO::SeqCst);
            tick_until(&mut e.app, "viewer", |a| a.viewer.is_some());
            quiet_ticks(&mut e.app);
            assert!(calls.load(AO::SeqCst) <= 2, "{}", calls.load(AO::SeqCst));
        }

        #[cfg(unix)]
        #[test]
        fn background_spawn_does_not_block_the_ui_thread() {
            let mut e = env(&format!("{CAT}background = true\n"));
            *e.fake.spawn_delay.lock().unwrap() = Some(Duration::from_millis(600));
            let t0 = Instant::now();
            press(&mut e.app, KeyCode::Enter);
            press(&mut e.app, KeyCode::Down);
            assert!(t0.elapsed() < Duration::from_millis(300), "UI blocked");
            tick_until(&mut e.app, "status", |a| {
                a.status.as_deref() == Some("실행: f.txt")
            });
        }

        // ---- open-with menu ----

        const TWO: &str = "[[assoc]]\next = [\"txt\"]\ncmd = \"cat {}\"\nname = \"Cat\"\n[[assoc]]\nglob = \"f.*\"\ncmd = \"less {}\"\n";

        #[test]
        fn ctrl_o_and_shift_enter_open_the_open_with_menu() {
            for (code, mods) in [
                (KeyCode::Char('o'), KeyModifiers::CONTROL),
                (KeyCode::Enter, KeyModifiers::SHIFT),
            ] {
                let mut e = env(TWO);
                press_mods(&mut e.app, code, mods);
                let open = e.app.menu.as_ref().expect("menu open");
                assert_eq!(
                    open.menu.items,
                    ["less {}", "Cat", "시스템 기본 프로그램", "뷰어(F3)"]
                );
            }
        }

        #[test]
        fn open_with_without_rules_lists_system_and_viewer_only() {
            let mut e = env("");
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            assert_eq!(
                e.app.menu.as_ref().unwrap().menu.items,
                ["시스템 기본 프로그램", "뷰어(F3)"]
            );
        }

        #[cfg(unix)]
        #[test]
        fn open_with_runs_the_chosen_rule() {
            let mut e = env(TWO);
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            press(&mut e.app, KeyCode::Down); // "Cat"
            press(&mut e.app, KeyCode::Enter);
            assert!(e.app.menu.is_none());
            run_pending_logged(&mut e);
            let got = log(&e);
            assert!(got[1].contains("|-c|cat \"$1\"|"), "{got:?}");
        }

        #[cfg(unix)]
        #[test]
        fn open_with_system_and_viewer_choices() {
            let mut e = env(TWO);
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            press(&mut e.app, KeyCode::Down);
            press(&mut e.app, KeyCode::Down); // system
            press(&mut e.app, KeyCode::Enter);
            let l = Arc::clone(&e.log);
            tick_until(&mut e.app, "spawn", move |_| !l.lock().unwrap().is_empty());
            let got = log(&e);
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].starts_with(&format!("spawn {}|", crate::assoc::system_opener())));

            let mut e = env(TWO);
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            for _ in 0..3 {
                press(&mut e.app, KeyCode::Down); // viewer
            }
            press(&mut e.app, KeyCode::Enter);
            assert!(e.app.viewer.is_some() && log(&e).is_empty());
        }

        #[test]
        fn open_with_esc_cancels_and_dirs_are_refused() {
            let mut e = env(TWO);
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            press(&mut e.app, KeyCode::Esc);
            assert!(e.app.menu.is_none() && !e.app.has_pending_run());
            cursor_to(&mut e.app, "adir");
            press_mods(&mut e.app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            assert!(e.app.menu.is_none());
            assert_eq!(e.app.status.as_deref(), Some("파일을 선택하세요"));
        }

        #[test]
        fn f9_menu_has_the_open_with_item() {
            let mut e = env(TWO);
            e.app.open_main_menu();
            pick(&mut e.app, "연결 프로그램 선택");
            assert_eq!(e.app.menu.as_ref().unwrap().menu.items.len(), 4);
        }
    }
    // ---- HDIR-31/32: command line, cwd on exit ----

    mod cmdline_run {
        use super::*;
        use crate::launcher::Launcher;
        use std::sync::Mutex;

        struct ShellFake {
            log: Mutex<Vec<String>>,
            result: Mutex<Result<i32, String>>,
        }

        impl Launcher for ShellFake {
            fn run(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                unreachable!("command line uses run_shell")
            }
            fn spawn_detached(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn wait_key(&self) {
                self.log.lock().unwrap().push("wait_key".into());
            }
            fn run_shell(&self, argv: &[OsString], cwd: &Path) -> Result<i32, String> {
                let a: Vec<_> = argv.iter().map(|a| a.to_string_lossy()).collect();
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("shell {} @{}", a.join("|"), cwd.display()));
                self.result.lock().unwrap().clone()
            }
            fn wait_key_after(&self, code: i32) {
                self.log.lock().unwrap().push(format!("wait_key {code}"));
            }
        }

        fn env() -> (TempDir, App, Arc<ShellFake>) {
            let (t, mut app) = fixture();
            fs::create_dir(t.path().join("한글 dir")).unwrap();
            fs::create_dir(t.path().join("adir/sub")).unwrap();
            let fake = Arc::new(ShellFake {
                log: Mutex::new(Vec::new()),
                result: Mutex::new(Ok(0)),
            });
            app.launcher = fake.clone();
            app.shell_path = OsString::from("/bin/zsh");
            app.home_dir = Some(app.panels[0].cwd.join("bdir"));
            (t, app, fake)
        }

        fn submit(app: &mut App, cmd: &str) {
            press(app, KeyCode::Char(':'));
            type_str(app, cmd);
            press(app, KeyCode::Enter);
        }

        fn log(f: &ShellFake) -> Vec<String> {
            f.log.lock().unwrap().clone()
        }

        #[cfg(unix)]
        fn run_logged(app: &mut App, f: &Arc<ShellFake>) {
            let f = Arc::clone(f);
            app.run_shell(move |run| {
                f.log.lock().unwrap().push("suspend".into());
                let r = run();
                f.log.lock().unwrap().push("resume".into());
                Ok(r)
            });
        }

        #[test]
        fn colon_opens_and_esc_closes_without_side_effects() {
            let (_t, mut app, _f) = env();
            press(&mut app, KeyCode::Char(':'));
            assert!(app.cmdline.is_some());
            type_str(&mut app, "q");
            assert!(app.running, "keys are text while the line is open");
            assert_eq!(app.cmdline.as_ref().unwrap().value, "q");
            press(&mut app, KeyCode::Esc);
            assert!(app.cmdline.is_none() && !app.has_pending_shell());
            assert!(app.running);
        }

        #[test]
        fn cd_relative_is_internal_and_async() {
            let (_t, mut app, f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd adir");
            assert!(app.cmdline.is_none() && !app.has_pending_shell());
            assert!(app.panels[0].loading.is_some());
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.join("adir"));
            assert!(log(&f).is_empty());
        }

        #[test]
        fn cd_dash_goes_back_and_dotdot_works() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd adir/sub");
            settle(&mut app);
            submit(&mut app, "cd ../..");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.as_path());
            submit(&mut app, "cd -");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.join("adir/sub"));
            submit(&mut app, "cd -");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.as_path(), "toggles");
        }

        #[test]
        fn cd_dash_without_previous_reports() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd -");
            assert_eq!(app.status.as_deref(), Some("이전 디렉터리 없음"));
            assert_eq!(app.panels[0].cwd, root.as_path());
        }

        #[test]
        fn cd_home_and_tilde_use_home_dir() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.join("bdir"));
            submit(&mut app, "cd ~/../adir");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.join("adir"));
            app.home_dir = None;
            submit(&mut app, "cd");
            assert_eq!(app.status.as_deref(), Some("홈 디렉터리를 알 수 없음"));
        }

        #[test]
        fn cd_quoted_korean_name_with_space() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd \"한글 dir\"");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.join("한글 dir"));
        }

        #[test]
        fn cd_to_missing_dir_shows_error_and_stays() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "cd nope");
            settle(&mut app);
            assert_eq!(app.panels[0].cwd, root.as_path());
            assert!(app.status.is_some(), "error surfaced");
        }

        #[test]
        fn cd_acts_on_the_active_panel_only() {
            let (_t, mut app, _f) = env();
            let root = app.panels[0].cwd.clone();
            press(&mut app, KeyCode::Tab);
            submit(&mut app, "cd adir");
            settle(&mut app);
            assert_eq!(app.panels[1].cwd, root.join("adir"));
            assert_eq!(app.panels[0].cwd, root.as_path());
        }

        #[cfg(unix)]
        #[test]
        fn other_commands_suspend_run_in_the_shell_and_reload() {
            let (_t, mut app, f) = env();
            let root = app.panels[0].cwd.clone();
            submit(&mut app, "ls -la && echo 한글");
            assert!(app.has_pending_shell());
            assert!(log(&f).is_empty(), "nothing runs before the suspend");
            run_logged(&mut app, &f);
            let cwd = root.display();
            assert_eq!(
                log(&f),
                [
                    "suspend".to_string(),
                    format!("shell /bin/zsh|-c|ls -la && echo 한글 @{cwd}"),
                    "wait_key 0".to_string(),
                    "resume".to_string(),
                ]
            );
            assert!(!app.has_pending_shell());
            assert!(app.panels.iter().all(|p| p.loading.is_some()), "reloaded");
            assert_eq!(
                app.status.as_deref(),
                Some("종료 코드 0: ls -la && echo 한글")
            );
        }

        #[cfg(unix)]
        #[test]
        fn exit_code_and_launch_failure_show_in_status() {
            let (_t, mut app, f) = env();
            *f.result.lock().unwrap() = Ok(3);
            submit(&mut app, "false");
            run_logged(&mut app, &f);
            assert_eq!(app.status.as_deref(), Some("종료 코드 3: false"));
            *f.result.lock().unwrap() = Err("no such shell".into());
            submit(&mut app, "x");
            run_logged(&mut app, &f);
            assert_eq!(app.status.as_deref(), Some("오류: x: no such shell"));
            assert!(!log(&f).iter().any(|l| l == "wait_key -1"));
        }

        #[cfg(unix)]
        #[test]
        fn command_runs_in_the_active_panels_cwd() {
            let (_t, mut app, f) = env();
            let root = app.panels[0].cwd.clone();
            press(&mut app, KeyCode::Tab);
            submit(&mut app, "cd adir");
            settle(&mut app);
            submit(&mut app, "pwd");
            run_logged(&mut app, &f);
            let want = format!("shell /bin/zsh|-c|pwd @{}", root.join("adir").display());
            assert!(log(&f).contains(&want), "{:?}", log(&f));
        }

        #[cfg(windows)]
        #[test]
        fn shell_commands_are_not_supported_on_windows_yet() {
            let (_t, mut app, _f) = env();
            submit(&mut app, "dir");
            assert!(!app.has_pending_shell());
            assert_eq!(app.status.as_deref(), Some("셸 명령: Windows 지원 예정"));
        }

        #[test]
        fn history_up_down_dedup() {
            let (_t, mut app, _f) = env();
            submit(&mut app, "cd adir");
            settle(&mut app);
            submit(&mut app, "cd adir");
            settle(&mut app);
            submit(&mut app, "cd -");
            settle(&mut app);
            press(&mut app, KeyCode::Char(':'));
            press(&mut app, KeyCode::Up);
            assert_eq!(app.cmdline.as_ref().unwrap().value, "cd -");
            press(&mut app, KeyCode::Up);
            assert_eq!(app.cmdline.as_ref().unwrap().value, "cd adir");
            press(&mut app, KeyCode::Up);
            assert_eq!(app.cmdline.as_ref().unwrap().value, "cd adir");
            press(&mut app, KeyCode::Down);
            assert_eq!(app.cmdline.as_ref().unwrap().value, "cd -");
        }

        #[test]
        fn blank_line_just_closes() {
            let (_t, mut app, _f) = env();
            submit(&mut app, "   ");
            assert!(app.cmdline.is_none() && !app.has_pending_shell());
            assert!(app.panels[0].loading.is_none());
        }

        #[test]
        fn dev_mode_shows_the_line_in_the_command_row() {
            let (_t, mut app, _f) = env();
            app.set_mode(Mode::Dev);
            press(&mut app, KeyCode::Char(':'));
            type_str(&mut app, "ls 한");
            let rows = draw_app(&mut app, 100, 30);
            assert!(rows[27].starts_with("> ls 한"), "{:?}", rows[27]);
            let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let pos = t.get_cursor_position().unwrap();
            assert_eq!((pos.x, pos.y), (2 + 3 + 2, 27), "wide char counts 2 cells");
        }

        #[test]
        fn classic_mode_shows_the_line_on_the_status_row() {
            let (_t, mut app, _f) = env();
            press(&mut app, KeyCode::Char(':'));
            type_str(&mut app, "ls");
            let rows = draw_app(&mut app, 100, 30);
            assert!(rows[28].trim_start().starts_with(": ls"), "{:?}", rows[28]);
        }

        #[test]
        fn quit_keys_decide_whether_the_cwd_is_written() {
            for (code, mods, cd) in [
                (KeyCode::F(10), KeyModifiers::NONE, true),
                (KeyCode::Char('q'), KeyModifiers::NONE, true),
                (KeyCode::F(10), KeyModifiers::SHIFT, false),
                (KeyCode::Char('x'), KeyModifiers::CONTROL, false),
            ] {
                let (_t, mut app, _f) = env();
                press_mods(&mut app, code, mods);
                assert!(!app.running, "{code:?}");
                assert_eq!(app.cd_on_exit, cd, "{code:?} {mods:?}");
            }
        }

        #[test]
        fn quit_from_the_trash_view_keeps_the_same_rules() {
            let (_t, mut app, _f) = env();
            app.trash_views[0] = Some(TrashView::new(app.panels[0].cwd.clone()));
            press_mods(&mut app, KeyCode::F(10), KeyModifiers::SHIFT);
            assert!(!app.running && !app.cd_on_exit);
        }
    }

    // ---- HDIR-35/37/43/44: ssh panels ----

    #[cfg(unix)]
    mod ssh_panels {
        use super::*;
        use crate::config::Mode;
        use crate::remote::{check_argv, master_argv};
        use crate::servers::ServerConfig;
        use crate::session::{CmdOutput, DISCONNECTED_MSG};
        use std::collections::HashMap;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        const UNSUPPORTED: &str = "원격: 다음 단계에서 지원";

        struct FakeSsh {
            master_up: AtomicBool,
            auth_ok: AtomicBool,
            down: AtomicBool,
            interactive: Mutex<Vec<Vec<String>>>,
            checks: Mutex<usize>,
            /// Held by a test to make every remote listing hang.
            list_gate: Mutex<()>,
            tree: HashMap<&'static str, Vec<(&'static str, &'static str)>>,
            /// Every ssh/scp argv that is not a master check or `ssh -V`.
            calls: Mutex<Vec<Vec<String>>>,
            /// Tests script answers here; `None` falls through to the defaults.
            hook: Mutex<Option<Hook>>,
            /// What `printf "$HOME"` answers.
            home: Mutex<String>,
        }

        type Hook = Box<dyn Fn(&[String]) -> Option<CmdOutput> + Send + Sync>;

        impl FakeSsh {
            fn new(master_up: bool) -> Arc<Self> {
                let mut tree = HashMap::new();
                tree.insert(
                    "/srv/data",
                    vec![("d", "sub"), ("f", "a.txt"), ("f", "한글 b.txt")],
                );
                tree.insert("/srv/data/sub", vec![("f", "deep.txt")]);
                tree.insert("/home/choi", vec![("d", "work")]);
                Arc::new(Self {
                    master_up: AtomicBool::new(master_up),
                    auth_ok: AtomicBool::new(true),
                    down: AtomicBool::new(false),
                    interactive: Mutex::new(Vec::new()),
                    checks: Mutex::new(0),
                    list_gate: Mutex::new(()),
                    tree,
                    calls: Mutex::new(Vec::new()),
                    hook: Mutex::new(None),
                    home: Mutex::new("/home/choi".into()),
                })
            }
        }

        fn out(code: i32, stdout: &str) -> CmdOutput {
            CmdOutput {
                code: Some(code),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            }
        }

        fn strs(argv: &[OsString]) -> Vec<String> {
            argv.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        }

        impl Cmd for FakeSsh {
            fn run(&self, argv: &[OsString]) -> CmdOutput {
                let a = strs(argv);
                if a == ["ssh", "-V"] {
                    return CmdOutput {
                        code: Some(0),
                        stdout: Vec::new(),
                        stderr: b"OpenSSH_9.6p1\n".to_vec(),
                    };
                }
                if a.iter().any(|x| x == "-O") {
                    *self.checks.lock().unwrap() += 1;
                    return out(
                        if self.master_up.load(Ordering::SeqCst) {
                            0
                        } else {
                            255
                        },
                        "",
                    );
                }
                if self.down.load(Ordering::SeqCst) {
                    return out(255, "");
                }
                let i = a.iter().position(|s| s == "--").unwrap();
                let cmd = a.get(i + 2).map_or("", String::as_str);
                if cmd != "uname -s" {
                    self.calls.lock().unwrap().push(a.clone());
                }
                if let Some(h) = self.hook.lock().unwrap().as_ref()
                    && let Some(o) = h(&a)
                {
                    return o;
                }
                if a[0] == "scp" {
                    return out(0, "");
                }
                if cmd == "uname -s" {
                    return out(0, "Linux\n");
                }
                if cmd.contains("\"$HOME\"") && !cmd.starts_with("cd -- ") {
                    return out(0, &format!("{}\0", self.home.lock().unwrap()));
                }
                if !cmd.starts_with("cd -- ") {
                    return out(0, ""); // mkdir, mv, rm, ... succeed by default
                }
                let dir = cmd
                    .strip_prefix("cd -- ")
                    .map(|r| r.split(" && ").next().unwrap().trim_matches('\''))
                    .unwrap();
                if cmd.contains("$PWD") {
                    let dir = if dir == "." || dir == "\"$HOME\"" {
                        "/home/choi"
                    } else {
                        dir
                    };
                    return out(0, &format!("{dir}\0"));
                }
                drop(self.list_gate.lock().unwrap()); // tests hold it to hang listings
                match self.tree.get(dir) {
                    Some(items) => {
                        let recs: String = items
                            .iter()
                            .map(|(k, n)| format!("{k}\t5\t1700000000.0\t644\t\t{n}\0"))
                            .collect();
                        out(0, &recs)
                    }
                    None => CmdOutput {
                        code: Some(1),
                        stdout: Vec::new(),
                        stderr: b"cd: no such directory\n".to_vec(),
                    },
                }
            }
            fn run_interactive(&self, argv: &[OsString]) -> CmdOutput {
                self.interactive.lock().unwrap().push(strs(argv));
                if self.auth_ok.load(Ordering::SeqCst) {
                    self.master_up.store(true, Ordering::SeqCst);
                    out(0, "")
                } else {
                    out(255, "")
                }
            }
        }

        /// Remote shell commands other than listings and path lookups.
        fn cmds(f: &FakeSsh) -> Vec<String> {
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|a| crate::testutil::remote_cmd(a).map(str::to_string))
                .filter(|c| !c.starts_with("cd -- "))
                .collect()
        }

        /// Every remote shell command, listings included.
        fn all_cmds(f: &FakeSsh) -> Vec<String> {
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|a| crate::testutil::remote_cmd(a).map(str::to_string))
                .collect()
        }

        fn script(f: &FakeSsh, h: impl Fn(&[String]) -> Option<CmdOutput> + Send + Sync + 'static) {
            *f.hook.lock().unwrap() = Some(Box::new(h));
        }

        fn env_with(master_up: bool) -> (TempDir, App, Arc<FakeSsh>) {
            let (t, mut app) = fixture();
            let fake = FakeSsh::new(master_up);
            app.cmd = fake.clone();
            app.control_path = t.path().join("cm/cm-%C");
            app.ssh_hosts = vec![SshHost {
                alias: "web".into(),
                hostname: Some("web.example.com".into()),
                user: Some("choi".into()),
                port: None,
                source: PathBuf::new(),
            }];
            app.settings.config.servers = vec![ServerConfig {
                name: Some("prod".into()),
                host: Some("prod-web".into()),
                path: Some("/srv/data".into()),
                theme: Some("mdir-red".into()),
                ..ServerConfig::default()
            }];
            (t, app, fake)
        }

        fn env() -> (TempDir, App, Arc<FakeSsh>) {
            env_with(true)
        }

        fn alt(app: &mut App, f: u8) {
            press_mods(app, KeyCode::F(f), KeyModifiers::ALT);
        }

        /// Tick until connecting and loading are over, or auth is needed.
        fn wait_remote(app: &mut App) {
            let start = Instant::now();
            while (app.is_connecting() || app.panels.iter().any(|p| p.loading.is_some()))
                && !app.has_pending_master()
            {
                assert!(start.elapsed() < Duration::from_secs(5), "remote timeout");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            app.tick();
        }

        fn run_master_ok(app: &mut App) {
            app.run_master(|f| Ok(f()));
        }

        /// Connect the right panel to "prod" (picker item 1) and make it active.
        fn connect_prod(app: &mut App) {
            alt(app, 2);
            press(app, KeyCode::Down);
            press(app, KeyCode::Enter);
            wait_remote(app);
        }

        fn names(app: &App, i: usize) -> Vec<String> {
            app.panels[i]
                .entries
                .iter()
                .map(|e| e.name.clone())
                .collect()
        }

        fn submit(app: &mut App, cmd: &str) {
            press(app, KeyCode::Char(':'));
            type_str(app, cmd);
            press(app, KeyCode::Enter);
        }

        #[test]
        fn alt_f1_opens_picker_with_local_and_servers() {
            let (_t, mut app, _f) = env();
            alt(&mut app, 1);
            let items = app.menu.as_ref().expect("picker").menu.items.clone();
            assert_eq!(items[0], "로컬");
            assert!(
                items[1].contains("prod") && items[1].contains("prod-web"),
                "{items:?}"
            );
            assert!(
                items[2].contains("web") && items[2].contains("web.example.com"),
                "{items:?}"
            );
            assert_eq!(items.len(), 3);
            press(&mut app, KeyCode::Esc);
            assert!(app.menu.is_none());
        }

        #[test]
        fn picking_a_server_connects_lists_and_activates_that_panel() {
            let (_t, mut app, f) = env();
            alt(&mut app, 2);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            assert!(app.is_connecting());
            wait_remote(&mut app);
            assert!(app.panels[1].location.is_remote());
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data"));
            assert_eq!(names(&app, 1), vec!["..", "sub", "a.txt", "한글 b.txt"]);
            assert_eq!(app.active, 1);
            assert!(!app.panels[0].location.is_remote());
            assert!(f.interactive.lock().unwrap().is_empty(), "master was alive");
            assert!(*f.checks.lock().unwrap() >= 1);
        }

        #[test]
        fn server_without_start_path_opens_the_remote_home() {
            let (_t, mut app, _f) = env();
            alt(&mut app, 1);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down); // web
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            assert_eq!(app.panels[0].cwd, PathBuf::from("/home/choi"));
            assert_eq!(names(&app, 0), vec!["..", "work"]);
        }

        #[test]
        fn dead_master_runs_ssh_interactively_then_connects() {
            let (_t, mut app, f) = env_with(false);
            alt(&mut app, 2);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            assert!(app.has_pending_master());
            assert!(!app.panels[1].location.is_remote());
            run_master_ok(&mut app);
            assert!(!app.has_pending_master());
            wait_remote(&mut app);
            let t = SshTarget {
                host: "prod-web".into(),
                port: None,
                control_path: app.control_path.clone(),
            };
            assert_eq!(f.interactive.lock().unwrap()[0], strs(&master_argv(&t)));
            assert_eq!(strs(&check_argv(&t))[..3], ["ssh", "-O", "check"]);
            assert!(app.panels[1].location.is_remote());
            assert_eq!(names(&app, 1)[1], "sub");
        }

        #[test]
        fn two_panels_needing_auth_both_get_the_terminal_in_order() {
            let (_t, mut app, f) = env_with(false);
            alt(&mut app, 2); // right panel -> prod
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            alt(&mut app, 1); // left panel -> web
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            let start = Instant::now();
            while app.pending_master.len() < 2 {
                assert!(start.elapsed() < Duration::from_secs(5), "auth requests");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            run_master_ok(&mut app);
            assert!(
                app.has_pending_master(),
                "the second request is still queued"
            );
            run_master_ok(&mut app);
            assert!(!app.has_pending_master());
            wait_remote(&mut app);
            let hosts: Vec<String> = f
                .interactive
                .lock()
                .unwrap()
                .iter()
                .map(|a| a[a.len() - 1].clone())
                .collect();
            assert_eq!(hosts, vec!["prod-web", "web"], "in request order");
            assert!(app.panels[0].location.is_remote());
            assert!(app.panels[1].location.is_remote());
            assert!(!app.is_connecting());
        }

        #[test]
        fn leaving_a_hung_remote_listing_does_not_block_the_local_one() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            let gate = f.list_gate.lock().unwrap();
            app.reload_panel(1, None); // hangs on the gate
            std::thread::sleep(Duration::from_millis(50));
            submit(&mut app, "local");
            let start = Instant::now();
            while app.panels[1].loading.is_some() {
                assert!(
                    start.elapsed() < Duration::from_secs(3),
                    "local listing stuck behind the hung remote one"
                );
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(!app.panels[1].location.is_remote());
            assert!(!app.panels[1].entries.is_empty());
            drop(gate);
            std::thread::sleep(Duration::from_millis(100));
            app.tick();
            assert!(!app.panels[1].location.is_remote(), "old result is dropped");
            assert!(app.panels[1].error.is_none());
        }

        #[test]
        fn failed_auth_reports_and_keeps_the_panel_local() {
            let (_t, mut app, f) = env_with(false);
            f.auth_ok.store(false, Ordering::SeqCst);
            let before = app.panels[1].cwd.clone();
            alt(&mut app, 2);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            run_master_ok(&mut app);
            wait_remote(&mut app);
            assert!(!app.is_connecting());
            assert!(
                app.status.as_deref().unwrap_or("").contains("연결 실패"),
                "{:?}",
                app.status
            );
            assert!(!app.panels[1].location.is_remote());
            assert_eq!(app.panels[1].cwd, before);
        }

        #[test]
        fn picking_local_returns_to_the_last_local_dir() {
            let (_t, mut app, _f) = env();
            let local = app.panels[1].cwd.clone();
            connect_prod(&mut app);
            alt(&mut app, 2);
            press(&mut app, KeyCode::Home); // 로컬
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            assert!(!app.panels[1].location.is_remote());
            assert_eq!(app.panels[1].cwd, local);
            assert!(names(&app, 1).contains(&"f.txt".to_string()));
        }

        #[test]
        fn ssh_command_connects_the_active_panel_and_local_returns() {
            let (_t, mut app, _f) = env();
            let local = app.panels[0].cwd.clone();
            submit(&mut app, "ssh web:/srv/data/sub");
            wait_remote(&mut app);
            assert!(app.panels[0].location.is_remote());
            assert_eq!(app.panels[0].cwd, PathBuf::from("/srv/data/sub"));
            assert_eq!(names(&app, 0), vec!["..", "deep.txt"]);
            assert!(app.cmdline.is_none() && !app.has_pending_shell());
            submit(&mut app, "local");
            wait_remote(&mut app);
            assert!(!app.panels[0].location.is_remote());
            assert_eq!(app.panels[0].cwd, local);
        }

        #[test]
        fn ssh_command_uses_server_name_and_start_path() {
            let (_t, mut app, _f) = env();
            submit(&mut app, "ssh prod");
            wait_remote(&mut app);
            assert_eq!(app.panels[0].cwd, PathBuf::from("/srv/data"));
            assert_eq!(app.panels[0].location.session().unwrap().name, "prod");
        }

        #[test]
        fn ssh_command_without_host_shows_usage_and_local_on_local_is_a_noop() {
            let (_t, mut app, _f) = env();
            submit(&mut app, "ssh");
            assert!(
                app.status.as_deref().unwrap_or("").contains("ssh"),
                "{:?}",
                app.status
            );
            assert!(!app.is_connecting());
            let cwd = app.panels[0].cwd.clone();
            submit(&mut app, "local");
            assert_eq!(app.panels[0].cwd, cwd);
            assert!(!app.has_pending_shell());
        }

        #[test]
        fn enter_and_backspace_navigate_remote_paths() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down); // sub
            press(&mut app, KeyCode::Enter);
            assert!(app.panels[1].loading.is_some(), "loads asynchronously");
            wait_remote(&mut app);
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data/sub"));
            assert_eq!(names(&app, 1), vec!["..", "deep.txt"]);
            press(&mut app, KeyCode::Backspace);
            wait_remote(&mut app);
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data"));
            assert_eq!(name_at(&app, 1), "sub", "cursor returns to the dir we left");
        }

        #[test]
        fn disconnect_is_marked_and_enter_reconnects() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            f.down.store(true, Ordering::SeqCst);
            f.master_up.store(false, Ordering::SeqCst);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Enter); // into sub: ssh dies
            wait_remote(&mut app);
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data"), "stays put");
            assert_eq!(app.panels[1].error.as_deref(), Some(DISCONNECTED_MSG));
            assert_eq!(app.status.as_deref(), Some(DISCONNECTED_MSG));
            assert!(!app.panels[1].location.session().unwrap().is_alive());
            // network is back, master gone: Enter asks for auth again
            f.down.store(false, Ordering::SeqCst);
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            assert!(app.has_pending_master());
            run_master_ok(&mut app);
            wait_remote(&mut app);
            assert!(app.panels[1].location.session().unwrap().is_alive());
            assert!(app.panels[1].error.is_none(), "{:?}", app.panels[1].error);
            assert_eq!(names(&app, 1)[1], "sub");
        }

        #[test]
        fn reconnect_with_a_live_master_needs_no_terminal() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            app.panels[1].location.session().unwrap().set_alive(false);
            press(&mut app, KeyCode::Enter);
            wait_remote(&mut app);
            assert!(!app.has_pending_master());
            assert!(app.panels[1].location.session().unwrap().is_alive());
            assert!(f.interactive.lock().unwrap().is_empty());
        }

        // ---- HDIR-38: remote mkdir / rename / delete ----

        fn rename_to(app: &mut App, new: &str) {
            press(app, KeyCode::F(2));
            for _ in 0..40 {
                press(app, KeyCode::Backspace);
            }
            type_str(app, new);
            press(app, KeyCode::Enter);
        }

        #[test]
        fn remote_f7_makes_the_dir_over_ssh_and_is_undoable() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "new dir");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.status.as_deref(), Some("폴더 생성 중…"));
            assert!(app.task.is_some(), "runs on a worker");
            wait_idle(&mut app);
            wait_remote(&mut app);
            assert_eq!(cmds(&f), vec!["mkdir -- '/srv/data/new dir'".to_string()]);
            assert_eq!(app.status.as_deref(), Some("폴더 생성: new dir"));
            assert_eq!(app.undo.len(), 1);
            ctrl_z(&mut app);
            wait_idle(&mut app);
            wait_remote(&mut app);
            assert_eq!(cmds(&f)[1], "rmdir -- '/srv/data/new dir'");
            assert_eq!(
                app.status.as_deref(),
                Some("되돌림: 폴더 생성 취소 new dir")
            );
        }

        #[test]
        fn remote_f7_with_weird_names_stays_quoted_and_rejects_slashes() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "한글 'q' -x");
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert_eq!(
                cmds(&f),
                vec!["mkdir -- '/srv/data/한글 '\\''q'\\'' -x'".to_string()]
            );
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "a/b");
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert_eq!(cmds(&f).len(), 1, "invalid name never reaches ssh");
            assert!(app.status.as_deref().unwrap().starts_with("오류"));
        }

        #[test]
        fn remote_f7_failure_is_reported_without_undo_entry() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("mkdir"))
                    .map(|_| CmdOutput {
                        code: Some(1),
                        stdout: Vec::new(),
                        stderr: b"mkdir: cannot create directory 'x': File exists\n".to_vec(),
                    })
            });
            connect_prod(&mut app);
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert!(app.status.as_deref().unwrap().contains("File exists"));
            assert_eq!(app.undo.len(), 0);
        }

        #[test]
        fn remote_f2_renames_without_clobbering_and_undo_moves_back() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down); // a.txt
            rename_to(&mut app, "b c.txt");
            assert_eq!(app.status.as_deref(), Some("이름 변경 중…"));
            wait_idle(&mut app);
            wait_remote(&mut app);
            let fwd = crate::remote::rename_cmd(b"/srv/data/a.txt", b"/srv/data/b c.txt");
            assert_eq!(cmds(&f), vec![String::from_utf8(fwd).unwrap()]);
            assert_eq!(app.status.as_deref(), Some("이름 변경: b c.txt"));
            assert_eq!(app.undo.len(), 1);
            ctrl_z(&mut app);
            wait_idle(&mut app);
            let back = crate::remote::rename_cmd(b"/srv/data/b c.txt", b"/srv/data/a.txt");
            assert_eq!(cmds(&f)[1], String::from_utf8(back).unwrap());
            assert_eq!(
                app.status.as_deref(),
                Some("되돌림: 이름 변경 b c.txt → a.txt")
            );
        }

        #[test]
        fn remote_f2_onto_an_existing_name_is_an_error() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -e"))
                    .map(|_| out(crate::remote::RENAME_EXISTS, ""))
            });
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            rename_to(&mut app, "sub");
            wait_idle(&mut app);
            assert_eq!(app.status.as_deref(), Some("오류: 이미 존재함: sub"));
            assert_eq!(app.undo.len(), 0);
        }

        #[test]
        fn remote_f8_asks_with_a_danger_dialog_naming_the_server() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Char(' ')); // sub
            press(&mut app, KeyCode::Char(' ')); // a.txt
            press(&mut app, KeyCode::F(8));
            let Some((
                Dialog::Confirm {
                    title,
                    message,
                    danger,
                },
                _,
            )) = app.dialog.clone()
            else {
                panic!("expected a confirm dialog: {:?}", app.dialog);
            };
            assert!(danger, "red permanent-delete style");
            assert_eq!(title, "영구 삭제 (원격: prod)");
            assert_eq!(message, vec!["a.txt".to_string(), "sub".to_string()]);
            press(&mut app, KeyCode::Esc);
            assert!(app.dialog.is_none());
            assert!(cmds(&f).is_empty(), "cancel deletes nothing");
        }

        #[test]
        fn remote_f8_confirm_runs_rm_rf_and_is_not_undoable() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Char(' ')); // a.txt
            press(&mut app, KeyCode::Char(' ')); // 한글 b.txt
            press(&mut app, KeyCode::F(8));
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.status.as_deref(), Some("영구 삭제 중…"));
            wait_idle(&mut app);
            wait_remote(&mut app);
            let rm: Vec<String> = cmds(&f)
                .into_iter()
                .filter(|c| c.contains("rm -rf --"))
                .collect();
            let want = crate::remote::delete_cmd(
                b"/srv/data",
                &["a.txt".as_bytes(), "한글 b.txt".as_bytes()],
            );
            assert_eq!(rm, vec![String::from_utf8(want).unwrap()]);
            assert_eq!(
                app.status.as_deref(),
                Some("영구 삭제: 2개 (원격 삭제는 되돌릴 수 없음)")
            );
            assert_eq!(app.undo.len(), 0);
            assert!(app.panels[1].selected.is_empty());
            ctrl_z(&mut app);
            assert_eq!(app.status.as_deref(), Some("되돌릴 작업 없음"));
        }

        #[test]
        fn remote_f8_refuses_the_remote_home_directory() {
            let (_t, mut app, f) = env();
            // the delete script itself refuses (it resolves $HOME on the host)
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.contains("rm -rf --"))
                    .map(|_| out(crate::remote::DELETE_PROTECTED, ""))
            });
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down); // sub == $HOME
            press(&mut app, KeyCode::F(8));
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert!(app.status.as_deref().unwrap().contains("삭제 거부"));
            assert!(app.status.as_deref().unwrap().contains("홈 디렉터리"));
        }

        #[test]
        fn remote_ops_are_refused_while_another_task_runs() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            let (_tx, rx) = std::sync::mpsc::channel();
            app.task = Some(BgTask {
                rx,
                kind: BgKind::Undo,
            });
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "nope");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
            assert!(cmds(&f).is_empty());
        }

        #[test]
        fn remote_ops_on_a_dropped_connection_say_so() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            f.down.store(true, Ordering::SeqCst);
            press(&mut app, KeyCode::F(7));
            type_str(&mut app, "x");
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert_eq!(
                app.status.as_deref(),
                Some(&format!("오류: {DISCONNECTED_MSG}")[..])
            );
            assert!(!app.panels[1].location.session().unwrap().is_alive());
        }

        // ---- HDIR-39: remote preview and F3 ----

        /// Serve `size\nbody` for preview commands.
        fn serve_files(f: &FakeSsh, body: &'static str) {
            script(f, move |a| {
                let c = crate::testutil::remote_cmd(a)?;
                if c.starts_with("if [ -d ") {
                    return Some(out(0, &format!("{}\n{body}", body.len())));
                }
                None
            });
        }

        fn wait_preview(app: &mut App) {
            let start = Instant::now();
            while app.preview_shown().is_none() {
                assert!(start.elapsed() < Duration::from_secs(5), "preview timeout");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[test]
        fn dev_preview_of_a_remote_file_reads_the_head_over_ssh() {
            let (_t, mut app, f) = env();
            serve_files(&f, "hello 한글\nline2\n");
            app.preview_debounce = Duration::ZERO;
            app.set_mode(Mode::Dev);
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down); // a.txt
            wait_preview(&mut app);
            assert_eq!(
                app.preview_shown(),
                Some(&Preview::Text {
                    lines: vec!["hello 한글".into(), "line2".into()],
                    truncated: false
                })
            );
            let want = crate::remote::preview_cmd(
                &crate::remote::RemoteOs::Linux,
                b"/srv/data/a.txt",
                65536 + 3,
            );
            assert!(cmds(&f).contains(&String::from_utf8(want).unwrap()));
        }

        #[test]
        fn dev_preview_of_a_remote_dir_shows_an_entry_count() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(crate::remote::PREVIEW_DIR, ""))
            });
            app.preview_debounce = Duration::ZERO;
            app.set_mode(Mode::Dev);
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down); // sub (1 entry)
            wait_preview(&mut app);
            assert_eq!(
                app.preview_shown(),
                Some(&Preview::Info("원격 디렉터리 · 항목 1개".into()))
            );
        }

        #[test]
        fn remote_preview_is_not_confused_with_a_local_file_of_the_same_path() {
            let (_t, mut app, f) = env();
            serve_files(&f, "remote text\n");
            app.preview_debounce = Duration::ZERO;
            app.set_mode(Mode::Dev);
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            wait_preview(&mut app);
            assert!(
                matches!(app.preview_shown(), Some(Preview::Text { lines, .. }) if lines[0] == "remote text")
            );
        }

        fn wait_viewer(app: &mut App) {
            let start = Instant::now();
            while matches!(
                app.viewer.as_ref().map(|v| &v.content),
                Some(viewer::Content::Loading)
            ) {
                assert!(start.elapsed() < Duration::from_secs(5), "viewer timeout");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[test]
        fn remote_f3_views_the_head_with_the_4_mib_cap() {
            let (_t, mut app, f) = env();
            serve_files(&f, "first\nsecond\n");
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down); // a.txt
            press(&mut app, KeyCode::F(3));
            assert!(app.viewer.is_some());
            wait_viewer(&mut app);
            let v = app.viewer.as_ref().unwrap();
            assert_eq!(v.path, PathBuf::from("/srv/data/a.txt"));
            assert!(
                matches!(&v.content, viewer::Content::Text { lines, .. } if lines == &["first", "second"])
            );
            let want = crate::remote::preview_cmd(
                &crate::remote::RemoteOs::Linux,
                b"/srv/data/a.txt",
                4 * 1024 * 1024 + 3,
            );
            assert!(cmds(&f).contains(&String::from_utf8(want).unwrap()));
            press(&mut app, KeyCode::Esc);
            assert!(app.viewer.is_none());
        }

        #[test]
        fn remote_f3_on_a_special_file_says_so_and_on_a_dir_does_nothing() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(crate::remote::PREVIEW_SPECIAL, ""))
            });
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down); // sub (dir)
            press(&mut app, KeyCode::F(3));
            assert!(app.viewer.is_none());
            press(&mut app, KeyCode::Down); // a.txt, reported special
            press(&mut app, KeyCode::F(3));
            wait_viewer(&mut app);
            assert!(matches!(&app.viewer.as_ref().unwrap().content,
                viewer::Content::Message(m) if m.contains("특수 파일")));
        }

        #[test]
        fn remote_f3_error_is_shown_in_the_viewer() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| CmdOutput {
                        code: Some(1),
                        stdout: Vec::new(),
                        stderr: b"wc: x: Permission denied\n".to_vec(),
                    })
            });
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::F(3));
            wait_viewer(&mut app);
            assert!(matches!(&app.viewer.as_ref().unwrap().content,
                viewer::Content::Message(m) if m.contains("Permission denied")));
        }

        // ---- HDIR-40: remote F4 edit and Enter / open-with ----

        type Uploads = Arc<Mutex<Vec<(String, String)>>>;

        /// Fake scp: downloads create `<dest dir>/<basename>` containing
        /// `body`; uploads answer `up` and are recorded with the local file's
        /// content at that moment.
        fn scp_hook(f: &FakeSsh, body: &'static str, up: CmdOutput) -> Uploads {
            let uploads: Uploads = Arc::new(Mutex::new(Vec::new()));
            let rec = Arc::clone(&uploads);
            script(f, move |a| {
                if a[0] != "scp" {
                    // the upload's resolve and mktemp scripts (see remote_ops)
                    let c = crate::testutil::remote_cmd(a)?;
                    let unq = |q: &str| q[1..q.len() - 1].replace("'\\''", "'");
                    if let Some(r) = c.strip_prefix("p=") {
                        let q = r.split_once("; n=0\n").unwrap().0;
                        return Some(out(0, &format!("{}\0", unq(q))));
                    }
                    if let Some(r) = c.strip_prefix("t=$(mktemp ") {
                        let q = r.split_once(") || exit 1").unwrap().0;
                        return Some(out(0, &format!("{}\0", unq(q).replace("XXXXXX", "Ab12Cd"))));
                    }
                    return None;
                }
                let (src, dst) = (&a[a.len() - 2], &a[a.len() - 1]);
                if dst.starts_with("prod-web:") {
                    let content = fs::read_to_string(src).unwrap_or_default();
                    rec.lock().unwrap().push((dst.clone(), content));
                    return Some(up.clone());
                }
                let name = src.rsplit('/').next().unwrap();
                fs::write(Path::new(dst).join(name), body).unwrap();
                Some(out(0, ""))
            });
            uploads
        }

        /// Editor fake: remembers (path, content before, dir mode), then
        /// writes `after` (nothing when `None`).
        fn edit_with(app: &mut App, after: Option<&'static str>) -> Arc<Mutex<Vec<String>>> {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let s2 = Arc::clone(&seen);
            app.editor_runner = Arc::new(move |argv: &[OsString]| {
                use std::os::unix::fs::PermissionsExt;
                let p = PathBuf::from(argv.last().unwrap());
                let mode = p.parent().unwrap().metadata().unwrap().permissions().mode() & 0o777;
                s2.lock().unwrap().push(format!(
                    "{}|{}|{:o}",
                    p.display(),
                    fs::read_to_string(&p).unwrap(),
                    mode
                ));
                if let Some(a) = after {
                    fs::write(&p, a).unwrap();
                }
                Ok(())
            });
            seen
        }

        fn wait_uploads(app: &mut App) {
            let start = Instant::now();
            while app.has_uploads() {
                assert!(start.elapsed() < Duration::from_secs(5), "upload timeout");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Connected, cursor on a.txt, editor "ed".
        fn edit_env() -> (TempDir, App, Arc<FakeSsh>) {
            let (t, mut app, f) = env();
            app.editor_vars = (None, Some("ed".into()));
            connect_prod(&mut app);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Down);
            (t, app, f)
        }

        fn f4_and_download(app: &mut App) {
            press(app, KeyCode::F(4));
            assert_eq!(app.status.as_deref(), Some("내려받는 중…"));
            wait_idle(app);
        }

        #[test]
        fn remote_f4_downloads_edits_and_uploads_a_changed_file() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(&f, "orig", out(0, ""));
            let seen = edit_with(&mut app, Some("edited"));
            f4_and_download(&mut app);
            assert!(app.has_pending_edit());
            app.run_edit(|f| Ok(f()));
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            let (path, rest) = seen[0].split_once('|').unwrap();
            assert!(
                path.ends_with("/a.txt") && !path.starts_with("/srv"),
                "{path}"
            );
            assert_eq!(rest, "orig|700", "private dir, downloaded content");
            assert_eq!(
                app.status.as_deref(),
                Some("변경 확인 중…: a.txt"),
                "hashing runs on a worker"
            );
            wait_uploads(&mut app);
            let ups = ups.lock().unwrap().clone();
            assert_eq!(ups.len(), 1);
            assert_eq!(ups[0].0, "prod-web:/srv/data/.hdir-edit.Ab12Cd");
            assert_eq!(ups[0].1, "edited");
            let tmp = ups[0].0.strip_prefix("prod-web:").unwrap();
            let want = crate::remote::replace_cmd(
                &crate::remote::RemoteOs::Linux,
                tmp.as_bytes(),
                b"/srv/data/a.txt",
                6,
            );
            assert!(cmds(&f).contains(&String::from_utf8(want).unwrap()));
            assert_eq!(app.status.as_deref(), Some("편집 완료: a.txt (업로드됨)"));
            assert!(!Path::new(path).exists(), "temp copy removed after upload");
        }

        #[test]
        fn remote_f4_without_changes_uploads_nothing_and_cleans_up() {
            for after in [None, Some("orig")] {
                let (_t, mut app, f) = edit_env();
                let ups = scp_hook(&f, "orig", out(0, ""));
                let seen = edit_with(&mut app, after);
                f4_and_download(&mut app);
                app.run_edit(|f| Ok(f()));
                wait_uploads(&mut app);
                assert!(ups.lock().unwrap().is_empty());
                assert_eq!(app.status.as_deref(), Some("편집 완료: a.txt (변경 없음)"));
                let path = seen.lock().unwrap()[0]
                    .split('|')
                    .next()
                    .unwrap()
                    .to_string();
                assert!(!Path::new(&path).exists());
            }
        }

        #[test]
        fn remote_f4_upload_failure_keeps_the_temp_copy_and_names_it() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(
                &f,
                "orig",
                CmdOutput {
                    code: Some(1),
                    stdout: Vec::new(),
                    stderr: b"scp: write failed: No space left on device\n".to_vec(),
                },
            );
            let seen = edit_with(&mut app, Some("precious edits"));
            f4_and_download(&mut app);
            app.run_edit(|f| Ok(f()));
            wait_uploads(&mut app);
            let path = seen.lock().unwrap()[0]
                .split('|')
                .next()
                .unwrap()
                .to_string();
            let status = app.status.clone().unwrap();
            assert!(status.starts_with("업로드 실패: a.txt"), "{status}");
            assert!(status.contains("No space left"), "{status}");
            assert!(status.contains(&path), "tells where the copy is: {status}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "precious edits");
            let tmp = ups.lock().unwrap()[0]
                .0
                .strip_prefix("prod-web:")
                .unwrap()
                .to_string();
            let rm = String::from_utf8(crate::remote::rm_file_cmd(tmp.as_bytes())).unwrap();
            assert!(cmds(&f).contains(&rm), "remote temp file cleaned up");
            fs::remove_dir_all(Path::new(&path).parent().unwrap()).unwrap();
        }

        #[test]
        fn remote_f4_download_failure_is_an_error_and_starts_no_editor() {
            let (_t, mut app, f) = edit_env();
            script(&f, |a| {
                (a[0] == "scp").then(|| CmdOutput {
                    code: Some(1),
                    stdout: Vec::new(),
                    stderr: b"scp: /srv/data/a.txt: Permission denied\n".to_vec(),
                })
            });
            press(&mut app, KeyCode::F(4));
            wait_idle(&mut app);
            assert!(!app.has_pending_edit());
            assert!(app.status.as_deref().unwrap().contains("Permission denied"));
        }

        #[test]
        fn remote_f4_uploads_even_when_the_editor_exits_with_an_error() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(&f, "orig", out(0, ""));
            app.editor_runner = Arc::new(|argv: &[OsString]| {
                fs::write(argv.last().unwrap(), "saved before crash").unwrap();
                Err("편집기가 오류로 종료됨 (종료 코드 1)".into())
            });
            f4_and_download(&mut app);
            app.run_edit(|f| Ok(f()));
            wait_uploads(&mut app);
            assert_eq!(ups.lock().unwrap()[0].1, "saved before crash");
        }

        #[test]
        fn remote_f4_from_the_remote_viewer_edits_that_file() {
            let (_t, mut app, f) = edit_env();
            serve_files(&f, "view me");
            let ups = scp_hook(&f, "orig", out(0, ""));
            let hook_inner = f.hook.lock().unwrap().take().unwrap();
            // previews answer first, scp falls through to the scp hook
            let preview_answer = |a: &[String]| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(0, "7\nview me"))
            };
            script(&f, move |a| preview_answer(a).or_else(|| hook_inner(a)));
            edit_with(&mut app, Some("edited in viewer"));
            press(&mut app, KeyCode::F(3));
            wait_viewer(&mut app);
            press(&mut app, KeyCode::F(4));
            assert_eq!(app.status.as_deref(), Some("내려받는 중…"));
            wait_idle(&mut app);
            assert!(app.has_pending_edit());
            app.run_edit(|f| Ok(f()));
            wait_uploads(&mut app);
            assert_eq!(ups.lock().unwrap()[0].1, "edited in viewer");
            assert!(app.viewer.is_some(), "viewer stays open and reloads");
        }

        #[test]
        fn remote_f4_uploads_when_the_content_changed_but_size_and_mtime_were_restored() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(&f, "orig", out(0, ""));
            app.editor_runner = Arc::new(|argv: &[OsString]| {
                let p = PathBuf::from(argv.last().unwrap());
                let old = fs::metadata(&p).unwrap().modified().unwrap();
                fs::write(&p, "ORIG").unwrap(); // same length
                fs::File::options()
                    .write(true)
                    .open(&p)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
                Ok(())
            });
            f4_and_download(&mut app);
            app.run_edit(|f| Ok(f()));
            wait_uploads(&mut app);
            assert_eq!(ups.lock().unwrap().len(), 1, "content decides");
            assert_eq!(ups.lock().unwrap()[0].1, "ORIG");
        }

        #[test]
        fn remote_f4_unchanged_after_an_editor_error_keeps_the_editor_message() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(&f, "orig", out(0, ""));
            app.editor_runner =
                Arc::new(|_: &[OsString]| Err("편집기가 오류로 종료됨 (종료 코드 1)".into()));
            f4_and_download(&mut app);
            app.run_edit(|f| Ok(f()));
            wait_uploads(&mut app);
            assert!(ups.lock().unwrap().is_empty());
            assert_eq!(
                app.status.as_deref(),
                Some("편집기가 오류로 종료됨 (종료 코드 1)")
            );
        }

        /// F4 edit of a.txt with `up` as the uploader (scp only serves the
        /// download), left running after `run_edit`.
        fn editing_with_uploader(
            up: impl Fn(&RemoteSession, &Path, &Path) -> Result<(), String> + Send + Sync + 'static,
        ) -> (TempDir, App, Arc<FakeSsh>) {
            let (t, mut app, f) = edit_env();
            scp_hook(&f, "orig", out(0, ""));
            app.uploader = Arc::new(up);
            edit_with(&mut app, Some("slow edit"));
            f4_and_download(&mut app);
            app.run_edit(|f| Ok(f()));
            assert!(app.has_uploads());
            (t, app, f)
        }

        #[test]
        fn quitting_waits_for_a_running_upload() {
            let (_t, mut app, _f) = editing_with_uploader(|_, _, _| {
                std::thread::sleep(Duration::from_millis(60));
                Ok(())
            });
            let mut err = Vec::new();
            assert!(app.quit_uploads(Duration::from_secs(5), &mut err));
            assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
            assert!(!app.has_uploads());
        }

        #[test]
        fn quit_timeout_keeps_the_edited_copy_and_reports_where_it_goes() {
            let (_t, mut app, _f) = editing_with_uploader(|_, local, _| {
                std::thread::sleep(Duration::from_millis(400));
                assert_eq!(fs::read_to_string(local).unwrap(), "slow edit");
                Ok(())
            });
            let mut err = Vec::new();
            assert!(
                !app.quit_uploads(Duration::from_millis(30), &mut err),
                "an unfinished upload fails the exit code"
            );
            let text = String::from_utf8(err).unwrap();
            assert!(
                text.starts_with("hdir: 업로드가 끝나지 않음: a.txt"),
                "{text}"
            );
            assert!(
                text.contains("원격 대상: prod-web:/srv/data/a.txt"),
                "{text}"
            );
            let local = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("편집한 복사본: "))
                .expect("names the local copy");
            // the worker finishing later must not delete what was reported
            std::thread::sleep(Duration::from_millis(600));
            assert_eq!(fs::read_to_string(local).unwrap(), "slow edit");
            fs::remove_dir_all(Path::new(local).parent().unwrap()).unwrap();
        }

        #[test]
        fn quit_reports_failed_uploads_with_their_kept_copies() {
            let (_t, mut app, _f) =
                editing_with_uploader(|_, _, _| Err("scp: No space left on device".into()));
            let start = Instant::now();
            while app.has_uploads() {
                assert!(start.elapsed() < Duration::from_secs(5));
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(app.status.as_deref().unwrap().starts_with("업로드 실패"));
            let mut err = Vec::new();
            assert!(!app.quit_uploads(Duration::from_secs(1), &mut err));
            let text = String::from_utf8(err).unwrap();
            assert!(text.contains("No space left"), "{text}");
            assert!(
                text.contains("원격 대상: prod-web:/srv/data/a.txt"),
                "{text}"
            );
            let local = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("편집한 복사본: "))
                .unwrap();
            assert_eq!(fs::read_to_string(local).unwrap(), "slow edit");
            // reported once
            let mut again = Vec::new();
            assert!(app.quit_uploads(Duration::from_millis(10), &mut again));
            assert!(again.is_empty());
            fs::remove_dir_all(Path::new(local).parent().unwrap()).unwrap();
        }

        #[test]
        fn remote_f4_is_refused_while_busy_and_on_directories() {
            let (_t, mut app, f) = edit_env();
            let (_tx, rx) = std::sync::mpsc::channel();
            app.task = Some(BgTask {
                rx,
                kind: BgKind::Undo,
            });
            press(&mut app, KeyCode::F(4));
            assert_eq!(app.status.as_deref(), Some("작업 진행 중"));
            app.task = None;
            app.status = None;
            press(&mut app, KeyCode::Up); // sub (dir)
            press(&mut app, KeyCode::F(4));
            assert!(app.task.is_none() && app.status.is_none());
            assert!(f.calls.lock().unwrap().iter().all(|a| a[0] != "scp"));
        }

        /// Launcher fake logging `kind argv @cwd`.
        struct LaunchLog(Arc<Mutex<Vec<String>>>);

        impl crate::launcher::Launcher for LaunchLog {
            fn run(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("run {} @{}", join(argv), cwd.display()));
                Ok(())
            }
            fn spawn_detached(&self, argv: &[OsString], cwd: &Path) -> Result<(), String> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("spawn {} @{}", join(argv), cwd.display()));
                Ok(())
            }
            fn wait_key(&self) {}
        }

        fn join(argv: &[OsString]) -> String {
            argv.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join("|")
        }

        fn launch_log(app: &mut App) -> Arc<Mutex<Vec<String>>> {
            let log = Arc::new(Mutex::new(Vec::new()));
            app.launcher = Arc::new(LaunchLog(Arc::clone(&log)));
            log
        }

        fn wait_log(log: &Arc<Mutex<Vec<String>>>, app: &mut App) -> Vec<String> {
            let start = Instant::now();
            while log.lock().unwrap().is_empty() {
                assert!(start.elapsed() < Duration::from_secs(5), "launch timeout");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            log.lock().unwrap().clone()
        }

        fn txt_rule(wait: bool) -> crate::assoc::AssocTable {
            crate::assoc::AssocTable::from_rules(vec![AssocRule {
                ext: Some(vec!["txt".into()]),
                glob: None,
                cmd: "cat {}".into(),
                wait,
                background: false,
                name: None,
            }])
            .0
        }

        #[test]
        fn remote_enter_runs_a_rule_on_a_downloaded_copy_and_removes_it_after() {
            let (_t, mut app, f) = edit_env();
            let ups = scp_hook(&f, "orig", out(0, ""));
            let log = launch_log(&mut app);
            app.settings.assoc = txt_rule(true);
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert!(app.has_pending_run(), "queued for the main loop");
            app.run_pending(|f| Ok(f()));
            let got = log.lock().unwrap().clone();
            assert_eq!(got.len(), 1, "{got:?}");
            let line = &got[0];
            assert!(
                line.starts_with("run ") && line.contains("|-c|cat \"$1\"|hdir|"),
                "{line}"
            );
            let local = line.split('|').nth(4).unwrap().to_string();
            assert!(
                local.ends_with("/a.txt") && !local.starts_with("/srv"),
                "{local}"
            );
            assert!(line.contains(&format!(
                "@{}",
                Path::new(&local).parent().unwrap().display()
            )));
            assert!(
                !Path::new(&local).exists(),
                "temp copy removed after the run"
            );
            assert!(
                ups.lock().unwrap().is_empty(),
                "read-only use: never uploaded"
            );
            assert_eq!(app.status.as_deref(), Some("실행 완료: a.txt"));
        }

        #[test]
        fn remote_enter_on_text_without_a_rule_opens_the_remote_viewer() {
            let (_t, mut app, f) = edit_env();
            serve_files(&f, "just text\n");
            press(&mut app, KeyCode::Enter);
            let start = Instant::now();
            while app.viewer.is_none() {
                assert!(start.elapsed() < Duration::from_secs(5), "no viewer");
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
            wait_viewer(&mut app);
            assert!(matches!(&app.viewer.as_ref().unwrap().content,
                viewer::Content::Text { lines, .. } if lines == &["just text"]));
            assert!(
                f.calls.lock().unwrap().iter().all(|a| a[0] != "scp"),
                "no download"
            );
        }

        #[test]
        fn remote_enter_on_a_binary_file_opens_a_downloaded_copy_with_the_system_opener() {
            let (_t, mut app, f) = edit_env();
            let preview_bin = |a: &[String]| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(0, "4\n\0\0\0\0"))
            };
            let _ups = scp_hook(&f, "\0\0\0\0", out(0, ""));
            let scp_inner = f.hook.lock().unwrap().take().unwrap();
            script(&f, move |a| preview_bin(a).or_else(|| scp_inner(a)));
            let log = launch_log(&mut app);
            press(&mut app, KeyCode::Enter);
            let got = wait_log(&log, &mut app);
            assert!(
                got[0].starts_with(&format!("spawn {}|", assoc::system_opener())),
                "{got:?}"
            );
            let local = got[0]
                .split('|')
                .nth(1)
                .unwrap()
                .split(" @")
                .next()
                .unwrap();
            assert!(
                local.ends_with("/a.txt") && !local.starts_with("/srv"),
                "{local}"
            );
            assert!(
                Path::new(local).exists(),
                "a background opener may still be reading it"
            );
        }

        #[test]
        fn remote_enter_on_a_special_file_reports_it() {
            let (_t, mut app, f) = edit_env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(crate::remote::PREVIEW_SPECIAL, ""))
            });
            press(&mut app, KeyCode::Enter);
            let start = Instant::now();
            while app.status.as_deref() != Some("특수 파일은 열 수 없음") {
                assert!(start.elapsed() < Duration::from_secs(5), "{:?}", app.status);
                app.tick();
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[test]
        fn remote_open_with_menu_offers_rules_system_and_viewer() {
            let (_t, mut app, f) = edit_env();
            serve_files(&f, "menu text\n");
            let hook_inner = {
                let _ups = scp_hook(&f, "orig", out(0, ""));
                f.hook.lock().unwrap().take().unwrap()
            };
            let preview_answer = |a: &[String]| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.starts_with("if [ -d "))
                    .map(|_| out(0, "10\nmenu text\n"))
            };
            script(&f, move |a| preview_answer(a).or_else(|| hook_inner(a)));
            let log = launch_log(&mut app);
            app.settings.assoc = txt_rule(false);
            press_mods(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
            let items = app.menu.as_ref().expect("menu").menu.items.clone();
            assert_eq!(items.last().unwrap(), "뷰어(F3)");
            assert!(items.len() >= 3, "{items:?}");
            // "cat {}" rule, foreground: downloaded copy, run suspended
            press(&mut app, KeyCode::Enter);
            wait_idle(&mut app);
            assert!(app.has_pending_run());
            app.run_pending(|f| Ok(f()));
            assert!(log.lock().unwrap()[0].contains("cat \"$1\""));
            // viewer entry reads over ssh
            press_mods(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
            for _ in 1..items.len() {
                press(&mut app, KeyCode::Down);
            }
            press(&mut app, KeyCode::Enter);
            wait_viewer(&mut app);
            assert!(app.viewer.is_some());
        }

        // ---- HDIR-41: remote command line ----

        /// Launcher fake for command line runs: logs argv/cwd and the prompt.
        struct ShellLog {
            log: Arc<Mutex<Vec<String>>>,
            code: i32,
        }

        impl crate::launcher::Launcher for ShellLog {
            fn run(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                unreachable!("command lines use run_shell")
            }
            fn spawn_detached(&self, _: &[OsString], _: &Path) -> Result<(), String> {
                Ok(())
            }
            fn wait_key(&self) {}
            fn run_shell(&self, argv: &[OsString], cwd: &Path) -> Result<i32, String> {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("shell {} @{}", join(argv), cwd.display()));
                Ok(self.code)
            }
            fn wait_key_after(&self, code: i32) {
                self.log.lock().unwrap().push(format!("prompt {code}"));
            }
        }

        fn shell_log(app: &mut App, code: i32) -> Arc<Mutex<Vec<String>>> {
            let log = Arc::new(Mutex::new(Vec::new()));
            app.launcher = Arc::new(ShellLog {
                log: Arc::clone(&log),
                code,
            });
            log
        }

        fn listings(f: &FakeSsh) -> usize {
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|a| crate::testutil::remote_cmd(a))
                .filter(|c| c.contains("find "))
                .count()
        }

        #[test]
        fn remote_command_runs_over_ssh_t_in_the_remote_cwd_then_prompts_and_reloads() {
            let (_t, mut app, f) = env();
            connect_prod(&mut app);
            let log = shell_log(&mut app, 7);
            let before = listings(&f);
            submit(&mut app, "ls -la \"it's\"");
            assert!(app.has_pending_shell(), "queued for the main loop");
            assert!(log.lock().unwrap().is_empty());
            app.run_shell(|f| Ok(f()));
            let t = app.panels[1].location.session().unwrap().target.clone();
            let want = crate::remote::remote_shell_argv(&t, b"/srv/data", "ls -la \"it's\"");
            let got = log.lock().unwrap().clone();
            assert_eq!(
                got,
                vec![format!("shell {} @.", join(&want)), "prompt 7".to_string()]
            );
            assert!(got[0].contains("|-t|--|prod-web|cd -- '/srv/data' && {\nls -la \"it's\"\n}"));
            assert_eq!(app.status.as_deref(), Some("종료 코드 7: ls -la \"it's\""));
            wait_remote(&mut app);
            assert!(listings(&f) > before, "remote panels are reloaded");
            assert!(!app.has_pending_shell());
        }

        #[test]
        fn remote_cd_is_resolved_on_the_server_without_leaving_the_tui() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.contains("&& cd -- "))
                    .map(|c| {
                        if c.contains("'nope'") {
                            CmdOutput {
                                code: Some(1),
                                stdout: Vec::new(),
                                stderr: b"sh: cd: nope: No such file or directory\n".to_vec(),
                            }
                        } else {
                            out(0, "/srv/data/sub\0")
                        }
                    })
            });
            connect_prod(&mut app);
            submit(&mut app, "cd sub");
            assert!(!app.has_pending_shell(), "cd never suspends");
            wait_idle(&mut app);
            wait_remote(&mut app);
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data/sub"));
            assert_eq!(names(&app, 1), vec!["..", "deep.txt"]);
            assert!(all_cmds(&f).contains(
                &"cd -- '/srv/data' && cd -- 'sub' && printf '%s\\0' \"$PWD\"".to_string()
            ));
            // cd - returns, cd to a missing dir reports the server's reason
            submit(&mut app, "cd -");
            wait_idle(&mut app);
            wait_remote(&mut app);
            assert_eq!(
                app.panels[1].cwd,
                PathBuf::from("/srv/data/sub"),
                "scripted pwd"
            );
            submit(&mut app, "cd nope");
            wait_idle(&mut app);
            assert!(app.status.as_deref().unwrap().contains("No such file"));
            assert_eq!(app.panels[1].cwd, PathBuf::from("/srv/data/sub"));
        }

        #[test]
        fn remote_cd_variants_build_the_right_targets() {
            let (_t, mut app, f) = env();
            script(&f, |a| {
                crate::testutil::remote_cmd(a)
                    .filter(|c| c.contains("&& cd -- "))
                    .map(|_| out(0, "/srv/data\0"))
            });
            connect_prod(&mut app);
            for line in ["cd", "cd ~", "cd ~/ab", "cd '/x y'", "cd .."] {
                submit(&mut app, line);
                wait_idle(&mut app);
                wait_remote(&mut app);
            }
            let cds: Vec<String> = all_cmds(&f)
                .into_iter()
                .filter(|c| c.contains("&& cd -- "))
                .collect();
            let tail = " && printf '%s\\0' \"$PWD\"";
            let pre = "cd -- '/srv/data' && cd -- ";
            assert_eq!(
                cds,
                vec![
                    format!("{pre}\"$HOME\"{tail}"),
                    format!("{pre}\"$HOME\"{tail}"),
                    format!("{pre}\"$HOME\"'/ab'{tail}"),
                    format!("{pre}'/x y'{tail}"),
                    format!("{pre}'..'{tail}"),
                ]
            );
        }

        #[test]
        fn complex_cd_lines_go_to_the_remote_shell() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            submit(&mut app, "cd sub && ls");
            assert!(app.has_pending_shell());
        }

        #[test]
        fn remote_command_on_a_dead_connection_does_not_start() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            app.panels[1].location.session().unwrap().set_alive(false);
            submit(&mut app, "ls");
            assert_eq!(app.status.as_deref(), Some(DISCONNECTED_MSG));
            assert!(!app.has_pending_shell());
            submit(&mut app, "cd sub");
            assert_eq!(app.status.as_deref(), Some(DISCONNECTED_MSG));
        }

        #[test]
        fn local_panels_still_run_commands_locally_with_the_shell() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Tab);
            submit(&mut app, "echo hi");
            assert!(app.has_pending_shell());
            let log = shell_log(&mut app, 0);
            app.run_shell(|f| Ok(f()));
            let got = log.lock().unwrap().clone();
            assert!(got[0].contains("|-c|echo hi @"), "{got:?}");
            assert!(!got[0].starts_with("shell ssh"), "{got:?}");
        }

        #[test]
        fn remote_transfers_stay_unsupported_until_m8() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            for f in [5u8, 6] {
                app.status = None;
                press(&mut app, KeyCode::F(f));
                assert_eq!(app.status.as_deref(), Some(UNSUPPORTED), "F{f}");
                assert!(app.dialog.is_none() && app.viewer.is_none(), "F{f}");
            }
        }

        #[test]
        fn copy_from_a_local_panel_to_a_remote_one_is_blocked_too() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            press(&mut app, KeyCode::Tab); // back to the local left panel
            assert_eq!(app.active, 0);
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::F(5));
            assert_eq!(app.status.as_deref(), Some(UNSUPPORTED));
            assert!(app.dialog.is_none());
        }

        #[test]
        fn each_panel_gets_its_server_theme() {
            let (_t, mut app, _f) = env();
            connect_prod(&mut app);
            assert_eq!(app.panel_theme(1).name, "mdir-red");
            assert_eq!(app.panel_theme(0).name, app.theme.name);
            let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let buf = t.backend().buffer().clone();
            let red = crate::theme::builtin("mdir-red").unwrap();
            assert_eq!(buf[(75, 8)].bg, red.bg, "remote panel");
            assert_ne!(buf[(10, 8)].bg, red.bg, "local panel");
            assert_eq!(
                buf[(0, 0)].bg,
                red.bg,
                "header follows the active remote panel"
            );
        }

        #[test]
        fn remote_without_theme_rule_uses_the_global_theme() {
            let (_t, mut app, _f) = env();
            app.settings.config.servers.clear();
            submit(&mut app, "ssh web:/srv/data");
            wait_remote(&mut app);
            assert_eq!(app.panel_theme(0).name, app.theme.name);
        }

        #[test]
        fn exit_cwd_ignores_remote_panels() {
            let (_t, mut app, _f) = env();
            let local = app.panels[1].cwd.clone();
            app.set_mode(Mode::Dev);
            connect_prod(&mut app);
            assert_eq!(app.exit_cwd(), Some(local.as_path()));
            assert!(app.preview_target().is_none());
        }
    }

    #[test]
    fn configure_reads_ssh_config_hosts_for_the_picker() {
        let (t, mut app) = fixture();
        let cfg = t.path().join("ssh_config");
        fs::write(&cfg, "Host web\n  HostName web.example.com\n  User choi\n").unwrap();
        app.ssh_config_path = Some(cfg);
        app.configure(None, Some("truecolor"));
        assert_eq!(app.ssh_hosts.len(), 1);
        assert_eq!(app.ssh_hosts[0].alias, "web");
    }

    #[cfg(windows)]
    #[test]
    fn ssh_connect_on_windows_is_a_stub() {
        let (_t, mut app) = fixture();
        app.settings.config.servers = vec![crate::servers::ServerConfig {
            name: Some("prod".into()),
            host: Some("prod-web".into()),
            ..Default::default()
        }];
        press_mods(&mut app, KeyCode::F(2), KeyModifiers::ALT);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("Windows 지원 예정")
        );
        assert!(!app.panels[1].location.is_remote());
    }
}
