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
macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $s:literal),+ $(,)? } default $def:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(from = "String", into = "String")]
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
    /// Persisted opt-in for building/loading the recursive filesystem index.
    /// Disabled by default so first launch never starts a full-tree scan.
    #[serde(default)]
    pub indexing_enabled: bool,
    /// Maximum thumbnail edge in pixels. Invalid values are clamped by the consumer.
    #[serde(default = "default_thumbnail_edge")]
    pub thumbnail_edge: u32,
    /// Command used by "E-mail". Each token containing `{file}` is repeated once per
    /// attachment with the path substituted; without `{file}`, paths are appended.
    #[serde(default = "default_email_command")]
    pub email_command: String,
    pub last_location: Option<PathBuf>,
    /// Folder (as string path) -> remembered view.
    pub per_folder: BTreeMap<String, FolderView>,
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
            indexing_enabled: false,
            thumbnail_edge: 256,
            email_command: default_email_command(),
            last_location: None,
            per_folder: BTreeMap::new(),
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
impl std::error::Error for SettingsError {}

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

    /// Never fails: on a corrupt file the old one is renamed to `*.bad` (so the user
    /// does not lose it) and defaults are returned along with the error.
    pub fn load_or_default(path: &Path) -> (Self, Option<SettingsError>) {
        match Self::load(path) {
            Ok(s) => (s, None),
            Err(e) => {
                if matches!(&e, SettingsError::Parse(_)) {
                    // Never overwrite a previously quarantined settings file.
                    let mut suffix = 0_u64;
                    let candidate = loop {
                        let mut name = path.as_os_str().to_owned();
                        name.push(".bad");
                        if suffix > 0 {
                            name.push(format!(".{suffix}"));
                        }
                        let candidate = PathBuf::from(name);
                        if !candidate.exists() {
                            break candidate;
                        }
                        suffix = suffix.saturating_add(1);
                    };
                    // If quarantine fails, leave the original file in place. The host can
                    // then avoid persisting defaults over a file it could not preserve.
                    let _ = std::fs::rename(path, candidate);
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
        if let Err(e) = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(SettingsError::Io(e));
        }
        drop(file);
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(SettingsError::Io(e));
        }
        #[cfg(unix)]
        if let Some(dir) = parent {
            if let Ok(handle) = std::fs::File::open(dir) {
                handle.sync_all().map_err(SettingsError::Io)?;
            }
        }
        Ok(())
    }

    pub fn folder_view(&self, folder: &Path) -> Option<&FolderView> {
        self.per_folder.get(folder.to_string_lossy().as_ref())
    }
    pub fn set_folder_view(&mut self, folder: &Path, v: FolderView) {
        self.per_folder
            .insert(folder.to_string_lossy().into_owned(), v);
    }
}

/// `$XDG_CONFIG_HOME/<app>/settings.toml` or `~/.config/<app>/settings.toml`.
pub fn default_config_path(app: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(app).join("settings.toml"))
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
        assert!(!s.indexing_enabled);
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
        assert!(dir.path().join("settings.toml.bad.1").exists());
        assert!(!p.exists());
    }
}
