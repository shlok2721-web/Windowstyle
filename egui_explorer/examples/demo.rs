//! Demo: a Windows 10 style explorer browsing the real Linux filesystem.
//!
//!     cargo run --example demo                # native window decorations
//!     cargo run --example demo -- --frameless # draw our own title bar + caption buttons
//!     cargo run --example demo -- --ribbon=hidden|minimized|expanded

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Proc;

use egui_explorer::{
    Command, Entry, EntryKind, Event, Explorer, ExplorerState, Icon, NavNode, RibbonMode, theme,
};

fn main() -> eframe::Result {
    let frameless = std::env::args().any(|a| a == "--frameless");
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 720.0])
        .with_min_inner_size([640.0, 420.0])
        .with_title("File Explorer");
    if frameless {
        viewport = viewport.with_decorations(false);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "egui_explorer demo",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new(frameless)))
        }),
    )
}

struct App {
    explorer: Explorer,
    state: ExplorerState,
    /// (paths, is_cut)
    clipboard: Option<(Vec<PathBuf>, bool)>,
    status: String,
}

impl App {
    fn new(frameless: bool) -> Self {
        let mut explorer = Explorer::new();
        explorer.options.title_bar = frameless;
        let mut state = ExplorerState::new();
        state.subfolders_fn = Some(subfolders);
        if let Some(arg) =
            std::env::args().find_map(|a| a.strip_prefix("--ribbon=").map(str::to_owned))
        {
            state.ribbon_mode = match arg.as_str() {
                "hidden" => RibbonMode::Hidden,
                "minimized" => RibbonMode::Minimized,
                _ => RibbonMode::Expanded,
            };
        }
        let arg = |k: &str| std::env::args().find_map(|a| a.strip_prefix(k).map(str::to_owned));
        if let Some(v) = arg("--view=") {
            state.view = match v.as_str() {
                "xl" => egui_explorer::ViewMode::ExtraLargeIcons,
                "large" => egui_explorer::ViewMode::LargeIcons,
                "medium" => egui_explorer::ViewMode::MediumIcons,
                "small" => egui_explorer::ViewMode::SmallIcons,
                "list" => egui_explorer::ViewMode::List,
                "tiles" => egui_explorer::ViewMode::Tiles,
                "content" => egui_explorer::ViewMode::Content,
                _ => egui_explorer::ViewMode::Details,
            };
        }
        if let Some(g) = arg("--group=") {
            state.group = match g.as_str() {
                "type" => egui_explorer::GroupKey::Type,
                "name" => egui_explorer::GroupKey::Name,
                "size" => egui_explorer::GroupKey::Size,
                "date" => egui_explorer::GroupKey::DateModified,
                _ => egui_explorer::GroupKey::None,
            };
        }
        match arg("--pane=").as_deref() {
            Some("details") => state.show_details_pane = true,
            Some("preview") => state.show_preview_pane = true,
            _ => {}
        }
        if let Some(t) = arg("--tab=") {
            state.active_tab = t;
        }
        state.nav = build_nav();
        let start = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let entries = load_dir(&start);
        state.set_location(start, entries);
        if std::env::args().any(|a| a == "--select-some") {
            let picks: Vec<PathBuf> = state
                .entries
                .iter()
                .filter(|e| !e.is_dir())
                .take(2)
                .map(|e| e.path.clone())
                .collect();
            for p in picks {
                state.selection.insert(p.clone());
                state.focus = Some(p);
            }
        }
        Self {
            explorer,
            state,
            clipboard: None,
            status: String::new(),
        }
    }

    fn navigate(&mut self, path: PathBuf) {
        if path.is_dir() {
            let entries = load_dir(&path);
            self.state.set_location(path, entries);
        } else {
            self.status = format!("Cannot find '{}'.", path.display());
        }
    }

    fn reload(&mut self) {
        let entries = load_dir(&self.state.location);
        self.state.set_entries(entries);
    }

    fn handle(&mut self, ctx: &egui::Context, ev: Event) {
        match ev {
            Event::Navigate(p) => self.navigate(p),
            Event::Open(p) => {
                spawn_reaped(Proc::new("xdg-open").arg(&p));
            }
            Event::LoadChildren(p) => {
                let kids = subfolders(&p)
                    .into_iter()
                    .filter(|c| {
                        !c.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                    })
                    .map(|c| {
                        let name = c
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        NavNode::new(c, name, Icon::Folder)
                    })
                    .collect();
                self.state.set_nav_children(&p, kids);
            }
            Event::Search(q) => {
                self.status = format!("Search for '{q}' (filtering current folder)")
            }
            Event::ThumbnailNeeded(_) => {}
            Event::Rename { path, new_name } => {
                let to = path.with_file_name(&new_name);
                match fs::rename(&path, &to) {
                    Ok(()) => self.reload(),
                    Err(e) => self.status = format!("Rename failed: {e}"),
                }
            }
            Event::Drop { sources, target } => {
                for s in sources {
                    if let Some(name) = s.file_name() {
                        let _ = fs::rename(&s, target.join(name));
                    }
                }
                self.reload();
            }
            Event::Command { cmd, targets } => self.command(ctx, cmd, targets),
        }
    }

    fn command(&mut self, ctx: &egui::Context, cmd: Command, targets: Vec<PathBuf>) {
        let here = self.state.location.clone();
        match cmd {
            Command::Copy => self.clipboard = Some((targets, false)),
            Command::Cut => self.clipboard = Some((targets, true)),
            Command::Paste => {
                if let Some((paths, cut)) = self.clipboard.take() {
                    for p in &paths {
                        let Some(name) = p.file_name() else { continue };
                        let dest = unique(&here.join(name));
                        let r = if cut {
                            fs::rename(p, &dest)
                        } else {
                            copy_recursive(p, &dest)
                        };
                        if let Err(e) = r {
                            self.status = format!("Paste failed: {e}");
                        }
                    }
                    if !cut {
                        self.clipboard = Some((paths, false));
                    }
                    self.state.cut.clear();
                    self.state.clipboard_has_items = self.clipboard.is_some();
                    self.reload();
                }
            }
            Command::CopyPath => {
                let text = targets
                    .iter()
                    .map(|p| format!("\"{}\"", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.copy_text(text);
            }
            Command::NewFolder => {
                let _ = fs::create_dir(unique(&here.join("New folder")));
                self.reload();
            }
            Command::NewTextFile => {
                let _ = fs::write(unique(&here.join("New Text Document.txt")), "");
                self.reload();
            }
            Command::Delete => {
                for p in &targets {
                    if let Err(e) = move_to_trash(p) {
                        self.status = format!("Could not move to trash: {e}");
                    }
                }
                self.reload();
            }
            Command::DeletePermanently => {
                for p in &targets {
                    let _ = if p.is_dir() {
                        fs::remove_dir_all(p)
                    } else {
                        fs::remove_file(p)
                    };
                }
                self.reload();
            }
            Command::OpenTerminal => {
                spawn_reaped(Proc::new("x-terminal-emulator").current_dir(&here));
            }
            Command::Compress => {
                for p in &targets {
                    let _ = Proc::new("zip")
                        .arg("-r")
                        .arg(format!("{}.zip", p.display()))
                        .arg(p)
                        .current_dir(&here)
                        .status();
                }
                self.reload();
            }
            other => {
                self.status = format!(
                    "{other:?} on {} item(s) — not implemented in the demo",
                    targets.len()
                )
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let events = self.explorer.show(ui, &mut self.state);
        for ev in events {
            self.handle(&ctx, ev);
        }
        if !self.status.is_empty() {
            eprintln!("{}", self.status);
            self.status.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// File-system helpers
// ---------------------------------------------------------------------------

fn load_dir(path: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(path) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|de| {
            let p = de.path();
            let meta = de.metadata().ok();
            let is_dir = meta.as_ref().is_some_and(|metadata| metadata.is_dir());
            let mut e = Entry::new(
                &p,
                if is_dir {
                    EntryKind::Folder
                } else {
                    EntryKind::File
                },
            );
            if let Some(metadata) = meta {
                e.size = if is_dir { 0 } else { metadata.len() };
                e.modified = metadata.modified().ok();
                if is_dir {
                    e.icon = special_folder_icon(&p);
                }
            }
            e
        })
        .collect()
}

fn special_folder_icon(p: &Path) -> Option<Icon> {
    let home = dirs::home_dir()?;
    let table = [
        (dirs::desktop_dir(), Icon::FolderDesktop),
        (dirs::download_dir(), Icon::FolderDownloads),
        (dirs::document_dir(), Icon::FolderDocuments),
        (dirs::picture_dir(), Icon::FolderPictures),
        (dirs::audio_dir(), Icon::FolderMusic),
        (dirs::video_dir(), Icon::FolderVideos),
        (Some(home), Icon::FolderHome),
    ];
    table
        .into_iter()
        .find(|(d, _)| d.as_deref() == Some(p))
        .map(|(_, i)| i)
}

fn subfolders(p: &Path) -> Vec<PathBuf> {
    fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|d| d.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

fn build_nav() -> Vec<NavNode> {
    let mut quick = Vec::new();
    for (dir, label, icon) in [
        (dirs::desktop_dir(), "Desktop", Icon::FolderDesktop),
        (dirs::download_dir(), "Downloads", Icon::FolderDownloads),
        (dirs::document_dir(), "Documents", Icon::FolderDocuments),
        (dirs::picture_dir(), "Pictures", Icon::FolderPictures),
        (dirs::audio_dir(), "Music", Icon::FolderMusic),
        (dirs::video_dir(), "Videos", Icon::FolderVideos),
    ] {
        if let Some(d) = dir.filter(|d| d.exists()) {
            quick.push(NavNode::new(d, label, icon).pinned());
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = vec![
        NavNode::new("quick-access://", "Quick access", Icon::QuickAccess)
            .with_children(quick)
            .expanded(),
    ];
    roots.push(NavNode::new(home.clone(), "Home", Icon::FolderHome));
    let mut pc = vec![NavNode::new("/", "File System", Icon::Drive)];
    if let Ok(rd) = fs::read_dir("/mnt") {
        for d in rd.flatten().filter(|d| d.path().is_dir()) {
            pc.push(NavNode::new(
                d.path(),
                d.file_name().to_string_lossy().into_owned(),
                Icon::Removable,
            ));
        }
    }
    roots.push(
        NavNode::new("this-pc://", "This PC", Icon::ThisPc)
            .with_children(pc)
            .expanded(),
    );
    // Point at the same directory `move_to_trash()` actually uses, and make
    // sure it exists so the node is navigable even before anything has been
    // deleted. (Previously this pointed at "/", so clicking "Trash" opened
    // the filesystem root instead.)
    let trash = trash_dir();
    roots.push(NavNode::new(trash, "Trash", Icon::Trash));
    roots
}

fn unique(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| p.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

/// Copy `from` to `to`, recursing into subdirectories.
///
/// Rejects the case where `to` lies inside `from` before doing any work: if
/// a directory were copied into one of its own descendants, the destination
/// just created would itself be scanned by the recursion that creates it,
/// growing without bound until something (disk space, path length, open file
/// handles) gives out.
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() && destination_inside_source(from, to) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "cannot copy '{}' into itself (destination '{}' is inside it)",
                from.display(),
                to.display()
            ),
        ));
    }
    copy_recursive_unchecked(from, to)
}

fn copy_recursive_unchecked(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_recursive_unchecked(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// True if `to` is `from` itself or lies somewhere underneath it.
///
/// `to` usually doesn't exist yet (it's about to be created), so this
/// canonicalizes `from` and the nearest existing ancestor of `to`, then
/// compares. This also catches the case where a symlink makes `from` and an
/// ancestor of `to` the same directory via different paths.
fn destination_inside_source(from: &Path, to: &Path) -> bool {
    let Ok(from_c) = fs::canonicalize(from) else {
        return false;
    };
    let to_c = canonicalize_existing_prefix(to);
    to_c == from_c || to_c.starts_with(&from_c)
}

/// Canonicalize the nearest existing ancestor of `p` and re-append whatever
/// trailing components don't exist yet, so a not-yet-created destination can
/// still be compared against a canonicalized source.
fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
    let mut trailing = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(base) = fs::canonicalize(cur) {
            return trailing.iter().rev().fold(base, |acc, c| acc.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                trailing.push(name);
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn trash_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Trash/files")
}

fn move_to_trash(p: &Path) -> std::io::Result<()> {
    let trash = trash_dir();
    fs::create_dir_all(&trash)?;
    let dest = unique(&trash.join(p.file_name().unwrap_or_default()));
    fs::rename(p, dest)
}

/// Start a detached helper process (opener, terminal) without leaving a zombie.
///
/// Dropping a `Child` without `wait()` leaves a defunct process behind on Unix
/// once it exits. A short-lived thread reaps it, so the UI never blocks and
/// nothing lingers; a failed spawn (tool not installed) is simply ignored.
fn spawn_reaped(cmd: &mut Proc) {
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
