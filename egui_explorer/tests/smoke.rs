//! Headless tests: run real egui frames through the whole UI in every
//! configuration and check the state machine behind it.

use std::time::{Duration, UNIX_EPOCH};

use egui::{Context, Event as InEvent, Modifiers, PointerButton, RawInput, Rect, pos2, vec2};
use egui_explorer::ribbon::{ContextKind, Ribbon};
use egui_explorer::*;

fn entries() -> Vec<Entry> {
    let mut v = Vec::new();
    for i in 0..12 {
        let mut e = Entry::new(format!("/home/u/Folder {i}"), EntryKind::Folder);
        e.modified = Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i * 86_400 * 9));
        v.push(e);
    }
    for (i, name) in [
        "report.pdf",
        "photo.png",
        "song.mp3",
        "movie.mkv",
        "backup.tar.gz",
        "main.rs",
        "run.AppImage",
        "notes.txt",
        ".hidden",
        "file10.txt",
        "file2.txt",
        "README",
        "a very long file name that has to be truncated with an ellipsis.docx",
    ]
    .into_iter()
    .enumerate()
    {
        let mut e = Entry::new(format!("/home/u/{name}"), EntryKind::File);
        e.size = 1000 * (i as u64 + 1).pow(3);
        e.modified = Some(UNIX_EPOCH + Duration::from_secs(1_760_000_000 - i as u64 * 40_000));
        v.push(e);
    }
    let mut d = Entry::new("/mnt/data", EntryKind::Drive);
    d.space = Some((500_000_000_000, 120_000_000_000));
    v.push(d);
    v
}

fn make_state() -> ExplorerState {
    let mut s = ExplorerState::new();
    s.nav = vec![
        NavNode::new("quick", "Quick access", Icon::QuickAccess)
            .with_children(vec![
                NavNode::new("/home/u/Desktop", "Desktop", Icon::FolderDesktop).pinned(),
            ])
            .expanded(),
        NavNode::new("/", "This PC", Icon::ThisPc)
            .with_children(vec![NavNode::new("/home", "home", Icon::Folder)])
            .expanded(),
    ];
    s.subfolders_fn = Some(|_| vec!["/a".into(), "/b".into()]);
    s.set_location("/home/u", entries());
    s
}

fn run(
    ctx: &Context,
    ex: &mut Explorer,
    st: &mut ExplorerState,
    events: Vec<InEvent>,
    t: f64,
) -> Vec<Event> {
    run_mod(ctx, ex, st, events, t, Modifiers::NONE)
}

fn run_mod(
    ctx: &Context,
    ex: &mut Explorer,
    st: &mut ExplorerState,
    events: Vec<InEvent>,
    t: f64,
    modifiers: Modifiers,
) -> Vec<Event> {
    let mut out = Vec::new();
    let mut all = vec![InEvent::ModifiersChanged(modifiers)];
    all.extend(events);
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))),
        time: Some(t),
        events: all,
        ..Default::default()
    };
    let mut full = ctx.run_ui(input, |ui| {
        out = ex.show(ui, st);
    });
    full.textures_delta.clear();
    // tessellating exercises every painted shape
    let _ = ctx.tessellate(full.shapes, full.pixels_per_point);
    out
}

fn frames(ctx: &Context, ex: &mut Explorer, st: &mut ExplorerState, n: usize) {
    for i in 0..n {
        run(ctx, ex, st, vec![], i as f64 * 0.016);
    }
}

#[test]
fn every_configuration_renders() {
    let ctx = Context::default();
    theme::apply(&ctx);
    for mode in [
        RibbonMode::Expanded,
        RibbonMode::Minimized,
        RibbonMode::Hidden,
    ] {
        for view in ViewMode::ALL {
            for group in GroupKey::ALL {
                for title_bar in [true, false] {
                    let mut ex = Explorer::new();
                    ex.options.title_bar = title_bar;
                    let mut st = make_state();
                    st.ribbon_mode = mode;
                    st.view = view;
                    st.group = group;
                    st.show_preview_pane = view == ViewMode::Tiles;
                    st.show_details_pane = view == ViewMode::List;
                    st.item_check_boxes = view == ViewMode::Content;
                    st.qat_below_ribbon = group == GroupKey::Size;
                    st.select_only(std::path::Path::new("/home/u/photo.png"));
                    frames(&ctx, &mut ex, &mut st, 3);
                    st.ribbon_overlay = true;
                    frames(&ctx, &mut ex, &mut st, 2);
                }
            }
        }
    }
}

#[test]
fn every_tab_renders_in_narrow_window() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut ex = Explorer::new();
    let mut st = make_state();
    for tab in ["home", "share", "view"] {
        st.active_tab = tab.into();
        frames(&ctx, &mut ex, &mut st, 2);
    }
    // contextual tabs
    for (path, kind) in [
        ("/home/u/photo.png", ContextKind::Picture),
        ("/home/u/backup.tar.gz", ContextKind::Archive),
        ("/home/u/run.AppImage", ContextKind::Application),
        ("/home/u/song.mp3", ContextKind::Media),
        ("/mnt/data", ContextKind::Drive),
    ] {
        st.select_only(std::path::Path::new(path));
        assert_eq!(Ribbon::context_for(&st), Some(kind), "{path}");
        st.active_tab = Ribbon::windows10()
            .tabs
            .iter()
            .find(|t| t.context == Some(kind))
            .unwrap()
            .id
            .into();
        frames(&ctx, &mut ex, &mut st, 2);
    }
    st.selection.clear();
    assert_eq!(Ribbon::context_for(&st), None);
    frames(&ctx, &mut ex, &mut st, 2);
    assert_eq!(
        st.active_tab, "home",
        "falls back when the contextual tab disappears"
    );
}

#[test]
fn history_and_navigation() {
    let mut st = ExplorerState::new();
    st.set_location("/a", vec![]);
    st.set_location("/a/b", vec![]);
    st.set_location("/a/b/c", vec![]);
    assert!(st.can_go_back() && !st.can_go_forward());
    st.go_back();
    assert!(matches!(st.take_events().as_slice(), [Event::Navigate(p)] if p.ends_with("a/b")));
    st.set_location("/a/b", vec![]); // host answers the navigate event
    assert!(st.can_go_forward());
    st.go_up();
    assert!(matches!(st.take_events().as_slice(), [Event::Navigate(p)] if p.ends_with("a")));
    // navigating somewhere new truncates the forward history
    st.set_location("/z", vec![]);
    assert!(!st.can_go_forward());
}

#[test]
fn selection_commands_and_enablement() {
    let mut st = make_state();
    assert!(!st.is_enabled(Command::Copy), "nothing selected");
    // Paste stays enabled even with an empty in-app clipboard: the host falls back to
    // reading the system clipboard (e.g. files copied externally with `wl-copy`), which
    // this crate can't see, so it can't be the thing gating enablement.
    assert!(st.is_enabled(Command::Paste), "paste always enabled");
    st.dispatch(Command::SelectAll);
    assert_eq!(
        st.selection.len(),
        st.visible_count(),
        "hidden file stays hidden"
    );
    assert_eq!(st.visible_count(), 25);
    st.dispatch(Command::InvertSelection);
    assert!(st.selection.is_empty());
    st.dispatch(Command::ToggleHiddenItems);
    assert_eq!(st.visible_count(), 26);
    st.select_only(std::path::Path::new("/home/u/notes.txt"));
    assert!(st.is_enabled(Command::Rename));
    st.dispatch(Command::Copy);
    assert!(st.clipboard_has_items && st.is_enabled(Command::Paste));
    st.take_events();
    st.dispatch(Command::Rename);
    assert_eq!(st.renaming.as_ref().unwrap().text, "notes.txt");
    st.dispatch(Command::Open);
    assert!(matches!(st.take_events().as_slice(), [Event::Open(_)]));
    st.dispatch(Command::SetView(ViewMode::Tiles));
    assert_eq!(st.view, ViewMode::Tiles);
    st.dispatch(Command::TogglePreviewPane);
    st.dispatch(Command::ToggleDetailsPane);
    assert!(
        st.show_details_pane && !st.show_preview_pane,
        "panes are mutually exclusive"
    );
}

#[test]
fn sorting_is_natural_and_folders_first() {
    let mut st = make_state();
    st.show_hidden = true;
    let names: Vec<_> = st
        .visible_order()
        .into_iter()
        .map(|i| st.entries[i].name.clone())
        .collect();
    let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
    assert!(pos("file2.txt") < pos("file10.txt"), "natural order");
    assert!(pos("Folder 11") < pos("report.pdf"), "folders first");
    st.sort_order = SortOrder::Descending;
    let names: Vec<_> = st
        .visible_order()
        .into_iter()
        .map(|i| st.entries[i].name.clone())
        .collect();
    assert!(
        names.iter().position(|x| x == "file10.txt") < names.iter().position(|x| x == "file2.txt")
    );
    st.sort_key = SortKey::Size;
    st.sort_order = SortOrder::Ascending;
    st.visible_order();
}

#[test]
fn formatting_helpers() {
    assert_eq!(format_size(0), "0 KB");
    assert_eq!(format_size(1), "1 KB");
    assert_eq!(format_size(1500), "2 KB");
    assert_eq!(format_size(5 * 1024 * 1024), "5.00 MB");
    assert_eq!(format_size(1536 * 1024 * 1024), "1.50 GB");
    assert_eq!(format_date_utc(UNIX_EPOCH), "1/1/1970 12:00 AM");
    assert_eq!(
        format_date_utc(UNIX_EPOCH + Duration::from_secs(1_760_000_000)),
        "10/9/2025 8:53 AM"
    );
    assert_eq!(natural_cmp("a2", "a10"), std::cmp::Ordering::Less);
    assert_eq!(natural_cmp("B", "a"), std::cmp::Ordering::Greater);
    assert_eq!(extension("x.tar.gz"), Some("gz"));
    assert_eq!(extension(".bashrc"), None);
    let e = Entry::new("/x/photo.png", EntryKind::File);
    assert_eq!(e.display_name(false), "photo");
    assert_eq!(e.display_name(true), "photo.png");
}

#[test]
fn ribbon_modes_cycle_and_ctrl_f1() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut ex = Explorer::new();
    let mut st = make_state();
    assert_eq!(st.ribbon_mode, RibbonMode::Expanded);
    let ctrl = Modifiers {
        ctrl: true,
        command: true,
        ..Default::default()
    };
    let key = |pressed| InEvent::Key {
        key: egui::Key::F1,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: ctrl,
    };
    run(&ctx, &mut ex, &mut st, vec![], 0.0);
    run_mod(
        &ctx,
        &mut ex,
        &mut st,
        vec![key(true), key(false)],
        0.1,
        ctrl,
    );
    assert_eq!(st.ribbon_mode, RibbonMode::Minimized);
    run_mod(
        &ctx,
        &mut ex,
        &mut st,
        vec![key(true), key(false)],
        0.2,
        ctrl,
    );
    assert_eq!(st.ribbon_mode, RibbonMode::Expanded);
    st.dispatch(Command::SetRibbonMode(RibbonMode::Hidden));
    frames(&ctx, &mut ex, &mut st, 2);
    assert_eq!(st.ribbon_mode, RibbonMode::Hidden);
}

#[test]
fn mouse_click_selects_and_double_click_navigates() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut ex = Explorer::new();
    ex.options.title_bar = false;
    let mut st = make_state();
    frames(&ctx, &mut ex, &mut st, 3);
    // Find the first item row: it is the first folder ("Folder 0") in Details view.
    // Scan down the file-list area until a click selects something.
    let mut hit = None;
    'scan: for y in (150..700).step_by(6) {
        let p = pos2(500.0, y as f32);
        let click = |pressed| InEvent::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        run(
            &ctx,
            &mut ex,
            &mut st,
            vec![InEvent::PointerMoved(p)],
            1.0 + y as f64,
        );
        run(&ctx, &mut ex, &mut st, vec![click(true)], 1.01 + y as f64);
        run(&ctx, &mut ex, &mut st, vec![click(false)], 1.02 + y as f64);
        run(&ctx, &mut ex, &mut st, vec![], 1.03 + y as f64);
        if !st.selection.is_empty() {
            hit = Some(p);
            break 'scan;
        }
    }
    let p = hit.expect("a click inside the item pane must select an item");
    assert_eq!(st.selection.len(), 1);
    // double click
    let click = |pressed| InEvent::PointerButton {
        pos: p,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    let mut evs = Vec::new();
    for (i, pressed) in [true, false, true, false].into_iter().enumerate() {
        evs.extend(run(
            &ctx,
            &mut ex,
            &mut st,
            vec![click(pressed)],
            5000.0 + i as f64 * 0.05,
        ));
    }
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::Navigate(_) | Event::Open(_))),
        "double click must open the item, got {evs:?}"
    );
}
