//! A Windows 10 File Explorer (ribbon style) UI for egui.
pub mod dialogs;
pub mod file_list;
pub mod icons;
pub mod model;
pub mod props;
pub mod ribbon;
pub mod shell;
pub mod theme;

pub use icons::{Icon, initialize_icon_placeholders, load_app_icon_image, set_icon_directory};
pub use model::*;
pub use ribbon::Ribbon;
pub use shell::{Explorer, ExplorerOptions};
