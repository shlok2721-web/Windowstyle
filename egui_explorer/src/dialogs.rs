//! Shared chrome for the small floating dialogs (Properties, Compress, History, Help, ...).
//!
//! Every dialog goes through [`show`], so they all get the same frame, title bar and shadow,
//! are centred over the application window, and behave as proper modals: a dimmed backdrop
//! sits behind them blocking the rest of the window, clicking that backdrop (or pressing
//! Escape) dismisses the dialog, and pressing Enter triggers its primary action.

use egui::epaint::StrokeKind;
use egui::{
    Align, Align2, Color32, Context, CornerRadius, Frame, Id, Key, Label, Layout, Margin, Modal,
    Rect, Response, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};

use crate::icons::Icon;
use crate::theme::{self, colors};

const TITLE_H: f32 = 30.0;
const CLOSE_W: f32 = 44.0;
const FOOTER_H: f32 = 46.0;
const BODY_MARGIN_X: i8 = 16;
const BODY_MARGIN_Y: i8 = 12;
const BUTTON_MIN_W: f32 = 84.0;
const BUTTON_H: f32 = 26.0;

/// Width of the label column in [`kv_row`].
pub const LABEL_W: f32 = 104.0;

/// What happened to a dialog shown with [`show`] this frame.
pub struct DialogOutcome {
    /// The title-bar close button was clicked, the dimmed backdrop behind the dialog was
    /// clicked, or Escape was pressed. Callers should treat this exactly like a click on the
    /// dialog's own safe, non-destructive button (its "Cancel" or "Close").
    pub dismissed: bool,
    /// Enter was pressed while this was the topmost dialog. Callers should treat this
    /// exactly like a click on whichever footer button was passed `primary: true`.
    pub confirmed: bool,
}

/// Show a dialog `width` points wide, as a true modal: a dimmed backdrop blocks and darkens
/// the rest of the window behind it so nothing back there can be clicked by accident, and the
/// backdrop itself (as well as Escape) dismisses the dialog without the user needing to find
/// the close button. `content` draws the body (use [`body`], [`footer`] and friends).
///
/// The dialog is centred on the application window. Use a fresh `id` for every opening (see
/// [`session_id`]) so a freshly-reopened dialog always starts in the same state.
pub fn show(
    ctx: &Context,
    id: Id,
    title: &str,
    width: f32,
    content: impl FnOnce(&mut Ui),
) -> DialogOutcome {
    let frame = Frame::NONE
        .fill(colors::panel_bg())
        .stroke(Stroke::new(1.0, colors::field_border()))
        .shadow(egui::epaint::Shadow {
            offset: [3, 5],
            blur: 14,
            spread: 0,
            color: Color32::from_black_alpha(if theme::dark_mode() { 130 } else { 70 }),
        });
    // Read this before `show`, which is where the dialog (and anything it contains, such as a
    // text field) consumes this frame's input.
    let enter_pressed = ctx.input(|i| i.key_pressed(Key::Enter));
    // A focused button is activated by Enter itself (egui `clicked()`), so `confirmed` must not
    // fire as well. Buttons set this flag while drawing; it is read (and reset) once per frame.
    let pass = ctx.cumulative_pass_nr();
    let vp = ctx.viewport_id();
    let button_was_focused = ctx.data_mut(|d| {
        d.insert_temp(Id::new(("dialog_last_shown_pass", vp)), pass);
        let f = d
            .get_temp::<bool>(Id::new(("dialog_button_focused", vp)))
            .unwrap_or(false);
        d.insert_temp(Id::new(("dialog_button_focused", vp)), false);
        f
    });
    let mut own_close = false;
    // `Modal::default_area` already centres the dialog (and, since an anchored area can't be
    // dragged, keeps it there), so no extra positioning is needed here.
    let response = Modal::new(id)
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(if theme::dark_mode() {
            120
        } else {
            90
        }))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.set_width(width);
            if title_bar(ui, title) {
                own_close = true;
            }
            content(ui);
        });
    DialogOutcome {
        dismissed: own_close || response.should_close(),
        confirmed: enter_pressed && response.is_top_modal && !button_was_focused,
    }
}

/// `true` while any modal dialog is on screen. The file list and the global shortcuts must not
/// react to the keyboard then: otherwise Enter would open the selected folder (and Delete,
/// Backspace, ... would run) *behind* the dialog instead of reaching its buttons.
pub fn modal_open(ctx: &Context) -> bool {
    // `show` stamps the pass it ran in. The explorer is drawn before the dialogs, so a dialog
    // shown last pass (or this one) counts as open. This also covers the first frame, where
    // egui's own `top_modal_layer` is still unset.
    let now = ctx.cumulative_pass_nr();
    let key = Id::new(("dialog_last_shown_pass", ctx.viewport_id()));
    ctx.data(|d| d.get_temp::<u64>(key))
        .is_some_and(|shown| now.saturating_sub(shown) <= 1)
}

/// Id for a dialog shown while `active`. It changes every time the dialog is opened again, so
/// a freshly-reopened dialog is centred and does not carry over any leftover per-id state.
/// Call it every frame (also while the dialog is closed) so the closed state is noticed.
pub fn session_id(ctx: &Context, name: &'static str, active: bool) -> Id {
    // Per window: two windows in one process must not bump each other's serial.
    let vp = ctx.viewport_id();
    let key = Id::new(("dialog_session", name, vp));
    let serial = ctx.data_mut(|d| {
        let (was_active, serial): &mut (bool, u32) = d.get_temp_mut_or_default(key);
        if active && !*was_active {
            *serial = serial.wrapping_add(1);
        }
        *was_active = active;
        *serial
    });
    Id::new((name, serial, vp))
}

/// Paints the title strip and returns `true` when the close button was clicked.
fn title_bar(ui: &mut Ui, title: &str) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, TITLE_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, colors::panel_bg());
    painter.text(
        pos2(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        theme::font(),
        colors::text(),
    );

    let btn = Rect::from_min_size(
        pos2(rect.right() - CLOSE_W, rect.top()),
        vec2(CLOSE_W, TITLE_H),
    );
    let resp = ui.interact(btn, ui.id().with("dialog_close"), Sense::click());
    let down = resp.is_pointer_button_down_on();
    let painter = ui.painter();
    let glyph = if resp.hovered() || down {
        painter.rect_filled(
            btn,
            CornerRadius::ZERO,
            if down {
                colors::close_pressed()
            } else {
                colors::close_hover()
            },
        );
        Color32::WHITE
    } else {
        colors::text()
    };
    let c = btn.center();
    let stroke = Stroke::new(1.2, glyph);
    painter.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], stroke);
    painter.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], stroke);
    resp.clicked()
}

/// Padded content area. Always as wide as the dialog so the footer lines up with it.
pub fn body<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::NONE
        .fill(colors::dialog_body_bg())
        .inner_margin(Margin::symmetric(BODY_MARGIN_X, BODY_MARGIN_Y))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.set_min_width(w);
            ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
            add(ui)
        })
        .inner
}

/// Bottom button bar. Buttons are laid out right to left: add the primary action first.
pub fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, FOOTER_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, colors::ribbon_bg());
    painter.line_segment(
        [rect.left_top(), rect.right_top()],
        Stroke::new(1.0, colors::ribbon_border()),
    );
    // The strip is already allocated above, so lay the buttons out in a child `Ui` that does
    // not allocate anything again.
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(12.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 8.0;
    add(&mut row);
}

/// A dialog push button. `primary` draws the accent border of a default button.
pub fn button(ui: &mut Ui, label: &str, primary: bool) -> Response {
    // Wide enough for the label ("Keep both (rename)"), never narrower than the standard size.
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::font(), Color32::PLACEHOLDER)
        .size()
        .x;
    let size = vec2((text_w + 28.0).max(BUTTON_MIN_W), BUTTON_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let down = resp.is_pointer_button_down_on();
    let focused = resp.has_focus();
    if focused {
        let vp = ui.ctx().viewport_id();
        ui.data_mut(|d| d.insert_temp(Id::new(("dialog_button_focused", vp)), true));
    }
    let (fill, border, width) = if down {
        (colors::btn_pressed_fill(), colors::btn_pressed_border(), 1.0)
    } else if resp.hovered() {
        (colors::btn_hover_fill(), colors::btn_hover_border(), 1.0)
    } else if primary || focused {
        (
            ui.visuals().widgets.inactive.bg_fill,
            colors::accent(),
            2.0,
        )
    } else {
        (
            ui.visuals().widgets.inactive.bg_fill,
            colors::field_border(),
            1.0,
        )
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::ZERO, fill);
    painter.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(width, border),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme::font(),
        colors::text(),
    );
    if primary {
        // Give the default button keyboard focus on the dialog's first frame, so Left/Right/Tab
        // can move from it to the other buttons. The id is fresh for every opening. A text
        // field that already took focus this frame is left alone.
        let key = resp.id.with("autofocus_done");
        let done = ui.data_mut(|d| d.get_temp::<bool>(key).unwrap_or(false));
        if !done {
            ui.data_mut(|d| d.insert_temp(key, true));
            if ui.memory(|m| m.focused().is_none()) {
                resp.request_focus();
            }
        }
    }
    resp
}

/// Icon + big name + secondary line, as at the top of a Windows properties sheet.
pub fn header(ui: &mut Ui, icon: Icon, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
        icon.paint(ui.painter(), r, true);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                Label::new(RichText::new(title).size(15.0).color(colors::text()))
                    .wrap()
                    .selectable(true),
            );
            ui.label(RichText::new(subtitle).color(colors::text_secondary()));
        });
    });
}

/// Section caption followed by a hairline that fills the rest of the row.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(title).color(colors::accent()));
        let r = ui.available_rect_before_wrap();
        ui.painter().line_segment(
            [pos2(r.left(), r.center().y), pos2(r.right(), r.center().y)],
            Stroke::new(1.0, colors::separator()),
        );
    });
    ui.add_space(2.0);
}

/// A full-width bordered box that visually groups related controls.
pub fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::NONE
        .fill(colors::ribbon_bg())
        .stroke(Stroke::new(1.0, colors::ribbon_border()))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.set_min_width(w);
            ui.spacing_mut().item_spacing = vec2(8.0, 4.0);
            add(ui)
        })
        .inner
}

/// `label: value` row with aligned columns. Long values wrap instead of widening the dialog.
pub fn kv_row(ui: &mut Ui, label_w: f32, label: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.allocate_ui(vec2(label_w, 0.0), |ui| {
            ui.set_min_width(label_w);
            ui.label(RichText::new(label).color(colors::text_secondary()));
        });
        ui.add(
            Label::new(RichText::new(value).color(colors::text()))
                .wrap()
                .selectable(true),
        );
    });
}
