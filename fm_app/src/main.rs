//! Native host for the reusable egui Explorer UI.
//! Filesystem traversal and thumbnails stay on bounded worker threads. Listings, hot-folder
//! search and learned behaviour live in RAM; only a tiny, hash-skipped state file and the
//! cost-selected thumbnails are ever written, and only rarely.

mod open_with;
mod single_instance;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::{ColorImage, TextureHandle, TextureOptions};
use egui_explorer::{
    Command, CompressFormat, Entry, EntryKind, Event, Explorer, ExplorerState, Icon, NavNode,
    PermMode, RibbonMode, THIS_PC_LOCATION, dialogs,
};
use fm_behav::Behavior;
use fm_persist::{AtomicFile, FlushPolicy, Flusher, Outcome, WriteCounter};
use fm_settings::{
    FolderView, GroupKey as SavedGroupKey, RibbonMode as SavedRibbonMode, Settings,
    SortKey as SavedSortKey, SortOrder as SavedSortOrder, ViewMode as SavedViewMode,
};
use fm_thumbs::{Thumbnail, ThumbnailAudit, ThumbnailGenerationReport, ThumbnailStore};

const THUMBNAIL_WORKERS: usize = 2;
const THUMBNAIL_QUEUE_CAPACITY: usize = 256;
/// Limit completion handling per UI frame so a burst of worker messages cannot monopolize it.
const WORKER_MESSAGES_PER_FRAME: usize = 16;
/// Bound filesystem-event batches applied while holding the hot-tier lock per UI frame.
const TIER_EVENT_BATCHES_PER_FRAME: usize = 16;
const SEARCH_LIMIT: usize = 500;
/// Paths gathered before ranking and cutting to `SEARCH_LIMIT`.
const RAW_SEARCH_LIMIT: usize = 5000;
/// Folder names whose contents are never searched (caches and clutter with random names).
const SEARCH_SKIP_NAMES: &[&str] = &[
    ".cache",
    "__pycache__",
    "node_modules",
    ".git",
    "GPUCache",
    "Code Cache",
    "ShaderCache",
    "CachedData",
    ".thumbnails",
];
/// System folders that hold no user files.
const SEARCH_SKIP_SYSTEM: &[&str] = &["/proc", "/sys", "/dev", "/run"];
/// Home-relative folders that are always skipped.
const SEARCH_SKIP_HOME: &[&str] = &[".local/share/Trash"];
/// Home-relative application data, skipped unless "Search app data" is on. `.config` and
/// `.local` as a whole stay searchable; only the bulky per-application stores are listed.
const SEARCH_SKIP_APP_DATA: &[&str] = &[
    ".var",
    ".steam",
    ".local/share/Steam",
    ".local/share/flatpak",
    ".local/share/containers",
    ".wine",
    ".mozilla",
    ".thunderbird",
    ".npm",
    ".gradle",
    ".m2",
    ".rustup",
    ".cargo/registry",
    ".cargo/git",
    ".docker",
    "snap",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/Code",
    ".config/discord",
    ".config/Slack",
];
/// Shell snippet run in a terminal by "Build index" (kept free of quotes for every emulator).
const UPDATEDB_SCRIPT: &str = "sudo updatedb; echo; echo Press Enter to close; read x";
/// Time budget per frame for loading hot folders into RAM after startup.
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
            // On Unix, rename replaces an existing destination. Undo must never silently
            // destroy an item created at the original path since the operation was recorded.
            if from != to && path_entry_exists(&to) {
                return Err(format!(
                    "Cannot restore '{}' because '{}' already exists.",
                    from.display(),
                    to.display()
                ));
            }
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

struct OpenWithDialog {
    paths: Vec<PathBuf>,
    mime: String,
    /// Every installed application; the matching ones are filtered from it each frame.
    apps: Vec<open_with::AppEntry>,
    show_all: bool,
    selected: Option<String>,
    custom: String,
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
/// capture state. The mutex also serializes first-window background scans with UI operations.
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

enum HyprlandPlacementEvent {
    /// The move succeeded; retain the intended workspace even if the subsequent focus command
    /// hangs.
    MovedToTarget(u32),
    /// The one-shot focus handoff finished (or could not move the window).
    Finished { serial: u32, moved_to_target: bool },
}

enum WorkerMessage {
    /// Initial directory listing for a newly opened child window. The viewport is created before
    /// filesystem traversal completes, so a slow directory cannot delay window appearance.
    WindowLoaded {
        serial: u32,
        path: PathBuf,
        entries: Vec<Entry>,
    },
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
    /// A `udisksctl mount`/`unmount` finished on its thread.
    DriveOp { ok: bool, message: String },
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

/// Drive-scan interval: 0 means never (no polling); anything else is clamped to 5..=20 s.
fn normalize_poll_secs(secs: u32) -> u32 {
    if secs == 0 { 0 } else { secs.clamp(5, 20) }
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
            single_instance::Acquire::Exit => return Ok(()),
            single_instance::Acquire::Primary(listener) => Some(listener),
            single_instance::Acquire::Unavailable | single_instance::Acquire::Fallback => None,
        }
    };
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 720.0])
        .with_min_inner_size([640.0, 420.0])
        .with_title("File Manager");
    let options = eframe::NativeOptions {
        viewport,
        // This app enables Glow, not WGPU. Select it explicitly so the VSync setting below is
        // guaranteed to configure the renderer this build actually uses.
        renderer: eframe::Renderer::Glow,
        // eframe 0.36+ keeps VSync in the Glow backend configuration. DontWait avoids blocking
        // the shared event loop on a compositor frame for an inactive workspace.
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
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
    /// Stable serial used to match compositor-placement completions to this child.
    serial: u32,
    /// Actual user-facing title. While Hyprland placement is pending, `launch_title` is the
    /// temporary unique title used to move/focus only this window without matching siblings.
    title: String,
    launch_title: Option<String>,
    /// Start time for the one-shot compositor handoff; a UI-side timeout restores normal focus.
    launch_started_at: Option<Instant>,
    /// True after Hyprland accepted the move, so a later focus timeout won't steal it back.
    launch_moved_to_target: bool,
    /// Set once the window asked to be closed; it is dropped at the end of the frame.
    close: bool,
    /// Cancelled IPC requests must not materialize after their launcher falls back standalone.
    request_active: Arc<std::sync::atomic::AtomicBool>,
    /// Acknowledges the request only once this native viewport's callback actually runs.
    opened_ack: Option<mpsc::Sender<bool>>,
    /// Raise the window on its first frame.
    fresh: bool,
    explorer: Explorer,
    state: ExplorerState,
    prev_location: Option<PathBuf>,
    watcher: Option<fm_notify::DirWatcher>,
    dest_dialog: Option<DestDialog>,
    open_with_dialog: Option<OpenWithDialog>,
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

/// Thumbnail memory budgets, loaded from a small `memory.toml` kept beside the main
/// `settings.toml`. These bound how many decoded thumbnails stay resident (as GPU textures
/// for on-screen icons, and as a smaller plain-RAM cache behind them for scroll-back) rather
/// than how big or sharp any individual thumbnail is, so changing them changes RAM usage
/// only -- never thumbnail quality or on-screen appearance.
///
/// The file is plain `key = value` lines (not full TOML), read fresh every time the app
/// starts, so a person can lower it for a lighter footprint or raise it for less re-decoding
/// on a machine with RAM to spare, without rebuilding anything.
struct MemoryBudgets {
    /// Max thumbnails kept as live GPU textures at once (on-screen icons + scroll-back).
    texture_handle_limit: usize,
    /// Byte ceiling for that same GPU texture cache.
    texture_cache_budget_bytes: usize,
    /// Decoded thumbnails kept in plain process RAM (CPU side, behind the GPU cache) so a
    /// re-request after eviction doesn't need a fresh disk read + decode.
    ram_cache_budget_bytes: usize,
}

impl MemoryBudgets {
    const DEFAULT_TEXTURE_HANDLE_LIMIT: usize = 96;
    const DEFAULT_TEXTURE_CACHE_BUDGET_MB: usize = 32;
    const DEFAULT_RAM_CACHE_BUDGET_MB: usize = 16;
    const FILE_NAME: &'static str = "memory.toml";

    fn defaults() -> Self {
        Self {
            texture_handle_limit: Self::DEFAULT_TEXTURE_HANDLE_LIMIT,
            texture_cache_budget_bytes: Self::DEFAULT_TEXTURE_CACHE_BUDGET_MB * 1024 * 1024,
            ram_cache_budget_bytes: Self::DEFAULT_RAM_CACHE_BUDGET_MB * 1024 * 1024,
        }
    }

    /// Reads `memory.toml` next to the main config file. The first time it is missing, it is
    /// created with the defaults below (and comments explaining each value) so there is
    /// always a file to edit; after that, edits here are picked up on the next launch.
    /// Any read or parse problem falls back to the defaults rather than failing to start.
    fn load_or_create(config_dir: &Path) -> Self {
        let path = config_dir.join(Self::FILE_NAME);
        let defaults = Self::defaults();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                let _ = fs::create_dir_all(config_dir);
                let _ = fs::write(&path, defaults.to_file_text());
                return defaults;
            }
        };
        let mut budgets = defaults;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<usize>() else {
                continue;
            };
            // A floor on each value keeps a typo (e.g. "0") from leaving the app with no
            // thumbnail cache at all rather than just a small one.
            match key.trim() {
                "texture_handle_limit" => budgets.texture_handle_limit = value.max(8),
                "texture_cache_budget_mb" => {
                    budgets.texture_cache_budget_bytes = value.max(4).saturating_mul(1024 * 1024)
                }
                "ram_cache_budget_mb" => {
                    budgets.ram_cache_budget_bytes = value.max(4).saturating_mul(1024 * 1024)
                }
                _ => {}
            }
        }
        budgets
    }

    fn to_file_text(&self) -> String {
        format!(
            "# Thumbnail memory budgets for the file manager.\n\
             # Edit a value and restart the app to apply it -- no rebuild needed.\n\
             # These only control how many decoded thumbnails stay cached in memory; they do\n\
             # not change thumbnail resolution, sharpness, or how the app looks.\n\
             \n\
             # Max thumbnails kept as GPU textures at once (on-screen icons + scroll-back\n\
             # buffer). Lower = less RAM, more re-decoding when you scroll back up.\n\
             texture_handle_limit = {}\n\
             \n\
             # Byte ceiling for that same GPU texture cache, in MB.\n\
             texture_cache_budget_mb = {}\n\
             \n\
             # Decoded thumbnails also kept in plain RAM (behind the GPU cache), in MB.\n\
             ram_cache_budget_mb = {}\n",
            self.texture_handle_limit,
            self.texture_cache_budget_bytes / (1024 * 1024),
            self.ram_cache_budget_bytes / (1024 * 1024),
        )
    }
}

struct AppCore {
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
    open_with_dialog: Option<OpenWithDialog>,
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
    /// Loaded once at startup from `memory.toml`; see [`MemoryBudgets`].
    texture_handle_limit: usize,
    texture_cache_budget_bytes: usize,
    maintenance_running: bool,
    /// Hot folders are still being loaded into RAM (a few milliseconds per frame).
    behavior: Behavior,
    state_file: AtomicFile,
    flusher: Flusher,
    write_counter: WriteCounter,
    /// SIGTERM/SIGHUP/SIGINT and logind suspend/shutdown hooks.
    lifecycle: fm_lifecycle::Lifecycle,
    last_fold: Instant,
    thumbs_pending_seen: usize,
    last_drive_poll: Instant,
    /// Entry path (mountpoint, or `device://<node>` while unmounted) -> backing block
    /// device, refreshed every `tick_drive_poll`. Used to resolve Mount/Unmount targets.
    drive_devices: std::collections::HashMap<PathBuf, String>,
    /// The folder shown before the current one, used to tell "back" from "open".
    prev_location: Option<PathBuf>,
    watcher: Option<fm_notify::DirWatcher>,
    /// One inotify instance for every hot directory (the on-screen folder keeps `watcher`).
    tier_watcher: Option<(fm_notify::Watcher, Receiver<Vec<fm_notify::Event>>)>,
    /// Extra windows of this process.
    children: Vec<ChildWindow>,
    window_serial: u32,
    /// Folders other launches of the program asked this instance to open in a new window.
    open_requests: Option<Receiver<single_instance::OpenRequest>>,
    /// One-shot completion channel for Hyprland's workspace/focus handoff. It is idle except
    /// immediately after a window is opened from another workspace.
    hyprland_placement_tx: mpsc::Sender<HyprlandPlacementEvent>,
    hyprland_placements: Receiver<HyprlandPlacementEvent>,
    /// The first window was closed while other windows were open: it is only hidden, because
    /// closing egui's root window would end the whole process.
    root_hidden: bool,
    /// The program is really shutting down (SIGTERM, logout, last window gone).
    quitting: bool,
    /// `(location, view)` for the folder currently on screen, as last applied or saved.
    /// Compared every frame against the live view/sort/group so a manual change made while
    /// browsing a real folder is written to `settings.per_folder` for that folder only.
    /// `None` while on a virtual location (This PC, search results) that has no folder to
    /// remember a view for.
    folder_view_active: Option<(PathBuf, FolderView)>,
}

/// eframe's root adapter. Deferred viewport callbacks capture the same shared application core,
/// so all windows live in one process and reuse the same caches and worker pools.
struct App {
    shared: Arc<Mutex<AppCore>>,
}

impl App {
    fn new(
        frameless: bool,
        ctx: &egui::Context,
        cli_start: Option<PathBuf>,
        listener: Option<single_instance::Listener>,
    ) -> Self {
        Self {
            shared: Arc::new(Mutex::new(AppCore::new(
                frameless, ctx, cli_start, listener,
            ))),
        }
    }
}

impl AppCore {
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

        // Read beside settings.toml every launch; see `MemoryBudgets` for the file format.
        let memory_budgets = MemoryBudgets::load_or_create(config_dir);

        let thumbnail_cache = fm_thumbs::default_cache_dir("file-manager")
            .unwrap_or_else(|| config_dir.join("cache/thumbnails"));
        // One counter for every byte this app writes (state file and thumbnail cache).
        let write_counter = WriteCounter::new();
        let thumbnail_store = Arc::new(
            ThumbnailStore::new(thumbnail_cache, settings.thumbnail_edge)
                .with_write_counter(write_counter.clone())
                .with_ram_budget_bytes(memory_budgets.ram_cache_budget_bytes),
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
        state.nav = build_nav(&user_pins, settings.nav.this_pc_expanded);
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
        if let Some(fv) = settings.folder_view(&state.location).cloned() {
            apply_folder_view(&mut state, &fv);
        }
        let folder_view_active = Some((state.location.clone(), folder_view_from_state(&state)));
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
        if let Some(watcher) = watcher.as_mut()
            && let Err(error) = watcher.set_dir(Some(&state.location))
        {
            eprintln!(
                "Could not watch {} for changes: {error}",
                state.location.display()
            );
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

        let (hyprland_placement_tx, hyprland_placements) = mpsc::channel();
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
            open_with_dialog: None,
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
            texture_handle_limit: memory_budgets.texture_handle_limit,
            texture_cache_budget_bytes: memory_budgets.texture_cache_budget_bytes,
            maintenance_running: false,
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
            // Poll immediately on first tick rather than waiting out a full interval
            // (drive_poll_secs is clamped to at most 20s, so 60s back is always enough).
            last_drive_poll: Instant::now() - Duration::from_secs(60),
            drive_devices: std::collections::HashMap::new(),
            prev_location: None,
            watcher,
            tier_watcher,
            children: Vec::new(),
            window_serial: 0,
            open_requests: listener.map(|listener| listener.serve(ctx.clone())),
            hyprland_placement_tx,
            hyprland_placements,
            root_hidden: false,
            quitting: false,
            folder_view_active,
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
        if let Some((loc, baseline)) = self.folder_view_active.clone() {
            // Only while the folder it was captured for is still the one on screen: a
            // navigation in between already re-seeds the baseline for the new folder.
            if loc == self.state.location {
                let current = folder_view_from_state(&self.state);
                if current != baseline {
                    self.settings.set_folder_view(&loc, current.clone());
                    self.folder_view_active = Some((loc, current));
                    changed = true;
                }
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

    /// Give each regular file the chosen access level (owner bits; see
    /// `PermMode::apply_to_mode`). Folders are skipped on purpose.
    fn set_permissions(&mut self, targets: Vec<PathBuf>, mode: PermMode) {
        use std::os::unix::fs::PermissionsExt;
        let (mut changed, mut failed, mut skipped) = (0usize, 0usize, 0usize);
        let mut first_error: Option<String> = None;
        for path in &targets {
            let md = match fs::metadata(path) {
                Ok(md) if md.is_file() => md,
                Ok(_) => {
                    skipped += 1;
                    continue;
                }
                Err(error) => {
                    failed += 1;
                    first_error.get_or_insert(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let new_mode = mode.apply_to_mode(md.permissions().mode());
            match fs::set_permissions(path, fs::Permissions::from_mode(new_mode)) {
                Ok(()) => changed += 1,
                Err(error) => {
                    failed += 1;
                    first_error.get_or_insert(format!("{}: {error}", path.display()));
                }
            }
        }
        self.status = if failed > 0 {
            format!(
                "Permissions: {changed} changed, {failed} failed ({}).",
                first_error.unwrap_or_default()
            )
        } else if skipped > 0 {
            format!(
                "Permissions set to \"{}\" on {changed} file(s); {skipped} skipped.",
                mode.label()
            )
        } else {
            format!(
                "Permissions set to \"{}\" on {changed} file(s).",
                mode.label()
            )
        };
    }

    fn navigate(&mut self, path: PathBuf) {
        // Synthetic nav-tree anchors are not real folders; redirect them.
        let path = match path.to_str() {
            Some(QUICK_ACCESS_ID) => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            _ => path,
        };
        self.status.clear();
        if path
            .to_str()
            .is_some_and(|s| s.starts_with(UNMOUNTED_DRIVE_PREFIX))
        {
            self.status = "This drive isn't mounted yet. Right-click it and choose Mount.".into();
            return;
        }
        if path == Path::new(THIS_PC_LOCATION) {
            // Virtual drive overview: list the drives instead of opening the filesystem root.
            let refresh = path == self.state.location;
            if !refresh {
                self.prev_location = Some(self.state.location.clone());
            }
            self.state.set_location(path.clone(), this_pc_entries());
            self.thumbnails_failed.clear();
            if let Some(watcher) = self.watcher.as_mut() {
                let _ = watcher.set_dir(None);
            }
            self.folder_view_active = None;
            return;
        }
        if path == trash_dir()
            && !path.exists()
            && let Err(error) = fs::create_dir_all(&path)
        {
            self.status = format!("Could not initialize Trash: {error}");
            return;
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
                let came_from = self.prev_location.replace(self.state.location.clone());
                if came_from.as_deref() == Some(path.as_path()) {
                    self.behavior.back(&path, unix_now());
                } else {
                    self.behavior.open_dir(&path, unix_now());
                }
            }
            self.state.set_location(path.clone(), entries);
            if let Some(fv) = self.settings.folder_view(&path).cloned() {
                apply_folder_view(&mut self.state, &fv);
            }
            self.folder_view_active = Some((path.clone(), folder_view_from_state(&self.state)));
            self.thumbnails_failed.clear();
            if let Some(watcher) = self.watcher.as_mut()
                && let Err(error) = watcher.set_dir(Some(&path))
            {
                self.status = format!("Could not watch {} for changes: {error}", path.display());
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
        // Avoid collecting every queued batch into a temporary Vec: a burst from a large
        // filesystem operation should not create a second, unbounded copy of the event queue.
        let Ok(first_batch) = rx.try_recv() else {
            return;
        };
        let mut tiers = tiers();
        let mut processed_batches = 1usize;
        for event in first_batch {
            apply_tier_event(&mut tiers, &event);
        }
        while processed_batches < TIER_EVENT_BATCHES_PER_FRAME {
            let Ok(batch) = rx.try_recv() else {
                break;
            };
            processed_batches += 1;
            for event in batch {
                apply_tier_event(&mut tiers, &event);
            }
        }
        if processed_batches == TIER_EVENT_BATCHES_PER_FRAME {
            // Remaining batches are applied next frame; do not let a sustained event burst
            // monopolize the tier lock or prevent the UI from repainting.
            self.egui_ctx.request_repaint();
        }
    }

    /// Make the set of inotify watches match the hot tier.
    fn apply_watch_plan(&mut self) {
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

    /// Rescan block devices (`lsblk`) every `settings.nav.drive_poll_secs` so a drive
    /// that was plugged in or removed shows up in This PC without the user doing
    /// anything; takes 5 to 20 seconds to notice, per the configured interval.
    fn tick_drive_poll(&mut self, ctx: &egui::Context) {
        // 0 ("Universe End") means never scan and never wake up for it.
        let secs = normalize_poll_secs(self.settings.nav.drive_poll_secs);
        if secs == 0 {
            return;
        }
        let interval = Duration::from_secs(secs.into());
        if self.last_drive_poll.elapsed() >= interval {
            self.last_drive_poll = Instant::now();
            self.refresh_drives();
        }
        ctx.request_repaint_after(interval);
    }

    /// Rebuild the device map (used to resolve Mount/Unmount targets), the nav pane's
    /// This PC children, and, if This PC is the folder on screen, its entries. Called on
    /// the poll timer and right after a Mount/Unmount so the change is reflected at once.
    fn refresh_drives(&mut self) {
        let list = drives();
        self.drive_devices.clear();
        for d in &list {
            if let Some(dev) = &d.device {
                self.drive_devices.insert(d.path.clone(), dev.clone());
            }
        }
        if let Some(node) = self
            .state
            .nav
            .iter_mut()
            .find(|n| n.path == Path::new(THIS_PC_LOCATION))
        {
            node.children = Some(
                list.iter()
                    .map(|d| {
                        let icon = if d.path == Path::new("/") {
                            Icon::Drive
                        } else {
                            Icon::Removable
                        };
                        NavNode::new(d.path.clone(), d.name.clone(), icon)
                    })
                    .collect(),
            );
        }
        if self.state.location == Path::new(THIS_PC_LOCATION) {
            self.state.set_entries(this_pc_entries_from(list));
        }
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
        let spawned = thread::Builder::new().name("fm-job".into()).spawn(move || {
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

    /// In-app "Open with" chooser built from the installed `.desktop` applications.
    fn open_with_dialog_ui(&mut self, ctx: &egui::Context) {
        let id = dialogs::session_id(ctx, "open_with_dialog", self.open_with_dialog.is_some());
        let Some(dialog) = self.open_with_dialog.as_mut() else {
            return;
        };
        let mut launch: Option<Option<open_with::AppEntry>> = None;
        let mut custom_run = false;
        let mut cancel = false;
        let file_name = dialog
            .paths
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let outcome = dialogs::show(ctx, id, "Open with", 460.0, |ui| {
            dialogs::body(ui, |ui| {
                ui.label(format!("Choose an application for \"{file_name}\":"));
                ui.checkbox(&mut dialog.show_all, "Show all applications");
                let list: Vec<&open_with::AppEntry> = if dialog.show_all {
                    dialog.apps.iter().collect()
                } else {
                    open_with::handlers_for(&dialog.apps, &dialog.mime)
                };
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    if list.is_empty() {
                        ui.label("No registered application found. Tick \"Show all applications\" or type a command below.");
                    }
                    for app in list {
                        let chosen = dialog.selected.as_deref() == Some(app.name.as_str());
                        let resp = ui.selectable_label(chosen, &app.name);
                        if resp.clicked() {
                            dialog.selected = Some(app.name.clone());
                        }
                        if resp.double_clicked() {
                            launch = Some(Some(app.clone()));
                        }
                    }
                });
                ui.add_space(6.0);
                ui.label("Or run a command:");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut dialog.custom)
                        .hint_text("e.g. vlc, code, mpv --fs")
                        .desired_width(f32::INFINITY),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    custom_run = true;
                }
            });
            dialogs::footer(ui, |ui| {
                if dialogs::button(ui, "Open", true).clicked() {
                    if !dialog.custom.trim().is_empty() {
                        custom_run = true;
                    } else if let Some(name) = dialog.selected.clone() {
                        launch = Some(dialog.apps.iter().find(|a| a.name == name).cloned());
                    }
                }
                if dialogs::button(ui, "Cancel", false).clicked() {
                    cancel = true;
                }
            });
        });
        cancel |= outcome.dismissed;
        if cancel {
            self.open_with_dialog = None;
            return;
        }
        let ok = if custom_run {
            Some(open_with::launch_custom(dialog.custom.trim(), &dialog.paths))
        } else if let Some(Some(app)) = launch {
            Some(open_with::launch(&app, &dialog.paths))
        } else {
            None
        };
        if let Some(ok) = ok {
            self.open_with_dialog = None;
            if !ok {
                self.status = "Couldn't start that application.".into();
            }
        }
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
                    if ui.button("Home").clicked()
                        && let Some(home) = dirs::home_dir()
                    {
                        dialog.text = home.display().to_string();
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
        if confirm && let Some(done) = self.dest_dialog.take() {
            self.transfer(done);
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
                (path_entry_exists(&target) && target != *p)
                    .then(|| name.to_string_lossy().into_owned())
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
                self.status =
                    "Copied inside Windowstyle only (system clipboard unavailable).".to_string();
            }
        }
    }

    /// Put plain text on the system clipboard (copy path / name / URL). The persistent arboard
    /// handle is used first, because it is the same one that already works for file copies and
    /// it keeps serving the selection afterwards (X11 / Wayland data-control). `ctx.copy_text`
    /// is only a fallback: on some Wayland setups the egui backend silently drops it, which is
    /// why "Copy path" and friends appeared to do nothing.
    fn copy_text_to_clipboard(&mut self, ctx: &egui::Context, text: String, what: &str) {
        if text.is_empty() {
            self.status = format!("Nothing to copy: no {what}.");
            return;
        }
        let count = text.lines().count();
        if self.sys_clipboard.is_none() {
            self.sys_clipboard = arboard::Clipboard::new().ok();
        }
        let copied = self
            .sys_clipboard
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(text.clone()).is_ok());
        if !copied {
            ctx.copy_text(text);
        }
        self.status = format!(
            "Copied {count} {what}{} to the clipboard.",
            if count == 1 { "" } else { "s" }
        );
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
        if self.state.location == Path::new(THIS_PC_LOCATION) {
            self.state.set_entries(this_pc_entries());
            return;
        }
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
            if outcome.search_generation == self.search_generation
                && self.state.location == root
                && !self.state.search_text.trim().is_empty()
            {
                // Keep the typed text and the history; the listing becomes the ranked results.
                self.state.set_entries(entries);
                self.state.search_active = true;
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
        let mut processed = 0usize;
        while processed < WORKER_MESSAGES_PER_FRAME {
            let Ok(message) = self.worker_messages.try_recv() else {
                break;
            };
            processed += 1;
            match message {
                WorkerMessage::WindowLoaded {
                    serial,
                    path,
                    mut entries,
                } => {
                    attach_cached_thumbnails(
                        &mut entries,
                        &self.textures,
                        self.settings.thumbnails,
                    );
                    if let Some(child) = self
                        .children
                        .iter_mut()
                        .find(|child| child.serial == serial)
                    {
                        // Do not overwrite a destination the user navigated to while the first
                        // listing was being read. The completion is also ignored after closure.
                        if child.state.location == path {
                            child.state.set_entries(entries);
                            if let Some(view) = self.settings.folder_view(&path).cloned() {
                                apply_folder_view(&mut child.state, &view);
                            }
                        }
                        if child.status == "Loading folder…" {
                            child.status.clear();
                            child.status_seen.clear();
                        }
                    }
                    // The background read may have promoted the folder into a hot tier.
                    self.apply_watch_plan();
                }
                WorkerMessage::Job(outcome) => self.finish_job(outcome),
                WorkerMessage::DriveOp { ok, message } => {
                    if !ok {
                        eprintln!("{message}");
                    }
                    self.status = message;
                    self.refresh_drives();
                }
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
                                && (self.textures.len() >= self.texture_handle_limit
                                    || self.texture_cache_bytes.saturating_add(memory_bytes)
                                        > self.texture_cache_budget_bytes)
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
                                        entry.thumb_size = None;
                                    }
                                }
                            }
                            // ThumbnailStore clamps its longest edge to 2048, so a valid
                            // RGBA thumbnail fits comfortably within this budget on its own.
                            if memory_bytes <= self.texture_cache_budget_bytes {
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
                            if self.settings.thumbnails
                                && let Some(entry) = self
                                    .state
                                    .entries_mut()
                                    .iter_mut()
                                    .find(|entry| entry.path == path)
                            {
                                entry.thumbnail =
                                    self.textures.get(&path).map(|cached| cached.handle.id());
                                entry.thumb_size = self.textures.get(&path).map(|cached| {
                                    let [w, h] = cached.handle.size();
                                    (w as u32, h as u32)
                                });
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
        if processed == WORKER_MESSAGES_PER_FRAME {
            // Leave the rest queued and yield to egui before handling another batch.
            ctx.request_repaint();
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
                let is_unmounted_drive = path
                    .to_str()
                    .is_some_and(|s| s.starts_with(UNMOUNTED_DRIVE_PREFIX));
                if path.is_dir() || is_unmounted_drive {
                    // Unmounted-drive placeholders go through navigate() so its guard
                    // reports "not mounted" instead of handing `device://…` to xdg-open.
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
                if to == path {
                    return;
                }
                if path_entry_exists(&to) {
                    self.status = format!("Rename failed: '{}' already exists.", to.display());
                    return;
                }
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
            Event::SetPermissions { targets, mode } => self.set_permissions(targets, mode),
            Event::Drop { sources, target } => {
                for source in sources {
                    if let Some(name) = source.file_name() {
                        let dest = target.join(name);
                        if dest != source && path_entry_exists(&dest) {
                            self.status = format!(
                                "Move failed: '{}' already exists in the destination folder.",
                                dest.display()
                            );
                            break;
                        }
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

    /// Recursive, case-insensitive name search below the current folder by walking it (used on
    /// tmpfs/ramfs, which plocate does not index).
    /// Runs on a job thread; a newer search makes an older, still running one irrelevant.
    fn search_current_folder(&mut self, query: &str) {
        let needle = query.trim().to_lowercase();
        let shown = query.trim().to_owned();
        let root = self.state.location.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.status = format!("Searching for '{shown}'…");
        let filter = SearchFilter::new(self.settings.search_app_data);
        self.spawn_job(move || {
            let (found, truncated) = walk_search(&root, &needle, &filter);
            let (found, truncated) = rank_results(found, &root, &needle, truncated);
            let message = search_message(found.len(), &shown, "walk", truncated);
            JobOutcome {
                origin: root.clone(),
                search: Some((root, found, message)),
                search_generation: generation,
                ..JobOutcome::default()
            }
        });
    }

    /// Search by file name. Folders on tmpfs/ramfs are not in the plocate database, so they
    /// are walked directly; everywhere else only the plocate database is used (no fallback).
    fn search(&mut self, query: String) {
        if query.trim().is_empty() {
            self.status = "Enter a search term.".into();
            return;
        }
        if is_ram_filesystem(&self.state.location) {
            self.search_current_folder(&query);
        } else {
            self.search_plocate(&query);
        }
    }

    /// Name search below the current folder: `plocate` database first, `find` if that fails.
    /// Runs on a job thread; a newer search makes an older, still running one irrelevant.
    fn search_plocate(&mut self, query: &str) {
        let needle = query.trim().to_lowercase();
        let shown = query.trim().to_owned();
        let root = self.state.location.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.status = format!("Searching for '{shown}'…");
        let filter = SearchFilter::new(self.settings.search_app_data);
        self.spawn_job(move || {
            // plocate first; when it errors or finds nothing (database may be stale or
            // missing) plain `find`; if that finds nothing either, "not found".
            let from_plocate = plocate_search(&root, &needle, &filter)
                .ok()
                .filter(|(found, _)| !found.is_empty());
            let (result, source) = match from_plocate {
                Some(result) => (Ok(result), "plocate"),
                None => (find_search(&root, &needle, &filter), "find"),
            };
            let (found, message) = match result {
                Ok((found, truncated)) => {
                    let message = search_message(found.len(), &shown, source, truncated);
                    (found, message)
                }
                Err(message) => (Vec::new(), message),
            };
            JobOutcome {
                origin: root.clone(),
                search: Some((root, found, message)),
                search_generation: generation,
                ..JobOutcome::default()
            }
        });
    }

    /// Mount or unmount `device` via `udisksctl`, off the UI thread: `udisksctl` blocks
    /// while a polkit authentication prompt (its own window) is up, so running it inline
    /// would freeze the app until that dialog is answered.
    fn run_drive_op(&mut self, ctx: &egui::Context, device: String, mount: bool) {
        let verb = if mount { "mount" } else { "unmount" };
        self.status = format!("{}ing {device}…", if mount { "Mount" } else { "Unmount" });
        let tx = self.worker_messages_sender();
        let worker_ctx = ctx.clone();
        let device_for_thread = device.clone();
        if let Err(error) = thread::Builder::new()
            .name("file-manager-drive-op".into())
            .spawn(move || {
                let out = Proc::new("udisksctl")
                    .arg(verb)
                    .arg("-b")
                    .arg(&device_for_thread)
                    .output();
                let message = match out {
                    Ok(out) if out.status.success() => {
                        String::from_utf8_lossy(&out.stdout).trim().to_string()
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        let err = err.trim();
                        let err = if err.is_empty() {
                            "udisksctl failed"
                        } else {
                            err
                        };
                        format!("Could not {verb} {device_for_thread}: {err}")
                    }
                    Err(error) => format!("Could not run udisksctl: {error}"),
                };
                let ok = !message.starts_with("Could not");
                let _ = tx.send(WorkerMessage::DriveOp { ok, message });
                worker_ctx.request_repaint();
            })
        {
            self.status = format!("Could not start udisksctl: {error}");
        }
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
            Command::ToggleThisPcExpanded => {
                self.state.this_pc_expanded = !self.state.this_pc_expanded;
                self.settings.nav.this_pc_expanded = self.state.this_pc_expanded;
                // Apply immediately to the live nav tree, not only on next launch.
                let expanded = self.state.this_pc_expanded;
                if let Some(node) = self
                    .state
                    .nav
                    .iter_mut()
                    .find(|n| n.path == Path::new(THIS_PC_LOCATION))
                {
                    node.expanded = expanded;
                }
                self.save_settings();
            }
            Command::SetDrivePollSecs(secs) => {
                let secs = normalize_poll_secs(secs);
                self.state.drive_poll_secs = secs;
                self.settings.nav.drive_poll_secs = secs;
                self.status = if secs == 0 {
                    "Drives will never be rescanned automatically.".to_owned()
                } else {
                    format!("Drives will be rescanned every {secs} seconds.")
                };
                self.save_settings();
            }
            Command::AuditThumbnails => self.start_audit(ctx),
            Command::GenerateMissingThumbnails => self.start_generation(ctx),
            Command::BuildIndex => {
                // The terminal asks for the sudo password; nothing runs without the user.
                self.status = if open_terminal_with(&here, Some(UPDATEDB_SCRIPT)) {
                    "Enter your password in the terminal to update the plocate database.".into()
                } else {
                    "No terminal emulator could be started.".into()
                };
            }
            Command::ToggleSearchAppData => {
                self.state.search_app_data = !self.state.search_app_data;
                self.settings.search_app_data = self.state.search_app_data;
                self.status = if self.state.search_app_data {
                    "Search now includes application data folders.".into()
                } else {
                    "Search skips application data folders (.config and .local stay searchable).".into()
                };
                self.save_settings();
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
                    let mut failed = false;
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
                                failed = true;
                                break;
                            }
                        }
                    }
                    if made > 0 && !failed {
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
                self.copy_text_to_clipboard(ctx, text, "path");
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
                self.copy_text_to_clipboard(ctx, folders.join("\n"), "folder path");
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
                // Exclusive creation closes the race between choosing a free name and writing
                // the file; `fs::write` could otherwise truncate a newly-created target.
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&new_path)
                {
                    Ok(_file) => {
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
                self.copy_text_to_clipboard(ctx, text, "name");
            }
            Command::CopyUrl => {
                let text = targets
                    .iter()
                    .map(|p| file_url(p))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.copy_text_to_clipboard(ctx, text, "URL");
            }
            Command::Edit => {
                for path in &targets {
                    if !open_in_editor(path) {
                        self.status = "No editor could be started.".into();
                    }
                }
            }
            Command::OpenWith => {
                if let Some(first) = targets.first() {
                    self.open_with_dialog = Some(OpenWithDialog {
                        paths: targets.to_vec(),
                        mime: open_with::mime_for(first),
                        apps: open_with::all_apps(),
                        show_all: false,
                        selected: None,
                        custom: String::new(),
                    });
                }
            }
            Command::Eject => {
                let Some(target) = targets.first() else {
                    return;
                };
                let Some(device) = self.drive_devices.get(target).cloned() else {
                    self.status = "Don't know the device behind this drive.".into();
                    return;
                };
                self.run_drive_op(ctx, device, false);
            }
            Command::MountDrive => {
                let Some(target) = targets.first() else {
                    return;
                };
                let Some(device) = self.drive_devices.get(target).cloned() else {
                    self.status = "Don't know the device behind this drive.".into();
                    return;
                };
                self.run_drive_op(ctx, device, true);
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
                    match run_undo_action(action.clone()) {
                        Ok(inverse) => {
                            self.redo_stack.push(inverse);
                            self.status = "Undone.".into();
                            self.reload();
                        }
                        Err(error) => {
                            self.undo_stack.push(action);
                            self.status = format!("Undo failed: {error}");
                        }
                    }
                }
            }
            Command::Redo => {
                if let Some(action) = self.redo_stack.pop() {
                    match run_undo_action(action.clone()) {
                        Ok(inverse) => {
                            self.undo_stack.push(inverse);
                            self.status = "Redone.".into();
                            self.reload();
                        }
                        Err(error) => {
                            self.redo_stack.push(action);
                            self.status = format!("Redo failed: {error}");
                        }
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
                let mut failed = false;
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
                            failed = true;
                            break;
                        }
                    }
                }
                if made > 0 && !failed {
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
                self.open_window(
                    ctx,
                    single_instance::OpenRequest::local(
                        start,
                        single_instance::current_hyprland_workspace(),
                    ),
                );
            }
            other => {
                self.status = format!("{other:?} is not available in this host.");
            }
        }
    }
}

impl AppCore {
    /// Everything that belongs to one window: reacting to changes of its folder, drawing it,
    /// handling its events and dialogs. The first window and every extra window run this
    /// with their own state swapped in.
    fn window_frame(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Settings are the single source of truth for the theme, so all windows agree.
        self.state.dark_mode = self.settings.dark_mode;
        self.state.nav_icon_size = self.settings.navigation_icon_size.clamp(12, 48) as f32;
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
        self.open_with_dialog_ui(ctx);
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
        std::mem::swap(&mut self.open_with_dialog, &mut w.open_with_dialog);
        std::mem::swap(&mut self.pending_transfer, &mut w.pending_transfer);
        std::mem::swap(&mut self.pending_delete, &mut w.pending_delete);
        std::mem::swap(&mut self.pending_extract, &mut w.pending_extract);
        std::mem::swap(&mut self.status, &mut w.status);
        std::mem::swap(&mut self.status_seen, &mut w.status_seen);
        std::mem::swap(&mut self.status_seen_at, &mut w.status_seen_at);
    }

    /// Open `request.path` in a child viewport, unless this IPC request has already timed out.
    fn open_window(&mut self, ctx: &egui::Context, request: single_instance::OpenRequest) {
        if !request.active.load(Ordering::Acquire) {
            return;
        }
        if self.children.len() >= MAX_CHILD_WINDOWS {
            self.status = format!("At most {} windows can be open at once.", MAX_CHILD_WINDOWS + 1);
            if let Some(ack) = request.opened.as_ref() {
                let _ = ack.send(false);
            }
            return;
        }
        let single_instance::OpenRequest {
            path,
            workspace_target,
            active: request_active,
            opened: opened_ack,
        } = request;
        eprintln!(
            "Windowstyle: processing open request for {} (root frame, workspace target {:?}).",
            path.display(),
            workspace_target
        );
        let start = if path.is_dir() {
            path
        } else {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        };
        let mut explorer = Explorer::new();
        explorer.options.title_bar = self.explorer.options.title_bar;
        let mut state = ExplorerState::new();
        state.subfolders_fn = Some(subfolders);
        load_settings_into_state(&mut state, &self.settings);
        let pins: Vec<PathBuf> = self.behavior.pins().cloned().collect();
        state.nav = build_nav(&pins, self.settings.nav.this_pc_expanded);
        // Do not scan the requested folder on the UI thread. This process may already be
        // managing other windows, so blocking here would delay their close/input events too.
        state.set_location(start.clone(), Vec::new());
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
        let serial = self.window_serial;
        let title = window_title(&start);
        // Immediate child viewports are tied to their parent for repaint scheduling and may be
        // placed on the parent's workspace by a compositor. In Hyprland, give the just-created
        // child a unique temporary title so we can move and focus exactly this window on the
        // workspace captured by the invoking process. Other desktops use normal eframe focus.
        let launch_title = workspace_target.as_ref().and_then(|_| {
            std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").map(|_| {
                format!("__windowstyle_{}_{}__", std::process::id(), serial)
            })
        });
        let target_workspace = workspace_target.filter(|_| launch_title.is_some());
        let launch_started_at = launch_title.as_ref().map(|_| Instant::now());
        self.children.push(ChildWindow {
            id: egui::ViewportId::from_hash_of(("file-manager-window", serial)),
            serial,
            title,
            launch_title: launch_title.clone(),
            launch_started_at,
            launch_moved_to_target: false,
            close: false,
            request_active,
            opened_ack,
            fresh: true,
            explorer,
            state,
            prev_location: None,
            watcher,
            dest_dialog: None,
            open_with_dialog: None,
            pending_transfer: None,
            pending_delete: None,
            pending_extract: None,
            status: "Loading folder…".into(),
            status_seen: String::new(),
            status_seen_at: Instant::now(),
        });

        // Populate the first listing off-thread; this is a one-shot worker per new window and
        // does not create another process or another copy of the shared caches.
        let listing_sender = self.worker_sender.clone();
        let listing_ctx = self.egui_ctx.clone();
        let listing_path = start.clone();
        let listing_spawn = thread::Builder::new()
            .name("file-manager-window-listing".into())
            .spawn(move || {
                let entries = load_dir(&listing_path, true);
                let _ = listing_sender.send(WorkerMessage::WindowLoaded {
                    serial,
                    path: listing_path,
                    entries,
                });
                listing_ctx.request_repaint();
            });
        if let Err(error) = listing_spawn {
            eprintln!("Could not start folder-listing worker: {error}");
            if let Some(child) = self.children.iter_mut().find(|child| child.serial == serial) {
                child.status = format!("Could not load folder: {error}");
            }
        }

        if let (Some(workspace_target), Some(marker)) = (target_workspace, launch_title) {
            let sender = self.hyprland_placement_tx.clone();
            let repaint_ctx = ctx.clone();
            let spawn = thread::Builder::new()
                .name("windowstyle-hyprland-placement".into())
                .spawn(move || {
                    let selector = format!("title:^{marker}$");
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let mut appeared = false;
                    // Wait briefly for the native viewport to be registered with Hyprland.
                    // This is a one-shot startup path, never an idle poll.
                    for _ in 0..30 {
                        if Instant::now() >= deadline {
                            break;
                        }
                        let listed = Proc::new("timeout")
                            .args(["-k", "100ms", "500ms", "hyprctl", "-j", "clients"])
                            .output()
                            .is_ok_and(|output| {
                                output.status.success()
                                    && String::from_utf8_lossy(&output.stdout).contains(&marker)
                            });
                        if Instant::now() >= deadline {
                            break;
                        }
                        if listed {
                            appeared = true;
                            break;
                        }
                        thread::sleep(Duration::from_millis(40));
                    }

                    let mut moved_to_target = false;
                    if appeared && Instant::now() < deadline {
                        let move_target = format!("{workspace_target},{selector}");
                        // A completed move remains valid even if the compositor took longer than
                        // expected to answer. Do not later focus the root workspace merely
                        // because the optional focus step missed the time budget.
                        moved_to_target =
                            hyprland_dispatch("movetoworkspacesilent", &move_target);
                        if moved_to_target {
                            // Report the move immediately. If the focus dispatcher then hangs,
                            // the UI timeout must not focus the window back onto the root workspace.
                            let _ = sender.send(HyprlandPlacementEvent::MovedToTarget(serial));
                        }
                        if moved_to_target && Instant::now() < deadline {
                            thread::sleep(Duration::from_millis(35));
                            let mut focused = false;
                            // One retry handles a short compositor scheduling race without
                            // introducing a permanent polling loop or extra process.
                            for attempt in 0..2 {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                if hyprland_dispatch("focuswindow", &selector) {
                                    focused = true;
                                    break;
                                }
                                if attempt == 0 && Instant::now() < deadline {
                                    thread::sleep(Duration::from_millis(45));
                                }
                            }
                            if !focused {
                                eprintln!(
                                    "Windowstyle: window moved to workspace {workspace_target}, but Hyprland could not confirm focus."
                                );
                            }
                        }
                    }
                    // Always release the temporary title. The UI suppresses native focus fallback
                    // on Hyprland so a failed move cannot deliberately focus the root workspace.
                    let _ = sender.send(HyprlandPlacementEvent::Finished {
                        serial,
                        moved_to_target,
                    });
                    repaint_ctx.request_repaint();
                });
            if let Err(error) = spawn {
                eprintln!("Could not start Hyprland window placement: {error}");
                if let Some(child) = self
                    .children
                    .iter_mut()
                    .find(|child| child.serial == serial)
                {
                    child.launch_title = None;
                    child.launch_started_at = None;
                }
            }
        }
        // A child viewport can open another child. Wake the root scheduler so it registers the
        // new deferred viewport immediately, even if the original root is hidden or idle.
        self.egui_ctx.request_repaint();
        ctx.request_repaint();
    }

    /// Folders sent by other launches of the program become windows of this process.
    fn poll_open_requests(&mut self, ctx: &egui::Context) {
        let mut wanted = Vec::new();
        if let Some(rx) = self.open_requests.as_ref() {
            while let Ok(request) = rx.try_recv() {
                wanted.push(request);
            }
        }
        for request in wanted {
            self.open_window(ctx, request);
        }
    }

    fn poll_hyprland_placements(&mut self) {
        // Drain progress before checking the timeout. A completed move must win over timeout
        // fallback even if its focus command is still waiting for compositor IPC.
        while let Ok(event) = self.hyprland_placements.try_recv() {
            match event {
                HyprlandPlacementEvent::MovedToTarget(serial) => {
                    if let Some(child) = self
                        .children
                        .iter_mut()
                        .find(|child| child.serial == serial)
                    {
                        if child.launch_title.is_some() {
                            child.launch_moved_to_target = true;
                        }
                    }
                }
                HyprlandPlacementEvent::Finished {
                    serial,
                    moved_to_target,
                } => {
                    if let Some(child) = self
                        .children
                        .iter_mut()
                        .find(|child| child.serial == serial)
                    {
                        // Ignore late completions after the timeout already released the marker.
                        if child.launch_title.is_some() {
                            child.launch_moved_to_target |= moved_to_target;
                            child.launch_title = None;
                            child.launch_started_at = None;
                            if !child.launch_moved_to_target {
                                child.fresh = true;
                            }
                        }
                    }
                }
            }
        }

        // Fail open even if hyprctl stalls: never leave a temporary title forever. If the move
        // already succeeded, keep the window on its requested workspace instead of focusing the
        // root workspace as a fallback.
        for child in &mut self.children {
            let timed_out = child.launch_title.is_some()
                && child
                    .launch_started_at
                    .is_some_and(|started| started.elapsed() >= Duration::from_secs(3));
            if timed_out {
                child.launch_title = None;
                child.launch_started_at = None;
                if child.launch_moved_to_target {
                    eprintln!(
                        "Windowstyle: window reached its target workspace; focus confirmation timed out."
                    );
                } else {
                    child.fresh = true;
                    eprintln!(
                        "Windowstyle: Hyprland placement timed out; suppressing root-workspace focus fallback."
                    );
                }
            }
        }
    }

    /// Register each child as a deferred viewport. Unlike immediate viewports, these native
    /// windows have independent repaint/event scheduling and do not need the root window's frame
    /// to synchronously render their contents.
    fn child_windows(&mut self, shared: &Arc<Mutex<AppCore>>, ctx: &egui::Context) {
        // Child close events are consumed in that child's own callback. Drop closed children
        // here, while the root frame owns the list, so the viewport is no longer registered.
        self.children.retain(|child| !child.close);

        let windows: Vec<(egui::ViewportId, String)> = self
            .children
            .iter()
            .map(|child| {
                (
                    child.id,
                    child
                        .launch_title
                        .as_ref()
                        .unwrap_or(&child.title)
                        .clone(),
                )
            })
            .collect();

        for (id, builder_title) in windows {
            let shared = Arc::clone(shared);
            let builder = egui::ViewportBuilder::default()
                .with_title(builder_title)
                .with_inner_size([1100.0, 680.0])
                .with_min_inner_size([640.0, 420.0]);

            ctx.show_viewport_deferred(id, builder, move |ui, _class| {
                let window_ctx = ui.ctx().clone();
                let mut core = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(index) = core.children.iter().position(|child| child.id == id) else {
                    // The root removed this child after its close request. Stop presenting it.
                    window_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                };

                let mut child = core.children.remove(index);
                if !child.request_active.load(Ordering::Acquire) {
                    // The launch timed out and started a standalone process. Close this viewport
                    // if it raced with cancellation, and never reinsert the stale child record.
                    window_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    if core.root_hidden && core.children.is_empty() {
                        core.quitting = true;
                        core.egui_ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Close,
                        );
                    } else {
                        core.egui_ctx.request_repaint_of(egui::ViewportId::ROOT);
                    }
                    return;
                }
                if let Some(ack) = child.opened_ack.take() {
                    eprintln!("Windowstyle: child viewport {:?} created; acknowledging request.", id);
                    let _ = ack.send(true);
                }
                if window_ctx.input(|input| input.viewport().close_requested()) {
                    child.close = true;
                } else {
                    if child.fresh {
                        child.fresh = false;
                        // Hyprland performs the precise workspace handoff. Otherwise ask the
                        // native window backend to focus this window when it is first shown.
                        if child.launch_title.is_none()
                            && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
                        {
                            // On Hyprland, never fall back to native focus for a launch whose
                            // workspace handoff failed: its default parent workspace may be the
                            // original root's workspace, and focusing it would recreate the bug.
                            // The IPC worker owns focus in Hyprland; other desktops use eframe.
                            window_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                    }

                    // Existing application code is written against one active view state. Swap
                    // this child's state in for the duration of its frame, while shared caches,
                    // settings, workers, clipboard and operations remain in the single process.
                    core.swap_window(&mut child);
                    core.window_frame(ui, &window_ctx);
                    let title = window_title(&core.state.location);
                    core.swap_window(&mut child);

                    if child.launch_title.is_none() && title != child.title {
                        child.title = title.clone();
                        window_ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
                    }
                }

                if !child.close {
                    let insertion = index.min(core.children.len());
                    core.children.insert(insertion, child);
                } else {
                    // Explicitly wake the root, not whichever deferred viewport last used this
                    // Context. If the root was hidden and this was the last child, close it
                    // directly: an invisible root may not receive another compositor frame.
                    if core.root_hidden && core.children.is_empty() {
                        core.quitting = true;
                        core.egui_ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Close,
                        );
                    } else {
                        core.egui_ctx.request_repaint_of(egui::ViewportId::ROOT);
                    }
                }
            });
        }
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

/// Execute one Hyprland IPC action. This is called only by the short-lived placement worker,
/// never during ordinary rendering. Failure is reported to the caller; on Hyprland we do not
/// fall back to native focus because that can return focus to the root's workspace.
fn hyprland_dispatch(dispatcher: &str, argument: &str) -> bool {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return false;
    }
    Proc::new("timeout")
        .args(["-k", "100ms", "500ms", "hyprctl", "dispatch", dispatcher, argument])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim().eq_ignore_ascii_case("ok")
        })
}

impl AppCore {
    fn ui_root(
        &mut self,
        shared: &Arc<Mutex<AppCore>>,
        ui: &mut egui::Ui,
        _frame: &mut eframe::Frame,
    ) {
        let ctx = ui.ctx().clone();
        self.handle_lifecycle(&ctx);
        self.persist_zoom(&ctx);
        self.pump_tier_events();
        self.poll_workers(&ctx);
        self.poll_open_requests(&ctx);
        self.poll_hyprland_placements();
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
        self.child_windows(shared, &ctx);
        self.tick_persistence(&ctx);
        self.tick_drive_poll(&ctx);
        if self.root_hidden && self.children.is_empty() {
            // Last window gone: really quit.
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if !self.children.is_empty() {
            // Deferred child viewports stay alive only while the root pass keeps registering
            // them, so keep the root scheduled at a low rate while children exist. Open requests
            // do NOT rely on this: the listener requests a ROOT repaint, and the patched eframe
            // (vendor/eframe) runs a full pass even when the compositor withholds frame callbacks
            // from a root on an inactive workspace.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let shared = Arc::clone(&self.shared);
        let mut core = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core.ui_root(&shared, ui, frame);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let mut core = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core.flusher.request_shutdown();
        core.flush_state();
    }
}

fn attach_cached_thumbnails(
    entries: &mut [Entry],
    textures: &HashMap<PathBuf, CachedTexture>,
    enabled: bool,
) {
    for entry in entries {
        let cached = if enabled {
            textures.get(&entry.path).filter(|cached| {
                cached.source_size == entry.size && cached.source_modified == entry.modified
            })
        } else {
            None
        };
        entry.thumbnail = cached.map(|cached| cached.handle.id());
        entry.thumb_size = cached.map(|cached| {
            let [w, h] = cached.handle.size();
            (w as u32, h as u32)
        });
    }
}

/// Build a [`FolderView`] snapshot of the view-related parts of `state`, so it can be
/// compared against (or written into) `settings.per_folder`.
fn folder_view_from_state(state: &ExplorerState) -> FolderView {
    FolderView {
        mode: match state.view {
            egui_explorer::ViewMode::ExtraLargeIcons => SavedViewMode::ExtraLargeIcons,
            egui_explorer::ViewMode::LargeIcons => SavedViewMode::LargeIcons,
            egui_explorer::ViewMode::MediumIcons => SavedViewMode::MediumIcons,
            egui_explorer::ViewMode::SmallIcons => SavedViewMode::SmallIcons,
            egui_explorer::ViewMode::List => SavedViewMode::List,
            egui_explorer::ViewMode::Details => SavedViewMode::Details,
            egui_explorer::ViewMode::Tiles => SavedViewMode::Tiles,
            egui_explorer::ViewMode::Content => SavedViewMode::Content,
        },
        sort_key: match state.sort_key {
            egui_explorer::SortKey::Name => SavedSortKey::Name,
            egui_explorer::SortKey::DateModified => SavedSortKey::DateModified,
            egui_explorer::SortKey::Type => SavedSortKey::Type,
            egui_explorer::SortKey::Size => SavedSortKey::Size,
        },
        sort_order: match state.sort_order {
            egui_explorer::SortOrder::Ascending => SavedSortOrder::Ascending,
            egui_explorer::SortOrder::Descending => SavedSortOrder::Descending,
        },
        group: match state.group {
            egui_explorer::GroupKey::None => SavedGroupKey::None,
            egui_explorer::GroupKey::Name => SavedGroupKey::Name,
            egui_explorer::GroupKey::DateModified => SavedGroupKey::DateModified,
            egui_explorer::GroupKey::Type => SavedGroupKey::Type,
            egui_explorer::GroupKey::Size => SavedGroupKey::Size,
        },
    }
}

/// Apply a remembered per-folder view (see [`folder_view_from_state`]) onto `state`.
fn apply_folder_view(state: &mut ExplorerState, fv: &FolderView) {
    state.view = match fv.mode {
        SavedViewMode::ExtraLargeIcons => egui_explorer::ViewMode::ExtraLargeIcons,
        SavedViewMode::LargeIcons => egui_explorer::ViewMode::LargeIcons,
        SavedViewMode::MediumIcons => egui_explorer::ViewMode::MediumIcons,
        SavedViewMode::SmallIcons => egui_explorer::ViewMode::SmallIcons,
        SavedViewMode::List => egui_explorer::ViewMode::List,
        SavedViewMode::Details => egui_explorer::ViewMode::Details,
        SavedViewMode::Tiles => egui_explorer::ViewMode::Tiles,
        SavedViewMode::Content => egui_explorer::ViewMode::Content,
    };
    state.sort_key = match fv.sort_key {
        SavedSortKey::Name => egui_explorer::SortKey::Name,
        SavedSortKey::DateModified => egui_explorer::SortKey::DateModified,
        SavedSortKey::Type => egui_explorer::SortKey::Type,
        SavedSortKey::Size => egui_explorer::SortKey::Size,
    };
    state.sort_order = match fv.sort_order {
        SavedSortOrder::Ascending => egui_explorer::SortOrder::Ascending,
        SavedSortOrder::Descending => egui_explorer::SortOrder::Descending,
    };
    state.group = match fv.group {
        SavedGroupKey::None => egui_explorer::GroupKey::None,
        SavedGroupKey::Name => egui_explorer::GroupKey::Name,
        SavedGroupKey::DateModified => egui_explorer::GroupKey::DateModified,
        SavedGroupKey::Type => egui_explorer::GroupKey::Type,
        SavedGroupKey::Size => egui_explorer::GroupKey::Size,
    };
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
    state.search_app_data = settings.search_app_data;
    state.this_pc_expanded = settings.nav.this_pc_expanded;
    state.drive_poll_secs = normalize_poll_secs(settings.nav.drive_poll_secs);
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

fn apply_tier_event(tiers: &mut fm_tiers::Tiers, event: &fm_notify::Event) {
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
    } else if is_executable_without_extension(&p) {
        // Linux executables have no `.exe`; the permission bit decides. Extensionless only,
        // so a user's `extension.<ext>` icons for named types still win.
        e.icon = Some(Icon::FileExec);
    }
    e
}

/// An extensionless regular file with an execute bit (one stat, only for such names).
#[cfg(unix)]
fn is_executable_without_extension(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if path.extension().is_some() {
        return false;
    }
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_without_extension(_path: &Path) -> bool {
    false
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

/// Mounted drives shown under This PC: (path, display name). The filesystem root first, then
/// everything mounted under /mnt, /media/<user> and /run/media/<user>.
/// One block-device partition as reported by `lsblk`, mounted or not.
struct BlockDevice {
    /// `/dev/sdXN` or similar.
    path: String,
    /// The filesystem label if it has one, else the bare device name (`sda1`).
    label: String,
    mountpoint: Option<PathBuf>,
}

/// Parse one `lsblk -P` line of space-separated `KEY="VALUE"` pairs. Values from lsblk
/// never contain a literal `"`, so this needs no escaping support.
fn parse_lsblk_pairs(line: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = line[key_start..i].to_owned();
        i += 1; // '='
        if i >= bytes.len() || bytes[i] != b'"' {
            break;
        }
        i += 1; // opening quote
        let val_start = i;
        while i < bytes.len() && bytes[i] != b'"' {
            i += 1;
        }
        let val = line[val_start..i].to_owned();
        i += 1; // closing quote
        map.insert(key, val);
    }
    map
}

/// Every formatted, non-swap partition `lsblk` knows about, mounted or not. Used to show
/// drives in This PC before the user has mounted them, and to find the device backing a
/// Mount/Unmount request.
fn list_block_devices() -> Vec<BlockDevice> {
    let Ok(out) = Proc::new("lsblk")
        .args(["-P", "-p", "-o", "PATH,FSTYPE,MOUNTPOINT,TYPE,LABEL"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devices = Vec::new();
    for line in text.lines() {
        let f = parse_lsblk_pairs(line);
        let is_partition = matches!(
            f.get("TYPE").map(String::as_str),
            Some("part") | Some("disk")
        );
        let fstype = f.get("FSTYPE").map(String::as_str).unwrap_or("");
        if !is_partition || fstype.is_empty() || fstype == "swap" {
            continue;
        }
        let Some(path) = f.get("PATH").filter(|p| !p.is_empty()).cloned() else {
            continue;
        };
        let label = f
            .get("LABEL")
            .filter(|l| !l.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone())
            });
        let mountpoint = f
            .get("MOUNTPOINT")
            .filter(|m| !m.is_empty())
            .map(PathBuf::from);
        devices.push(BlockDevice {
            path,
            label,
            mountpoint,
        });
    }
    devices
}

/// A drive as shown under This PC: `path` is the mountpoint when mounted, or a synthetic
/// `device://<node>` placeholder (nothing to browse into) when it is not.
struct Drive {
    path: PathBuf,
    name: String,
    /// `/dev/sdXN`; `None` only for the root filesystem, which is never offered for
    /// mount/unmount.
    device: Option<String>,
    mounted: bool,
}

/// Prefix used for the synthetic identity of a drive that `lsblk` reports but which
/// isn't mounted anywhere yet.
const UNMOUNTED_DRIVE_PREFIX: &str = "device://";

fn drives() -> Vec<Drive> {
    let mut drives = vec![Drive {
        path: PathBuf::from("/"),
        name: "File System".to_owned(),
        device: None,
        mounted: true,
    }];
    let mut roots = vec![PathBuf::from("/mnt")];
    if let Some(user) = std::env::var_os("USER") {
        roots.push(Path::new("/media").join(&user));
        roots.push(Path::new("/run/media").join(&user));
    }
    for root in roots {
        let Ok(rd) = fs::read_dir(&root) else {
            continue;
        };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|d| d.path())
            .filter(|p| p.is_dir())
            .collect();
        found.sort();
        for p in found {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            drives.push(Drive {
                path: p,
                name,
                device: None,
                mounted: true,
            });
        }
    }
    let seen: std::collections::HashSet<PathBuf> = drives.iter().map(|d| d.path.clone()).collect();
    for bd in list_block_devices() {
        match &bd.mountpoint {
            Some(mp) if seen.contains(mp) => continue, // already listed above
            Some(mp) => drives.push(Drive {
                path: mp.clone(),
                name: bd.label,
                device: Some(bd.path),
                mounted: true,
            }),
            None => drives.push(Drive {
                path: PathBuf::from(format!("{UNMOUNTED_DRIVE_PREFIX}{}", bd.path)),
                name: bd.label,
                device: Some(bd.path),
                mounted: false,
            }),
        }
    }
    drives
}

/// (total bytes, free bytes) of the filesystem holding `path`, via `df`.
fn drive_space(path: &Path) -> Option<(u64, u64)> {
    let out = Proc::new("df")
        .args(["-B1", "--output=size,avail"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cols = text.lines().nth(1)?.split_whitespace();
    let total = cols.next()?.parse().ok()?;
    let free = cols.next()?.parse().ok()?;
    Some((total, free))
}

/// Entries for the This PC overview, one per drive (mounted or not).
fn this_pc_entries() -> Vec<Entry> {
    this_pc_entries_from(drives())
}

fn this_pc_entries_from(list: Vec<Drive>) -> Vec<Entry> {
    list.into_iter()
        .map(|d| {
            let mut e = Entry::new(&d.path, EntryKind::Drive);
            e.name = d.name;
            e.name_lc = e.name.to_lowercase();
            e.device = d.device;
            e.mounted = d.mounted;
            e.space = if d.mounted {
                drive_space(&d.path)
            } else {
                None
            };
            e.icon = Some(if d.path == Path::new("/") {
                Icon::Drive
            } else {
                Icon::Removable
            });
            e
        })
        .collect()
}

fn build_nav(user_pins: &[PathBuf], this_pc_expanded: bool) -> Vec<NavNode> {
    let quick = quick_access_nodes(user_pins);
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = vec![
        NavNode::new(QUICK_ACCESS_ID, "Quick access", Icon::QuickAccess)
            .with_children(quick)
            .expanded(),
    ];
    roots.push(NavNode::new(home.clone(), "Home", Icon::FolderHome));
    let pc: Vec<NavNode> = drives()
        .into_iter()
        .map(|d| {
            let icon = if d.path == Path::new("/") {
                Icon::Drive
            } else {
                Icon::Removable
            };
            NavNode::new(d.path, d.name, icon)
        })
        .collect();
    let mut this_pc = NavNode::new("this-pc://", "This PC", Icon::ThisPc).with_children(pc);
    if this_pc_expanded {
        this_pc = this_pc.expanded();
    }
    roots.push(this_pc);
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
        let replacing =
            choice == ConflictChoice::Replace && path_entry_exists(&wanted) && wanted != *path;
        let target = if replacing {
            // Keep the replaced destination recoverable in the Trash before creating the new
            // item. The Trash crate is not included in this supplied archive, so its restore
            // identity cannot be made part of a composite Undo action safely here.
            if let Err(error) = move_to_trash(&wanted) {
                outcome.status = Some(format!("Could not replace '{}': {error}", wanted.display()));
                break;
            }
            wanted
        } else {
            unique(&wanted)
        };
        let result = if cut {
            let rename_result = if path_entry_exists(&target) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("destination '{}' already exists", target.display()),
                ))
            } else {
                fs::rename(path, &target)
            };
            rename_result.or_else(|rename_error| {
                // Do not turn a newly-arrived destination into an overwrite during the
                // cross-filesystem fallback. Standard-library rename cannot make this check
                // atomic against external processes, but exclusive copy creation is a second
                // protection when the destination races with this operation.
                if path_entry_exists(&target) {
                    return Err(rename_error);
                }
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
    // Do not overwrite a failure from a later item with a misleading success message.
    if done > 0 && outcome.status.is_none() {
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
        // Purge through the trash crate so the .trashinfo records go away too. A failed
        // index read must not look like a successful delete in the UI.
        let items = match fm_trash::list() {
            Ok(items) => items,
            Err(error) => {
                outcome.status = Some(format!("Could not read Trash: {error}"));
                return outcome;
            }
        };
        let mut matched = 0usize;
        let mut missing = 0usize;
        for path in &targets {
            if let Some(item) = items.iter().find(|i| i.trashed_path() == *path) {
                matched += 1;
                if let Err(error) = item.purge() {
                    outcome.status = Some(format!("Permanent delete failed: {error}"));
                    return outcome;
                }
            } else {
                missing += 1;
            }
        }
        outcome.status = Some(if missing > 0 && matched > 0 {
            format!(
                "Permanently deleted {matched} item(s); {missing} selected item(s) were no longer in the Trash."
            )
        } else if missing > 0 {
            "Permanent delete failed: the selected item(s) are no longer in the Trash.".into()
        } else {
            format!("Permanently deleted {matched} item(s) from Trash.")
        });
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

/// Whether `path` lives on tmpfs/ramfs (longest matching mount point in /proc/mounts).
fn is_ram_filesystem(path: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_), Some(point), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // /proc/mounts escapes spaces as \040.
        let point = PathBuf::from(point.replace("\\040", " "));
        if path.starts_with(&point) {
            let depth = point.components().count();
            if best.is_none_or(|(d, _)| depth >= d) {
                best = Some((depth, matches!(fstype, "tmpfs" | "ramfs")));
            }
        }
    }
    best.is_some_and(|(_, ram)| ram)
}

/// A search result row for `path` (one stat), or `None` if it vanished.
fn search_entry(path: &Path) -> Option<Entry> {
    let meta = fs::symlink_metadata(path).ok()?;
    let is_dir = meta.is_dir() || (meta.file_type().is_symlink() && path.is_dir());
    let mut e = Entry::new(
        path,
        if is_dir {
            EntryKind::Folder
        } else {
            EntryKind::File
        },
    );
    if !is_dir {
        e.size = meta.len();
        e.type_name = fm_mime::guess(path).type_name;
    }
    e.modified = meta.modified().ok();
    Some(e)
}

/// Folders the search ignores: caches and clutter always, application data unless enabled.
struct SearchFilter {
    skip_paths: Vec<PathBuf>,
}

impl SearchFilter {
    fn new(app_data: bool) -> Self {
        let mut skip_paths: Vec<PathBuf> = SEARCH_SKIP_SYSTEM.iter().map(PathBuf::from).collect();
        if let Some(home) = dirs::home_dir() {
            skip_paths.extend(SEARCH_SKIP_HOME.iter().map(|p| home.join(p)));
            if !app_data {
                skip_paths.extend(SEARCH_SKIP_APP_DATA.iter().map(|p| home.join(p)));
            }
        }
        Self { skip_paths }
    }

    /// Skip paths that do not contain the search root (searching inside one still works).
    fn active_paths<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        self.skip_paths.iter().filter(move |p| !root.starts_with(p))
    }

    /// A folder whose contents are not searched.
    fn prunes(&self, dir: &Path, root: &Path) -> bool {
        dir.file_name()
            .is_some_and(|name| SEARCH_SKIP_NAMES.iter().any(|n| name == std::ffi::OsStr::new(n)))
            || self.active_paths(root).any(|p| dir == p.as_path())
    }

    /// `path` lies inside an ignored folder (the folder itself may still match by name).
    fn skips(&self, path: &Path, root: &Path) -> bool {
        let in_named = path.strip_prefix(root).ok().and_then(Path::parent).is_some_and(|rel| {
            rel.components().any(|c| {
                SEARCH_SKIP_NAMES
                    .iter()
                    .any(|n| c.as_os_str() == std::ffi::OsStr::new(n))
            })
        });
        in_named
            || self
                .active_paths(root)
                .any(|p| path.starts_with(p) && path != p.as_path())
    }
}

/// Escape `*?[]\` so a name is matched literally by `find -iname/-path`.
fn glob_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Folders before files, then fewer levels below `root` first, then names that start with
/// the search text, then alphabetical; cut to `SEARCH_LIMIT`.
fn rank_results(
    mut found: Vec<Entry>,
    root: &Path,
    needle: &str,
    raw_truncated: bool,
) -> (Vec<Entry>, bool) {
    let base = root.components().count();
    found.sort_by_cached_key(|e| {
        (
            !e.is_dir(),
            e.path.components().count().saturating_sub(base),
            !e.name_lc.starts_with(needle),
            e.name_lc.clone(),
        )
    });
    let truncated = raw_truncated || found.len() > SEARCH_LIMIT;
    found.truncate(SEARCH_LIMIT);
    (found, truncated)
}

fn search_message(count: usize, shown: &str, source: &str, truncated: bool) -> String {
    if count == 0 {
        return format!("Not found: nothing matches '{shown}' here.");
    }
    format!(
        "Found {count} result(s) for '{shown}' in this folder and its subfolders ({source}){}.",
        if truncated {
            format!(" (limit {SEARCH_LIMIT})")
        } else {
            String::new()
        }
    )
}

/// Read NUL- or newline-separated paths from `child`, keeping existing ones below `root`
/// that are not in an ignored folder. Returns the entries, whether `RAW_SEARCH_LIMIT` cut
/// the list, and whatever the process printed on stderr.
fn collect_paths(
    mut child: std::process::Child,
    root: &Path,
    separator: u8,
    filter: &SearchFilter,
) -> (Vec<Entry>, bool, String) {
    use std::io::{BufRead, BufReader};
    use std::os::unix::ffi::OsStrExt;
    let mut found: Vec<Entry> = Vec::new();
    let mut truncated = false;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).split(separator).map_while(Result::ok) {
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(&line));
            if path == root || !path.starts_with(root) || filter.skips(&path, root) {
                continue;
            }
            if let Some(entry) = search_entry(&path) {
                found.push(entry);
                if found.len() >= RAW_SEARCH_LIMIT {
                    truncated = true;
                    let _ = child.kill();
                    break;
                }
            }
        }
    }
    let mut stderr = String::new();
    if !truncated {
        if let Some(mut err) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
        }
    }
    let _ = child.wait();
    (found, truncated, stderr.trim().to_owned())
}

/// Query the plocate database for names containing `needle` below `root`. An error means
/// plocate could not answer (missing program or database).
fn plocate_search(
    root: &Path,
    needle: &str,
    filter: &SearchFilter,
) -> Result<(Vec<Entry>, bool), String> {
    use std::process::Stdio;
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let child = Proc::new("plocate")
        .args(["-i", "-b", "-e", "--"])
        .arg(needle)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not run plocate: {error}"))?;
    let (found, truncated, stderr) = collect_paths(child, &root, b'\n', filter);
    if found.is_empty() && !stderr.is_empty() {
        return Err(stderr);
    }
    Ok(rank_results(found, &root, needle, truncated))
}

/// Name search with `find` (files and folders, case-insensitive). Searches a few levels
/// deep first and only goes deeper while there are few results, so shallow matches win.
fn find_search(
    root: &Path,
    needle: &str,
    filter: &SearchFilter,
) -> Result<(Vec<Entry>, bool), String> {
    use std::process::Stdio;
    let pattern = format!("*{}*", glob_escape(needle));
    let mut conditions: Vec<(&str, String)> = SEARCH_SKIP_NAMES
        .iter()
        .map(|name| ("-name", (*name).to_owned()))
        .collect();
    for path in filter.active_paths(root) {
        conditions.push(("-path", glob_escape(&path.to_string_lossy())));
    }
    let mut last = (Vec::new(), false);
    for depth in [Some(3u32), Some(6), Some(12), None] {
        let mut command = Proc::new("find");
        command.arg(root);
        if let Some(depth) = depth {
            command.arg("-maxdepth").arg(depth.to_string());
        }
        command.args(["-mindepth", "1", "(", "-type", "d", "("]);
        for (i, (flag, value)) in conditions.iter().enumerate() {
            if i > 0 {
                command.arg("-o");
            }
            command.arg(flag).arg(value);
        }
        // Ignored folders are still listed when their own name matches, but not entered.
        command.args(["(", "(", "-iname"]).arg(&pattern);
        command.args([")", "-print0", "-o", "-true", ")", "-prune", ")", "-o", "-iname"]);
        command.arg(&pattern).arg("-print0");
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not run find: {error}"))?;
        let (found, truncated, _) = collect_paths(child, root, 0, filter);
        let enough = found.len() >= 100 || truncated;
        last = rank_results(found, root, needle, truncated);
        if enough || depth.is_none() {
            break;
        }
    }
    Ok(last)
}

fn walk_search(root: &Path, needle: &str, filter: &SearchFilter) -> (Vec<Entry>, bool) {
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
                if found.len() >= RAW_SEARCH_LIMIT {
                    truncated = true;
                    break 'walk;
                }
            }
            // `file_type()` does not follow symlinks, so link loops cannot occur.
            if is_dir && !filter.prunes(&path, root) {
                stack.push(path);
            }
        }
    }
    (found, truncated)
}

fn tool_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// The 7-Zip command line tool, whichever way it is packaged.
fn seven_zip() -> Option<&'static str> {
    ["7zz", "7z", "7za"]
        .into_iter()
        .find(|name| tool_exists(name))
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
    let single = [
        (".gz", "gzip"),
        (".xz", "xz"),
        (".bz2", "bzip2"),
        (".zst", "zstd"),
    ]
    .into_iter()
    .find(|&(ext, _)| lower.ends_with(ext) && !lower.ends_with(&format!(".tar{ext}")));
    if let Some((_, program)) = single {
        if !tool_exists(program) {
            return Err(fail(format!("{program} is not installed")));
        }
        let target = unique(&parent.join(&stem));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| fail(e.to_string()))?;
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

/// Treat dangling symlinks as occupied directory entries too. `Path::exists()` follows
/// symlinks and returns false for a dangling link, which is unsafe for collision handling.
fn path_entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn unique(p: &Path) -> PathBuf {
    if !path_entry_exists(p) {
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
        .find(|candidate| !path_entry_exists(candidate))
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
    let metadata = fs::symlink_metadata(from)?;
    // Only real directories are recursively traversed. A directory symlink is copied as a
    // symlink, so self-links and links to ancestors cannot recurse indefinitely.
    if metadata.is_dir() && destination_inside_source(from, to) {
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
    let metadata = fs::symlink_metadata(from)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(from)?;
        return make_link(&target, to);
    }
    if metadata.is_dir() {
        // create_dir (not create_dir_all) fails rather than merging with an existing target.
        fs::create_dir(to)?;
        let result: std::io::Result<()> = (|| {
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                copy_recursive_unchecked(&entry.path(), &to.join(entry.file_name()))?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Best-effort rollback of the partial output. This assumes another process has not
            // concurrently written unrelated files inside the newly created destination tree.
            // remove_dir_all does not follow symlinks inside the copied tree.
            let _ = fs::remove_dir_all(to);
            return Err(error);
        }
        return Ok(());
    }
    if metadata.is_file() {
        let mut source = fs::File::open(from)?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        if let Err(error) = std::io::copy(&mut source, &mut destination) {
            drop(destination);
            let _ = fs::remove_file(to);
            return Err(error);
        }
        drop(destination);
        if let Err(error) = fs::set_permissions(to, metadata.permissions()) {
            let _ = fs::remove_file(to);
            return Err(error);
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "copying special filesystem object '{}' is not supported",
            from.display()
        ),
    ))
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
        if let Ok(value) = std::env::var(var)
            && !value.trim().is_empty()
        {
            candidates.push(value);
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
    open_terminal_with(dir, None)
}

/// Like [`open_terminal`], optionally running `script` (via `sh -c`) in the new terminal.
fn open_terminal_with(dir: &Path, script: Option<&str>) -> bool {
    // (program, args, how the command is passed). The directory is also set via
    // `current_dir`, but some emulators run inside a server process that ignores it, so
    // those get an explicit flag as well.
    let mut candidates: Vec<(String, Vec<String>, &str)> = Vec::new();
    if let Ok(term) = std::env::var("TERMINAL") {
        let mut parts = term.split_whitespace();
        if let Some(program) = parts.next() {
            candidates.push((program.to_string(), parts.map(str::to_string).collect(), "-e"));
        }
    }
    let wd = dir.display().to_string();
    for (name, args, exec) in [
        ("x-terminal-emulator", vec![], "-e"),
        ("gnome-terminal", vec![format!("--working-directory={wd}")], "--"),
        ("konsole", vec!["--workdir".to_string(), wd.clone()], "-e"),
        ("xfce4-terminal", vec![format!("--working-directory={wd}")], "-x"),
        ("mate-terminal", vec![format!("--working-directory={wd}")], "-x"),
        ("tilix", vec![format!("--working-directory={wd}")], "tilix"),
        ("lxterminal", vec![format!("--working-directory={wd}")], "-e"),
        ("alacritty", vec!["--working-directory".to_string(), wd.clone()], "-e"),
        ("kitty", vec!["--directory".to_string(), wd.clone()], ""),
        ("foot", vec![format!("--working-directory={wd}")], ""),
        ("xterm", vec![], "-e"),
        ("uxterm", vec![], "-e"),
    ] {
        candidates.push((name.to_string(), args, exec));
    }
    for (program, mut args, exec) in candidates {
        if let Some(script) = script {
            match exec {
                // tilix wants the command as one string.
                "tilix" => args.extend(["-e".to_string(), format!("sh -c '{script}'")]),
                flag => {
                    if !flag.is_empty() {
                        args.push(flag.to_string());
                    }
                    args.extend(["sh".to_string(), "-c".to_string(), script.to_string()]);
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "windowstyle-fm-app-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn partial_transfer_keeps_the_failure_status() {
        let temp = TestDir::new();
        let source = temp.0.join("source.txt");
        fs::write(&source, b"payload").expect("write source");
        let missing = temp.0.join("missing.txt");
        let destination = temp.0.join("destination");
        fs::create_dir(&destination).expect("create destination");

        let outcome = transfer_job(
            temp.0.clone(),
            vec![source.clone(), missing],
            destination.clone(),
            false,
            ConflictChoice::KeepBoth,
        );

        assert!(
            outcome
                .status
                .as_deref()
                .is_some_and(|status| status.contains("Copy failed"))
        );
        assert_eq!(outcome.undo.len(), 1);
        assert_eq!(
            fs::read(destination.join("source.txt")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn undo_rename_refuses_to_replace_an_existing_destination() {
        let temp = TestDir::new();
        let moved = temp.0.join("moved.txt");
        let original = temp.0.join("original.txt");
        fs::write(&moved, b"moved data").expect("write moved source");
        fs::write(&original, b"new user data").expect("write occupied destination");

        let result = run_undo_action(UndoAction::Rename {
            from: moved.clone(),
            to: original.clone(),
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&moved).unwrap(), b"moved data");
        assert_eq!(fs::read(&original).unwrap(), b"new user data");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_copy_preserves_directory_symlinks_without_following_them() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let source = temp.0.join("source");
        let destination = temp.0.join("copied");
        fs::create_dir(&source).expect("create source directory");
        fs::write(source.join("item.txt"), b"payload").expect("write source item");
        symlink(&source, source.join("loop")).expect("create self-referential directory symlink");

        copy_recursive(&source, &destination).expect("copy directory with symlink");

        let copied_link = destination.join("loop");
        assert!(
            fs::symlink_metadata(&copied_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(copied_link).unwrap(), source);
        assert_eq!(fs::read(destination.join("item.txt")).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn unique_treats_dangling_symlinks_as_occupied_names() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let occupied = temp.0.join("occupied");
        let second = temp.0.join("occupied (2)");
        symlink("missing-target", &occupied).expect("create dangling symlink");
        symlink("missing-target", &second).expect("create second dangling symlink");

        assert_eq!(unique(&occupied), temp.0.join("occupied (3)"));
    }

    #[test]
    fn lsblk_pairs_parse_quoted_values_and_empty_fields() {
        let f = parse_lsblk_pairs(
            r#"PATH="/dev/sdb1" FSTYPE="vfat" MOUNTPOINT="" TYPE="part" LABEL="MY USB""#,
        );
        assert_eq!(f.get("PATH").map(String::as_str), Some("/dev/sdb1"));
        assert_eq!(f.get("MOUNTPOINT").map(String::as_str), Some(""));
        assert_eq!(f.get("LABEL").map(String::as_str), Some("MY USB"));
    }

    #[test]
    fn unmounted_drive_entry_uses_placeholder_path() {
        let entries = this_pc_entries_from(vec![Drive {
            path: PathBuf::from(format!("{UNMOUNTED_DRIVE_PREFIX}/dev/sdb1")),
            name: "USB".into(),
            device: Some("/dev/sdb1".into()),
            mounted: false,
        }]);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].mounted);
        assert!(
            entries[0].is_dir(),
            "drives route through Navigate, not xdg-open"
        );
        assert!(
            entries[0]
                .path
                .to_str()
                .is_some_and(|s| s.starts_with(UNMOUNTED_DRIVE_PREFIX))
        );
    }

    #[test]
    fn drive_poll_setting_is_clamped_when_loaded_into_state() {
        let mut settings = Settings::default();
        settings.nav.drive_poll_secs = 1;
        settings.nav.this_pc_expanded = false;
        let mut state = ExplorerState::new();
        load_settings_into_state(&mut state, &settings);
        assert_eq!(state.drive_poll_secs, 5);
        assert!(!state.this_pc_expanded);
    }
}
