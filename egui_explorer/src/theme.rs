//! Windows 10 File Explorer colour palette and egui style.

use std::sync::atomic::{AtomicBool, Ordering};

use egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals, vec2,
};

/// Palette sampled from the Windows 10 (build 19041+) light theme.
pub mod colors {
    use egui::Color32;

    pub fn window_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(0, 0, 0)
        } else {
            Color32::from_rgb(244, 243, 239)
        }
    }
    /// Navigation pane background. Kept at the previous dark grey while the rest of the
    /// dark interface is pure black.
    pub fn nav_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(10, 10, 10)
        } else {
            Color32::from_rgb(239, 239, 234)
        }
    }
    /// Title bar, quick-access toolbar, status bar and the ribbon tab strip.
    pub fn chrome_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(8, 8, 8)
        } else {
            Color32::from_rgb(245, 244, 240)
        }
    }
    /// Preview/details pane and dialog surfaces.
    pub fn panel_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(13, 13, 13)
        } else {
            Color32::from_rgb(240, 240, 236)
        }
    }
    /// Navigation / address row (breadcrumbs and navigation buttons).
    pub fn navrow_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(9, 9, 9)
        } else {
            Color32::from_rgb(240, 240, 236)
        }
    }
    /// Quick-access toolbar.
    pub fn qat_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(8, 8, 8)
        } else {
            Color32::from_rgb(240, 240, 236)
        }
    }
    /// File-list column headers.
    pub fn header_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(0, 0, 0)
        } else {
            Color32::from_rgb(240, 240, 236)
        }
    }
    /// Content area inside dialogs (the dialog frame and title bar use `panel_bg`).
    pub fn dialog_body_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(13, 13, 13)
        } else {
            Color32::from_rgb(247, 246, 242)
        }
    }
    pub fn ribbon_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(8, 8, 8)
        } else {
            Color32::from_rgb(238, 238, 234)
        }
    }
    pub fn ribbon_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(32, 32, 32)
        } else {
            Color32::from_rgb(212, 212, 205)
        }
    }
    pub fn tab_selected_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(8, 8, 8)
        } else {
            Color32::from_rgb(238, 238, 234)
        }
    }
    pub fn tab_hover_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(18, 18, 18)
        } else {
            Color32::from_rgb(233, 233, 228)
        }
    }
    pub fn file_tab() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(0, 120, 215)
        } else {
            Color32::from_rgb(25, 121, 202)
        }
    }
    pub fn file_tab_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(35, 143, 225)
        } else {
            Color32::from_rgb(62, 146, 218)
        }
    }
    pub fn file_tab_pressed() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(0, 90, 165)
        } else {
            Color32::from_rgb(18, 98, 166)
        }
    }

    pub fn text() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(232, 232, 232)
        } else {
            Color32::from_rgb(37, 37, 37)
        }
    }
    pub fn text_disabled() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(120, 120, 120)
        } else {
            Color32::from_rgb(150, 150, 143)
        }
    }
    pub fn text_secondary() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(175, 175, 175)
        } else {
            Color32::from_rgb(102, 102, 96)
        }
    }
    pub fn accent() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(76, 171, 255)
        } else {
            Color32::from_rgb(0, 120, 215)
        }
    }

    pub fn btn_hover_fill() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(24, 24, 24)
        } else {
            Color32::from_rgb(231, 231, 226)
        }
    }
    pub fn btn_hover_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(52, 78, 100)
        } else {
            Color32::from_rgb(196, 205, 211)
        }
    }
    pub fn btn_pressed_fill() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(16, 38, 56)
        } else {
            Color32::from_rgb(211, 227, 238)
        }
    }
    pub fn btn_pressed_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(40, 84, 116)
        } else {
            Color32::from_rgb(153, 184, 205)
        }
    }
    pub fn btn_checked_fill() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(16, 38, 56)
        } else {
            Color32::from_rgb(222, 234, 242)
        }
    }
    pub fn btn_checked_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(40, 84, 116)
        } else {
            Color32::from_rgb(153, 184, 205)
        }
    }

    pub fn item_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(20, 20, 20)
        } else {
            Color32::from_rgb(232, 232, 227)
        }
    }
    pub fn item_selected() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(16, 38, 56)
        } else {
            Color32::from_rgb(222, 234, 242)
        }
    }
    pub fn item_selected_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(40, 84, 116)
        } else {
            Color32::from_rgb(153, 184, 205)
        }
    }
    pub fn item_selected_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(22, 52, 77)
        } else {
            Color32::from_rgb(211, 227, 238)
        }
    }
    pub fn item_selected_inactive() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(36, 36, 36)
        } else {
            Color32::from_rgb(232, 232, 228)
        }
    }

    pub fn separator() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(26, 26, 26)
        } else {
            Color32::from_rgb(218, 218, 212)
        }
    }
    pub fn field_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(48, 48, 48)
        } else {
            Color32::from_rgb(212, 212, 205)
        }
    }
    pub fn field_bg() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(7, 7, 7)
        } else {
            Color32::from_rgb(248, 247, 243)
        }
    }
    pub fn field_hover_border() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(76, 171, 255)
        } else {
            Color32::from_rgb(126, 180, 234)
        }
    }

    pub fn header_text() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(160, 185, 210)
        } else {
            Color32::from_rgb(76, 96, 122)
        }
    }
    pub fn header_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(28, 28, 28)
        } else {
            Color32::from_rgb(231, 231, 226)
        }
    }
    pub fn group_text() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(150, 180, 255)
        } else {
            Color32::from_rgb(30, 50, 135)
        }
    }

    pub fn close_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(190, 25, 40)
        } else {
            Color32::from_rgb(232, 17, 35)
        }
    }
    pub fn close_pressed() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(150, 20, 35)
        } else {
            Color32::from_rgb(241, 112, 122)
        }
    }
    pub fn caption_hover() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(24, 24, 24)
        } else {
            Color32::from_rgb(231, 231, 226)
        }
    }
    pub fn caption_pressed() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(43, 43, 43)
        } else {
            Color32::from_rgb(217, 217, 210)
        }
    }

    /// Colours of the contextual tab headers (Picture Tools, Drive Tools, ...).
    pub fn ctx_picture() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(180, 110, 180)
        } else {
            Color32::from_rgb(160, 80, 160)
        }
    }
    pub fn ctx_drive() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(55, 170, 110)
        } else {
            Color32::from_rgb(40, 150, 90)
        }
    }
    pub fn ctx_app() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(225, 115, 65)
        } else {
            Color32::from_rgb(210, 90, 40)
        }
    }
    pub fn ctx_archive() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(200, 165, 45)
        } else {
            Color32::from_rgb(190, 150, 20)
        }
    }
    pub fn ctx_media() -> Color32 {
        if super::dark_mode() {
            Color32::from_rgb(65, 145, 210)
        } else {
            Color32::from_rgb(40, 120, 190)
        }
    }
}

/// Base font size (Segoe UI 9pt == 12px; egui's default face renders a little
/// smaller so we go one step up).
pub const BASE_FONT: f32 = 13.0;

static DARK_MODE: AtomicBool = AtomicBool::new(false);

/// Whether the custom-painted parts of the explorer are currently using the dark palette.
pub fn dark_mode() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

/// Apply the light Windows 10 look. Kept for existing callers.
pub fn apply(ctx: &Context) {
    apply_mode(ctx, false);
}

/// Apply either the light or dark interface palette. Only UI colours change; file content,
/// thumbnails and source files are left untouched.
pub fn apply_mode(ctx: &Context, dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
    let mut style = (*ctx.global_style()).clone();
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    visuals.panel_fill = colors::window_bg();
    visuals.window_fill = colors::window_bg();
    visuals.extreme_bg_color = if dark {
        Color32::from_rgb(8, 8, 8)
    } else {
        Color32::from_rgb(248, 247, 243)
    };
    visuals.faint_bg_color = if dark {
        Color32::from_rgb(10, 10, 10)
    } else {
        Color32::from_rgb(240, 240, 236)
    };
    visuals.window_corner_radius = CornerRadius::ZERO;
    visuals.menu_corner_radius = CornerRadius::ZERO;
    visuals.window_stroke = Stroke::new(1.0, colors::ribbon_border());
    visuals.selection.bg_fill = colors::item_selected();
    visuals.selection.stroke = Stroke::new(1.0, colors::item_selected_border());
    visuals.hyperlink_color = colors::accent();
    visuals.popup_shadow.color = Color32::from_black_alpha(if dark { 110 } else { 40 });

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, colors::separator());
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, colors::text());
    widgets.noninteractive.corner_radius = CornerRadius::ZERO;
    widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    widgets.inactive.bg_fill = if dark {
        Color32::from_rgb(24, 24, 24)
    } else {
        Color32::from_rgb(228, 228, 223)
    };
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.inactive.fg_stroke = Stroke::new(1.0, colors::text());
    widgets.inactive.corner_radius = CornerRadius::ZERO;
    widgets.hovered.weak_bg_fill = colors::btn_hover_fill();
    widgets.hovered.bg_fill = colors::btn_hover_fill();
    widgets.hovered.bg_stroke = Stroke::new(1.0, colors::btn_hover_border());
    widgets.hovered.fg_stroke = Stroke::new(1.0, colors::text());
    widgets.hovered.corner_radius = CornerRadius::ZERO;
    widgets.active.weak_bg_fill = colors::btn_pressed_fill();
    widgets.active.bg_fill = colors::btn_pressed_fill();
    widgets.active.bg_stroke = Stroke::new(1.0, colors::btn_pressed_border());
    widgets.active.fg_stroke = Stroke::new(1.0, colors::text());
    widgets.active.corner_radius = CornerRadius::ZERO;
    widgets.open.weak_bg_fill = colors::btn_pressed_fill();
    widgets.open.bg_fill = colors::btn_pressed_fill();
    widgets.open.bg_stroke = Stroke::new(1.0, colors::btn_pressed_border());
    widgets.open.fg_stroke = Stroke::new(1.0, colors::text());
    widgets.open.corner_radius = CornerRadius::ZERO;

    style.visuals = visuals;
    style.spacing.item_spacing = vec2(4.0, 2.0);
    style.spacing.button_padding = vec2(6.0, 2.0);
    style.spacing.menu_margin = Margin::symmetric(2, 2);
    style.spacing.interact_size.y = 20.0;
    style.spacing.scroll.bar_width = 10.0;
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(BASE_FONT, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(BASE_FONT, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(18.0, FontFamily::Proportional),
    );
    ctx.set_global_style(style);
}

/// Shorthand for the default UI font.
pub fn font() -> FontId {
    FontId::new(BASE_FONT, FontFamily::Proportional)
}

/// Shorthand for the small UI font (group captions, status bar).
pub fn font_small() -> FontId {
    FontId::new(12.0, FontFamily::Proportional)
}
