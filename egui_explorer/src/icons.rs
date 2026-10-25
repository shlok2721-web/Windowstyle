//! Vector icons drawn with the egui painter (no image assets needed).
//!
//! Every icon is authored on a 16x16 grid and scaled to the requested rect, so
//! the same icon works for the 16px small buttons, 32px large ribbon buttons and
//! the 48..96px thumbnails in the "Large icons" views.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use egui::epaint::StrokeKind;
use egui::{
    Color32, ColorImage, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, TextureHandle,
    TextureOptions, vec2,
};

const BLUE: Color32 = Color32::from_rgb(0, 120, 215);
const DBLUE: Color32 = Color32::from_rgb(0, 84, 153);
const LBLUE: Color32 = Color32::from_rgb(153, 209, 255);
const GREEN: Color32 = Color32::from_rgb(16, 150, 60);
const RED: Color32 = Color32::from_rgb(214, 40, 40);
const YELLOW: Color32 = Color32::from_rgb(255, 196, 40);
const GRAY: Color32 = Color32::from_rgb(112, 112, 112);
const DGRAY: Color32 = Color32::from_rgb(64, 64, 64);
const LGRAY: Color32 = Color32::from_rgb(222, 224, 226);
const WHITE: Color32 = Color32::WHITE;
const FOLDER_BACK: Color32 = Color32::from_rgb(222, 168, 52);
const FOLDER_FRONT: Color32 = Color32::from_rgb(255, 217, 102);
const FOLDER_EDGE: Color32 = Color32::from_rgb(205, 150, 40);

/// Every icon the crate knows about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    // --- places & files -------------------------------------------------
    Folder,
    FolderOpen,
    FolderDesktop,
    FolderDownloads,
    FolderDocuments,
    FolderPictures,
    FolderMusic,
    FolderVideos,
    FolderHome,
    File,
    FileText,
    FileImage,
    FileAudio,
    FileVideo,
    FileArchive,
    FileCode,
    FileExec,
    FilePdf,
    Drive,
    Removable,
    ThisPc,
    Network,
    QuickAccess,
    Cloud,
    Trash,
    // --- navigation / chrome -------------------------------------------
    Back,
    Forward,
    Up,
    Refresh,
    Search,
    ChevronRight,
    ChevronDown,
    ChevronUp,
    ChevronLeft,
    DoubleChevronLeft,
    Pin,
    Close,
    Minimize,
    Maximize,
    Restore,
    Recent,
    Check,
    Ribbon,
    // --- commands -------------------------------------------------------
    Copy,
    Cut,
    Paste,
    PasteShortcut,
    CopyPath,
    MoveTo,
    CopyTo,
    Delete,
    Rename,
    NewFolder,
    NewItem,
    EasyAccess,
    Properties,
    Open,
    Edit,
    History,
    SelectAll,
    SelectNone,
    InvertSelection,
    Share,
    Email,
    Compress,
    Extract,
    Print,
    NavPane,
    PreviewPane,
    DetailsPane,
    ViewExtraLarge,
    ViewLarge,
    ViewMedium,
    ViewSmall,
    ViewList,
    ViewDetails,
    ViewTiles,
    ViewContent,
    SortBy,
    GroupBy,
    AddColumns,
    SizeColumns,
    ItemCheck,
    FileExt,
    HiddenItems,
    HideSelected,
    Options,
    Terminal,
    Help,
    Eject,
    Format,
    Cleanup,
    Optimize,
    Autoplay,
    Wallpaper,
    RotateLeft,
    RotateRight,
    Slideshow,
    Play,
    RunAsAdmin,
    NewWindow,
    DeleteHistory,
    Undo,
    Redo,
}

impl Icon {
    /// Every vector icon with a corresponding optional custom PNG slot.
    pub const ALL: &'static [Icon] = &[
        Icon::Folder,
        Icon::FolderOpen,
        Icon::FolderDesktop,
        Icon::FolderDownloads,
        Icon::FolderDocuments,
        Icon::FolderPictures,
        Icon::FolderMusic,
        Icon::FolderVideos,
        Icon::FolderHome,
        Icon::File,
        Icon::FileText,
        Icon::FileImage,
        Icon::FileAudio,
        Icon::FileVideo,
        Icon::FileArchive,
        Icon::FileCode,
        Icon::FileExec,
        Icon::FilePdf,
        Icon::Drive,
        Icon::Removable,
        Icon::ThisPc,
        Icon::Network,
        Icon::QuickAccess,
        Icon::Cloud,
        Icon::Trash,
        Icon::Back,
        Icon::Forward,
        Icon::Up,
        Icon::Refresh,
        Icon::Search,
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::ChevronUp,
        Icon::ChevronLeft,
        Icon::DoubleChevronLeft,
        Icon::Pin,
        Icon::Close,
        Icon::Minimize,
        Icon::Maximize,
        Icon::Restore,
        Icon::Recent,
        Icon::Check,
        Icon::Ribbon,
        Icon::Copy,
        Icon::Cut,
        Icon::Paste,
        Icon::PasteShortcut,
        Icon::CopyPath,
        Icon::MoveTo,
        Icon::CopyTo,
        Icon::Delete,
        Icon::Rename,
        Icon::NewFolder,
        Icon::NewItem,
        Icon::EasyAccess,
        Icon::Properties,
        Icon::Open,
        Icon::Edit,
        Icon::History,
        Icon::SelectAll,
        Icon::SelectNone,
        Icon::InvertSelection,
        Icon::Share,
        Icon::Email,
        Icon::Compress,
        Icon::Extract,
        Icon::Print,
        Icon::NavPane,
        Icon::PreviewPane,
        Icon::DetailsPane,
        Icon::ViewExtraLarge,
        Icon::ViewLarge,
        Icon::ViewMedium,
        Icon::ViewSmall,
        Icon::ViewList,
        Icon::ViewDetails,
        Icon::ViewTiles,
        Icon::ViewContent,
        Icon::SortBy,
        Icon::GroupBy,
        Icon::AddColumns,
        Icon::SizeColumns,
        Icon::ItemCheck,
        Icon::FileExt,
        Icon::HiddenItems,
        Icon::HideSelected,
        Icon::Options,
        Icon::Terminal,
        Icon::Help,
        Icon::Eject,
        Icon::Format,
        Icon::Cleanup,
        Icon::Optimize,
        Icon::Autoplay,
        Icon::Wallpaper,
        Icon::RotateLeft,
        Icon::RotateRight,
        Icon::Slideshow,
        Icon::Play,
        Icon::RunAsAdmin,
        Icon::NewWindow,
        Icon::DeleteHistory,
        Icon::Undo,
        Icon::Redo,
    ];

    /// Stable, case-sensitive filename stem used in the user's icon directory.
    pub fn file_stem(self) -> &'static str {
        match self {
            Icon::Folder => "Folder",
            Icon::FolderOpen => "FolderOpen",
            Icon::FolderDesktop => "FolderDesktop",
            Icon::FolderDownloads => "FolderDownloads",
            Icon::FolderDocuments => "FolderDocuments",
            Icon::FolderPictures => "FolderPictures",
            Icon::FolderMusic => "FolderMusic",
            Icon::FolderVideos => "FolderVideos",
            Icon::FolderHome => "FolderHome",
            Icon::File => "File",
            Icon::FileText => "FileText",
            Icon::FileImage => "FileImage",
            Icon::FileAudio => "FileAudio",
            Icon::FileVideo => "FileVideo",
            Icon::FileArchive => "FileArchive",
            Icon::FileCode => "FileCode",
            Icon::FileExec => "FileExec",
            Icon::FilePdf => "FilePdf",
            Icon::Drive => "Drive",
            Icon::Removable => "Removable",
            Icon::ThisPc => "ThisPc",
            Icon::Network => "Network",
            Icon::QuickAccess => "QuickAccess",
            Icon::Cloud => "Cloud",
            Icon::Trash => "Trash",
            Icon::Back => "Back",
            Icon::Forward => "Forward",
            Icon::Up => "Up",
            Icon::Refresh => "Refresh",
            Icon::Search => "Search",
            Icon::ChevronRight => "ChevronRight",
            Icon::ChevronDown => "ChevronDown",
            Icon::ChevronUp => "ChevronUp",
            Icon::ChevronLeft => "ChevronLeft",
            Icon::DoubleChevronLeft => "DoubleChevronLeft",
            Icon::Pin => "Pin",
            Icon::Close => "Close",
            Icon::Minimize => "Minimize",
            Icon::Maximize => "Maximize",
            Icon::Restore => "Restore",
            Icon::Recent => "Recent",
            Icon::Check => "Check",
            Icon::Ribbon => "Ribbon",
            Icon::Copy => "Copy",
            Icon::Cut => "Cut",
            Icon::Paste => "Paste",
            Icon::PasteShortcut => "PasteShortcut",
            Icon::CopyPath => "CopyPath",
            Icon::MoveTo => "MoveTo",
            Icon::CopyTo => "CopyTo",
            Icon::Delete => "Delete",
            Icon::Rename => "Rename",
            Icon::NewFolder => "NewFolder",
            Icon::NewItem => "NewItem",
            Icon::EasyAccess => "EasyAccess",
            Icon::Properties => "Properties",
            Icon::Open => "Open",
            Icon::Edit => "Edit",
            Icon::History => "History",
            Icon::SelectAll => "SelectAll",
            Icon::SelectNone => "SelectNone",
            Icon::InvertSelection => "InvertSelection",
            Icon::Share => "Share",
            Icon::Email => "Email",
            Icon::Compress => "Compress",
            Icon::Extract => "Extract",
            Icon::Print => "Print",
            Icon::NavPane => "NavPane",
            Icon::PreviewPane => "PreviewPane",
            Icon::DetailsPane => "DetailsPane",
            Icon::ViewExtraLarge => "ViewExtraLarge",
            Icon::ViewLarge => "ViewLarge",
            Icon::ViewMedium => "ViewMedium",
            Icon::ViewSmall => "ViewSmall",
            Icon::ViewList => "ViewList",
            Icon::ViewDetails => "ViewDetails",
            Icon::ViewTiles => "ViewTiles",
            Icon::ViewContent => "ViewContent",
            Icon::SortBy => "SortBy",
            Icon::GroupBy => "GroupBy",
            Icon::AddColumns => "AddColumns",
            Icon::SizeColumns => "SizeColumns",
            Icon::ItemCheck => "ItemCheck",
            Icon::FileExt => "FileExt",
            Icon::HiddenItems => "HiddenItems",
            Icon::HideSelected => "HideSelected",
            Icon::Options => "Options",
            Icon::Terminal => "Terminal",
            Icon::Help => "Help",
            Icon::Eject => "Eject",
            Icon::Format => "Format",
            Icon::Cleanup => "Cleanup",
            Icon::Optimize => "Optimize",
            Icon::Autoplay => "Autoplay",
            Icon::Wallpaper => "Wallpaper",
            Icon::RotateLeft => "RotateLeft",
            Icon::RotateRight => "RotateRight",
            Icon::Slideshow => "Slideshow",
            Icon::Play => "Play",
            Icon::RunAsAdmin => "RunAsAdmin",
            Icon::NewWindow => "NewWindow",
            Icon::DeleteHistory => "DeleteHistory",
            Icon::Undo => "Undo",
            Icon::Redo => "Redo",
        }
    }
}

impl Icon {
    /// Paint the icon centred in `rect` (fitted to the shorter side).
    pub fn paint(self, painter: &Painter, rect: Rect, enabled: bool) {
        if paint_custom_icon(self, painter, rect, enabled) {
            return;
        }

        let size = rect.width().min(rect.height());
        let origin = rect.center() - vec2(size, size) * 0.5;
        if enabled {
            // The enabled path is the common case: draw directly rather than cloning the
            // painter for every visible file and ribbon icon.
            draw(
                self,
                &Cv {
                    p: painter,
                    o: origin,
                    s: size / 16.0,
                },
            );
        } else {
            let mut dimmed = painter.clone();
            dimmed.multiply_opacity(0.4);
            draw(
                self,
                &Cv {
                    p: &dimmed,
                    o: origin,
                    s: size / 16.0,
                },
            );
        }
    }
}

static ICON_DIRECTORY: OnceLock<RwLock<PathBuf>> = OnceLock::new();
// Avoid touching the directory lock on every paint when custom icons are not configured.
static ICON_DIRECTORY_ACTIVE: AtomicBool = AtomicBool::new(false);
static ICON_TEXTURES: OnceLock<RwLock<HashMap<Icon, Option<TextureHandle>>>> = OnceLock::new();

// ---- per-file-extension custom icons ----------------------------------------------------
// CUSTOM EXTENSION ICONS: to give one file type its own icon (handy when many projects have
// look-alike files), put an image in the icon directory (config/icons) named
//     extension.<extension>.<svg|png>
// for example `extension.py.svg`, `extension.pdf.png`, `extension.epub.svg` or, for compound
// types, `extension.tar.xz.svg`. SVG is preferred (sharp at every size) and wins over PNG when
// both exist. The longest matching extension wins (`tar.xz` before `xz`). Extensions are matched
// case-insensitively. These files are NOT created as inactive placeholders; the directory is
// scanned once at startup, so restart the file manager after adding or removing one.
// `extension.noext.<svg|png>` is used for files that have no extension and are not executable.
const EXTENSION_ICON_PREFIX: &str = "extension.";
const EXTENSION_ICON_README: &str = "README-extension-icons.txt";
const EXTENSION_ICON_HELP: &str = "\
CUSTOM ICONS PER FILE EXTENSION
===============================
To use your own icon for a file type, drop an image in this folder named:

    extension.<extension>.<svg|png>

Examples:
    extension.py.svg
    extension.pdf.png
    extension.epub.svg
    extension.tar.xz.svg      (compound extensions work; the longest match wins)
    extension.noext.svg       (files with no extension that are not executable)

* SVG is preferred (stays sharp at any size); if both .svg and .png exist, the .svg is used.
* Extensions are case-insensitive. Do not include a leading dot in the extension part.
* Square images look best. Invalid images are ignored and the built-in icon is used.
* Icons are scanned once when the file manager starts: restart it after adding or removing files.
* Files like <IconName>.png.inactive are for the built-in icons, not for extensions.
";

/// Lowercased extension -> icon file (`.svg` preferred over `.png`), found by one startup scan.
static EXTENSION_ICON_FILES: OnceLock<RwLock<HashMap<String, PathBuf>>> = OnceLock::new();
// Avoid any lock/lookup per painted file when the user has no extension icons.
static EXTENSION_ICONS_ACTIVE: AtomicBool = AtomicBool::new(false);
static EXTENSION_TEXTURES: OnceLock<RwLock<HashMap<String, Option<TextureHandle>>>> =
    OnceLock::new();

/// Scan `dir` once for `extension.<ext>.svg|png` files.
fn scan_extension_icons(dir: &Path) -> HashMap<String, PathBuf> {
    let mut found: HashMap<String, PathBuf> = HashMap::new();
    let Ok(read) = fs::read_dir(dir) else {
        return found;
    };
    for item in read.flatten() {
        let path = item.path();
        if !path.is_file() {
            continue;
        }
        let name = item.file_name().to_string_lossy().to_ascii_lowercase();
        let Some(rest) = name.strip_prefix(EXTENSION_ICON_PREFIX) else {
            continue;
        };
        let (ext, is_svg) = if let Some(ext) = rest.strip_suffix(".svg") {
            (ext, true)
        } else if let Some(ext) = rest.strip_suffix(".png") {
            (ext, false)
        } else {
            continue;
        };
        if ext.is_empty() || ext.starts_with('.') || ext.contains(['/', '\\']) {
            continue;
        }
        // SVG replaces a PNG for the same extension; a PNG never replaces an SVG.
        if is_svg || !found.contains_key(ext) {
            found.insert(ext.to_owned(), path);
        }
    }
    found
}

/// Create `README-extension-icons.txt` once so users know how to add extension icons.
fn write_extension_icon_readme(directory: &Path) {
    let path = directory.join(EXTENSION_ICON_README);
    if let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(&path) {
        let _ = file.write_all(EXTENSION_ICON_HELP.as_bytes());
    }
}

/// Paint a user-supplied `extension.<ext>.svg|png` icon for the file `name_lc` (already
/// lowercased). Returns `false` when no custom icon applies, so the caller draws the default.
pub fn paint_extension_icon(name_lc: &str, painter: &Painter, rect: Rect, enabled: bool) -> bool {
    if !EXTENSION_ICONS_ACTIVE.load(Ordering::Acquire) {
        return false;
    }
    let (Some(files), Some(textures)) = (EXTENSION_ICON_FILES.get(), EXTENSION_TEXTURES.get())
    else {
        return false;
    };
    // Try every suffix after a dot, longest first ("a.tar.xz" -> "tar.xz", then "xz"). The
    // leading dot of a hidden file is not an extension separator.
    let mut key: Option<&str> = None;
    {
        let files = files.read().unwrap_or_else(|e| e.into_inner());
        for (i, _) in name_lc.match_indices('.') {
            if i == 0 {
                continue;
            }
            let suffix = &name_lc[i + 1..];
            if !suffix.is_empty() && files.contains_key(suffix) {
                key = Some(suffix);
                break;
            }
        }
    }
    // A name with no extension at all uses `extension.noext.<svg|png>` when present
    // (executables are handled earlier and keep the executable icon).
    if key.is_none() && !name_lc.char_indices().any(|(i, c)| c == '.' && i > 0) {
        let files = files.read().unwrap_or_else(|e| e.into_inner());
        if files.contains_key("noext") {
            key = Some("noext");
        }
    }
    let Some(key) = key else {
        return false;
    };

    let cached = {
        let cache = textures.read().unwrap_or_else(|e| e.into_inner());
        cache
            .get(key)
            .map(|t| t.as_ref().map(|t| (t.id(), t.size_vec2())))
    };
    let info = match cached {
        Some(info) => info,
        None => {
            let mut cache = textures.write().unwrap_or_else(|e| e.into_inner());
            let texture = cache.entry(key.to_owned()).or_insert_with(|| {
                let path = files
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(key)
                    .cloned()?;
                let is_svg = path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
                let image = if is_svg {
                    load_svg(&path)
                } else {
                    load_png(&path)
                }?;
                Some(painter.ctx().load_texture(
                    format!("fm-extension-icon-{key}"),
                    image,
                    TextureOptions::LINEAR,
                ))
            });
            texture.as_ref().map(|t| (t.id(), t.size_vec2()))
        }
    };
    let Some((id, size)) = info else {
        return false;
    };
    paint_fitted_texture(painter, id, size, rect, enabled)
}

/// Draw a texture fitted (not stretched) and centred in `rect`. `false` if it cannot be drawn.
fn paint_fitted_texture(
    painter: &Painter,
    id: egui::TextureId,
    size: egui::Vec2,
    rect: Rect,
    enabled: bool,
) -> bool {
    if !(size.x > 0.0 && size.y > 0.0) {
        return false;
    }
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    if !scale.is_finite() || scale <= 0.0 {
        return false;
    }
    let tint = Color32::from_white_alpha(if enabled { 255 } else { 102 });
    painter.image(
        id,
        Rect::from_center_size(rect.center(), size * scale),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        tint,
    );
    true
}

// Tiny, intentionally non-image shell. Replacing it with a valid PNG is required
// before renaming `.png.inactive` to `.png`; invalid active files fall back to vector icons.
// A `<IconName>.svg` file in the same directory is used directly (no renaming needed).
const ICON_PLACEHOLDER: &[u8] = b"Replace with a PNG icon, then remove .inactive\n";

/// Set the directory containing active `<IconName>.svg` / `<IconName>.png` files. Call once before painting.
pub fn set_icon_directory(path: impl Into<PathBuf>) {
    let path = path.into();
    let active = !path.as_os_str().is_empty();
    *ICON_DIRECTORY
        .get_or_init(|| RwLock::new(PathBuf::new()))
        .write()
        .unwrap_or_else(|e| e.into_inner()) = path;
    ICON_DIRECTORY_ACTIVE.store(active, Ordering::Release);

    // One scan for `extension.<ext>.svg|png` custom icons; lookups afterwards are in memory.
    let scanned = if active {
        scan_extension_icons(
            &ICON_DIRECTORY
                .get_or_init(|| RwLock::new(PathBuf::new()))
                .read()
                .unwrap_or_else(|e| e.into_inner()),
        )
    } else {
        HashMap::new()
    };
    let any = !scanned.is_empty();
    *EXTENSION_ICON_FILES
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|e| e.into_inner()) = scanned;
    EXTENSION_TEXTURES
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    EXTENSION_ICONS_ACTIVE.store(any, Ordering::Release);
}

/// Create missing `<IconName>.png.inactive` shell files. Existing inactive or active files
/// are never overwritten, so startup does not rewrite user configuration on every launch.
pub fn initialize_icon_placeholders(directory: &Path) -> io::Result<usize> {
    fs::create_dir_all(directory)?;
    write_extension_icon_readme(directory);
    set_icon_directory(directory.to_path_buf());
    let mut created = 0;
    for icon in Icon::ALL {
        let stem = icon.file_stem();
        let active = directory.join(format!("{stem}.png"));
        if active.exists() || directory.join(format!("{stem}.svg")).exists() {
            continue;
        }
        let inactive = directory.join(format!("{stem}.png.inactive"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&inactive)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(ICON_PLACEHOLDER) {
                    let _ = fs::remove_file(&inactive);
                    return Err(error);
                }
                created += 1;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(created)
}

/// SVG texture edge in pixels. 256 px stays sharp for 96-point icons at 2x display scale.
const SVG_RASTER_SIZE: u32 = 256;

fn load_png(path: &Path) -> Option<ColorImage> {
    if !path.is_file() {
        return None;
    }
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let rgba = reader.decode().ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw()))
}

/// Rasterise an SVG into a square, transparent canvas (aspect ratio kept, centred).
fn load_svg(path: &Path) -> Option<ColorImage> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
        return None;
    }
    let data = fs::read(path).ok()?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    let edge = SVG_RASTER_SIZE as f32;
    let scale = edge / w.max(h);
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale)
        .post_translate((edge - w * scale) / 2.0, (edge - h * scale) / 2.0);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SVG_RASTER_SIZE, SVG_RASTER_SIZE)?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(ColorImage::from_rgba_premultiplied(
        [SVG_RASTER_SIZE as usize; 2],
        pixmap.data(),
    ))
}

/// Load an application icon (PNG or SVG) as an image, for hosts that show app logos
/// (e.g. the "Open with" flyout). Returns `None` for unsupported or unreadable files.
pub fn load_app_icon_image(path: &Path) -> Option<ColorImage> {
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
    if is_svg {
        load_svg(path)
    } else {
        load_png(path)
    }
}

fn paint_custom_icon(icon: Icon, painter: &Painter, rect: Rect, enabled: bool) -> bool {
    if !ICON_DIRECTORY_ACTIVE.load(Ordering::Acquire) {
        return false;
    }

    let cache = ICON_TEXTURES.get_or_init(|| RwLock::new(HashMap::new()));

    // The common path takes only a shared cache lock and copies the texture metadata. The
    // directory is read and its PathBuf cloned only on the first paint of each icon.
    let cached = {
        let cache = cache.read().unwrap_or_else(|e| e.into_inner());
        cache.get(&icon).map(|texture| {
            texture
                .as_ref()
                .map(|texture| (texture.id(), texture.size_vec2()))
        })
    };
    let texture_info = match cached {
        Some(info) => info,
        None => {
            let mut cache = cache.write().unwrap_or_else(|e| e.into_inner());
            let texture = cache.entry(icon).or_insert_with(|| {
                let directory = ICON_DIRECTORY.get_or_init(|| RwLock::new(PathBuf::new()));
                let dir = directory.read().unwrap_or_else(|e| e.into_inner()).clone();
                if dir.as_os_str().is_empty() {
                    return None;
                }
                let stem = icon.file_stem();
                // `<Name>.svg` wins over `<Name>.png`; an unreadable file falls through to
                // the next format and finally to the built-in vector icon.
                let color_image = load_svg(&dir.join(format!("{stem}.svg")))
                    .or_else(|| load_png(&dir.join(format!("{stem}.png"))))?;
                Some(painter.ctx().load_texture(
                    format!("fm-custom-icon-{stem}"),
                    color_image,
                    TextureOptions::LINEAR,
                ))
            });
            texture
                .as_ref()
                .map(|texture| (texture.id(), texture.size_vec2()))
        }
    };

    let Some((texture_id, texture_size)) = texture_info else {
        return false;
    };
    if !(texture_size.x > 0.0 && texture_size.y > 0.0) {
        return false;
    }

    // Fit rather than stretch custom PNGs; this also keeps SVG/PNG replacements consistent
    // with the built-in vector icons when callers provide non-square paint rectangles.
    paint_fitted_texture(painter, texture_id, texture_size, rect, enabled)
}

struct Cv<'a> {
    p: &'a Painter,
    o: Pos2,
    s: f32,
}

type Pt = (f32, f32);

impl Cv<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        self.o + vec2(x * self.s, y * self.s)
    }
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.pt(x, y), vec2(w * self.s, h * self.s))
    }
    fn fill(&self, x: f32, y: f32, w: f32, h: f32, c: Color32) {
        self.p
            .rect_filled(self.rect(x, y, w, h), CornerRadius::ZERO, c);
    }
    fn rfill(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color32) {
        self.p.rect_filled(self.rect(x, y, w, h), r * self.s, c);
    }
    fn frame(&self, x: f32, y: f32, w: f32, h: f32, c: Color32, wd: f32) {
        self.p.rect_stroke(
            self.rect(x, y, w, h),
            CornerRadius::ZERO,
            Stroke::new(wd * self.s, c),
            StrokeKind::Middle,
        );
    }
    #[allow(clippy::too_many_arguments)]
    fn rframe(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color32, wd: f32) {
        self.p.rect_stroke(
            self.rect(x, y, w, h),
            r * self.s,
            Stroke::new(wd * self.s, c),
            StrokeKind::Middle,
        );
    }
    fn line(&self, pts: &[Pt], c: Color32, wd: f32) {
        let stroke = Stroke::new(wd * self.s, c);
        // Most calls are a single straight stroke between two points (icons
        // are built out of many of these). Use the dedicated two-point shape,
        // which needs no heap allocation, instead of always building a Vec.
        if let [p0, p1] = pts {
            self.p.add(Shape::line_segment(
                [self.pt(p0.0, p0.1), self.pt(p1.0, p1.1)],
                stroke,
            ));
            return;
        }
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::line(v, stroke));
    }
    fn closed(&self, pts: &[Pt], c: Color32, wd: f32) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p
            .add(Shape::closed_line(v, Stroke::new(wd * self.s, c)));
    }
    fn poly(&self, pts: &[Pt], fill: Color32) {
        let v: Vec<Pos2> = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::convex_polygon(v, fill, Stroke::NONE));
    }
    fn disc(&self, x: f32, y: f32, r: f32, c: Color32) {
        self.p.circle_filled(self.pt(x, y), r * self.s, c);
    }
    fn ring(&self, x: f32, y: f32, r: f32, c: Color32, wd: f32) {
        self.p
            .circle_stroke(self.pt(x, y), r * self.s, Stroke::new(wd * self.s, c));
    }
    fn text(&self, x: f32, y: f32, t: &str, size: f32, c: Color32) {
        self.p.text(
            self.pt(x, y),
            egui::Align2::CENTER_CENTER,
            t,
            egui::FontId::proportional(size * self.s),
            c,
        );
    }

    // ---- reusable glyphs ------------------------------------------------

    fn folder(&self) {
        self.fill(1.0, 3.0, 5.5, 2.5, FOLDER_BACK);
        self.fill(1.0, 4.0, 14.0, 9.5, FOLDER_BACK);
        self.fill(1.0, 5.5, 14.0, 8.0, FOLDER_FRONT);
        self.frame(1.0, 5.5, 14.0, 8.0, FOLDER_EDGE, 0.6);
    }
    fn page(&self, x: f32, y: f32, w: f32, h: f32) {
        let f = 3.5_f32.min(w * 0.4);
        self.poly(
            &[
                (x, y),
                (x + w - f, y),
                (x + w, y + f),
                (x + w, y + h),
                (x, y + h),
            ],
            WHITE,
        );
        self.closed(
            &[
                (x, y),
                (x + w - f, y),
                (x + w, y + f),
                (x + w, y + h),
                (x, y + h),
            ],
            GRAY,
            0.8,
        );
        self.line(
            &[(x + w - f, y), (x + w - f, y + f), (x + w, y + f)],
            GRAY,
            0.7,
        );
    }
    fn arrow_right(&self, x: f32, y: f32, len: f32, c: Color32) {
        self.line(&[(x, y), (x + len, y)], c, 1.6);
        self.line(
            &[
                (x + len - 2.5, y - 2.5),
                (x + len, y),
                (x + len - 2.5, y + 2.5),
            ],
            c,
            1.6,
        );
    }
    fn plus(&self, x: f32, y: f32, r: f32, c: Color32) {
        self.disc(x, y, r, c);
        self.line(&[(x - r * 0.55, y), (x + r * 0.55, y)], WHITE, 1.3);
        self.line(&[(x, y - r * 0.55), (x, y + r * 0.55)], WHITE, 1.3);
    }
    fn monitor(&self, x: f32, y: f32, w: f32, h: f32, screen: Color32) {
        self.rfill(x, y, w, h, 0.8, DGRAY);
        self.fill(x + 1.0, y + 1.0, w - 2.0, h - 2.0, screen);
        self.fill(x + w / 2.0 - 2.0, y + h, 4.0, 1.5, GRAY);
        self.fill(x + w / 2.0 - 3.0, y + h + 1.5, 6.0, 1.0, GRAY);
    }
    fn gear(&self, x: f32, y: f32, r: f32, c: Color32) {
        for i in 0..8 {
            let a = i as f32 * std::f32::consts::TAU / 8.0;
            let (s, co) = a.sin_cos();
            self.line(
                &[
                    (x + co * r * 0.7, y + s * r * 0.7),
                    (x + co * r * 1.15, y + s * r * 1.15),
                ],
                c,
                2.0,
            );
        }
        self.disc(x, y, r * 0.85, c);
        self.disc(x, y, r * 0.35, WHITE);
    }
    fn grid(&self, n: usize, cell: f32, gap: f32, x0: f32, y0: f32, c: Color32) {
        for i in 0..n {
            for j in 0..n {
                self.fill(
                    x0 + i as f32 * (cell + gap),
                    y0 + j as f32 * (cell + gap),
                    cell,
                    cell,
                    c,
                );
            }
        }
    }
    fn panes(&self, left: bool, right: bool, bottom: bool, accent: Color32) {
        self.fill(1.0, 2.0, 14.0, 12.0, WHITE);
        self.frame(1.0, 2.0, 14.0, 12.0, GRAY, 0.9);
        self.fill(1.0, 2.0, 14.0, 2.0, LBLUE);
        if left {
            self.fill(1.5, 4.0, 4.0, 9.5, accent);
        }
        if right {
            self.fill(10.5, 4.0, 4.0, 9.5, accent);
        }
        if bottom {
            self.fill(1.5, 10.5, 13.0, 3.0, accent);
        }
    }
    fn badge_folder(&self, f: impl FnOnce(&Self)) {
        self.folder();
        f(self);
    }
}

fn draw(icon: Icon, c: &Cv) {
    use Icon::*;
    match icon {
        // ---------------- places ----------------
        Folder => c.folder(),
        FolderOpen => {
            c.fill(1.0, 3.0, 5.5, 2.5, FOLDER_BACK);
            c.fill(1.0, 4.0, 13.0, 9.5, FOLDER_BACK);
            c.poly(
                &[(3.5, 6.5), (15.5, 6.5), (13.0, 13.5), (1.0, 13.5)],
                FOLDER_FRONT,
            );
            c.closed(
                &[(3.5, 6.5), (15.5, 6.5), (13.0, 13.5), (1.0, 13.5)],
                FOLDER_EDGE,
                0.6,
            );
        }
        FolderDesktop => c.badge_folder(|c| {
            c.fill(4.0, 7.5, 8.0, 4.5, BLUE);
            c.fill(5.0, 8.5, 6.0, 2.5, LBLUE);
        }),
        FolderDownloads => c.badge_folder(|c| {
            c.line(&[(8.0, 7.0), (8.0, 12.0)], GREEN, 1.8);
            c.line(&[(5.5, 9.8), (8.0, 12.3), (10.5, 9.8)], GREEN, 1.8);
        }),
        FolderDocuments => c.badge_folder(|c| {
            c.fill(5.0, 6.8, 6.0, 5.8, WHITE);
            c.line(&[(6.0, 8.5), (10.0, 8.5)], GRAY, 0.7);
            c.line(&[(6.0, 10.0), (10.0, 10.0)], GRAY, 0.7);
        }),
        FolderPictures => c.badge_folder(|c| {
            c.poly(
                &[
                    (4.0, 12.5),
                    (7.0, 8.0),
                    (9.0, 10.5),
                    (10.5, 9.0),
                    (12.5, 12.5),
                ],
                GREEN,
            );
            c.disc(10.5, 8.0, 1.0, WHITE);
        }),
        FolderMusic => c.badge_folder(|c| {
            c.line(&[(9.5, 12.0), (9.5, 7.0), (12.0, 7.8)], DBLUE, 1.2);
            c.disc(8.3, 12.0, 1.5, DBLUE);
        }),
        FolderVideos => c.badge_folder(|c| {
            c.fill(4.5, 7.0, 7.0, 5.5, DGRAY);
            c.poly(&[(7.0, 8.0), (7.0, 11.5), (10.0, 9.75)], WHITE);
        }),
        FolderHome => c.badge_folder(|c| {
            c.poly(&[(8.0, 6.5), (12.0, 9.8), (4.0, 9.8)], BLUE);
            c.fill(5.2, 9.8, 5.6, 3.0, BLUE);
            c.fill(7.2, 10.8, 1.6, 2.0, WHITE);
        }),
        File => c.page(2.5, 1.0, 11.0, 14.0),
        FileText => {
            c.page(2.5, 1.0, 11.0, 14.0);
            for y in [6.5, 8.5, 10.5, 12.5] {
                c.line(&[(4.5, y), (11.5, y)], GRAY, 0.8);
            }
        }
        FileImage => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.poly(
                &[
                    (4.0, 12.5),
                    (7.0, 8.0),
                    (9.0, 10.5),
                    (10.2, 9.3),
                    (12.0, 12.5),
                ],
                GREEN,
            );
            c.disc(10.5, 6.8, 1.1, YELLOW);
        }
        FileAudio => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.line(&[(9.5, 11.5), (9.5, 6.5), (11.8, 7.2)], DBLUE, 1.1);
            c.disc(8.3, 11.5, 1.4, DBLUE);
        }
        FileVideo => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.fill(4.0, 7.0, 8.0, 5.5, DGRAY);
            c.poly(&[(7.0, 8.0), (7.0, 11.5), (10.0, 9.75)], WHITE);
        }
        FileArchive => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.fill(7.2, 1.5, 1.6, 12.0, FOLDER_FRONT);
            for y in [3.0, 5.0, 7.0, 9.0] {
                c.fill(7.2, y, 1.6, 1.0, DGRAY);
            }
        }
        FileCode => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.line(&[(6.5, 7.0), (4.5, 9.5), (6.5, 12.0)], BLUE, 1.2);
            c.line(&[(9.5, 7.0), (11.5, 9.5), (9.5, 12.0)], BLUE, 1.2);
        }
        FileExec => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.gear(8.0, 9.5, 2.8, GRAY);
        }
        FilePdf => {
            c.page(2.5, 1.0, 11.0, 14.0);
            c.fill(3.0, 8.0, 10.0, 4.2, RED);
            c.text(8.0, 10.1, "PDF", 4.2, WHITE);
        }
        Drive => {
            c.rfill(1.0, 5.0, 14.0, 7.0, 1.0, Color32::from_rgb(190, 194, 198));
            c.rfill(1.0, 9.0, 14.0, 3.0, 1.0, Color32::from_rgb(140, 146, 152));
            c.fill(2.5, 10.2, 5.0, 0.8, WHITE);
            c.disc(12.5, 10.6, 0.8, GREEN);
            c.line(&[(2.0, 5.5), (14.0, 5.5)], WHITE, 0.7);
        }
        Removable => {
            c.rfill(3.0, 6.0, 10.0, 7.0, 1.0, Color32::from_rgb(120, 126, 132));
            c.fill(5.0, 2.5, 6.0, 3.5, LGRAY);
            c.fill(6.0, 3.2, 1.0, 1.8, DGRAY);
            c.fill(9.0, 3.2, 1.0, 1.8, DGRAY);
            c.disc(8.0, 9.5, 1.0, GREEN);
        }
        ThisPc => c.monitor(1.5, 2.0, 13.0, 9.0, BLUE),
        Network => {
            c.ring(8.0, 8.0, 6.3, BLUE, 1.1);
            c.line(&[(1.7, 8.0), (14.3, 8.0)], BLUE, 0.9);
            c.line(&[(8.0, 1.7), (8.0, 14.3)], BLUE, 0.9);
            c.closed(
                &[
                    (5.0, 3.0),
                    (3.4, 8.0),
                    (5.0, 13.0),
                    (8.0, 14.3),
                    (11.0, 13.0),
                    (12.6, 8.0),
                    (11.0, 3.0),
                    (8.0, 1.7),
                ],
                BLUE,
                0.7,
            );
        }
        QuickAccess => {
            let pts = star(8.0, 8.4, 7.0, 3.0);
            c.poly(&pts, BLUE);
        }
        Cloud => {
            c.disc(5.5, 9.5, 3.0, LBLUE);
            c.disc(9.0, 7.5, 3.8, LBLUE);
            c.disc(12.0, 10.0, 2.6, LBLUE);
            c.fill(5.5, 9.0, 6.5, 3.6, LBLUE);
        }
        Trash => {
            c.fill(3.5, 4.5, 9.0, 10.0, LGRAY);
            c.frame(3.5, 4.5, 9.0, 10.0, GRAY, 0.8);
            c.fill(2.5, 3.0, 11.0, 1.5, GRAY);
            c.fill(6.0, 1.8, 4.0, 1.2, GRAY);
            for x in [6.0, 8.0, 10.0] {
                c.line(&[(x, 6.5), (x, 12.5)], GRAY, 0.8);
            }
        }
        // ---------------- chrome ----------------
        Back => {
            c.line(&[(13.0, 8.0), (3.0, 8.0)], DGRAY, 1.5);
            c.line(&[(7.5, 3.5), (3.0, 8.0), (7.5, 12.5)], DGRAY, 1.5);
        }
        Forward => {
            c.line(&[(3.0, 8.0), (13.0, 8.0)], DGRAY, 1.5);
            c.line(&[(8.5, 3.5), (13.0, 8.0), (8.5, 12.5)], DGRAY, 1.5);
        }
        Up => {
            c.line(&[(8.0, 13.0), (8.0, 3.0)], DGRAY, 1.5);
            c.line(&[(3.5, 7.5), (8.0, 3.0), (12.5, 7.5)], DGRAY, 1.5);
        }
        Refresh => {
            arc(c, 8.0, 8.0, 5.0, 0.6, 5.7, DGRAY, 1.5);
            c.poly(&[(13.5, 2.5), (13.5, 7.5), (8.8, 5.6)], DGRAY);
        }
        Search => {
            c.ring(6.5, 6.5, 4.2, DGRAY, 1.4);
            c.line(&[(9.7, 9.7), (14.0, 14.0)], DGRAY, 1.8);
        }
        ChevronRight => c.line(&[(5.5, 3.0), (10.5, 8.0), (5.5, 13.0)], GRAY, 1.3),
        ChevronDown => c.line(&[(3.0, 5.5), (8.0, 10.5), (13.0, 5.5)], GRAY, 1.3),
        ChevronUp => c.line(&[(3.0, 10.5), (8.0, 5.5), (13.0, 10.5)], GRAY, 1.3),
        ChevronLeft => c.line(&[(10.5, 3.0), (5.5, 8.0), (10.5, 13.0)], GRAY, 1.3),
        DoubleChevronLeft => {
            c.line(&[(8.0, 3.0), (3.0, 8.0), (8.0, 13.0)], GRAY, 1.2);
            c.line(&[(13.0, 3.0), (8.0, 8.0), (13.0, 13.0)], GRAY, 1.2);
        }
        Pin => {
            c.line(&[(5.5, 3.0), (10.5, 3.0)], BLUE, 1.4);
            c.fill(6.5, 3.0, 3.0, 5.0, BLUE);
            c.line(&[(4.5, 8.0), (11.5, 8.0)], BLUE, 1.4);
            c.line(&[(8.0, 8.0), (8.0, 14.0)], BLUE, 1.2);
        }
        Close => {
            c.line(&[(3.0, 3.0), (13.0, 13.0)], DGRAY, 1.2);
            c.line(&[(13.0, 3.0), (3.0, 13.0)], DGRAY, 1.2);
        }
        Minimize => c.line(&[(3.0, 8.5), (13.0, 8.5)], DGRAY, 1.1),
        Maximize => c.frame(3.2, 3.2, 9.6, 9.6, DGRAY, 1.1),
        Restore => {
            c.frame(3.0, 5.2, 7.6, 7.6, DGRAY, 1.1);
            c.line(
                &[
                    (5.6, 5.2),
                    (5.6, 3.2),
                    (12.8, 3.2),
                    (12.8, 10.4),
                    (10.6, 10.4),
                ],
                DGRAY,
                1.1,
            );
        }
        Recent => {
            c.ring(8.0, 8.0, 6.0, GRAY, 1.2);
            c.line(&[(8.0, 4.0), (8.0, 8.0), (11.0, 9.5)], GRAY, 1.2);
        }
        Check => c.line(&[(2.5, 8.5), (6.5, 12.5), (13.5, 4.0)], DGRAY, 1.6),
        Ribbon => {
            c.fill(1.5, 3.0, 13.0, 3.0, LBLUE);
            c.frame(1.5, 3.0, 13.0, 10.0, GRAY, 0.9);
            c.fill(3.0, 7.5, 3.0, 4.0, BLUE);
            c.fill(7.0, 7.5, 3.0, 4.0, BLUE);
        }
        // ---------------- commands ----------------
        Copy => {
            c.page(2.0, 1.5, 8.0, 10.0);
            c.page(6.0, 5.0, 8.0, 10.0);
        }
        Cut => {
            c.line(&[(5.0, 2.0), (10.5, 10.0)], DGRAY, 1.2);
            c.line(&[(11.0, 2.0), (5.5, 10.0)], DGRAY, 1.2);
            c.ring(4.8, 12.2, 2.0, RED, 1.2);
            c.ring(11.2, 12.2, 2.0, RED, 1.2);
        }
        Paste => {
            c.rfill(2.0, 2.5, 9.0, 12.0, 1.0, Color32::from_rgb(196, 150, 80));
            c.rfill(4.5, 1.5, 4.0, 2.5, 0.8, GRAY);
            c.page(6.0, 6.0, 8.0, 9.0);
        }
        PasteShortcut => {
            c.rfill(2.0, 2.5, 9.0, 12.0, 1.0, Color32::from_rgb(196, 150, 80));
            c.rfill(4.5, 1.5, 4.0, 2.5, 0.8, GRAY);
            c.page(6.0, 6.0, 8.0, 9.0);
            c.arrow_right(7.5, 12.5, 4.0, BLUE);
        }
        CopyPath => {
            c.page(2.0, 1.5, 8.0, 10.0);
            c.fill(5.0, 10.0, 10.0, 4.5, BLUE);
            c.text(10.0, 12.3, "path", 3.4, WHITE);
        }
        MoveTo => {
            c.folder();
            c.arrow_right(3.5, 9.5, 8.0, BLUE);
        }
        CopyTo => {
            c.folder();
            c.page(4.5, 6.0, 5.0, 7.0);
            c.plus(12.5, 12.5, 2.6, GREEN);
        }
        Delete => {
            c.line(&[(3.0, 3.0), (13.0, 13.0)], RED, 2.2);
            c.line(&[(13.0, 3.0), (3.0, 13.0)], RED, 2.2);
        }
        Rename => {
            c.rframe(1.5, 4.5, 13.0, 7.0, 0.8, GRAY, 0.9);
            c.line(&[(5.0, 6.0), (5.0, 10.0)], BLUE, 1.2);
            c.text(9.5, 8.2, "ab", 4.6, DGRAY);
        }
        NewFolder => {
            c.folder();
            c.plus(12.5, 4.0, 3.2, GREEN);
        }
        NewItem => {
            c.page(2.5, 1.5, 10.0, 13.5);
            c.poly(&star(12.5, 3.5, 3.2, 1.4), YELLOW);
        }
        EasyAccess => {
            c.folder();
            c.poly(&star(11.8, 10.5, 3.4, 1.5), BLUE);
        }
        Properties => {
            c.page(2.0, 1.0, 11.0, 14.0);
            c.line(&[(4.5, 8.0), (7.0, 10.8), (11.5, 5.0)], RED, 1.6);
        }
        Open => {
            c.folder();
            c.poly(&[(9.0, 4.0), (15.0, 4.0), (15.0, 10.0)], BLUE);
            c.line(&[(7.0, 12.0), (14.5, 4.5)], BLUE, 1.2);
        }
        Edit => {
            c.page(2.0, 1.5, 9.0, 13.0);
            c.line(&[(7.0, 13.0), (14.0, 5.0)], YELLOW, 2.4);
            c.poly(&[(6.0, 14.5), (6.5, 12.2), (8.3, 13.8)], DGRAY);
        }
        History => {
            c.ring(8.0, 8.0, 6.0, BLUE, 1.2);
            c.line(&[(8.0, 4.0), (8.0, 8.0), (11.0, 9.5)], BLUE, 1.2);
        }
        SelectAll => {
            c.frame(2.0, 2.0, 12.0, 12.0, BLUE, 1.0);
            c.fill(4.0, 4.0, 8.0, 8.0, LBLUE);
            c.line(&[(5.0, 8.2), (7.2, 10.5), (11.0, 5.5)], DBLUE, 1.3);
        }
        SelectNone => {
            c.frame(2.0, 2.0, 12.0, 12.0, GRAY, 1.0);
            c.fill(4.0, 4.0, 8.0, 8.0, WHITE);
        }
        InvertSelection => {
            c.fill(2.0, 2.0, 7.0, 7.0, LBLUE);
            c.frame(2.0, 2.0, 7.0, 7.0, BLUE, 0.9);
            c.frame(7.0, 7.0, 7.0, 7.0, GRAY, 0.9);
            c.fill(7.5, 7.5, 6.0, 6.0, WHITE);
        }
        Share => {
            c.disc(4.0, 8.0, 2.0, BLUE);
            c.disc(12.0, 3.5, 2.0, BLUE);
            c.disc(12.0, 12.5, 2.0, BLUE);
            c.line(&[(4.0, 8.0), (12.0, 3.5)], BLUE, 1.0);
            c.line(&[(4.0, 8.0), (12.0, 12.5)], BLUE, 1.0);
        }
        Email => {
            c.fill(1.5, 3.5, 13.0, 9.0, WHITE);
            c.frame(1.5, 3.5, 13.0, 9.0, GRAY, 0.9);
            c.line(&[(1.8, 4.0), (8.0, 9.0), (14.2, 4.0)], GRAY, 0.9);
        }
        Compress => {
            c.folder();
            c.fill(7.2, 5.5, 1.6, 8.0, DGRAY);
            for y in [6.2, 8.2, 10.2, 12.2] {
                c.fill(7.2, y, 1.6, 0.9, WHITE);
            }
        }
        Extract => {
            c.folder();
            c.line(&[(8.0, 6.5), (8.0, 12.5)], GREEN, 1.6);
            c.line(&[(5.5, 10.2), (8.0, 12.7), (10.5, 10.2)], GREEN, 1.6);
        }
        Print => {
            c.fill(4.0, 1.5, 8.0, 5.0, WHITE);
            c.frame(4.0, 1.5, 8.0, 5.0, GRAY, 0.8);
            c.rfill(1.5, 6.0, 13.0, 6.0, 1.0, GRAY);
            c.fill(4.0, 10.0, 8.0, 4.5, WHITE);
            c.frame(4.0, 10.0, 8.0, 4.5, GRAY, 0.8);
            c.disc(12.0, 8.0, 0.7, GREEN);
        }
        NavPane => c.panes(true, false, false, LBLUE),
        PreviewPane => c.panes(false, true, false, LBLUE),
        DetailsPane => c.panes(false, false, true, LBLUE),
        ViewExtraLarge => {
            c.fill(2.0, 2.0, 12.0, 12.0, FOLDER_FRONT);
            c.frame(2.0, 2.0, 12.0, 12.0, FOLDER_EDGE, 0.8);
        }
        ViewLarge => c.grid(2, 6.0, 1.5, 1.5, 1.5, FOLDER_FRONT),
        ViewMedium => c.grid(3, 3.6, 1.6, 1.0, 1.0, FOLDER_FRONT),
        ViewSmall => c.grid(4, 2.4, 1.2, 1.0, 1.0, FOLDER_FRONT),
        ViewList => {
            for (i, y) in [2.5, 6.0, 9.5, 13.0].into_iter().enumerate() {
                let _ = i;
                c.fill(1.5, y, 2.0, 2.0, FOLDER_FRONT);
                c.fill(4.5, y + 0.5, 3.5, 1.0, GRAY);
                c.fill(9.0, y, 2.0, 2.0, FOLDER_FRONT);
                c.fill(12.0, y + 0.5, 2.5, 1.0, GRAY);
            }
        }
        ViewDetails => {
            for y in [2.5, 6.0, 9.5, 13.0] {
                c.fill(1.5, y, 2.0, 2.0, FOLDER_FRONT);
                c.fill(4.5, y + 0.5, 4.0, 1.0, GRAY);
                c.fill(10.0, y + 0.5, 4.5, 1.0, LGRAY);
            }
        }
        ViewTiles => {
            for y in [2.0, 9.0] {
                c.fill(1.5, y, 5.0, 5.0, FOLDER_FRONT);
                c.fill(7.5, y + 0.5, 6.0, 1.0, GRAY);
                c.fill(7.5, y + 3.0, 4.0, 1.0, LGRAY);
            }
        }
        ViewContent => {
            for y in [1.5, 6.0, 10.5] {
                c.fill(1.5, y, 3.5, 3.5, FOLDER_FRONT);
                c.fill(6.0, y + 0.2, 8.0, 1.0, GRAY);
                c.fill(6.0, y + 2.3, 6.0, 0.9, LGRAY);
            }
        }
        SortBy => {
            c.fill(1.5, 3.0, 5.0, 1.5, GRAY);
            c.fill(1.5, 7.0, 7.5, 1.5, GRAY);
            c.fill(1.5, 11.0, 10.0, 1.5, GRAY);
            c.line(&[(13.0, 3.0), (13.0, 13.0)], BLUE, 1.3);
            c.line(&[(11.0, 11.0), (13.0, 13.2), (15.0, 11.0)], BLUE, 1.3);
        }
        GroupBy => {
            c.fill(1.5, 2.0, 13.0, 2.0, LBLUE);
            c.fill(3.5, 5.5, 8.0, 1.3, GRAY);
            c.fill(3.5, 8.0, 6.0, 1.3, GRAY);
            c.fill(1.5, 10.5, 13.0, 2.0, LBLUE);
            c.fill(3.5, 13.5, 7.0, 1.3, GRAY);
        }
        AddColumns => {
            c.fill(1.5, 2.5, 4.0, 11.0, LGRAY);
            c.fill(6.0, 2.5, 4.0, 11.0, LGRAY);
            c.plus(12.5, 8.0, 3.0, GREEN);
        }
        SizeColumns => {
            c.fill(1.5, 3.0, 13.0, 2.5, LGRAY);
            c.line(&[(1.0, 11.0), (15.0, 11.0)], BLUE, 1.2);
            c.line(&[(3.5, 9.0), (1.2, 11.0), (3.5, 13.0)], BLUE, 1.2);
            c.line(&[(12.5, 9.0), (14.8, 11.0), (12.5, 13.0)], BLUE, 1.2);
        }
        ItemCheck => {
            c.frame(2.5, 2.5, 11.0, 11.0, DGRAY, 1.0);
            c.line(&[(4.8, 8.2), (7.2, 10.8), (11.4, 5.2)], BLUE, 1.5);
        }
        FileExt => {
            c.page(2.0, 1.0, 9.0, 14.0);
            c.fill(6.0, 9.5, 9.0, 4.5, BLUE);
            c.text(10.5, 11.8, ".txt", 3.4, WHITE);
        }
        HiddenItems => {
            c.closed(
                &[
                    (1.0, 8.0),
                    (4.5, 4.5),
                    (11.5, 4.5),
                    (15.0, 8.0),
                    (11.5, 11.5),
                    (4.5, 11.5),
                ],
                GRAY,
                1.0,
            );
            c.disc(8.0, 8.0, 2.2, GRAY);
        }
        HideSelected => {
            c.closed(
                &[
                    (1.0, 8.0),
                    (4.5, 4.5),
                    (11.5, 4.5),
                    (15.0, 8.0),
                    (11.5, 11.5),
                    (4.5, 11.5),
                ],
                GRAY,
                1.0,
            );
            c.disc(8.0, 8.0, 2.2, GRAY);
            c.line(&[(2.5, 14.0), (13.5, 2.0)], RED, 1.6);
        }
        Options => {
            c.page(1.5, 2.0, 10.0, 12.5);
            c.gear(11.0, 11.0, 3.0, GRAY);
        }
        Terminal => {
            c.fill(1.0, 2.5, 14.0, 11.0, Color32::from_rgb(30, 30, 30));
            c.line(&[(3.5, 6.0), (6.0, 8.0), (3.5, 10.0)], WHITE, 1.1);
            c.line(&[(7.5, 10.5), (11.0, 10.5)], WHITE, 1.1);
        }
        Help => {
            c.disc(8.0, 8.0, 6.5, BLUE);
            c.text(8.0, 8.4, "?", 10.0, WHITE);
        }
        Eject => {
            c.poly(&[(8.0, 2.5), (14.0, 9.0), (2.0, 9.0)], DGRAY);
            c.fill(2.5, 10.5, 11.0, 2.5, DGRAY);
        }
        Format => {
            c.rfill(1.0, 4.0, 14.0, 8.0, 1.0, Color32::from_rgb(170, 176, 182));
            c.line(&[(3.0, 8.0), (13.0, 8.0)], WHITE, 1.2);
            c.line(&[(8.0, 5.5), (8.0, 10.5)], WHITE, 1.2);
        }
        Cleanup => {
            c.fill(2.5, 3.0, 11.0, 9.0, LGRAY);
            c.line(&[(2.5, 12.0), (13.5, 3.0)], RED, 1.4);
            c.disc(11.0, 12.0, 2.2, FOLDER_FRONT);
        }
        Optimize => {
            c.rfill(1.5, 3.0, 13.0, 10.0, 1.0, LGRAY);
            c.fill(3.0, 5.0, 3.0, 3.0, BLUE);
            c.fill(7.0, 5.0, 3.0, 3.0, GREEN);
            c.fill(3.0, 9.0, 3.0, 3.0, GREEN);
            c.fill(7.0, 9.0, 3.0, 3.0, BLUE);
        }
        Autoplay => {
            c.disc(8.0, 8.0, 6.5, GREEN);
            c.poly(&[(6.0, 4.8), (6.0, 11.2), (11.5, 8.0)], WHITE);
        }
        Wallpaper => {
            c.monitor(1.5, 2.0, 13.0, 9.0, LBLUE);
            c.poly(
                &[(3.5, 9.5), (6.5, 5.5), (9.0, 8.5), (10.5, 7.0), (12.5, 9.5)],
                GREEN,
            );
        }
        RotateLeft => {
            arc(c, 8.0, 8.5, 5.0, 0.4, 5.3, BLUE, 1.6);
            c.poly(&[(1.2, 3.5), (6.2, 3.5), (3.7, 7.5)], BLUE);
        }
        RotateRight => {
            arc(c, 8.0, 8.5, 5.0, -2.1, 2.8, BLUE, 1.6);
            c.poly(&[(14.8, 3.5), (9.8, 3.5), (12.3, 7.5)], BLUE);
        }
        Slideshow => {
            c.monitor(1.5, 2.0, 13.0, 9.0, LBLUE);
            c.poly(&[(6.5, 4.0), (6.5, 9.0), (10.5, 6.5)], WHITE);
        }
        Play => {
            c.disc(8.0, 8.0, 6.5, BLUE);
            c.poly(&[(6.0, 4.8), (6.0, 11.2), (11.5, 8.0)], WHITE);
        }
        RunAsAdmin => {
            c.poly(
                &[
                    (8.0, 1.5),
                    (14.0, 3.5),
                    (13.0, 10.0),
                    (8.0, 14.5),
                    (3.0, 10.0),
                    (2.0, 3.5),
                ],
                BLUE,
            );
            c.line(&[(5.5, 8.0), (7.5, 10.0), (11.0, 5.5)], WHITE, 1.4);
        }
        NewWindow => {
            c.fill(1.5, 2.5, 13.0, 11.0, WHITE);
            c.frame(1.5, 2.5, 13.0, 11.0, GRAY, 0.9);
            c.fill(1.5, 2.5, 13.0, 2.5, LBLUE);
            c.plus(11.5, 11.0, 3.0, GREEN);
        }
        DeleteHistory => {
            c.ring(7.0, 8.0, 5.5, GRAY, 1.1);
            c.line(&[(7.0, 4.5), (7.0, 8.0), (9.5, 9.0)], GRAY, 1.1);
            c.line(&[(10.0, 10.0), (15.0, 15.0)], RED, 1.8);
            c.line(&[(15.0, 10.0), (10.0, 15.0)], RED, 1.8);
        }
        Undo => {
            c.line(
                &[
                    (5.0, 6.0),
                    (10.0, 6.0),
                    (13.0, 9.0),
                    (10.0, 12.0),
                    (7.0, 12.0),
                ],
                BLUE,
                1.5,
            );
            c.poly(&[(1.8, 6.0), (6.5, 2.5), (6.5, 9.5)], BLUE);
        }
        Redo => {
            c.line(
                &[
                    (11.0, 6.0),
                    (6.0, 6.0),
                    (3.0, 9.0),
                    (6.0, 12.0),
                    (9.0, 12.0),
                ],
                BLUE,
                1.5,
            );
            c.poly(&[(14.2, 6.0), (9.5, 2.5), (9.5, 9.5)], BLUE);
        }
    }
}

fn star(cx: f32, cy: f32, ro: f32, ri: f32) -> Vec<Pt> {
    (0..10)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            let r = if i % 2 == 0 { ro } else { ri };
            (cx + a.cos() * r, cy + a.sin() * r)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn arc(c: &Cv, cx: f32, cy: f32, r: f32, a0: f32, a1: f32, col: Color32, wd: f32) {
    let n = 18;
    let pts: Vec<Pt> = (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            (cx + a.cos() * r, cy + a.sin() * r)
        })
        .collect();
    c.line(&pts, col, wd);
}

/// Pick the right folder/file icon for a name (used by [`crate::Entry`]).
///
/// This runs for every visible file on every repaint (via `Entry::icon()`),
/// so it compares case-insensitively without allocating a lowercase copy of
/// `ext` first.
pub fn icon_for_extension(ext: &str) -> Icon {
    let one = |s: &str| ext.eq_ignore_ascii_case(s);
    let any = |list: &[&str]| list.iter().any(|s| ext.eq_ignore_ascii_case(s));
    if any(&[
        "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff",
    ]) {
        Icon::FileImage
    } else if any(&["mp3", "flac", "wav", "ogg", "m4a", "opus", "aac"]) {
        Icon::FileAudio
    } else if any(&["mp4", "mkv", "avi", "mov", "webm", "wmv"]) {
        Icon::FileVideo
    } else if any(&[
        "zip", "tar", "gz", "xz", "bz2", "7z", "rar", "zst", "deb", "rpm",
    ]) {
        Icon::FileArchive
    } else if any(&[
        "rs", "c", "h", "cpp", "py", "js", "ts", "json", "toml", "yaml", "yml", "html", "css",
        "sh", "go", "java",
    ]) {
        Icon::FileCode
    } else if any(&["txt", "md", "log", "csv", "ini", "conf", "cfg"]) {
        Icon::FileText
    } else if one("pdf") {
        Icon::FilePdf
    } else if any(&["exe", "bin", "appimage", "run"]) {
        Icon::FileExec
    } else {
        Icon::File
    }
}
