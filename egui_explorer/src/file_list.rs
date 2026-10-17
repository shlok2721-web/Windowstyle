//! The item pane: Details, List, Icons (4 sizes), Tiles and Content views with
//! grouping, rubber-band selection, inline rename, keyboard navigation,
//! type-ahead find, drag & drop and context menus.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use egui::epaint::StrokeKind;
use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Galley, Id, Key, LayerId, Margin,
    Modifiers, Order, Painter, PointerButton, Rect, ScrollArea, Sense, Stroke, TextEdit, Ui,
    UiBuilder, Vec2, pos2, vec2,
};

use crate::icons::Icon;
use crate::model::*;
use crate::ribbon::{MenuEntry, context_menu, menu_row, render_menu};
use crate::theme::{self, colors};

const ROW_H: f32 = 22.0;
const CONTENT_ROW_H: f32 = 54.0;
const HEADER_H: f32 = 24.0;
const GROUP_H: f32 = 30.0;

// ---------------------------------------------------------------------------
// Deferred actions (the render pass only reads `ExplorerState`)
// ---------------------------------------------------------------------------

enum Act {
    Select {
        sel: HashSet<PathBuf>,
        focus: Option<PathBuf>,
        anchor: Option<PathBuf>,
    },
    ClearSel,
    Open(PathBuf, bool),
    Cmd(Command),
    Rename(PathBuf, String),
    Drop(Vec<PathBuf>, PathBuf),
    ToggleGroup(String),
    SetSort(SortKey),
    SetColumnWidth(usize, f32),
    ResetColumnWidth(usize),
    ToggleColumn(usize),
    ThumbnailNeeded(PathBuf),
}

fn apply(state: &mut ExplorerState, acts: Vec<Act>) {
    for a in acts {
        match a {
            Act::Select { sel, focus, anchor } => {
                state.selection = sel;
                if focus.is_some() {
                    state.focus = focus;
                }
                if anchor.is_some() {
                    state.anchor = anchor;
                }
            }
            Act::ClearSel => state.selection.clear(),
            Act::Open(p, is_dir) => {
                if is_dir {
                    state.navigate(p);
                } else {
                    state.emit(Event::Open(p));
                }
            }
            Act::Cmd(c) => state.dispatch(c),
            Act::Rename(path, new_name) => state.emit(Event::Rename { path, new_name }),
            Act::Drop(sources, target) => state.emit(Event::Drop { sources, target }),
            Act::ThumbnailNeeded(path) => state.emit(Event::ThumbnailNeeded(path)),
            Act::ToggleGroup(l) => {
                if !state.collapsed_groups.remove(&l) {
                    state.collapsed_groups.insert(l);
                }
            }
            Act::SetSort(k) => {
                if state.sort_key == k {
                    state.sort_order = match state.sort_order {
                        SortOrder::Ascending => SortOrder::Descending,
                        SortOrder::Descending => SortOrder::Ascending,
                    };
                } else {
                    state.sort_key = k;
                    state.sort_order = SortOrder::Ascending;
                }
            }
            Act::SetColumnWidth(i, w) => {
                if let Some(c) = state.columns.get_mut(i) {
                    c.width = w.max(40.0);
                }
            }
            Act::ResetColumnWidth(i) => {
                if let Some(c) = state.columns.get_mut(i) {
                    c.width = match c.key {
                        SortKey::Name => 260.0,
                        SortKey::DateModified => 140.0,
                        SortKey::Type => 120.0,
                        SortKey::Size => 80.0,
                    };
                }
            }
            Act::ToggleColumn(i) => {
                if let Some(c) = state.columns.get_mut(i)
                    && c.key != SortKey::Name
                {
                    c.visible = !c.visible;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Where one group (or the whole list, when ungrouped) sits in the layout.
///
/// The layout is described by these few records plus the view's geometry, not
/// by one record per item: an item's rectangle is computed on demand from its
/// flat index, so building a layout costs O(groups) and only the items that are
/// actually on screen are ever touched.
struct Section {
    /// Index into `GroupSet::groups` (header text, collapse key).
    gi: usize,
    /// Offset of the first shown item inside `GroupSet::flat`.
    off: usize,
    /// Items shown (0 while the group is collapsed).
    len: usize,
    /// Flat layout index of the first shown item (the `k` used everywhere).
    start: usize,
    collapsed: bool,
    /// Top of the group header (equal to `y0` when ungrouped).
    hdr_y: f32,
    /// Top of the item area, just below the header.
    y0: f32,
    /// Bottom of the section; the next section's `hdr_y`.
    y_end: f32,
}

/// Per-view geometry used to turn a flat item index into a rectangle.
#[derive(Clone, Copy)]
enum Geo {
    /// Details / Content: one full-width row per item.
    Rows { row_h: f32, width: f32, hdr_w: f32 },
    /// List: column-major, `per_col` rows per column.
    List { col_w: f32, per_col: usize },
    /// Icon views and Tiles: row-major grid of `cols` cells per row.
    Grid { cell: Vec2, cols: usize, hdr_w: f32 },
}

struct Layout<'a> {
    set: &'a GroupSet,
    geo: Geo,
    sections: Vec<Section>,
    /// Number of sections that show at least one item.
    live: usize,
    /// Number of shown items across all sections.
    total: usize,
    size: Vec2,
}

impl<'a> Layout<'a> {
    fn len(&self) -> usize {
        self.total
    }

    fn grouped(&self) -> bool {
        self.set.grouped()
    }

    /// True when at least one group header is laid out.
    fn has_headers(&self) -> bool {
        self.grouped() && !self.sections.is_empty()
    }

    /// The section that owns flat index `k` (`k < len()`).
    fn section_of(&self, k: usize) -> usize {
        // Empty (collapsed) sections share `start` with their successor, so
        // taking the *last* section whose start is <= k lands on the owner.
        self.sections
            .partition_point(|s| s.start <= k)
            .saturating_sub(1)
    }

    /// Index into `entries` of the item at flat index `k`.
    fn entry_idx(&self, k: usize) -> usize {
        let s = &self.sections[self.section_of(k)];
        self.set.flat[s.off + (k - s.start)]
    }

    fn rect_in(&self, s: &Section, n: usize) -> Rect {
        match self.geo {
            Geo::Rows { row_h, width, .. } => {
                Rect::from_min_size(pos2(0.0, s.y0 + n as f32 * row_h), vec2(width, row_h))
            }
            Geo::List { col_w, per_col } => {
                let (c, r) = (n / per_col, n % per_col);
                Rect::from_min_size(
                    pos2(4.0 + c as f32 * col_w, 2.0 + r as f32 * ROW_H),
                    vec2(col_w, ROW_H),
                )
            }
            Geo::Grid { cell, cols, .. } => {
                let (c, r) = (n % cols, n / cols);
                Rect::from_min_size(
                    pos2(4.0 + c as f32 * cell.x, s.y0 + 4.0 + r as f32 * cell.y),
                    cell,
                )
            }
        }
    }

    /// Rectangle (layout coordinates) of the item at flat index `k`.
    fn rect(&self, k: usize) -> Rect {
        let s = &self.sections[self.section_of(k)];
        self.rect_in(s, k - s.start)
    }

    /// Header rectangle of a section (grouped layouts only).
    fn header_rect(&self, s: &Section) -> Rect {
        let w = match self.geo {
            Geo::Rows { hdr_w, .. } | Geo::Grid { hdr_w, .. } => hdr_w,
            Geo::List { col_w, .. } => col_w,
        };
        Rect::from_min_size(pos2(0.0, s.hdr_y), vec2(w, GROUP_H))
    }

    /// Call `f(k, entry_idx, rect)` for every item whose rectangle intersects
    /// `area`, in display order. Work is proportional to the number of items
    /// in (or just around) `area` plus O(log groups) to find where to start,
    /// never to the folder size.
    fn visit_in(&self, area: Rect, f: &mut dyn FnMut(usize, usize, Rect)) {
        let mut emit = |s: &Section, lo: usize, hi: usize| {
            for n in lo..hi.min(s.len) {
                let r = self.rect_in(s, n);
                if r.intersects(area) {
                    f(s.start + n, self.set.flat[s.off + n], r);
                }
            }
        };
        let floor_idx = |v: f32| -> usize { v.floor().max(0.0) as usize };
        match self.geo {
            Geo::List { col_w, per_col } => {
                // Single section, column-major: whole columns span the width.
                let Some(s) = self.sections.first() else {
                    return;
                };
                let c0 = floor_idx((area.left() - 4.0) / col_w);
                let c1 = floor_idx((area.right() - 4.0) / col_w);
                emit(
                    s,
                    c0.saturating_sub(1).saturating_mul(per_col),
                    c1.saturating_add(2).saturating_mul(per_col),
                );
            }
            Geo::Rows { row_h, .. } => {
                let first = self.sections.partition_point(|s| s.y_end < area.top());
                for s in self.sections[first..]
                    .iter()
                    .take_while(|s| s.hdr_y <= area.bottom())
                {
                    if s.len == 0 {
                        continue;
                    }
                    let r0 = floor_idx((area.top() - s.y0) / row_h);
                    let r1 = floor_idx((area.bottom() - s.y0) / row_h);
                    emit(s, r0.saturating_sub(1), r1.saturating_add(2));
                }
            }
            Geo::Grid { cell, cols, .. } => {
                let first = self.sections.partition_point(|s| s.y_end < area.top());
                for s in self.sections[first..]
                    .iter()
                    .take_while(|s| s.hdr_y <= area.bottom())
                {
                    if s.len == 0 {
                        continue;
                    }
                    let r0 = floor_idx((area.top() - (s.y0 + 4.0)) / cell.y);
                    let r1 = floor_idx((area.bottom() - (s.y0 + 4.0)) / cell.y);
                    emit(
                        s,
                        r0.saturating_sub(1).saturating_mul(cols),
                        r1.saturating_add(2).saturating_mul(cols),
                    );
                }
            }
        }
    }

    /// Call `f(section)` for each group header that intersects `area`.
    fn visit_headers(&self, area: Rect, f: &mut dyn FnMut(&Section, Rect)) {
        if !self.grouped() {
            return;
        }
        let first = self.sections.partition_point(|s| s.y_end < area.top());
        for s in self.sections[first..]
            .iter()
            .take_while(|s| s.hdr_y <= area.bottom())
        {
            let r = self.header_rect(s);
            if r.intersects(area) {
                f(s, r);
            }
        }
    }

    /// Every item as `(flat index, rectangle)`. O(n): only for the rare
    /// navigation cases arithmetic cannot answer.
    fn all_rects(&self) -> impl Iterator<Item = (usize, Rect)> + '_ {
        self.sections
            .iter()
            .flat_map(move |s| (0..s.len).map(move |n| (s.start + n, self.rect_in(s, n))))
    }

    /// Entry indices for flat positions `lo..=hi`, in order.
    fn entries_in(&self, lo: usize, hi: usize) -> impl Iterator<Item = usize> + '_ {
        self.sections
            .iter()
            .filter(move |s| s.len > 0 && s.start <= hi && s.start + s.len > lo)
            .flat_map(move |s| {
                let a = lo.max(s.start) - s.start;
                let b = (hi + 1).min(s.start + s.len) - s.start;
                self.set.flat[s.off + a..s.off + b].iter().copied()
            })
    }

    /// First flat position (in display order) whose entry satisfies `f`.
    fn position_of(&self, f: impl Fn(usize) -> bool) -> Option<usize> {
        self.sections.iter().find_map(|s| {
            self.set.flat[s.off..s.off + s.len]
                .iter()
                .position(|&i| f(i))
                .map(|p| s.start + p)
        })
    }

    /// First flat position at or after `start`, wrapping around, whose entry
    /// satisfies `f`.
    fn find_wrapping(&self, start: usize, f: impl Fn(usize) -> bool) -> Option<usize> {
        let n = self.total;
        if n == 0 {
            return None;
        }
        let start = start % n;
        let hit = |lo: usize, hi: usize| -> Option<usize> {
            self.entries_in(lo, hi)
                .zip(lo..)
                .find(|&(i, _)| f(i))
                .map(|(_, k)| k)
        };
        hit(start, n - 1).or_else(|| if start > 0 { hit(0, start - 1) } else { None })
    }
}

#[derive(Clone, Copy)]
struct Grid {
    cell: Vec2,
    icon: f32,
    tiles: bool,
    inline: bool,
}

fn grid_for(v: ViewMode) -> Option<Grid> {
    let g = |w: f32, h: f32, icon: f32, tiles: bool, inline: bool| Grid {
        cell: vec2(w, h),
        icon,
        tiles,
        inline,
    };
    Some(match v {
        ViewMode::ExtraLargeIcons => g(276.0, 300.0, 256.0, false, false),
        ViewMode::LargeIcons => g(116.0, 142.0, 96.0, false, false),
        ViewMode::MediumIcons => g(96.0, 92.0, 48.0, false, false),
        ViewMode::SmallIcons => g(180.0, 22.0, 16.0, false, true),
        ViewMode::Tiles => g(260.0, 58.0, 48.0, true, false),
        ViewMode::List | ViewMode::Details | ViewMode::Content => return None,
    })
}

fn details_width(st: &ExplorerState) -> f32 {
    st.columns
        .iter()
        .filter(|c| c.visible)
        .map(|c| c.width)
        .sum::<f32>()
        + 4.0
}

fn build_layout<'a>(st: &ExplorerState, set: &'a GroupSet, avail: Vec2) -> Layout<'a> {
    let grouped = set.grouped();
    let mut sections: Vec<Section> = Vec::with_capacity(set.groups.len());
    let mut total = 0usize;
    let mut live = 0usize;
    let mut y = 0.0_f32;
    let geo;
    let size: Vec2;
    // Header + collapse bookkeeping shared by the Rows and Grid layouts.
    // Returns (hdr_y, y0, collapsed) and advances `y` past the header.
    let open = |gi: usize, y: &mut f32| -> (f32, f32, bool) {
        let hdr_y = *y;
        let mut collapsed = false;
        if grouped {
            collapsed = st.collapsed_groups.contains(set.groups[gi].ukey.as_str());
            *y += GROUP_H;
        }
        (hdr_y, *y, collapsed)
    };
    match st.view {
        ViewMode::Details | ViewMode::Content => {
            let details = st.view == ViewMode::Details;
            let (row_h, width) = if details {
                (ROW_H, details_width(st))
            } else {
                (CONTENT_ROW_H, avail.x.max(320.0))
            };
            let span = width.max(avail.x);
            for (gi, g) in set.groups.iter().enumerate() {
                let (hdr_y, y0, collapsed) = open(gi, &mut y);
                let len = if collapsed { 0 } else { g.len };
                y = y0 + len as f32 * row_h;
                sections.push(Section {
                    gi,
                    off: g.off,
                    len,
                    start: total,
                    collapsed,
                    hdr_y,
                    y0,
                    y_end: y,
                });
                total += len;
                live += usize::from(len > 0);
            }
            size = vec2(width, y);
            geo = Geo::Rows {
                row_h,
                width,
                hdr_w: span,
            };
        }
        ViewMode::List => {
            let n = set.groups[0].len;
            let longest = st.longest_name_chars();
            let col_w = (longest as f32 * 6.8 + 36.0).clamp(130.0, 320.0);
            let per_col = (((avail.y - 4.0) / ROW_H).floor() as usize).max(1);
            let cols = n.div_ceil(per_col);
            sections.push(Section {
                gi: 0,
                off: 0,
                len: n,
                start: 0,
                collapsed: false,
                hdr_y: 0.0,
                y0: 0.0,
                y_end: 0.0,
            });
            total = n;
            live = usize::from(n > 0);
            size = vec2(4.0 + cols as f32 * col_w, avail.y);
            geo = Geo::List { col_w, per_col };
        }
        v => {
            let g = grid_for(v).expect("grid view");
            let cols = (((avail.x - 8.0) / g.cell.x).floor() as usize).max(1);
            for (gi, grp) in set.groups.iter().enumerate() {
                let (hdr_y, y0, collapsed) = open(gi, &mut y);
                if collapsed {
                    sections.push(Section {
                        gi,
                        off: grp.off,
                        len: 0,
                        start: total,
                        collapsed,
                        hdr_y,
                        y0,
                        y_end: y0,
                    });
                    continue;
                }
                let len = grp.len;
                y = y0 + 4.0 + len.div_ceil(cols) as f32 * g.cell.y + 8.0;
                sections.push(Section {
                    gi,
                    off: grp.off,
                    len,
                    start: total,
                    collapsed,
                    hdr_y,
                    y0,
                    y_end: y,
                });
                total += len;
                live += usize::from(len > 0);
            }
            size = vec2(avail.x, y);
            geo = Geo::Grid {
                cell: g.cell,
                cols,
                hdr_w: avail.x.max(g.cell.x),
            };
        }
    }
    Layout {
        set,
        geo,
        sections,
        live,
        total,
        size,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Draw the item pane into the remaining space of `ui`.
pub fn show(ui: &mut Ui, state: &mut ExplorerState) {
    // Cached: the filter/sort/group work only reruns when an input changed.
    let set = state.group_set();
    let mut renaming = state.renaming.take();
    let mut acts: Vec<Act> = Vec::new();
    {
        let st: &ExplorerState = state;
        render(ui, st, &set, &mut renaming, &mut acts);
    }
    // A rename that was committed/cancelled this frame comes back as an action.
    let finished = acts.iter().any(|a| matches!(a, Act::Rename(..)))
        || renaming
            .as_ref()
            .is_some_and(|r| r.path.as_os_str().is_empty());
    state.renaming = if finished { None } else { renaming };
    apply(state, acts);
}

fn render(
    ui: &mut Ui,
    st: &ExplorerState,
    set: &GroupSet,
    renaming: &mut Option<RenameState>,
    acts: &mut Vec<Act>,
) {
    let avail = ui.available_size();
    let salt = ("file_list", &st.location, st.view);
    match st.view {
        ViewMode::Details => {
            let total_w = details_width(st).max(avail.x);
            ScrollArea::horizontal()
                .id_salt((salt, "h"))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(total_w);
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    details_header(ui, st, total_w, acts);
                    ScrollArea::vertical()
                        .id_salt((salt, "v"))
                        .auto_shrink([false, false])
                        .show_viewport(ui, |ui, vp| {
                            let layout = build_layout(st, set, vec2(total_w, vp.height()));
                            draw_content(
                                ui,
                                st,
                                &layout,
                                vp,
                                None,
                                renaming,
                                acts,
                                vp.height(),
                                total_w,
                            );
                        });
                });
        }
        ViewMode::List => {
            ScrollArea::horizontal()
                .id_salt((salt, "h"))
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, vp| {
                    // vertical wheel scrolls sideways, like Explorer
                    let dy = ui.input(|i| i.smooth_scroll_delta.y);
                    if dy != 0.0 && ui.rect_contains_pointer(ui.max_rect()) {
                        ui.scroll_with_delta(vec2(dy, 0.0));
                    }
                    let layout = build_layout(st, set, vec2(avail.x, avail.y));
                    draw_content(ui, st, &layout, vp, None, renaming, acts, avail.y, avail.x);
                });
        }
        v => {
            let grid = grid_for(v);
            ScrollArea::vertical()
                .id_salt((salt, "v"))
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, vp| {
                    let layout = build_layout(st, set, vec2(avail.x - 12.0, avail.y));
                    draw_content(
                        ui,
                        st,
                        &layout,
                        vp,
                        grid,
                        renaming,
                        acts,
                        vp.height(),
                        avail.x - 12.0,
                    );
                });
        }
    }
    ghost(ui);
}

/// Floating label that follows the pointer while dragging items.
fn ghost(ui: &Ui) {
    let ctx = ui.ctx();
    let Some(payload) = egui::DragAndDrop::payload::<Vec<PathBuf>>(ctx) else {
        return;
    };
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    ctx.set_cursor_icon(CursorIcon::Grabbing);
    let text = if payload.len() == 1 {
        payload[0]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        format!("{} items", payload.len())
    };
    let p = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("fl_dnd_ghost")));
    let g = p.layout_no_wrap(text, theme::font(), colors::text());
    let r = Rect::from_min_size(pos + vec2(14.0, 14.0), g.size() + vec2(30.0, 6.0));
    p.rect_filled(r, CornerRadius::ZERO, colors::item_hover());
    p.rect_stroke(
        r,
        CornerRadius::ZERO,
        Stroke::new(1.0, colors::item_selected_border()),
        StrokeKind::Inside,
    );
    Icon::File.paint(
        &p,
        Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), vec2(14.0, 14.0)),
        true,
    );
    p.galley(
        pos2(r.left() + 24.0, r.center().y - g.size().y / 2.0),
        g,
        colors::text(),
    );
}

// ---------------------------------------------------------------------------
// Details header
// ---------------------------------------------------------------------------

fn details_header(ui: &mut Ui, st: &ExplorerState, width: f32, acts: &mut Vec<Act>) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, HEADER_H), Sense::hover());
    let p = ui.painter().clone();
    p.rect_filled(rect, CornerRadius::ZERO, colors::header_bg());
    let font = theme::font();
    let mut x = rect.left();
    for (ci, col) in st.columns.iter().enumerate() {
        if !col.visible {
            continue;
        }
        let r = Rect::from_min_size(pos2(x, rect.top()), vec2(col.width, HEADER_H));
        let resp = ui.interact(r, Id::new(("col_hdr", ci)), Sense::click());
        if resp.hovered() {
            p.rect_filled(r, CornerRadius::ZERO, colors::header_hover());
        }
        let label = col.key.label();
        if col.key == SortKey::Size {
            p.text(
                pos2(r.right() - 10.0, r.center().y + 2.0),
                Align2::RIGHT_CENTER,
                label,
                font.clone(),
                colors::header_text(),
            );
        } else {
            let off = if st.item_check_boxes && col.key == SortKey::Name {
                18.0
            } else {
                0.0
            };
            p.text(
                pos2(r.left() + 8.0 + off, r.center().y + 2.0),
                Align2::LEFT_CENTER,
                label,
                font.clone(),
                colors::header_text(),
            );
        }
        if st.sort_key == col.key {
            let icon = if st.sort_order == SortOrder::Ascending {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            };
            icon.paint(
                &p,
                Rect::from_center_size(pos2(r.center().x, r.top() + 5.0), vec2(8.0, 8.0)),
                true,
            );
        }
        p.line_segment(
            [
                pos2(r.right(), r.top() + 2.0),
                pos2(r.right(), r.bottom() - 2.0),
            ],
            Stroke::new(1.0, colors::separator()),
        );
        // select-all check box (Name column header, only with item check boxes on)
        let mut all_clicked = false;
        if st.item_check_boxes && col.key == SortKey::Name {
            let br =
                Rect::from_center_size(pos2(r.left() + 12.0, r.center().y + 1.0), vec2(14.0, 14.0));
            let total = st.visible_count();
            let all = total > 0 && st.selection.len() >= total;
            p.rect_filled(br, CornerRadius::ZERO, colors::field_bg());
            p.rect_stroke(
                br,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::field_border()),
                StrokeKind::Inside,
            );
            if all {
                Icon::Check.paint(&p, br.shrink(1.0), true);
            }
            let cb = ui.interact(br, Id::new(("col_all", ci)), Sense::click());
            if cb.clicked() {
                all_clicked = true;
                acts.push(if all {
                    Act::ClearSel
                } else {
                    Act::Cmd(Command::SelectAll)
                });
            }
        }
        if resp.clicked() && !all_clicked {
            acts.push(Act::SetSort(col.key));
        }
        // column chooser
        let mut toggle: Option<usize> = None;
        context_menu(&resp, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            for (i, c) in st.columns.iter().enumerate() {
                if menu_row(
                    ui,
                    None,
                    c.key.label(),
                    Some(c.visible),
                    c.key != SortKey::Name,
                    None,
                )
                .clicked()
                {
                    toggle = Some(i);
                    ui.close();
                }
            }
        });
        if let Some(i) = toggle {
            acts.push(Act::ToggleColumn(i));
        }
        // resize grip on the right edge
        let grip = Rect::from_min_size(pos2(r.right() - 3.0, r.top()), vec2(7.0, HEADER_H));
        let gr = ui.interact(grip, Id::new(("col_grip", ci)), Sense::click_and_drag());
        if gr.hovered() || gr.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if gr.dragged() {
            acts.push(Act::SetColumnWidth(ci, col.width + gr.drag_delta().x));
        }
        if gr.double_clicked() {
            acts.push(Act::ResetColumnWidth(ci));
        }
        x += col.width;
    }
}

// ---------------------------------------------------------------------------
// Content drawing & interaction
// ---------------------------------------------------------------------------

struct Flags {
    selected: bool,
    hovered: bool,
    drop_hover: bool,
    faded: bool,
    check_visible: bool,
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    ui: &mut Ui,
    st: &ExplorerState,
    layout: &Layout<'_>,
    vp: Rect,
    grid: Option<Grid>,
    renaming: &mut Option<RenameState>,
    acts: &mut Vec<Act>,
    min_h: f32,
    min_w: f32,
) {
    let entries = &st.entries;
    let size = vec2(layout.size.x.max(min_w), layout.size.y.max(min_h));
    let (rect, bg) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let origin = rect.min;
    let shift = origin.to_vec2();
    let painter = ui.painter().clone();
    let mods = ui.input(|i| i.modifiers);
    let vp_screen = vp.translate(shift);
    let ctx = ui.ctx().clone();

    // ---- keyboard ---------------------------------------------------------
    if renaming.is_none()
        && let Some((act, target)) = keyboard(ui, st, layout, vp.height())
    {
        acts.push(act);
        ui.scroll_to_rect(target.translate(shift), None);
    }

    // ---- background: clear selection, rubber band, context menu ----------------
    if bg.clicked_by(PointerButton::Primary) && !mods.ctrl && !mods.shift {
        acts.push(Act::ClearSel);
    }
    let band_id = Id::new("fl_band");
    if bg.drag_started_by(PointerButton::Primary)
        && let Some(pos) = bg.interact_pointer_pos()
    {
        let base = if mods.ctrl || mods.shift {
            st.selection.clone()
        } else {
            HashSet::new()
        };
        ctx.data_mut(|d| d.insert_temp(band_id, (pos - origin, base)));
    }
    if bg.dragged_by(PointerButton::Primary) {
        let state: Option<(Vec2, HashSet<PathBuf>)> = ctx.data(|d| d.get_temp(band_id));
        if let (Some(cur), Some((start, base))) = (bg.interact_pointer_pos(), state) {
            let band = Rect::from_two_pos(origin + start, cur);
            painter.rect_filled(
                band,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(0, 120, 215, 50),
            );
            painter.rect_stroke(
                band,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::accent()),
                StrokeKind::Inside,
            );
            let band_c = band.translate(-shift);
            let mut sel = base;
            layout.visit_in(band_c, &mut |_, idx, _| {
                sel.insert(entries[idx].path.clone());
            });
            acts.push(Act::Select {
                sel,
                focus: None,
                anchor: None,
            });
            // auto-scroll while the band is dragged past the edges
            if cur.y < vp_screen.top() {
                ui.scroll_with_delta(vec2(0.0, 12.0));
            } else if cur.y > vp_screen.bottom() {
                ui.scroll_with_delta(vec2(0.0, -12.0));
            }
        }
    }
    let mut picked: Option<Command> = None;
    if bg.secondary_clicked() {
        acts.push(Act::ClearSel);
    }
    context_menu(&bg, |ui| {
        render_menu(ui, &background_menu(st.in_trash()), st, &mut picked);
        if picked.is_some() {
            ui.close();
        }
    });

    // ---- empty message ------------------------------------------------------
    if layout.len() == 0 && !layout.has_headers() {
        let msg = if st.search_text.is_empty() {
            "This folder is empty."
        } else {
            "No items match your search."
        };
        painter.text(
            pos2(rect.center().x, rect.top() + 36.0),
            Align2::CENTER_CENTER,
            msg,
            theme::font(),
            colors::text_secondary(),
        );
    }

    // ---- group headers --------------------------------------------------------
    layout.visit_headers(vp, &mut |sec, hr| {
        let grp = &layout.set.groups[sec.gi];
        let r = hr.translate(shift);
        let resp = ui.interact(r, Id::new(("fl_group", &grp.ukey)), Sense::click());
        let icon = if sec.collapsed {
            Icon::ChevronRight
        } else {
            Icon::ChevronDown
        };
        icon.paint(
            &painter,
            Rect::from_center_size(pos2(r.left() + 12.0, r.center().y + 2.0), vec2(10.0, 10.0)),
            true,
        );
        let g = painter.layout_no_wrap(
            grp.label.clone(),
            FontId::proportional(15.0),
            colors::group_text(),
        );
        let tw = g.size().x;
        painter.galley(
            pos2(r.left() + 26.0, r.center().y - g.size().y / 2.0 + 2.0),
            g,
            colors::group_text(),
        );
        painter.text(
            pos2(r.left() + 26.0 + tw + 6.0, r.center().y + 3.0),
            Align2::LEFT_CENTER,
            format!("({})", grp.len),
            theme::font_small(),
            colors::text_secondary(),
        );
        painter.line_segment(
            [
                pos2(r.left() + 26.0 + tw + 46.0, r.center().y + 4.0),
                pos2(r.right() - 10.0, r.center().y + 4.0),
            ],
            Stroke::new(1.0, colors::separator()),
        );
        if resp.clicked() {
            acts.push(Act::ToggleGroup(grp.ukey.clone()));
        }
    });

    // ---- items --------------------------------------------------------------------
    let now_drag = egui::DragAndDrop::has_payload_of_type::<Vec<PathBuf>>(&ctx);
    // Only items in or just around the viewport are ever materialised.
    let mut visible: Vec<(usize, usize, Rect)> = Vec::new();
    layout.visit_in(vp.expand(4.0), &mut |k, idx, r| visible.push((k, idx, r)));
    for (k, idx, item_rect) in visible {
        let r = item_rect.translate(shift);
        if !r.intersects(vp_screen.expand(4.0)) {
            continue;
        }
        let e = &entries[idx];
        let icon_view = matches!(
            st.view,
            ViewMode::ExtraLargeIcons
                | ViewMode::LargeIcons
                | ViewMode::MediumIcons
                | ViewMode::SmallIcons
                | ViewMode::Tiles
        );
        let wants_preview = st.show_preview_pane && st.selection.contains(&e.path);
        if ((st.thumbnails_enabled && icon_view) || wants_preview)
            && e.kind == EntryKind::File
            && e.thumbnail.is_none()
            && fm_thumbs::ThumbnailStore::is_supported_media(&e.path)
        {
            acts.push(Act::ThumbnailNeeded(e.path.clone()));
        }
        let selected = st.selection.contains(&e.path);
        let id = Id::new(("fl_item", &e.path));
        let resp = ui.interact(r, id, Sense::click_and_drag());
        let is_renaming = renaming.as_ref().is_some_and(|rn| rn.path == e.path);

        // drag & drop
        let mut drop_hover = false;
        if e.is_dir() {
            if let Some(p) = resp.dnd_hover_payload::<Vec<PathBuf>>() {
                drop_hover = !p.contains(&e.path);
            }
            if let Some(p) = resp.dnd_release_payload::<Vec<PathBuf>>()
                && !p.contains(&e.path)
            {
                acts.push(Act::Drop((*p).clone(), e.path.clone()));
            }
        }
        if resp.drag_started_by(PointerButton::Primary) && !is_renaming {
            let paths: Vec<PathBuf> = if selected {
                entries
                    .iter()
                    .filter(|x| st.selection.contains(&x.path))
                    .map(|x| x.path.clone())
                    .collect()
            } else {
                acts.push(Act::Select {
                    sel: HashSet::from([e.path.clone()]),
                    focus: Some(e.path.clone()),
                    anchor: Some(e.path.clone()),
                });
                vec![e.path.clone()]
            };
            resp.dnd_set_drag_payload(paths);
        }

        let check_visible = st.item_check_boxes && (selected || resp.hovered());
        let flags = Flags {
            selected,
            hovered: resp.hovered() && !now_drag || drop_hover,
            drop_hover,
            faded: st.cut.contains(&e.path) || e.hidden,
            check_visible,
        };
        paint_item(&painter, st, e, r, grid, &flags, is_renaming);

        // check box (registered after the cell so it wins the click)
        let mut box_clicked = false;
        if st.item_check_boxes {
            let br = check_rect(st.view, grid, r);
            let cb = ui.interact(br, id.with("cb"), Sense::click());
            box_clicked = cb.clicked();
        }

        if is_renaming {
            if let Some(rn) = renaming.as_mut() {
                let nr = name_rect(st, grid, r);
                rename_editor(ui, rn, nr, e.is_dir(), acts);
            }
        } else {
            if box_clicked {
                let mut sel = st.selection.clone();
                if !sel.remove(&e.path) {
                    sel.insert(e.path.clone());
                }
                acts.push(Act::Select {
                    sel,
                    focus: Some(e.path.clone()),
                    anchor: Some(e.path.clone()),
                });
            } else if resp.clicked_by(PointerButton::Primary) {
                acts.push(click_selection(st, layout, k, mods));
            }
            if resp.secondary_clicked() && !selected {
                acts.push(Act::Select {
                    sel: HashSet::from([e.path.clone()]),
                    focus: Some(e.path.clone()),
                    anchor: Some(e.path.clone()),
                });
            }
            if resp.double_clicked_by(PointerButton::Primary) && !box_clicked {
                acts.push(Act::Open(e.path.clone(), e.is_dir()));
            }
            context_menu(&resp, |ui| {
                // Built only while the menu is open: it depends on what is selected.
                let item_menu_entries = item_menu(st.in_trash(), st.selection_has_archive());
                render_menu(ui, &item_menu_entries, st, &mut picked);
                if picked.is_some() {
                    ui.close();
                }
            });
        }
        if st.view != ViewMode::Details && resp.hovered() && !e.name.is_empty() {
            // tooltip with the info Explorer shows on hover
            let tip = format!(
                "Name: {}\nType: {}{}{}",
                e.name,
                e.type_name,
                if e.is_dir() {
                    String::new()
                } else {
                    format!("\nSize: {}", format_size(e.size))
                },
                e.modified
                    .map(|t| format!("\nDate modified: {}", (st.date_format)(t)))
                    .unwrap_or_default(),
            );
            resp.on_hover_text(tip);
        }
    }
    if let Some(c) = picked {
        acts.push(Act::Cmd(c));
    }
}

fn click_selection(st: &ExplorerState, layout: &Layout<'_>, k: usize, mods: Modifiers) -> Act {
    let entries = &st.entries;
    let path = entries[layout.entry_idx(k)].path.clone();
    if mods.shift {
        let anchor_pos = st
            .anchor
            .as_ref()
            .and_then(|a| layout.position_of(|i| &entries[i].path == a))
            .unwrap_or(k);
        let (lo, hi) = (anchor_pos.min(k), anchor_pos.max(k));
        let mut sel = if mods.ctrl {
            st.selection.clone()
        } else {
            HashSet::new()
        };
        for i in layout.entries_in(lo, hi) {
            sel.insert(entries[i].path.clone());
        }
        Act::Select {
            sel,
            focus: Some(path),
            anchor: None,
        }
    } else if mods.ctrl {
        let mut sel = st.selection.clone();
        if !sel.remove(&path) {
            sel.insert(path.clone());
        }
        Act::Select {
            sel,
            focus: Some(path.clone()),
            anchor: Some(path),
        }
    } else {
        Act::Select {
            sel: HashSet::from([path.clone()]),
            focus: Some(path.clone()),
            anchor: Some(path),
        }
    }
}

// ---------------------------------------------------------------------------
// Context menus
// ---------------------------------------------------------------------------

fn item_menu(in_trash: bool, has_archive: bool) -> Vec<MenuEntry> {
    use Command as C;
    use Icon as I;
    use MenuEntry as M;
    if in_trash {
        return vec![
            M::item("Restore", C::RestoreFromTrash),
            M::Sep,
            M::item("Delete permanently", C::DeletePermanently).icon(I::Trash),
            M::Sep,
            M::item("Properties", C::Properties)
                .icon(I::Properties)
                .key("Alt+Enter"),
        ];
    }
    let mut v = vec![
        M::item("Open", C::Open).icon(I::Open),
        M::item("Open with...", C::OpenWith),
        M::Sep,
        M::item("Pin to Quick access", C::PinToQuickAccess).icon(I::Pin),
        M::item("Compress to tar.xz", C::Compress).icon(I::Compress),
        M::item(
            "Compress to ZIP",
            C::CompressAs(crate::CompressFormat::Zip, 6),
        ),
        M::item("Compress to...", C::ShowCompressOptions),
        M::item("Share", C::Share).icon(I::Share),
        M::Sep,
        M::item("Cut", C::Cut).icon(I::Cut).key("Ctrl+X"),
        M::item("Copy", C::Copy).icon(I::Copy).key("Ctrl+C"),
        M::item("Copy file path", C::CopyPath),
        M::item("Copy folder path", C::CopyFolderPath),
        M::item("Copy file URL", C::CopyUrl),
        M::item("Copy name", C::CopyName),
        M::Sep,
        M::item("Create link", C::NewShortcut),
        M::item("Delete", C::Delete).icon(I::Trash).key("Del"),
        M::item("Delete permanently", C::DeletePermanently).key("Shift+Del"),
        M::item("Rename", C::Rename).icon(I::Rename).key("F2"),
        M::Sep,
        M::item("Properties", C::Properties)
            .icon(I::Properties)
            .key("Alt+Enter"),
    ];
    if has_archive {
        // Right after the "Compress to..." entries.
        let at = v
            .iter()
            .position(|e| {
                matches!(
                    e,
                    M::Item {
                        cmd: C::ShowCompressOptions,
                        ..
                    }
                )
            })
            .map_or(1, |i| i + 1);
        v.insert(at, M::item("Extract here", C::ExtractHere).icon(I::Extract));
        v.insert(at + 1, M::item("Extract to folder", C::Extract));
    }
    v
}

fn background_menu(in_trash: bool) -> Vec<MenuEntry> {
    use Command as C;
    use MenuEntry as M;
    let full = background_menu_full();
    if !in_trash {
        return full;
    }
    // Trash: keep View / Sort by / Group by / Refresh, drop Paste + New, add Empty Trash.
    let mut v: Vec<MenuEntry> = full.into_iter().take(4).collect();
    v.push(M::Sep);
    v.push(M::item("Empty Trash", C::EmptyTrash).icon(Icon::Trash));
    v.push(M::Sep);
    v.push(M::item("Properties", C::Properties).icon(Icon::Properties));
    v
}

fn background_menu_full() -> Vec<MenuEntry> {
    use Command as C;
    use MenuEntry as M;
    vec![
        M::Sub {
            label: "View",
            items: ViewMode::ALL
                .iter()
                .map(|&v| M::item(v.label(), C::SetView(v)))
                .collect(),
        },
        M::Sub {
            label: "Sort by",
            items: vec![
                M::item("Name", C::SortBy(SortKey::Name)),
                M::item("Size", C::SortBy(SortKey::Size)),
                M::item("Type", C::SortBy(SortKey::Type)),
                M::item("Date modified", C::SortBy(SortKey::DateModified)),
                M::Sep,
                M::item("Ascending", C::SetSortOrder(SortOrder::Ascending)),
                M::item("Descending", C::SetSortOrder(SortOrder::Descending)),
            ],
        },
        M::Sub {
            label: "Group by",
            items: GroupKey::ALL
                .iter()
                .map(|&g| M::item(g.label(), C::GroupBy(g)))
                .collect(),
        },
        M::item("Refresh", C::Refresh).key("F5"),
        M::Sep,
        M::item("Paste", C::Paste).icon(Icon::Paste).key("Ctrl+V"),
        M::item("Paste shortcut", C::PasteShortcut).icon(Icon::PasteShortcut),
        M::Sep,
        M::Sub {
            label: "New",
            items: vec![
                M::item("Folder", C::NewFolder).icon(Icon::Folder),
                M::item("Shortcut", C::NewShortcut),
                M::item("Text Document", C::NewTextFile).icon(Icon::FileText),
            ],
        },
        M::Sep,
        M::item("Open terminal here", C::OpenTerminal).icon(Icon::Terminal),
        M::item("Properties", C::Properties).icon(Icon::Properties),
    ]
}

// ---------------------------------------------------------------------------
// Painting a single item
// ---------------------------------------------------------------------------

fn fit(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
    rows: usize,
    center: bool,
) -> Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_owned(), font, color, max_w.max(8.0));
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    if center {
        job.halign = Align::Center;
    }
    painter.layout_job(job)
}

fn check_rect(view: ViewMode, grid: Option<Grid>, r: Rect) -> Rect {
    match (view, grid) {
        (ViewMode::Details | ViewMode::List, _) => {
            Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), vec2(14.0, 14.0))
        }
        (_, Some(g)) if g.inline => {
            Rect::from_center_size(pos2(r.left() + 12.0, r.center().y), vec2(14.0, 14.0))
        }
        (ViewMode::Content, _) => {
            Rect::from_center_size(pos2(r.left() + 12.0, r.top() + 12.0), vec2(14.0, 14.0))
        }
        _ => Rect::from_center_size(pos2(r.left() + 12.0, r.top() + 12.0), vec2(14.0, 14.0)),
    }
}

fn name_rect(st: &ExplorerState, grid: Option<Grid>, r: Rect) -> Rect {
    let cb = if st.item_check_boxes { 18.0 } else { 0.0 };
    match (st.view, grid) {
        (ViewMode::Details, _) => {
            let w = st
                .columns
                .iter()
                .find(|c| c.key == SortKey::Name)
                .map(|c| c.width)
                .unwrap_or(240.0);
            Rect::from_min_size(
                pos2(r.left() + 24.0 + cb, r.top() + 1.0),
                vec2(w - 26.0 - cb, r.height() - 2.0),
            )
        }
        (ViewMode::List, _) => Rect::from_min_size(
            pos2(r.left() + 24.0 + cb, r.top() + 1.0),
            vec2(r.width() - 26.0 - cb, r.height() - 2.0),
        ),
        (ViewMode::Content, _) => Rect::from_min_size(
            pos2(r.left() + 48.0, r.top() + 4.0),
            vec2(r.width() - 56.0, 22.0),
        ),
        (_, Some(g)) if g.inline => Rect::from_min_size(
            pos2(r.left() + 22.0 + cb, r.top() + 1.0),
            vec2(r.width() - 24.0 - cb, r.height() - 2.0),
        ),
        (_, Some(g)) if g.tiles => Rect::from_min_size(
            pos2(r.left() + g.icon + 12.0, r.top() + 3.0),
            vec2(r.width() - g.icon - 18.0, 22.0),
        ),
        (_, Some(g)) => Rect::from_min_size(
            pos2(r.left() + 2.0, r.top() + g.icon + 8.0),
            vec2(r.width() - 4.0, 22.0),
        ),
        _ => r,
    }
}

fn paint_item(
    painter: &Painter,
    st: &ExplorerState,
    e: &Entry,
    r: Rect,
    grid: Option<Grid>,
    f: &Flags,
    skip_name: bool,
) {
    // background
    let rr = r.shrink(0.5);
    if f.selected {
        let fill = if !st.window_focused {
            colors::item_selected_inactive()
        } else if f.hovered {
            colors::item_selected_hover()
        } else {
            colors::item_selected()
        };
        painter.rect_filled(rr, CornerRadius::ZERO, fill);
        painter.rect_stroke(
            rr,
            CornerRadius::ZERO,
            Stroke::new(1.0, colors::item_selected_border()),
            StrokeKind::Inside,
        );
    } else if f.hovered {
        painter.rect_filled(rr, CornerRadius::ZERO, colors::item_hover());
        if f.drop_hover {
            painter.rect_stroke(
                rr,
                CornerRadius::ZERO,
                Stroke::new(1.0, colors::item_selected_border()),
                StrokeKind::Inside,
            );
        }
    }
    let mut p = painter.clone();
    if f.faded {
        p.multiply_opacity(0.5);
    }
    let font = theme::font();
    let small = theme::font_small();
    let name = e.display_name(st.show_extensions);
    let cb = st.item_check_boxes;

    // check box
    if f.check_visible {
        let b = check_rect(st.view, grid, r);
        painter.rect_filled(b, CornerRadius::ZERO, colors::field_bg());
        painter.rect_stroke(
            b,
            CornerRadius::ZERO,
            Stroke::new(1.0, colors::field_border()),
            StrokeKind::Inside,
        );
        if f.selected {
            Icon::Check.paint(painter, b.shrink(1.0), true);
        }
    }

    match (st.view, grid) {
        (ViewMode::Details, _) => {
            let mut x = r.left();
            let cy = r.center().y;
            for col in st.columns.iter().filter(|c| c.visible) {
                let cr = Rect::from_min_size(pos2(x, r.top()), vec2(col.width, r.height()));
                let clip = p.with_clip_rect(cr.intersect(p.clip_rect()));
                match col.key {
                    SortKey::Name => {
                        let ix = cr.left() + 4.0 + if cb { 18.0 } else { 0.0 };
                        e.icon().paint(
                            &clip,
                            Rect::from_center_size(pos2(ix + 8.0, cy), vec2(16.0, 16.0)),
                            true,
                        );
                        if !skip_name {
                            let g = fit(
                                &clip,
                                name,
                                font.clone(),
                                colors::text(),
                                cr.right() - ix - 26.0,
                                1,
                                false,
                            );
                            clip.galley(pos2(ix + 22.0, cy - g.size().y / 2.0), g, colors::text());
                        }
                    }
                    SortKey::DateModified => {
                        if let Some(t) = e.modified {
                            clip.text(
                                pos2(cr.left() + 8.0, cy),
                                Align2::LEFT_CENTER,
                                (st.date_format)(t),
                                font.clone(),
                                colors::text(),
                            );
                        }
                    }
                    SortKey::Type => {
                        clip.text(
                            pos2(cr.left() + 8.0, cy),
                            Align2::LEFT_CENTER,
                            &e.type_name,
                            font.clone(),
                            colors::text(),
                        );
                    }
                    SortKey::Size => {
                        if e.kind == EntryKind::File {
                            clip.text(
                                pos2(cr.right() - 10.0, cy),
                                Align2::RIGHT_CENTER,
                                format_size(e.size),
                                font.clone(),
                                colors::text(),
                            );
                        }
                    }
                }
                x += col.width;
            }
        }
        (ViewMode::List, _) => {
            let cy = r.center().y;
            let ix = r.left() + 4.0 + if cb { 18.0 } else { 0.0 };
            e.icon().paint(
                &p,
                Rect::from_center_size(pos2(ix + 8.0, cy), vec2(16.0, 16.0)),
                true,
            );
            if !skip_name {
                let g = fit(
                    &p,
                    name,
                    font.clone(),
                    colors::text(),
                    r.right() - ix - 28.0,
                    1,
                    false,
                );
                p.galley(pos2(ix + 22.0, cy - g.size().y / 2.0), g, colors::text());
            }
        }
        (ViewMode::Content, _) => {
            let ix = r.left() + 10.0 + if cb { 18.0 } else { 0.0 };
            e.icon().paint(
                &p,
                Rect::from_center_size(pos2(ix + 16.0, r.center().y), vec2(32.0, 32.0)),
                true,
            );
            let tx = r.left() + 48.0 + if cb { 14.0 } else { 0.0 };
            if !skip_name {
                let g = fit(
                    &p,
                    name,
                    font.clone(),
                    colors::text(),
                    r.right() - tx - 8.0,
                    1,
                    false,
                );
                p.galley(pos2(tx, r.top() + 7.0), g, colors::text());
            }
            let mut info = e.type_name.clone();
            if let Some(t) = e.modified {
                info.push_str(&format!("    Date modified: {}", (st.date_format)(t)));
            }
            if e.kind == EntryKind::File {
                info.push_str(&format!("    Size: {}", format_size(e.size)));
            }
            let g = fit(
                &p,
                &info,
                small.clone(),
                colors::text_secondary(),
                r.right() - tx - 8.0,
                1,
                false,
            );
            p.galley(pos2(tx, r.top() + 29.0), g, colors::text_secondary());
            painter.line_segment(
                [
                    pos2(r.left() + 6.0, r.bottom() - 0.5),
                    pos2(r.right() - 6.0, r.bottom() - 0.5),
                ],
                Stroke::new(1.0, colors::separator()),
            );
        }
        (_, Some(g)) if g.inline => {
            let cy = r.center().y;
            let ix = r.left() + 4.0 + if cb { 18.0 } else { 0.0 };
            e.icon().paint(
                &p,
                Rect::from_center_size(pos2(ix + 8.0, cy), vec2(16.0, 16.0)),
                true,
            );
            if !skip_name {
                let gl = fit(
                    &p,
                    name,
                    font.clone(),
                    colors::text(),
                    r.right() - ix - 28.0,
                    1,
                    false,
                );
                p.galley(pos2(ix + 22.0, cy - gl.size().y / 2.0), gl, colors::text());
            }
        }
        (_, Some(g)) if g.tiles => {
            let icon_r = Rect::from_center_size(
                pos2(r.left() + 6.0 + g.icon / 2.0, r.center().y),
                vec2(g.icon, g.icon),
            );
            paint_icon_or_thumb(&p, e, icon_r);
            let tx = r.left() + g.icon + 12.0;
            let w = r.right() - tx - 6.0;
            if !skip_name {
                let gl = fit(&p, name, font.clone(), colors::text(), w, 1, false);
                p.galley(pos2(tx, r.top() + 4.0), gl, colors::text());
            }
            if let (EntryKind::Drive, Some((total, free))) = (e.kind, e.space) {
                let bar = Rect::from_min_size(pos2(tx, r.top() + 23.0), vec2(w.min(150.0), 12.0));
                let used = if total > 0 {
                    1.0 - free as f32 / total as f32
                } else {
                    0.0
                };
                p.rect_filled(
                    bar,
                    CornerRadius::ZERO,
                    if theme::dark_mode() {
                        Color32::from_rgb(58, 58, 58)
                    } else {
                        Color32::from_rgb(230, 230, 230)
                    },
                );
                let fill = Rect::from_min_size(bar.min, vec2(bar.width() * used, bar.height()));
                p.rect_filled(
                    fill,
                    CornerRadius::ZERO,
                    if used > 0.9 {
                        Color32::from_rgb(218, 38, 38)
                    } else {
                        Color32::from_rgb(38, 160, 218)
                    },
                );
                p.rect_stroke(
                    bar,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, colors::field_border()),
                    StrokeKind::Inside,
                );
                let t = format!("{} free of {}", format_size(free), format_size(total));
                let gl = fit(&p, &t, small.clone(), colors::text_secondary(), w, 1, false);
                p.galley(pos2(tx, r.top() + 38.0), gl, colors::text_secondary());
            } else {
                let gl = fit(
                    &p,
                    &e.type_name,
                    small.clone(),
                    colors::text_secondary(),
                    w,
                    1,
                    false,
                );
                p.galley(pos2(tx, r.top() + 22.0), gl, colors::text_secondary());
                if e.kind == EntryKind::File {
                    let gl = fit(
                        &p,
                        &format_size(e.size),
                        small.clone(),
                        colors::text_secondary(),
                        w,
                        1,
                        false,
                    );
                    p.galley(pos2(tx, r.top() + 38.0), gl, colors::text_secondary());
                }
            }
        }
        (_, Some(g)) => {
            let icon_r = Rect::from_center_size(
                pos2(r.center().x, r.top() + 5.0 + g.icon / 2.0),
                vec2(g.icon, g.icon),
            );
            paint_icon_or_thumb(&p, e, icon_r);
            if !skip_name {
                let rows = if f.selected { 4 } else { 2 };
                let gl = fit(
                    &p,
                    name,
                    font.clone(),
                    colors::text(),
                    r.width() - 8.0,
                    rows,
                    true,
                );
                p.galley(
                    pos2(r.center().x, r.top() + g.icon + 10.0),
                    gl,
                    colors::text(),
                );
            }
        }
        _ => {}
    }
}

fn paint_icon_or_thumb(p: &Painter, e: &Entry, r: Rect) {
    if let Some(tex) = e.thumbnail {
        let box_r = r.shrink(2.0);
        p.rect_filled(box_r, CornerRadius::ZERO, colors::window_bg());
        p.image(
            tex,
            box_r,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        p.rect_stroke(
            box_r,
            CornerRadius::ZERO,
            Stroke::new(1.0, colors::field_border()),
            StrokeKind::Outside,
        );
    } else {
        e.icon().paint(p, r, true);
    }
}

// ---------------------------------------------------------------------------
// Inline rename
// ---------------------------------------------------------------------------

fn rename_editor(ui: &mut Ui, rn: &mut RenameState, rect: Rect, is_dir: bool, acts: &mut Vec<Act>) {
    let id = Id::new("fl_rename_edit");
    let out = ui
        .scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            TextEdit::singleline(&mut rn.text)
                .id(id)
                .font(theme::font())
                .margin(Margin::symmetric(2, 1))
                .desired_width(rect.width())
                .show(ui)
        })
        .inner;
    if rn.just_started {
        out.response.request_focus();
        let total = rn.text.chars().count();
        let stem = if is_dir {
            total
        } else {
            match rn.text.rfind('.') {
                Some(i) if i > 0 => rn.text[..i].chars().count(),
                _ => total,
            }
        };
        let mut state = out.state.clone();
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(stem))));
        state.store(ui.ctx(), id);
        rn.just_started = false;
        return;
    }
    if out.response.lost_focus() {
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        let new_name = rn.text.trim().to_owned();
        let old = rn
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !esc && !new_name.is_empty() && new_name != old {
            acts.push(Act::Rename(rn.path.clone(), new_name));
        } else {
            // cancel: mark as finished by clearing the path
            rn.path = PathBuf::new();
        }
    }
}

// ---------------------------------------------------------------------------
// Keyboard navigation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// What an arrow key does when no item lies strictly in that direction: step
/// through reading order (wrapping rows in the icon views and columns in
/// List); Details/Content only ever move vertically.
fn neighbor_fallback(n: usize, cur: usize, dir: Dir, linear: bool) -> usize {
    match dir {
        Dir::Left | Dir::Up if linear && cur > 0 => cur - 1,
        Dir::Right | Dir::Down if linear && cur + 1 < n => cur + 1,
        Dir::Up if cur > 0 => cur - 1,
        Dir::Down if cur + 1 < n => cur + 1,
        _ => cur,
    }
}

/// Geometric neighbour search: the item whose rectangle is nearest to `cur`'s
/// in the given direction (nearest along the axis, then closest across it).
///
/// Scans every item, so it is only used where the closed forms in
/// [`neighbor`] don't apply, and as the reference those closed forms are
/// tested against.
fn neighbor_scan(layout: &Layout<'_>, cur: usize, dir: Dir) -> usize {
    let c = layout.rect(cur);
    let mut best: Option<(usize, f32, f32)> = None;
    for (i, r) in layout.all_rects() {
        let (primary, secondary) = match dir {
            Dir::Down if r.top() > c.top() + 1.0 => {
                (r.top() - c.top(), (r.center().x - c.center().x).abs())
            }
            Dir::Up if r.top() < c.top() - 1.0 => {
                (c.top() - r.top(), (r.center().x - c.center().x).abs())
            }
            Dir::Right if r.left() > c.left() + 1.0 => {
                (r.left() - c.left(), (r.center().y - c.center().y).abs())
            }
            Dir::Left if r.left() < c.left() - 1.0 => {
                (c.left() - r.left(), (r.center().y - c.center().y).abs())
            }
            _ => continue,
        };
        let better = match best {
            None => true,
            Some((_, bp, bs)) => {
                primary < bp - 0.5 || ((primary - bp).abs() <= 0.5 && secondary < bs)
            }
        };
        if better {
            best = Some((i, primary, secondary));
        }
    }
    match best {
        Some((i, _, _)) => i,
        None => neighbor_fallback(
            layout.total,
            cur,
            dir,
            !matches!(layout.geo, Geo::Rows { .. }),
        ),
    }
}

/// The item to move to from `cur`. Every case is answered by index arithmetic
/// in O(1) (O(groups) when stepping across a group boundary); each closed form
/// reproduces [`neighbor_scan`] exactly (see the differential test).
fn neighbor(layout: &Layout<'_>, cur: usize, dir: Dir) -> usize {
    let n = layout.total;
    let linear = !matches!(layout.geo, Geo::Rows { .. });
    let fallback = || neighbor_fallback(n, cur, dir, linear);
    match layout.geo {
        // One row per item, tops strictly increasing: Up/Down are the
        // adjacent items, Left/Right have no target.
        Geo::Rows { .. } => match dir {
            Dir::Up if cur > 0 => cur - 1,
            Dir::Down if cur + 1 < n => cur + 1,
            _ => cur,
        },
        // Column-major: `per_col` items per column, columns left to right.
        Geo::List { per_col: rows, .. } => {
            let (c, r) = (cur / rows, cur % rows);
            match dir {
                Dir::Up if r >= 1 => cur - 1,
                Dir::Left if c >= 1 => cur - rows,
                // The next row down in this column, or in the previous
                // column when this is the (short) last column's final item.
                Dir::Down if r + 1 < rows && cur + 1 < n => cur + 1,
                Dir::Down if r + 1 < rows && c >= 1 => cur - rows + 1,
                // Next column over, same row, clamped to its last item.
                Dir::Right if (c + 1) * rows < n => (cur + rows).min(n - 1),
                _ => fallback(),
            }
        }
        // Row-major grid, possibly split into several groups stacked
        // vertically; every group has the same column geometry.
        Geo::Grid { cols, .. } => {
            let si = layout.section_of(cur);
            let sec = &layout.sections[si];
            let (i, len) = (cur - sec.start, sec.len);
            let (c, r) = (i % cols, i / cols);
            match dir {
                Dir::Left if c >= 1 => cur - 1,
                Dir::Up if r >= 1 => cur - cols,
                // Previous group: its last row, nearest column.
                Dir::Up => match layout.sections[..si].iter().rev().find(|s| s.len > 0) {
                    Some(p) => {
                        let last = p.len - 1;
                        let (lr, last_row_len) = (last / cols, p.len - (last / cols) * cols);
                        p.start + lr * cols + c.min(last_row_len - 1)
                    }
                    None => fallback(),
                },
                // Next row of this group (clamped to its last item) ...
                Dir::Down if (r + 1) * cols < len => (cur + cols).min(sec.start + len - 1),
                // ... or the first row of the next group, nearest column.
                Dir::Down => match layout.sections[si + 1..].iter().find(|s| s.len > 0) {
                    Some(nx) => nx.start + c.min(nx.len - 1),
                    None => fallback(),
                },
                Dir::Right if c + 1 < cols && i + 1 < len => cur + 1,
                // Last item of a group in a short final row: the best match
                // is the nearest item in column c+1 by height. With a single
                // group that is the cell above-right; across several groups
                // it depends on their layout, so ask the geometry.
                Dir::Right if c + 1 < cols && layout.live > 1 => neighbor_scan(layout, cur, dir),
                Dir::Right if c + 1 < cols && r >= 1 => cur - cols + 1,
                _ => fallback(),
            }
        }
    }
}

fn keyboard(
    ui: &mut Ui,
    st: &ExplorerState,
    layout: &Layout<'_>,
    page_h: f32,
) -> Option<(Act, Rect)> {
    let ctx = ui.ctx().clone();
    // `consume_key` ignores extra Alt, so leave Alt+Arrow / Alt+Enter to the global shortcuts.
    if layout.len() == 0
        || ctx.egui_wants_keyboard_input()
        || crate::dialogs::modal_open(&ctx)
        || ctx.input(|i| i.modifiers.alt)
    {
        return None;
    }
    let entries = &st.entries;
    let path_of = |k: usize| entries[layout.entry_idx(k)].path.clone();

    let (shift, up, down, left, right, home, end, pgup, pgdn) = ctx.input_mut(|i| {
        let mut take = |k: Key| {
            i.consume_key(Modifiers::NONE, k)
                | i.consume_key(Modifiers::SHIFT, k)
                | i.consume_key(Modifiers::CTRL, k)
        };
        let up = take(Key::ArrowUp);
        let down = take(Key::ArrowDown);
        let left = take(Key::ArrowLeft);
        let right = take(Key::ArrowRight);
        let home = take(Key::Home);
        let end = take(Key::End);
        let pgup = take(Key::PageUp);
        let pgdn = take(Key::PageDown);
        (
            i.modifiers.shift,
            up,
            down,
            left,
            right,
            home,
            end,
            pgup,
            pgdn,
        )
    });
    let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));

    // Position of the focused (else first selected) item in display order.
    // Finding it means searching the layout, so it is resolved lazily, at
    // most once, and only on frames where something below actually needs it;
    // a frame with no navigation key never pays for it.
    let mut cur_memo: Option<Option<usize>> = None;
    let mut cur = || -> Option<usize> {
        *cur_memo.get_or_insert_with(|| {
            st.focus
                .as_ref()
                .and_then(|f| layout.position_of(|i| &entries[i].path == f))
                .or_else(|| {
                    st.selection
                        .iter()
                        .next()
                        .and_then(|s| layout.position_of(|i| &entries[i].path == s))
                })
        })
    };

    // Enter opens the selection. Gate this on `cur` (a visible layout item),
    // not just on the selection being non-empty: if search or hidden-file
    // filtering has removed every selected item from view, there is no
    // sensible on-screen target, and opening the stored selection anyway
    // would act on an item the user can no longer see.
    if enter && !st.selection.is_empty() {
        if let Some(c) = cur() {
            return Some((Act::Cmd(Command::Open), layout.rect(c)));
        }
        return None;
    }

    let mut target: Option<usize> = None;
    let n = layout.len();
    let view = st.view;
    let step = |from: usize, dir: Dir, times: usize| {
        let mut k = from;
        for _ in 0..times {
            k = neighbor(layout, k, dir);
        }
        k
    };
    let rows_per_page = {
        let h = match view {
            ViewMode::Details => ROW_H,
            ViewMode::Content => CONTENT_ROW_H,
            ViewMode::List => ROW_H,
            v => grid_for(v).map(|g| g.cell.y).unwrap_or(ROW_H),
        };
        ((page_h / h).floor() as usize).max(1)
    };
    if up || down || left || right || pgup || pgdn {
        if let Some(c) = cur() {
            if up {
                target = Some(step(c, Dir::Up, 1));
            } else if down {
                target = Some(step(c, Dir::Down, 1));
            } else if left && !matches!(view, ViewMode::Details | ViewMode::Content) {
                target = Some(step(c, Dir::Left, 1));
            } else if right && !matches!(view, ViewMode::Details | ViewMode::Content) {
                target = Some(step(c, Dir::Right, 1));
            } else if pgup {
                target = Some(step(c, Dir::Up, rows_per_page));
            } else if pgdn {
                target = Some(step(c, Dir::Down, rows_per_page));
            }
        } else {
            target = Some(0);
        }
    }
    if home {
        target = Some(0);
    } else if end {
        target = Some(n - 1);
    }

    // type-ahead find
    if target.is_none() {
        let typed: String = ctx.input(|i| {
            if i.modifiers.ctrl || i.modifiers.alt || i.modifiers.command {
                return String::new();
            }
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) if !t.trim().is_empty() => Some(t.clone()),
                    _ => None,
                })
                .collect()
        });
        if !typed.is_empty() {
            let now = ctx.input(|i| i.time);
            let id = Id::new("fl_typeahead");
            let (mut buf, last): (String, f64) = ctx.data(|d| d.get_temp(id)).unwrap_or_default();
            if now - last > 1.0 {
                buf.clear();
            }
            buf.push_str(&typed.to_lowercase());
            ctx.data_mut(|d| d.insert_temp(id, (buf.clone(), now)));
            let single_repeat = buf.chars().count() > 1 && buf.chars().all(|c| buf.starts_with(c));
            let needle = if single_repeat {
                buf.chars().next().unwrap().to_string()
            } else {
                buf.clone()
            };
            let start = if needle.chars().count() == 1 {
                cur().map(|c| c + 1).unwrap_or(0)
            } else {
                cur().unwrap_or(0)
            };
            target = layout.find_wrapping(start, |i| entries[i].name_lc.starts_with(&needle));
        }
    }

    let t = target?;
    let path = path_of(t);
    let rect = layout.rect(t);
    if shift && cur().is_some() {
        let anchor_pos = st
            .anchor
            .as_ref()
            .and_then(|a| layout.position_of(|i| &entries[i].path == a))
            .or(cur())
            .unwrap_or(t);
        let (lo, hi) = (anchor_pos.min(t), anchor_pos.max(t));
        let sel: HashSet<PathBuf> = layout
            .entries_in(lo, hi)
            .map(|i| entries[i].path.clone())
            .collect();
        Some((
            Act::Select {
                sel,
                focus: Some(path),
                anchor: None,
            },
            rect,
        ))
    } else {
        Some((
            Act::Select {
                sel: HashSet::from([path.clone()]),
                focus: Some(path.clone()),
                anchor: Some(path),
            },
            rect,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests: the viewport-aware layout and O(1) navigation are checked against an
// independent re-implementation of the original algorithms (full per-item
// layout, allocation-heavy grouping, whole-layout geometric neighbour search).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const VIEWS: [ViewMode; 8] = [
        ViewMode::Details,
        ViewMode::Content,
        ViewMode::List,
        ViewMode::Tiles,
        ViewMode::SmallIcons,
        ViewMode::MediumIcons,
        ViewMode::LargeIcons,
        ViewMode::ExtraLargeIcons,
    ];
    const GROUPS: [GroupKey; 5] = [
        GroupKey::None,
        GroupKey::Name,
        GroupKey::DateModified,
        GroupKey::Type,
        GroupKey::Size,
    ];
    const DIRS: [Dir; 4] = [Dir::Up, Dir::Down, Dir::Left, Dir::Right];

    fn make(n: usize, view: ViewMode, group: GroupKey) -> ExplorerState {
        let now = SystemTime::now();
        let names = [
            "alpha", "Bravo", "charlie", "9lives", "_under", "Echo", "delta",
        ];
        let exts = ["txt", "png", "rs", "mp3", "pdf", "zip", "TXT"];
        let sizes = [
            0u64,
            10,
            20_000,
            2_000_000,
            200_000_000,
            2_000_000_000,
            9_000_000_000,
        ];
        let days = [0u64, 1, 3, 9, 20, 45, 200, 900];
        let mut v = Vec::new();
        for i in 0..n {
            let mut e = if i % 6 == 0 {
                Entry::new(
                    format!("/t/{}{i}", names[i % names.len()]),
                    EntryKind::Folder,
                )
            } else {
                Entry::new(
                    format!("/t/{}{i}.{}", names[i % names.len()], exts[i % exts.len()]),
                    EntryKind::File,
                )
            };
            e.size = sizes[i % sizes.len()];
            if i % 11 != 10 {
                e.modified = Some(now - Duration::from_secs(days[i % days.len()] * 86_400 + 5));
            }
            v.push(e);
        }
        let mut st = ExplorerState::new();
        st.set_location("/t", v);
        st.view = view;
        st.group = group;
        st
    }

    // ---- oracle: the original algorithms, verbatim in spirit ----------------

    fn old_groups(st: &ExplorerState) -> Vec<(String, Vec<usize>)> {
        let order = st.visible_order();
        if st.group == GroupKey::None || st.view == ViewMode::List {
            return vec![(String::new(), order)];
        }
        let now = SystemTime::now();
        let key = st.group;
        let mut v: Vec<(String, usize)> = order
            .into_iter()
            .map(|i| (group_label(&st.entries[i], key, now), i))
            .collect();
        let desc = st.sort_order == SortOrder::Descending;
        v.sort_by(|a, b| {
            let o = group_rank(&a.0, key).cmp(&group_rank(&b.0, key));
            if desc { o.reverse() } else { o }
        });
        let mut out: Vec<(String, Vec<usize>)> = Vec::new();
        for (l, i) in v {
            match out.last_mut() {
                Some((ll, items)) if *ll == l => items.push(i),
                _ => out.push((l, vec![i])),
            }
        }
        out
    }

    struct OldHdr {
        key: String,
        rect: Rect,
        collapsed: bool,
        count: usize,
    }

    struct Old {
        items: Vec<(usize, Rect)>,
        hdrs: Vec<OldHdr>,
        size: Vec2,
    }

    fn old_layout(st: &ExplorerState, groups: &[(String, Vec<usize>)], avail: Vec2) -> Old {
        let grouped = !(groups.len() == 1 && groups[0].0.is_empty());
        let mut items = Vec::new();
        let mut hdrs = Vec::new();
        let mut y = 0.0_f32;
        let size: Vec2;
        match st.view {
            ViewMode::Details | ViewMode::Content => {
                let (row_h, width) = if st.view == ViewMode::Details {
                    (ROW_H, details_width(st))
                } else {
                    (CONTENT_ROW_H, avail.x.max(320.0))
                };
                let span = width.max(avail.x);
                for (label, idxs) in groups {
                    if grouped {
                        let gkey = group_ui_key(st.group, label);
                        let collapsed = st.collapsed_groups.contains(&gkey);
                        hdrs.push(OldHdr {
                            key: gkey,
                            rect: Rect::from_min_size(pos2(0.0, y), vec2(span, GROUP_H)),
                            collapsed,
                            count: idxs.len(),
                        });
                        y += GROUP_H;
                        if collapsed {
                            continue;
                        }
                    }
                    for &i in idxs {
                        items.push((i, Rect::from_min_size(pos2(0.0, y), vec2(width, row_h))));
                        y += row_h;
                    }
                }
                size = vec2(width, y);
            }
            ViewMode::List => {
                let idxs = &groups[0].1;
                let longest = idxs
                    .iter()
                    .map(|&i| {
                        st.entries[i]
                            .display_name(st.show_extensions)
                            .chars()
                            .count()
                    })
                    .max()
                    .unwrap_or(10);
                let col_w = (longest as f32 * 6.8 + 36.0).clamp(130.0, 320.0);
                let per_col = (((avail.y - 4.0) / ROW_H).floor() as usize).max(1);
                for (n, &i) in idxs.iter().enumerate() {
                    let (c, r) = (n / per_col, n % per_col);
                    items.push((
                        i,
                        Rect::from_min_size(
                            pos2(4.0 + c as f32 * col_w, 2.0 + r as f32 * ROW_H),
                            vec2(col_w, ROW_H),
                        ),
                    ));
                }
                size = vec2(4.0 + idxs.len().div_ceil(per_col) as f32 * col_w, avail.y);
            }
            v => {
                let g = grid_for(v).unwrap();
                let cols = (((avail.x - 8.0) / g.cell.x).floor() as usize).max(1);
                for (label, idxs) in groups {
                    if grouped {
                        let gkey = group_ui_key(st.group, label);
                        let collapsed = st.collapsed_groups.contains(&gkey);
                        hdrs.push(OldHdr {
                            key: gkey,
                            rect: Rect::from_min_size(
                                pos2(0.0, y),
                                vec2(avail.x.max(g.cell.x), GROUP_H),
                            ),
                            collapsed,
                            count: idxs.len(),
                        });
                        y += GROUP_H;
                        if collapsed {
                            continue;
                        }
                    }
                    for (n, &i) in idxs.iter().enumerate() {
                        let (c, r) = (n % cols, n / cols);
                        items.push((
                            i,
                            Rect::from_min_size(
                                pos2(4.0 + c as f32 * g.cell.x, y + 4.0 + r as f32 * g.cell.y),
                                g.cell,
                            ),
                        ));
                    }
                    y += 4.0 + idxs.len().div_ceil(cols) as f32 * g.cell.y + 8.0;
                }
                size = vec2(avail.x, y);
            }
        }
        Old { items, hdrs, size }
    }

    fn old_neighbor(items: &[(usize, Rect)], cur: usize, dir: Dir, view: ViewMode) -> usize {
        let c = items[cur].1;
        let mut best: Option<(usize, f32, f32)> = None;
        for (i, (_, r)) in items.iter().enumerate() {
            let r = *r;
            let (primary, secondary) = match dir {
                Dir::Down if r.top() > c.top() + 1.0 => {
                    (r.top() - c.top(), (r.center().x - c.center().x).abs())
                }
                Dir::Up if r.top() < c.top() - 1.0 => {
                    (c.top() - r.top(), (r.center().x - c.center().x).abs())
                }
                Dir::Right if r.left() > c.left() + 1.0 => {
                    (r.left() - c.left(), (r.center().y - c.center().y).abs())
                }
                Dir::Left if r.left() < c.left() - 1.0 => {
                    (c.left() - r.left(), (r.center().y - c.center().y).abs())
                }
                _ => continue,
            };
            let better = match best {
                None => true,
                Some((_, bp, bs)) => {
                    primary < bp - 0.5 || ((primary - bp).abs() <= 0.5 && secondary < bs)
                }
            };
            if better {
                best = Some((i, primary, secondary));
            }
        }
        if let Some((i, _, _)) = best {
            return i;
        }
        let linear = !matches!(view, ViewMode::Details | ViewMode::Content);
        match dir {
            Dir::Left | Dir::Up if linear && cur > 0 => cur - 1,
            Dir::Right | Dir::Down if linear && cur + 1 < items.len() => cur + 1,
            Dir::Up if cur > 0 => cur - 1,
            Dir::Down if cur + 1 < items.len() => cur + 1,
            _ => cur,
        }
    }

    fn close(a: Rect, b: Rect) -> bool {
        (a.min - b.min).abs().max_elem() < 0.05 && (a.max - b.max).abs().max_elem() < 0.05
    }

    const NS: [usize; 11] = [0, 1, 2, 3, 7, 8, 9, 25, 64, 100, 151];
    const AVAILS: [(f32, f32); 5] = [
        (700.0, 400.0),
        (300.0, 200.0),
        (1000.0, 150.0),
        (130.0, 90.0),
        (40.0, 30.0),
    ];

    /// Collapse none / every other / all groups.
    fn collapse(st: &mut ExplorerState, mode: usize) {
        st.collapsed_groups.clear();
        let set = st.group_set();
        if !set.grouped() {
            return;
        }
        for (gi, g) in set.groups.iter().enumerate() {
            if mode == 2 || (mode == 1 && gi % 2 == 0) {
                st.collapsed_groups.insert(g.ukey.clone());
            }
        }
    }

    #[test]
    fn grouping_matches_original_algorithm() {
        for &n in &NS {
            for g in GROUPS {
                for v in VIEWS {
                    for desc in [false, true] {
                        let mut st = make(n, v, g);
                        if desc {
                            st.sort_order = SortOrder::Descending;
                        }
                        let set = st.group_set();
                        let old = old_groups(&st);
                        assert_eq!(set.groups.len(), old.len(), "n={n} {g:?} {v:?}");
                        for (gi, (label, idxs)) in old.iter().enumerate() {
                            let ng = &set.groups[gi];
                            assert_eq!(&ng.label, label);
                            assert_eq!(
                                &set.flat[ng.off..ng.off + ng.len],
                                idxs.as_slice(),
                                "n={n} {g:?} {v:?} g{gi}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn layout_matches_original_in_every_view() {
        for &n in &NS {
            for g in GROUPS {
                for v in VIEWS {
                    for cm in 0..3 {
                        let mut st = make(n, v, g);
                        collapse(&mut st, cm);
                        let set = st.group_set();
                        let old_g = old_groups(&st);
                        for (ax, ay) in AVAILS {
                            let avail = vec2(ax, ay);
                            let old = old_layout(&st, &old_g, avail);
                            let lay = build_layout(&st, &set, avail);
                            let ctx = format!("n={n} {g:?} {v:?} collapse={cm} avail={avail:?}");
                            assert_eq!(lay.len(), old.items.len(), "{ctx}");
                            assert!(
                                (lay.size - old.size).abs().max_elem() < 0.05,
                                "size {ctx}: {:?} vs {:?}",
                                lay.size,
                                old.size
                            );
                            assert_eq!(lay.has_headers(), !old.hdrs.is_empty(), "{ctx}");
                            for (k, (idx, rect)) in old.items.iter().enumerate() {
                                assert_eq!(lay.entry_idx(k), *idx, "{ctx} k={k}");
                                assert!(
                                    close(lay.rect(k), *rect),
                                    "{ctx} k={k}: {:?} vs {:?}",
                                    lay.rect(k),
                                    rect
                                );
                            }
                            // headers: visit with a huge area must list them all, in order
                            let mut hs = Vec::new();
                            lay.visit_headers(Rect::EVERYTHING, &mut |s, r| {
                                hs.push((s.gi, s.collapsed, r))
                            });
                            assert_eq!(hs.len(), old.hdrs.len(), "{ctx}");
                            for (h, o) in hs.iter().zip(&old.hdrs) {
                                assert_eq!(set.groups[h.0].ukey, o.key);
                                assert_eq!(h.1, o.collapsed);
                                assert_eq!(set.groups[h.0].len, o.count);
                                assert!(close(h.2, o.rect), "{ctx}: hdr {:?} vs {:?}", h.2, o.rect);
                            }
                            // viewport queries: scrolled windows, thin slices and
                            // far-away areas must select exactly what a full filter does
                            let h = old.size.y.max(old.size.x).max(1.0);
                            let mut areas = vec![
                                Rect::EVERYTHING,
                                Rect::from_min_size(pos2(-500.0, -500.0), vec2(1.0, 1.0)),
                            ];
                            let mut off = -10.0;
                            while off < h + 50.0 {
                                areas.push(Rect::from_min_size(
                                    pos2(0.0, off),
                                    vec2(ax + 8.0, ay + 8.0),
                                ));
                                areas.push(Rect::from_min_size(
                                    pos2(off, 0.0),
                                    vec2(ax + 8.0, ay + 8.0),
                                ));
                                areas.push(Rect::from_min_size(
                                    pos2(off * 0.3, off),
                                    vec2(37.0, 11.0),
                                ));
                                off += (ay * 0.37).max(7.0);
                            }
                            for a in areas {
                                let want: Vec<(usize, usize)> = old
                                    .items
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, (_, r))| r.intersects(a))
                                    .map(|(k, (i, _))| (k, *i))
                                    .collect();
                                let mut got = Vec::new();
                                lay.visit_in(a, &mut |k, i, r| {
                                    assert!(close(r, old.items[k].1), "{ctx} rect k={k}");
                                    got.push((k, i));
                                });
                                assert_eq!(got, want, "{ctx} area={a:?}");
                                let mut hw: Vec<usize> = old
                                    .hdrs
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, o)| o.rect.intersects(a))
                                    .map(|(i, _)| i)
                                    .collect();
                                let mut hg = Vec::new();
                                lay.visit_headers(a, &mut |s, _| hg.push(s.gi));
                                hw.sort();
                                assert_eq!(hg, hw, "{ctx} header area={a:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn neighbor_matches_original_geometric_search() {
        let mut checked = 0usize;
        for &n in &NS {
            for g in GROUPS {
                for v in VIEWS {
                    for cm in 0..3 {
                        let mut st = make(n, v, g);
                        collapse(&mut st, cm);
                        let set = st.group_set();
                        let old_g = old_groups(&st);
                        for (ax, ay) in AVAILS {
                            let avail = vec2(ax, ay);
                            let old = old_layout(&st, &old_g, avail);
                            let lay = build_layout(&st, &set, avail);
                            for cur in 0..lay.len() {
                                for d in DIRS {
                                    let want = old_neighbor(&old.items, cur, d, v);
                                    let got = neighbor(&lay, cur, d);
                                    assert_eq!(
                                        got,
                                        want,
                                        "n={n} {g:?} {v:?} collapse={cm} avail={avail:?} cur={cur} dir={}",
                                        match d {
                                            Dir::Up => "up",
                                            Dir::Down => "down",
                                            Dir::Left => "left",
                                            Dir::Right => "right",
                                        }
                                    );
                                    assert_eq!(neighbor_scan(&lay, cur, d), want);
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            checked > 100_000,
            "test should be exhaustive, checked {checked}"
        );
    }

    #[test]
    fn lookup_helpers() {
        let mut st = make(40, ViewMode::MediumIcons, GroupKey::Type);
        collapse(&mut st, 1);
        let set = st.group_set();
        let lay = build_layout(&st, &set, vec2(500.0, 300.0));
        let all: Vec<usize> = (0..lay.len()).map(|k| lay.entry_idx(k)).collect();
        assert_eq!(lay.entries_in(0, lay.len() - 1).collect::<Vec<_>>(), all);
        for (lo, hi) in [
            (0, 0),
            (3, 9),
            (lay.len() - 1, lay.len() - 1),
            (5, lay.len() - 1),
        ] {
            assert_eq!(lay.entries_in(lo, hi).collect::<Vec<_>>(), all[lo..=hi]);
        }
        for k in 0..lay.len() {
            assert_eq!(lay.position_of(|i| i == all[k]), Some(k));
        }
        for start in [0, 1, lay.len() - 1, lay.len(), lay.len() + 3] {
            let t = all[(start + 2) % lay.len()];
            let want = (0..lay.len())
                .map(|o| (start + o) % lay.len())
                .find(|&k| all[k] == t);
            assert_eq!(lay.find_wrapping(start, |i| i == t), want);
        }
        assert_eq!(lay.find_wrapping(0, |_| false), None);
    }
}
