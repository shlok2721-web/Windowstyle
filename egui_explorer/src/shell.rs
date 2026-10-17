//! Window chrome and composition: title bar + Quick Access Toolbar, ribbon,
//! navigation buttons, breadcrumb address bar, search box, navigation pane,
//! item pane, details/preview pane, status bar and keyboard shortcuts.

use std::path::{Path, PathBuf};

use egui::epaint::StrokeKind;
use egui::text::{CCursor, CCursorRange};
use egui::{
    Align2, Color32, CornerRadius, Frame, Id, Key, Panel, Popup, Rect, Response, RichText,
    ScrollArea, Sense, Stroke, TextEdit, Ui, UiBuilder, ViewportCommand, pos2, vec2,
};

use crate::icons::Icon;
use crate::model::*;
use crate::ribbon::{BODY_H, Ribbon, TAB_H, context_menu, menu_frame, menu_row, menu_separator};
use crate::theme::{self, colors};
use crate::{dialogs, file_list, props};

const TITLE_H: f32 = 31.0;
const QAT_H: f32 = 25.0;
const NAVROW_H: f32 = 38.0;
const STATUS_H: f32 = 23.0;

/// Id of the address bar's text edit (focus it with `ctx.memory_mut(|m| m.request_focus(..))`).
pub fn address_edit_id() -> Id {
    Id::new("exp_address_edit")
}
/// Id of the search box's text edit.
pub fn search_edit_id() -> Id {
    Id::new("exp_search_edit")
}

#[derive(Clone, Debug)]
pub struct ExplorerOptions {
    /// Draw our own title bar (use together with `ViewportBuilder::with_decorations(false)`).
    pub title_bar: bool,
    /// Minimize / maximize / close buttons in the title bar.
    pub window_controls: bool,
    pub app_icon: Icon,
}

impl Default for ExplorerOptions {
    fn default() -> Self {
        Self {
            title_bar: true,
            window_controls: true,
            app_icon: Icon::Folder,
        }
    }
}

/// The complete Windows 10 style file explorer window.
pub struct Explorer {
    pub ribbon: Ribbon,
    pub options: ExplorerOptions,
    applied_dark_mode: Option<bool>,
}

impl Default for Explorer {
    fn default() -> Self {
        Self::new()
    }
}

impl Explorer {
    pub fn new() -> Self {
        Self {
            ribbon: Ribbon::windows10(),
            options: ExplorerOptions::default(),
            applied_dark_mode: None,
        }
    }

    /// Draw the whole explorer into `ui` (normally the root `Ui` of the window)
    /// and return the events the host has to handle.
    pub fn show(&mut self, ui: &mut Ui, state: &mut ExplorerState) -> Vec<Event> {
        let ctx = ui.ctx().clone();
        if self.applied_dark_mode != Some(state.dark_mode) {
            theme::apply_mode(&ctx, state.dark_mode);
            self.applied_dark_mode = Some(state.dark_mode);
        }
        state.window_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        global_keys(&ctx, state);
        info_windows(&ctx, state);

        // ---- title bar ------------------------------------------------------
        let mut title_rect = None;
        if self.options.title_bar {
            let r = Panel::top(Id::new("exp_title"))
                .exact_size(TITLE_H)
                .frame(Frame::NONE.fill(colors::chrome_bg()))
                .show_separator_line(false)
                .show(ui, |ui| title_bar(ui, state, &self.options));
            title_rect = Some(r.response.rect);
        }

        // ---- ribbon ---------------------------------------------------------
        let rib_h = match state.ribbon_mode {
            RibbonMode::Expanded => TAB_H + BODY_H,
            RibbonMode::Minimized => TAB_H,
            RibbonMode::Hidden => 0.0,
        };
        let mut header = None;
        Panel::top(Id::new("exp_ribbon"))
            .exact_size(rib_h)
            .frame(Frame::NONE.fill(colors::chrome_bg()))
            .show_separator_line(false)
            .show(ui, |ui| {
                header = self.ribbon.show(ui, state).context_header;
            });
        if let (Some(h), Some(tr)) = (header, title_rect) {
            let r = Rect::from_min_max(
                pos2(h.x_min, tr.bottom() - 20.0),
                pos2(h.x_max, tr.bottom()),
            );
            let p = ui.painter();
            p.rect_filled(r, CornerRadius::ZERO, h.color);
            p.text(
                r.center(),
                Align2::CENTER_CENTER,
                h.label,
                theme::font(),
                Color32::WHITE,
            );
        }

        // ---- quick access toolbar (when shown below the ribbon) -----------------
        if state.qat_below_ribbon || !self.options.title_bar {
            Panel::top(Id::new("exp_qat"))
                .exact_size(QAT_H)
                .frame(Frame::NONE.fill(colors::qat_bg()))
                .show_separator_line(false)
                .show(ui, |ui| {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), QAT_H), Sense::hover());
                    ui.painter().line_segment(
                        [r.left_bottom(), r.right_bottom()],
                        Stroke::new(1.0, colors::separator()),
                    );
                    qat(ui, state, r.left() + 6.0, r.center().y);
                    if !self.options.title_bar {
                        ribbon_display_button(ui, state, pos2(r.right() - 20.0, r.center().y));
                    }
                });
        }

        // ---- navigation row ------------------------------------------------------
        Panel::top(Id::new("exp_navrow"))
            .exact_size(NAVROW_H)
            .frame(Frame::NONE.fill(colors::navrow_bg()))
            .show_separator_line(false)
            .show(ui, |ui| nav_row(ui, state));

        // The title bar, ribbon, quick-access bar and navigation row above are
        // the only panels that can change the selection this frame, and they
        // have all run by now. Summarise it once (memoised, O(1) when nothing
        // is selected) and share the result with the status bar and side pane
        // instead of each re-scanning the folder.
        let sel = state.selection_summary();

        // ---- status bar -------------------------------------------------------------
        Panel::bottom(Id::new("exp_status"))
            .exact_size(STATUS_H)
            .frame(Frame::NONE.fill(colors::chrome_bg()))
            .show_separator_line(false)
            .show(ui, |ui| status_bar(ui, state, sel));

        // ---- navigation pane -----------------------------------------------------------
        if state.show_nav_pane {
            Panel::left(Id::new("exp_nav"))
                .resizable(true)
                .default_size(220.0)
                .min_size(120.0)
                .max_size(480.0)
                .frame(Frame::NONE.fill(colors::nav_bg()))
                .show(ui, |ui| nav_pane(ui, state));
        }

        // ---- preview / details pane --------------------------------------------------------
        if state.show_preview_pane || state.show_details_pane {
            Panel::right(Id::new("exp_side"))
                .resizable(true)
                .default_size(260.0)
                .min_size(160.0)
                .max_size(560.0)
                .frame(
                    Frame::NONE
                        .fill(colors::panel_bg())
                        .inner_margin(egui::Margin::same(10)),
                )
                .show(ui, |ui| side_pane(ui, state, sel));
        }

        // ---- item pane -------------------------------------------------------------------------
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(colors::window_bg()))
            .show(ui, |ui| file_list::show(ui, state));

        if self.options.title_bar && self.options.window_controls {
            resize_grips(&ctx, ui);
        }

        // ---- window-level commands -------------------------------------------------------------------
        let mut events = state.take_events();
        events.retain(|e| match e {
            Event::Command {
                cmd: Command::Close,
                ..
            } => {
                ctx.send_viewport_cmd(ViewportCommand::Close);
                false
            }
            _ => true,
        });
        events
    }
}

// ---------------------------------------------------------------------------
// Keyboard shortcuts
// ---------------------------------------------------------------------------

fn global_keys(ctx: &egui::Context, state: &mut ExplorerState) {
    if ctx.egui_wants_keyboard_input() || crate::dialogs::modal_open(ctx) {
        return;
    }
    let mut cmds: Vec<Command> = Vec::new();
    let mut focus_address = false;
    let mut focus_search = false;
    // Facts about the clipboard chord gathered inside the input closure (egui's context
    // lock is held there, so the per-chord memory is read and written after it).
    let mut paste_event = false;
    let mut v_released = false;
    let mut now = 0.0_f64;
    let mut mods_now = (false, false, false); // (ctrl, shift, alt)
    ctx.input(|i| {
        let m = i.modifiers;
        now = i.time;
        mods_now = (m.ctrl, m.shift, m.alt);
        v_released = i.key_released(Key::V);
        let k = |key: Key, ctrl: bool, shift: bool, alt: bool| {
            i.key_pressed(key) && m.ctrl == ctrl && m.shift == shift && m.alt == alt
        };
        // egui-winit turns *every* Ctrl+C / Ctrl+V chord into `Event::Copy` / `Event::Paste`
        // (it ignores Shift and Alt) and swallows the key event, so `key_pressed(Key::C)`
        // never fires for Ctrl+Shift+C, Ctrl+Alt+C, Ctrl+Alt+Shift+C or Ctrl+Shift+V.
        // Tell the variants apart here from the live modifier state instead.
        for e in &i.events {
            match e {
                egui::Event::Copy => {
                    let cmd = match (m.shift, m.alt) {
                        (false, false) => Command::Copy,
                        (true, false) => Command::CopyPath, // Ctrl+Shift+C: full path(s)
                        (false, true) => Command::CopyName, // Ctrl+Alt+C: file name(s)
                        (true, true) => Command::CopyFolderPath, // Ctrl+Alt+Shift+C: folder
                    };
                    cmds.push(cmd);
                }
                // Ctrl+Shift+X is not bound to anything; do not turn it into a plain Cut.
                egui::Event::Cut if !m.shift && !m.alt => cmds.push(Command::Cut),
                egui::Event::Paste(_) => {
                    paste_event = true;
                    // Ctrl+Shift+V: paste as shortcut, never an ordinary paste.
                    let cmd = if m.shift && !m.alt {
                        Command::PasteShortcut
                    } else {
                        Command::Paste
                    };
                    if !cmds.contains(&cmd) {
                        cmds.push(cmd);
                    }
                }
                _ => {}
            }
        }
        // Fallbacks for backends that deliver these chords as plain key events.
        let mut push_once = |cmd: Command| {
            if !cmds.contains(&cmd) {
                cmds.push(cmd);
            }
        };
        if k(Key::C, true, true, false) {
            push_once(Command::CopyPath);
        }
        if k(Key::C, true, false, true) {
            push_once(Command::CopyName);
        }
        if k(Key::C, true, true, true) {
            push_once(Command::CopyFolderPath);
        }
        if k(Key::V, true, true, false) {
            push_once(Command::PasteShortcut);
        }
        // Ctrl+D: delete (same as Del); Ctrl+Shift+A: clear selection; Ctrl+I: invert.
        if k(Key::D, true, false, false) {
            cmds.push(Command::Delete);
        }
        if k(Key::A, true, true, false) {
            cmds.push(Command::SelectNone);
        }
        if k(Key::I, true, false, false) {
            cmds.push(Command::InvertSelection);
        }
        // Ctrl+Enter: open the selection.
        if k(Key::Enter, true, false, false) {
            cmds.push(Command::Open);
        }
        // Ctrl+Shift+T: new text file; Ctrl+Alt+T: open a terminal here.
        if k(Key::T, true, true, false) {
            cmds.push(Command::NewTextFile);
        }
        if k(Key::T, true, false, true) {
            cmds.push(Command::OpenTerminal);
        }
        // Ctrl+H: toggle hidden items; Ctrl+Shift+H: toggle file extensions.
        if k(Key::H, true, false, false) {
            cmds.push(Command::ToggleHiddenItems);
        }
        if k(Key::H, true, true, false) {
            cmds.push(Command::ToggleFileExtensions);
        }
        // Alt+N: toggle the navigation pane.
        if k(Key::N, false, false, true) {
            cmds.push(Command::ToggleNavPane);
        }
        if k(Key::V, true, false, false) && !cmds.contains(&Command::Paste) {
            cmds.push(Command::Paste);
        }
        if k(Key::A, true, false, false) {
            cmds.push(Command::SelectAll);
        }
        if k(Key::Z, true, false, false) {
            cmds.push(Command::Undo);
        }
        if k(Key::Y, true, false, false) {
            cmds.push(Command::Redo);
        }
        if k(Key::Delete, false, false, false) {
            cmds.push(Command::Delete);
        }
        if k(Key::Delete, false, true, false) {
            cmds.push(Command::DeletePermanently);
        }
        if k(Key::F2, false, false, false) {
            cmds.push(Command::Rename);
        }
        if k(Key::F5, false, false, false) || k(Key::R, true, false, false) {
            cmds.push(Command::Refresh);
        }
        if k(Key::F1, false, false, false) {
            cmds.push(Command::Help);
        }
        if k(Key::N, true, true, false) {
            cmds.push(Command::NewFolder);
        }
        if k(Key::N, true, false, false) {
            cmds.push(Command::OpenNewWindow);
        }
        if k(Key::Enter, false, false, true) {
            cmds.push(Command::Properties);
        }
        if k(Key::ArrowLeft, false, false, true) || k(Key::Backspace, false, false, false) {
            cmds.push(Command::Back);
        }
        if k(Key::ArrowRight, false, false, true) {
            cmds.push(Command::Forward);
        }
        if k(Key::ArrowUp, false, false, true) {
            cmds.push(Command::Up);
        }
        if k(Key::P, false, false, true) {
            cmds.push(Command::TogglePreviewPane);
        }
        if k(Key::P, false, true, true) {
            cmds.push(Command::ToggleDetailsPane);
        }
        let views = [
            (Key::Num1, ViewMode::ExtraLargeIcons),
            (Key::Num2, ViewMode::LargeIcons),
            (Key::Num3, ViewMode::MediumIcons),
            (Key::Num4, ViewMode::SmallIcons),
            (Key::Num5, ViewMode::List),
            (Key::Num6, ViewMode::Details),
            (Key::Num7, ViewMode::Tiles),
            (Key::Num8, ViewMode::Content),
        ];
        for (key, v) in views {
            if k(key, true, true, false) {
                cmds.push(Command::SetView(v));
            }
        }
        if k(Key::L, true, false, false)
            || k(Key::D, false, false, true)
            || k(Key::F4, false, false, false)
        {
            focus_address = true;
        }
        if k(Key::E, true, false, false)
            || k(Key::F, true, false, false)
            || k(Key::F3, false, false, false)
        {
            focus_search = true;
        }
    });
    // egui-winit only emits `Event::Paste` when the system clipboard holds *text*. When this
    // app has published a file list (`text/uri-list`) there is no text target, so the paste
    // chord arrives as nothing but the release of V. Recover it from that release, provided
    // Ctrl was held at (or just before) the release and no Paste event was seen for this
    // press, and only when this app actually has something to paste.
    {
        let chord_id = Id::new("ws_clipboard_chord");
        let (mut ctrl_t, mut shift, mut alt, mut paste_seen) = ctx
            .data(|d| d.get_temp::<(f64, bool, bool, bool)>(chord_id))
            .unwrap_or((f64::NEG_INFINITY, false, false, false));
        if paste_event {
            paste_seen = true;
        }
        if mods_now.0 {
            ctrl_t = now;
            shift = mods_now.1;
            alt = mods_now.2;
        }
        if v_released {
            if !paste_seen && state.clipboard_has_items && now - ctrl_t <= 0.3 {
                let cmd = if shift && !alt {
                    Command::PasteShortcut
                } else {
                    Command::Paste
                };
                if !cmds.contains(&cmd) {
                    cmds.push(cmd);
                }
            }
            paste_seen = false;
        }
        ctx.data_mut(|d| d.insert_temp(chord_id, (ctrl_t, shift, alt, paste_seen)));
    }
    for c in cmds {
        state.dispatch(c);
    }
    if focus_address {
        begin_address_edit(ctx, state);
    }
    if focus_search {
        ctx.memory_mut(|m| m.request_focus(search_edit_id()));
    }
}

fn begin_address_edit(ctx: &egui::Context, state: &mut ExplorerState) {
    state.address_editing = true;
    state.address_text = state.location.to_string_lossy().into_owned();
    ctx.data_mut(|d| d.insert_temp(Id::new("addr_just_started"), true));
}

// ---------------------------------------------------------------------------
// Small shared widgets
// ---------------------------------------------------------------------------

fn icon_button(ui: &mut Ui, rect: Rect, id: Id, icon: Icon, enabled: bool, tip: &str) -> Response {
    let resp = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let p = ui.painter();
    if enabled {
        if resp.is_pointer_button_down_on() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::btn_pressed_fill());
            p.rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::btn_pressed_border()),
                StrokeKind::Inside,
            );
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::ZERO, colors::btn_hover_fill());
            p.rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::btn_hover_border()),
                StrokeKind::Inside,
            );
        }
    }
    icon.paint(
        p,
        Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
        enabled,
    );
    resp.on_hover_text(tip)
}

fn command_meta(cmd: Command) -> (Icon, &'static str) {
    match cmd {
        Command::Properties => (Icon::Properties, "Properties"),
        Command::NewFolder => (Icon::NewFolder, "New folder"),
        Command::Undo => (Icon::Undo, "Undo"),
        Command::Redo => (Icon::Redo, "Redo"),
        Command::Delete => (Icon::Delete, "Delete"),
        Command::Rename => (Icon::Rename, "Rename"),
        Command::Copy => (Icon::Copy, "Copy"),
        Command::Cut => (Icon::Cut, "Cut"),
        Command::Paste => (Icon::Paste, "Paste"),
        Command::Refresh => (Icon::Refresh, "Refresh"),
        _ => (Icon::File, "Command"),
    }
}

const QAT_CHOICES: [Command; 6] = [
    Command::Undo,
    Command::Redo,
    Command::Delete,
    Command::Properties,
    Command::NewFolder,
    Command::Rename,
];

// ---------------------------------------------------------------------------
// Title bar + QAT
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Cap {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn caption_button(ui: &mut Ui, rect: Rect, id: &str, kind: Cap) -> bool {
    let resp = ui.interact(rect, Id::new(id), Sense::click());
    let close = kind == Cap::Close;
    let p = ui.painter();
    if resp.is_pointer_button_down_on() {
        p.rect_filled(
            rect,
            CornerRadius::ZERO,
            if close {
                colors::close_pressed()
            } else {
                colors::caption_pressed()
            },
        );
    } else if resp.hovered() {
        p.rect_filled(
            rect,
            CornerRadius::ZERO,
            if close {
                colors::close_hover()
            } else {
                colors::caption_hover()
            },
        );
    }
    let col = if close && resp.hovered() {
        Color32::WHITE
    } else {
        colors::text()
    };
    let s = Stroke::new(1.0, col);
    let c = rect.center();
    match kind {
        Cap::Minimize => {
            p.line_segment([pos2(c.x - 5.0, c.y + 0.5), pos2(c.x + 5.0, c.y + 0.5)], s);
        }
        Cap::Maximize => {
            p.rect_stroke(
                Rect::from_center_size(c, vec2(10.0, 10.0)),
                CornerRadius::ZERO,
                s,
                StrokeKind::Inside,
            );
        }
        Cap::Restore => {
            p.rect_stroke(
                Rect::from_min_size(pos2(c.x - 5.0, c.y - 2.0), vec2(8.0, 8.0)),
                CornerRadius::ZERO,
                s,
                StrokeKind::Inside,
            );
            p.line_segment([pos2(c.x - 2.0, c.y - 2.0), pos2(c.x - 2.0, c.y - 5.0)], s);
            p.line_segment([pos2(c.x - 2.0, c.y - 5.0), pos2(c.x + 5.0, c.y - 5.0)], s);
            p.line_segment([pos2(c.x + 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 2.0)], s);
            p.line_segment([pos2(c.x + 5.0, c.y + 2.0), pos2(c.x + 3.0, c.y + 2.0)], s);
        }
        Cap::Close => {
            p.line_segment([pos2(c.x - 5.0, c.y - 5.0), pos2(c.x + 5.0, c.y + 5.0)], s);
            p.line_segment([pos2(c.x + 5.0, c.y - 5.0), pos2(c.x - 5.0, c.y + 5.0)], s);
        }
    }
    resp.clicked()
}

fn title_bar(ui: &mut Ui, state: &mut ExplorerState, opts: &ExplorerOptions) {
    let ctx = ui.ctx().clone();
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_H), Sense::hover());
    let cy = rect.center().y - 1.0;
    // background drag area first, so buttons on top win the hit test
    let drag = ui.interact(rect, Id::new("exp_title_drag"), Sense::click_and_drag());

    // ---- window controls (right) ------------------------------------------------------
    let mut right = rect.right();
    if opts.window_controls {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let w = 46.0;
        let r = |n: f32| {
            Rect::from_min_max(
                pos2(rect.right() - w * n, rect.top()),
                pos2(rect.right() - w * (n - 1.0), rect.top() + TITLE_H - 1.0),
            )
        };
        if caption_button(ui, r(1.0), "cap_close", Cap::Close) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        let kind = if maximized {
            Cap::Restore
        } else {
            Cap::Maximize
        };
        if caption_button(ui, r(2.0), "cap_max", kind) {
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }
        if caption_button(ui, r(3.0), "cap_min", Cap::Minimize) {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        right -= w * 3.0;
    }

    ribbon_display_button(ui, state, pos2(right - 20.0, cy));

    // ---- left: app icon, QAT ---------------------------------------------------------------------------
    opts.app_icon.paint(
        ui.painter(),
        Rect::from_center_size(pos2(rect.left() + 16.0, cy), vec2(16.0, 16.0)),
        true,
    );
    let mut x = rect.left() + 32.0;
    if !state.qat_below_ribbon {
        ui.painter().line_segment(
            [pos2(x, cy - 8.0), pos2(x, cy + 8.0)],
            Stroke::new(1.0, colors::separator()),
        );
        x = qat(ui, state, x + 4.0, cy);
    }

    // ---- centred title -------------------------------------------------------------------------------------------
    let tcol = if state.window_focused {
        colors::text()
    } else {
        colors::text_disabled()
    };
    let tx = rect.center().x.max(x + 80.0);
    ui.painter().text(
        pos2(tx, cy),
        Align2::CENTER_CENTER,
        &state.title,
        theme::font(),
        tcol,
    );

    // ---- drag / double-click ------------------------------------------------------------------------------------------
    if opts.window_controls {
        if drag.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }
    }
    if drag.clicked() && state.ribbon_mode == RibbonMode::Hidden {
        state.ribbon_overlay = true;
    }
}

/// The "Ribbon Display Options" button (Auto-hide / Show tabs / Show tabs and commands).
fn ribbon_display_button(ui: &mut Ui, state: &mut ExplorerState, center: egui::Pos2) {
    let rb = Rect::from_center_size(center, vec2(28.0, 22.0));
    let rbr = icon_button(
        ui,
        rb,
        Id::new("ribbon_display_options"),
        Icon::Ribbon,
        true,
        "Ribbon Display Options",
    );
    let mut mode: Option<RibbonMode> = None;
    Popup::menu(&rbr).gap(0.0).frame(menu_frame()).show(|ui| {
        ui.set_min_width(220.0);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for (label, m) in [
            ("Auto-hide Ribbon", RibbonMode::Hidden),
            ("Show tabs only", RibbonMode::Minimized),
            ("Show tabs and commands", RibbonMode::Expanded),
        ] {
            if menu_row(ui, None, label, Some(state.ribbon_mode == m), true, None).clicked() {
                mode = Some(m);
            }
        }
    });
    if let Some(m) = mode {
        state.dispatch(Command::SetRibbonMode(m));
    }
}

/// Invisible grips on the window edges so a frameless window can be resized.
fn resize_grips(ctx: &egui::Context, ui: &mut Ui) {
    use egui::viewport::ResizeDirection as D;
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let r = ctx.content_rect();
    let t = 4.0;
    let c = 10.0;
    let grips = [
        (
            Rect::from_min_max(
                pos2(r.left() + c, r.top()),
                pos2(r.right() - c, r.top() + t),
            ),
            D::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(
                pos2(r.left() + c, r.bottom() - t),
                pos2(r.right() - c, r.bottom()),
            ),
            D::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(
                pos2(r.left(), r.top() + c),
                pos2(r.left() + t, r.bottom() - c),
            ),
            D::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(
                pos2(r.right() - t, r.top() + c),
                pos2(r.right(), r.bottom() - c),
            ),
            D::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_size(r.left_top(), vec2(c, c)),
            D::NorthWest,
            egui::CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_size(pos2(r.right() - c, r.top()), vec2(c, c)),
            D::NorthEast,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_size(pos2(r.left(), r.bottom() - c), vec2(c, c)),
            D::SouthWest,
            egui::CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_size(pos2(r.right() - c, r.bottom() - c), vec2(c, c)),
            D::SouthEast,
            egui::CursorIcon::ResizeNwSe,
        ),
    ];
    for (i, (rect, dir, cursor)) in grips.into_iter().enumerate() {
        let resp = ui.interact(rect, Id::new(("win_grip", i)), Sense::drag());
        if resp.hovered() {
            ctx.set_cursor_icon(cursor);
        }
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

/// Draw the Quick Access Toolbar buttons; returns the x after the last one.
fn qat(ui: &mut Ui, state: &mut ExplorerState, left: f32, cy: f32) -> f32 {
    let mut x = left;
    for (i, cmd) in state.qat_items.clone().into_iter().enumerate() {
        let (icon, tip) = command_meta(cmd);
        let r = Rect::from_center_size(pos2(x + 11.0, cy), vec2(22.0, 22.0));
        let enabled = state.is_enabled(cmd);
        if icon_button(ui, r, Id::new(("qat", i)), icon, enabled, tip).clicked() {
            state.dispatch(cmd);
        }
        x += 22.0;
    }
    let r = Rect::from_center_size(pos2(x + 8.0, cy), vec2(16.0, 22.0));
    let resp = icon_button(
        ui,
        r,
        Id::new("qat_customize"),
        Icon::ChevronDown,
        true,
        "Customize Quick Access Toolbar",
    );
    let mut toggle: Option<Command> = None;
    let mut cmd: Option<Command> = None;
    Popup::menu(&resp).gap(0.0).frame(menu_frame()).show(|ui| {
        ui.set_min_width(240.0);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Customize Quick Access Toolbar")
                    .color(colors::text_secondary())
                    .small(),
            );
        });
        for c in QAT_CHOICES {
            let (icon, label) = command_meta(c);
            let on = state.qat_items.contains(&c);
            if menu_row(ui, Some(icon), label, Some(on), true, None).clicked() {
                toggle = Some(c);
            }
        }
        menu_separator(ui);
        let below = if state.qat_below_ribbon {
            "Show above the Ribbon"
        } else {
            "Show below the Ribbon"
        };
        if menu_row(ui, None, below, None, true, None).clicked() {
            cmd = Some(Command::ToggleQatPosition);
        }
        let min = state.ribbon_mode != RibbonMode::Expanded;
        if menu_row(
            ui,
            None,
            "Minimize the Ribbon",
            Some(min),
            true,
            Some("Ctrl+F1"),
        )
        .clicked()
        {
            cmd = Some(Command::ToggleRibbon);
        }
    });
    if let Some(c) = toggle {
        if let Some(pos) = state.qat_items.iter().position(|x| *x == c) {
            state.qat_items.remove(pos);
        } else {
            // keep the canonical order of QAT_CHOICES
            let mut items: Vec<Command> = QAT_CHOICES
                .iter()
                .copied()
                .filter(|x| *x == c || state.qat_items.contains(x))
                .collect();
            items.dedup();
            state.qat_items = items;
        }
    }
    if let Some(c) = cmd {
        state.dispatch(c);
    }
    x + 18.0
}

// ---------------------------------------------------------------------------
// Navigation row: back / forward / recent / up, address bar, search box
// ---------------------------------------------------------------------------

fn nav_row(ui: &mut Ui, state: &mut ExplorerState) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), NAVROW_H), Sense::hover());
    let cy = row.center().y;
    let mut x = row.left() + 6.0;

    let back = icon_button(
        ui,
        Rect::from_center_size(pos2(x + 13.0, cy), vec2(26.0, 26.0)),
        Id::new("nav_back"),
        Icon::Back,
        state.can_go_back(),
        "Back (Alt+Left Arrow)",
    );
    if back.clicked() {
        state.go_back();
    }
    x += 28.0;
    let fwd = icon_button(
        ui,
        Rect::from_center_size(pos2(x + 13.0, cy), vec2(26.0, 26.0)),
        Id::new("nav_fwd"),
        Icon::Forward,
        state.can_go_forward(),
        "Forward (Alt+Right Arrow)",
    );
    if fwd.clicked() {
        state.go_forward();
    }
    x += 28.0;
    let recent = icon_button(
        ui,
        Rect::from_center_size(pos2(x + 7.0, cy), vec2(14.0, 26.0)),
        Id::new("nav_recent"),
        Icon::ChevronDown,
        true,
        "Recent locations",
    );
    history_popup(&recent, state);
    x += 16.0;
    let up = icon_button(
        ui,
        Rect::from_center_size(pos2(x + 13.0, cy), vec2(26.0, 26.0)),
        Id::new("nav_up"),
        Icon::Up,
        state.can_go_up(),
        "Up to parent folder (Alt+Up Arrow)",
    );
    if up.clicked() {
        state.go_up();
    }
    x += 34.0;

    let search_w = ((row.right() - x) * 0.28).clamp(140.0, 260.0);
    let search = Rect::from_min_max(
        pos2(row.right() - 8.0 - search_w, cy - 13.0),
        pos2(row.right() - 8.0, cy + 13.0),
    );
    let addr = Rect::from_min_max(
        pos2(x, cy - 13.0),
        pos2((search.left() - 8.0).max(x + 60.0), cy + 13.0),
    );
    address_bar(ui, state, addr);
    search_box(ui, state, search);
}

/// Number of the current info-window "session". It changes whenever a (different) window
/// opens, so every opening gets a fresh window id and is centred again, even if the
/// previous one was dragged somewhere else.
fn info_session(ctx: &egui::Context, kind: Option<InfoWindow>) -> u32 {
    let id = Id::new("exp_info_session");
    ctx.data_mut(|d| {
        let (last, serial): &mut (Option<InfoWindow>, u32) = d.get_temp_mut_or_default(id);
        if *last != kind {
            *last = kind;
            if kind.is_some() {
                *serial = serial.wrapping_add(1);
            }
        }
        *serial
    })
}

const HELP_KEYS: [(&str, &str); 31] = [
    ("Ctrl+C / X / V", "Copy / Cut / Paste"),
    ("Ctrl+Shift+C", "Copy full path(s)"),
    ("Ctrl+Alt+C", "Copy file name(s)"),
    ("Ctrl+Alt+Shift+C", "Copy containing folder path"),
    ("Ctrl+Shift+V", "Paste as shortcut"),
    ("Ctrl+A", "Select all"),
    ("Ctrl+Shift+A", "Clear selection"),
    ("Ctrl+I", "Invert selection"),
    ("Ctrl+Z / Ctrl+Y", "Undo / Redo"),
    ("Ctrl+D / Del", "Delete"),
    ("Shift+Del", "Delete permanently"),
    ("F2", "Rename"),
    ("F5 / Ctrl+R", "Refresh"),
    ("Ctrl+Enter", "Open"),
    ("Ctrl+N", "New window"),
    ("Ctrl+Shift+N", "New folder"),
    ("Ctrl+Shift+T", "New text file"),
    ("Ctrl+Alt+T", "Open terminal here"),
    ("Alt+Enter", "Properties"),
    ("Alt+Left / Right / Up", "Back / Forward / Up"),
    ("Backspace", "Back"),
    ("Ctrl+L / Alt+D / F4", "Edit the address bar"),
    ("Ctrl+E / Ctrl+F / F3", "Search"),
    ("Ctrl+H", "Toggle hidden items"),
    ("Ctrl+Shift+H", "Toggle file extensions"),
    ("Alt+N", "Toggle navigation pane"),
    ("Alt+P", "Toggle preview pane"),
    ("Alt+Shift+P", "Toggle details pane"),
    ("Ctrl+Shift+1..8", "Change view"),
    ("Ctrl+F1", "Minimize / expand the Ribbon"),
    ("F1", "This help"),
];

/// "6 - Normal": the compression level together with a plain-language name.
fn level_text(level: u8) -> String {
    let name = match level {
        1..=3 => "Fast",
        4..=6 => "Normal",
        _ => "Maximum",
    };
    format!("{level} - {name}")
}

fn info_windows(ctx: &egui::Context, state: &mut ExplorerState) {
    let session = info_session(ctx, state.info_window);
    let Some(kind) = state.info_window else {
        props::release(ctx);
        return;
    };
    if kind != InfoWindow::Properties {
        props::release(ctx);
    }
    let (title, width) = match kind {
        InfoWindow::History => ("History", 400.0),
        InfoWindow::Properties => ("Properties", 420.0),
        InfoWindow::Help => ("Help", 420.0),
        InfoWindow::Compress => ("Compress", 380.0),
        InfoWindow::Security => ("Advanced security", 400.0),
    };

    // Properties: the folder statistics are gathered on a worker thread (see `props`) and
    // only read here, so the window opens instantly and fills in as the scan progresses.
    let mut view: Option<props::View> = None;
    if kind == InfoWindow::Properties {
        let sel = state.selected_entries();
        let key = if sel.is_empty() {
            props::key_of(std::iter::once(state.location.as_path()))
        } else {
            props::key_of(
                sel.iter()
                    .filter(|e| e.kind != EntryKind::Drive)
                    .map(|e| e.path.as_path()),
            )
        };
        let scan = props::scan_for(ctx, key, || {
            if sel.is_empty() {
                vec![state.location.clone()]
            } else {
                sel.iter()
                    .filter(|e| e.kind != EntryKind::Drive)
                    .map(|e| e.path.clone())
                    .collect()
            }
        });
        let snap = scan.snapshot();
        if !snap.done {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        view = Some(props::build_view(
            &sel,
            &state.location,
            state.entries.len(),
            &snap,
            state.date_format,
        ));
    }
    let compress_summary = if kind == InfoWindow::Compress {
        match state.selected_entries().as_slice() {
            [] => "No items selected".to_owned(),
            [e] => format!("Compress \"{}\"", e.name),
            many => format!("Compress {} items", many.len()),
        }
    } else {
        String::new()
    };
    let history: Vec<String> = if kind == InfoWindow::History {
        state
            .history
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    } else {
        Vec::new()
    };
    let cursor = state.history_cursor;
    let mut fmt = state.compress_format;
    let mut level = state.compress_level;
    let mut pw = std::mem::take(&mut state.compress_password);
    let mut pw2 = std::mem::take(&mut state.compress_password2);
    let mut run_compress = false;
    let mut close = false;
    let mut jump: Option<usize> = None;

    let outcome = dialogs::show(
        ctx,
        Id::new(("exp_info_window", session)),
        title,
        width,
        |ui| {
            match kind {
                InfoWindow::History => dialogs::body(ui, |ui| {
                    if history.is_empty() {
                        ui.label(RichText::new("No history yet.").color(colors::text_secondary()));
                    }
                    ScrollArea::vertical()
                        .max_height(300.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.with_layout(
                                egui::Layout::top_down_justified(egui::Align::Min),
                                |ui| {
                                    for (i, label) in history.iter().enumerate().rev() {
                                        if ui
                                            .selectable_label(i == cursor, label.as_str())
                                            .clicked()
                                        {
                                            jump = Some(i);
                                        }
                                    }
                                },
                            );
                        });
                }),
                InfoWindow::Properties => {
                    if let Some(v) = &view {
                        dialogs::body(ui, |ui| {
                            dialogs::header(ui, v.icon, &v.title, &v.subtitle);
                            ScrollArea::vertical()
                                .max_height(380.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    dialogs::section(ui, "General");
                                    for row in &v.rows {
                                        match row {
                                            props::Row::Section(t) => dialogs::section(ui, t),
                                            props::Row::Item(k, val) => {
                                                dialogs::kv_row(ui, dialogs::LABEL_W, k, val)
                                            }
                                        }
                                    }
                                });
                        });
                    }
                }
                InfoWindow::Security => dialogs::body(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            "Advanced security settings (permissions, auditing, ownership) \
                             are not supported by this file manager on this platform.",
                        )
                        .wrap(),
                    );
                }),
                InfoWindow::Compress => dialogs::body(ui, |ui| {
                    ui.label(RichText::new(&compress_summary).color(colors::text()));
                    dialogs::section(ui, "Archive format");
                    dialogs::group(ui, |ui| {
                        for f in CompressFormat::ALL {
                            ui.radio_value(&mut fmt, f, f.label());
                        }
                    });
                    dialogs::section(ui, "Compression level");
                    dialogs::group(ui, |ui| {
                        let uses_level = fmt.uses_level();
                        ui.add_enabled_ui(uses_level, |ui| {
                            ui.horizontal(|ui| {
                                let value_w = 96.0;
                                let gap = ui.spacing().item_spacing.x;
                                ui.spacing_mut().slider_width =
                                    (ui.available_width() - value_w - gap).max(80.0);
                                ui.add(egui::Slider::new(&mut level, 1..=9).show_value(false));
                                ui.label(RichText::new(level_text(level)).strong());
                            });
                        });
                        let hint = if uses_level {
                            "Higher levels make smaller archives but take longer."
                        } else {
                            "TAR stores files without compressing them."
                        };
                        ui.label(RichText::new(hint).small().color(colors::text_secondary()));
                    });
                    dialogs::section(ui, "Password (optional)");
                    dialogs::group(ui, |ui| {
                        let can = fmt.supports_password();
                        ui.add_enabled_ui(can, |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut pw)
                                    .password(true)
                                    .hint_text("Password")
                                    .desired_width(f32::INFINITY),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut pw2)
                                    .password(true)
                                    .hint_text("Repeat password")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        if can && pw != pw2 {
                            ui.label(
                                RichText::new("The passwords do not match.")
                                    .small()
                                    .color(Color32::from_rgb(200, 40, 40)),
                            );
                        } else {
                            let note = match fmt {
                                CompressFormat::SevenZip => {
                                    "AES-256 encryption; file names are hidden too."
                                }
                                CompressFormat::Zip => {
                                    "AES-256 encryption (needs 7-Zip installed), otherwise legacy ZipCrypto."
                                }
                                _ => "Password protection is available for ZIP and 7Z.",
                            };
                            ui.label(RichText::new(note).small().color(colors::text_secondary()));
                        }
                    });
                }),
                InfoWindow::Help => dialogs::body(ui, |ui| {
                    ScrollArea::vertical()
                        .max_height(380.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (k, v) in HELP_KEYS {
                                dialogs::kv_row(ui, 150.0, k, v);
                            }
                        });
                }),
            }
            dialogs::footer(ui, |ui| {
                if kind == InfoWindow::Compress {
                    if dialogs::button(ui, "Compress", true).clicked() {
                        run_compress = true;
                    }
                    if dialogs::button(ui, "Cancel", false).clicked() {
                        close = true;
                    }
                } else if dialogs::button(ui, "Close", true).clicked() {
                    close = true;
                }
            });
        },
    );
    // Compress's primary action is the "Compress" button, so Enter runs it; every other info
    // window only has "Close", so Enter (like the backdrop and Escape) just closes it.
    if kind == InfoWindow::Compress {
        run_compress |= outcome.confirmed;
    } else {
        close |= outcome.confirmed;
    }
    close |= outcome.dismissed;

    state.compress_format = fmt;
    state.compress_level = level;
    state.compress_password = pw;
    state.compress_password2 = pw2;
    // Never start compressing while the two password fields disagree.
    if run_compress
        && fmt.supports_password()
        && state.compress_password != state.compress_password2
    {
        run_compress = false;
    }
    if run_compress {
        state.info_window = None;
        if !fmt.supports_password() {
            state.compress_password.clear();
        }
        state.compress_password2.clear();
        state.dispatch(Command::CompressAs(fmt, level));
        return;
    }
    if let Some(i) = jump {
        if let Some(p) = state.history.get(i).cloned() {
            state.history_cursor = i;
            state.emit(Event::Navigate(p));
        }
    }
    if close {
        state.info_window = None;
        state.compress_password.clear();
        state.compress_password2.clear();
    }
}

fn history_popup(resp: &Response, state: &mut ExplorerState) {
    let mut jump: Option<usize> = None;
    Popup::menu(resp).gap(0.0).frame(menu_frame()).show(|ui| {
        ui.set_min_width(280.0);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for (i, p) in state.history.iter().enumerate().rev().take(15) {
            let label = p.to_string_lossy().into_owned();
            if menu_row(
                ui,
                Some(Icon::Folder),
                &label,
                Some(i == state.history_cursor),
                true,
                None,
            )
            .clicked()
            {
                jump = Some(i);
            }
        }
    });
    if let Some(i) = jump {
        state.history_cursor = i;
        let p = state.history[i].clone();
        state.emit(Event::Navigate(p));
    }
}

fn seg_label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into())
}

fn text_w(ui: &Ui, s: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(s.to_owned(), theme::font(), colors::text())
        .size()
        .x
}

fn field_frame(ui: &Ui, rect: Rect, hovered: bool, focused: bool) {
    let border = if focused {
        colors::accent()
    } else if hovered {
        colors::field_hover_border()
    } else {
        colors::field_border()
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, colors::field_bg());
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
}

fn address_bar(ui: &mut Ui, state: &mut ExplorerState, rect: Rect) {
    let ctx = ui.ctx().clone();
    let bg = ui.interact(rect, Id::new("addr_bg"), Sense::click());
    let editing = state.address_editing;
    field_frame(ui, rect, bg.hovered(), editing);

    // right-hand buttons: history drop-down + refresh
    let refresh = Rect::from_min_size(
        pos2(rect.right() - 26.0, rect.top() + 1.0),
        vec2(25.0, rect.height() - 2.0),
    );
    let hist = Rect::from_min_size(
        pos2(refresh.left() - 17.0, rect.top() + 1.0),
        vec2(17.0, rect.height() - 2.0),
    );
    let hist_resp = icon_button(
        ui,
        hist,
        Id::new("addr_hist"),
        Icon::ChevronDown,
        true,
        "Recent locations",
    );
    history_popup(&hist_resp, state);
    let r = icon_button(
        ui,
        refresh,
        Id::new("addr_refresh"),
        Icon::Refresh,
        true,
        "Refresh \"{}\" (F5)",
    );
    if r.clicked() {
        state.dispatch(Command::Refresh);
    }

    let icon_rect =
        Rect::from_center_size(pos2(rect.left() + 14.0, rect.center().y), vec2(16.0, 16.0));
    Icon::Folder.paint(ui.painter(), icon_rect, true);

    if editing {
        let er = Rect::from_min_max(
            pos2(rect.left() + 26.0, rect.top() + 2.0),
            pos2(hist.left() - 2.0, rect.bottom() - 2.0),
        );
        let id = address_edit_id();
        let out = ui
            .scope_builder(UiBuilder::new().max_rect(er), |ui| {
                TextEdit::singleline(&mut state.address_text)
                    .id(id)
                    .frame(Frame::NONE)
                    .font(theme::font())
                    .margin(egui::Margin::symmetric(2, 3))
                    .desired_width(er.width())
                    .show(ui)
            })
            .inner;
        let started: bool = ctx
            .data_mut(|d| d.remove_temp::<bool>(Id::new("addr_just_started")))
            .unwrap_or(false);
        if started {
            out.response.request_focus();
            let mut ts = out.state.clone();
            let n = state.address_text.chars().count();
            ts.cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(n))));
            ts.store(&ctx, id);
            return;
        }
        if out.response.lost_focus() {
            let enter = ctx.input(|i| i.key_pressed(Key::Enter));
            if enter {
                let mut t = state.address_text.trim().to_owned();
                if let Some(rest) = t.strip_prefix('~')
                    && let Some(h) = std::env::var_os("HOME")
                {
                    t = format!("{}{}", h.to_string_lossy(), rest);
                }
                if !t.is_empty() {
                    state.navigate(PathBuf::from(t));
                }
            }
            state.address_editing = false;
        }
        return;
    }

    // ---- breadcrumbs ---------------------------------------------------------------------
    let mut segs: Vec<(String, PathBuf)> = state
        .location
        .ancestors()
        .filter(|a| !a.as_os_str().is_empty())
        .map(|a| (seg_label(a), a.to_path_buf()))
        .collect();
    segs.reverse();
    let limit = hist.left() - 4.0;
    let start_x = rect.left() + 26.0;
    // choose the first visible segment so that the rest fits
    let widths: Vec<f32> = segs
        .iter()
        .map(|(l, _)| text_w(ui, l) + 14.0 + 16.0)
        .collect();
    let mut first = 0;
    let mut total: f32 = widths.iter().sum();
    while first + 1 < segs.len() && start_x + total + if first > 0 { 20.0 } else { 0.0 } > limit {
        total -= widths[first];
        first += 1;
    }
    let mut x = start_x;
    let top = rect.top() + 2.0;
    let h = rect.height() - 4.0;
    let mut go: Option<PathBuf> = None;
    if first > 0 {
        let r = Rect::from_min_size(pos2(x, top), vec2(18.0, h));
        let resp = icon_button(
            ui,
            r,
            Id::new("crumb_overflow"),
            Icon::DoubleChevronLeft,
            true,
            "Show parent folders",
        );
        Popup::menu(&resp).gap(0.0).frame(menu_frame()).show(|ui| {
            ui.set_min_width(180.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            for (l, p) in segs[..first].iter().rev() {
                if menu_row(ui, Some(Icon::Folder), l, None, true, None).clicked() {
                    go = Some(p.clone());
                }
            }
        });
        x += 20.0;
    }
    let mut pending_subs: Option<(usize, Response)> = None;
    for (i, (label, path)) in segs.iter().enumerate().skip(first) {
        let tw = text_w(ui, label);
        let r = Rect::from_min_size(pos2(x, top), vec2(tw + 14.0, h));
        let resp = ui.interact(r, Id::new(("crumb", i)), Sense::click());
        if resp.is_pointer_button_down_on() {
            ui.painter()
                .rect_filled(r, CornerRadius::ZERO, colors::btn_pressed_fill());
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(r, CornerRadius::ZERO, colors::btn_hover_fill());
            ui.painter().rect_stroke(
                r,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::btn_hover_border()),
                StrokeKind::Inside,
            );
        }
        ui.painter().text(
            r.center(),
            Align2::CENTER_CENTER,
            label,
            theme::font(),
            colors::text(),
        );
        if resp.clicked() {
            go = Some(path.clone());
        }
        x += tw + 14.0;
        let cr = Rect::from_min_size(pos2(x, top), vec2(16.0, h));
        let cresp = ui.interact(cr, Id::new(("crumb_chev", i)), Sense::click());
        let open = Popup::is_id_open(&ctx, Popup::default_response_id(&cresp));
        if open || cresp.hovered() {
            let fill = if open || cresp.is_pointer_button_down_on() {
                colors::btn_pressed_fill()
            } else {
                colors::btn_hover_fill()
            };
            ui.painter().rect_filled(cr, CornerRadius::ZERO, fill);
        }
        let ic = if open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        };
        ic.paint(
            ui.painter(),
            Rect::from_center_size(cr.center(), vec2(10.0, 10.0)),
            true,
        );
        if open || cresp.clicked() {
            pending_subs = Some((i, cresp));
        }
        x += 16.0;
    }
    if let Some((i, cresp)) = pending_subs {
        let seg_path = segs[i].1.clone();
        let child = segs.get(i + 1).map(|s| s.1.clone());
        let subs = cached_subfolders(&ctx, state, &seg_path);
        Popup::menu(&cresp).gap(0.0).frame(menu_frame()).show(|ui| {
            ui.set_min_width(180.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            if subs.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new("  (no subfolders)").color(colors::text_disabled()));
                ui.add_space(4.0);
            }
            ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                ui.set_min_width(176.0);
                for sp in &subs {
                    let name = seg_label(sp);
                    let is_child = child.as_ref() == Some(sp);
                    if menu_row(ui, Some(Icon::Folder), &name, Some(is_child), true, None).clicked()
                    {
                        go = Some(sp.clone());
                    }
                }
            });
        });
    }
    if let Some(p) = go {
        state.navigate(p);
    } else if bg.clicked() {
        begin_address_edit(&ctx, state);
    }
}

fn cached_subfolders(ctx: &egui::Context, state: &ExplorerState, path: &Path) -> Vec<PathBuf> {
    let Some(f) = state.subfolders_fn else {
        return Vec::new();
    };
    let id = Id::new(("crumb_subs", path));
    let now = ctx.input(|i| i.time);
    if let Some((t, v)) = ctx.data(|d| d.get_temp::<(f64, Vec<PathBuf>)>(id))
        && now - t < 2.0
    {
        return v;
    }
    let mut v = f(path);
    v.sort_by(|a, b| natural_cmp(&seg_label(a), &seg_label(b)));
    ctx.data_mut(|d| d.insert_temp(id, (now, v.clone())));
    v
}

fn search_box(ui: &mut Ui, state: &mut ExplorerState, rect: Rect) {
    let ctx = ui.ctx().clone();
    let id = search_edit_id();
    let hovered = ui.rect_contains_pointer(rect);
    let focused = ctx.memory(|m| m.has_focus(id));
    field_frame(ui, rect, hovered, focused);
    let er = Rect::from_min_max(
        pos2(rect.left() + 4.0, rect.top() + 2.0),
        pos2(rect.right() - 26.0, rect.bottom() - 2.0),
    );
    let hint = RichText::new(format!("Search {}", state.title)).color(colors::text_secondary());
    let out = ui
        .scope_builder(UiBuilder::new().max_rect(er), |ui| {
            TextEdit::singleline(&mut state.search_text)
                .id(id)
                .frame(Frame::NONE)
                .font(theme::font())
                .hint_text(hint)
                .margin(egui::Margin::symmetric(2, 3))
                .desired_width(er.width())
                .show(ui)
        })
        .inner;
    if out.response.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
        let t = state.search_text.clone();
        state.emit(Event::Search(t));
    }
    let br = Rect::from_min_size(
        pos2(rect.right() - 24.0, rect.top() + 1.0),
        vec2(23.0, rect.height() - 2.0),
    );
    if state.search_text.is_empty() {
        Icon::Search.paint(
            ui.painter(),
            Rect::from_center_size(br.center(), vec2(15.0, 15.0)),
            true,
        );
    } else if icon_button(
        ui,
        br,
        Id::new("search_clear"),
        Icon::Close,
        true,
        "Clear search",
    )
    .clicked()
    {
        state.search_text.clear();
    }
}

// ---------------------------------------------------------------------------
// Navigation pane
// ---------------------------------------------------------------------------

fn nav_pane(ui: &mut Ui, state: &mut ExplorerState) {
    let mut nav = std::mem::take(&mut state.nav);
    let loc = state.location.clone();
    let hover = ui.rect_contains_pointer(ui.max_rect());
    let mut evs: Vec<Event> = Vec::new();
    let width = ui.available_width();
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.add_space(4.0);
            for n in nav.iter_mut() {
                nav_node(ui, n, 0, &loc, hover, width, &mut evs);
            }
        });
    state.nav = nav;
    for e in evs {
        state.emit(e);
    }
}

fn nav_node(
    ui: &mut Ui,
    n: &mut NavNode,
    depth: usize,
    loc: &Path,
    hover: bool,
    width: f32,
    evs: &mut Vec<Event>,
) {
    let (r, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    let cy = r.center().y;
    let indent = 4.0 + depth as f32 * 16.0;
    let id = Id::new(("nav", &n.path, &n.label, depth));
    let resp = ui.interact(r, id, Sense::click());
    let selected = n.path == loc;
    let p = ui.painter().with_clip_rect(r.intersect(ui.clip_rect()));
    let mut drop_hover = false;
    if let Some(pl) = resp.dnd_hover_payload::<Vec<PathBuf>>() {
        drop_hover = !pl.contains(&n.path);
    }
    if let Some(pl) = resp.dnd_release_payload::<Vec<PathBuf>>()
        && !pl.contains(&n.path)
    {
        evs.push(Event::Drop {
            sources: (*pl).clone(),
            target: n.path.clone(),
        });
    }
    if selected {
        p.rect_filled(
            r,
            CornerRadius::ZERO,
            if resp.hovered() {
                colors::item_selected_hover()
            } else {
                colors::item_selected()
            },
        );
        p.rect_stroke(
            r.shrink(0.5),
            CornerRadius::ZERO,
            Stroke::new(1.0, colors::item_selected_border()),
            StrokeKind::Inside,
        );
    } else if resp.hovered() || drop_hover {
        p.rect_filled(r, CornerRadius::ZERO, colors::item_hover());
    }
    let has_children = !n.leaf && n.children.as_ref().is_none_or(|c| !c.is_empty());
    let chev = Rect::from_center_size(pos2(r.left() + indent + 8.0, cy), vec2(16.0, 18.0));
    if has_children {
        let cresp = ui.interact(chev, id.with("chev"), Sense::click());
        if hover || n.expanded {
            let ic = if n.expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            if hover {
                ic.paint(
                    &p,
                    Rect::from_center_size(chev.center(), vec2(9.0, 9.0)),
                    true,
                );
            }
        }
        if cresp.clicked() {
            n.expanded = !n.expanded;
            if n.expanded && n.children.is_none() {
                evs.push(Event::LoadChildren(n.path.clone()));
            }
        }
    }
    let ix = r.left() + indent + 20.0;
    n.icon.paint(
        &p,
        Rect::from_center_size(pos2(ix + 8.0, cy), vec2(16.0, 16.0)),
        true,
    );
    p.text(
        pos2(ix + 22.0, cy),
        Align2::LEFT_CENTER,
        &n.label,
        theme::font(),
        colors::text(),
    );
    if n.pinned {
        Icon::Pin.paint(
            &p,
            Rect::from_center_size(pos2(r.right() - 14.0, cy), vec2(10.0, 10.0)),
            true,
        );
    }
    if resp.clicked() && !drop_hover {
        evs.push(Event::Navigate(n.path.clone()));
    }
    if resp.double_clicked() && has_children {
        n.expanded = !n.expanded;
        if n.expanded && n.children.is_none() {
            evs.push(Event::LoadChildren(n.path.clone()));
        }
    }
    let mut picked: Option<Command> = None;
    context_menu(&resp, |ui| {
        ui.set_min_width(200.0);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        if menu_row(ui, Some(Icon::Open), "Open", None, true, None).clicked() {
            picked = Some(Command::Open);
        }
        if menu_row(
            ui,
            Some(Icon::NewWindow),
            "Open in new window",
            None,
            true,
            None,
        )
        .clicked()
        {
            picked = Some(Command::OpenNewWindow);
        }
        let pin = if n.pinned {
            "Unpin from Quick access"
        } else {
            "Pin to Quick access"
        };
        if menu_row(ui, Some(Icon::Pin), pin, None, true, None).clicked() {
            picked = Some(Command::PinToQuickAccess);
        }
        menu_separator(ui);
        if menu_row(ui, Some(Icon::Properties), "Properties", None, true, None).clicked() {
            picked = Some(Command::Properties);
        }
        if picked.is_some() {
            ui.close();
        }
    });
    match picked {
        Some(Command::Open) => evs.push(Event::Navigate(n.path.clone())),
        Some(cmd) => evs.push(Event::Command {
            cmd,
            targets: vec![n.path.clone()],
        }),
        None => {}
    }
    if n.expanded
        && let Some(ch) = n.children.as_mut()
    {
        for c in ch.iter_mut() {
            nav_node(ui, c, depth + 1, loc, hover, width, evs);
        }
    }
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn status_bar(ui: &mut Ui, state: &mut ExplorerState, sel: SelSummary) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), STATUS_H), Sense::hover());
    let cy = r.center().y;
    let font = theme::font_small();
    let ui_p = ui.painter().clone();
    if let Some(message) = state.host_status.as_deref() {
        // Reserve room for the view switchers on the right and clip long paths or
        // audit summaries instead of allowing them to overlap the controls.
        let clip = Rect::from_min_max(r.min, pos2(r.right() - 64.0, r.bottom()));
        let status_painter = ui_p.with_clip_rect(clip);
        let g = status_painter.layout_no_wrap(
            message.to_owned(),
            font.clone(),
            colors::text_secondary(),
        );
        status_painter.galley(
            pos2(r.left() + 10.0, cy - g.size().y / 2.0),
            g,
            colors::text_secondary(),
        );
    } else {
        let n = state.visible_count();
        let mut text = format!("{} item{}", n, if n == 1 { "" } else { "s" });
        let g = ui_p.layout_no_wrap(text.clone(), font.clone(), colors::text());
        ui_p.galley(
            pos2(r.left() + 10.0, cy - g.size().y / 2.0),
            g.clone(),
            colors::text(),
        );
        let x = r.left() + 10.0 + g.size().x + 14.0;
        if sel.count > 0 {
            ui_p.line_segment(
                [pos2(x - 7.0, cy - 7.0), pos2(x - 7.0, cy + 7.0)],
                Stroke::new(1.0, colors::separator()),
            );
            text = format!(
                "{} item{} selected",
                sel.count,
                if sel.count == 1 { "" } else { "s" }
            );
            if sel.file_count > 0 {
                text.push_str(&format!("   {}", format_size(sel.file_bytes)));
            }
            ui_p.text(
                pos2(x, cy),
                Align2::LEFT_CENTER,
                &text,
                font,
                colors::text(),
            );
        }
    }
    // view switchers on the right
    let mut bx = r.right() - 8.0;
    for (v, tip) in [
        (ViewMode::LargeIcons, "Large icons (Ctrl+Shift+2)"),
        (ViewMode::Details, "Details (Ctrl+Shift+6)"),
    ] {
        let b = Rect::from_min_size(pos2(bx - 22.0, cy - 10.0), vec2(22.0, 20.0));
        let resp = ui.interact(b, Id::new(("status_view", tip)), Sense::click());
        let on = state.view == v;
        if on || resp.hovered() {
            let fill = if on {
                colors::btn_checked_fill()
            } else {
                colors::btn_hover_fill()
            };
            let border = if on {
                colors::btn_checked_border()
            } else {
                colors::btn_hover_border()
            };
            ui_p.rect_filled(b, CornerRadius::ZERO, fill);
            ui_p.rect_stroke(
                b,
                CornerRadius::ZERO,
                Stroke::new(1.0, border),
                StrokeKind::Inside,
            );
        }
        v.icon().paint(
            &ui_p,
            Rect::from_center_size(b.center(), vec2(14.0, 14.0)),
            true,
        );
        if resp.on_hover_text(tip).clicked() {
            state.apply_local(Command::SetView(v));
        }
        bx -= 24.0;
    }
}

// ---------------------------------------------------------------------------
// Preview / details pane
// ---------------------------------------------------------------------------

fn kv(ui: &mut Ui, k: &str, v: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(k).small().color(colors::text_secondary()));
    ui.label(RichText::new(v).color(colors::text()));
}

fn side_pane(ui: &mut Ui, state: &ExplorerState, sel: SelSummary) {
    // Only the "exactly one selected" case needs the entry itself; the empty
    // and multi-selection cases are fully described by the summary.
    let single = sel.single(&state.entries);
    let preview = state.show_preview_pane;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if preview {
                match (sel.count, single) {
                    (0, _) => {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Select a file to preview.")
                                    .color(colors::text_secondary()),
                            );
                        });
                    }
                    (_, Some(e)) if e.kind == EntryKind::File || e.thumbnail.is_some() => {
                        let side = ui.available_width().min(320.0);
                        let (r, _) = ui
                            .allocate_exact_size(vec2(ui.available_width(), side), Sense::hover());
                        let box_r = Rect::from_center_size(r.center(), vec2(side, side));
                        if let Some(tex) = e.thumbnail {
                            ui.painter().image(
                                tex,
                                box_r.shrink(8.0),
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else {
                            e.icon().paint(ui.painter(), box_r.shrink(side * 0.2), true);
                        }
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("No preview available.")
                                    .color(colors::text_secondary()),
                            );
                        });
                    }
                    _ => {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("No preview available.")
                                    .color(colors::text_secondary()),
                            );
                        });
                    }
                }
                return;
            }
            // details pane
            match (sel.count, single) {
                (0, _) => {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 96.0), Sense::hover());
                    Icon::Folder.paint(
                        ui.painter(),
                        Rect::from_center_size(r.center(), vec2(80.0, 80.0)),
                        true,
                    );
                    ui.label(RichText::new(&state.title).size(18.0));
                    ui.label(RichText::new("File folder").color(colors::text_secondary()));
                    let n = state.visible_count();
                    kv(ui, "Items", &n.to_string());
                    kv(ui, "Location", &state.location.to_string_lossy());
                }
                (_, Some(e)) => {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 112.0), Sense::hover());
                    if let Some(tex) = e.thumbnail {
                        ui.painter().image(
                            tex,
                            Rect::from_center_size(r.center(), vec2(104.0, 104.0)),
                            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else {
                        e.icon().paint(
                            ui.painter(),
                            Rect::from_center_size(r.center(), vec2(96.0, 96.0)),
                            true,
                        );
                    }
                    ui.label(RichText::new(&e.name).size(18.0));
                    ui.label(RichText::new(&e.type_name).color(colors::text_secondary()));
                    if e.kind == EntryKind::File {
                        kv(ui, "Size", &format_size(e.size));
                    }
                    if let Some(t) = e.modified {
                        kv(ui, "Date modified", &(state.date_format)(t));
                    }
                    if let Some(parent) = e.path.parent() {
                        kv(ui, "Folder path", &parent.to_string_lossy());
                    }
                    if let Some((total, free)) = e.space {
                        kv(ui, "Space used", &format_size(total.saturating_sub(free)));
                        kv(ui, "Space free", &format_size(free));
                        kv(ui, "Total size", &format_size(total));
                    }
                }
                _ => {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 96.0), Sense::hover());
                    Icon::File.paint(
                        ui.painter(),
                        Rect::from_center_size(r.center() + vec2(10.0, -6.0), vec2(64.0, 64.0)),
                        true,
                    );
                    Icon::File.paint(
                        ui.painter(),
                        Rect::from_center_size(r.center() - vec2(10.0, 0.0), vec2(64.0, 64.0)),
                        true,
                    );
                    ui.label(RichText::new(format!("{} items selected", sel.count)).size(18.0));
                    kv(ui, "Total size", &format_size(sel.file_bytes));
                }
            }
        });
}
