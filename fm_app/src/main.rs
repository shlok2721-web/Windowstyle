//! Native host for the reusable egui Explorer UI.
//! Filesystem traversal and thumbnails stay on bounded worker threads. Listings, hot-folder
//! search and learned behaviour live in RAM; only a tiny, hash-skipped state file and the
//! cost-selected thumbnails are ever written, and only rarely.

mod single_instance;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::{ColorImage, TextureHandle, TextureOptions};
use egui_explorer::{
    Command, CompressFormat, Entry, EntryKind, Event, Explorer, ExplorerState, Icon, NavNode,
    RibbonMode, dialogs,
};
use fm_behav::Behavior;
use fm_persist::{AtomicFile, FlushPolicy, Flusher, Outcome, WriteCounter};
use fm_settings::{
    GroupKey as SavedGroupKey, RibbonMode as SavedRibbonMode, Settings, SortKey as SavedSortKey,
    SortOrder as SavedSortOrder, ViewMode as SavedViewMode,
};
use fm_thumbs::{Thumbnail, ThumbnailAudit, ThumbnailGenerationReport, ThumbnailStore};

const THUMBNAIL_WORKERS: usize = 2;
const THUMBNAIL_QUEUE_CAPACITY: usize = 256;
const TEXTURE_HANDLE_LIMIT: usize = 256;
const TEXTURE_CACHE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const SEARCH_LIMIT: usize = 500;
/// Time budget per frame for loading hot folders into RAM after startup.
const PRIME_BUDGET: Duration = Duration::from_millis(3);
/// Events are folded into the aggregates at most this often.
const FOLD_INTERVAL: Duration = Duration::from_secs(5);
/// The idle app wakes this often so the periodic flush check can run.
const IDLE_WAKE: Duration = Duration::from_secs(30 * 60);
const QUICK_ACCESS_ID: &str = "quick-access://";
/// Extra windows (besides the first) one process will open.
const MAX_CHILD_WINDOWS: usize = 12;
/// Status-bar messages disappear after this long.
const STATUS_TTL: Duration = Duration::from_secs(6);

/// One reversible step. The stacks hold the action that *undoes* an operation;
/// running it yields the inverse action, which goes onto the opposite stack.
#[derive(Clone, Debug)]
enum UndoAction {
    /// Rename/move `from` to `to`.
    Rename { from: PathBuf, to: PathBuf },
    /// Move this path to the Trash.
    Trash(PathBuf),
    /// Restore the item whose original location was this path from the Trash.
    Restore(PathBuf),
}

fn run_undo_action(action: UndoAction) -> Result<UndoAction, String> {
    match action {
        UndoAction::Rename { from, to } => {
            fs::rename(&from, &to).map_err(|error| error.to_string())?;
            Ok(UndoAction::Rename { from: to, to: from })
        }
        UndoAction::Trash(path) => {
            move_to_trash(&path)?;
            Ok(UndoAction::Restore(path))
        }
        UndoAction::Restore(path) => {
            let items = fm_trash::list().map_err(|error| error.to_string())?;
            let item = items
                .iter()
                .rev()
                .find(|item| item.original_path() == path.as_path())
                .ok_or_else(|| format!("'{}' is no longer in the Trash.", path.display()))?;
            item.restore().map_err(|error| error.to_string())?;
            Ok(UndoAction::Trash(path))
        }
    }
}

/// In-app "Move to / Copy to" destination dialog state.
struct DestDialog {
    cut: bool,
    paths: Vec<PathBuf>,
    text: String,
}

/// A copy/move that would overwrite existing names and awaits the user's decision.
struct PendingTransfer {
    paths: Vec<PathBuf>,
    dest: PathBuf,
    cut: bool,
    paste: bool,
    conflicts: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConflictChoice {
    KeepBoth,
    Replace,
}

/// A thumbnail request: `hot` tells the persist policy whether the file's folder is hot.
struct ThumbJob {
    path: PathBuf,
    hot: bool,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Process-wide RAM-only listing tiers (hot / warm / cold). Shared through a global because
/// the UI crate takes `fn(&Path) -> Vec<PathBuf>` pointers for tree expansion, which cannot
/// capture state. All access happens on the UI thread, so contention is nil.
static TIERS: LazyLock<Mutex<fm_tiers::Tiers>> = LazyLock::new(|| {
    Mutex::new(fm_tiers::Tiers::new(fm_tiers::DenyList::defaults(
        dirs::home_dir().as_deref(),
    )))
});

fn tiers() -> MutexGuard<'static, fm_tiers::Tiers> {
    TIERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum WorkerMessage {
    Thumbnail {
        path: PathBuf,
        result: Result<(Thumbnail, fm_thumbs::EnsureReport), String>,
    },
    AuditFinished {
        root: PathBuf,
        report: ThumbnailAudit,
    },
    GenerationFinished {
        root: PathBuf,
        report: ThumbnailGenerationReport,
    },
    /// A file operation (copy, move, delete, archive, search ...) finished on its thread.
    Job(JobOutcome),
}

/// Result of a file operation that ran off the UI thread. Applied by `poll_workers`.
#[derive(Default)]
struct JobOutcome {
    /// Folder the operation was started from.
    origin: PathBuf,
    status: Option<String>,
    undo: Vec<UndoAction>,
    pending_extract: Option<PendingExtract>,
    /// `(root, entries, message)` of a finished folder search.
    search: Option<(PathBuf, Vec<Entry>, String)>,
    /// Search generation this job belongs to (0 = not a search).
    search_generation: u64,
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let frameless = args.iter().any(|a| a == "--frameless");
    // `file-manager [FOLDER]`: a folder to open. Flags all start with `--`.
    let cli_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    // One process for every window: a second launch hands its folder to the running instance
    // (which opens a new window) and exits. `--new-instance` opts out.
    let listener = if args.iter().any(|a| a == "--new-instance") {
        None
    } else {
        match single_instance::acquire(cli_path.as_deref()) {
            single_instance::Acquire::Forwarded => return Ok(()),
            single_instance::Acquire::Primary(listener) => Some(listener),
            single_instance::Acquire::Unavailable => None,
        }
    };
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 720.0])
        .with_min_inner_size([640.0, 420.0])
        .with_title("File Manager");
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "File Manager",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                frameless,
                &cc.egui_ctx,
                cli_path,
                listener,
            )))
        }),
    )
}

/// An extra window living in this same process. Everything heavy (listing tiers, thumbnail
/// workers and textures, behaviour state, clipboard, undo history, inotify) is shared with the
/// first window; only the per-window view state lives here. While the window is drawn its
/// fields are swapped into the `App` (see `App::swap_window`), so all existing code that works
/// on `self.state` just operates on this window.
struct ChildWindow {
    id: egui::ViewportId,
    title: String,
    /// Set once the window asked to be closed; it is dropped at the end of the frame.
    close: bool,
    /// Raise the window on its first frame.
    fresh: bool,
    explorer: Explorer,
    state: ExplorerState,
    prev_location: Option<PathBuf>,
    watcher: Option<fm_notify::DirWatcher>,
    dest_dialog: Option<DestDialog>,
    pending_transfer: Option<PendingTransfer>,
    pending_delete: Option<Vec<PathBuf>>,
    pending_extract: Option<PendingExtract>,
    status: String,
    status_seen: String,
    status_seen_at: Instant,
}

struct CachedTexture {
    handle: TextureHandle,
    source_size: u64,
    source_modified: Option<std::time::SystemTime>,
    memory_bytes: usize,
}

struct App {
    explorer: Explorer,
    state: ExplorerState,
    /// (paths, is_cut)
    clipboard: Option<(Vec<PathBuf>, bool)>,
    /// System clipboard handle used to publish copied files as `text/uri-list`. It must
    /// stay alive for as long as the data should be pasteable: on X11 (and XWayland) this
    /// process serves the selection directly; on a native Wayland compositor the
    /// `wayland-data-control` backend hands the data off to the compositor/clipboard
    /// manager instead.
    sys_clipboard: Option<arboard::Clipboard>,
    status: String,
    /// Last status text seen by the expiry logic, and when it first appeared.
    status_seen: String,
    status_seen_at: Instant,
    dest_dialog: Option<DestDialog>,
    pending_transfer: Option<PendingTransfer>,
    pending_delete: Option<Vec<PathBuf>>,
    pending_extract: Option<PendingExtract>,
    undo_stack: Vec<UndoAction>,
    redo_stack: Vec<UndoAction>,
    settings: Settings,
    config_path: PathBuf,
    config_write_blocked: bool,
    /// `zoom.txt` next to the settings file: the interface zoom (Ctrl + / Ctrl -), remembered
    /// across runs.
    zoom_path: PathBuf,
    last_zoom: f32,
    thumbnail_store: Arc<ThumbnailStore>,
    thumbnail_jobs: SyncSender<ThumbJob>,
    worker_messages: Receiver<WorkerMessage>,
    worker_sender: mpsc::Sender<WorkerMessage>,
    /// Used by job threads to wake the UI when they finish.
    egui_ctx: egui::Context,
    /// File operations currently running on job threads.
    jobs_running: usize,
    /// Bumped by every new folder search; a finished search for an older number is dropped.
    search_generation: u64,
    thumbnails_pending: HashSet<PathBuf>,
    thumbnails_failed: HashSet<PathBuf>,
    textures: HashMap<PathBuf, CachedTexture>,
    texture_order: VecDeque<PathBuf>,
    texture_cache_bytes: usize,
    maintenance_running: bool,
    /// Hot folders are still being loaded into RAM (a few milliseconds per frame).
    priming: bool,
    behavior: Behavior,
    state_file: AtomicFile,
    flusher: Flusher,
    write_counter: WriteCounter,
    /// SIGTERM/SIGHUP/SIGINT and logind suspend/shutdown hooks.
    lifecycle: fm_lifecycle::Lifecycle,
    last_fold: Instant,
    thumbs_pending_seen: usize,
    /// The folder shown before the current one, used to tell "back" from "open".
    prev_location: Option<PathBuf>,
    watcher: Option<fm_notify::DirWatcher>,
    /// One inotify instance for every hot directory (the on-screen folder keeps `watcher`).
    tier_watcher: Option<(fm_notify::Watcher, Receiver<Vec<fm_notify::Event>>)>,
    /// Extra windows of this process.
    children: Vec<ChildWindow>,
    window_serial: u32,
    /// Folders other launches of the program asked this instance to open in a new window.
    open_requests: Option<Receiver<PathBuf>>,
    /// The first window was closed while other windows were open: it is only hidden, because
    /// closing egui's root window would end the whole process.
    root_hidden: bool,
    /// The program is really shutting down (SIGTERM, logout, last window gone).
    quitting: bool,
}

impl App {
    fn new(
        frameless: bool,
        ctx: &egui::Context,
        cli_start: Option<PathBuf>,
        listener: Option<single_instance::Listener>,
    ) -> Self {
        let config_path = fm_settings::default_config_path("file-manager")
            .unwrap_or_else(|| PathBuf::from(".config/file-manager/settings.toml"));
        let (settings, settings_error) = Settings::load_or_default(&config_path);
        let config_write_blocked = settings_error.is_some() && config_path.exists();
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        // Restore the interface zoom the user last chose with Ctrl + / Ctrl -, so the window
        // opens at the size they are used to.
        let zoom_path = config_dir.join("zoom.txt");
        let saved_zoom = fs::read_to_string(&zoom_path)
            .ok()
            .and_then(|text| parse_zoom(&text));
        if let Some(zoom) = saved_zoom {
            ctx.set_zoom_factor(zoom);
        }
        let last_zoom = ctx.zoom_factor();
        if saved_zoom.is_none() {
            write_zoom(&zoom_path, last_zoom);
        }
        if !config_path.exists() {
            // Missing config, or a malformed file successfully quarantined as *.bad.
            if let Err(error) = settings.save(&config_path) {
                eprintln!("Could not initialize settings: {error}");
            }
        }

        let icon_dir = config_dir.join("icons");
        match egui_explorer::initialize_icon_placeholders(&icon_dir) {
            Ok(created) if created > 0 => eprintln!(
                "Initialized {created} inactive icon placeholders in {}",
                icon_dir.display()
            ),
            Ok(_) => {}
            Err(error) => eprintln!("Could not initialize icon placeholders: {error}"),
        }

        let thumbnail_cache = fm_thumbs::default_cache_dir("file-manager")
            .unwrap_or_else(|| config_dir.join("cache/thumbnails"));
        // One counter for every byte this app writes (state file and thumbnail cache).
        let write_counter = WriteCounter::new();
        let thumbnail_store = Arc::new(
            ThumbnailStore::new(thumbnail_cache, settings.thumbnail_edge)
                .with_write_counter(write_counter.clone()),
        );

        // Behaviour state: the one thing that cannot be re-derived from the filesystem.
        let state_path = dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .map(|dir| dir.join("file-manager").join("behavior.txt"))
            .unwrap_or_else(|| config_dir.join("behavior.txt"));
        let mut state_file = AtomicFile::new(state_path, write_counter.clone());
        let started_at = unix_now();
        let behavior = match state_file.read() {
            Ok(Some(bytes)) => Behavior::from_bytes(&bytes, started_at).unwrap_or_else(|error| {
                eprintln!("Ignoring behaviour state ({error}); starting fresh.");
                Behavior::new()
            }),
            Ok(None) => Behavior::new(),
            Err(error) => {
                eprintln!("Could not read behaviour state: {error}");
                Behavior::new()
            }
        };
        restore_tiers(&behavior, started_at);

        let (job_tx, job_rx) = mpsc::sync_channel::<ThumbJob>(THUMBNAIL_QUEUE_CAPACITY);
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
        let maintenance_tx = worker_tx.clone();
        let shared_job_rx = Arc::new(Mutex::new(job_rx));
        for worker_number in 0..THUMBNAIL_WORKERS {
            let jobs = Arc::clone(&shared_job_rx);
            let tx = worker_tx.clone();
            let store = Arc::clone(&thumbnail_store);
            let worker_ctx = ctx.clone();
            let spawn = thread::Builder::new()
                .name(format!("file-manager-thumbnail-{worker_number}"))
                .spawn(move || {
                    loop {
                        let received = {
                            let receiver =
                                jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            receiver.recv()
                        };
                        let Ok(job) = received else { break };
                        let path = job.path;
                        let result = store
                            .ensure_hinted(&path, job.hot)
                            .map_err(|error| error.to_string());
                        let _ = tx.send(WorkerMessage::Thumbnail { path, result });
                        worker_ctx.request_repaint();
                    }
                });
            if let Err(error) = spawn {
                eprintln!("Could not start thumbnail worker: {error}");
            }
        }
        drop(worker_tx);

        let mut explorer = Explorer::new();
        explorer.options.title_bar = frameless;
        let mut state = ExplorerState::new();
        state.subfolders_fn = Some(subfolders);
        load_settings_into_state(&mut state, &settings);
        if let Some(arg) =
            std::env::args().find_map(|a| a.strip_prefix("--ribbon=").map(str::to_owned))
        {
            state.ribbon_mode = match arg.as_str() {
                "hidden" => RibbonMode::Hidden,
                "minimized" => RibbonMode::Minimized,
                _ => RibbonMode::Expanded,
            };
        }
        let arg = |key: &str| std::env::args().find_map(|a| a.strip_prefix(key).map(str::to_owned));
        if let Some(value) = arg("--view=") {
            state.view = match value.as_str() {
                "xl" => egui_explorer::ViewMode::ExtraLargeIcons,
                "large" => egui_explorer::ViewMode::LargeIcons,
                "medium" => egui_explorer::ViewMode::MediumIcons,
                "small" => egui_explorer::ViewMode::SmallIcons,
                "list" => egui_explorer::ViewMode::List,
                "tiles" => egui_explorer::ViewMode::Tiles,
                "content" => egui_explorer::ViewMode::Content,
                _ => egui_explorer::ViewMode::Details,
            };
        }
        if let Some(value) = arg("--group=") {
            state.group = match value.as_str() {
                "type" => egui_explorer::GroupKey::Type,
                "name" => egui_explorer::GroupKey::Name,
                "size" => egui_explorer::GroupKey::Size,
                "date" => egui_explorer::GroupKey::DateModified,
                _ => egui_explorer::GroupKey::None,
            };
        }
        match arg("--pane=").as_deref() {
            Some("details") => state.show_details_pane = true,
            Some("preview") => state.show_preview_pane = true,
            _ => {}
        }
        if let Some(tab) = arg("--tab=") {
            state.active_tab = tab;
        }

        let user_pins: Vec<PathBuf> = behavior.pins().cloned().collect();
        state.nav = build_nav(&user_pins);
        let start = cli_start
            .filter(|path| path.is_dir())
            .or_else(|| {
                settings
                    .last_location
                    .as_ref()
                    .filter(|path| path.is_dir())
                    .cloned()
            })
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
        seed_tiers();
        let entries = load_dir(&start, true);
        state.set_location(start, entries);
        let mut watcher = match fm_notify::DirWatcher::new({
            let wake_ctx = ctx.clone();
            move || wake_ctx.request_repaint()
        }) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                eprintln!(
                    "Filesystem notifications are unavailable; manual refresh remains available: {error}"
                );
                None
            }
        };
        if let Some(watcher) = watcher.as_mut() {
            if let Err(error) = watcher.set_dir(Some(&state.location)) {
                eprintln!(
                    "Could not watch {} for changes: {error}",
                    state.location.display()
                );
            }
        }
        let tier_watcher = {
            let wake_ctx = ctx.clone();
            let (tx, rx) = mpsc::channel();
            match fm_notify::Watcher::new(fm_notify::Config::default(), move |batch| {
                if tx.send(batch).is_ok() {
                    wake_ctx.request_repaint();
                }
            }) {
                Ok(watcher) => Some((watcher, rx)),
                Err(error) => {
                    eprintln!(
                        "Hot-folder notifications are unavailable; hot folders fall back to warm validation: {error}"
                    );
                    None
                }
            }
        };
        if std::env::args().any(|a| a == "--select-some") {
            let picks: Vec<PathBuf> = state
                .entries
                .iter()
                .filter(|entry| !entry.is_dir())
                .take(2)
                .map(|entry| entry.path.clone())
                .collect();
            for path in picks {
                state.selection.insert(path.clone());
                state.focus = Some(path);
            }
        }

        let mut app = Self {
            explorer,
            state,
            clipboard: None,
            sys_clipboard: None,
            status: settings_error
                .map(|error| format!("Settings were reset to defaults: {error}"))
                .unwrap_or_default(),
            status_seen: String::new(),
            status_seen_at: Instant::now(),
            dest_dialog: None,
            pending_transfer: None,
            pending_delete: None,
            zoom_path,
            last_zoom,
            pending_extract: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            settings,
            config_path,
            config_write_blocked,
            thumbnail_store,
            thumbnail_jobs: job_tx,
            worker_messages: worker_rx,
            worker_sender: maintenance_tx,
            egui_ctx: ctx.clone(),
            jobs_running: 0,
            search_generation: 0,
            thumbnails_pending: HashSet::new(),
            thumbnails_failed: HashSet::new(),
            textures: HashMap::new(),
            texture_order: VecDeque::new(),
            texture_cache_bytes: 0,
            maintenance_running: false,
            priming: false,
            behavior,
            state_file,
            flusher: Flusher::new(FlushPolicy::default(), Instant::now()),
            write_counter,
            lifecycle: fm_lifecycle::Lifecycle::start({
                let wake_ctx = ctx.clone();
                move || wake_ctx.request_repaint()
            }),
            last_fold: Instant::now(),
            thumbs_pending_seen: 0,
            prev_location: None,
            watcher,
            tier_watcher,
            children: Vec::new(),
            window_serial: 0,
            open_requests: listener.map(|listener| listener.serve(ctx.clone())),
            root_hidden: false,
            quitting: false,
        };
        let hooks = app.lifecycle.status();
        if !hooks.signals {
            eprintln!("Could not install signal handlers; state is flushed only on window close.");
        }
        app.behavior.open_dir(&app.state.location, started_at);
        app.apply_watch_plan();
        app
    }

    /// When `ribbon.remember` / `view.remember` are on, keep the settings in step with what
    /// the user chose in the app. `save` is change-aware, so this writes only on a change.
    fn sync_remembered_prefs(&mut self) {
        let mut changed = false;
        if self.settings.ribbon.remember {
            let mode = match self.state.ribbon_mode {
                RibbonMode::Expanded => SavedRibbonMode::Expanded,
                RibbonMode::Minimized => SavedRibbonMode::Minimized,
                RibbonMode::Hidden => SavedRibbonMode::Hidden,
            };
            if mode != self.settings.ribbon.mode {
                self.settings.ribbon.mode = mode;
                changed = true;
            }
        }
        if self.settings.view.remember {
            let mode = match self.state.view {
                egui_explorer::ViewMode::ExtraLargeIcons => SavedViewMode::ExtraLargeIcons,
                egui_explorer::ViewMode::LargeIcons => SavedViewMode::LargeIcons,
                egui_explorer::ViewMode::MediumIcons => SavedViewMode::MediumIcons,
                egui_explorer::ViewMode::SmallIcons => SavedViewMode::SmallIcons,
                egui_explorer::ViewMode::List => SavedViewMode::List,
                egui_explorer::ViewMode::Details => SavedViewMode::Details,
                egui_explorer::ViewMode::Tiles => SavedViewMode::Tiles,
                egui_explorer::ViewMode::Content => SavedViewMode::Content,
            };
            if mode != self.settings.view.mode {
                self.settings.view.mode = mode;
                changed = true;
            }
        }
        if changed {
            self.save_settings();
        }
    }

    fn save_settings(&mut self) {
        if self.config_write_blocked {
            self.status = "Settings changed for this session only: the existing settings file could not be read or safely quarantined, so it was not overwritten.".into();
            return;
        }
        if let Err(error) = self.settings.save(&self.config_path) {
            self.status = format!("Could not save settings: {error}");
        }
    }

    fn attach_current_thumbnails(&mut self) {
        attach_cached_thumbnails(
            &mut self.state.entries,
            &self.textures,
            self.settings.thumbnails,
        );
    }

    fn navigate(&mut self, path: PathBuf) {
        // Synthetic nav-tree anchors are not real folders; redirect them.
        let path = match path.to_str() {
            Some("this-pc://") => PathBuf::from("/"),
            Some(QUICK_ACCESS_ID) => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            _ => path,
        };
        self.status.clear();
        if path == trash_dir() && !path.exists() {
            if let Err(error) = fs::create_dir_all(&path) {
                self.status = format!("Could not initialize Trash: {error}");
                return;
            }
        }
        if path.is_dir() {
            let refresh = path == self.state.location;
            if refresh {
                // A refresh (or re-click) of the folder on screen must never be answered from RAM.
                tiers().invalidate(&path);
            }
            let mut entries = load_dir(&path, true);
            attach_cached_thumbnails(&mut entries, &self.textures, self.settings.thumbnails);
            if !refresh {
                let came_from =
                    std::mem::replace(&mut self.prev_location, Some(self.state.location.clone()));
                if came_from.as_deref() == Some(path.as_path()) {
                    self.behavior.back(&path, unix_now());
                } else {
                    self.behavior.open_dir(&path, unix_now());
                }
            }
            self.state.set_location(path.clone(), entries);
            self.thumbnails_failed.clear();
            if let Some(watcher) = self.watcher.as_mut() {
                if let Err(error) = watcher.set_dir(Some(&path)) {
                    self.status =
                        format!("Could not watch {} for changes: {error}", path.display());
                }
            }
            // The visit may have promoted this folder to the hot tier.
            self.apply_watch_plan();
        } else {
            self.status = format!("Cannot find '{}'.", path.display());
        }
    }

    /// Apply tier events from the hot-folder watcher to the RAM listings.
    fn pump_tier_events(&mut self) {
        let Some((_, rx)) = self.tier_watcher.as_ref() else {
            return;
        };
        let mut batches = Vec::new();
        while let Ok(batch) = rx.try_recv() {
            batches.push(batch);
        }
        if batches.is_empty() {
            return;
        }
        let mut tiers = tiers();
        for event in batches.iter().flatten() {
            match event {
                fm_notify::Event::Created { path, .. }
                | fm_notify::Event::Removed { path, .. }
                | fm_notify::Event::Modified { path } => tiers.touch_path(path),
                fm_notify::Event::Renamed { from, to, .. } => {
                    tiers.touch_path(from);
                    tiers.touch_path(to);
                }
                fm_notify::Event::WatchRemoved { path } => {
                    tiers.touch_path(path);
                    tiers.unwatched(path);
                }
                fm_notify::Event::Rescan => tiers.invalidate_all(),
                _ => tiers.invalidate_all(),
            }
        }
    }

    /// Make the set of inotify watches match the hot tier.
    fn apply_watch_plan(&mut self) {
        // Newly hot folders must be loaded into RAM for hot-folder search.
        self.priming = self.settings.indexing_enabled;
        let Some((watcher, _)) = self.tier_watcher.as_ref() else {
            return;
        };
        let plan = tiers().watch_plan();
        if plan.add.is_empty() && plan.remove.is_empty() {
            return;
        }
        for dir in &plan.remove {
            let _ = watcher.unwatch(dir);
        }
        let results: Vec<(PathBuf, bool)> = plan
            .add
            .into_iter()
            .map(|dir| {
                let ok = watcher.watch(&dir).is_ok();
                (dir, ok)
            })
            .collect();
        let mut tiers = tiers();
        for dir in &plan.remove {
            tiers.unwatched(dir);
        }
        for (dir, ok) in results {
            tiers.watch_result(&dir, ok);
        }
    }

    /// Load hot folders into RAM a few milliseconds per frame until none are left.
    fn prime_step(&mut self, ctx: &egui::Context) {
        if !self.priming {
            return;
        }
        if tiers().prime_hot(PRIME_BUDGET) == 0 {
            self.priming = false;
        } else {
            ctx.request_repaint();
        }
    }

    /// Save the interface zoom to `zoom.txt` whenever the user changes it.
    fn persist_zoom(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if (zoom - self.last_zoom).abs() > 1e-4 {
            self.last_zoom = zoom;
            write_zoom(&self.zoom_path, zoom);
        }
    }

    /// Fold behaviour events now and then, and flush state when the policy says so
    /// (rarely: at shutdown, or after enough change and enough time).
    fn tick_persistence(&mut self, ctx: &egui::Context) {
        if self.last_fold.elapsed() >= FOLD_INTERVAL {
            self.last_fold = Instant::now();
            if self.behavior.pending() > 0 {
                self.behavior.fold(unix_now());
            }
            self.flusher.mark_dirty(self.behavior.take_dirty());
            let queued = self.thumbnail_store.pending_writes();
            if queued > self.thumbs_pending_seen {
                self.flusher
                    .mark_dirty((queued - self.thumbs_pending_seen) as f32);
            }
            self.thumbs_pending_seen = queued;
        }
        if self.flusher.should_flush(Instant::now()) {
            self.flush_state();
        }
        ctx.request_repaint_after(IDLE_WAKE);
    }

    /// React to SIGTERM and to logind suspend/shutdown: flush first, then let the system go on.
    fn handle_lifecycle(&mut self, ctx: &egui::Context) {
        for event in self.lifecycle.poll() {
            match event {
                fm_lifecycle::Event::Terminate(_) | fm_lifecycle::Event::Shutdown => {
                    self.quitting = true;
                    self.flush_state();
                    self.lifecycle.release_inhibitor();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                fm_lifecycle::Event::Suspend => {
                    self.flush_state();
                    self.lifecycle.release_inhibitor();
                }
                fm_lifecycle::Event::Resume => {
                    self.lifecycle.rearm_inhibitor();
                    // Mounts and files may have changed while asleep: never trust RAM listings.
                    tiers().invalidate_all();
                    self.reload();
                    self.priming = self.settings.indexing_enabled;
                }
            }
        }
    }

    /// Write behaviour state (unless unchanged) and the queued thumbnails in one batch.
    fn flush_state(&mut self) {
        let now = unix_now();
        self.behavior.fold(now);
        self.behavior.set_tiers(tiers().table().export_raw(now));
        let bytes = self.behavior.to_bytes();
        match self.state_file.write_if_changed(&bytes) {
            Ok(Outcome::Written { .. } | Outcome::Skipped) => {}
            Err(error) => self.status = format!("Could not save behaviour state: {error}"),
        }
        let thumbs = self.thumbnail_store.flush_pending();
        if thumbs.failed > 0 {
            self.status = format!("{} thumbnail cache write(s) failed.", thumbs.failed);
        }
        let _ = self.behavior.take_dirty();
        self.thumbs_pending_seen = 0;
        self.flusher.flushed(Instant::now());
        if std::env::var_os("FM_WRITE_STATS").is_some() {
            let stats = self.write_counter.snapshot();
            eprintln!(
                "file-manager disk writes: {} bytes in {} write(s), {} skipped as unchanged, ~{:.0} bytes/hour",
                stats.bytes, stats.writes, stats.skipped, stats.bytes_per_hour,
            );
        }
    }

    /// Pin or unpin folders in Quick access. Pins are behaviour state: they are kept in RAM
    /// and reach disk only with the next flush.
    fn toggle_pins(&mut self, targets: Vec<PathBuf>) {
        let builtin: Vec<PathBuf> = quick_access_dirs()
            .into_iter()
            .map(|(dir, _, _)| dir)
            .collect();
        let now = unix_now();
        for path in targets {
            if !path.is_dir() {
                self.status = "Only folders can be pinned to Quick access.".into();
            } else if builtin.contains(&path) {
                self.status = format!(
                    "'{}' is already a built-in Quick access folder.",
                    path.display()
                );
            } else if self.behavior.is_pinned(&path) {
                self.behavior.unpin(&path, now);
                tiers().unpin(&path);
                self.status = format!("Unpinned '{}' from Quick access.", path.display());
            } else if self.behavior.pin(&path, now) {
                self.status = format!("Pinned '{}' to Quick access.", path.display());
                tiers().pin(path);
            } else {
                self.status = "The Quick access pin limit has been reached.".into();
            }
        }
        let pins: Vec<PathBuf> = self.behavior.pins().cloned().collect();
        self.state
            .set_nav_children(Path::new(QUICK_ACCESS_ID), quick_access_nodes(&pins));
        self.apply_watch_plan();
    }

    fn record_undo(&mut self, action: UndoAction) {
        self.undo_stack.push(action);
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Archive each target next to itself using the real tar/zip/7z tools. Only the item's
    /// bare name is stored as the archive member (never an absolute path). `password` is
    /// honoured for ZIP (AES-256 through 7-Zip when installed, else legacy ZipCrypto) and 7Z
    /// (AES-256, with the file names encrypted too); other formats ignore it.
    fn compress_items(
        &mut self,
        targets: &[PathBuf],
        format: CompressFormat,
        level: u8,
        password: Option<&str>,
    ) {
        let targets = targets.to_vec();
        let password = password.map(str::to_owned);
        let origin = self.state.location.clone();
        self.status = "Compressing…".into();
        self.spawn_job(move || compress_job(origin, targets, format, level, password));
    }

    /// Run `work` on its own thread. The result comes back as `WorkerMessage::Job`, so the UI
    /// thread never waits for the disk or for an external tool.
    fn spawn_job(&mut self, work: impl FnOnce() -> JobOutcome + Send + 'static) {
        let sender = self.worker_messages_sender();
        let wake = self.egui_ctx.clone();
        let spawned = thread::Builder::new()
            .name("fm-job".into())
            .spawn(move || {
                let outcome = work();
                let _ = sender.send(WorkerMessage::Job(outcome));
                wake.request_repaint();
            });
        match spawned {
            Ok(_) => self.jobs_running += 1,
            Err(error) => self.status = format!("Could not start background job: {error}"),
        }
    }

    /// Extract every archive among `targets`: into a new folder named after each archive, or
    /// (`here`) straight into the folder it is in. Password protected archives open the
    /// password dialog.
    fn extract_items(&mut self, targets: &[PathBuf], here: bool) {
        let archives: Vec<PathBuf> = targets
            .iter()
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .is_some_and(|n| egui_explorer::is_archive_name(&n.to_string_lossy()))
            })
            .cloned()
            .collect();
        if archives.is_empty() {
            self.status = "Select an archive to extract.".into();
            return;
        }
        self.run_extract_queue(archives, here, None);
    }

    /// Extract the archives in `queue` one after another on a job thread. `password` applies
    /// to the first one only. Stops at the first archive that needs (or rejected) a password
    /// and parks the rest in `pending_extract` for the password dialog.
    fn run_extract_queue(&mut self, queue: Vec<PathBuf>, here: bool, password: Option<String>) {
        let origin = self.state.location.clone();
        self.status = "Extracting…".into();
        self.spawn_job(move || extract_job(origin, queue, here, password));
    }

    fn dest_dialog_ui(&mut self, ctx: &egui::Context) {
        let here = self.state.location.clone();
        // Always asked (even while closed) so the dialog is centred again on every opening.
        let id = dialogs::session_id(ctx, "dest_dialog", self.dest_dialog.is_some());
        let Some(dialog) = self.dest_dialog.as_mut() else {
            return;
        };
        let title = if dialog.cut { "Move to" } else { "Copy to" };
        let mut confirm = false;
        let mut cancel = false;
        let outcome = dialogs::show(ctx, id, title, 460.0, |ui| {
            dialogs::body(ui, |ui| {
                ui.label("Destination folder:");
                ui.add(egui::TextEdit::singleline(&mut dialog.text).desired_width(f32::INFINITY));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Current folder").clicked() {
                        dialog.text = here.display().to_string();
                    }
                    if ui.button("Home").clicked() {
                        if let Some(home) = dirs::home_dir() {
                            dialog.text = home.display().to_string();
                        }
                    }
                    for (dir, _, _) in quick_access_dirs() {
                        let label = dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "/".to_string());
                        if ui.button(label).clicked() {
                            dialog.text = dir.display().to_string();
                        }
                    }
                });
            });
            dialogs::footer(ui, |ui| {
                if dialogs::button(ui, title, true).clicked() {
                    confirm = true;
                }
                if dialogs::button(ui, "Cancel", false).clicked() {
                    cancel = true;
                }
            });
        });
        // "Move to" / "Copy to" is this dialog's primary action, so Enter confirms it; the
        // backdrop and Escape are always the safe cancel.
        cancel |= outcome.dismissed;
        confirm |= outcome.confirmed;
        if cancel {
            self.dest_dialog = None;
            return;
        }
        if confirm {
            if let Some(done) = self.dest_dialog.take() {
                self.transfer(done);
            }
        }
    }

    fn transfer(&mut self, dialog: DestDialog) {
        let dest = PathBuf::from(dialog.text.trim());
        if !dest.is_dir() {
            self.status = format!("'{}' is not an existing folder.", dest.display());
            self.dest_dialog = Some(dialog);
            return;
        }
        self.start_transfer(dialog.paths, dest, dialog.cut, false);
    }

    /// Copy/move `paths` into `dest`; asks first when a name already exists there.
    fn start_transfer(&mut self, paths: Vec<PathBuf>, dest: PathBuf, cut: bool, paste: bool) {
        let conflicts: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                let name = p.file_name()?;
                let target = dest.join(name);
                (target.exists() && target != *p).then(|| name.to_string_lossy().into_owned())
            })
            .collect();
        if conflicts.is_empty() {
            self.run_transfer(paths, dest, cut, paste, ConflictChoice::KeepBoth);
        } else {
            self.pending_transfer = Some(PendingTransfer {
                paths,
                dest,
                cut,
                paste,
                conflicts,
            });
        }
    }

    fn run_transfer(
        &mut self,
        paths: Vec<PathBuf>,
        dest: PathBuf,
        cut: bool,
        paste: bool,
        choice: ConflictChoice,
    ) {
        let origin = self.state.location.clone();
        self.status = format!("{}…", if cut { "Moving" } else { "Copying" });
        if paste {
            // Same immediate effect as before: a pasted cut is consumed right away.
            if cut {
                self.clipboard = None;
            }
            self.state.cut.clear();
            self.state.clipboard_has_items = self.clipboard.is_some();
        }
        self.spawn_job(move || transfer_job(origin, paths, dest, cut, choice));
    }

    /// Offer the copied files to other applications as a standard file list
    /// (`text/uri-list`). The in-app clipboard stays authoritative for Windowstyle itself;
    /// this is purely for interoperability. `arboard`'s `wayland-data-control` feature is
    /// enabled so this reaches native Wayland compositors too: arboard tries the Wayland
    /// data-control protocol first and falls back to X11 (which also covers XWayland)
    /// automatically. If neither backend is reachable the copy still works inside the app.
    fn publish_files_to_system_clipboard(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        if self.sys_clipboard.is_none() {
            self.sys_clipboard = arboard::Clipboard::new().ok();
        }
        match self.sys_clipboard.as_mut() {
            Some(clipboard) => {
                if let Err(error) = clipboard.set().file_list(paths) {
                    self.status =
                        format!("Copied inside Windowstyle only (system clipboard: {error}).");
                }
            }
            None => {
                self.status = "Copied inside Windowstyle only (system clipboard unavailable)."
                    .to_string();
            }
        }
    }

    fn permanent_delete(&mut self, targets: Vec<PathBuf>) {
        let origin = self.state.location.clone();
        let in_trash = self.state.in_trash();
        self.status = "Deleting…".into();
        self.spawn_job(move || delete_job(origin, targets, in_trash));
    }

    /// Confirmation dialogs: permanent delete and name-conflict (replace / keep both).
    fn confirm_dialogs_ui(&mut self, ctx: &egui::Context) {
        // Asked every frame (even while closed) so each dialog is centred on every opening.
        let delete_id = dialogs::session_id(ctx, "delete_confirm", self.pending_delete.is_some());
        let transfer_id =
            dialogs::session_id(ctx, "transfer_conflict", self.pending_transfer.is_some());
        let extract_id =
            dialogs::session_id(ctx, "extract_password", self.pending_extract.is_some());
        if let Some(pe) = self.pending_extract.as_mut() {
            let name = pe
                .archives
                .first()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let wrong = pe.wrong;
            let (mut ok, mut cancel) = (false, false);
            let outcome = {
                let password = &mut pe.password;
                dialogs::show(ctx, extract_id, "Password required", 400.0, |ui| {
                    dialogs::body(ui, |ui| {
                        ui.add(egui::Label::new(format!("'{name}' is password protected.")).wrap());
                        if wrong {
                            ui.label(
                                egui::RichText::new("Incorrect password. Try again.")
                                    .color(egui::Color32::from_rgb(200, 40, 40)),
                            );
                        }
                        let edit = ui.add(
                            egui::TextEdit::singleline(password)
                                .password(true)
                                .hint_text("Password")
                                .desired_width(f32::INFINITY),
                        );
                        // Keep the caret in the field so typing (and Enter) always lands here.
                        edit.request_focus();
                    });
                    dialogs::footer(ui, |ui| {
                        if dialogs::button(ui, "Extract", true).clicked() {
                            ok = true;
                        }
                        if dialogs::button(ui, "Cancel", false).clicked() {
                            cancel = true;
                        }
                    });
                })
            };
            ok |= outcome.confirmed;
            cancel |= outcome.dismissed;
            let ready = ok && !pe.password.is_empty();
            if ready {
                if let Some(pe) = self.pending_extract.take() {
                    self.run_extract_queue(pe.archives, pe.here, Some(pe.password));
                }
            } else if cancel {
                self.pending_extract = None;
                self.status = "Extraction cancelled.".into();
            }
        }
        if let Some(items) = self.pending_delete.as_ref() {
            let text = if items.len() == 1 {
                let name = items[0]
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!("Permanently delete '{name}'?")
            } else {
                format!("Permanently delete these {} items?", items.len())
            };
            let (mut yes, mut no) = (false, false);
            let outcome = dialogs::show(ctx, delete_id, "Delete permanently", 380.0, |ui| {
                dialogs::body(ui, |ui| {
                    ui.add(egui::Label::new(text).wrap());
                    ui.label(
                        egui::RichText::new("This cannot be undone.")
                            .color(egui_explorer::theme::colors::text_secondary()),
                    );
                });
                dialogs::footer(ui, |ui| {
                    // Cancel is the default (safe) action of a destructive prompt.
                    if dialogs::button(ui, "Cancel", true).clicked() {
                        no = true;
                    }
                    if dialogs::button(ui, "Delete", false).clicked() {
                        yes = true;
                    }
                });
            });
            // Cancel is also this dialog's primary action, so the backdrop, Escape and Enter
            // all land on it too (clicking outside never deletes anything).
            no |= outcome.dismissed || outcome.confirmed;
            if yes {
                if let Some(items) = self.pending_delete.take() {
                    self.permanent_delete(items);
                }
            } else if no {
                self.pending_delete = None;
            }
        }
        if let Some(pt) = self.pending_transfer.as_ref() {
            let text = if pt.conflicts.len() == 1 {
                format!(
                    "'{}' already exists in the destination folder.",
                    pt.conflicts[0]
                )
            } else {
                format!(
                    "{} items already exist in the destination folder.",
                    pt.conflicts.len()
                )
            };
            let (mut replace, mut keep, mut cancel) = (false, false, false);
            let title = if pt.cut { "Move" } else { "Copy" };
            let outcome = dialogs::show(ctx, transfer_id, title, 440.0, |ui| {
                dialogs::body(ui, |ui| {
                    ui.add(egui::Label::new(text).wrap());
                });
                dialogs::footer(ui, |ui| {
                    if dialogs::button(ui, "Cancel", true).clicked() {
                        cancel = true;
                    }
                    if dialogs::button(ui, "Keep both (rename)", false).clicked() {
                        keep = true;
                    }
                    if dialogs::button(ui, "Replace", false).clicked() {
                        replace = true;
                    }
                });
            });
            // Cancel is also this dialog's primary action, so the backdrop, Escape and Enter
            // all land on it too.
            cancel |= outcome.dismissed || outcome.confirmed;
            if replace || keep {
                if let Some(pt) = self.pending_transfer.take() {
                    let choice = if replace {
                        ConflictChoice::Replace
                    } else {
                        ConflictChoice::KeepBoth
                    };
                    self.run_transfer(pt.paths, pt.dest, pt.cut, pt.paste, choice);
                }
            } else if cancel {
                self.pending_transfer = None;
            }
        }
    }

    /// Put a freshly created item into inline-rename mode (selected, editor focused).
    fn begin_rename(&mut self, path: PathBuf) {
        let text = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.state.selection.clear();
        self.state.selection.insert(path.clone());
        self.state.renaming = Some(egui_explorer::RenameState {
            path,
            text,
            just_started: true,
        });
    }

    fn reload(&mut self) {
        // An explicit reload (user refresh, own file operation, change event) must never be
        // answered from RAM.
        tiers().invalidate(&self.state.location);
        let mut entries = load_dir(&self.state.location, false);
        attach_cached_thumbnails(&mut entries, &self.textures, self.settings.thumbnails);
        self.state.set_entries(entries);
        self.thumbnails_failed.clear();
    }

    fn request_thumbnail(&mut self, path: PathBuf) {
        if !self.settings.thumbnails {
            return;
        }
        let Some(current_signature) = self
            .state
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| (entry.size, entry.modified))
        else {
            // Ignore delayed UI events for entries no longer in the current listing.
            return;
        };
        if let Some(cached) = self.textures.get(&path) {
            if current_signature == (cached.source_size, cached.source_modified) {
                self.texture_order
                    .retain(|cached_path| cached_path != &path);
                self.texture_order.push_back(path);
                return;
            }
            if let Some(removed) = self.textures.remove(&path) {
                self.texture_cache_bytes = self
                    .texture_cache_bytes
                    .saturating_sub(removed.memory_bytes);
            }
            self.texture_order
                .retain(|cached_path| cached_path != &path);
        }
        if self.thumbnails_pending.contains(&path) || self.thumbnails_failed.contains(&path) {
            return;
        }
        self.thumbnails_pending.insert(path.clone());
        let hot = path
            .parent()
            .is_some_and(|dir| tiers().tier_of(dir) == fm_tiers::Tier::Hot);
        match self.thumbnail_jobs.try_send(ThumbJob {
            path: path.clone(),
            hot,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.thumbnails_pending.remove(&path);
                self.status = "Thumbnail queue is full; visible files will be retried as capacity becomes available.".into();
            }
            Err(TrySendError::Disconnected(_)) => {
                self.thumbnails_pending.remove(&path);
                self.thumbnails_failed.insert(path.clone());
                self.status = format!("Thumbnail worker is unavailable for {}.", path.display());
            }
        }
    }

    /// Apply the result of a job thread on the UI thread.
    fn finish_job(&mut self, outcome: JobOutcome) {
        self.jobs_running = self.jobs_running.saturating_sub(1);
        if let Some((root, entries, message)) = outcome.search {
            // Only the newest search may replace the listing, and only if the user is
            // still looking at the folder it searched.
            if outcome.search_generation == self.search_generation && self.state.location == root
            {
                self.state.set_location(root, entries);
                self.attach_current_thumbnails();
                self.status = message;
            } else if self.jobs_running == 0 && self.status.ends_with('…') {
                self.status.clear();
            }
            return;
        }
        for action in outcome.undo {
            self.record_undo(action);
        }
        if let Some(pe) = outcome.pending_extract {
            self.pending_extract = Some(pe);
        }
        match outcome.status {
            Some(status) => self.status = status,
            None if self.status.ends_with('…') => self.status.clear(),
            None => {}
        }
        tiers().invalidate(&outcome.origin);
        if outcome.origin == self.state.location {
            self.reload();
        }
    }

    fn poll_workers(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.worker_messages.try_recv() {
            match message {
                WorkerMessage::Job(outcome) => self.finish_job(outcome),
                WorkerMessage::Thumbnail { path, result } => {
                    self.thumbnails_pending.remove(&path);
                    match result {
                        Ok((thumbnail, fingerprint)) => {
                            let memory_bytes = thumbnail.rgba.len();
                            if let Some(previous) = self.textures.remove(&path) {
                                self.texture_cache_bytes = self
                                    .texture_cache_bytes
                                    .saturating_sub(previous.memory_bytes);
                                self.texture_order
                                    .retain(|cached_path| cached_path != &path);
                            }
                            while !self.texture_order.is_empty()
                                && (self.textures.len() >= TEXTURE_HANDLE_LIMIT
                                    || self.texture_cache_bytes.saturating_add(memory_bytes)
                                        > TEXTURE_CACHE_BUDGET_BYTES)
                            {
                                let Some(oldest) = self.texture_order.pop_front() else {
                                    break;
                                };
                                if let Some(removed) = self.textures.remove(&oldest) {
                                    self.texture_cache_bytes = self
                                        .texture_cache_bytes
                                        .saturating_sub(removed.memory_bytes);
                                }
                                for entry in self.state.entries_mut() {
                                    if entry.path == oldest {
                                        entry.thumbnail = None;
                                    }
                                }
                            }
                            // ThumbnailStore clamps its longest edge to 2048, so a valid
                            // RGBA thumbnail fits comfortably within this budget on its own.
                            if memory_bytes <= TEXTURE_CACHE_BUDGET_BYTES {
                                let image = ColorImage::from_rgba_unmultiplied(
                                    [thumbnail.width as usize, thumbnail.height as usize],
                                    &thumbnail.rgba,
                                );
                                let handle = ctx.load_texture(
                                    format!("thumbnail:{}", path.display()),
                                    image,
                                    TextureOptions::LINEAR,
                                );
                                self.texture_order.push_back(path.clone());
                                self.texture_cache_bytes =
                                    self.texture_cache_bytes.saturating_add(memory_bytes);
                                self.textures.insert(
                                    path.clone(),
                                    CachedTexture {
                                        handle,
                                        source_size: fingerprint.source_size,
                                        source_modified: fingerprint.source_modified,
                                        memory_bytes,
                                    },
                                );
                            }
                            if self.settings.thumbnails {
                                if let Some(entry) = self
                                    .state
                                    .entries_mut()
                                    .iter_mut()
                                    .find(|entry| entry.path == path)
                                {
                                    entry.thumbnail =
                                        self.textures.get(&path).map(|cached| cached.handle.id());
                                }
                            }
                            self.thumbnails_failed.remove(&path);
                        }
                        Err(error) => {
                            self.thumbnails_failed.insert(path.clone());
                            self.status =
                                format!("Thumbnail failed for {}: {error}", path.display());
                        }
                    }
                }
                WorkerMessage::AuditFinished { root, report } => {
                    self.maintenance_running = false;
                    self.status = format!(
                        "Thumbnail audit of {}: {} media; {} present, {} kept in RAM only (cheap to regenerate), {} missing, {} stale, {} corrupt, {} unsupported, {} errors, {} traversal errors{}.",
                        root.display(),
                        report.scanned_media,
                        report.present,
                        report.not_persisted,
                        report.missing,
                        report.stale,
                        report.corrupt,
                        report.unsupported,
                        report.errors,
                        report.traversal_errors,
                        if report.samples_truncated {
                            " (additional problem paths omitted)"
                        } else {
                            ""
                        },
                    );
                    if let Some(sample) = report.samples.first() {
                        self.status.push_str(&format!(
                            " First item needing attention: {} ({:?}).",
                            sample.path.display(),
                            sample.status
                        ));
                    }
                }
                WorkerMessage::GenerationFinished { root, report } => {
                    self.maintenance_running = false;
                    self.status = format!(
                        "Thumbnail generation under {}: {} generated, {} reused without rewriting, {} left in RAM only (cheaper to decode than to store), {} unsupported, {} failed, {} traversal errors.",
                        root.display(),
                        report.generated,
                        report.reused,
                        report.not_persisted,
                        report.unsupported,
                        report.failed,
                        report.traversal_errors,
                    );
                    self.thumbnails_failed.clear();
                }
            }
        }
    }

    fn start_audit(&mut self, ctx: &egui::Context) {
        if self.maintenance_running {
            self.status = "Another maintenance scan is already running.".into();
            return;
        }
        let root = self.state.location.clone();
        self.maintenance_running = true;
        self.status = format!(
            "Auditing thumbnail cache under {} (read-only)…",
            root.display()
        );
        let store = Arc::clone(&self.thumbnail_store);
        let tx = self.worker_messages_sender();
        let worker_ctx = ctx.clone();
        if let Err(error) = thread::Builder::new()
            .name("file-manager-thumbnail-audit".into())
            .spawn(move || {
                let report = store.audit_tree(&root);
                let _ = tx.send(WorkerMessage::AuditFinished { root, report });
                worker_ctx.request_repaint();
            })
        {
            self.maintenance_running = false;
            self.status = format!("Could not start thumbnail audit: {error}");
        }
    }

    fn start_generation(&mut self, ctx: &egui::Context) {
        if self.maintenance_running {
            self.status = "Another maintenance scan is already running.".into();
            return;
        }
        let root = self.state.location.clone();
        self.maintenance_running = true;
        self.status = format!(
            "Generating only missing/stale thumbnails under {}…",
            root.display()
        );
        let store = Arc::clone(&self.thumbnail_store);
        let tx = self.worker_messages_sender();
        let worker_ctx = ctx.clone();
        if let Err(error) = thread::Builder::new()
            .name("file-manager-thumbnail-generation".into())
            .spawn(move || {
                let report = store.generate_needed(&root);
                let _ = tx.send(WorkerMessage::GenerationFinished { root, report });
                worker_ctx.request_repaint();
            })
        {
            self.maintenance_running = false;
            self.status = format!("Could not start thumbnail generation: {error}");
        }
    }

    fn worker_messages_sender(&self) -> mpsc::Sender<WorkerMessage> {
        // Each detached worker needs a sender. Keeping one sender in the app is not
        // necessary; clone the sender held by thumbnail workers through a separate
        // sender retained in the receiver's shared channel handle.
        self.worker_sender.clone()
    }

    fn handle(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Navigate(path) => self.navigate(path),
            Event::Open(path) => {
                // `is_dir` follows symlinks: a link to a folder navigates, it is not "opened".
                // (Handing a folder to xdg-open could also start a second instance of this app.)
                if path.is_dir() {
                    self.navigate(path);
                } else {
                    self.behavior.open_file(&path, unix_now());
                    spawn_reaped(Proc::new("xdg-open").arg(&path));
                }
            }
            Event::LoadChildren(path) => {
                let kids = subfolders(&path)
                    .into_iter()
                    .filter(|child| {
                        !child
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    })
                    .map(|child| {
                        let name = child
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        NavNode::new(child, name, Icon::Folder)
                    })
                    .collect();
                self.state.set_nav_children(&path, kids);
            }
            Event::Search(query) => self.search(query),
            Event::ThumbnailNeeded(path) => self.request_thumbnail(path),
            Event::Rename { path, new_name } => {
                let to = path.with_file_name(&new_name);
                match fs::rename(&path, &to) {
                    Ok(()) => {
                        self.record_undo(UndoAction::Rename {
                            from: to.clone(),
                            to: path.clone(),
                        });
                        self.reload();
                    }
                    Err(error) => self.status = format!("Rename failed: {error}"),
                }
            }
            Event::Drop { sources, target } => {
                for source in sources {
                    if let Some(name) = source.file_name() {
                        let dest = target.join(name);
                        match fs::rename(&source, &dest) {
                            Ok(()) => self.record_undo(UndoAction::Rename {
                                from: dest,
                                to: source.clone(),
                            }),
                            Err(error) => {
                                self.status = format!("Move failed: {error}");
                                break;
                            }
                        }
                    }
                }
                self.reload();
            }
            Event::Command { cmd, targets } => self.command(ctx, cmd, targets),
        }
    }

    /// Recursive, case-insensitive name search below the current folder (no index needed).
    /// Runs on a job thread; a newer search makes an older, still running one irrelevant.
    fn search_current_folder(&mut self, query: &str) {
        let needle = query.trim().to_lowercase();
        let shown = query.trim().to_owned();
        let root = self.state.location.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.status = format!("Searching for '{shown}'…");
        self.spawn_job(move || {
            let (found, truncated) = walk_search(&root, &needle);
            let message = format!(
                "Found {} result(s) for '{shown}' in this folder and its subfolders{}.",
                found.len(),
                if truncated {
                    format!(" (limit {SEARCH_LIMIT})")
                } else {
                    String::new()
                }
            );
            JobOutcome {
                origin: root.clone(),
                search: Some((root, found, message)),
                search_generation: generation,
                ..JobOutcome::default()
            }
        });
    }

    fn search(&mut self, query: String) {
        if query.trim().is_empty() {
            self.status = "Enter a search term to search hot folders.".into();
            return;
        }
        if !self.settings.indexing_enabled {
            self.search_current_folder(&query);
            return;
        }
        let report = tiers().search_hot(&query, SEARCH_LIMIT);
        if report.dirs_searched == 0 || report.items.is_empty() {
            self.search_current_folder(&query);
            return;
        }
        let entries: Vec<Entry> = report.items.into_iter().map(entry_from_item).collect();
        let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        self.state.set_location(root, entries);
        self.attach_current_thumbnails();
        self.status = format!(
            "Hot-folder search returned {} result(s) for '{query}' from {} of {} hot folder(s) in RAM{}{}.",
            self.state.entries.len(),
            report.dirs_searched,
            report.dirs_total,
            if report.truncated {
                format!(" (limit {SEARCH_LIMIT})")
            } else {
                String::new()
            },
            if report.dirs_unprimed > 0 {
                format!("; {} not loaded yet", report.dirs_unprimed)
            } else {
                String::new()
            },
        );
    }

    fn command(&mut self, ctx: &egui::Context, cmd: Command, targets: Vec<PathBuf>) {
        let here = self.state.location.clone();
        // A new action supersedes any stale message from the previous one.
        self.status.clear();
        match cmd {
            Command::ToggleDarkMode => {
                self.state.dark_mode = !self.state.dark_mode;
                self.settings.dark_mode = self.state.dark_mode;
                self.save_settings();
            }
            Command::ToggleThumbnails => {
                self.state.thumbnails_enabled = !self.state.thumbnails_enabled;
                self.settings.thumbnails = self.state.thumbnails_enabled;
                if self.settings.thumbnails {
                    self.attach_current_thumbnails();
                    self.status =
                        "Thumbnails enabled; visible supported media will load lazily.".into();
                } else {
                    for entry in self.state.entries_mut() {
                        entry.thumbnail = None;
                    }
                    self.status =
                        "Thumbnails disabled; cached thumbnail files were retained.".into();
                }
                self.save_settings();
            }
            Command::ToggleIndexing => {
                self.state.indexing_enabled = !self.state.indexing_enabled;
                self.settings.indexing_enabled = self.state.indexing_enabled;
                if self.state.indexing_enabled {
                    self.priming = true;
                    self.status = "Hot-folder search enabled; hot folders are loading into RAM (nothing is written to disk).".into();
                } else {
                    self.priming = false;
                    self.status = "Hot-folder search disabled.".into();
                }
                self.save_settings();
            }
            Command::AuditThumbnails => self.start_audit(ctx),
            Command::GenerateMissingThumbnails => self.start_generation(ctx),
            Command::BuildIndex => {
                self.state.indexing_enabled = true;
                self.settings.indexing_enabled = true;
                self.save_settings();
                self.priming = true;
                self.status =
                    "Loading hot folders into RAM for search (no index file is written).".into();
            }
            Command::PinToQuickAccess => self.toggle_pins(targets),
            Command::Copy => {
                self.publish_files_to_system_clipboard(&targets);
                self.clipboard = Some((targets, false));
            }
            Command::Cut => {
                self.publish_files_to_system_clipboard(&targets);
                self.clipboard = Some((targets, true));
            }
            Command::Paste => {
                if let Some((paths, cut)) = self.clipboard.clone() {
                    self.start_transfer(paths, here.to_path_buf(), cut, true);
                }
            }
            Command::PasteShortcut => {
                if let Some((paths, _)) = self.clipboard.clone() {
                    let mut made = 0usize;
                    for path in &paths {
                        let Some(name) = path.file_name() else {
                            continue;
                        };
                        let link =
                            unique(&here.join(format!("{} - Shortcut", name.to_string_lossy())));
                        match make_link(path, &link) {
                            Ok(()) => {
                                made += 1;
                                self.record_undo(UndoAction::Trash(link.clone()));
                            }
                            Err(error) => {
                                self.status = format!("Create shortcut failed: {error}");
                                break;
                            }
                        }
                    }
                    if made > 0 {
                        self.status = format!("Created {made} shortcut(s).");
                    }
                    self.reload();
                }
            }
            Command::CopyPath => {
                let text = targets
                    .iter()
                    .map(|path| format!("\"{}\"", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.copy_text(text);
            }
            Command::CopyFolderPath => {
                let mut folders: Vec<String> = Vec::new();
                for path in &targets {
                    let dir = path.parent().unwrap_or(&here).display().to_string();
                    if !folders.contains(&dir) {
                        folders.push(dir);
                    }
                }
                if folders.is_empty() {
                    folders.push(here.display().to_string());
                }
                ctx.copy_text(folders.join("\n"));
            }
            Command::NewFolder => {
                let new_path = unique(&here.join("New folder"));
                match fs::create_dir(&new_path) {
                    Ok(()) => {
                        self.record_undo(UndoAction::Trash(new_path.clone()));
                        self.reload();
                        self.begin_rename(new_path);
                    }
                    Err(error) => self.status = format!("Create folder failed: {error}"),
                }
            }
            Command::NewTextFile => {
                let new_path = unique(&here.join("New Text Document.txt"));
                match fs::write(&new_path, "") {
                    Ok(()) => {
                        self.record_undo(UndoAction::Trash(new_path.clone()));
                        self.reload();
                        self.begin_rename(new_path);
                    }
                    Err(error) => self.status = format!("Create file failed: {error}"),
                }
            }
            Command::MoveTo | Command::CopyTo => {
                if !targets.is_empty() {
                    self.dest_dialog = Some(DestDialog {
                        cut: cmd == Command::MoveTo,
                        paths: targets,
                        text: here.display().to_string(),
                    });
                }
            }
            Command::EmptyTrash => match fm_trash::empty() {
                Ok(count) => {
                    self.status = format!("Emptied Trash ({count} item(s)).");
                    self.reload();
                }
                Err(error) => self.status = format!("Could not empty Trash: {error}"),
            },
            Command::RestoreFromTrash => match fm_trash::list() {
                Ok(items) => {
                    let mut restored = 0usize;
                    let mut failed = false;
                    for path in &targets {
                        if let Some(item) = items.iter().find(|i| i.trashed_path() == *path) {
                            match item.restore() {
                                Ok(_) => restored += 1,
                                Err(error) => {
                                    self.status = format!("Restore failed: {error}");
                                    failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    if restored > 0 && !failed {
                        self.status = format!("Restored {restored} item(s).");
                    }
                    self.reload();
                }
                Err(error) => self.status = format!("Could not read Trash: {error}"),
            },
            Command::CopyName => {
                let text = targets
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.copy_text(text);
            }
            Command::CopyUrl => {
                let text = targets
                    .iter()
                    .map(|p| file_url(p))
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.copy_text(text);
            }
            Command::Edit => {
                for path in &targets {
                    if !open_in_editor(path) {
                        self.status = "No editor could be started.".into();
                    }
                }
            }
            Command::Email => {
                let template = self.settings.email_command.clone();
                let mut parts = template.split_whitespace();
                let Some(program) = parts.next() else {
                    self.status =
                        "The email command is empty (set email_command in settings).".into();
                    return;
                };
                let files: Vec<String> = targets
                    .iter()
                    .filter(|p| p.is_file())
                    .map(|p| p.display().to_string())
                    .collect();
                let mut args: Vec<String> = Vec::new();
                let mut used_placeholder = false;
                for part in parts {
                    if part.contains("{file}") {
                        used_placeholder = true;
                        for file in &files {
                            args.push(part.replace("{file}", file));
                        }
                    } else {
                        args.push(part.to_string());
                    }
                }
                if !used_placeholder {
                    args.extend(files);
                }
                let mut mail = Proc::new(program);
                mail.args(&args);
                spawn_reaped(&mut mail);
            }
            Command::Undo => {
                if let Some(action) = self.undo_stack.pop() {
                    match run_undo_action(action) {
                        Ok(inverse) => {
                            self.redo_stack.push(inverse);
                            self.status = "Undone.".into();
                            self.reload();
                        }
                        Err(error) => self.status = format!("Undo failed: {error}"),
                    }
                }
            }
            Command::Redo => {
                if let Some(action) = self.redo_stack.pop() {
                    match run_undo_action(action) {
                        Ok(inverse) => {
                            self.undo_stack.push(inverse);
                            self.status = "Redone.".into();
                            self.reload();
                        }
                        Err(error) => self.status = format!("Redo failed: {error}"),
                    }
                }
            }
            Command::Delete => {
                for path in &targets {
                    if let Err(error) = move_to_trash(path) {
                        self.status = format!("Could not move to trash: {error}");
                    } else {
                        self.record_undo(UndoAction::Restore(path.clone()));
                    }
                }
                self.reload();
            }
            Command::DeletePermanently => {
                if !targets.is_empty() {
                    self.pending_delete = Some(targets.clone());
                }
            }
            Command::OpenTerminal => {
                if !open_terminal(&here) {
                    self.status = "No terminal emulator could be started.".into();
                }
            }
            Command::Extract => self.extract_items(&targets, false),
            Command::ExtractHere => self.extract_items(&targets, true),
            Command::Compress => self.compress_items(&targets, CompressFormat::Xz, 6, None),
            Command::CompressAs(format, level) => {
                let password = std::mem::take(&mut self.state.compress_password);
                let password = (!password.is_empty()).then_some(password);
                self.compress_items(&targets, format, level, password.as_deref());
            }
            Command::NewShortcut => {
                let mut made = 0usize;
                for path in &targets {
                    let Some(name) = path.file_name() else {
                        continue;
                    };
                    let link = unique(&here.join(format!("{} - Link", name.to_string_lossy())));
                    match make_link(path, &link) {
                        Ok(()) => {
                            made += 1;
                            self.record_undo(UndoAction::Trash(link.clone()));
                        }
                        Err(error) => {
                            self.status = format!("Create link failed: {error}");
                            break;
                        }
                    }
                }
                if made > 0 {
                    self.status = format!("Created {made} link(s).");
                }
                self.reload();
            }
            Command::OpenNewWindow => {
                let start = targets
                    .iter()
                    .find(|path| path.is_dir())
                    .cloned()
                    .unwrap_or_else(|| here.clone());
                self.open_window(ctx, start);
            }
            other => {
                self.status = format!("{other:?} is not available in this host.");
            }
        }
    }
}

impl App {
    /// Everything that belongs to one window: reacting to changes of its folder, drawing it,
    /// handling its events and dialogs. The first window and every extra window run this
    /// with their own state swapped in.
    fn window_frame(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Settings are the single source of truth for the theme, so all windows agree.
        self.state.dark_mode = self.settings.dark_mode;
        let changed = self
            .watcher
            .as_mut()
            .is_some_and(|watcher| !watcher.poll().is_empty());
        if changed {
            if self.state.location.is_dir() {
                self.reload();
            } else if let Some(parent) = self
                .state
                .location
                .parent()
                .filter(|path| path.is_dir())
                .map(Path::to_path_buf)
            {
                self.status = "The displayed directory was removed; moved to its parent.".into();
                self.navigate(parent);
            } else {
                self.status = "The displayed directory was removed or became unavailable.".into();
            }
        }
        let events = self.explorer.show(ui, &mut self.state);
        for event in events {
            self.handle(ctx, event);
        }
        self.dest_dialog_ui(ctx);
        self.confirm_dialogs_ui(ctx);
        self.state.can_undo = !self.undo_stack.is_empty();
        self.state.can_redo = !self.redo_stack.is_empty();
        // Expire stale status messages.
        if self.status != self.status_seen {
            self.status_seen = self.status.clone();
            self.status_seen_at = Instant::now();
        } else if !self.status.is_empty() && self.status_seen_at.elapsed() >= STATUS_TTL {
            self.status.clear();
            self.status_seen.clear();
        }
        if !self.status.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.state.host_status = if self.status.is_empty() {
            None
        } else {
            Some(self.status.clone())
        };
    }

    /// Exchange the per-window fields of the app with those of `w`. Calling it twice restores
    /// everything.
    fn swap_window(&mut self, w: &mut ChildWindow) {
        std::mem::swap(&mut self.explorer, &mut w.explorer);
        std::mem::swap(&mut self.state, &mut w.state);
        std::mem::swap(&mut self.prev_location, &mut w.prev_location);
        std::mem::swap(&mut self.watcher, &mut w.watcher);
        std::mem::swap(&mut self.dest_dialog, &mut w.dest_dialog);
        std::mem::swap(&mut self.pending_transfer, &mut w.pending_transfer);
        std::mem::swap(&mut self.pending_delete, &mut w.pending_delete);
        std::mem::swap(&mut self.pending_extract, &mut w.pending_extract);
        std::mem::swap(&mut self.status, &mut w.status);
        std::mem::swap(&mut self.status_seen, &mut w.status_seen);
        std::mem::swap(&mut self.status_seen_at, &mut w.status_seen_at);
    }

    /// Open `start` in a new window of this process (no new process, no second set of caches).
    fn open_window(&mut self, ctx: &egui::Context, start: PathBuf) {
        if self.children.len() >= MAX_CHILD_WINDOWS {
            self.status = format!("At most {} windows can be open at once.", MAX_CHILD_WINDOWS + 1);
            return;
        }
        let start = if start.is_dir() {
            start
        } else {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        };
        let mut explorer = Explorer::new();
        explorer.options.title_bar = self.explorer.options.title_bar;
        let mut state = ExplorerState::new();
        state.subfolders_fn = Some(subfolders);
        load_settings_into_state(&mut state, &self.settings);
        let pins: Vec<PathBuf> = self.behavior.pins().cloned().collect();
        state.nav = build_nav(&pins);
        let mut entries = load_dir(&start, true);
        attach_cached_thumbnails(&mut entries, &self.textures, self.settings.thumbnails);
        state.set_location(start.clone(), entries);
        let mut watcher = fm_notify::DirWatcher::new({
            let wake_ctx = ctx.clone();
            move || wake_ctx.request_repaint()
        })
        .ok();
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.set_dir(Some(&start));
        }
        self.behavior.open_dir(&start, unix_now());
        self.apply_watch_plan();
        self.window_serial = self.window_serial.wrapping_add(1);
        self.children.push(ChildWindow {
            id: egui::ViewportId::from_hash_of(("file-manager-window", self.window_serial)),
            title: window_title(&start),
            close: false,
            fresh: true,
            explorer,
            state,
            prev_location: None,
            watcher,
            dest_dialog: None,
            pending_transfer: None,
            pending_delete: None,
            pending_extract: None,
            status: String::new(),
            status_seen: String::new(),
            status_seen_at: Instant::now(),
        });
        ctx.request_repaint();
    }

    /// Folders sent by other launches of the program become windows of this process.
    fn poll_open_requests(&mut self, ctx: &egui::Context) {
        let mut wanted = Vec::new();
        if let Some(rx) = self.open_requests.as_ref() {
            while let Ok(path) = rx.try_recv() {
                wanted.push(path);
            }
        }
        for path in wanted {
            self.open_window(ctx, path);
        }
    }

    /// Draw every extra window as an immediate viewport of this frame.
    fn child_windows(&mut self, ctx: &egui::Context) {
        if self.children.is_empty() {
            return;
        }
        let mut children = std::mem::take(&mut self.children);
        for child in &mut children {
            self.swap_window(child);
            let builder = egui::ViewportBuilder::default()
                .with_title(child.title.clone())
                .with_inner_size([1100.0, 680.0])
                .with_min_inner_size([640.0, 420.0]);
            ctx.show_viewport_immediate(child.id, builder, |ui, _class| {
                let window_ctx = ui.ctx().clone();
                if window_ctx.input(|i| i.viewport().close_requested()) {
                    child.close = true;
                    return;
                }
                if child.fresh {
                    child.fresh = false;
                    window_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                self.window_frame(ui, &window_ctx);
                let title = window_title(&self.state.location);
                if title != child.title {
                    window_ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                    child.title = title;
                }
            });
            self.swap_window(child);
        }
        // Windows opened while an extra window was being drawn went into `self.children`.
        let added = std::mem::take(&mut self.children);
        children.extend(added);
        children.retain(|child| !child.close);
        self.children = children;
    }
}

/// "<folder> - File Manager", as shown in an extra window's title bar.
fn window_title(location: &Path) -> String {
    let name = location
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| location.display().to_string());
    format!("{name} - File Manager")
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_lifecycle(&ctx);
        self.persist_zoom(&ctx);
        self.pump_tier_events();
        self.poll_workers(&ctx);
        self.prime_step(&ctx);
        self.poll_open_requests(&ctx);
        // Closing the first window must not take the other windows with it. While others are
        // open, only hide it; the process ends when the last window is closed.
        if !self.quitting
            && !self.root_hidden
            && !self.children.is_empty()
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.root_hidden = true;
            self.flush_state();
        }
        self.window_frame(ui, &ctx);
        self.sync_remembered_prefs();
        self.child_windows(&ctx);
        self.tick_persistence(&ctx);
        if self.root_hidden {
            if self.children.is_empty() {
                // Last window gone: really quit.
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                // Keep the (hidden) root ticking: it drives every extra window.
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flusher.request_shutdown();
        self.flush_state();
    }
}

fn attach_cached_thumbnails(
    entries: &mut [Entry],
    textures: &HashMap<PathBuf, CachedTexture>,
    enabled: bool,
) {
    for entry in entries {
        entry.thumbnail = if enabled {
            textures
                .get(&entry.path)
                .filter(|cached| {
                    cached.source_size == entry.size && cached.source_modified == entry.modified
                })
                .map(|cached| cached.handle.id())
        } else {
            None
        };
    }
}

fn load_settings_into_state(state: &mut ExplorerState, settings: &Settings) {
    state.view = match settings.view.mode {
        SavedViewMode::ExtraLargeIcons => egui_explorer::ViewMode::ExtraLargeIcons,
        SavedViewMode::LargeIcons => egui_explorer::ViewMode::LargeIcons,
        SavedViewMode::MediumIcons => egui_explorer::ViewMode::MediumIcons,
        SavedViewMode::SmallIcons => egui_explorer::ViewMode::SmallIcons,
        SavedViewMode::List => egui_explorer::ViewMode::List,
        SavedViewMode::Details => egui_explorer::ViewMode::Details,
        SavedViewMode::Tiles => egui_explorer::ViewMode::Tiles,
        SavedViewMode::Content => egui_explorer::ViewMode::Content,
    };
    state.sort_key = match settings.view.sort_key {
        SavedSortKey::Name => egui_explorer::SortKey::Name,
        SavedSortKey::DateModified => egui_explorer::SortKey::DateModified,
        SavedSortKey::Type => egui_explorer::SortKey::Type,
        SavedSortKey::Size => egui_explorer::SortKey::Size,
    };
    state.sort_order = match settings.view.sort_order {
        SavedSortOrder::Ascending => egui_explorer::SortOrder::Ascending,
        SavedSortOrder::Descending => egui_explorer::SortOrder::Descending,
    };
    state.group = match settings.view.group {
        SavedGroupKey::None => egui_explorer::GroupKey::None,
        SavedGroupKey::Name => egui_explorer::GroupKey::Name,
        SavedGroupKey::DateModified => egui_explorer::GroupKey::DateModified,
        SavedGroupKey::Type => egui_explorer::GroupKey::Type,
        SavedGroupKey::Size => egui_explorer::GroupKey::Size,
    };
    state.folders_first = settings.view.folders_first;
    state.show_hidden = settings.show.hidden_items;
    state.show_extensions = settings.show.file_name_extensions;
    state.item_check_boxes = settings.show.item_check_boxes;
    state.show_nav_pane = settings.show.nav_pane;
    state.show_preview_pane = settings.show.preview_pane;
    state.show_details_pane = settings.show.details_pane;
    state.thumbnails_enabled = settings.thumbnails;
    state.dark_mode = settings.dark_mode;
    state.indexing_enabled = settings.indexing_enabled;
    state.ribbon_mode = match settings.ribbon.mode {
        SavedRibbonMode::Expanded => RibbonMode::Expanded,
        SavedRibbonMode::Minimized => RibbonMode::Minimized,
        SavedRibbonMode::Hidden => RibbonMode::Hidden,
    };
    state.qat_below_ribbon = settings.ribbon.qat_below_ribbon;
    state.key_tips_enabled = settings.ribbon.key_tips;
    state.active_tab = settings.ribbon.active_tab.clone();
    state.columns = settings
        .view
        .columns
        .iter()
        .map(|column| egui_explorer::Column {
            key: match column.key {
                SavedSortKey::Name => egui_explorer::SortKey::Name,
                SavedSortKey::DateModified => egui_explorer::SortKey::DateModified,
                SavedSortKey::Type => egui_explorer::SortKey::Type,
                SavedSortKey::Size => egui_explorer::SortKey::Size,
            },
            width: column.width.clamp(36.0, 1600.0),
            visible: column.visible,
        })
        .collect();
}

// ---------------------------------------------------------------------------
// File-system helpers
// ---------------------------------------------------------------------------

fn load_dir(path: &Path, visit: bool) -> Vec<Entry> {
    let Ok(items) = tiers().read(path, visit) else {
        return Vec::new();
    };
    items.into_iter().map(entry_from_item).collect()
}

fn entry_from_item(item: fm_tiers::Item) -> Entry {
    let p = item.path;
    let mut is_dir = item.meta.is_some_and(|metadata| metadata.is_dir);
    // Listings do not follow symlinks, so a link to a folder arrives as a non-directory.
    // Resolve it here (one stat, only for links) so it shows as a folder and opens as one.
    if !is_dir && matches!(item.kind, fm_tiers::Kind::Symlink) {
        is_dir = fs::metadata(&p).is_ok_and(|target| target.is_dir());
    }
    let mut e = Entry::new(
        &p,
        if is_dir {
            EntryKind::Folder
        } else {
            EntryKind::File
        },
    );
    if let Some(metadata) = item.meta {
        e.size = if is_dir { 0 } else { metadata.size };
        e.modified = metadata.modified;
    }
    if !is_dir {
        e.type_name = fm_mime::guess(&p).type_name;
    }
    if is_dir {
        e.icon = special_folder_icon(&p);
    }
    e
}

/// Put restored behaviour (tier scores, user pins) into the tier manager. Called before
/// `seed_tiers`, because seeds never override restored state.
fn restore_tiers(behavior: &Behavior, now: u64) {
    let mut tiers = tiers();
    tiers
        .table_mut()
        .import_raw(behavior.tiers().iter().cloned(), now);
    for pin in behavior.pins() {
        tiers.pin(pin.clone());
    }
}

/// Seed the hot tier with the places a user returns to constantly, and pin the quick-access
/// folders. Restored behaviour (added later) takes precedence over these seeds.
fn seed_tiers() {
    let mut tiers = tiers();
    let xdg = [
        dirs::desktop_dir(),
        dirs::download_dir(),
        dirs::document_dir(),
        dirs::picture_dir(),
        dirs::audio_dir(),
        dirs::video_dir(),
    ];
    for dir in xdg.into_iter().flatten() {
        tiers.pin(dir.clone());
        tiers.seed([dir]);
    }
    if let Some(home) = dirs::home_dir() {
        tiers.seed(fm_tiers::Tiers::default_seed_paths(&home));
    }
}

fn special_folder_icon(p: &Path) -> Option<Icon> {
    let home = dirs::home_dir()?;
    let table = [
        (dirs::desktop_dir(), Icon::FolderDesktop),
        (dirs::download_dir(), Icon::FolderDownloads),
        (dirs::document_dir(), Icon::FolderDocuments),
        (dirs::picture_dir(), Icon::FolderPictures),
        (dirs::audio_dir(), Icon::FolderMusic),
        (dirs::video_dir(), Icon::FolderVideos),
        (Some(home), Icon::FolderHome),
    ];
    table
        .into_iter()
        .find(|(d, _)| d.as_deref() == Some(p))
        .map(|(_, i)| i)
}

fn subfolders(p: &Path) -> Vec<PathBuf> {
    tiers().subdirs(p).unwrap_or_default()
}

/// The built-in Quick access folders that exist.
fn quick_access_dirs() -> Vec<(PathBuf, &'static str, Icon)> {
    [
        (dirs::desktop_dir(), "Desktop", Icon::FolderDesktop),
        (dirs::download_dir(), "Downloads", Icon::FolderDownloads),
        (dirs::document_dir(), "Documents", Icon::FolderDocuments),
        (dirs::picture_dir(), "Pictures", Icon::FolderPictures),
        (dirs::audio_dir(), "Music", Icon::FolderMusic),
        (dirs::video_dir(), "Videos", Icon::FolderVideos),
    ]
    .into_iter()
    .filter_map(|(dir, label, icon)| dir.filter(|d| d.exists()).map(|d| (d, label, icon)))
    .collect()
}

/// Quick access children: the built-in folders followed by the user's pins.
fn quick_access_nodes(user_pins: &[PathBuf]) -> Vec<NavNode> {
    let mut quick: Vec<NavNode> = quick_access_dirs()
        .into_iter()
        .map(|(dir, label, icon)| NavNode::new(dir, label, icon).pinned())
        .collect();
    for pin in user_pins.iter().filter(|pin| pin.is_dir()) {
        let name = pin
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| pin.display().to_string());
        quick.push(NavNode::new(pin.clone(), name, Icon::Folder).pinned());
    }
    quick
}

fn build_nav(user_pins: &[PathBuf]) -> Vec<NavNode> {
    let quick = quick_access_nodes(user_pins);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = vec![
        NavNode::new(QUICK_ACCESS_ID, "Quick access", Icon::QuickAccess)
            .with_children(quick)
            .expanded(),
    ];
    roots.push(NavNode::new(home.clone(), "Home", Icon::FolderHome));
    let mut pc = vec![NavNode::new("/", "File System", Icon::Drive)];
    if let Ok(rd) = fs::read_dir("/mnt") {
        for d in rd.flatten().filter(|d| d.path().is_dir()) {
            pc.push(NavNode::new(
                d.path(),
                d.file_name().to_string_lossy().into_owned(),
                Icon::Removable,
            ));
        }
    }
    roots.push(
        NavNode::new("this-pc://", "This PC", Icon::ThisPc)
            .with_children(pc)
            .expanded(),
    );
    // Point at the same directory `move_to_trash()` actually uses, and make
    // sure it exists so the node is navigable even before anything has been
    // deleted. (Previously this pointed at "/", so clicking "Trash" opened
    // the filesystem root instead.)
    let trash = trash_dir();
    roots.push(NavNode::new(trash, "Trash", Icon::Trash));
    roots
}

/// An extraction waiting for the user to type a password.
struct PendingExtract {
    /// Archives still to extract; the first one is the one that needs the password.
    archives: Vec<PathBuf>,
    here: bool,
    password: String,
    /// The previous attempt was rejected, so say so in the dialog.
    wrong: bool,
}

/// Why an extraction did not finish.
enum ExtractError {
    NeedPassword,
    WrongPassword,
    Failed(String),
}

/// Archive each target next to itself with the real tar/zip/7z tools. Runs on a job thread.
fn compress_job(
    origin: PathBuf,
    targets: Vec<PathBuf>,
    format: CompressFormat,
    level: u8,
    password: Option<String>,
) -> JobOutcome {
    let mut outcome = JobOutcome {
        origin,
        ..JobOutcome::default()
    };
    let password = password.as_deref();
        let level = level.clamp(1, 9);
        let password = password.filter(|pw| !pw.is_empty() && format.supports_password());
        for path in &targets {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                continue;
            };
            let archive =
                unique(&parent.join(format!("{}.{}", name.to_string_lossy(), format.extension())));
            let mut legacy_zip_crypto = false;
            let mut cmd;
            match format {
                CompressFormat::Zip => match (password, seven_zip()) {
                    (Some(pw), Some(sz)) => {
                        cmd = Proc::new(sz);
                        cmd.arg("a")
                            .arg("-tzip")
                            .arg("-mem=AES256")
                            .arg(format!("-mx={level}"))
                            .arg(format!("-p{pw}"))
                            .arg("-bd")
                            .arg("--")
                            .arg(&archive)
                            .arg(name);
                    }
                    (Some(pw), None) => {
                        legacy_zip_crypto = true;
                        cmd = Proc::new("zip");
                        cmd.arg("-r")
                            .arg(format!("-{level}"))
                            .arg("-P")
                            .arg(pw)
                            .arg(&archive)
                            .arg(name);
                    }
                    (None, _) => {
                        cmd = Proc::new("zip");
                        cmd.arg("-r")
                            .arg(format!("-{level}"))
                            .arg(&archive)
                            .arg(name);
                    }
                },
                CompressFormat::SevenZip => {
                    let Some(sz) = seven_zip() else {
                        outcome.status = Some(
                            "7-Zip (7z / 7zz / 7za) is not installed, so 7z archives cannot be created."
                                .into(),
                        );
                        break;
                    };
                    cmd = Proc::new(sz);
                    cmd.arg("a").arg("-t7z").arg(format!("-mx={level}"));
                    if let Some(pw) = password {
                        // -mhe=on also encrypts the file names inside the archive.
                        cmd.arg(format!("-p{pw}")).arg("-mhe=on");
                    }
                    cmd.arg("-bd").arg("--").arg(&archive).arg(name);
                }
                CompressFormat::Tar => {
                    cmd = Proc::new("tar");
                    cmd.arg("-cf").arg(&archive).arg("-C").arg(parent).arg(name);
                }
                CompressFormat::Gzip
                | CompressFormat::Xz
                | CompressFormat::Zstd
                | CompressFormat::Bzip2 => {
                    let program = match format {
                        CompressFormat::Gzip => "gzip",
                        CompressFormat::Xz => "xz",
                        CompressFormat::Bzip2 => "bzip2",
                        _ => "zstd",
                    };
                    cmd = Proc::new("tar");
                    cmd.arg(format!("--use-compress-program={program} -{level}"))
                        .arg("-cf")
                        .arg(&archive)
                        .arg("-C")
                        .arg(parent)
                        .arg(name);
                }
            }
            cmd.current_dir(parent).stdin(std::process::Stdio::null());
            match cmd.output() {
                Ok(done) if done.status.success() => {
                    outcome.status = Some(if legacy_zip_crypto {
                        format!(
                            "Created {} (legacy ZipCrypto; install 7-Zip for AES-256)",
                            archive.display()
                        )
                    } else if password.is_some() {
                        format!("Created {} (password protected)", archive.display())
                    } else {
                        format!("Created {}", archive.display())
                    });
                    outcome.undo.push(UndoAction::Trash(archive.clone()));
                }
                Ok(done) => {
                    // `archive` did not exist before this run, so a partial file is ours.
                    let _ = fs::remove_file(&archive);
                    outcome.status = Some(format!("Archive failed: {}", tail_message(&done)));
                    break;
                }
                Err(error) => {
                    outcome.status = Some(format!("Archive failed: {error}"));
                    break;
                }
            }
        }
    outcome
}

fn extract_job(
    origin: PathBuf,
    mut queue: Vec<PathBuf>,
    here: bool,
    mut password: Option<String>,
) -> JobOutcome {
    let mut outcome = JobOutcome {
        origin,
        ..JobOutcome::default()
    };
    let mut done = 0usize;
    let mut message: Option<String> = None;
    while !queue.is_empty() {
        let archive = queue[0].clone();
        match extract_archive(&archive, here, password.as_deref()) {
            Ok(created) => {
                done += 1;
                if let Some(created) = created {
                    outcome.undo.push(UndoAction::Trash(created));
                }
                queue.remove(0);
                password = None;
            }
            Err(ExtractError::Failed(reason)) => {
                message = Some(format!("Extract failed: {reason}"));
                break;
            }
            Err(other) => {
                outcome.pending_extract = Some(PendingExtract {
                    archives: std::mem::take(&mut queue),
                    here,
                    password: String::new(),
                    wrong: matches!(other, ExtractError::WrongPassword),
                });
                break;
            }
        }
    }
    if let Some(message) = message {
        outcome.status = Some(message);
    } else if done > 0 {
        outcome.status = Some(format!("Extracted {done} archive(s)."));
    } else if outcome.pending_extract.is_some() {
        outcome.status = Some(String::new());
    }
    outcome
}

fn transfer_job(
    origin: PathBuf,
    paths: Vec<PathBuf>,
    dest: PathBuf,
    cut: bool,
    choice: ConflictChoice,
) -> JobOutcome {
    let mut outcome = JobOutcome {
        origin,
        ..JobOutcome::default()
    };
    let mut done = 0usize;
    for path in &paths {
        let Some(name) = path.file_name() else {
            continue;
        };
        let wanted = dest.join(name);
        let target = if choice == ConflictChoice::Replace && wanted.exists() && wanted != *path {
            // The replaced item goes to the Trash so it stays recoverable.
            if let Err(error) = move_to_trash(&wanted) {
                outcome.status = Some(format!(
                    "Could not replace '{}': {error}",
                    wanted.display()
                ));
                break;
            }
            wanted
        } else {
            unique(&wanted)
        };
        let result = if cut {
            fs::rename(path, &target).or_else(|_| {
                // Different filesystem: copy, then remove the source.
                copy_recursive(path, &target).and_then(|()| {
                    if path.is_dir() {
                        fs::remove_dir_all(path)
                    } else {
                        fs::remove_file(path)
                    }
                })
            })
        } else {
            copy_recursive(path, &target)
        };
        match result {
            Ok(()) => {
                done += 1;
                outcome.undo.push(if cut {
                    UndoAction::Rename {
                        from: target.clone(),
                        to: path.clone(),
                    }
                } else {
                    UndoAction::Trash(target.clone())
                });
            }
            Err(error) => {
                outcome.status = Some(format!(
                    "{} failed: {error}",
                    if cut { "Move" } else { "Copy" }
                ));
                break;
            }
        }
    }
    if done > 0 {
        outcome.status = Some(format!(
            "{} {done} item(s) to '{}'.",
            if cut { "Moved" } else { "Copied" },
            dest.display()
        ));
    }
    outcome
}

fn delete_job(origin: PathBuf, targets: Vec<PathBuf>, in_trash: bool) -> JobOutcome {
    let mut outcome = JobOutcome {
        origin,
        status: Some(String::new()),
        ..JobOutcome::default()
    };
    if in_trash {
        // Purge through the trash crate so the .trashinfo records go away too.
        if let Ok(items) = fm_trash::list() {
            for path in &targets {
                if let Some(item) = items.iter().find(|i| i.trashed_path() == *path) {
                    if let Err(error) = item.purge() {
                        outcome.status = Some(format!("Permanent delete failed: {error}"));
                    }
                }
            }
        }
        return outcome;
    }
    for path in &targets {
        let result = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(error) = result {
            outcome.status = Some(format!("Permanent delete failed: {error}"));
            break;
        }
    }
    outcome
}

fn walk_search(root: &Path, needle: &str) -> (Vec<Entry>, bool) {
        let mut found: Vec<Entry> = Vec::new();
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        let mut truncated = false;
        'walk: while let Some(dir) = stack.pop() {
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for item in read.flatten() {
                let path = item.path();
                let Ok(file_type) = item.file_type() else {
                    continue;
                };
                let is_dir = file_type.is_dir();
                // A link to a folder is listed (and opens) as a folder, but is never walked
                // into, so link loops still cannot occur.
                let shown_as_dir = is_dir || (file_type.is_symlink() && path.is_dir());
                let name = item.file_name().to_string_lossy().to_lowercase();
                if name.contains(needle) {
                    let mut e = Entry::new(
                        &path,
                        if shown_as_dir {
                            EntryKind::Folder
                        } else {
                            EntryKind::File
                        },
                    );
                    if let Ok(metadata) = item.metadata() {
                        if !shown_as_dir {
                            e.size = metadata.len();
                        }
                        e.modified = metadata.modified().ok();
                    }
                    if !shown_as_dir {
                        e.type_name = fm_mime::guess(&path).type_name;
                    }
                    found.push(e);
                    if found.len() >= SEARCH_LIMIT {
                        truncated = true;
                        break 'walk;
                    }
                }
                // `file_type()` does not follow symlinks, so link loops cannot occur.
                if is_dir {
                    stack.push(path);
                }
            }
        }
    (found, truncated)
}

fn tool_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
    })
}

/// The 7-Zip command line tool, whichever way it is packaged.
fn seven_zip() -> Option<&'static str> {
    ["7zz", "7z", "7za"].into_iter().find(|name| tool_exists(name))
}

fn output_text(out: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    )
}

/// One short line explaining why a tool failed.
fn tail_message(out: &std::process::Output) -> String {
    let text = output_text(out);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.to_ascii_lowercase().starts_with("error"))
        .or_else(|| text.lines().map(str::trim).rev().find(|l| !l.is_empty()))
        .unwrap_or("unknown error");
    let short: String = line.chars().take(200).collect();
    short
}

/// Whether a failed extraction failed because of a missing or wrong password.
fn password_problem(out: &std::process::Output) -> bool {
    // unzip: exit code 82 = every file was skipped because of bad decryption passwords.
    if out.status.code() == Some(82) {
        return true;
    }
    let text = output_text(out).to_ascii_lowercase();
    [
        "wrong password",
        "incorrect password",
        "enter password",
        "unable to get password",
        "bad password",
        "encrypted",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Extract one archive. Returns the path that was newly created (a folder or a single
/// decompressed file, so it can be undone), or `None` when extracting straight into the
/// archive's own folder.
fn extract_archive(
    archive: &Path,
    here: bool,
    password: Option<&str>,
) -> Result<Option<PathBuf>, ExtractError> {
    let fail = |message: String| ExtractError::Failed(message);
    let parent = archive
        .parent()
        .ok_or_else(|| fail("the archive has no parent folder".into()))?;
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let stem = egui_explorer::archive_stem(&name)
        .unwrap_or(&name)
        .to_owned();

    // A lone compressed file (file.txt.gz), as opposed to a tar archive.
    let single = [(".gz", "gzip"), (".xz", "xz"), (".bz2", "bzip2"), (".zst", "zstd")]
        .into_iter()
        .find(|&(ext, _)| lower.ends_with(ext) && !lower.ends_with(&format!(".tar{ext}")));
    if let Some((_, program)) = single {
        if !tool_exists(program) {
            return Err(fail(format!("{program} is not installed")));
        }
        let target = unique(&parent.join(&stem));
        let file = fs::File::create(&target).map_err(|e| fail(e.to_string()))?;
        let out = Proc::new(program)
            .arg("-dc")
            .arg(archive)
            .stdin(std::process::Stdio::null())
            .stdout(file)
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| fail(format!("could not start {program}: {e}")))?;
        if out.status.success() {
            return Ok(Some(target));
        }
        let _ = fs::remove_file(&target);
        return Err(fail(tail_message(&out)));
    }

    let dest = if here {
        parent.to_path_buf()
    } else {
        unique(&parent.join(&stem))
    };
    if !here {
        fs::create_dir_all(&dest)
            .map_err(|e| fail(format!("cannot create {}: {e}", dest.display())))?;
    }
    let result = run_extract_tool(archive, &lower, &dest, password);
    if result.is_err() && !here {
        // Only removes the folder when nothing was extracted into it.
        let _ = fs::remove_dir(&dest);
    }
    result.map(|()| if here { None } else { Some(dest) })
}

fn run_extract_tool(
    archive: &Path,
    lower: &str,
    dest: &Path,
    password: Option<&str>,
) -> Result<(), ExtractError> {
    let is_tar = [
        ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz2", ".tbz", ".tar.zst",
        ".tzst",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix));
    // Exit code 1 means "finished with warnings" for unzip and 7-Zip.
    let mut warnings_ok = false;
    let mut cmd;
    if is_tar {
        cmd = Proc::new("tar");
        cmd.arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .arg("--skip-old-files");
    } else if lower.ends_with(".zip") && seven_zip().is_none() {
        warnings_ok = true;
        cmd = Proc::new("unzip");
        // No -q: unzip only reports "incorrect password" when it is not quiet.
        cmd.arg("-n");
        if let Some(pw) = password {
            cmd.arg("-P").arg(pw);
        }
        cmd.arg(archive).arg("-d").arg(dest);
    } else if let Some(sz) = seven_zip() {
        warnings_ok = true;
        cmd = Proc::new(sz);
        cmd.arg("x")
            .arg("-y")
            .arg("-aou")
            .arg("-bd")
            .arg(format!("-o{}", dest.to_string_lossy()));
        if let Some(pw) = password {
            cmd.arg(format!("-p{pw}"));
        }
        cmd.arg("--").arg(archive);
    } else if lower.ends_with(".rar") && tool_exists("unrar") {
        cmd = Proc::new("unrar");
        cmd.arg("x").arg("-y").arg("-o-");
        match password {
            Some(pw) => cmd.arg(format!("-p{pw}")),
            None => cmd.arg("-p-"),
        };
        cmd.arg(archive).arg(format!("{}/", dest.to_string_lossy()));
    } else {
        return Err(ExtractError::Failed(
            "7-Zip (7z / 7zz / 7za) is needed for this archive type but is not installed".into(),
        ));
    }
    cmd.stdin(std::process::Stdio::null());
    let out = cmd
        .output()
        .map_err(|e| ExtractError::Failed(format!("could not start the extraction tool: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    // Checked before the "warnings" exit code: unzip exits with 1 for a wrong password too.
    if !is_tar && password_problem(&out) {
        return Err(if password.is_some() {
            ExtractError::WrongPassword
        } else {
            ExtractError::NeedPassword
        });
    }
    if warnings_ok && out.status.code() == Some(1) {
        return Ok(());
    }
    Err(ExtractError::Failed(tail_message(&out)))
}

/// Read a zoom factor from the text of `zoom.txt` (`zoom = 1.5`, or just `1.5`).
fn parse_zoom(text: &str) -> Option<f32> {
    let value = text
        .trim()
        .trim_start_matches("zoom")
        .trim_start()
        .trim_start_matches('=')
        .trim()
        .parse::<f32>()
        .ok()?;
    value.is_finite().then(|| value.clamp(0.25, 5.0))
}

/// Write `zoom.txt` atomically (a half-written file must never be read back).
fn write_zoom(path: &Path, zoom: f32) {
    let tmp = path.with_extension("txt.tmp");
    if fs::write(&tmp, format!("zoom = {zoom:.3}\n")).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn unique(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| p.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

/// Copy `from` to `to`, recursing into subdirectories.
///
/// Rejects the case where `to` lies inside `from` before doing any work: if
/// a directory were copied into one of its own descendants, the destination
/// just created would itself be scanned by the recursion that creates it,
/// growing without bound until something (disk space, path length, open file
/// handles) gives out.
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() && destination_inside_source(from, to) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "cannot copy '{}' into itself (destination '{}' is inside it)",
                from.display(),
                to.display()
            ),
        ));
    }
    copy_recursive_unchecked(from, to)
}

fn copy_recursive_unchecked(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_recursive_unchecked(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// True if `to` is `from` itself or lies somewhere underneath it.
///
/// `to` usually doesn't exist yet (it's about to be created), so this
/// canonicalizes `from` and the nearest existing ancestor of `to`, then
/// compares. This also catches the case where a symlink makes `from` and an
/// ancestor of `to` the same directory via different paths.
fn destination_inside_source(from: &Path, to: &Path) -> bool {
    let Ok(from_c) = fs::canonicalize(from) else {
        return false;
    };
    let to_c = canonicalize_existing_prefix(to);
    to_c == from_c || to_c.starts_with(&from_c)
}

/// Canonicalize the nearest existing ancestor of `p` and re-append whatever
/// trailing components don't exist yet, so a not-yet-created destination can
/// still be compared against a canonicalized source.
fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
    let mut trailing = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(base) = fs::canonicalize(cur) {
            return trailing.iter().rev().fold(base, |acc, c| acc.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                trailing.push(name);
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(unix)]
fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_link(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "links are not supported on this platform",
    ))
}

/// `file://` URL with percent-encoding of everything outside the unreserved set.
fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Open a file in `$VISUAL` / `$EDITOR`, then common GUI editors, then `xdg-open`.
/// Spawns without blocking; returns whether anything started.
fn open_in_editor(path: &Path) -> bool {
    let mut candidates: Vec<String> = Vec::new();
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                candidates.push(value);
            }
        }
    }
    for name in [
        "gedit", "kate", "xed", "mousepad", "pluma", "code", "xdg-open",
    ] {
        candidates.push(name.to_string());
    }
    for candidate in candidates {
        let mut parts = candidate.split_whitespace();
        let Some(program) = parts.next() else {
            continue;
        };
        let mut command = Proc::new(program);
        command.args(parts).arg(path);
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}

fn trash_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Trash/files")
}

fn move_to_trash(path: &Path) -> Result<(), String> {
    fm_trash::trash(path)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Start a terminal emulator in `dir`. Honours `$TERMINAL`, then tries
/// `x-terminal-emulator` and the common emulators in turn (not every host has the
/// Debian alternatives symlink; a plain `xterm` install is enough).
fn open_terminal(dir: &Path) -> bool {
    // (program, args). The directory is also set via `current_dir`, but some emulators run
    // inside a server process that ignores it, so those get an explicit flag as well.
    let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(term) = std::env::var("TERMINAL") {
        let mut parts = term.split_whitespace();
        if let Some(program) = parts.next() {
            candidates.push((program.to_string(), parts.map(str::to_string).collect()));
        }
    }
    let wd = dir.display().to_string();
    for (name, args) in [
        ("x-terminal-emulator", vec![]),
        ("gnome-terminal", vec![format!("--working-directory={wd}")]),
        ("konsole", vec!["--workdir".to_string(), wd.clone()]),
        ("xfce4-terminal", vec![format!("--working-directory={wd}")]),
        ("mate-terminal", vec![format!("--working-directory={wd}")]),
        ("tilix", vec![format!("--working-directory={wd}")]),
        ("lxterminal", vec![format!("--working-directory={wd}")]),
        (
            "alacritty",
            vec!["--working-directory".to_string(), wd.clone()],
        ),
        ("kitty", vec!["--directory".to_string(), wd.clone()]),
        ("foot", vec![format!("--working-directory={wd}")]),
        ("xterm", vec![]),
        ("uxterm", vec![]),
    ] {
        candidates.push((name.to_string(), args));
    }
    for (program, args) in candidates {
        let mut command = Proc::new(&program);
        command.args(&args).current_dir(dir);
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}

/// Start a detached helper process (opener, terminal) without leaving a zombie.
///
/// Dropping a `Child` without `wait()` leaves a defunct process behind on Unix
/// once it exits. A short-lived thread reaps it, so the UI never blocks and
/// nothing lingers; a failed spawn (tool not installed) is simply ignored.
fn spawn_reaped(cmd: &mut Proc) {
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
