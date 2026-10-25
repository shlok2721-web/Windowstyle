//! The Windows 10 ribbon: tab strip, groups, large/small/split buttons, the
//! layout gallery, contextual tabs, key tips and the Expanded / Minimized /
//! Hidden display modes.

use egui::containers::menu::SubMenu;
use egui::epaint::StrokeKind;
use egui::{
    Align2, Area, Color32, CornerRadius, Id, Key, Order, Painter, Popup, Rect, Response, Sense,
    Stroke, Ui, pos2, vec2,
};

use crate::icons::Icon;
use crate::model::*;
use crate::theme::{self, colors};

pub const TAB_H: f32 = 24.0;
pub const BODY_H: f32 = 92.0;
const LARGE_H: f32 = 74.0;
const SMALL_H: f32 = 22.0;
fn menu_hover() -> Color32 {
    if theme::dark_mode() {
        Color32::from_rgb(54, 54, 54)
    } else {
        Color32::from_rgb(145, 201, 247)
    }
}
fn menu_bg() -> Color32 {
    if theme::dark_mode() {
        Color32::from_rgb(37, 37, 38)
    } else {
        Color32::from_rgb(242, 242, 242)
    }
}

// ---------------------------------------------------------------------------
// Definition types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item {
        label: &'static str,
        cmd: Command,
        icon: Option<Icon>,
        shortcut: Option<&'static str>,
    },
    Sep,
    Header(&'static str),
    Sub {
        label: &'static str,
        items: Vec<MenuEntry>,
    },
}

impl MenuEntry {
    pub fn item(label: &'static str, cmd: Command) -> Self {
        MenuEntry::Item {
            label,
            cmd,
            icon: None,
            shortcut: None,
        }
    }
    pub fn icon(self, i: Icon) -> Self {
        match self {
            MenuEntry::Item {
                label,
                cmd,
                shortcut,
                ..
            } => MenuEntry::Item {
                label,
                cmd,
                icon: Some(i),
                shortcut,
            },
            o => o,
        }
    }
    pub fn key(self, s: &'static str) -> Self {
        match self {
            MenuEntry::Item {
                label, cmd, icon, ..
            } => MenuEntry::Item {
                label,
                cmd,
                icon,
                shortcut: Some(s),
            },
            o => o,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// Click runs the command.
    Plain,
    /// Main part runs the command, the arrow part opens the menu.
    Split,
    /// The whole button opens the menu.
    Dropdown,
    /// Check box + label (Show/hide group). Checked state comes from the command.
    Check,
}

#[derive(Clone, Debug)]
pub struct Button {
    pub label: &'static str,
    pub icon: Icon,
    pub cmd: Option<Command>,
    pub kind: ButtonKind,
    pub menu: Vec<MenuEntry>,
    pub tip: &'static str,
}

impl Button {
    pub fn new(label: &'static str, icon: Icon, cmd: Command) -> Self {
        Self {
            label,
            icon,
            cmd: Some(cmd),
            kind: ButtonKind::Plain,
            menu: vec![],
            tip: "",
        }
    }
    pub fn dropdown(label: &'static str, icon: Icon, menu: Vec<MenuEntry>) -> Self {
        Self {
            label,
            icon,
            cmd: None,
            kind: ButtonKind::Dropdown,
            menu,
            tip: "",
        }
    }
    pub fn split(label: &'static str, icon: Icon, cmd: Command, menu: Vec<MenuEntry>) -> Self {
        Self {
            label,
            icon,
            cmd: Some(cmd),
            kind: ButtonKind::Split,
            menu,
            tip: "",
        }
    }
    pub fn check(label: &'static str, cmd: Command) -> Self {
        Self {
            label,
            icon: Icon::Check,
            cmd: Some(cmd),
            kind: ButtonKind::Check,
            menu: vec![],
            tip: "",
        }
    }
    pub fn tip(mut self, t: &'static str) -> Self {
        self.tip = t;
        self
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Large(Button),
    /// Up to three small buttons stacked vertically.
    Stack(Vec<Button>),
    /// The 8-entry "Layout" gallery of the View tab.
    LayoutGallery,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub title: &'static str,
    pub items: Vec<Item>,
}

/// Which kind of selection makes a contextual tab appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Drive,
    Picture,
    Application,
    Archive,
    Media,
}

impl ContextKind {
    pub fn label(self) -> &'static str {
        match self {
            ContextKind::Drive => "Drive Tools",
            ContextKind::Picture => "Picture Tools",
            ContextKind::Application => "Application Tools",
            ContextKind::Archive => "Compressed Folder Tools",
            ContextKind::Media => "Media Tools",
        }
    }
    pub fn color(self) -> Color32 {
        match self {
            ContextKind::Drive => colors::ctx_drive(),
            ContextKind::Picture => colors::ctx_picture(),
            ContextKind::Application => colors::ctx_app(),
            ContextKind::Archive => colors::ctx_archive(),
            ContextKind::Media => colors::ctx_media(),
        }
    }
}

/// The contextual tab a single *file* entry would call for, judged by its icon
/// class. `None` for files that have no contextual tab.
pub(crate) fn file_context(e: &Entry) -> Option<ContextKind> {
    match e.icon() {
        Icon::FileImage => Some(ContextKind::Picture),
        Icon::FileArchive => Some(ContextKind::Archive),
        Icon::FileExec => Some(ContextKind::Application),
        Icon::FileAudio | Icon::FileVideo => Some(ContextKind::Media),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub id: &'static str,
    pub title: &'static str,
    pub key_tip: &'static str,
    pub context: Option<ContextKind>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Debug)]
pub struct Ribbon {
    pub tabs: Vec<Tab>,
}

/// Where the contextual header ("Picture Tools") should be painted.
pub struct ContextHeader {
    pub x_min: f32,
    pub x_max: f32,
    pub label: &'static str,
    pub color: Color32,
}

#[derive(Default)]
pub struct RibbonOut {
    pub context_header: Option<ContextHeader>,
    /// Total height consumed in the layout (0 when hidden).
    pub height: f32,
}

// ---------------------------------------------------------------------------
// The Windows 10 definition
// ---------------------------------------------------------------------------

impl Ribbon {
    pub fn windows10() -> Self {
        use Command as C;
        use Icon as I;
        use MenuEntry as M;

        let home = Tab {
            id: "home",
            title: "Home",
            key_tip: "H",
            context: None,
            groups: vec![
                Group {
                    title: "Clipboard",
                    items: vec![
                        Item::Large(Button::new(
                            "Pin to Quick\naccess",
                            I::Pin,
                            C::PinToQuickAccess,
                        )),
                        Item::Large(Button::new("Copy", I::Copy, C::Copy).tip("Copy (Ctrl+C)")),
                        Item::Large(
                            Button::split(
                                "Paste",
                                I::Paste,
                                C::Paste,
                                vec![
                                    M::item("Paste", C::Paste).icon(I::Paste).key("Ctrl+V"),
                                    M::item("Paste shortcut", C::PasteShortcut)
                                        .icon(I::PasteShortcut),
                                ],
                            )
                            .tip("Paste (Ctrl+V)"),
                        ),
                        Item::Stack(vec![
                            Button::new("Cut", I::Cut, C::Cut).tip("Cut (Ctrl+X)"),
                            Button::new("Copy path", I::CopyPath, C::CopyPath),
                            Button::new("Paste shortcut", I::PasteShortcut, C::PasteShortcut),
                        ]),
                    ],
                },
                Group {
                    title: "Organize",
                    items: vec![
                        Item::Stack(vec![
                            Button::dropdown(
                                "Move to",
                                I::MoveTo,
                                vec![M::item("Choose location...", C::MoveTo)],
                            ),
                            Button::dropdown(
                                "Copy to",
                                I::CopyTo,
                                vec![M::item("Choose location...", C::CopyTo)],
                            ),
                        ]),
                        Item::Stack(vec![
                            Button::split(
                                "Delete",
                                I::Delete,
                                C::Delete,
                                vec![
                                    M::item("Recycle", C::Delete).icon(I::Trash).key("Del"),
                                    M::item("Permanently delete", C::DeletePermanently)
                                        .icon(I::Delete)
                                        .key("Shift+Del"),
                                ],
                            ),
                            Button::new("Rename", I::Rename, C::Rename).tip("Rename (F2)"),
                        ]),
                    ],
                },
                Group {
                    title: "New",
                    items: vec![
                        Item::Large(
                            Button::new("New\nfolder", I::NewFolder, C::NewFolder)
                                .tip("New folder (Ctrl+Shift+N)"),
                        ),
                        Item::Stack(vec![
                            Button::dropdown(
                                "New item",
                                I::NewItem,
                                vec![
                                    M::item("Folder", C::NewFolder).icon(I::Folder),
                                    M::item("Shortcut", C::NewShortcut).icon(I::PasteShortcut),
                                    M::item("Text Document", C::NewTextFile).icon(I::FileText),
                                ],
                            ),
                            Button::dropdown(
                                "Easy access",
                                I::EasyAccess,
                                vec![
                                    M::item("Pin to Quick access", C::PinToQuickAccess)
                                        .icon(I::Pin),
                                ],
                            ),
                        ]),
                    ],
                },
                Group {
                    title: "Open",
                    items: vec![
                        Item::Large(
                            Button::new("Properties", I::Properties, C::Properties)
                                .tip("Properties (Alt+Enter)"),
                        ),
                        Item::Stack(vec![
                            Button::split(
                                "Open",
                                I::Open,
                                C::Open,
                                vec![
                                    M::item("Open", C::Open).icon(I::Open),
                                    M::item("Open with...", C::OpenWith),
                                    M::Sep,
                                    M::item("Run", C::Run),
                                    M::item("Run in terminal", C::RunInTerminal),
                                ],
                            ),
                            Button::new("Edit", I::Edit, C::Edit),
                            Button::new("History", I::History, C::ShowHistory),
                        ]),
                    ],
                },
                Group {
                    title: "Select",
                    items: vec![Item::Stack(vec![
                        Button::new("Select all", I::SelectAll, C::SelectAll)
                            .tip("Select all (Ctrl+A)"),
                        Button::new("Select none", I::SelectNone, C::SelectNone),
                        Button::new("Invert selection", I::InvertSelection, C::InvertSelection),
                    ])],
                },
                Group {
                    title: "Maintenance",
                    items: vec![Item::Stack(vec![
                        Button::new("Audit thumbnails", I::Cleanup, C::AuditThumbnails)
                            .tip("Find missing or invalid thumbnails without generating them"),
                        Button::new(
                            "Generate missing",
                            I::FileImage,
                            C::GenerateMissingThumbnails,
                        )
                        .tip("Generate only missing, stale, or corrupt thumbnails"),
                        Button::new("Build index", I::Search, C::BuildIndex)
                            .tip("Open a terminal running 'sudo updatedb' to refresh the plocate database"),
                    ])],
                },
            ],
        };

        let share = Tab {
            id: "share",
            title: "Share",
            key_tip: "S",
            context: None,
            groups: vec![
                Group {
                    title: "Send",
                    items: vec![
                        Item::Large(Button::new("Share", I::Share, C::Share)),
                        Item::Large(Button::new("E-mail", I::Email, C::Email)),
                        Item::Large(Button::new("Zip", I::Compress, C::ShowCompressOptions)),
                        Item::Large(Button::new("Print", I::Print, C::Print)),
                    ],
                },
                Group {
                    title: "Security",
                    items: vec![Item::Large(Button::new(
                        "Permissions",
                        I::Properties,
                        C::AdvancedSecurity,
                    ))],
                },
            ],
        };

        let nav_menu = vec![M::item("Navigation pane", C::ToggleNavPane).icon(I::NavPane)];
        let sort_menu = vec![
            M::item("Name", C::SortBy(SortKey::Name)),
            M::item("Date modified", C::SortBy(SortKey::DateModified)),
            M::item("Type", C::SortBy(SortKey::Type)),
            M::item("Size", C::SortBy(SortKey::Size)),
            M::Sep,
            M::item("Ascending", C::SetSortOrder(SortOrder::Ascending)),
            M::item("Descending", C::SetSortOrder(SortOrder::Descending)),
        ];
        let group_menu = vec![
            M::item("(None)", C::GroupBy(GroupKey::None)),
            M::Sep,
            M::item("Name", C::GroupBy(GroupKey::Name)),
            M::item("Date modified", C::GroupBy(GroupKey::DateModified)),
            M::item("Type", C::GroupBy(GroupKey::Type)),
            M::item("Size", C::GroupBy(GroupKey::Size)),
            M::Sep,
            M::item("Ascending", C::SetSortOrder(SortOrder::Ascending)),
            M::item("Descending", C::SetSortOrder(SortOrder::Descending)),
        ];
        let add_cols_menu = vec![
            M::Header("Columns"),
            M::item("Name", C::ToggleColumn(SortKey::Name)),
            M::item("Date modified", C::ToggleColumn(SortKey::DateModified)),
            M::item("Type", C::ToggleColumn(SortKey::Type)),
            M::item("Size", C::ToggleColumn(SortKey::Size)),
        ];

        let view = Tab {
            id: "view",
            title: "View",
            key_tip: "V",
            context: None,
            groups: vec![
                Group {
                    title: "Panes",
                    items: vec![
                        Item::Large(Button::dropdown("Navigation\npane", I::NavPane, nav_menu)),
                        Item::Large(
                            Button::new("Preview pane", I::PreviewPane, C::TogglePreviewPane)
                                .tip("Preview pane (Alt+P)"),
                        ),
                        Item::Large(
                            Button::new("Details pane", I::DetailsPane, C::ToggleDetailsPane)
                                .tip("Details pane (Alt+Shift+P)"),
                        ),
                    ],
                },
                Group {
                    title: "Layout",
                    items: vec![Item::LayoutGallery],
                },
                Group {
                    title: "Current view",
                    items: vec![
                        Item::Large(Button::dropdown("Sort\nby", I::SortBy, sort_menu)),
                        Item::Stack(vec![
                            Button::dropdown("Group by", I::GroupBy, group_menu),
                            Button::dropdown("Add columns", I::AddColumns, add_cols_menu),
                            Button::new(
                                "Size all columns to fit",
                                I::SizeColumns,
                                C::SizeColumnsToFit,
                            ),
                        ]),
                    ],
                },
                Group {
                    title: "Show/hide",
                    items: vec![
                        Item::Stack(vec![
                            Button::check("Item check boxes", C::ToggleItemCheckBoxes),
                            Button::check("File name extensions", C::ToggleFileExtensions),
                            Button::check("Hidden items", C::ToggleHiddenItems),
                        ]),
                        Item::Large(Button::new(
                            "Hide selected\nitems",
                            I::HideSelected,
                            C::HideSelectedItems,
                        )),
                    ],
                },
                Group {
                    title: "",
                    items: vec![Item::Large(Button::dropdown(
                        "Options",
                        I::Options,
                        vec![M::item("Change folder and search options", C::Options)],
                    ))],
                },
            ],
        };

        // These settings are host-controlled so configuration and background work stay
        // outside the reusable GUI library.
        let preferences = Group {
            title: "Preferences",
            items: vec![
                Item::Stack(vec![
                    Button::check("Dark mode", C::ToggleDarkMode),
                    Button::check("Thumbnails", C::ToggleThumbnails),
                    Button::check("Search app data", C::ToggleSearchAppData),
                ]),
                Item::Stack(vec![
                    Button::check("This PC expanded", C::ToggleThisPcExpanded),
                    Button::dropdown(
                        "Drive scan",
                        I::Options,
                        vec![
                            M::item("Every 5 seconds", C::SetDrivePollSecs(5)),
                            M::item("Every 10 seconds", C::SetDrivePollSecs(10)),
                            M::item("Every 20 seconds", C::SetDrivePollSecs(20)),
                            M::item("Universe End", C::SetDrivePollSecs(0)),
                        ],
                    ),
                ]),
            ],
        };

        // Keep the preferences group inside the View tab without duplicating the
        // main tab's definition or coupling GUI code to configuration storage.
        let mut view = view;
        view.groups.push(preferences);

        let drive = Tab {
            id: "ctx-drive",
            title: "Manage",
            key_tip: "JD",
            context: Some(ContextKind::Drive),
            groups: vec![
                Group {
                    title: "Protect",
                    items: vec![
                        Item::Large(Button::new("Eject", I::Eject, C::Eject)),
                        Item::Large(Button::new("Mount", I::Drive, C::MountDrive)),
                    ],
                },
                Group {
                    title: "Media",
                    items: vec![Item::Stack(vec![
                        Button::new("Format", I::Format, C::Format),
                        Button::new("Autoplay", I::Autoplay, C::Autoplay),
                    ])],
                },
                Group {
                    title: "Manage",
                    items: vec![
                        Item::Large(Button::new("Cleanup", I::Cleanup, C::Cleanup)),
                        Item::Large(Button::new("Optimize", I::Optimize, C::Optimize)),
                    ],
                },
            ],
        };
        let picture = Tab {
            id: "ctx-picture",
            title: "Manage",
            key_tip: "JP",
            context: Some(ContextKind::Picture),
            groups: vec![
                Group {
                    title: "Rotate",
                    items: vec![Item::Stack(vec![
                        Button::new("Rotate left", I::RotateLeft, C::RotateLeft),
                        Button::new("Rotate right", I::RotateRight, C::RotateRight),
                    ])],
                },
                Group {
                    title: "Set as",
                    items: vec![Item::Large(Button::new(
                        "Set as\nbackground",
                        I::Wallpaper,
                        C::SetAsBackground,
                    ))],
                },
            ],
        };
        let app = Tab {
            id: "ctx-app",
            title: "Manage",
            key_tip: "JA",
            context: Some(ContextKind::Application),
            groups: vec![Group {
                title: "Run",
                items: vec![Item::Large(Button::new(
                    "Run as\nadministrator",
                    I::RunAsAdmin,
                    C::RunAsAdmin,
                ))],
            }],
        };
        let archive = Tab {
            id: "ctx-archive",
            title: "Extract",
            key_tip: "JE",
            context: Some(ContextKind::Archive),
            groups: vec![Group {
                title: "Extract To",
                items: vec![
                    Item::Large(Button::new("Extract\nall", I::Extract, C::Extract)),
                    Item::Large(Button::new("Extract\nhere", I::Extract, C::ExtractHere)),
                ],
            }],
        };
        let media = Tab {
            id: "ctx-media",
            title: "Play",
            key_tip: "JM",
            context: Some(ContextKind::Media),
            groups: vec![Group {
                title: "Playback",
                items: vec![Item::Large(Button::new("Play", I::Play, C::Play))],
            }],
        };

        Ribbon {
            tabs: vec![home, share, view, drive, picture, app, archive, media],
        }
    }

    /// Which contextual tab (if any) the current selection calls for.
    ///
    /// Reads the memoised selection summary, so calling this (and the status
    /// bar / side pane reading the same summary) costs one pass over the
    /// folder per selection change rather than one per call.
    pub fn context_for(state: &ExplorerState) -> Option<ContextKind> {
        state.selection_summary().context
    }

    fn visible_tabs(&self, ctx: Option<ContextKind>) -> Vec<&Tab> {
        self.tabs
            .iter()
            .filter(|t| t.context.is_none() || t.context == ctx)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Top-level entry point
    // -----------------------------------------------------------------------

    /// Draw the ribbon according to `state.ribbon_mode`.
    pub fn show(&self, ui: &mut Ui, state: &mut ExplorerState) -> RibbonOut {
        let ctx_kind = Self::context_for(state);
        if !self
            .visible_tabs(ctx_kind)
            .iter()
            .any(|t| t.id == state.active_tab)
        {
            state.active_tab = "home".into();
        }
        let mut out = RibbonOut::default();
        self.handle_keys(ui, state, ctx_kind);

        match state.ribbon_mode {
            RibbonMode::Expanded => {
                let r = self.show_tab_strip(ui, state, ctx_kind);
                out.context_header = r;
                self.show_body(ui, state, ctx_kind);
                out.height = TAB_H + BODY_H;
            }
            RibbonMode::Minimized => {
                let tab_rect_start = ui.cursor().min;
                out.context_header = self.show_tab_strip(ui, state, ctx_kind);
                out.height = TAB_H;
                if state.ribbon_overlay {
                    let pos = pos2(tab_rect_start.x, tab_rect_start.y + TAB_H);
                    let width = ui.max_rect().width();
                    self.show_overlay(ui.ctx(), state, ctx_kind, pos, width, false);
                }
            }
            RibbonMode::Hidden => {
                if state.ribbon_overlay {
                    let pos = ui.cursor().min;
                    let width = ui.max_rect().width();
                    self.show_overlay(ui.ctx(), state, ctx_kind, pos, width, true);
                }
            }
        }
        out
    }

    fn show_overlay(
        &self,
        ctx: &egui::Context,
        state: &mut ExplorerState,
        ctx_kind: Option<ContextKind>,
        pos: egui::Pos2,
        width: f32,
        with_tabs: bool,
    ) {
        let mut activated = false;
        let resp = Area::new(Id::unique("ribbon_overlay"))
            .order(Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                if with_tabs {
                    self.show_tab_strip(ui, state, ctx_kind);
                }
                activated = self.show_body(ui, state, ctx_kind);
            });
        let area_rect = resp.response.rect;
        let popup_open = Popup::is_any_open(ctx);
        let (pressed_outside, esc) = ctx.input(|i| {
            let outside = i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| {
                    !area_rect.contains(p) && !(p.y < pos.y + TAB_H && p.y >= pos.y - TAB_H)
                });
            (outside, i.key_pressed(Key::Escape))
        });
        if activated || esc || (pressed_outside && !popup_open) {
            state.ribbon_overlay = false;
            state.key_tips = false;
        }
    }

    // -----------------------------------------------------------------------
    // Keyboard: Alt key tips, Ctrl+F1
    // -----------------------------------------------------------------------

    fn handle_keys(&self, ui: &Ui, state: &mut ExplorerState, ctx_kind: Option<ContextKind>) {
        let ctx = ui.ctx();
        let id = Id::unique("ribbon_alt_state");
        let (alt_down, other_key, ctrl_f1, esc, typed): (bool, bool, bool, bool, Vec<String>) = ctx.input(|i| {
            let other = i
                .events
                .iter()
                .any(|e| matches!(e, egui::Event::Key { pressed: true, key, .. } if !matches!(key, Key::Escape)));
            let typed = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { pressed: true, key, .. } => Some(key.name().to_uppercase()),
                    _ => None,
                })
                .collect();
            (
                i.modifiers.alt,
                other,
                i.modifiers.ctrl && i.key_pressed(Key::F1),
                i.key_pressed(Key::Escape),
                typed,
            )
        });
        let (focused, pointer_down, now) = ctx.input(|i| {
            (
                i.focused,
                i.pointer.any_down() || i.pointer.any_pressed(),
                i.time,
            )
        });
        let (prev_alt, used, alt_since): (bool, bool, f64) =
            ctx.data(|d| d.get_temp(id)).unwrap_or((false, false, 0.0));
        // Alt only counts as a "key tips" gesture when it is pressed and released on its own,
        // quickly, while this window has focus. Window managers (Hyprland, sway, ...) grab
        // Alt+<key> shortcuts and focus changes: the app then sees a bare Alt release that was
        // never meant for it, and the tips used to pop up out of nowhere.
        let used = if alt_down {
            used || other_key || pointer_down || !focused
        } else {
            false
        };
        let alt_since = if alt_down && !prev_alt {
            now
        } else {
            alt_since
        };
        let quick_tap = now - alt_since <= KEY_TIP_TAP_MAX_SECS;
        if !focused || pointer_down {
            // Losing focus or clicking dismisses tips that are showing.
            if state.key_tips {
                state.key_tips = false;
                if state.ribbon_mode == RibbonMode::Hidden {
                    state.ribbon_overlay = false;
                }
            }
        }
        // Alt released without any other key => toggle key tips.
        if prev_alt && !alt_down && !used && focused && quick_tap && state.key_tips_enabled {
            state.key_tips = !state.key_tips;
            if state.key_tips && state.ribbon_mode == RibbonMode::Hidden {
                state.ribbon_overlay = true;
            }
        }
        ctx.data_mut(|d| d.insert_temp(id, (alt_down, used, alt_since)));

        if esc && state.key_tips {
            state.key_tips = false;
            if state.ribbon_mode == RibbonMode::Hidden {
                state.ribbon_overlay = false;
            }
        }
        if ctrl_f1 {
            state.apply_local(Command::ToggleRibbon);
        }
        if state.key_tips && !alt_down {
            for k in typed {
                if k.len() == 1 && k != "ESCAPE" {
                    if k == "F" {
                        ctx.data_mut(|d| d.insert_temp(Id::unique("file_menu_request"), true));
                        state.key_tips = false;
                        break;
                    }
                    let tabs = self.visible_tabs(ctx_kind);
                    if let Some(t) = tabs.iter().find(|t| {
                        t.key_tip == k || (t.key_tip.len() > 1 && t.key_tip.starts_with(&k))
                    }) {
                        state.active_tab = t.id.to_owned();
                        if state.ribbon_mode != RibbonMode::Expanded {
                            state.ribbon_overlay = true;
                        }
                        state.key_tips = false;
                        break;
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tab strip
    // -----------------------------------------------------------------------

    fn show_tab_strip(
        &self,
        ui: &mut Ui,
        state: &mut ExplorerState,
        ctx_kind: Option<ContextKind>,
    ) -> Option<ContextHeader> {
        let width = ui.available_width();
        let (strip, _) = ui.allocate_exact_size(vec2(width, TAB_H), Sense::hover());
        let painter = ui.painter_at(strip);
        painter.rect_filled(strip, CornerRadius::ZERO, colors::chrome_bg());
        let font = theme::font();
        let mut x = strip.left();
        let mut header = None;

        // ---- File tab ----
        let file_rect = Rect::from_min_size(pos2(x, strip.top()), vec2(54.0, TAB_H));
        let file_resp = ui.interact(file_rect, Id::unique("ribbon_file_tab"), Sense::click());
        let fill = if file_resp.is_pointer_button_down_on() {
            colors::file_tab_pressed()
        } else if file_resp.hovered() {
            colors::file_tab_hover()
        } else {
            colors::file_tab()
        };
        painter.rect_filled(file_rect, CornerRadius::ZERO, fill);
        painter.text(
            file_rect.center(),
            Align2::CENTER_CENTER,
            "File",
            font.clone(),
            Color32::WHITE,
        );
        let file_req: bool = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<bool>(Id::unique("file_menu_request")))
            .unwrap_or(false);
        if state.key_tips {
            key_tip_badge(&painter, file_rect.center_bottom() + vec2(0.0, -2.0), "F");
        }
        let popup_resp = file_resp.clone();
        if file_req {
            Popup::open_id(ui.ctx(), Popup::default_response_id(&popup_resp));
        }
        let mut chosen: Option<Command> = None;
        let mut nav_to: Option<std::path::PathBuf> = None;
        Popup::menu(&popup_resp)
            .gap(0.0)
            .width(511.0)
            .frame(
                egui::Frame::NONE
                    .fill(colors::window_bg())
                    .stroke(Stroke::new(1.0, colors::ribbon_border())),
            )
            .show(|ui| {
                show_file_menu(ui, state, &mut chosen, &mut nav_to);
            });
        if let Some(c) = chosen {
            state.dispatch(c);
        }
        if let Some(p) = nav_to {
            state.navigate(p);
        }
        x = file_rect.right() + 2.0;

        // ---- normal + contextual tabs ----
        let tabs = self.visible_tabs(ctx_kind);
        let mut ctx_span: Option<(f32, f32)> = None;
        for t in tabs {
            let tw = ui
                .painter()
                .layout_no_wrap(t.title.to_owned(), font.clone(), colors::text())
                .size()
                .x
                + 26.0;
            let r = Rect::from_min_size(pos2(x, strip.top()), vec2(tw, TAB_H));
            let resp = ui.interact(r, Id::unique(("ribbon_tab", t.id)), Sense::click());
            let selected = state.active_tab == t.id
                && (state.ribbon_mode == RibbonMode::Expanded || state.ribbon_overlay);
            if selected {
                painter.rect_filled(r, CornerRadius::ZERO, colors::tab_selected_bg());
                let s = Stroke::new(1.0, colors::ribbon_border());
                painter.line_segment([r.left_bottom(), r.left_top()], s);
                painter.line_segment([r.left_top(), r.right_top()], s);
                painter.line_segment([r.right_top(), r.right_bottom()], s);
            } else if resp.hovered() {
                painter.rect_filled(r, CornerRadius::ZERO, colors::tab_hover_bg());
            }
            let text_col = t.context.map(|c| c.color()).unwrap_or(colors::text());
            if let Some(c) = t.context {
                painter.rect_filled(
                    Rect::from_min_size(r.left_top(), vec2(r.width(), 2.0)),
                    CornerRadius::ZERO,
                    c.color(),
                );
                ctx_span = Some(match ctx_span {
                    Some((a, b)) => (a.min(r.left()), b.max(r.right())),
                    None => (r.left(), r.right()),
                });
                header = Some(c);
            }
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                t.title,
                font.clone(),
                text_col,
            );
            if state.key_tips {
                key_tip_badge(&painter, r.center_bottom() + vec2(0.0, -2.0), t.key_tip);
            }
            if resp.clicked() {
                let was_active = state.active_tab == t.id;
                state.active_tab = t.id.to_owned();
                if state.ribbon_mode == RibbonMode::Minimized {
                    state.ribbon_overlay = !(state.ribbon_overlay && was_active);
                }
            }
            if resp.double_clicked() {
                state.apply_local(Command::ToggleRibbon);
            }
            x = r.right();
        }

        // ---- right side: collapse chevron + help ----
        let help = Rect::from_center_size(
            pos2(strip.right() - 14.0, strip.center().y),
            vec2(20.0, 20.0),
        );
        let help_resp = ui.interact(help, Id::unique("ribbon_help"), Sense::click());
        if help_resp.hovered() {
            painter.rect_filled(help, CornerRadius::ZERO, colors::btn_hover_fill());
        }
        Icon::Help.paint(&painter, help.shrink(3.0), true);
        if help_resp.clicked() {
            state.dispatch(Command::Help);
        }
        help_resp.on_hover_text("Help (F1)");

        let chev =
            Rect::from_center_size(pos2(help.left() - 14.0, strip.center().y), vec2(20.0, 20.0));
        let chev_resp = ui.interact(chev, Id::unique("ribbon_chevron"), Sense::click());
        if chev_resp.hovered() {
            painter.rect_filled(chev, CornerRadius::ZERO, colors::btn_hover_fill());
        }
        let expanded = state.ribbon_mode == RibbonMode::Expanded;
        let ci = if expanded {
            Icon::ChevronUp
        } else {
            Icon::ChevronDown
        };
        ci.paint(&painter, chev.shrink(5.0), true);
        if chev_resp.clicked() {
            state.apply_local(Command::ToggleRibbon);
        }
        chev_resp.on_hover_text(if expanded {
            "Minimize the Ribbon (Ctrl+F1)"
        } else {
            "Expand the Ribbon (Ctrl+F1)"
        });

        header.and_then(|c| {
            ctx_span.map(|(a, b)| ContextHeader {
                x_min: a,
                x_max: b,
                label: c.label(),
                color: c.color(),
            })
        })
    }

    // -----------------------------------------------------------------------
    // Body
    // -----------------------------------------------------------------------

    /// Draw the active tab's groups. Returns `true` if a command was activated.
    fn show_body(
        &self,
        ui: &mut Ui,
        state: &mut ExplorerState,
        ctx_kind: Option<ContextKind>,
    ) -> bool {
        let tabs = self.visible_tabs(ctx_kind);
        let Some(tab) = tabs.iter().find(|t| t.id == state.active_tab).copied() else {
            return false;
        };
        let avail = ui.available_width();
        let (body, _) = ui.allocate_exact_size(vec2(avail, BODY_H), Sense::hover());
        let painter = ui.painter_at(body);
        painter.rect_filled(body, CornerRadius::ZERO, colors::ribbon_bg());
        painter.line_segment(
            [body.left_top(), body.right_top()],
            Stroke::new(1.0, colors::ribbon_border()),
        );
        painter.line_segment(
            [body.left_bottom(), body.right_bottom()],
            Stroke::new(1.0, colors::ribbon_border()),
        );

        let font = theme::font();
        let full: Vec<f32> = tab
            .groups
            .iter()
            .map(|g| group_width(ui, g, &font))
            .collect();
        let collapsed_w: Vec<f32> = tab
            .groups
            .iter()
            .map(|g| text_w(ui, g.title, &font).max(34.0) + 16.0)
            .collect();
        // Collapse groups from the right until everything fits.
        let mut is_collapsed = vec![false; tab.groups.len()];
        let width_of = |c: &Vec<bool>| -> f32 {
            (0..tab.groups.len())
                .map(|i| if c[i] { collapsed_w[i] } else { full[i] } + 2.0)
                .sum::<f32>()
                + 4.0
        };
        let mut i = tab.groups.len();
        while width_of(&is_collapsed) > avail && i > 0 {
            i -= 1;
            is_collapsed[i] = true;
        }

        let mut activated = false;
        let mut x = body.left() + 3.0;
        for (gi, g) in tab.groups.iter().enumerate() {
            let w = if is_collapsed[gi] {
                collapsed_w[gi]
            } else {
                full[gi]
            };
            let rect = Rect::from_min_size(pos2(x, body.top() + 1.0), vec2(w, BODY_H - 2.0));
            let id = Id::unique(("ribbon_group", tab.id, gi));
            if is_collapsed[gi] {
                activated |= draw_collapsed_group(ui, &painter, rect, g, state, id, full[gi]);
            } else {
                activated |= draw_group(ui, &painter, rect, g, state, id);
            }
            x += w + 1.0;
            if gi + 1 < tab.groups.len() {
                painter.line_segment(
                    [pos2(x, body.top() + 6.0), pos2(x, body.bottom() - 6.0)],
                    Stroke::new(1.0, colors::ribbon_border()),
                );
            }
            x += 1.0;
        }
        activated
    }
}

// ---------------------------------------------------------------------------
// Group / item layout & painting
// ---------------------------------------------------------------------------

fn text_w(ui: &Ui, s: &str, font: &egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(s.to_owned(), font.clone(), colors::text())
        .size()
        .x
}

fn large_width(ui: &Ui, b: &Button, font: &egui::FontId) -> f32 {
    let lines: Vec<&str> = b.label.split('\n').collect();
    let last = lines.len() - 1;
    let w = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            text_w(ui, l, font)
                + if i == last && b.kind == ButtonKind::Dropdown {
                    12.0
                } else {
                    0.0
                }
        })
        .fold(0.0, f32::max);
    (w + 14.0).max(46.0)
}

fn stack_width(ui: &Ui, bs: &[Button], font: &egui::FontId) -> f32 {
    bs.iter()
        .map(|b| {
            let base = text_w(ui, b.label, font) + 28.0;
            match b.kind {
                ButtonKind::Dropdown => base + 12.0,
                ButtonKind::Split => base + 14.0,
                ButtonKind::Check => base - 6.0,
                ButtonKind::Plain => base,
            }
        })
        .fold(0.0, f32::max)
        + 4.0
}

const GALLERY_COL_W: f32 = 132.0;
const GALLERY_W: f32 = GALLERY_COL_W * 2.0 + 18.0;

fn item_width(ui: &Ui, it: &Item, font: &egui::FontId) -> f32 {
    match it {
        Item::Large(b) => large_width(ui, b, font),
        Item::Stack(bs) => stack_width(ui, bs, font),
        Item::LayoutGallery => GALLERY_W,
    }
}

fn group_width(ui: &Ui, g: &Group, font: &egui::FontId) -> f32 {
    let items: f32 =
        g.items.iter().map(|i| item_width(ui, i, font)).sum::<f32>() + 2.0 * g.items.len() as f32;
    items.max(text_w(ui, g.title, &theme::font_small()) + 12.0)
}

fn draw_group(
    ui: &mut Ui,
    painter: &Painter,
    rect: Rect,
    g: &Group,
    state: &mut ExplorerState,
    id: Id,
) -> bool {
    let font = theme::font();
    let mut activated = false;
    // When the caption is wider than the commands (e.g. one large button under a long
    // caption) centre the commands under it instead of leaving them stuck to the left edge.
    let items_w: f32 = g
        .items
        .iter()
        .map(|i| item_width(ui, i, &font))
        .sum::<f32>()
        + 2.0 * g.items.len() as f32;
    let mut x = rect.left() + ((rect.width() - items_w) / 2.0).max(0.0);
    let top = rect.top() + 2.0;
    for (ii, it) in g.items.iter().enumerate() {
        let w = item_width(ui, it, &font);
        let iid = id.with(ii);
        match it {
            Item::Large(b) => {
                let r = Rect::from_min_size(pos2(x, top), vec2(w, LARGE_H));
                activated |= draw_large(ui, painter, r, b, state, iid);
            }
            Item::Stack(bs) => {
                let n = bs.len() as f32;
                let y0 = top + (LARGE_H - n * SMALL_H) / 2.0;
                for (si, b) in bs.iter().enumerate() {
                    let r =
                        Rect::from_min_size(pos2(x, y0 + si as f32 * SMALL_H), vec2(w, SMALL_H));
                    activated |= draw_small(ui, painter, r, b, state, iid.with(si));
                }
            }
            Item::LayoutGallery => {
                let r = Rect::from_min_size(pos2(x, top + 1.0), vec2(w, LARGE_H - 2.0));
                activated |= draw_gallery(ui, painter, r, state, iid);
            }
        }
        x += w + 2.0;
    }
    // caption
    painter.text(
        pos2(rect.center().x, rect.bottom() - 9.0),
        Align2::CENTER_CENTER,
        g.title,
        theme::font_small(),
        colors::text_secondary(),
    );
    activated
}

/// A group that did not fit: shown as one button that opens the group as a popup.
fn draw_collapsed_group(
    ui: &mut Ui,
    painter: &Painter,
    rect: Rect,
    g: &Group,
    state: &mut ExplorerState,
    id: Id,
    full_w: f32,
) -> bool {
    let r = Rect::from_min_size(rect.min + vec2(0.0, 2.0), vec2(rect.width(), LARGE_H));
    let resp = ui.interact(r, id, Sense::click());
    let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&resp));
    button_bg(
        painter,
        r,
        resp.hovered(),
        resp.is_pointer_button_down_on() || open,
        false,
    );
    let icon = g
        .items
        .iter()
        .find_map(|i| match i {
            Item::Large(b) => Some(b.icon),
            Item::Stack(bs) => bs.first().map(|b| b.icon),
            Item::LayoutGallery => Some(Icon::ViewMedium),
        })
        .unwrap_or(Icon::Options);
    icon.paint(
        painter,
        Rect::from_center_size(pos2(r.center().x, r.top() + 22.0), vec2(32.0, 32.0)),
        true,
    );
    painter.text(
        pos2(r.center().x, r.top() + 46.0),
        Align2::CENTER_TOP,
        g.title,
        theme::font(),
        colors::text(),
    );
    Icon::ChevronDown.paint(
        painter,
        Rect::from_center_size(pos2(r.center().x, r.bottom() - 4.0), vec2(8.0, 8.0)),
        true,
    );

    let mut activated = false;
    Popup::menu(&resp)
        .gap(0.0)
        .frame(
            egui::Frame::NONE
                .fill(colors::ribbon_bg())
                .stroke(Stroke::new(1.0, colors::ribbon_border())),
        )
        .show(|ui| {
            let (inner, _) =
                ui.allocate_exact_size(vec2(full_w + 6.0, BODY_H - 2.0), Sense::hover());
            let p = ui.painter_at(inner);
            activated = draw_group(
                ui,
                &p,
                inner.shrink2(vec2(3.0, 0.0)),
                g,
                state,
                id.with("popup"),
            );
        });
    activated
}

fn button_bg(painter: &Painter, r: Rect, hovered: bool, pressed: bool, checked: bool) {
    let (fill, border) = if pressed || (checked && hovered) {
        (colors::btn_pressed_fill(), colors::btn_pressed_border())
    } else if checked {
        (colors::btn_checked_fill(), colors::btn_checked_border())
    } else if hovered {
        (colors::btn_hover_fill(), colors::btn_hover_border())
    } else {
        return;
    };
    painter.rect_filled(r, CornerRadius::ZERO, fill);
    painter.rect_stroke(
        r,
        CornerRadius::ZERO,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
}

fn tooltip(b: &Button) -> String {
    if b.tip.is_empty() {
        b.label.replace('\n', " ")
    } else {
        b.tip.to_owned()
    }
}

/// Runs the popup for a dropdown/split arrow and returns the picked command.
fn show_dropdown(
    resp: &Response,
    anchor: Rect,
    menu: &[MenuEntry],
    state: &ExplorerState,
) -> Option<Command> {
    let mut picked = None;
    Popup::menu(resp)
        .anchor(anchor)
        .gap(0.0)
        .frame(menu_frame())
        .show(|ui| {
            // At least as wide as the button it drops from; wider if the entries need it.
            ui.set_min_width(anchor.width());
            render_menu(ui, menu, state, &mut picked);
        });
    picked
}

fn menu_shadow() -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: [2, 3],
        blur: 6,
        spread: 0,
        color: Color32::from_black_alpha(45),
    }
}

/// The frame shared by every menu: ribbon drop-downs, right-click menus and sub-menus.
pub fn menu_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(menu_bg())
        .stroke(Stroke::new(1.0, colors::ribbon_border()))
        .inner_margin(egui::Margin::symmetric(2, 2))
        .shadow(menu_shadow())
}

/// Right-click menu on `resp`, drawn in the shared [`menu_frame`] (egui's own context-menu
/// frame would not match the ribbon drop-downs).
pub fn context_menu(resp: &Response, add_contents: impl FnOnce(&mut Ui)) {
    Popup::context_menu(resp)
        .frame(menu_frame())
        .show(add_contents);
}

fn draw_large(
    ui: &mut Ui,
    painter: &Painter,
    r: Rect,
    b: &Button,
    state: &mut ExplorerState,
    id: Id,
) -> bool {
    let enabled = b.cmd.map(|c| state.is_enabled(c)).unwrap_or(true);
    let checked = b.cmd.and_then(|c| state.is_checked(c)).unwrap_or(false)
        && matches!(
            b.cmd,
            Some(Command::TogglePreviewPane | Command::ToggleDetailsPane)
        );
    let font = theme::font();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let split = b.kind == ButtonKind::Split;
    let main_rect = if split {
        Rect::from_min_size(r.min, vec2(r.width(), 42.0))
    } else {
        r
    };
    let arrow_rect = Rect::from_min_max(pos2(r.left(), r.top() + 42.0), r.max);
    let main = ui.interact(main_rect, id.with("main"), sense);
    let arrow = if split {
        Some(ui.interact(arrow_rect, id.with("arrow"), sense))
    } else {
        None
    };

    let any_hover = main.hovered() || arrow.as_ref().is_some_and(|a| a.hovered());
    if enabled {
        if split {
            painter.rect_stroke(
                r,
                CornerRadius::ZERO,
                Stroke::new(
                    1.0,
                    if any_hover {
                        colors::btn_hover_border()
                    } else {
                        Color32::TRANSPARENT
                    },
                ),
                StrokeKind::Inside,
            );
            if main.hovered() {
                painter.rect_filled(
                    main_rect.shrink(1.0),
                    CornerRadius::ZERO,
                    if main.is_pointer_button_down_on() {
                        colors::btn_pressed_fill()
                    } else {
                        colors::btn_hover_fill()
                    },
                );
            }
            if let Some(a) = &arrow {
                let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(a));
                if a.hovered() || open {
                    painter.rect_filled(
                        arrow_rect.shrink(1.0),
                        CornerRadius::ZERO,
                        if a.is_pointer_button_down_on() || open {
                            colors::btn_pressed_fill()
                        } else {
                            colors::btn_hover_fill()
                        },
                    );
                }
            }
        } else {
            let open = b.kind == ButtonKind::Dropdown
                && Popup::is_id_open(ui.ctx(), Popup::default_response_id(&main));
            button_bg(
                painter,
                r,
                main.hovered(),
                main.is_pointer_button_down_on() || open,
                checked,
            );
        }
    } else if checked {
        button_bg(painter, r, false, false, true);
    }

    let text_col = if enabled {
        colors::text()
    } else {
        colors::text_disabled()
    };
    b.icon.paint(
        painter,
        Rect::from_center_size(pos2(r.center().x, r.top() + 22.0), vec2(32.0, 32.0)),
        enabled,
    );
    let lines: Vec<&str> = b.label.split('\n').collect();
    let mut y = r.top() + 42.0;
    for (i, l) in lines.iter().enumerate() {
        let last = i == lines.len() - 1;
        let show_arrow = last && matches!(b.kind, ButtonKind::Dropdown | ButtonKind::Split);
        let tw = text_w(ui, l, &font);
        let off = if show_arrow { 5.0 } else { 0.0 };
        painter.text(
            pos2(r.center().x - off, y),
            Align2::CENTER_TOP,
            l,
            font.clone(),
            text_col,
        );
        if show_arrow {
            Icon::ChevronDown.paint(
                painter,
                Rect::from_center_size(
                    pos2(r.center().x - off + tw / 2.0 + 7.0, y + 7.0),
                    vec2(8.0, 8.0),
                ),
                enabled,
            );
        }
        y += 14.0;
    }
    // A single-line split/dropdown button keeps its arrow on a second row.
    if lines.len() == 1 && matches!(b.kind, ButtonKind::Dropdown | ButtonKind::Split) {
        // already drawn inline above
    }

    let mut activated = false;
    let main = main.on_hover_text(tooltip(b));
    match b.kind {
        ButtonKind::Dropdown => {
            if let Some(c) = show_dropdown(&main, r, &b.menu, state) {
                state.dispatch(c);
                activated = true;
            }
        }
        ButtonKind::Split => {
            if main.clicked()
                && let Some(c) = b.cmd
            {
                state.dispatch(c);
                activated = true;
            }
            if let Some(a) = &arrow
                && let Some(c) = show_dropdown(a, r, &b.menu, state)
            {
                state.dispatch(c);
                activated = true;
            }
        }
        _ => {
            if main.clicked()
                && let Some(c) = b.cmd
            {
                state.dispatch(c);
                activated = true;
            }
        }
    }
    activated
}

fn draw_small(
    ui: &mut Ui,
    painter: &Painter,
    r: Rect,
    b: &Button,
    state: &mut ExplorerState,
    id: Id,
) -> bool {
    let enabled = b.cmd.map(|c| state.is_enabled(c)).unwrap_or(true);
    let font = theme::font();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let split = b.kind == ButtonKind::Split;
    let arrow_w = 14.0;
    let main_rect = if split {
        Rect::from_min_max(r.min, pos2(r.right() - arrow_w, r.bottom()))
    } else {
        r
    };
    let arrow_rect = Rect::from_min_max(pos2(r.right() - arrow_w, r.top()), r.max);
    let main = ui.interact(main_rect, id.with("main"), sense);
    let arrow = if split {
        Some(ui.interact(arrow_rect, id.with("arrow"), sense))
    } else {
        None
    };

    if enabled && b.kind != ButtonKind::Check {
        if split {
            let hv = main.hovered() || arrow.as_ref().is_some_and(|a| a.hovered());
            if hv {
                painter.rect_stroke(
                    r,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, colors::btn_hover_border()),
                    StrokeKind::Inside,
                );
            }
            if main.hovered() {
                painter.rect_filled(
                    main_rect.shrink(1.0),
                    CornerRadius::ZERO,
                    if main.is_pointer_button_down_on() {
                        colors::btn_pressed_fill()
                    } else {
                        colors::btn_hover_fill()
                    },
                );
            }
            if let Some(a) = &arrow {
                let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(a));
                if a.hovered() || open {
                    painter.rect_filled(
                        arrow_rect.shrink(1.0),
                        CornerRadius::ZERO,
                        if a.is_pointer_button_down_on() || open {
                            colors::btn_pressed_fill()
                        } else {
                            colors::btn_hover_fill()
                        },
                    );
                }
            }
        } else {
            let open = b.kind == ButtonKind::Dropdown
                && Popup::is_id_open(ui.ctx(), Popup::default_response_id(&main));
            button_bg(
                painter,
                r,
                main.hovered(),
                main.is_pointer_button_down_on() || open,
                false,
            );
        }
    } else if b.kind == ButtonKind::Check && main.hovered() {
        button_bg(painter, r, true, false, false);
    }

    let text_col = if enabled {
        colors::text()
    } else {
        colors::text_disabled()
    };
    if b.kind == ButtonKind::Check {
        let checked = b.cmd.and_then(|c| state.is_checked(c)).unwrap_or(false);
        let bx = Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), vec2(13.0, 13.0));
        painter.rect_filled(bx, CornerRadius::ZERO, colors::window_bg());
        painter.rect_stroke(
            bx,
            CornerRadius::ZERO,
            Stroke::new(
                1.0,
                if main.hovered() {
                    colors::accent()
                } else {
                    colors::field_border()
                },
            ),
            StrokeKind::Inside,
        );
        if checked {
            Icon::Check.paint(painter, bx.shrink(1.0), true);
        }
        painter.text(
            pos2(r.left() + 24.0, r.center().y),
            Align2::LEFT_CENTER,
            b.label,
            font,
            text_col,
        );
    } else {
        b.icon.paint(
            painter,
            Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), vec2(16.0, 16.0)),
            enabled,
        );
        painter.text(
            pos2(r.left() + 24.0, r.center().y),
            Align2::LEFT_CENTER,
            b.label,
            font,
            text_col,
        );
        if matches!(b.kind, ButtonKind::Dropdown) {
            Icon::ChevronDown.paint(
                painter,
                Rect::from_center_size(pos2(r.right() - 8.0, r.center().y), vec2(8.0, 8.0)),
                enabled,
            );
        }
        if split {
            Icon::ChevronDown.paint(
                painter,
                Rect::from_center_size(arrow_rect.center(), vec2(8.0, 8.0)),
                enabled,
            );
        }
    }

    let mut activated = false;
    let main = main.on_hover_text(tooltip(b));
    match b.kind {
        ButtonKind::Dropdown => {
            if let Some(c) = show_dropdown(&main, r, &b.menu, state) {
                state.dispatch(c);
                activated = true;
            }
        }
        ButtonKind::Split => {
            if main.clicked()
                && let Some(c) = b.cmd
            {
                state.dispatch(c);
                activated = true;
            }
            if let Some(a) = &arrow
                && let Some(c) = show_dropdown(a, r, &b.menu, state)
            {
                state.dispatch(c);
                activated = true;
            }
        }
        _ => {
            if main.clicked()
                && let Some(c) = b.cmd
            {
                state.dispatch(c);
                activated = true;
            }
        }
    }
    activated
}

fn draw_gallery(
    ui: &mut Ui,
    painter: &Painter,
    r: Rect,
    state: &mut ExplorerState,
    id: Id,
) -> bool {
    let font = theme::font();
    let offset_id = id.with("rows");
    let mut row_off: usize = ui.ctx().data(|d| d.get_temp(offset_id)).unwrap_or(0);
    // keep the selected view visible
    let sel_row = ViewMode::ALL
        .iter()
        .position(|v| *v == state.view)
        .unwrap_or(0)
        / 2;
    if sel_row < row_off {
        row_off = sel_row;
    } else if sel_row >= row_off + 3 {
        row_off = sel_row - 2;
    }
    let max_off = 1;
    painter.rect_filled(r, CornerRadius::ZERO, colors::window_bg());
    painter.rect_stroke(
        r,
        CornerRadius::ZERO,
        Stroke::new(1.0, colors::ribbon_border()),
        StrokeKind::Inside,
    );

    let mut activated = false;
    let row_h = (r.height() - 2.0) / 3.0;
    for row in 0..3 {
        for col in 0..2 {
            let idx = (row_off + row) * 2 + col;
            let Some(&mode) = ViewMode::ALL.get(idx) else {
                continue;
            };
            let cell = Rect::from_min_size(
                pos2(
                    r.left() + 1.0 + col as f32 * GALLERY_COL_W,
                    r.top() + 1.0 + row as f32 * row_h,
                ),
                vec2(GALLERY_COL_W, row_h),
            );
            let resp = ui.interact(cell, id.with(("cell", idx)), Sense::click());
            let selected = state.view == mode;
            if selected || resp.hovered() {
                button_bg(
                    painter,
                    cell.shrink(1.0),
                    resp.hovered(),
                    resp.is_pointer_button_down_on(),
                    selected,
                );
            }
            mode.icon().paint(
                painter,
                Rect::from_center_size(pos2(cell.left() + 13.0, cell.center().y), vec2(16.0, 16.0)),
                true,
            );
            painter.text(
                pos2(cell.left() + 26.0, cell.center().y),
                Align2::LEFT_CENTER,
                mode.label(),
                font.clone(),
                colors::text(),
            );
            if resp.clicked() {
                state.dispatch(Command::SetView(mode));
                activated = true;
            }
        }
    }
    // scroll arrows
    let ax = r.right() - 17.0;
    let third = r.height() / 3.0;
    let arrows = [
        (Icon::ChevronUp, 0usize),
        (Icon::ChevronDown, 1),
        (Icon::ChevronDown, 2),
    ];
    for (i, (icon, kind)) in arrows.into_iter().enumerate() {
        let cell = Rect::from_min_size(pos2(ax, r.top() + i as f32 * third), vec2(17.0, third));
        let resp = ui.interact(cell, id.with(("arrow", i)), Sense::click());
        if resp.hovered() {
            button_bg(painter, cell, true, resp.is_pointer_button_down_on(), false);
        }
        icon.paint(
            painter,
            Rect::from_center_size(cell.center(), vec2(8.0, 8.0)),
            true,
        );
        match kind {
            0 if resp.clicked() => row_off = row_off.saturating_sub(1),
            1 if resp.clicked() => row_off = (row_off + 1).min(max_off),
            2 => {
                // "more" button: shows the full gallery
                let mut picked = None;
                Popup::menu(&resp)
                    .anchor(r)
                    .gap(0.0)
                    .frame(menu_frame())
                    .show(|ui| {
                        ui.set_min_width(r.width());
                        for m in ViewMode::ALL {
                            let checked = state.view == m;
                            if menu_row(ui, Some(m.icon()), m.label(), Some(checked), true, None)
                                .clicked()
                            {
                                picked = Some(m);
                            }
                        }
                    });
                if let Some(m) = picked {
                    state.dispatch(Command::SetView(m));
                    activated = true;
                }
            }
            _ => {}
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(offset_id, row_off));
    activated
}

// ---------------------------------------------------------------------------
// Menus (also used by the context menus in the file list)
// ---------------------------------------------------------------------------

/// Height of every row in a menu (plain rows and sub-menu rows alike).
const MENU_ROW_H: f32 = 22.0;
/// Space left of a label: the icon / check column.
const MENU_LEFT: f32 = 34.0;
/// Space right of the label, shortcut or sub-menu arrow.
const MENU_RIGHT: f32 = 12.0;
/// Smallest gap between a label and its shortcut / sub-menu arrow.
const MENU_GAP: f32 = 28.0;
/// Width reserved for the sub-menu arrow.
const MENU_ARROW_W: f32 = 10.0;
/// Narrowest and widest a menu may become.
const MENU_MIN_W: f32 = 120.0;
const MENU_MAX_W: f32 = 640.0;

/// Per-menu bookkeeping for [`menu_width`], kept in egui's temp memory.
#[derive(Clone, Copy, Default)]
struct MenuWidth {
    /// Pass in which `current` was collected.
    pass: u64,
    /// Widest natural row width seen so far in this pass.
    current: f32,
    /// Final widest natural row width of the previous pass.
    previous: f32,
}

/// Width that every row of the menu drawn into `ui` must use, so the panel is exactly as
/// wide as its widest row (label + gap + shortcut / arrow + padding) and the label and
/// shortcut columns can never collide.
///
/// Rows are laid out one after another, so a row cannot know how wide its later siblings
/// are. Each menu therefore remembers the widest row of the previous pass. A new popup
/// always runs a hidden sizing pass first, so the first visible frame is already correct.
/// A `set_min_width` made by the caller (e.g. "at least as wide as the ribbon button")
/// is honoured through `ui.min_rect()`.
fn menu_width(ui: &Ui, natural: f32) -> f32 {
    let ctx = ui.ctx();
    let id = ui.scope_id().with("menu_width");
    let pass = ctx.cumulative_pass_nr();
    let st = ctx.data_mut(|d| {
        let st: &mut MenuWidth = d.get_temp_mut_or_default(id);
        if st.pass != pass {
            st.pass = pass;
            st.previous = st.current;
            st.current = 0.0;
        }
        st.current = st.current.max(natural);
        *st
    });
    st.current
        .max(st.previous)
        .max(ui.min_rect().width())
        .clamp(MENU_MIN_W, MENU_MAX_W)
}

/// Reserve one menu row of height `h`. Menus are laid out cross-justified, which would
/// stretch every row to whatever width the popup area currently offers (the cause of the
/// over-wide sub-menus). Pinning the ui's maximum width to the computed menu width first
/// makes the row, and therefore the whole panel, exactly as wide as its content.
fn menu_row_rect(ui: &mut Ui, natural: f32, h: f32) -> Rect {
    let w = menu_width(ui, natural);
    ui.set_max_width(w);
    ui.allocate_space(vec2(w, h)).1
}

/// Natural width of a row: icon column + label + (gap + `trailing`) + padding.
fn menu_natural_width(ui: &Ui, label: &str, trailing: Option<f32>) -> f32 {
    let mut w = MENU_LEFT + text_w(ui, label, &theme::font()) + MENU_RIGHT;
    if let Some(t) = trailing {
        w += MENU_GAP + t;
    }
    w
}

/// Painter limited to one row, so an over-long label can never bleed out of the menu.
fn row_painter(ui: &Ui, rect: Rect) -> Painter {
    ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()))
}

/// One Windows-style menu row. `checked: Some(_)` reserves the check column.
pub fn menu_row(
    ui: &mut Ui,
    icon: Option<Icon>,
    label: &str,
    checked: Option<bool>,
    enabled: bool,
    shortcut: Option<&str>,
) -> Response {
    let shortcut_w = shortcut.map(|s| text_w(ui, s, &theme::font()));
    let natural = menu_natural_width(ui, label, shortcut_w);
    // Use an explicit, label-derived id instead of the Ui's sequential auto-id. Hover
    // hit-testing is keyed by widget id, so if a neighbouring row consumes a
    // different number of auto-ids from one frame to the next, every row below it would
    // inherit the hover state of the wrong row (visible label != highlighted row).
    let id = ui
        .scope_id()
        .with(("menu_row", label, shortcut.unwrap_or("")));
    let rect = menu_row_rect(ui, natural, MENU_ROW_H);
    let resp = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let p = row_painter(ui, rect);
    if resp.hovered() && enabled {
        p.rect_filled(rect, CornerRadius::ZERO, menu_hover());
    }
    let col = if enabled {
        colors::text()
    } else {
        colors::text_disabled()
    };
    let icon_rect =
        Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(16.0, 16.0));
    if checked == Some(true) {
        let hl = icon_rect.expand(3.0);
        p.rect_filled(hl, CornerRadius::ZERO, colors::btn_checked_fill());
        p.rect_stroke(
            hl,
            CornerRadius::ZERO,
            Stroke::new(1.0, colors::btn_checked_border()),
            StrokeKind::Inside,
        );
    }
    match icon {
        Some(i) => i.paint(&p, icon_rect, enabled),
        None if checked == Some(true) => Icon::Check.paint(&p, icon_rect, enabled),
        None => {}
    }
    p.text(
        pos2(rect.left() + MENU_LEFT, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::font(),
        col,
    );
    if let Some(s) = shortcut {
        p.text(
            pos2(rect.right() - MENU_RIGHT, rect.center().y),
            Align2::RIGHT_CENTER,
            s,
            theme::font(),
            if enabled {
                colors::text_secondary()
            } else {
                colors::text_disabled()
            },
        );
    }
    resp
}

/// A row that opens a sub-menu. Drawn exactly like [`menu_row`] (same width rules, hover
/// colour and label column) with an arrow on the right, instead of egui's own button whose
/// popup stretched to whatever width the parent offered.
fn menu_sub_row(
    ui: &mut Ui,
    label: &str,
    items: &[MenuEntry],
    state: &ExplorerState,
    picked: &mut Option<Command>,
) {
    let natural = menu_natural_width(ui, label, Some(MENU_ARROW_W));
    let id = ui.scope_id().with(("menu_sub_row", label));
    let rect = menu_row_rect(ui, natural, MENU_ROW_H);
    let resp = ui.interact(rect, id, Sense::click());
    let open = SubMenu::new()
        .show(ui, &resp, |ui| render_menu(ui, items, state, picked))
        .is_some();

    let p = row_painter(ui, rect);
    if resp.hovered() || open {
        p.rect_filled(rect, CornerRadius::ZERO, menu_hover());
    }
    p.text(
        pos2(rect.left() + MENU_LEFT, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::font(),
        colors::text(),
    );
    Icon::ChevronRight.paint(
        &p,
        Rect::from_center_size(
            pos2(
                rect.right() - MENU_RIGHT - MENU_ARROW_W / 2.0,
                rect.center().y,
            ),
            vec2(MENU_ARROW_W, MENU_ARROW_W),
        ),
        true,
    );
}

pub fn menu_separator(ui: &mut Ui) {
    let rect = menu_row_rect(ui, 0.0, 5.0);
    ui.painter().line_segment(
        [
            pos2(rect.left() + 30.0, rect.center().y),
            pos2(rect.right() - 2.0, rect.center().y),
        ],
        Stroke::new(1.0, colors::separator()),
    );
}

/// Render [`MenuEntry`]s. `picked` receives the chosen command.
pub fn render_menu(
    ui: &mut Ui,
    entries: &[MenuEntry],
    state: &ExplorerState,
    picked: &mut Option<Command>,
) {
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    // egui builds the frame of every sub-menu popup from the parent ui's visuals; make them
    // match `menu_frame()` so sub-menus look like the menu they belong to.
    let v = ui.visuals_mut();
    v.window_fill = menu_bg();
    v.window_stroke = Stroke::new(1.0, colors::ribbon_border());
    v.popup_shadow = menu_shadow();
    for e in entries {
        match e {
            MenuEntry::Item {
                label,
                cmd,
                icon,
                shortcut,
            } => {
                let checked = state.is_checked(*cmd);
                let enabled = state.is_enabled(*cmd);
                if menu_row(ui, *icon, label, checked, enabled, *shortcut).clicked() {
                    *picked = Some(*cmd);
                }
            }
            MenuEntry::Sep => menu_separator(ui),
            MenuEntry::Header(t) => {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(*t)
                        .color(colors::text_secondary())
                        .small(),
                );
            }
            MenuEntry::Sub { label, items } => menu_sub_row(ui, label, items, state, picked),
        }
    }
}

// ---------------------------------------------------------------------------
// File menu (the blue tab)
// ---------------------------------------------------------------------------

fn show_file_menu(
    ui: &mut Ui,
    state: &ExplorerState,
    chosen: &mut Option<Command>,
    nav_to: &mut Option<std::path::PathBuf>,
) {
    ui.set_width(511.0);
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    ui.visuals_mut().widgets.hovered.weak_bg_fill = colors::item_hover();
    ui.horizontal_top(|ui| {
        ui.allocate_ui(vec2(250.0, 0.0), |ui| {
            ui.set_width(250.0);
            ui.add_space(4.0);
            for (label, icon, cmd) in [
                ("Open new window", Icon::NewWindow, Command::OpenNewWindow),
                ("Open terminal here", Icon::Terminal, Command::OpenTerminal),
                (
                    "Delete history",
                    Icon::DeleteHistory,
                    Command::DeleteHistory,
                ),
                ("Help", Icon::Help, Command::Help),
            ] {
                if file_menu_row(ui, icon, label).clicked() {
                    *chosen = Some(cmd);
                }
            }
            ui.add_space(2.0);
            let (r, _) = ui.allocate_exact_size(vec2(250.0, 1.0), Sense::hover());
            ui.painter().rect_filled(
                r.shrink2(vec2(6.0, 0.0)),
                CornerRadius::ZERO,
                colors::separator(),
            );
            ui.add_space(2.0);
            if file_menu_row(ui, Icon::Close, "Close").clicked() {
                *chosen = Some(Command::Close);
            }
            ui.add_space(4.0);
        });
        // vertical separator
        let h = ui.min_rect().height();
        let (r, _) = ui.allocate_exact_size(vec2(1.0, h), Sense::hover());
        ui.painter()
            .rect_filled(r, CornerRadius::ZERO, colors::separator());
        ui.allocate_ui(vec2(260.0, 0.0), |ui| {
            ui.set_width(260.0);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Frequent places")
                        .color(colors::group_text())
                        .size(15.0),
                );
            });
            ui.add_space(4.0);
            let frequent: Vec<&NavNode> = state
                .nav
                .iter()
                .find(|n| n.label == "Quick access")
                .and_then(|n| n.children.as_ref())
                .map(|c| c.iter().take(8).collect())
                .unwrap_or_default();
            for n in frequent {
                let resp = file_menu_row(ui, n.icon, &n.label);
                if resp.clicked() {
                    *nav_to = Some(n.path.clone());
                }
                resp.on_hover_text(n.path.display().to_string());
            }
            ui.add_space(6.0);
        });
    });
}

fn file_menu_row(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    let painter = ui.painter().with_clip_rect(rect);
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::ZERO, colors::item_hover());
    }
    icon.paint(
        &painter,
        Rect::from_center_size(pos2(rect.left() + 24.0, rect.center().y), vec2(20.0, 20.0)),
        true,
    );
    painter.text(
        pos2(rect.left() + 48.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        theme::font(),
        colors::text(),
    );
    resp
}

/// Longest Alt press that still counts as a tap that shows the key tips.
const KEY_TIP_TAP_MAX_SECS: f64 = 0.8;

fn key_tip_badge(painter: &Painter, center: egui::Pos2, text: &str) {
    let font = egui::FontId::proportional(11.0);
    let galley = painter.layout_no_wrap(text.to_owned(), font, colors::text());
    let size = galley.size() + vec2(8.0, 2.0);
    let r = Rect::from_center_size(center, size);
    painter.rect_filled(
        r,
        CornerRadius::ZERO,
        if theme::dark_mode() {
            Color32::from_rgb(76, 65, 30)
        } else {
            Color32::from_rgb(255, 250, 205)
        },
    );
    painter.rect_stroke(
        r,
        CornerRadius::ZERO,
        Stroke::new(1.0, colors::field_border()),
        StrokeKind::Inside,
    );
    painter.galley(r.center() - galley.size() / 2.0, galley, colors::text());
}
