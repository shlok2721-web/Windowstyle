//! "Open with": find installed applications for a file from the freedesktop `.desktop`
//! files and launch them directly.
//!
//! This needs no terminal, no `mimeopen` and no compositor protocol, so it behaves the same
//! on GNOME, KDE Plasma, Hyprland, sway and any other Linux desktop.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub name: String,
    /// Desktop file id (`org.gnome.gedit.desktop`), or `cmd:<command>` for a typed command.
    pub id: String,
    /// Raw `Icon=` value (theme name or absolute path).
    pub icon: Option<String>,
    /// `Categories=` from the desktop file (used to find sharing-capable apps).
    pub categories: Vec<String>,
    exec: String,
    terminal: bool,
    mimes: Vec<String>,
}

/// Directories that hold `applications/*.desktop`, highest priority first.
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    let home = dirs::home_dir();
    match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(p) if p.is_absolute() => dirs_out.push(p.join("applications")),
        _ => {
            if let Some(h) = &home {
                dirs_out.push(h.join(".local/share/applications"));
            }
        }
    }
    if let Some(h) = &home {
        dirs_out.push(h.join(".local/share/flatpak/exports/share/applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs_out.push(Path::new(d).join("applications"));
    }
    dirs_out.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs_out.push(PathBuf::from("/var/lib/snapd/desktop/applications"));
    dirs_out
}

fn parse_desktop(text: &str, id: &str) -> Option<AppEntry> {
    let mut in_entry = false;
    let mut icon: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let (mut name, mut exec, mut try_exec) = (None, None, None);
    let (mut terminal, mut hidden, mut is_app) = (false, false, true);
    let mut mimes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim().to_owned()),
            "Categories" => {
                categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "Icon" => icon = Some(value.trim().to_owned()).filter(|v| !v.is_empty()),
            "TryExec" => try_exec = Some(value.trim().to_owned()),
            "Terminal" => terminal = value.trim() == "true",
            "NoDisplay" | "Hidden" => hidden |= value.trim() == "true",
            "Type" => is_app = value.trim() == "Application",
            "MimeType" => {
                mimes = value
                    .split(';')
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            _ => {}
        }
    }
    if hidden || !is_app {
        return None;
    }
    if let Some(t) = try_exec
        && !program_exists(&t)
    {
        return None;
    }
    Some(AppEntry {
        name: name?,
        id: id.to_owned(),
        icon,
        categories,
        exec: exec?,
        terminal,
        mimes,
    })
}

fn program_exists(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(program).exists()))
        .unwrap_or(false)
}

/// Every visible application, de-duplicated by desktop file id, sorted by name.
pub fn all_apps() -> Vec<AppEntry> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in application_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let id_copy = id.clone();
            if !seen.insert(id) {
                continue; // an earlier (higher priority) directory already provided it
            }
            if let Ok(text) = fs::read_to_string(&path)
                && let Some(app) = parse_desktop(&text, &id_copy)
            {
                apps.push(app);
            }
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

pub fn mime_for(path: &Path) -> String {
    if path.is_dir() {
        "inode/directory".to_owned()
    } else {
        fm_mime::guess(path).mime
    }
}

/// Applications that declare support for `mime` (text files also match plain-text apps).
pub fn handlers_for<'a>(apps: &'a [AppEntry], mime: &str) -> Vec<&'a AppEntry> {
    let text_like = mime.starts_with("text/");
    let mut list: Vec<&AppEntry> = apps
        .iter()
        .filter(|a| {
            a.mimes
                .iter()
                .any(|m| m == mime || (text_like && m == "text/plain"))
        })
        .collect();
    // The system default for this type (mimeapps.list) goes first; the sort is stable so
    // the rest keep their alphabetical order.
    let defaults = system_defaults(mime);
    list.sort_by_key(|a| {
        defaults
            .iter()
            .position(|d| *d == a.id)
            .unwrap_or(usize::MAX)
    });
    list
}

/// The desktop's default application for `mime`: `xdg-mime query default` first (it follows
/// every mimeapps.list the desktop uses), then our own reading of `mimeapps.list`.
pub fn default_app(mime: &str) -> Option<AppEntry> {
    let mut ids: Vec<String> = Proc::new("xdg-mime")
        .args(["query", "default", mime])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|id| !id.is_empty())
        .into_iter()
        .collect();
    ids.extend(system_defaults(mime));
    let apps = all_apps();
    ids.iter()
        .find_map(|id| apps.iter().find(|a| a.id == *id).cloned())
}

fn spawn_quiet(program: &str, args: &[&Path]) -> bool {
    let mut command = Proc::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Open `path` with the desktop's default application: `xdg-open`, then `gio open`, then the
/// default handler for its MIME type resolved by hand. Returns whether anything started.
pub fn open_default(path: &Path) -> bool {
    if spawn_quiet("xdg-open", &[path]) {
        return true;
    }
    {
        let mut command = Proc::new("gio");
        command
            .arg("open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    default_app(&mime_for(path)).is_some_and(|app| launch(&app, &[path.to_path_buf()]))
}

/// Open `path` in the text editor the user set as default for plain text (so a `.conf` file
/// opens in their editor even when `.conf` has no handler of its own), falling back to
/// `open_default`.
pub fn open_in_default_text_editor(path: &Path) -> bool {
    if let Some(app) = default_app("text/plain")
        && launch(&app, &[path.to_path_buf()])
    {
        return true;
    }
    open_default(path)
}

/// Desktop ids listed under `[Default Applications]` / `[Added Associations]` for `mime`.
fn system_defaults(mime: &str) -> Vec<String> {
    let mut files = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(p) if p.is_absolute() => files.push(p.join("mimeapps.list")),
        _ => {
            if let Some(h) = dirs::home_dir() {
                files.push(h.join(".config/mimeapps.list"));
            }
        }
    }
    if let Some(h) = dirs::home_dir() {
        files.push(h.join(".local/share/applications/mimeapps.list"));
    }
    let mut out = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(file) else { continue };
        let mut wanted = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                wanted = line == "[Default Applications]" || line == "[Added Associations]";
                continue;
            }
            if let (true, Some((key, value))) = (wanted, line.split_once('=')) {
                if key.trim() == mime {
                    out.extend(value.split(';').filter(|v| !v.is_empty()).map(str::to_owned));
                }
            }
        }
    }
    out
}

/// Split an `Exec` value into arguments following the Desktop Entry quoting rules.
fn split_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut in_quote = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quote = !in_quote;
                in_token = true;
            }
            '\\' if in_quote => {
                if let Some(&n) = chars.peek()
                    && matches!(n, '"' | '`' | '$' | '\\')
                {
                    cur.push(n);
                    chars.next();
                } else {
                    cur.push(c);
                }
                in_token = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// Build the argv for `app` opening `paths`, expanding the `%f %F %u %U ...` field codes.
fn build_argv(app: &AppEntry, paths: &[PathBuf]) -> Vec<String> {
    let mut argv = Vec::new();
    let mut used_files = false;
    for token in split_exec(&app.exec) {
        match token.as_str() {
            "%F" | "%U" => {
                used_files = true;
                argv.extend(paths.iter().map(|p| p.to_string_lossy().into_owned()));
                continue;
            }
            "%i" | "%k" => continue, // icon / desktop file location: not needed
            _ => {}
        }
        let first = paths
            .first()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut s = String::new();
        let mut chars = token.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                s.push(c);
                continue;
            }
            match chars.next() {
                Some('f' | 'u') => {
                    used_files = true;
                    s.push_str(&first);
                }
                Some('F' | 'U') => {
                    used_files = true;
                    s.push_str(&first);
                }
                Some('c') => s.push_str(&app.name),
                Some('%') => s.push('%'),
                Some(_) | None => {} // deprecated or unknown codes expand to nothing
            }
        }
        if !s.is_empty() || !token.contains('%') {
            argv.push(s);
        }
    }
    if !used_files {
        argv.extend(paths.iter().map(|p| p.to_string_lossy().into_owned()));
    }
    argv
}

fn terminal_candidates() -> Vec<(String, &'static str)> {
    let mut v: Vec<(String, &'static str)> = Vec::new();
    if let Ok(term) = std::env::var("TERMINAL")
        && let Some(program) = term.split_whitespace().next()
    {
        v.push((program.to_owned(), "-e"));
    }
    for (name, flag) in [
        ("x-terminal-emulator", "-e"),
        ("kitty", "-e"),
        ("foot", "-e"),
        ("alacritty", "-e"),
        ("konsole", "-e"),
        ("gnome-terminal", "--"),
        ("xfce4-terminal", "-x"),
        ("wezterm", "start"),
        ("xterm", "-e"),
    ] {
        v.push((name.to_owned(), flag));
    }
    v
}

fn spawn_detached(mut cmd: Proc, dir: &Path) -> bool {
    use std::os::unix::process::CommandExt;
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0); // survives this window / process closing
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

fn launch_argv(argv: &[String], terminal: bool, dir: &Path) -> bool {
    let Some((program, rest)) = argv.split_first() else {
        return false;
    };
    if terminal {
        for (term, flag) in terminal_candidates() {
            let mut cmd = Proc::new(&term);
            cmd.arg(flag).arg(program).args(rest);
            if spawn_detached(cmd, dir) {
                return true;
            }
        }
        return false;
    }
    let mut cmd = Proc::new(program);
    cmd.args(rest);
    spawn_detached(cmd, dir)
}

/// Launch `app` with `paths`.
pub fn launch(app: &AppEntry, paths: &[PathBuf]) -> bool {
    let dir = paths
        .first()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."));
    launch_argv(&build_argv(app, paths), app.terminal, dir)
}

/// Launch a user-typed command line with the files appended (or substituted for `%f`/`%F`).
pub fn launch_custom(command: &str, paths: &[PathBuf]) -> bool {
    let app = AppEntry {
        name: command.to_owned(),
        id: format!("cmd:{command}"),
        icon: None,
        categories: Vec::new(),
        exec: command.to_owned(),
        terminal: false,
        mimes: Vec::new(),
    };
    launch(&app, paths)
}

// ---------------------------------------------------------------------------------------
// Application icons
// ---------------------------------------------------------------------------------------

/// Resolve an `Icon=` value to an image file (PNG/SVG), following the icon-theme layout.
pub fn icon_file(app: &AppEntry) -> Option<PathBuf> {
    let name = app.icon.as_deref()?;
    if name.starts_with('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(h) = dirs::home_dir() {
        bases.push(h.join(".local/share"));
        bases.push(h.join(".local/share/flatpak/exports/share"));
        bases.push(h.join(".icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    bases.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    bases.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    bases.push(PathBuf::from("/var/lib/snapd/desktop"));

    const SIZES: [&str; 9] = [
        "128x128", "256x256", "96x96", "64x64", "48x48", "512x512", "scalable", "32x32", "24x24",
    ];
    for base in &bases {
        for theme in ["hicolor", "Adwaita", "breeze", "Papirus"] {
            for size in SIZES {
                let dir = base.join("icons").join(theme).join(size).join("apps");
                for ext in ["png", "svg"] {
                    let p = dir.join(format!("{name}.{ext}"));
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
        for ext in ["png", "svg"] {
            let p = base.join("pixmaps").join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
            let p = base.join("icons").join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

thread_local! {
    static ICON_CACHE: std::cell::RefCell<std::collections::HashMap<String, Option<egui::TextureHandle>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Texture for the app's logo, loaded lazily (a few per frame so a long list never stalls the
/// UI). `None` means "not available (yet)": the caller draws a letter badge instead.
pub fn icon_texture(ctx: &egui::Context, app: &AppEntry, budget: &mut u32) -> Option<egui::TextureId> {
    ICON_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(entry) = cache.get(&app.id) {
            return entry.as_ref().map(|t| t.id());
        }
        if *budget == 0 {
            ctx.request_repaint();
            return None;
        }
        *budget -= 1;
        let tex = icon_file(app)
            .and_then(|p| egui_explorer::load_app_icon_image(&p))
            .map(|img| ctx.load_texture(format!("open-with-icon:{}", app.id), img, egui::TextureOptions::LINEAR));
        let id = tex.as_ref().map(|t| t.id());
        cache.insert(app.id.clone(), tex);
        id
    })
}

// ---------------------------------------------------------------------------------------
// "Remember my choice"
// ---------------------------------------------------------------------------------------

/// What a remembered choice points at.
pub enum Choice {
    App(AppEntry),
    /// Execute the file itself (detached).
    Run,
    /// Execute the file itself inside a terminal.
    RunInTerminal,
}

const ID_RUN: &str = "@run";
const ID_TERMINAL: &str = "@terminal";

fn assoc_file() -> Option<PathBuf> {
    fm_settings::default_config_path("file-manager")?
        .parent()
        .map(|d| d.join("open_with.tsv"))
}

/// Key shared by every file of the same kind: its extension, or its exact name when it has none.
pub fn assoc_key(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("ext:{}", ext.to_lowercase()),
        _ => format!(
            "name:{}",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ),
    }
}

/// Human label for the tick box, e.g. `.png files` or `files named "Makefile"`.
pub fn assoc_label(path: &Path) -> String {
    match assoc_key(path).split_once(':') {
        Some(("ext", ext)) => format!(".{ext} files"),
        Some((_, name)) => format!("files named \"{name}\""),
        None => "this file type".to_owned(),
    }
}

fn read_assocs() -> Vec<(String, String)> {
    let Some(file) = assoc_file() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(file) else { return Vec::new() };
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn write_assocs(list: &[(String, String)]) {
    let Some(file) = assoc_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let text: String = list.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
    let tmp = file.with_extension("tsv.tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, &file);
    }
}

/// Store `choice` as the default for every file of the same kind as `path`.
pub fn remember(path: &Path, id: &str) {
    let key = assoc_key(path);
    let mut list = read_assocs();
    list.retain(|(k, _)| *k != key);
    list.push((key, id.replace(['\t', '\n'], " ")));
    write_assocs(&list);
}

pub fn remember_run(path: &Path, terminal: bool) {
    remember(path, if terminal { ID_TERMINAL } else { ID_RUN });
}

/// Forget the remembered choice for this kind of file.
pub fn forget(path: &Path) {
    let key = assoc_key(path);
    let mut list = read_assocs();
    list.retain(|(k, _)| *k != key);
    write_assocs(&list);
}

/// Id remembered for this kind of file, if any (cheap: one small file read).
pub fn remembered_id(path: &Path) -> Option<String> {
    let key = assoc_key(path);
    read_assocs().into_iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Resolve a remembered id to something launchable. `None` if the app was uninstalled.
pub fn resolve_choice(id: &str) -> Option<Choice> {
    match id {
        ID_RUN => Some(Choice::Run),
        ID_TERMINAL => Some(Choice::RunInTerminal),
        _ => {
            if let Some(cmd) = id.strip_prefix("cmd:") {
                return Some(Choice::App(AppEntry {
                    name: cmd.to_owned(),
                    id: id.to_owned(),
                    icon: None,
                    categories: Vec::new(),
                    exec: cmd.to_owned(),
                    terminal: false,
                    mimes: Vec::new(),
                }));
            }
            all_apps().into_iter().find(|a| a.id == id).map(Choice::App)
        }
    }
}

/// Open `paths` with a resolved choice.
pub fn launch_choice(choice: &Choice, paths: &[PathBuf]) -> bool {
    match choice {
        Choice::App(app) => launch(app, paths),
        Choice::Run => paths.iter().all(|p| run_executable(p, false)),
        Choice::RunInTerminal => paths.iter().all(|p| run_executable(p, true)),
    }
}

// ---------------------------------------------------------------------------------------
// Executables
// ---------------------------------------------------------------------------------------

/// A regular file with an execute bit (binaries, AppImages, scripts with a shebang).
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run the file itself, fully detached from this window:
/// `nohup "$@" >/dev/null 2>&1 & disown`. With `terminal` it runs in a terminal emulator and
/// waits for Enter afterwards so output stays readable.
pub fn run_executable(path: &Path, terminal: bool) -> bool {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let target = path.to_string_lossy().into_owned();
    if terminal {
        let argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "\"$0\"; printf '\\n[Process exited. Press Enter to close] '; read _".to_owned(),
            target,
        ];
        return launch_argv(&argv, true, dir);
    }
    // `disown` is a bash builtin (dash has none), so prefer bash and fall back to plain sh.
    for (shell, script) in [
        ("bash", "nohup \"$@\" >/dev/null 2>&1 & disown"),
        ("sh", "nohup \"$@\" >/dev/null 2>&1 &"),
    ] {
        let mut cmd = Proc::new(shell);
        cmd.arg("-c").arg(script).arg("open-with").arg(&target);
        if spawn_detached(cmd, dir) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------
// Share
// ---------------------------------------------------------------------------------------

impl AppEntry {
    /// Whether the `Exec` line takes file / URL arguments (`%f %F %u %U`).
    pub fn takes_files(&self) -> bool {
        ["%f", "%F", "%u", "%U"].iter().any(|c| self.exec.contains(c))
    }
}

/// Somewhere a file can be sent to on this desktop.
#[derive(Clone)]
pub enum ShareTarget {
    /// An installed app that takes files: chat / mail / transfer clients and the file
    /// manager "Send to" entries. `accepts` is true when it declares support for every
    /// selected file type (those are listed first).
    App { app: AppEntry, accepts: bool },
    /// The configured email client (handled by the host's email command).
    Email,
    /// Bluetooth transfer tool (`blueman-sendto` / `bluetooth-sendto`).
    Bluetooth(&'static str),
}

impl ShareTarget {
    pub fn label(&self) -> String {
        match self {
            ShareTarget::App { app, .. } => app.name.clone(),
            ShareTarget::Email => "Email".to_owned(),
            ShareTarget::Bluetooth(_) => "Bluetooth device".to_owned(),
        }
    }
}

fn sendto_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = dirs::home_dir() {
        v.push(h.join(".local/share/Thunar/sendto"));
    }
    v.push(PathBuf::from("/usr/share/Thunar/sendto"));
    v
}

/// Look through the installed desktop applications for ones that accept `paths`: file-transfer
/// tools (LocalSend, Warpinator, KDE Connect ...), chat and mail clients that take files, and
/// the file manager "Send to" entries. Browsers and remote-desktop tools are left out.
pub fn share_targets(paths: &[PathBuf]) -> Vec<ShareTarget> {
    let mimes: Vec<String> = paths.iter().map(|p| mime_for(p)).collect();
    let mut out: Vec<ShareTarget> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dir in sendto_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(file) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if let Ok(text) = fs::read_to_string(&path)
                && let Some(app) = parse_desktop(&text, &format!("sendto:{file}"))
                && seen.insert(app.name.to_lowercase())
            {
                out.push(ShareTarget::App { app, accepts: true });
            }
        }
    }

    for app in all_apps() {
        let has = |c: &str| app.categories.iter().any(|x| x == c);
        let chatty = has("Network") || has("Email") || has("InstantMessaging") || has("Chat") || has("P2P");
        let candidate = has("FileTransfer") || (chatty && app.takes_files());
        if !candidate || has("WebBrowser") || has("RemoteAccess") || has("Dialup") {
            continue;
        }
        if !seen.insert(app.name.to_lowercase()) {
            continue;
        }
        let accepts = app.mimes.iter().any(|m| m == "*/*")
            || mimes.iter().all(|mm| app.mimes.iter().any(|m| m == mm));
        out.push(ShareTarget::App { app, accepts });
    }
    // Apps that declare the file types first (stable: keeps "Send to" entries and name order).
    out.sort_by_key(|t| match t {
        ShareTarget::App { accepts, .. } => u8::from(!*accepts),
        _ => 2,
    });
    out.push(ShareTarget::Email);
    if let Some(prog) = ["blueman-sendto", "bluetooth-sendto"]
        .into_iter()
        .find(|p| program_exists(p))
    {
        out.push(ShareTarget::Bluetooth(prog));
    }
    out
}

/// Reachable KDE Connect devices as `(id, name)`. Empty when KDE Connect is not installed.
/// Talks to D-Bus, so call it off the UI thread.
pub fn kde_connect_devices() -> Vec<(String, String)> {
    if !program_exists("kdeconnect-cli") {
        return Vec::new();
    }
    let Ok(out) = Proc::new("kdeconnect-cli")
        .args(["--list-available", "--id-name-only"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (id, name) = l.trim().split_once(' ')?;
            Some((id.to_owned(), name.trim().to_owned()))
        })
        .collect()
}

/// Send every file to a KDE Connect device.
pub fn share_to_device(id: &str, paths: &[PathBuf]) -> bool {
    paths.iter().all(|p| {
        let dir = p.parent().unwrap_or_else(|| Path::new("."));
        let mut cmd = Proc::new("kdeconnect-cli");
        cmd.args(["-d", id, "--share"]).arg(p);
        spawn_detached(cmd, dir)
    })
}

/// Hand the files to a share target (everything except [`ShareTarget::Email`], which the host
/// routes through its own email command).
pub fn share_launch(target: &ShareTarget, paths: &[PathBuf]) -> bool {
    match target {
        ShareTarget::App { app, .. } => launch(app, paths),
        ShareTarget::Bluetooth(prog) => {
            let dir = paths
                .first()
                .and_then(|p| p.parent())
                .unwrap_or_else(|| Path::new("."));
            let mut cmd = Proc::new(prog);
            cmd.args(paths);
            spawn_detached(cmd, dir)
        }
        ShareTarget::Email => false,
    }
}
