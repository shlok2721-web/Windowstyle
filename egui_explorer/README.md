# egui_explorer

A **Windows 10 File Explorer (ribbon style)** widget set for [egui](https://github.com/emilk/egui),
written in pure Rust, meant as the UI layer of a Linux file manager.

The crate is **UI only**: it never touches the disk. You feed it entries and it hands
back `Event`s (navigate, open, rename, paste, ...). No `unsafe`, no native dependencies.

```rust
let mut explorer = egui_explorer::Explorer::new();
let mut state = egui_explorer::ExplorerState::new();
egui_explorer::theme::apply(&ctx);                // once

// every frame (inside eframe::App::ui):
for ev in explorer.show(ui, &mut state) {
    match ev {
        Event::Navigate(p) => state.set_location(&p, my_read_dir(&p)),
        Event::Open(p)     => open_with_xdg(p),
        Event::Command { cmd, targets } => my_handle_command(cmd, targets),
        Event::Rename { path, new_name } => std::fs::rename(...).ok().map(|_| refresh()),
        Event::Drop { sources, target } => move_files(sources, target),
        Event::LoadChildren(p) => state.set_nav_children(&p, subfolders(&p)),
        Event::Search(q) => ...,
    };
}
```

Run the demo (browses your real home directory):

```
cargo run --example demo                  # native window decoration
cargo run --example demo -- --frameless   # our own title bar + caption buttons
cargo run --example demo -- --ribbon=hidden --view=tiles --group=type --pane=details
```

## Ribbon display modes ("hidden ribbon" etc.)

`state.ribbon_mode`:

| Mode | Behaviour | How to switch |
|---|---|---|
| `Expanded` | Tabs + commands | default |
| `Minimized` | Tabs only; clicking a tab shows commands as an overlay, Esc/click-away closes | `Ctrl+F1`, chevron `^` at the right of the tab strip, double-click a tab |
| `Hidden` | Nothing at all (auto-hide); overlay appears on `Alt`, click on the title bar, or via the menu | title-bar "Ribbon Display Options" button (Auto-hide / Show tabs / Show tabs and commands) |

Also: Quick Access Toolbar above or below the ribbon (customisable drop-down),
Alt **key tips** (`Alt` -> `H`/`S`/`V`/`F`), groups that collapse into a single drop-down button when the window is narrow.

## Windows 10 element -> crate mapping

| Explorer element | Where |
|---|---|
| Title bar, app icon, centred folder title, min/max/close, drag, double-click maximise, edge resize | `shell::title_bar`, `ExplorerOptions` |
| Quick Access Toolbar + "Customize Quick Access Toolbar" menu | `shell::qat` |
| Blue **File** tab menu (Open new window, Open terminal here (PowerShell on Win), Delete history, Help, Close, Frequent places) | `ribbon::show_file_menu` |
| **Home** tab: Clipboard (Pin, Copy, Paste split, Cut, Copy path, Paste shortcut), Organize (Move to, Copy to, Delete split, Rename), New (New folder, New item, Easy access), Open (Properties, Open split, Edit, History), Select (all/none/invert) | `Ribbon::windows10()` |
| **Share** tab: Share, E-mail, Zip, Print, Advanced security | same |
| **View** tab: Panes (Navigation/Preview/Details), **Layout gallery** (8 views, scroll arrows, expand), Current view (Sort by, Group by, Add columns, Size all columns), Show/hide (Item check boxes, File name extensions, Hidden items, Hide selected items), Options | same |
| Contextual tabs **Drive / Picture / Application / Compressed / Media Tools** with coloured header | `Ribbon::context_for` |
| Back / Forward / Recent locations / Up | `shell::nav_row` |
| Breadcrumb address bar: clickable segments, `>` sub-folder drop-downs, `<<` overflow, click-to-edit path, history drop-down, refresh | `shell::address_bar` |
| Search box (live filter + `Search` event) | `shell::search_box` |
| Navigation pane tree (Quick access, This PC, ...) with lazy loading, pin overlay, hover-only chevrons, drop targets, context menu | `shell::nav_pane`, `NavNode` |
| Item pane: **Details** (resizable/sortable columns, column chooser), **List**, **Small/Medium/Large/Extra-large icons**, **Tiles** (drive usage bar), **Content** | `file_list` |
| Grouping with collapsible headers (Name, Date modified buckets, Type, Size buckets) | `file_list::build_groups` |
| Selection: click, Ctrl, Shift, rubber band, check boxes, Select all/invert | `file_list` |
| Inline rename (F2, stem pre-selected), type-ahead find, arrow/Home/End/PageUp/PageDown | `file_list` |
| Drag & drop of items onto folders / nav pane | `Event::Drop` |
| Cut items and hidden items drawn faded | `file_list::paint_item` |
| Context menus (item, empty space, column header, nav node) | `file_list`, `shell` |
| Preview pane / Details pane | `shell::side_pane` |
| Status bar: item count, selection count + size, view switcher | `shell::status_bar` |
| Keyboard shortcuts: Ctrl+A/C/X/V/Z/Y, Del, Shift+Del, F2, F5, F1, Alt+Enter, Alt+Left/Right/Up, Backspace, Ctrl+L / Alt+D / F4, Ctrl+E / Ctrl+F / F3, Ctrl+Shift+N, **Ctrl+Shift+C (copy path)**, Ctrl+Alt+C (copy name), Ctrl+Alt+Shift+C (copy folder path), Ctrl+Shift+V (paste shortcut), Ctrl+D (delete), Ctrl+I / Ctrl+Shift+A (invert / clear selection), Ctrl+Enter (open), Ctrl+Shift+T (new text file), Ctrl+Alt+T (terminal), Ctrl+H / Ctrl+Shift+H (hidden items / extensions), Alt+N (nav pane), Ctrl+Shift+1..8 (views), Alt+P / Alt+Shift+P (panes), Ctrl+F1 | `shell::global_keys` |
| Icons: 100+ vector icons drawn with the egui painter, scale to any size (no image assets) | `icons` |
| Light/dark palette and shared UI colours | `theme` |

## Integration notes and known gaps

The reusable UI supports thumbnail textures through `Entry::thumbnail`; the root `fm_app` host supplies on-demand thumbnail generation, cache audits, and explicit missing-thumbnail generation. Dark mode is controlled through `ExplorerState::dark_mode` and updates the interface palette without tinting media content.

* Column re-ordering by drag, address bar auto-complete, and "Ctrl + mouse wheel" icon zoom are not implemented.
* Nav-pane horizontal scrolling is not implemented (long labels are clipped).
* Strings are English only.

Headless UI smoke tests in `tests/smoke.rs` exercise ribbon modes, views, grouping, and title-bar configurations. Run them from the repository root with `cargo test -p egui_explorer`.
