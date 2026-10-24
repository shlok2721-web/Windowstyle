//! Persistent preferences (view modes, sorting, columns, window, ribbon) as TOML.
//! **No egui / egui_explorer dependency.**
//!
//! The enums here deliberately *mirror* the ones in `egui_explorer` instead of
//! importing them. They are stored as strings, and unknown or renamed values fall
//! back to a default instead of failing the whole file. If egui_explorer later adds
//! or renames a variant, only your small mapping code in the app has to change,
//! never this crate or users' saved files.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const CURRENT_VERSION: u32 = 2;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Declares a string-serialised enum with a lenient, never-failing parser.
///
/// Serializing directly as `&str` avoids allocating a temporary `String` for
/// every enum in a settings save. This matters for large `per_folder` maps.
macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $s:literal),+ $(,)? } default $def:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str { match self { $($name::$variant => $s),+ } }
            /// Parse, falling back to the default for unknown values.
            pub fn parse_lossy(s: &str) -> Self {
                match s { $($s => $name::$variant,)+ _ => $name::$def }
            }
        }
        impl Default for $name { fn default() -> Self { $name::$def } }
        impl From<String> for $name { fn from(s: String) -> Self { Self::parse_lossy(&s) } }
        impl From<$name> for String { fn from(v: $name) -> String { v.as_str().to_owned() } }
        impl serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct EnumVisitor;
                impl<'de_vis> serde::de::Visitor<'de_vis> for EnumVisitor {
                    type Value = $name;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a string setting value")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok($name::parse_lossy(value))
                    }
                }
                deserializer.deserialize_str(EnumVisitor)
            }
        }
    };
}

string_enum!(
    /// Mirrors `egui_explorer::ViewMode`.
    ViewMode {
        ExtraLargeIcons = "extra-large-icons", LargeIcons = "large-icons", MediumIcons = "medium-icons",
        SmallIcons = "small-icons", List = "list", Details = "details", Tiles = "tiles", Content = "content",
    } default Details
);
string_enum!(
    /// Mirrors `egui_explorer::SortKey` (also used for column identity).
    SortKey { Name = "name", DateModified = "date-modified", Type = "type", Size = "size" } default Name
);
string_enum!(
    /// Mirrors `egui_explorer::SortOrder`.
    SortOrder { Ascending = "ascending", Descending = "descending" } default Ascending
);
string_enum!(
    /// Mirrors `egui_explorer::GroupKey`.
    GroupKey { None = "none", Name = "name", DateModified = "date-modified", Type = "type", Size = "size" } default None
);
string_enum!(
    /// Mirrors `egui_explorer::RibbonMode`.
    RibbonMode { Expanded = "expanded", Minimized = "minimized", Hidden = "hidden" } default Expanded
);

fn yes() -> bool {
    true
}

fn default_thumbnail_edge() -> u32 {
    256
}

fn default_navigation_icon_size() -> u32 {
    18
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowSettings {
    pub width: f32,
    pub height: f32,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub maximized: bool,
    /// `true` = our own title bar, `false` = native decorations.
    pub frameless: bool,
}
impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1100.0,
            height: 700.0,
            x: None,
            y: None,
            maximized: false,
            frameless: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSetting {
    pub key: SortKey,
    pub width: f32,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    /// Layout used when the app starts: `details`, `list`, `tiles`, `content`,
    /// `small-icons`, `medium-icons`, `large-icons` or `extra-large-icons`.
    pub mode: ViewMode,
    /// `true`: the layout last chosen in the app is saved here and restored next time.
    /// `false` (default): every start uses `mode` exactly as written.
    pub remember: bool,
    pub sort_key: SortKey,
    pub sort_order: SortOrder,
    pub group: GroupKey,
    pub folders_first: bool,
    pub columns: Vec<ColumnSetting>,
}
impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            mode: ViewMode::Details,
            remember: false,
            sort_key: SortKey::Name,
            sort_order: SortOrder::Ascending,
            group: GroupKey::None,
            folders_first: true,
            columns: vec![
                ColumnSetting {
                    key: SortKey::Name,
                    width: 260.0,
                    visible: true,
                },
                ColumnSetting {
                    key: SortKey::DateModified,
                    width: 140.0,
                    visible: true,
                },
                ColumnSetting {
                    key: SortKey::Type,
                    width: 120.0,
                    visible: true,
                },
                ColumnSetting {
                    key: SortKey::Size,
                    width: 80.0,
                    visible: true,
                },
            ],
        }
    }
}

/// Per-folder override (Windows "remembers view per folder").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderView {
    pub mode: ViewMode,
    pub sort_key: SortKey,
    pub sort_order: SortOrder,
    pub group: GroupKey,
}
impl Default for FolderView {
    fn default() -> Self {
        let v = ViewSettings::default();
        Self {
            mode: v.mode,
            sort_key: v.sort_key,
            sort_order: v.sort_order,
            group: v.group,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowSettings {
    pub hidden_items: bool,
    pub file_name_extensions: bool,
    pub item_check_boxes: bool,
    pub nav_pane: bool,
    pub preview_pane: bool,
    pub details_pane: bool,
}
impl Default for ShowSettings {
    fn default() -> Self {
        Self {
            hidden_items: false,
            file_name_extensions: true,
            item_check_boxes: false,
            nav_pane: true,
            preview_pane: false,
            details_pane: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RibbonSettings {
    /// Ribbon state at start: `expanded` (always open), `minimized` (tabs only, click a tab
    /// to peek) or `hidden` (Ctrl+F1 or Alt shows it).
    pub mode: RibbonMode,
    /// `true`: the state last used in the app is saved here and restored next time.
    /// `false` (default): every start uses `mode` exactly as written.
    pub remember: bool,
    /// Tapping Alt shows the key tips (F, H, S, V ...). Set `false` to turn that off.
    pub key_tips: bool,
    pub qat_below_ribbon: bool,
    /// Command identifiers chosen by the app (kept as strings on purpose).
    pub qat_items: Vec<String>,
    pub active_tab: String,
}
impl Default for RibbonSettings {
    fn default() -> Self {
        Self {
            mode: RibbonMode::Expanded,
            remember: false,
            key_tips: true,
            qat_below_ribbon: false,
            qat_items: ["properties", "new-folder", "undo", "redo"]
                .map(String::from)
                .to_vec(),
            active_tab: "home".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NavSettings {
    /// Whether the "This PC" node in the navigation pane starts expanded (showing its
    /// drives right away) or collapsed (a single row until clicked). `true` keeps the
    /// previous behaviour.
    pub this_pc_expanded: bool,
    /// How often, in seconds, the app rescans block devices for newly attached or
    /// detached drives (`lsblk`). 0 means never; otherwise the consumer clamps to 5..=20.
    #[serde(default = "default_drive_poll_secs")]
    pub drive_poll_secs: u32,
}
impl Default for NavSettings {
    fn default() -> Self {
        Self {
            this_pc_expanded: true,
            drive_poll_secs: default_drive_poll_secs(),
        }
    }
}
fn default_drive_poll_secs() -> u32 {
    5
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub window: WindowSettings,
    pub view: ViewSettings,
    pub show: ShowSettings,
    pub ribbon: RibbonSettings,
    /// Generate image/video thumbnails on demand while browsing.
    #[serde(default = "yes")]
    pub thumbnails: bool,
    /// Apply a dark palette to the application interface only.
    #[serde(default)]
    pub dark_mode: bool,
    /// Include application-data folders (`.var`, browser profiles, ...) in file search.
    #[serde(default, rename = "search-app-data")]
    pub search_app_data: bool,
    /// Maximum thumbnail edge in pixels. Invalid values are clamped by the consumer.
    #[serde(default = "default_thumbnail_edge")]
    pub thumbnail_edge: u32,
    /// Icon size in pixels for the left navigation pane (`navigation-icon-size = 22` in
    /// settings.toml). The row height grows with it; the consumer clamps it to 12..=48.
    #[serde(
        default = "default_navigation_icon_size",
        rename = "navigation-icon-size"
    )]
    pub navigation_icon_size: u32,
    /// Command used by "E-mail". Each token containing `{file}` is repeated once per
    /// attachment with the path substituted; without `{file}`, paths are appended.
    #[serde(default = "default_email_command")]
    pub email_command: String,
    pub last_location: Option<PathBuf>,
    /// Folder (as string path) -> remembered view.
    pub per_folder: BTreeMap<String, FolderView>,
    pub nav: NavSettings,
}

fn default_email_command() -> String {
    "xdg-email --utf8 --attach {file}".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            window: WindowSettings::default(),
            view: ViewSettings::default(),
            show: ShowSettings::default(),
            ribbon: RibbonSettings::default(),
            thumbnails: true,
            dark_mode: false,
            search_app_data: false,
            thumbnail_edge: 256,
            navigation_icon_size: default_navigation_icon_size(),
            email_command: default_email_command(),
            last_location: None,
            per_folder: BTreeMap::new(),
            nav: NavSettings::default(),
        }
    }
}

#[derive(Debug)]
pub enum SettingsError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}
impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "settings i/o error: {e}"),
            Self::Parse(e) => write!(f, "settings parse error: {e}"),
            Self::Serialize(e) => write!(f, "settings serialize error: {e}"),
        }
    }
}
impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

impl Settings {
    pub fn from_toml(s: &str) -> Result<Self, SettingsError> {
        let mut settings: Self = toml::from_str(s).map_err(SettingsError::Parse)?;
        // Older settings files remain loadable; version updates in memory and
        // are persisted only when a write is otherwise needed.
        if settings.version < CURRENT_VERSION {
            settings.version = CURRENT_VERSION;
        }
        Ok(settings)
    }
    pub fn to_toml(&self) -> Result<String, SettingsError> {
        toml::to_string_pretty(self).map_err(SettingsError::Serialize)
    }

    /// Load from `path`. A missing file is not an error (returns defaults).
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(SettingsError::Io(e)),
        }
    }

    /// Return defaults alongside a load error. For malformed TOML, try to preserve
    /// the original file under a unique `*.bad` name before returning defaults.
    /// If the filesystem cannot create a hard link, the original remains untouched.
    pub fn load_or_default(path: &Path) -> (Self, Option<SettingsError>) {
        match Self::load(path) {
            Ok(s) => (s, None),
            Err(e) => {
                let malformed = match &e {
                    SettingsError::Parse(_) => true,
                    // `read_to_string` reports invalid UTF-8 as InvalidData. It is
                    // a corrupt TOML file, not a transient filesystem failure.
                    SettingsError::Io(error) => error.kind() == std::io::ErrorKind::InvalidData,
                    SettingsError::Serialize(_) => false,
                };
                if malformed {
                    quarantine_corrupt_file(path);
                }
                (Self::default(), Some(e))
            }
        }
    }

    /// Atomic save (write a unique temporary file, then rename); unchanged
    /// settings are not rewritten. Parent directories are created as needed.
    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        use std::io::Write;

        let text = self.to_toml()?;
        match std::fs::read(path) {
            Ok(existing) if existing == text.as_bytes() => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SettingsError::Io(e)),
        }

        // Atomic replacement otherwise resets an existing file's mode to the
        // process umask defaults. Preserve permissions for regular settings files.
        // Do not copy symlink metadata: `rename` replaces the link itself.
        let existing_permissions = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => Some(metadata.permissions()),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(SettingsError::Io(e)),
        };

        let parent = path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = parent {
            std::fs::create_dir_all(dir).map_err(SettingsError::Io)?;
        }
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let (tmp, mut file) = loop {
            let suffix = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!(".{file_name}.tmp-{}-{suffix}", std::process::id());
            let candidate = parent.map_or_else(|| PathBuf::from(&name), |d| d.join(&name));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(SettingsError::Io(e)),
            }
        };
        let write_result = (|| {
            file.write_all(text.as_bytes())?;
            if let Some(permissions) = existing_permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(SettingsError::Io(e));
        }
        drop(file);
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(SettingsError::Io(e));
        }
        #[cfg(unix)]
        if let Some(dir) = parent
            && let Ok(handle) = std::fs::File::open(dir)
        {
            handle.sync_all().map_err(SettingsError::Io)?;
        }
        Ok(())
    }

    pub fn folder_view(&self, folder: &Path) -> Option<&FolderView> {
        if let Some(path) = folder.to_str() {
            if !uses_reserved_folder_key_prefix(path) {
                // The common case stays allocation-free on lookup.
                return self.per_folder.get(path);
            }
            return self.per_folder.get(&escape_text_folder_key(path));
        }
        self.per_folder.get(&non_unicode_folder_key(folder))
    }
    pub fn set_folder_view(&mut self, folder: &Path, v: FolderView) {
        let key = folder
            .to_str()
            .map(|path| {
                if uses_reserved_folder_key_prefix(path) {
                    escape_text_folder_key(path)
                } else {
                    path.to_owned()
                }
            })
            .unwrap_or_else(|| non_unicode_folder_key(folder));
        self.per_folder.insert(key, v);
    }
}

const NON_UNICODE_FOLDER_PREFIX: &str = "@windowstyle:raw-path-bytes:";
const ESCAPED_TEXT_FOLDER_PREFIX: &str = "@windowstyle:escaped-text-path:";

fn uses_reserved_folder_key_prefix(path: &str) -> bool {
    path.starts_with(NON_UNICODE_FOLDER_PREFIX) || path.starts_with(ESCAPED_TEXT_FOLDER_PREFIX)
}

fn escape_text_folder_key(path: &str) -> String {
    let mut key = String::with_capacity(ESCAPED_TEXT_FOLDER_PREFIX.len() + path.len());
    key.push_str(ESCAPED_TEXT_FOLDER_PREFIX);
    key.push_str(path);
    key
}

/// Non-UTF-8 filesystem paths cannot safely be represented by `to_string_lossy`:
/// distinct paths may collapse to the same replacement-character string. Encode
/// raw Unix path bytes as hex in a reserved ASCII namespace, preserving the
/// public `BTreeMap<String, FolderView>` and TOML representation.
fn non_unicode_folder_key(folder: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = folder.as_os_str().as_bytes();
        let capacity = NON_UNICODE_FOLDER_PREFIX
            .len()
            .saturating_add(bytes.len().saturating_mul(2));
        let mut key = String::with_capacity(capacity);
        key.push_str(NON_UNICODE_FOLDER_PREFIX);
        for byte in bytes {
            key.push(HEX[(byte >> 4) as usize] as char);
            key.push(HEX[(byte & 0x0f) as usize] as char);
        }
        key
    }
    #[cfg(not(unix))]
    {
        // Windows filesystem paths are Unicode; retain a best-effort fallback on
        // platforms whose `OsStr` does not expose Unix's raw-byte representation.
        folder.to_string_lossy().into_owned()
    }
}

fn quarantine_corrupt_file(path: &Path) {
    let mut suffix = 0_u64;
    loop {
        let mut name = path.as_os_str().to_owned();
        name.push(".bad");
        if suffix > 0 {
            name.push(format!(".{suffix}"));
        }
        let candidate = PathBuf::from(name);

        // `rename` may replace an existing destination on Unix. `hard_link` is
        // atomic and fails with AlreadyExists, so a previous quarantine is never
        // overwritten. Linking within the same directory also avoids cross-device
        // failures; if hard links are unsupported, leave the source untouched.
        match std::fs::hard_link(path, &candidate) {
            Ok(()) => {
                // The backup exists before the original name is removed. If removal
                // fails, both links remain and the user's data is still preserved.
                let _ = std::fs::remove_file(path);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let Some(next) = suffix.checked_add(1) else {
                    return;
                };
                suffix = next;
            }
            Err(_) => return,
        }
    }
}

fn config_base(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
}

/// `$XDG_CONFIG_HOME/<app>/settings.toml` or `~/.config/<app>/settings.toml`.
/// Relative `XDG_CONFIG_HOME` values are ignored to avoid working-directory-dependent paths.
pub fn default_config_path(app: &str) -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    Some(config_base(xdg, home)?.join(app).join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut s = Settings::default();
        s.view.mode = ViewMode::Tiles;
        s.view.group = GroupKey::Type;
        s.ribbon.mode = RibbonMode::Hidden;
        s.last_location = Some("/home/me".into());
        s.set_folder_view(
            Path::new("/home/me/Pictures"),
            FolderView {
                mode: ViewMode::LargeIcons,
                ..Default::default()
            },
        );
        let text = s.to_toml().unwrap();
        assert!(text.contains("tiles"));
        assert_eq!(Settings::from_toml(&text).unwrap(), s);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let s = Settings::from_toml("[view]\nmode = \"list\"\n").unwrap();
        assert_eq!(s.view.mode, ViewMode::List);
        assert_eq!(s.view.sort_key, SortKey::Name);
        assert_eq!(s.window, WindowSettings::default());
        assert!(s.thumbnails);
    }

    #[test]
    fn new_settings_are_opt_in_and_legacy_fields_default() {
        let s = Settings::from_toml("[view]\nmode = \"list\"\n").unwrap();
        assert!(!s.dark_mode);
        assert!(s.thumbnails);
        assert_eq!(s.thumbnail_edge, 256);
        assert_eq!(s.version, CURRENT_VERSION);
    }

    #[test]
    fn unchanged_save_does_not_rewrite_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let settings = Settings::default();
        settings.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let modified_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        settings.save(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified_before
        );
    }

    #[test]
    fn unknown_enum_values_and_keys_are_tolerated() {
        let s = Settings::from_toml(
            "future_key = 1\n[view]\nmode = \"holographic\"\nsort_key = \"size\"\n",
        )
        .unwrap();
        assert_eq!(s.view.mode, ViewMode::Details); // default
        assert_eq!(s.view.sort_key, SortKey::Size);
    }

    #[test]
    fn every_variant_round_trips_through_its_string() {
        for v in ViewMode::ALL {
            assert_eq!(ViewMode::parse_lossy(v.as_str()), *v);
        }
        for v in GroupKey::ALL {
            assert_eq!(GroupKey::parse_lossy(v.as_str()), *v);
        }
        for v in RibbonMode::ALL {
            assert_eq!(RibbonMode::parse_lossy(v.as_str()), *v);
        }
    }

    #[test]
    fn file_io() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested/dir/settings.toml");
        assert_eq!(Settings::load(&p).unwrap(), Settings::default()); // missing -> default
        let mut s = Settings::default();
        s.show.hidden_items = true;
        s.save(&p).unwrap();
        assert!(!p.with_extension("toml.tmp").exists());
        assert_eq!(Settings::load(&p).unwrap(), s);
    }

    #[test]
    fn corrupt_file_is_preserved_and_defaults_returned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(&p, "this is [not toml").unwrap();
        let (s, err) = Settings::load_or_default(&p);
        assert!(err.is_some());
        assert_eq!(s, Settings::default());
        assert!(dir.path().join("settings.toml.bad").exists());
    }

    #[test]
    fn corrupt_file_does_not_overwrite_existing_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        let old_bad = dir.path().join("settings.toml.bad");
        std::fs::write(&p, "this is [not toml").unwrap();
        std::fs::write(&old_bad, "previous corrupt settings").unwrap();

        let (_, err) = Settings::load_or_default(&p);
        assert!(err.is_some());
        assert_eq!(
            std::fs::read(&old_bad).unwrap(),
            b"previous corrupt settings"
        );
        let second_quarantine = dir.path().join("settings.toml.bad.1");
        assert_eq!(
            std::fs::read(second_quarantine).unwrap(),
            b"this is [not toml"
        );
        assert!(!p.exists());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_settings_are_quarantined_without_data_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let invalid_toml = [0xff, 0xfe, 0x00, 0x61];
        std::fs::write(&path, invalid_toml).unwrap();

        let (settings, error) = Settings::load_or_default(&path);
        assert!(error.is_some());
        assert_eq!(settings, Settings::default());
        assert_eq!(
            std::fs::read(dir.path().join("settings.toml.bad")).unwrap(),
            invalid_toml
        );
        assert!(!path.exists());
    }

    #[test]
    fn relative_xdg_config_home_is_ignored() {
        let home = std::env::temp_dir().join("windowstyle-home");
        assert_eq!(
            config_base(Some(PathBuf::from("relative-config")), Some(home.clone())),
            Some(home.join(".config")),
        );
        assert_eq!(
            config_base(Some(PathBuf::from("relative-config")), None),
            None,
        );
        let absolute_xdg = std::env::temp_dir().join("windowstyle-xdg");
        assert_eq!(
            config_base(Some(absolute_xdg.clone()), Some(home.clone())),
            Some(absolute_xdg),
        );
        assert_eq!(
            config_base(None, Some(PathBuf::from("relative-home"))),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_folder_paths_do_not_collide_and_survive_toml_round_trip() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(b"/tmp/windowstyle-\xff".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/windowstyle-\xfe".to_vec()));
        let first_view = FolderView {
            mode: ViewMode::LargeIcons,
            ..FolderView::default()
        };
        let second_view = FolderView {
            mode: ViewMode::Tiles,
            ..FolderView::default()
        };
        // A legal UTF-8 path resembling the encoded-key namespace must remain
        // distinct from the non-UTF-8 path's internal representation.
        let encoded_looking_path = PathBuf::from(non_unicode_folder_key(&first));
        let encoded_looking_view = FolderView {
            mode: ViewMode::List,
            ..FolderView::default()
        };

        let mut settings = Settings::default();
        settings.set_folder_view(&first, first_view.clone());
        settings.set_folder_view(&second, second_view.clone());
        settings.set_folder_view(&encoded_looking_path, encoded_looking_view.clone());
        assert_eq!(settings.folder_view(&first), Some(&first_view));
        assert_eq!(settings.folder_view(&second), Some(&second_view));
        assert_eq!(
            settings.folder_view(&encoded_looking_path),
            Some(&encoded_looking_view)
        );
        assert_ne!(
            non_unicode_folder_key(&first),
            non_unicode_folder_key(&second)
        );

        let serialized = settings.to_toml().unwrap();
        let restored = Settings::from_toml(&serialized).unwrap();
        assert_eq!(restored.folder_view(&first), Some(&first_view));
        assert_eq!(restored.folder_view(&second), Some(&second_view));
        assert_eq!(
            restored.folder_view(&encoded_looking_path),
            Some(&encoded_looking_view)
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_file_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let mut settings = Settings::default();
        settings.save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        settings.show.hidden_items = true;
        settings.save(&path).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }
}
