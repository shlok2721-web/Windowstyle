//! Data model: entries, commands, view state and the events the host receives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::icons::{Icon, icon_for_extension};
use crate::ribbon::ContextKind;

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// Virtual location used by hosts for the drive overview. It is not a real filesystem path.
pub const THIS_PC_LOCATION: &str = "this-pc://";

/// Virtual location used by hosts for the Quick access history view (visited folders on top,
/// opened files below). Like [`THIS_PC_LOCATION`] it is not a real filesystem path.
pub const QUICK_ACCESS_LOCATION: &str = "quick-access://";

/// A regular file with an execute bit (binary, AppImage, script with a shebang).
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Folder,
    File,
    Drive,
}

/// One item shown in the file list. The host fills these in from the real file
/// system (see `examples/demo.rs`); the UI never touches the disk itself.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// Lowercased `name`, computed once in [`Entry::new`]. Search filtering and
    /// type-ahead find use this instead of re-lowercasing `name` on every scan.
    pub name_lc: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// "Text Document", "File folder", ...
    pub type_name: String,
    pub hidden: bool,
    /// Optional override of the automatically chosen icon.
    pub icon: Option<Icon>,
    /// For drives: (total bytes, free bytes).
    pub space: Option<(u64, u64)>,
    /// For drives: the backing block device (`/dev/sda3`), if known. `None` for
    /// non-drive entries and for drives the host could not identify a device for.
    pub device: Option<String>,
    /// For drives: whether the device is currently mounted. Always `true` for non-drive
    /// entries. An unmounted drive has no usable `path` to browse into; mounting it is
    /// what makes one available.
    pub mounted: bool,
    /// For drives: whether the host can mount the device (it carries a recognised
    /// filesystem). Always `true` for non-drive entries and for already mounted drives.
    pub mountable: bool,
    /// When the item was last opened / visited. Only filled by hosts for history views
    /// (Quick access); `None` elsewhere.
    pub accessed: Option<SystemTime>,
    /// Optional thumbnail texture shown in the icon views instead of the vector icon.
    pub thumbnail: Option<egui::TextureId>,
    /// Pixel size (width, height) of `thumbnail`, so it can be drawn at its original aspect ratio.
    pub thumb_size: Option<(u32, u32)>,
}

impl Entry {
    pub fn new(path: impl Into<PathBuf>, kind: EntryKind) -> Self {
        let path = path.into();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let hidden = name.starts_with('.');
        let type_name = match kind {
            EntryKind::Folder => "File folder".to_owned(),
            EntryKind::Drive => "Local Disk".to_owned(),
            EntryKind::File => match extension(&name) {
                Some(e) => format!("{} File", e.to_uppercase()),
                None => "File".to_owned(),
            },
        };
        let name_lc = name.to_lowercase();
        Self {
            path,
            name,
            name_lc,
            kind,
            size: 0,
            modified: None,
            type_name,
            hidden,
            icon: None,
            space: None,
            device: None,
            mounted: true,
            mountable: true,
            accessed: None,
            thumbnail: None,
            thumb_size: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Folder | EntryKind::Drive)
    }

    pub fn icon(&self) -> Icon {
        if let Some(i) = self.icon {
            return i;
        }
        match self.kind {
            EntryKind::Folder => Icon::Folder,
            EntryKind::Drive => Icon::Drive,
            EntryKind::File => extension(&self.name)
                .map(icon_for_extension)
                .unwrap_or(Icon::File),
        }
    }

    /// Paint this entry's icon. A user-supplied `extension.<ext>.svg|png` icon from the icon
    /// directory (see `icons::paint_extension_icon`) wins over the built-in per-type icon.
    pub fn paint_icon(&self, painter: &egui::Painter, rect: egui::Rect, enabled: bool) {
        if self.icon.is_none()
            && self.kind == EntryKind::File
            && crate::icons::paint_extension_icon(&self.name_lc, painter, rect, enabled)
        {
            return;
        }
        self.icon().paint(painter, rect, enabled);
    }

    /// Name as displayed (honours "File name extensions").
    pub fn display_name(&self, show_extensions: bool) -> &str {
        if show_extensions || self.kind != EntryKind::File {
            return &self.name;
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 => &self.name[..i],
            _ => &self.name,
        }
    }
}

/// Suffixes recognised as archives / compressed files, longest compound suffixes first.
const ARCHIVE_SUFFIXES: [&str; 17] = [
    ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".txz", ".tbz2", ".tbz", ".tzst", ".tar",
    ".zip", ".7z", ".rar", ".gz", ".xz", ".bz2", ".zst",
];

/// The file name without its archive suffix (`"a.tar.gz"` -> `"a"`), or `None` when `name`
/// is not an archive name (or nothing would be left of it).
pub fn archive_stem(name: &str) -> Option<&str> {
    // All known suffixes are ASCII, so ASCII-insensitive comparison is sufficient and
    // avoids allocating a lowercased copy for every selected path.
    ARCHIVE_SUFFIXES
        .iter()
        .find_map(|suffix| {
            let start = name.len().checked_sub(suffix.len())?;
            let tail = name.get(start..)?;
            tail.eq_ignore_ascii_case(suffix).then_some(&name[..start])
        })
        .filter(|stem| !stem.is_empty())
}

/// Whether `name` looks like an archive or compressed file that can be extracted.
pub fn is_archive_name(name: &str) -> bool {
    archive_stem(name).is_some()
}

pub fn extension(name: &str) -> Option<&str> {
    let i = name.rfind('.')?;
    (i > 0 && i + 1 < name.len()).then(|| &name[i + 1..])
}

// ---------------------------------------------------------------------------
// View settings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ViewMode {
    ExtraLargeIcons,
    LargeIcons,
    MediumIcons,
    SmallIcons,
    List,
    Details,
    Tiles,
    Content,
}

impl ViewMode {
    pub const ALL: [ViewMode; 8] = [
        ViewMode::ExtraLargeIcons,
        ViewMode::LargeIcons,
        ViewMode::MediumIcons,
        ViewMode::SmallIcons,
        ViewMode::List,
        ViewMode::Details,
        ViewMode::Tiles,
        ViewMode::Content,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::ExtraLargeIcons => "Extra large icons",
            ViewMode::LargeIcons => "Large icons",
            ViewMode::MediumIcons => "Medium icons",
            ViewMode::SmallIcons => "Small icons",
            ViewMode::List => "List",
            ViewMode::Details => "Details",
            ViewMode::Tiles => "Tiles",
            ViewMode::Content => "Content",
        }
    }
    pub fn icon(self) -> Icon {
        match self {
            ViewMode::ExtraLargeIcons => Icon::ViewExtraLarge,
            ViewMode::LargeIcons => Icon::ViewLarge,
            ViewMode::MediumIcons => Icon::ViewMedium,
            ViewMode::SmallIcons => Icon::ViewSmall,
            ViewMode::List => Icon::ViewList,
            ViewMode::Details => Icon::ViewDetails,
            ViewMode::Tiles => Icon::ViewTiles,
            ViewMode::Content => Icon::ViewContent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKey {
    Name,
    DateModified,
    Type,
    Size,
    /// Only offered as a column in the Quick access history view.
    LastAccessed,
}

impl SortKey {
    pub const ALL: [SortKey; 4] = [
        SortKey::Name,
        SortKey::DateModified,
        SortKey::Type,
        SortKey::Size,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::DateModified => "Date modified",
            SortKey::Type => "Type",
            SortKey::Size => "Size",
            SortKey::LastAccessed => "Last accessed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortOrder {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupKey {
    None,
    Name,
    DateModified,
    Type,
    Size,
}

impl GroupKey {
    pub const ALL: [GroupKey; 5] = [
        GroupKey::None,
        GroupKey::Name,
        GroupKey::DateModified,
        GroupKey::Type,
        GroupKey::Size,
    ];
    pub fn label(self) -> &'static str {
        match self {
            GroupKey::None => "(None)",
            GroupKey::Name => "Name",
            GroupKey::DateModified => "Date modified",
            GroupKey::Type => "Type",
            GroupKey::Size => "Size",
        }
    }
}

/// How much of the ribbon is visible (Windows: "Minimize the Ribbon" +
/// Office-style "Auto-hide Ribbon").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RibbonMode {
    /// Tabs and commands always visible.
    Expanded,
    /// Only the tab strip is visible; clicking a tab shows the commands as an overlay.
    Minimized,
    /// Nothing is visible. Press `Alt` (or use the title-bar button) to reveal it temporarily.
    Hidden,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A ribbon / context-menu / shortcut command.
///
/// View-only commands are handled inside the crate (see
/// [`ExplorerState::apply_local`]); everything that touches the file system is
/// forwarded to the host as [`Event::Command`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    // Clipboard
    PinToQuickAccess,
    Copy,
    Paste,
    Cut,
    CopyPath,
    /// Copy the path of the folder that contains the selection (or the current folder).
    CopyFolderPath,
    PasteShortcut,
    // Organize
    MoveTo,
    CopyTo,
    Delete,
    DeletePermanently,
    Rename,
    // New
    NewFolder,
    NewTextFile,
    NewShortcut,
    // Open
    Open,
    OpenWith,
    /// Execute the selected file (it has an execute bit), detached from this window.
    Run,
    /// Execute the selected file inside a terminal.
    RunInTerminal,
    Edit,
    Properties,
    AdvancedSecurity,
    // Select
    SelectAll,
    SelectNone,
    InvertSelection,
    // Share
    Share,
    Email,
    Compress,
    /// Extract the selected archive(s) into a new folder named after each archive.
    Extract,
    /// Extract the selected archive(s) into the folder they are in.
    ExtractHere,
    Print,
    // View → panes
    ToggleNavPane,
    TogglePreviewPane,
    ToggleDetailsPane,
    // View → current view
    SetView(ViewMode),
    SortBy(SortKey),
    SetSortOrder(SortOrder),
    GroupBy(GroupKey),
    SizeColumnsToFit,
    ToggleColumn(SortKey),
    // View → show/hide
    ToggleItemCheckBoxes,
    ToggleFileExtensions,
    ToggleHiddenItems,
    HideSelectedItems,
    Options,
    // File menu
    OpenNewWindow,
    OpenTerminal,
    DeleteHistory,
    Help,
    Close,
    // Navigation
    Back,
    Forward,
    Up,
    Refresh,
    // Ribbon
    SetRibbonMode(RibbonMode),
    ToggleRibbon,
    ToggleQatPosition,
    Undo,
    Redo,
    // Contextual tabs
    /// Unmount the selected drive. Only ever offered for a single mounted drive.
    Eject,
    /// Mount the selected (currently unmounted) drive.
    MountDrive,
    Format,
    Cleanup,
    Optimize,
    Autoplay,
    SetAsBackground,
    RotateLeft,
    RotateRight,
    Slideshow,
    Play,
    RunAsAdmin,
    // Application maintenance and preferences, handled by the host.
    ToggleDarkMode,
    ToggleThumbnails,
    /// Keep the "This PC" node expanded in the navigation pane by default.
    ToggleThisPcExpanded,
    /// Set how often (seconds) the host rescans block devices.
    SetDrivePollSecs(u32),
    AuditThumbnails,
    GenerateMissingThumbnails,
    /// Run `sudo updatedb` in a terminal to refresh the plocate database.
    BuildIndex,
    /// Also search application-data folders (browser profiles, `.var`, ...).
    ToggleSearchAppData,
    // History window, trash handling, extra copy helpers
    ShowHistory,
    EmptyTrash,
    RestoreFromTrash,
    CopyName,
    CopyUrl,
    /// Copy the address bar's current location text (used by its right-click menu).
    CopyAddress,
    // Compression window + explicit-format compress
    ShowCompressOptions,
    CompressAs(CompressFormat, u8),
}

/// Archive formats offered by the compression window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompressFormat {
    Tar,
    Gzip,
    Xz,
    Zip,
    Zstd,
    Bzip2,
    SevenZip,
}

impl CompressFormat {
    pub const ALL: [CompressFormat; 7] = [
        CompressFormat::Tar,
        CompressFormat::Gzip,
        CompressFormat::Xz,
        CompressFormat::Zip,
        CompressFormat::Zstd,
        CompressFormat::Bzip2,
        CompressFormat::SevenZip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CompressFormat::Tar => "TAR (no compression)",
            CompressFormat::Gzip => "TAR.GZ (gzip)",
            CompressFormat::Xz => "TAR.XZ (xz)",
            CompressFormat::Zip => "ZIP",
            CompressFormat::Zstd => "TAR.ZST (zstd)",
            CompressFormat::Bzip2 => "TAR.BZ2 (bzip2)",
            CompressFormat::SevenZip => "7Z (LZMA2)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            CompressFormat::Tar => "tar",
            CompressFormat::Gzip => "tar.gz",
            CompressFormat::Xz => "tar.xz",
            CompressFormat::Zip => "zip",
            CompressFormat::Zstd => "tar.zst",
            CompressFormat::Bzip2 => "tar.bz2",
            CompressFormat::SevenZip => "7z",
        }
    }

    /// Whether a compression level applies to this format.
    pub fn uses_level(self) -> bool {
        self != CompressFormat::Tar
    }

    /// Whether archives of this format can be password protected (encrypted).
    pub fn supports_password(self) -> bool {
        matches!(self, CompressFormat::Zip | CompressFormat::SevenZip)
    }
}

/// The four simple access levels offered by the security dialog. They describe the
/// *owner's* access; see [`PermMode::apply_to_mode`] for how group/other are treated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PermMode {
    ReadOnly,
    WriteOnly,
    #[default]
    ReadWrite,
    /// Read + execute (a program or script that can be run but not edited).
    Executable,
}

impl PermMode {
    pub const ALL: [PermMode; 4] = [
        PermMode::ReadOnly,
        PermMode::WriteOnly,
        PermMode::ReadWrite,
        PermMode::Executable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PermMode::ReadOnly => "Read only",
            PermMode::WriteOnly => "Write only",
            PermMode::ReadWrite => "Read & write",
            PermMode::Executable => "Executable (read & run)",
        }
    }

    /// The `rwx` bits (as one octal digit) the owner gets.
    pub fn bits(self) -> u32 {
        match self {
            PermMode::ReadOnly => 0o4,
            PermMode::WriteOnly => 0o2,
            PermMode::ReadWrite => 0o6,
            PermMode::Executable => 0o5,
        }
    }

    /// Compute the new Unix mode from `old`. The owner gets exactly this level; group and
    /// others keep only the bits they already had that are also part of this level, so
    /// applying a level can never grant anyone *more* access than before. Setuid, setgid
    /// and sticky bits are preserved.
    pub fn apply_to_mode(self, old: u32) -> u32 {
        let want = self.bits();
        let owner = want << 6;
        let group = old & 0o070 & (want << 3);
        let other = old & 0o007 & want;
        (old & !0o777) | owner | group | other
    }

    /// The level that exactly matches the owner bits of `mode`, if any.
    pub fn from_mode(mode: u32) -> Option<PermMode> {
        match (mode >> 6) & 0o7 {
            0o4 => Some(PermMode::ReadOnly),
            0o2 => Some(PermMode::WriteOnly),
            0o6 => Some(PermMode::ReadWrite),
            0o5 => Some(PermMode::Executable),
            _ => None,
        }
    }
}

/// Small floating info windows that are rendered by the UI crate itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoWindow {
    History,
    Properties,
    Help,
    Compress,
    Security,
}

// ---------------------------------------------------------------------------
// Navigation pane model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct NavNode {
    pub path: PathBuf,
    pub label: String,
    pub icon: Icon,
    /// `None` = children not loaded yet (a chevron is shown and
    /// [`Event::LoadChildren`] is emitted on first expansion).
    pub children: Option<Vec<NavNode>>,
    pub expanded: bool,
    /// Show the little pin overlay (Quick access entries).
    pub pinned: bool,
    /// Leaf nodes never show a chevron.
    pub leaf: bool,
}

impl NavNode {
    pub fn new(path: impl Into<PathBuf>, label: impl Into<String>, icon: Icon) -> Self {
        Self {
            path: path.into(),
            label: label.into(),
            icon,
            children: None,
            expanded: false,
            pinned: false,
            leaf: false,
        }
    }
    pub fn with_children(mut self, children: Vec<NavNode>) -> Self {
        self.children = Some(children);
        self
    }
    pub fn expanded(mut self) -> Self {
        self.expanded = true;
        self
    }
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self.leaf = true;
        self
    }
    pub fn find_mut(&mut self, path: &Path) -> Option<&mut NavNode> {
        if self.path == path {
            return Some(self);
        }
        self.children
            .as_mut()?
            .iter_mut()
            .find_map(|c| c.find_mut(path))
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Things the host application has to react to.
#[derive(Clone, Debug)]
pub enum Event {
    /// Show this folder (the host reads it and calls [`ExplorerState::set_location`]).
    Navigate(PathBuf),
    /// Double-click / Enter on a file.
    Open(PathBuf),
    /// A non-view command applied to the current selection (or the current folder when empty).
    Command { cmd: Command, targets: Vec<PathBuf> },
    /// Inline rename finished.
    Rename { path: PathBuf, new_name: String },
    /// The security dialog asked to give these files an access level.
    SetPermissions {
        targets: Vec<PathBuf>,
        mode: PermMode,
    },
    /// Search box submitted.
    Search(String),
    /// A visible image/video needs a thumbnail; the host deduplicates work.
    ThumbnailNeeded(PathBuf),
    /// A navigation-pane node was expanded and has no children yet.
    LoadChildren(PathBuf),
    /// The user dropped `sources` onto folder `target`.
    Drop {
        sources: Vec<PathBuf>,
        target: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Column configuration (Details view)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Column {
    pub key: SortKey,
    pub width: f32,
    pub visible: bool,
}

/// Which entries the item pane lists. Only the Quick access history view narrows it: it
/// draws its folders and its files as two panes, each with a different view mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListScope {
    #[default]
    All,
    FoldersOnly,
    FilesOnly,
}

/// The view parameters that the Quick access history view temporarily replaces.
struct ViewStash {
    view: ViewMode,
    sort_key: SortKey,
    sort_order: SortOrder,
    group: GroupKey,
    folders_first: bool,
    columns: Vec<Column>,
}

pub struct RenameState {
    pub path: PathBuf,
    pub text: String,
    pub just_started: bool,
}

// ---------------------------------------------------------------------------
// Explorer state
// ---------------------------------------------------------------------------

pub struct ExplorerState {
    // --- content ---
    pub location: PathBuf,
    pub title: String,
    pub entries: Vec<Entry>,
    pub nav: Vec<NavNode>,
    pub history: Vec<PathBuf>,
    pub history_cursor: usize,
    // --- view ---
    pub view: ViewMode,
    pub sort_key: SortKey,
    pub sort_order: SortOrder,
    pub group: GroupKey,
    pub columns: Vec<Column>,
    pub folders_first: bool,
    /// View settings parked while the Quick access history view is on screen.
    history_stash: Option<Box<ViewStash>>,
    /// Narrows the listing to folders or files (set only while the history panes are drawn).
    pub scope: ListScope,
    // --- selection ---
    pub selection: HashSet<PathBuf>,
    pub focus: Option<PathBuf>,
    pub anchor: Option<PathBuf>,
    pub cut: HashSet<PathBuf>,
    pub clipboard_has_items: bool,
    /// Host sets these to enable the Undo / Redo buttons.
    pub can_undo: bool,
    pub can_redo: bool,
    pub renaming: Option<RenameState>,
    /// Currently open History / Properties / Help window, if any.
    pub info_window: Option<InfoWindow>,
    /// Choices remembered by the compression window.
    pub compress_format: CompressFormat,
    pub compress_level: u8,
    /// Access level currently picked in the security dialog.
    pub perm_choice: PermMode,
    /// Password typed into the compression window (and its "repeat" field). Never persisted;
    /// the host takes `compress_password` when it handles `CompressAs`.
    pub compress_password: String,
    pub compress_password2: String,
    // --- toggles ---
    pub show_nav_pane: bool,
    pub show_preview_pane: bool,
    pub show_details_pane: bool,
    pub item_check_boxes: bool,
    pub show_extensions: bool,
    pub show_hidden: bool,
    /// On-demand image/video thumbnails while browsing.
    pub thumbnails_enabled: bool,
    /// Interface-only dark palette.
    pub dark_mode: bool,
    /// Icon size (px) in the navigation pane; rows grow with it. Host-supplied from settings.
    pub nav_icon_size: f32,
    /// Host-supplied: whether file search also looks inside application-data folders.
    pub search_app_data: bool,
    /// Host-supplied: the listing currently holds deep-search results (kept in rank order).
    pub search_active: bool,
    /// Host-supplied: whether "This PC" starts expanded in the nav pane.
    pub this_pc_expanded: bool,
    /// Host-supplied: drive rescan interval in seconds (shown in the ribbon).
    pub drive_poll_secs: u32,
    /// Latest host operation status, retained in the GUI status bar.
    pub host_status: Option<String>,
    // --- ribbon ---
    pub ribbon_mode: RibbonMode,
    pub active_tab: String,
    pub ribbon_overlay: bool,
    pub qat_below_ribbon: bool,
    pub qat_items: Vec<Command>,
    pub key_tips: bool,
    /// Tapping Alt may show the key tips (File: F, Home: H ...). Set `false` to disable.
    pub key_tips_enabled: bool,
    // --- address / search ---
    pub address_editing: bool,
    pub address_text: String,
    pub search_text: String,
    pub collapsed_groups: HashSet<String>,
    // --- misc ---
    pub window_focused: bool,
    pub date_format: fn(SystemTime) -> String,
    /// Used by the breadcrumb drop-downs: list the sub-folders of a path.
    pub subfolders_fn: Option<fn(&Path) -> Vec<PathBuf>>,
    pub(crate) events: Vec<Event>,
    /// Bumped whenever `entries` is replaced or edited through this type's own
    /// API (see [`ExplorerState::invalidate_view`]). One input of the
    /// derived-view cache signature.
    pub(crate) entries_rev: u64,
    /// Memoised filter/sort/group/selection results. `Mutex` (not `RefCell`)
    /// so the state stays `Send + Sync`; it is never contended.
    pub(crate) view_cache: Mutex<ViewCache>,
}

impl Default for ExplorerState {
    fn default() -> Self {
        Self {
            location: PathBuf::from("/"),
            title: "/".into(),
            entries: Vec::new(),
            nav: Vec::new(),
            history: vec![PathBuf::from("/")],
            history_cursor: 0,
            view: ViewMode::Details,
            sort_key: SortKey::Name,
            sort_order: SortOrder::Ascending,
            group: GroupKey::None,
            columns: vec![
                Column {
                    key: SortKey::Name,
                    width: 260.0,
                    visible: true,
                },
                Column {
                    key: SortKey::DateModified,
                    width: 140.0,
                    visible: true,
                },
                Column {
                    key: SortKey::Type,
                    width: 120.0,
                    visible: true,
                },
                Column {
                    key: SortKey::Size,
                    width: 80.0,
                    visible: true,
                },
            ],
            folders_first: true,
            history_stash: None,
            scope: ListScope::All,
            selection: HashSet::new(),
            focus: None,
            anchor: None,
            cut: HashSet::new(),
            clipboard_has_items: false,
            can_undo: false,
            can_redo: false,
            renaming: None,
            info_window: None,
            compress_format: CompressFormat::Xz,
            compress_level: 6,
            perm_choice: PermMode::ReadWrite,
            compress_password: String::new(),
            compress_password2: String::new(),
            show_nav_pane: true,
            show_preview_pane: false,
            show_details_pane: false,
            item_check_boxes: false,
            show_extensions: true,
            show_hidden: false,
            thumbnails_enabled: true,
            dark_mode: false,
            nav_icon_size: 18.0,
            search_app_data: false,
            search_active: false,
            this_pc_expanded: true,
            drive_poll_secs: 10,
            host_status: None,
            ribbon_mode: RibbonMode::Expanded,
            active_tab: "home".into(),
            ribbon_overlay: false,
            qat_below_ribbon: false,
            qat_items: vec![
                Command::Properties,
                Command::NewFolder,
                Command::Undo,
                Command::Redo,
            ],
            key_tips: false,
            key_tips_enabled: true,
            address_editing: false,
            address_text: String::new(),
            search_text: String::new(),
            collapsed_groups: HashSet::new(),
            window_focused: true,
            date_format: format_date_utc,
            subfolders_fn: None,
            events: Vec::new(),
            entries_rev: 0,
            view_cache: Mutex::new(ViewCache::default()),
        }
    }
}

impl ExplorerState {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- host API ------------------------------------------------------

    /// Entering the Quick access history switches to a Details view sorted by last access
    /// (newest first) with a "Last accessed" column; leaving it brings the previous view,
    /// sort and columns back, so browsing history never disturbs a folder's own view.
    fn switch_history_view(&mut self, next: &Path) {
        let entering = next == Path::new(QUICK_ACCESS_LOCATION) && !self.in_quick_access();
        let leaving = self.in_quick_access() && next != Path::new(QUICK_ACCESS_LOCATION);
        if entering {
            let columns = vec![
                Column {
                    key: SortKey::Name,
                    width: 300.0,
                    visible: true,
                },
                Column {
                    key: SortKey::LastAccessed,
                    width: 150.0,
                    visible: true,
                },
                Column {
                    key: SortKey::DateModified,
                    width: 140.0,
                    visible: true,
                },
                Column {
                    key: SortKey::Type,
                    width: 120.0,
                    visible: true,
                },
                Column {
                    key: SortKey::Size,
                    width: 80.0,
                    visible: true,
                },
            ];
            self.history_stash = Some(Box::new(ViewStash {
                view: self.view,
                sort_key: self.sort_key,
                sort_order: self.sort_order,
                group: self.group,
                folders_first: self.folders_first,
                columns: std::mem::replace(&mut self.columns, columns),
            }));
            self.view = ViewMode::Details;
            self.sort_key = SortKey::LastAccessed;
            self.sort_order = SortOrder::Descending;
            self.group = GroupKey::None;
            // Visited folders on top, opened files below.
            self.folders_first = true;
        } else if leaving && let Some(stash) = self.history_stash.take() {
            self.view = stash.view;
            self.sort_key = stash.sort_key;
            self.sort_order = stash.sort_order;
            self.group = stash.group;
            self.folders_first = stash.folders_first;
            self.columns = stash.columns;
        }
    }

    /// Tell the UI which folder is now displayed and what it contains.
    /// Maintains the Back/Forward history automatically.
    pub fn set_location(&mut self, path: impl Into<PathBuf>, entries: Vec<Entry>) {
        let path = path.into();
        if self.history.get(self.history_cursor) != Some(&path) {
            self.history.truncate(self.history_cursor + 1);
            self.history.push(path.clone());
            self.history_cursor = self.history.len() - 1;
        }
        self.switch_history_view(&path);
        self.title = if path == Path::new(THIS_PC_LOCATION) {
            "This PC".to_owned()
        } else if path == Path::new(QUICK_ACCESS_LOCATION) {
            "Quick access".to_owned()
        } else {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        };
        self.location = path;
        self.entries = entries;
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.selection.clear();
        self.focus = None;
        self.anchor = None;
        self.renaming = None;
        self.search_text.clear();
        self.search_active = false;
        self.address_editing = false;
    }

    /// Replace the entries without touching history/selection (refresh).
    ///
    /// Also reconciles everything that refers to a path by identity rather
    /// than by index: entries that disappeared in this refresh are dropped
    /// from the selection and cut set, and a focus/anchor/rename that pointed
    /// at a now-gone entry is cleared rather than left dangling. Without this,
    /// a rename target that vanished mid-edit could stay "in progress"
    /// forever, since nothing in the UI matches its stored path anymore.
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        let alive: HashSet<&PathBuf> = entries.iter().map(|e| &e.path).collect();
        self.selection.retain(|p| alive.contains(p));
        self.cut.retain(|p| alive.contains(p));
        if self.focus.as_ref().is_some_and(|p| !alive.contains(p)) {
            self.focus = None;
        }
        if self.anchor.as_ref().is_some_and(|p| !alive.contains(p)) {
            self.anchor = None;
        }
        if self
            .renaming
            .as_ref()
            .is_some_and(|rn| !alive.contains(&rn.path))
        {
            self.renaming = None;
        }
        self.entries = entries;
        self.entries_rev = self.entries_rev.wrapping_add(1);
    }

    pub fn set_nav_children(&mut self, parent: &Path, children: Vec<NavNode>) {
        for root in &mut self.nav {
            if let Some(n) = root.find_mut(parent) {
                n.children = Some(children);
                return;
            }
        }
    }

    /// Drain pending events (usually done via [`crate::Explorer::show`]'s return value).
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn emit(&mut self, e: Event) {
        self.events.push(e);
    }

    // ---- history ---------------------------------------------------------

    /// Whether the folder on screen is a virtual overview (This PC, Quick access) rather than
    /// a real directory: it has no parent and nothing can be created in it.
    pub fn in_virtual_location(&self) -> bool {
        self.location == Path::new(THIS_PC_LOCATION)
            || self.location == Path::new(QUICK_ACCESS_LOCATION)
    }

    /// Whether the Quick access history view is on screen.
    pub fn in_quick_access(&self) -> bool {
        self.location == Path::new(QUICK_ACCESS_LOCATION)
    }

    pub fn can_go_back(&self) -> bool {
        self.history_cursor > 0
    }
    pub fn can_go_forward(&self) -> bool {
        self.history_cursor + 1 < self.history.len()
    }
    pub fn can_go_up(&self) -> bool {
        !self.in_virtual_location() && self.location.parent().is_some()
    }
    pub fn go_back(&mut self) {
        if self.can_go_back() {
            self.history_cursor -= 1;
            let p = self.history[self.history_cursor].clone();
            self.emit(Event::Navigate(p));
        }
    }
    pub fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.history_cursor += 1;
            let p = self.history[self.history_cursor].clone();
            self.emit(Event::Navigate(p));
        }
    }
    pub fn go_up(&mut self) {
        if self.in_virtual_location() {
            return;
        }
        if let Some(p) = self.location.parent() {
            let p = p.to_path_buf();
            self.emit(Event::Navigate(p));
        }
    }
    pub fn navigate(&mut self, p: impl Into<PathBuf>) {
        self.emit(Event::Navigate(p.into()));
    }

    // ---- derived data ----------------------------------------------------
    //
    // Everything below is memoised in `view_cache`. The cache never relies on
    // callers *telling* us something changed (most inputs are public fields
    // that hosts and tests assign directly); instead each lookup compares a
    // cheap signature of every input with the one the cached value was built
    // from, and rebuilds on any difference. The one thing a signature cannot
    // see is an in-place edit of an `Entry`'s fields: do that through
    // [`ExplorerState::entries_mut`] or call [`ExplorerState::invalidate_view`]
    // afterwards. Replacing the whole list (`set_entries`, `set_location`) and
    // pushing/removing entries are detected automatically.

    /// Drop every cached derived value. Call after editing fields of existing
    /// `entries` in place (size, modified, hidden, name, ...). Not needed after
    /// `set_location` / `set_entries`, or when only adding/removing entries.
    pub fn invalidate_view(&mut self) {
        self.entries_rev = self.entries_rev.wrapping_add(1);
    }

    /// Mutable access to the entry list that also invalidates the cached view,
    /// so in-place edits can never leave a stale ordering behind.
    pub fn entries_mut(&mut self) -> &mut Vec<Entry> {
        self.invalidate_view();
        &mut self.entries
    }

    fn lock_cache(&self) -> MutexGuard<'_, ViewCache> {
        // A poisoned lock only means another thread panicked mid-rebuild; the
        // signature check below makes the half-built value harmless.
        self.view_cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entries_sig(&self) -> (u64, usize, usize) {
        (
            self.entries_rev,
            self.entries.len(),
            self.entries.as_ptr() as usize,
        )
    }

    /// Rebuild the filtered + sorted index list if (and only if) one of its
    /// inputs changed since it was last built.
    fn refresh_order(&self, c: &mut ViewCache) {
        let (rev, len, ptr) = self.entries_sig();
        let desc = self.sort_order == SortOrder::Descending;
        let fresh = c.ord.as_ref().is_some_and(|s| {
            s.rev == rev
                && s.len == len
                && s.ptr == ptr
                && s.show_hidden == self.show_hidden
                && s.sort_key == self.sort_key
                && s.desc == desc
                && s.folders_first == self.folders_first
                && s.scope == self.scope
                && s.search == self.search_text
        });
        if fresh {
            return;
        }
        c.order = Arc::new(self.compute_order());
        c.order_gen = c.order_gen.wrapping_add(1);
        match c.ord.as_mut() {
            Some(s) => {
                s.rev = rev;
                s.len = len;
                s.ptr = ptr;
                s.show_hidden = self.show_hidden;
                s.sort_key = self.sort_key;
                s.desc = desc;
                s.folders_first = self.folders_first;
                s.scope = self.scope;
                s.search.clear();
                s.search.push_str(&self.search_text);
            }
            None => {
                c.ord = Some(OrderSig {
                    rev,
                    len,
                    ptr,
                    show_hidden: self.show_hidden,
                    sort_key: self.sort_key,
                    desc,
                    folders_first: self.folders_first,
                    scope: self.scope,
                    search: self.search_text.clone(),
                });
            }
        }
    }

    /// The uncached filter + sort. Only called from `refresh_order`.
    fn compute_order(&self) -> Vec<usize> {
        let needle = self.search_text.to_lowercase();
        let mut indices: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_hidden || !e.hidden)
            .filter(|(_, e)| match self.scope {
                ListScope::All => true,
                ListScope::FoldersOnly => e.is_dir(),
                ListScope::FilesOnly => !e.is_dir(),
            })
            .filter(|(_, e)| needle.is_empty() || e.name_lc.contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.search_active {
            // Deep-search results arrive ranked (folders first, shallow first): keep that order.
            return indices;
        }
        let entries = &self.entries;
        let key = self.sort_key;
        let desc = self.sort_order == SortOrder::Descending;
        let folders_first = self.folders_first;

        if key == SortKey::Type {
            // Decorate candidates once, then sort the candidate list directly. This
            // avoids the previous extra 0..n permutation vector and final remapping.
            let mut typed: Vec<(usize, String)> = indices
                .into_iter()
                .map(|i| (i, entries[i].type_name.to_lowercase()))
                .collect();
            typed.sort_by(|(ia, ta), (ib, tb)| {
                let (a, b) = (&entries[*ia], &entries[*ib]);
                if folders_first && a.is_dir() != b.is_dir() {
                    return b.is_dir().cmp(&a.is_dir());
                }
                // Dot-items always sort after normal ones, in either direction.
                let (da, db) = (a.name.starts_with('.'), b.name.starts_with('.'));
                if da != db {
                    return da.cmp(&db);
                }
                let ord = ta.cmp(tb).then_with(|| natural_cmp(&a.name, &b.name));
                if desc { ord.reverse() } else { ord }
            });
            return typed.into_iter().map(|(i, _)| i).collect();
        }

        // Sorting indices in place preserves the stable tie ordering while avoiding
        // an additional permutation allocation for Name, Date modified, and Size.
        indices.sort_by(|&ia, &ib| {
            let (a, b) = (&entries[ia], &entries[ib]);
            if folders_first && a.is_dir() != b.is_dir() {
                return b.is_dir().cmp(&a.is_dir());
            }
            // Dot-items always sort after normal ones, in either direction.
            let (da, db) = (a.name.starts_with('.'), b.name.starts_with('.'));
            if da != db {
                return da.cmp(&db);
            }
            let ord = match key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::DateModified => a.modified.cmp(&b.modified),
                SortKey::Size => a
                    .size
                    .cmp(&b.size)
                    .then_with(|| natural_cmp(&a.name, &b.name)),
                SortKey::LastAccessed => a
                    .accessed
                    .cmp(&b.accessed)
                    .then_with(|| natural_cmp(&a.name, &b.name)),
                // The Type branch returns above.
                SortKey::Type => a.type_name.cmp(&b.type_name),
            };
            if desc { ord.reverse() } else { ord }
        });
        indices
    }

    /// Shared, cached display order (indices into `entries`). Cheap to call
    /// repeatedly within a frame: a hit is a signature compare and an `Arc`
    /// clone.
    pub(crate) fn order_arc(&self) -> Arc<Vec<usize>> {
        let mut c = self.lock_cache();
        self.refresh_order(&mut c);
        Arc::clone(&c.order)
    }

    /// Indices into `entries` in display order (filtered, sorted).
    ///
    /// Returns an owned copy; internal code uses the shared cached list.
    pub fn visible_order(&self) -> Vec<usize> {
        self.order_arc().as_ref().clone()
    }

    /// Number of items currently shown (O(1) once the view is cached).
    pub fn visible_count(&self) -> usize {
        let mut c = self.lock_cache();
        self.refresh_order(&mut c);
        c.order.len()
    }

    /// The display order split into group buckets (one unlabelled bucket when
    /// grouping is off or the view is List). Cached together with the order.
    pub(crate) fn group_set(&self) -> Arc<GroupSet> {
        let mut c = self.lock_cache();
        self.refresh_order(&mut c);
        // List never groups; `GroupKey::None` makes the signature independent
        // of view mode and sort direction so switching between ungrouped
        // views reuses the cached set.
        let key = if self.group == GroupKey::None || self.view == ViewMode::List {
            GroupKey::None
        } else {
            self.group
        };
        let desc = key != GroupKey::None && self.sort_order == SortOrder::Descending;
        // Date grouping is the only view-derived value whose labels age with wall
        // time; avoid clock reads for all other grouping modes.
        let now = if key == GroupKey::DateModified {
            SystemTime::now()
        } else {
            UNIX_EPOCH
        };
        let sig = GroupSig {
            order_gen: c.order_gen,
            key,
            desc,
        };
        if let Some(cache) = &c.groups
            && cache.sig == sig
            && now >= cache.built_at
            && cache.valid_until.is_none_or(|deadline| now < deadline)
        {
            return Arc::clone(&cache.value);
        }
        let set = Arc::new(self.build_group_set(&c.order, key, desc, now));
        let valid_until = if key == GroupKey::DateModified {
            self.next_date_group_change(&c.order, now)
        } else {
            None
        };
        c.groups = Some(GroupCache {
            sig,
            value: Arc::clone(&set),
            built_at: now,
            valid_until,
        });
        set
    }

    /// Earliest instant at which a visible entry can move to another date group.
    /// Only the label boundaries matter (1, 2, 7, 14, 31, 61, and 366 days), so
    /// old files do not force daily regrouping when their label cannot change.
    fn next_date_group_change(&self, order: &[usize], now: SystemTime) -> Option<SystemTime> {
        const LABEL_BOUNDARIES_DAYS: [u64; 7] = [1, 2, 7, 14, 31, 61, 366];

        order
            .iter()
            .filter_map(|&i| self.entries[i].modified)
            .filter_map(|modified| {
                let next_boundary_days = if now < modified {
                    // Future timestamps remain in the "Today" bucket until one
                    // full day after the timestamp, matching `group_label`.
                    1
                } else {
                    let age_days = now.duration_since(modified).ok()?.as_secs() / 86_400;
                    LABEL_BOUNDARIES_DAYS
                        .into_iter()
                        .find(|&boundary| boundary > age_days)?
                };
                modified
                    .checked_add(Duration::from_secs(next_boundary_days * 86_400))
                    .filter(|deadline| *deadline > now)
            })
            .min()
    }

    fn build_group_set(
        &self,
        order: &Arc<Vec<usize>>,
        key: GroupKey,
        desc: bool,
        now: SystemTime,
    ) -> GroupSet {
        if key == GroupKey::None {
            return GroupSet {
                flat: Arc::clone(order),
                groups: vec![GroupInfo {
                    label: String::new(),
                    ukey: String::new(),
                    off: 0,
                    len: order.len(),
                }],
            };
        }
        // Decorate-sort-undecorate: rank each entry once instead of
        // re-allocating lowercase labels on both sides of every comparison.
        // `sort_by` is stable, so equal ranks keep display order.
        let mut v: Vec<((usize, String), String, usize)> = order
            .iter()
            .map(|&i| {
                let label = group_label(&self.entries[i], key, now);
                (group_rank(&label, key), label, i)
            })
            .collect();
        v.sort_by(|a, b| {
            let o = a.0.cmp(&b.0);
            if desc { o.reverse() } else { o }
        });
        let mut flat = Vec::with_capacity(v.len());
        let mut groups: Vec<GroupInfo> = Vec::new();
        for (_, label, i) in v {
            if groups.last().is_some_and(|g| g.label == label) {
                if let Some(g) = groups.last_mut() {
                    g.len += 1;
                }
            } else {
                groups.push(GroupInfo {
                    ukey: group_ui_key(key, &label),
                    label,
                    off: flat.len(),
                    len: 1,
                });
            }
            flat.push(i);
        }
        GroupSet {
            flat: Arc::new(flat),
            groups,
        }
    }

    /// Longest display name (in chars) among the visible items; sizes the
    /// List view's columns. Cached per display order and extension setting.
    pub(crate) fn longest_name_chars(&self) -> usize {
        let mut c = self.lock_cache();
        self.refresh_order(&mut c);
        if let Some((g, ext, v)) = c.longest
            && g == c.order_gen
            && ext == self.show_extensions
        {
            return v;
        }
        let v = c
            .order
            .iter()
            .map(|&i| {
                self.entries[i]
                    .display_name(self.show_extensions)
                    .chars()
                    .count()
            })
            .max()
            .unwrap_or(10);
        c.longest = Some((c.order_gen, self.show_extensions, v));
        v
    }

    /// Count, file bytes, first entry and ribbon context of the selection.
    ///
    /// Computed in a single pass over `entries` and memoised against a
    /// fingerprint of the selection set, so the status bar, side pane and
    /// ribbon share one result instead of each re-scanning the folder. Costs
    /// O(1) when nothing is selected and O(selected) per call otherwise.
    pub(crate) fn selection_summary(&self) -> SelSummary {
        if self.selection.is_empty() {
            return SelSummary::default();
        }
        let (rev, len, ptr) = self.entries_sig();
        let sig = SelSig {
            rev,
            len,
            ptr,
            sel_len: self.selection.len(),
            fp: selection_fingerprint(&self.selection),
        };
        let mut c = self.lock_cache();
        if let Some((s, sum)) = &c.sel
            && *s == sig
        {
            return *sum;
        }
        let mut sum = SelSummary::default();
        let mut ctx = CtxAcc::Unset;
        for (i, e) in self.entries.iter().enumerate() {
            if !self.selection.contains(&e.path) {
                continue;
            }
            if sum.count == 0 {
                sum.first = Some(i);
            }
            sum.count += 1;
            if e.kind == EntryKind::Drive {
                sum.has_drive = true;
            }
            if e.kind == EntryKind::File {
                sum.file_count += 1;
                sum.file_bytes = sum.file_bytes.saturating_add(e.size);
            }
            ctx = match (ctx, e.kind) {
                (CtxAcc::Bad, _) | (_, EntryKind::Folder | EntryKind::Drive) => CtxAcc::Bad,
                (acc, EntryKind::File) => match (acc, crate::ribbon::file_context(e)) {
                    (_, None) => CtxAcc::Bad,
                    (CtxAcc::Unset, Some(k)) => CtxAcc::Kind(k),
                    (CtxAcc::Kind(p), Some(k)) if p == k => CtxAcc::Kind(p),
                    _ => CtxAcc::Bad,
                },
            };
        }
        let first_is_drive = sum
            .first
            .is_some_and(|i| self.entries[i].kind == EntryKind::Drive);
        sum.single_drive_mounted = if sum.count == 1 && first_is_drive {
            sum.first.map(|i| self.entries[i].mounted)
        } else {
            None
        };
        sum.single_drive_mountable = if sum.count == 1 && first_is_drive {
            sum.first.is_some_and(|i| self.entries[i].mountable)
        } else {
            false
        };
        sum.context = if sum.count == 1 && first_is_drive {
            Some(ContextKind::Drive)
        } else if let CtxAcc::Kind(k) = ctx {
            Some(k)
        } else {
            None
        };
        c.sel = Some((sig, sum));
        sum
    }

    /// Whether any selected item is named like an archive (zip, tar.gz, 7z, ...).
    pub fn selection_has_archive(&self) -> bool {
        self.selection.iter().any(|p| {
            p.file_name()
                .is_some_and(|n| is_archive_name(&n.to_string_lossy()))
        })
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        if self.selection.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|e| self.selection.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    pub fn selected_entries(&self) -> Vec<&Entry> {
        if self.selection.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|e| self.selection.contains(&e.path))
            .collect()
    }

    pub fn select_only(&mut self, p: &Path) {
        self.selection.clear();
        self.selection.insert(p.to_path_buf());
        self.focus = Some(p.to_path_buf());
        self.anchor = Some(p.to_path_buf());
    }

    pub fn is_checked(&self, cmd: Command) -> Option<bool> {
        Some(match cmd {
            Command::ToggleNavPane => self.show_nav_pane,
            Command::TogglePreviewPane => self.show_preview_pane,
            Command::ToggleDetailsPane => self.show_details_pane,
            Command::ToggleItemCheckBoxes => self.item_check_boxes,
            Command::ToggleFileExtensions => self.show_extensions,
            Command::ToggleHiddenItems => self.show_hidden,
            Command::ToggleThumbnails => self.thumbnails_enabled,
            Command::ToggleDarkMode => self.dark_mode,
            Command::ToggleSearchAppData => self.search_app_data,
            Command::ToggleThisPcExpanded => self.this_pc_expanded,
            Command::SetDrivePollSecs(n) => self.drive_poll_secs == n,
            Command::SetView(v) => self.view == v,
            Command::SortBy(k) => self.sort_key == k,
            Command::SetSortOrder(o) => self.sort_order == o,
            Command::GroupBy(g) => self.group == g,
            Command::ToggleColumn(k) => self
                .columns
                .iter()
                .find(|c| c.key == k)
                .map(|c| c.visible)
                .unwrap_or(false),
            Command::SetRibbonMode(m) => self.ribbon_mode == m,
            Command::ToggleQatPosition => self.qat_below_ribbon,
            _ => return None,
        })
    }

    /// True while the Trash folder is the current location.
    pub fn in_trash(&self) -> bool {
        self.location.ends_with("Trash/files")
    }

    /// Whether a command can currently run (used to grey out buttons).
    pub fn is_enabled(&self, cmd: Command) -> bool {
        let n = self.selection.len();
        // Neither virtual overview is a real folder: nothing can be created or pasted there.
        let in_this_pc = self.location == Path::new(THIS_PC_LOCATION)
            || self.location == Path::new(QUICK_ACCESS_LOCATION);
        // A drive entry denotes an entire mounted volume. Never offer operations
        // that would recursively copy, move, rename, hide, compress, or delete it.
        if n > 0
            && matches!(
                cmd,
                Command::Copy
                    | Command::Cut
                    | Command::MoveTo
                    | Command::CopyTo
                    | Command::Delete
                    | Command::DeletePermanently
                    | Command::Rename
                    | Command::HideSelectedItems
                    | Command::Share
                    | Command::Email
                    | Command::Compress
                    | Command::ShowCompressOptions
                    | Command::CompressAs(..)
                    | Command::Print
                    | Command::OpenWith
                    | Command::Run
                    | Command::RunInTerminal
                    | Command::Edit
            )
            && self.selection_summary().has_drive
        {
            return false;
        }
        if in_this_pc
            && matches!(
                cmd,
                Command::Paste
                    | Command::PasteShortcut
                    | Command::NewFolder
                    | Command::NewTextFile
                    | Command::NewShortcut
                    | Command::OpenTerminal
            )
        {
            return false;
        }
        match cmd {
            Command::Back => self.can_go_back(),
            Command::Forward => self.can_go_forward(),
            Command::Up => self.can_go_up(),
            // `clipboard_has_items` only reflects Windowstyle's own in-app clipboard.
            // Files copied externally (e.g. `wl-copy`, another file manager) only live in
            // the system clipboard, which this crate can't see — so Paste must stay
            // enabled and the host (fm_app) falls back to reading the system clipboard
            // when the in-app one is empty, reporting "Nothing to paste" if both are.
            Command::Paste | Command::PasteShortcut => true,
            Command::Copy
            | Command::Cut
            | Command::CopyPath
            | Command::CopyFolderPath
            | Command::MoveTo
            | Command::CopyTo
            | Command::Delete
            | Command::DeletePermanently
            | Command::HideSelectedItems
            | Command::Share
            | Command::Email
            | Command::Compress
            | Command::ShowCompressOptions
            | Command::CompressAs(..)
            | Command::Print
            | Command::PinToQuickAccess
            | Command::Open
            | Command::OpenWith
            | Command::Edit => n > 0,
            Command::Run | Command::RunInTerminal => {
                n == 1
                    && self
                        .selected_entries()
                        .first()
                        .is_some_and(|e| e.kind == EntryKind::File && is_executable_file(&e.path))
            }
            Command::Rename => n == 1,
            Command::CopyName | Command::CopyUrl => n > 0,
            Command::RestoreFromTrash => n > 0 && self.in_trash(),
            Command::EmptyTrash => self.in_trash(),
            Command::Undo => self.can_undo,
            Command::Redo => self.can_redo,
            Command::Extract | Command::ExtractHere => self.selection_has_archive(),
            Command::Eject => self.selection_summary().single_drive_mounted == Some(true),
            Command::MountDrive => {
                let sum = self.selection_summary();
                sum.single_drive_mounted == Some(false) && sum.single_drive_mountable
            }
            _ => true,
        }
    }

    /// Handle view-only commands. Returns `true` when consumed.
    pub fn apply_local(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::ShowHistory => self.info_window = Some(InfoWindow::History),
            Command::Properties => self.info_window = Some(InfoWindow::Properties),
            Command::AdvancedSecurity => self.info_window = Some(InfoWindow::Security),
            Command::Help => self.info_window = Some(InfoWindow::Help),
            Command::ShowCompressOptions => self.info_window = Some(InfoWindow::Compress),
            Command::ToggleNavPane => self.show_nav_pane ^= true,
            Command::TogglePreviewPane => {
                self.show_preview_pane ^= true;
                if self.show_preview_pane {
                    self.show_details_pane = false;
                }
            }
            Command::ToggleDetailsPane => {
                self.show_details_pane ^= true;
                if self.show_details_pane {
                    self.show_preview_pane = false;
                }
            }
            Command::SetView(v) => self.view = v,
            Command::SortBy(k) => self.sort_key = k,
            Command::SetSortOrder(o) => self.sort_order = o,
            Command::GroupBy(g) => self.group = g,
            Command::ToggleColumn(k) => {
                if let Some(c) = self.columns.iter_mut().find(|c| c.key == k) {
                    c.visible ^= true;
                }
            }
            Command::SizeColumnsToFit => {
                for c in &mut self.columns {
                    c.width = match c.key {
                        SortKey::Name => 240.0,
                        SortKey::DateModified => 140.0,
                        SortKey::Type => 110.0,
                        SortKey::Size => 80.0,
                        SortKey::LastAccessed => 150.0,
                    };
                }
            }
            Command::ToggleItemCheckBoxes => self.item_check_boxes ^= true,
            Command::ToggleFileExtensions => self.show_extensions ^= true,
            Command::ToggleHiddenItems => self.show_hidden ^= true,
            Command::HideSelectedItems => {
                let sel = self.selected_paths();
                self.emit(Event::Command { cmd, targets: sel });
                // the host persists the hidden flag; we hide immediately
                for e in &mut self.entries {
                    if self.selection.contains(&e.path) {
                        e.hidden = true;
                    }
                }
                // in-place edit of a filter input: drop the cached view
                self.invalidate_view();
                // If hidden items are currently filtered out of the list, the
                // entries we just hid are about to disappear from view. Drop
                // them from the selection (and repair focus/anchor) so the
                // status bar, ribbon and side pane don't keep reporting on an
                // invisible selection. With "show hidden" on, the items stay
                // visible, so the selection is left untouched.
                if !self.show_hidden {
                    let newly_hidden = std::mem::take(&mut self.selection);
                    if self
                        .focus
                        .as_ref()
                        .is_some_and(|p| newly_hidden.contains(p))
                    {
                        self.focus = None;
                    }
                    if self
                        .anchor
                        .as_ref()
                        .is_some_and(|p| newly_hidden.contains(p))
                    {
                        self.anchor = None;
                    }
                }
            }
            Command::SelectAll => {
                let order = self.order_arc();
                self.selection = order
                    .iter()
                    .map(|&i| self.entries[i].path.clone())
                    .collect();
            }
            Command::SelectNone => self.selection.clear(),
            Command::InvertSelection => {
                let order = self.order_arc();
                let all: Vec<PathBuf> = order
                    .iter()
                    .map(|&i| self.entries[i].path.clone())
                    .collect();
                self.selection = all
                    .into_iter()
                    .filter(|p| !self.selection.contains(p))
                    .collect();
            }
            Command::Back => self.go_back(),
            Command::Forward => self.go_forward(),
            Command::Up => self.go_up(),
            Command::Refresh => {
                let p = self.location.clone();
                self.emit(Event::Navigate(p));
            }
            Command::SetRibbonMode(m) => {
                self.ribbon_mode = m;
                self.ribbon_overlay = false;
            }
            Command::ToggleRibbon => {
                self.ribbon_mode = if self.ribbon_mode == RibbonMode::Expanded {
                    RibbonMode::Minimized
                } else {
                    RibbonMode::Expanded
                };
                self.ribbon_overlay = false;
            }
            Command::ToggleQatPosition => self.qat_below_ribbon ^= true,
            Command::Cut => {
                self.cut = self.selection.clone();
                self.clipboard_has_items = !self.cut.is_empty();
                let targets = self.selected_paths();
                self.emit(Event::Command { cmd, targets });
            }
            Command::Copy => {
                self.cut.clear();
                self.clipboard_has_items = !self.selection.is_empty();
                let targets = self.selected_paths();
                self.emit(Event::Command { cmd, targets });
            }
            Command::Rename => {
                if let Some(e) = self.selected_entries().first() {
                    let (path, text) = (e.path.clone(), e.name.clone());
                    self.renaming = Some(RenameState {
                        path,
                        text,
                        just_started: true,
                    });
                }
            }
            _ => return false,
        }
        true
    }

    /// Run a command: local ones are applied, the rest become host events.
    pub fn dispatch(&mut self, cmd: Command) {
        if !self.is_enabled(cmd) {
            return;
        }
        if self.apply_local(cmd) {
            return;
        }
        let targets = self.selected_paths();
        if cmd == Command::Open && targets.len() == 1 {
            let is_dir = self.selected_entries().first().is_some_and(|e| e.is_dir());
            let p = targets[0].clone();
            self.emit(if is_dir {
                Event::Navigate(p)
            } else {
                Event::Open(p)
            });
            return;
        }
        self.emit(Event::Command { cmd, targets });
    }
}

// ---------------------------------------------------------------------------
// Derived-view cache types
// ---------------------------------------------------------------------------

/// Inputs the cached display order was built from.
pub(crate) struct OrderSig {
    rev: u64,
    len: usize,
    ptr: usize,
    show_hidden: bool,
    sort_key: SortKey,
    desc: bool,
    folders_first: bool,
    scope: ListScope,
    search: String,
}

#[derive(PartialEq, Eq)]
pub(crate) struct GroupSig {
    order_gen: u64,
    key: GroupKey,
    desc: bool,
}

pub(crate) struct GroupCache {
    sig: GroupSig,
    value: Arc<GroupSet>,
    built_at: SystemTime,
    valid_until: Option<SystemTime>,
}

#[derive(PartialEq, Eq)]
pub(crate) struct SelSig {
    rev: u64,
    len: usize,
    ptr: usize,
    sel_len: usize,
    fp: (u64, u64),
}

/// Memoised derived data of an [`ExplorerState`]; see its "derived data"
/// section for the validity rules.
#[derive(Default)]
pub(crate) struct ViewCache {
    ord: Option<OrderSig>,
    order: Arc<Vec<usize>>,
    /// Incremented on every order rebuild; keys the data derived from it.
    order_gen: u64,
    groups: Option<GroupCache>,
    longest: Option<(u64, bool, usize)>,
    sel: Option<(SelSig, SelSummary)>,
}

/// One group bucket: a contiguous run of `GroupSet::flat`.
pub(crate) struct GroupInfo {
    pub label: String,
    /// Collapse-state / widget identity: the grouping mode plus the label, so
    /// identical labels under different modes never share state.
    pub ukey: String,
    pub off: usize,
    pub len: usize,
}

/// The visible items bucketed by group, in display order.
pub(crate) struct GroupSet {
    /// Entry indices; each group is the slice `off..off + len`.
    pub flat: Arc<Vec<usize>>,
    pub groups: Vec<GroupInfo>,
}

impl GroupSet {
    /// True when group headers should be shown (anything but the single
    /// unlabelled bucket).
    pub fn grouped(&self) -> bool {
        !(self.groups.len() == 1 && self.groups[0].label.is_empty())
    }
}

/// What the status bar, side pane and ribbon need to know about the selection.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SelSummary {
    pub count: usize,
    pub file_count: usize,
    pub file_bytes: u64,
    /// Index (into `entries`) of the first selected entry in list order.
    pub first: Option<usize>,
    pub context: Option<ContextKind>,
    /// True if any selected item represents a volume root rather than a normal file/folder.
    pub has_drive: bool,
    /// `Some(mounted)` when exactly one drive is selected; `None` otherwise (nothing
    /// selected, more than one item, or the single selection is not a drive).
    pub single_drive_mounted: Option<bool>,
    /// For a single selected drive: whether the host can mount it (see [`Entry::mountable`]).
    pub single_drive_mountable: bool,
}

impl SelSummary {
    /// The selected entry when exactly one is selected.
    pub fn single<'a>(&self, entries: &'a [Entry]) -> Option<&'a Entry> {
        if self.count == 1 {
            self.first.and_then(|i| entries.get(i))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
enum CtxAcc {
    Unset,
    Kind(ContextKind),
    Bad,
}

/// Order-independent 128-bit-ish fingerprint of a set of paths: cheap enough
/// to take every frame (O(selected), a few ns per byte) and, unlike a
/// revision counter, immune to hosts assigning `state.selection` directly.
fn selection_fingerprint(sel: &HashSet<PathBuf>) -> (u64, u64) {
    let (mut a, mut b) = (0u64, 0u64);
    for p in sel {
        let h = fast_hash(p.as_os_str().as_encoded_bytes());
        a = a.wrapping_add(mix64(h));
        b ^= mix64(h ^ 0xA5A5_A5A5_5A5A_5A5A).rotate_left(21);
    }
    (a, b)
}

fn fast_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (bytes.len() as u64) ^ 0xCBF2_9CE4_8422_2325;
    for c in bytes.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(K);
    }
    h
}

fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Collapse-state/widget identity for a group header: combines the grouping
/// mode with the label so two groups that happen to render the same label
/// under different grouping modes are never confused with each other.
pub(crate) fn group_ui_key(key: GroupKey, label: &str) -> String {
    format!("{key:?}\u{0}{label}")
}

/// Sort rank of a group label: fixed buckets for Size/Date, alphabetical
/// (case-insensitive) otherwise.
pub(crate) fn group_rank(label: &str, key: GroupKey) -> (usize, String) {
    const SIZE: &[&str] = &[
        "Unspecified",
        "Empty",
        "Tiny (0 - 16 KB)",
        "Small (16 KB - 1 MB)",
        "Medium (1 - 128 MB)",
        "Large (128 MB - 1 GB)",
        "Huge (1 - 4 GB)",
        "Gigantic (> 4 GB)",
    ];
    const DATE: &[&str] = &[
        "Today",
        "Yesterday",
        "Earlier this week",
        "Last week",
        "Earlier this month",
        "Last month",
        "Earlier this year",
        "A long time ago",
        "Unspecified",
    ];
    match key {
        GroupKey::Size => (
            SIZE.iter().position(|s| *s == label).unwrap_or(99),
            String::new(),
        ),
        GroupKey::DateModified => (
            DATE.iter().position(|s| *s == label).unwrap_or(99),
            String::new(),
        ),
        _ => (0, label.to_lowercase()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Windows-style "natural" ordering: case-insensitive, numbers compared by value.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // Walk UTF-8 character boundaries, but compare ASCII digit runs as byte
    // slices. Numeric runs are compared by significant length then lexically,
    // so arbitrarily long numbers remain correctly ordered without overflow or
    // allocating temporary strings in the sort comparator.
    let (mut ia, mut ib) = (0usize, 0usize);
    while ia < a.len() && ib < b.len() {
        let x = a[ia..]
            .chars()
            .next()
            .expect("index is a character boundary");
        let y = b[ib..]
            .chars()
            .next()
            .expect("index is a character boundary");
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (sa, sb) = (ia, ib);
            while ia < a.len() && a.as_bytes()[ia].is_ascii_digit() {
                ia += 1;
            }
            while ib < b.len() && b.as_bytes()[ib].is_ascii_digit() {
                ib += 1;
            }
            let (ra, rb) = (&a[sa..ia], &b[sb..ib]);
            let (na, nb) = (ra.trim_start_matches('0'), rb.trim_start_matches('0'));
            match na.len().cmp(&nb.len()).then_with(|| na.cmp(nb)) {
                Ordering::Equal => {}
                ord => return ord,
            }
        } else {
            let (lx, ly) = (
                x.to_lowercase().next().unwrap_or(x),
                y.to_lowercase().next().unwrap_or(y),
            );
            match lx.cmp(&ly) {
                Ordering::Equal => {
                    ia += x.len_utf8();
                    ib += y.len_utf8();
                }
                ord => return ord,
            }
        }
    }
    match (ia == a.len(), ib == b.len()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => Ordering::Equal,
    }
}

/// "4.2 KB" style size (Windows Explorer uses KB for everything >= 1 KiB).
pub fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 KB".into();
    }
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    if v < 1.0 {
        return "1 KB".into();
    }
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} KB", v.ceil() as u64)
    } else if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[u])
    } else if v >= 10.0 {
        format!("{:.1} {}", v, UNITS[u])
    } else {
        format!("{:.2} {}", v, UNITS[u])
    }
}

/// "10/14/2026 4:19 PM" (UTC; override `ExplorerState::date_format` for local time).
pub fn format_date_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    let (h, mi) = (sod / 3600, (sod % 3600) / 60);
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{m}/{d}/{y} {h12}:{mi:02} {ap}")
}

/// Days since epoch -> (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Group bucket label for an entry.
pub fn group_label(e: &Entry, key: GroupKey, now: SystemTime) -> String {
    match key {
        GroupKey::None => String::new(),
        GroupKey::Name => {
            let c = e
                .name
                .chars()
                .next()
                .unwrap_or('#')
                .to_uppercase()
                .next()
                .unwrap_or('#');
            if c.is_alphabetic() {
                c.to_string()
            } else {
                "&".into()
            }
        }
        GroupKey::Type => e.type_name.clone(),
        GroupKey::Size => {
            if e.is_dir() {
                return "Unspecified".into();
            }
            match e.size {
                0 => "Empty",
                1..=16_384 => "Tiny (0 - 16 KB)",
                16_385..=1_048_576 => "Small (16 KB - 1 MB)",
                1_048_577..=134_217_728 => "Medium (1 - 128 MB)",
                134_217_729..=1_073_741_824 => "Large (128 MB - 1 GB)",
                1_073_741_825..=4_294_967_296 => "Huge (1 - 4 GB)",
                _ => "Gigantic (> 4 GB)",
            }
            .into()
        }
        GroupKey::DateModified => {
            let Some(t) = e.modified else {
                return "Unspecified".into();
            };
            let age = now.duration_since(t).unwrap_or(Duration::ZERO).as_secs() / 86_400;
            match age {
                0 => "Today",
                1 => "Yesterday",
                2..=6 => "Earlier this week",
                7..=13 => "Last week",
                14..=30 => "Earlier this month",
                31..=60 => "Last month",
                61..=365 => "Earlier this year",
                _ => "A long time ago",
            }
            .into()
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn file(name: &str, size: u64) -> Entry {
        let mut e = Entry::new(format!("/d/{name}"), EntryKind::File);
        e.size = size;
        e
    }

    fn state() -> ExplorerState {
        let mut st = ExplorerState::new();
        let mut v = vec![
            file("b.txt", 30),
            file("a.png", 10),
            file("c.rs", 20),
            file(".h", 5),
        ];
        v.push(Entry::new("/d/dir", EntryKind::Folder));
        st.set_location("/d", v);
        st
    }

    /// The cached order must always equal a from-scratch computation.
    fn assert_fresh(st: &ExplorerState, why: &str) {
        assert_eq!(st.visible_order(), st.compute_order(), "{why}");
        assert_eq!(st.visible_count(), st.compute_order().len(), "{why}");
    }

    #[test]
    fn quick_access_switches_the_view_and_restores_it_on_leaving() {
        let mut st = ExplorerState::new();
        st.set_location("/home/user", vec![]);
        st.view = ViewMode::LargeIcons;
        st.sort_key = SortKey::Size;
        let columns_before = st.columns.len();

        st.set_location(QUICK_ACCESS_LOCATION, vec![]);
        assert_eq!(st.title, "Quick access");
        assert!(st.in_quick_access() && !st.can_go_up());
        assert_eq!(st.view, ViewMode::Details);
        assert_eq!(st.sort_key, SortKey::LastAccessed);
        assert_eq!(st.sort_order, SortOrder::Descending);
        assert!(st.columns.iter().any(|c| c.key == SortKey::LastAccessed));

        st.set_location("/home/user", vec![]);
        assert_eq!(st.view, ViewMode::LargeIcons);
        assert_eq!(st.sort_key, SortKey::Size);
        assert_eq!(st.columns.len(), columns_before);
        assert!(!st.columns.iter().any(|c| c.key == SortKey::LastAccessed));
    }

    #[test]
    fn unmountable_unmounted_drive_cannot_be_mounted() {
        let mut st = ExplorerState::new();
        let mut drive = Entry::new("device:///dev/sdb2", EntryKind::Drive);
        drive.mounted = false;
        drive.mountable = false;
        st.set_location(THIS_PC_LOCATION, vec![drive.clone()]);
        st.select_only(&drive.path);
        assert!(!st.is_enabled(Command::MountDrive));
        let mut mountable = drive.clone();
        mountable.mountable = true;
        st.set_location(THIS_PC_LOCATION, vec![mountable.clone()]);
        st.select_only(&mountable.path);
        assert!(st.is_enabled(Command::MountDrive));
    }

    #[test]
    fn this_pc_is_a_virtual_root_without_a_parent() {
        let mut st = ExplorerState::new();
        st.set_location(THIS_PC_LOCATION, vec![]);
        assert_eq!(st.title, "This PC");
        assert!(!st.can_go_up());
        st.go_up();
        assert!(st.take_events().is_empty());
    }

    #[test]
    fn drive_roots_cannot_be_copied_or_deleted_as_recursive_items() {
        let mut st = ExplorerState::new();
        let mut drive = Entry::new("/", EntryKind::Drive);
        drive.name = "Drive C:".into();
        st.set_location(THIS_PC_LOCATION, vec![drive]);
        st.selection.insert(PathBuf::from("/"));
        assert!(st.is_enabled(Command::Open));
        assert!(st.is_enabled(Command::CopyPath));
        assert!(!st.is_enabled(Command::Copy));
        assert!(!st.is_enabled(Command::Cut));
        assert!(!st.is_enabled(Command::Delete));
        assert!(!st.is_enabled(Command::DeletePermanently));
        st.dispatch(Command::Delete);
        assert!(st.take_events().is_empty());
    }

    #[test]
    fn this_pc_disables_filesystem_creation_and_paste() {
        let mut st = ExplorerState::new();
        st.set_location(THIS_PC_LOCATION, vec![]);
        st.clipboard_has_items = true;
        for cmd in [
            Command::Paste,
            Command::PasteShortcut,
            Command::NewFolder,
            Command::NewTextFile,
            Command::NewShortcut,
            Command::OpenTerminal,
        ] {
            assert!(!st.is_enabled(cmd), "{cmd:?} must be disabled on This PC");
        }
    }

    #[test]
    fn natural_sort_handles_unbounded_numeric_runs() {
        assert!(
            natural_cmp(
                "file999999999999999999999999999999999999",
                "file1000000000000000000000000000000000000"
            )
            .is_lt()
        );
        assert_eq!(natural_cmp("file0002", "file2"), std::cmp::Ordering::Equal);
        assert!(natural_cmp("a2b", "a2c").is_lt());
        assert!(natural_cmp("a2٢", "a22").is_lt());
    }

    #[test]
    fn type_sort_keeps_folders_first_and_uses_case_insensitive_type_then_name() {
        let mut directory = Entry::new("/d/dir", EntryKind::Folder);
        directory.type_name = "File folder".into();
        let mut z = file("z.rs", 1);
        z.type_name = "Text".into();
        let mut a = file("a.txt", 1);
        a.type_name = "text".into();
        let mut st = ExplorerState::new();
        st.set_entries(vec![z, directory, a]);
        st.sort_key = SortKey::Type;
        st.sort_order = SortOrder::Ascending;
        let names = |st: &ExplorerState| {
            st.visible_order()
                .into_iter()
                .map(|i| st.entries[i].name.clone())
                .collect::<Vec<String>>()
        };
        assert_eq!(names(&st), vec!["dir", "a.txt", "z.rs"]);
        st.sort_order = SortOrder::Descending;
        assert_eq!(names(&st), vec!["dir", "z.rs", "a.txt"]);
    }

    #[test]
    fn type_sort_preserves_input_order_when_type_and_name_tie() {
        let mut first = Entry::new("/first/shared.txt", EntryKind::File);
        first.type_name = "Text".into();
        let mut second = Entry::new("/second/shared.txt", EntryKind::File);
        second.type_name = "text".into();
        let mut st = ExplorerState::new();
        st.set_entries(vec![first, second]);
        st.sort_key = SortKey::Type;

        let paths = |st: &ExplorerState| {
            st.visible_order()
                .into_iter()
                .map(|i| st.entries[i].path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&st), vec!["/first/shared.txt", "/second/shared.txt"]);
        st.sort_order = SortOrder::Descending;
        assert_eq!(paths(&st), vec!["/first/shared.txt", "/second/shared.txt"]);
    }

    #[test]
    fn group_cache_is_reused_when_its_inputs_are_unchanged() {
        let mut st = state();
        st.group = GroupKey::Name;
        let first = st.group_set();
        let second = st.group_set();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn date_group_deadline_is_the_next_actual_age_boundary() {
        let mut st = ExplorerState::new();
        let modified = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut e = file("dated.txt", 1);
        e.modified = Some(modified);
        st.set_entries(vec![e]);
        let order = [0usize];
        let before = modified + Duration::from_secs(86_399);
        assert_eq!(
            st.next_date_group_change(&order, before),
            Some(modified + Duration::from_secs(86_400))
        );
        let yesterday_boundary = modified + Duration::from_secs(86_400);
        assert_eq!(
            st.next_date_group_change(&order, yesterday_boundary),
            Some(modified + Duration::from_secs(172_800))
        );
        // The label remains "Earlier this week" across ages 2 through 6;
        // regrouping is only needed when the entry reaches age 7 days.
        let earlier_this_week = modified + Duration::from_secs(2 * 86_400);
        assert_eq!(
            st.next_date_group_change(&order, earlier_this_week),
            Some(modified + Duration::from_secs(7 * 86_400))
        );
    }

    #[test]
    fn archive_stem_matches_case_insensitively_without_suffix_confusion() {
        assert_eq!(archive_stem("package.TAR.GZ"), Some("package"));
        assert_eq!(archive_stem("archive.zip"), Some("archive"));
        assert_eq!(archive_stem(".tar.gz"), None);
        assert_eq!(archive_stem("document.txt"), None);
    }

    #[test]
    fn state_stays_send_and_sync() {
        fn ok<T: Send + Sync>() {}
        ok::<ExplorerState>();
    }

    #[test]
    fn order_tracks_every_input() {
        let mut st = state();
        assert_fresh(&st, "initial");
        let before = st.visible_order();
        assert_eq!(
            st.visible_order(),
            before,
            "second call is a pure cache hit"
        );

        st.show_hidden = true;
        assert_fresh(&st, "show_hidden");
        assert_eq!(st.visible_count(), 5);
        st.sort_key = SortKey::Size;
        assert_fresh(&st, "sort key");
        st.sort_order = SortOrder::Descending;
        assert_fresh(&st, "sort order");
        st.folders_first = false;
        assert_fresh(&st, "folders_first");
        st.search_text = "A".into();
        assert_fresh(&st, "search");
        assert_eq!(
            st.visible_count(),
            1,
            "case-insensitive: only a.png contains an a"
        );
        st.search_text.clear();
        assert_fresh(&st, "search cleared");
    }

    #[test]
    fn order_tracks_entry_list_changes() {
        let mut st = state();
        let n = st.visible_count();
        st.entries.push(file("z.txt", 1)); // direct push: detected via length
        assert_eq!(st.visible_count(), n + 1);
        assert_fresh(&st, "push");
        st.entries.pop();
        assert_fresh(&st, "pop");
        st.set_entries(vec![file("only", 1)]);
        assert_eq!(st.visible_order(), vec![0]);
        st.set_location("/e", vec![]);
        assert_eq!(st.visible_count(), 0);
        st.entries = vec![file("x", 1), file("y", 2)]; // direct replacement
        assert_fresh(&st, "direct assignment");
        assert_eq!(st.visible_count(), 2);
    }

    #[test]
    fn in_place_edits_need_entries_mut_and_get_it() {
        let mut st = state();
        st.sort_key = SortKey::Size;
        st.folders_first = false;
        let before = st.visible_order();
        // make the smallest file the largest, through the invalidating accessor
        let smallest = before[0];
        st.entries_mut()[smallest].size = 1_000;
        assert_fresh(&st, "entries_mut edit");
        assert_ne!(
            st.visible_order(),
            before,
            "order must change after the edit"
        );
        assert_eq!(*st.visible_order().last().unwrap(), smallest);
        // and the explicit hook for hosts that edit through the public field
        st.entries[smallest].size = 0;
        st.invalidate_view();
        assert_fresh(&st, "invalidate_view");
        assert_eq!(st.visible_order()[0], smallest);
    }

    #[test]
    fn hide_selected_items_updates_view_and_selection() {
        let mut st = state();
        st.select_only(Path::new("/d/b.txt"));
        let n = st.visible_count();
        st.dispatch(Command::HideSelectedItems);
        assert_eq!(
            st.visible_count(),
            n - 1,
            "hidden entry leaves the cached view at once"
        );
        assert_fresh(&st, "after hide");
        assert!(st.selection.is_empty());
    }

    #[test]
    fn groups_follow_key_view_and_order() {
        let mut st = state();
        st.view = ViewMode::MediumIcons;
        let flat_ungrouped = st.group_set();
        assert!(!flat_ungrouped.grouped());
        st.group = GroupKey::Type;
        let g = st.group_set();
        assert!(g.grouped() && g.groups.len() > 1);
        assert_eq!(
            g.groups.iter().map(|x| x.len).sum::<usize>(),
            st.visible_count()
        );
        st.view = ViewMode::List;
        assert!(!st.group_set().grouped(), "List never groups");
        st.view = ViewMode::Details;
        st.sort_order = SortOrder::Descending;
        let desc = st.group_set();
        let asc_labels: Vec<_> = g.groups.iter().map(|x| x.label.clone()).collect();
        let mut rev: Vec<_> = desc.groups.iter().map(|x| x.label.clone()).collect();
        rev.reverse();
        assert_eq!(asc_labels, rev, "descending reverses group order");
        st.search_text = "zzz".into();
        assert_eq!(st.group_set().flat.len(), 0, "filter propagates to groups");
    }

    #[test]
    fn longest_name_follows_extensions_and_filter() {
        let mut st = ExplorerState::new();
        st.set_entries(vec![file("a.txt", 0), file("a-much-longer-name.md", 0)]);
        assert_eq!(st.longest_name_chars(), "a-much-longer-name.md".len());
        st.show_extensions = false;
        assert_eq!(st.longest_name_chars(), "a-much-longer-name".len());
        st.search_text = "a.t".into();
        assert_eq!(st.longest_name_chars(), 1);
        st.search_text = "nomatch".into();
        assert_eq!(
            st.longest_name_chars(),
            10,
            "empty view keeps the old default"
        );
    }

    #[test]
    fn selection_summary_tracks_selection_and_entries() {
        let mut st = ExplorerState::new();
        let mut img = file("p.png", 100);
        img.size = 100;
        st.set_entries(vec![
            img,
            file("q.png", 50),
            file("r.txt", 7),
            Entry::new("/d/dir", EntryKind::Folder),
        ]);
        let s = st.selection_summary();
        assert_eq!((s.count, s.context), (0, None));

        st.select_only(Path::new("/d/p.png"));
        let s = st.selection_summary();
        assert_eq!((s.count, s.file_count, s.file_bytes), (1, 1, 100));
        assert_eq!(s.context, Some(ContextKind::Picture));
        assert_eq!(s.single(&st.entries).unwrap().name, "p.png");

        st.selection.insert(PathBuf::from("/d/q.png")); // direct field write
        let s = st.selection_summary();
        assert_eq!(
            (s.count, s.file_bytes, s.context),
            (2, 150, Some(ContextKind::Picture))
        );
        assert!(s.single(&st.entries).is_none());

        st.selection.insert(PathBuf::from("/d/r.txt"));
        assert_eq!(
            st.selection_summary().context,
            None,
            "mixed kinds: no contextual tab"
        );
        st.selection.remove(Path::new("/d/r.txt"));
        st.selection.insert(PathBuf::from("/d/dir"));
        let s = st.selection_summary();
        assert_eq!(
            (s.count, s.file_count, s.context),
            (3, 2, None),
            "a folder disables context"
        );

        // selection naming a vanished path must not be counted
        st.selection = [PathBuf::from("/d/gone")].into();
        assert_eq!(st.selection_summary().count, 0);

        // same selection, new entries -> recomputed
        st.select_only(Path::new("/d/p.png"));
        assert_eq!(st.selection_summary().file_bytes, 100);
        let mut bigger = file("p.png", 999);
        bigger.size = 999;
        st.set_entries(vec![bigger]);
        assert_eq!(st.selection_summary().file_bytes, 999);
        st.entries_mut()[0].size = 5;
        assert_eq!(st.selection_summary().file_bytes, 5);
    }

    #[test]
    fn drive_selection_gives_drive_context() {
        let mut st = ExplorerState::new();
        st.set_entries(vec![
            Entry::new("/mnt", EntryKind::Drive),
            Entry::new("/mnt2", EntryKind::Drive),
        ]);
        st.select_only(Path::new("/mnt"));
        assert_eq!(st.selection_summary().context, Some(ContextKind::Drive));
        st.selection.insert(PathBuf::from("/mnt2"));
        assert_eq!(
            st.selection_summary().context,
            None,
            "two drives: no contextual tab"
        );
    }
}

#[cfg(test)]
mod nav_preference_tests {
    use super::*;

    #[test]
    fn this_pc_expanded_reports_checked_state() {
        let mut st = ExplorerState::new();
        st.this_pc_expanded = false;
        assert_eq!(st.is_checked(Command::ToggleThisPcExpanded), Some(false));
        st.this_pc_expanded = true;
        assert_eq!(st.is_checked(Command::ToggleThisPcExpanded), Some(true));
    }

    #[test]
    fn drive_poll_choice_is_checked_only_for_current_interval() {
        let mut st = ExplorerState::new();
        st.drive_poll_secs = 10;
        assert_eq!(st.is_checked(Command::SetDrivePollSecs(10)), Some(true));
        assert_eq!(st.is_checked(Command::SetDrivePollSecs(5)), Some(false));
    }

    #[test]
    fn new_preference_commands_are_always_enabled() {
        let st = ExplorerState::new();
        assert!(st.is_enabled(Command::ToggleThisPcExpanded));
        assert!(st.is_enabled(Command::SetDrivePollSecs(20)));
    }

    #[test]
    fn perm_modes_set_owner_bits_and_never_widen_access() {
        // rw-r--r--
        assert_eq!(PermMode::ReadOnly.apply_to_mode(0o100644), 0o100444);
        assert_eq!(PermMode::WriteOnly.apply_to_mode(0o100644), 0o100200);
        assert_eq!(PermMode::ReadWrite.apply_to_mode(0o100644), 0o100644);
        // rwxr-xr-x -> owner r-x, group r-x, other r-x
        assert_eq!(PermMode::Executable.apply_to_mode(0o100755), 0o100555);
        // group/other do not gain execute from a plain rw file
        assert_eq!(PermMode::Executable.apply_to_mode(0o100644), 0o100544);
        // setuid is preserved
        assert_eq!(PermMode::ReadOnly.apply_to_mode(0o104755), 0o104444);
    }

    #[test]
    fn perm_mode_round_trips_from_owner_bits() {
        for m in PermMode::ALL {
            assert_eq!(PermMode::from_mode(m.apply_to_mode(0o100777)), Some(m));
        }
        assert_eq!(PermMode::from_mode(0o100000), None);
    }
}
