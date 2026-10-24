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

fn parse_desktop(text: &str) -> Option<AppEntry> {
    let mut in_entry = false;
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
            if !seen.insert(id) {
                continue; // an earlier (higher priority) directory already provided it
            }
            if let Ok(text) = fs::read_to_string(&path)
                && let Some(app) = parse_desktop(&text)
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
    apps.iter()
        .filter(|a| {
            a.mimes
                .iter()
                .any(|m| m == mime || (text_like && m == "text/plain"))
        })
        .collect()
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
        exec: command.to_owned(),
        terminal: false,
        mimes: Vec::new(),
    };
    launch(&app, paths)
}
