//! One process, many windows, with a bounded standalone fallback.
//!
//! A second launch normally forwards its folder to the primary process, which creates a child
//! viewport and shares its caches and workers. Merely queueing that request is not enough on
//! Wayland: a root window on an inactive workspace may not run another UI frame promptly. V4
//! therefore waits for an acknowledgement from the actual child viewport callback. If that
//! callback does not happen in time, the caller cancels its queued request and opens the folder
//! in a standalone process rather than hanging until the parent workspace is visited.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// A request from another invocation of the same binary.
pub struct OpenRequest {
    pub path: PathBuf,
    pub workspace_target: Option<String>,
    /// Cancellation is shared between the socket handler and the UI queue.
    pub active: Arc<AtomicBool>,
    /// Sent only when the native child viewport callback actually runs. `false` means rejected.
    pub opened: Option<Sender<bool>>,
}

impl OpenRequest {
    pub fn local(path: PathBuf, workspace_target: Option<String>) -> Self {
        Self {
            path,
            workspace_target,
            active: Arc::new(AtomicBool::new(true)),
            opened: None,
        }
    }
}

pub enum Acquire {
    /// A running or potentially running instance owns the one-process slot; exit rather than
    /// starting a duplicate UI process. Forwarding failures are reported to stderr.
    Exit,
    /// This process is the instance; hand the listener to the app.
    Primary(Listener),
    /// No single-instance support is available; run without owning the socket.
    Unavailable,
    /// The primary accepted the request but failed to create a native viewport in time. Run this
    /// invocation standalone, while the primary cancels the queued request.
    Fallback,
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const PROTOCOL_GREETING: &str = "WINDOWSTYLE/4";
    const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
    const HANDLER_TIMEOUT: Duration = Duration::from_secs(8);
    #[derive(Default)]
    struct PendingState {
        active: HashMap<String, Arc<AtomicBool>>,
        cancelled_before_registration: HashSet<String>,
        completed: HashSet<String>,
    }
    type PendingRequests = Arc<Mutex<PendingState>>;

    pub struct Listener {
        listener: UnixListener,
        sock: PathBuf,
    }

    fn socket_path() -> Option<PathBuf> {
        // Runtime storage is private to the user. Avoid a shared /tmp socket name.
        let dir = dirs::runtime_dir()
            .or_else(dirs::state_dir)
            .or_else(dirs::data_local_dir)?;
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join("file-manager.sock"))
    }

    /// Only a refused/missing socket proves that the primary no longer owns the endpoint.
    fn socket_is_definitely_stale(sock: &Path) -> bool {
        matches!(
            UnixStream::connect(sock),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                )
        )
    }

    fn write_all_and_flush(stream: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
        stream.write_all(bytes)?;
        stream.flush()
    }

    fn new_request_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{nanos}", std::process::id())
    }

    /// Read a stable Hyprland workspace selector only in a Hyprland session. Numeric workspace
    /// IDs are preferred; named and special workspaces must be addressed by name.
    pub fn current_hyprland_workspace() -> Option<String> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            return None;
        }
        let output = std::process::Command::new("timeout")
            .args(["-k", "100ms", "250ms", "hyprctl", "-j", "activeworkspace"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let json = String::from_utf8_lossy(&output.stdout);
        let id = json_integer_field(&json, "id")?;
        if id > 0 {
            return Some(id.to_string());
        }
        let name = json_string_field(&json, "name")?;
        if name.is_empty() || name.contains(['\n', '\r', ',']) {
            return None;
        }
        if name == "special" || name.starts_with("special:") || name.starts_with("name:") {
            Some(name)
        } else {
            Some(format!("name:{name}"))
        }
    }

    fn json_integer_field(json: &str, field: &str) -> Option<i64> {
        let key = format!("\"{field}\"");
        let start = json.find(&key)? + key.len();
        let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
        let number: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
            .collect();
        number.parse().ok()
    }

    fn json_string_field(json: &str, field: &str) -> Option<String> {
        let key = format!("\"{field}\"");
        let start = json.find(&key)? + key.len();
        let rest = json[start..].trim_start().strip_prefix(':')?.trim_start();
        let mut chars = rest.strip_prefix('"')?.chars();
        let mut value = String::new();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => return Some(value),
                '\\' => match chars.next()? {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    '/' => value.push('/'),
                    'b' => value.push('\u{0008}'),
                    'f' => value.push('\u{000c}'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    _ => return None,
                },
                _ => value.push(ch),
            }
        }
        None
    }

    pub fn acquire(forward: Option<&Path>) -> Acquire {
        // Capture the launching workspace before any socket negotiation or fallback delay.
        let workspace = current_hyprland_workspace().unwrap_or_else(|| "-".to_owned());
        let Some(sock) = socket_path() else {
            return Acquire::Unavailable;
        };

        match UnixStream::connect(&sock) {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                let _ = reader
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_millis(350)));
                let mut greeting = String::new();
                let is_v4 = reader
                    .read_line(&mut greeting)
                    .is_ok_and(|bytes| bytes > 0 && greeting.trim() == PROTOCOL_GREETING);
                if is_v4 {
                    return forward_v4(&sock, forward, &workspace, reader);
                }
                // Do not send an un-cancelable request to an older primary: it can sit in that
                // process's queue and create a duplicate window when its workspace is revisited.
                // A probe has no request payload, so standalone fallback is safe.
                drop(reader);
                if socket_is_definitely_stale(&sock) {
                    claim_primary(sock)
                } else {
                    eprintln!("Windowstyle: the running instance does not support V4 window acknowledgements; opening this folder independently. Restart the primary on the V4 build to restore shared-process windows.");
                    Acquire::Fallback
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => claim_primary(sock),
            Err(error) => {
                eprintln!("Windowstyle: single-instance socket state is ambiguous; not starting a duplicate process: {error}");
                Acquire::Exit
            }
        }
    }

    fn forward_v4(
        sock: &Path,
        forward: Option<&Path>,
        workspace: &str,
        mut reader: BufReader<UnixStream>,
    ) -> Acquire {
        let request_id = new_request_id();
        let _ = reader
            .get_ref()
            .set_write_timeout(Some(Duration::from_secs(2)));
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(OPEN_TIMEOUT));
        let path = forward
            .map(|value| value.to_string_lossy().replace(['\n', '\r'], " "))
            .unwrap_or_default();
        let payload = format!(
            "{path}\n@windowstyle-workspace={workspace}\n@windowstyle-request={request_id}\n"
        );
        if let Err(error) = write_all_and_flush(reader.get_mut(), payload.as_bytes()) {
            if socket_is_definitely_stale(sock) {
                return claim_primary(sock.to_path_buf());
            }
            eprintln!("Windowstyle: could not forward the open request: {error}");
            send_cancel(sock, &request_id);
            return Acquire::Fallback;
        }

        let mut queued = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => match line.trim() {
                    "QUEUED" => {
                        queued = true;
                        eprintln!("Windowstyle: request {request_id} queued by the existing process.");
                        // The primary now owns the request and enforces HANDLER_TIMEOUT itself,
                        // answering CANCELLED. Wait for that verdict instead of declaring failure
                        // after a short arbitrary delay (delayed frame, inactive workspace).
                        let _ = reader
                            .get_ref()
                            .set_read_timeout(Some(HANDLER_TIMEOUT + Duration::from_secs(2)));
                    }
                    "OPENED" => {
                        eprintln!("Windowstyle: request {request_id} opened by the existing process.");
                        return Acquire::Exit;
                    }
                    "REJECTED" => {
                        eprintln!("Windowstyle: the running instance rejected the open request (window limit reached or request invalid)." );
                        return Acquire::Exit;
                    }
                    "CANCELLED" | "FAILED" => break,
                    _ => {}
                },
                Err(_) => break,
            }
        }

        // No viewport-level acknowledgement is a real failure, even if the listener did send
        // QUEUED. Cancel before falling back so a later root frame cannot open a duplicate.
        send_cancel(sock, &request_id);
        if socket_is_definitely_stale(sock) {
            return claim_primary(sock.to_path_buf());
        }
        if queued {
            eprintln!("Windowstyle: the existing process queued the folder but did not create a window before its deadline; opening it independently as a last resort.");
        } else {
            eprintln!("Windowstyle: the existing process did not confirm window creation; opening the folder independently instead.");
        }
        Acquire::Fallback
    }

    fn send_cancel(sock: &Path, request_id: &str) {
        let Ok(stream) = UnixStream::connect(sock) else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_millis(350)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
        let mut reader = BufReader::new(stream);
        let mut greeting = String::new();
        if !reader
            .read_line(&mut greeting)
            .is_ok_and(|bytes| bytes > 0 && greeting.trim() == PROTOCOL_GREETING)
        {
            return;
        }
        let message = format!("@windowstyle-cancel={request_id}\n");
        let _ = write_all_and_flush(reader.get_mut(), message.as_bytes());
    }

    fn claim_primary(sock: PathBuf) -> Acquire {
        // Bind privately, then publish with an atomic hard link: of concurrent launches exactly
        // one wins. A stale socket is removed only after a refused/missing connection proves it.
        let tmp = sock.with_extension(format!("sock.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let listener = match UnixListener::bind(&tmp) {
            Ok(listener) => listener,
            Err(_) => return Acquire::Unavailable,
        };
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        for _ in 0..3 {
            match fs::hard_link(&tmp, &sock) {
                Ok(()) => {
                    let _ = fs::remove_file(&tmp);
                    return Acquire::Primary(Listener { listener, sock });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if UnixStream::connect(&sock).is_ok() {
                        let _ = fs::remove_file(&tmp);
                        return Acquire::Exit;
                    }
                    if socket_is_definitely_stale(&sock) {
                        let _ = fs::remove_file(&sock);
                        continue;
                    }
                    let _ = fs::remove_file(&tmp);
                    eprintln!("Windowstyle: socket publication raced with an ambiguous existing instance; not starting a duplicate process.");
                    return Acquire::Exit;
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&tmp);
        Acquire::Unavailable
    }

    impl Listener {
        /// Start the accept loop. Each client gets a small handler thread so a request waiting on
        /// its UI acknowledgement cannot block a concurrent CANCEL or another folder launch.
        pub fn serve(self, ctx: egui::Context) -> Receiver<OpenRequest> {
            let (tx, rx) = mpsc::channel();
            let Listener { listener, sock } = self;
            let spawned = thread::Builder::new()
                .name("file-manager-single-instance".into())
                .spawn(move || {
                    let _guard = RemoveOnDrop(sock);
                    let pending: PendingRequests = Arc::new(Mutex::new(PendingState::default()));
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { continue };
                        let tx = tx.clone();
                        let pending = Arc::clone(&pending);
                        let ctx = ctx.clone();
                        let _ = thread::Builder::new()
                            .name("file-manager-ipc-client".into())
                            .spawn(move || handle_client(stream, tx, pending, ctx));
                    }
                });
            if let Err(error) = spawned {
                eprintln!("Could not start the single-instance listener: {error}");
            }
            rx
        }
    }

    fn handle_client(
        stream: UnixStream,
        tx: Sender<OpenRequest>,
        pending: PendingRequests,
        ctx: egui::Context,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(700)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let reader_stream = match stream.try_clone() {
            Ok(clone) => clone,
            Err(_) => return,
        };
        let mut writer = stream;
        if write_all_and_flush(&mut writer, format!("{PROTOCOL_GREETING}\n").as_bytes()).is_err() {
            return;
        }
        let mut reader = BufReader::new(reader_stream);
        let mut first_line = String::new();
        if !reader.read_line(&mut first_line).is_ok_and(|bytes| bytes > 0) {
            return;
        }
        let first_line = first_line.trim_end_matches(['\n', '\r']).to_owned();
        if let Some(request_id) = first_line.strip_prefix("@windowstyle-cancel=") {
            let mut state = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(active) = state.active.get(request_id).cloned() {
                active.store(false, Ordering::Release);
            } else if !state.completed.contains(request_id) {
                // Cancellation may arrive before the request handler is scheduled. Remember it
                // so a late registration cannot resurrect the timed-out request.
                state.cancelled_before_registration.insert(request_id.to_owned());
            }
            ctx.request_repaint_of(egui::ViewportId::ROOT);
            let _ = write_all_and_flush(&mut writer, b"CANCELLED\n");
            return;
        }

        let path = PathBuf::from(first_line);
        let mut metadata = String::new();
        let _ = reader.read_line(&mut metadata);
        let workspace_target = metadata
            .trim()
            .strip_prefix("@windowstyle-workspace=")
            .map(str::to_owned)
            .filter(|value| value != "-" && !value.is_empty());

        // V4 clients send this line in the same write as the path and workspace. Legacy callers
        // do not, so keep this read short and continue to support their two-line request format.
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(150)));
        let mut request_meta = String::new();
        let _ = reader.read_line(&mut request_meta);
        let request_id = request_meta
            .trim()
            .strip_prefix("@windowstyle-request=")
            .filter(|value| !value.is_empty())
            .map(str::to_owned);

        eprintln!(
            "Windowstyle: request {} received for {}.",
            request_id.as_deref().unwrap_or("legacy"),
            path.display()
        );
        let active = Arc::new(AtomicBool::new(true));
        let (opened, opened_rx) = if let Some(id) = request_id.as_ref() {
            let mut state = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.cancelled_before_registration.remove(id) {
                active.store(false, Ordering::Release);
            } else {
                state.active.insert(id.clone(), Arc::clone(&active));
            }
            let (opened_tx, opened_rx) = mpsc::channel();
            (Some(opened_tx), Some(opened_rx))
        } else {
            (None, None)
        };
        if tx
            .send(OpenRequest {
                path,
                workspace_target,
                active: Arc::clone(&active),
                opened,
            })
            .is_err()
        {
            active.store(false, Ordering::Release);
            remove_pending(&pending, request_id.as_deref());
            return;
        }
        eprintln!(
            "Windowstyle: request {} queued for the UI.",
            request_id.as_deref().unwrap_or("legacy")
        );
        ctx.request_repaint_of(egui::ViewportId::ROOT);
        if write_all_and_flush(&mut writer, b"QUEUED\n").is_err() {
            active.store(false, Ordering::Release);
            remove_pending(&pending, request_id.as_deref());
            return;
        }

        let Some(opened_rx) = opened_rx else {
            // V1-V3 callers only understand the queue acknowledgement.
            return;
        };
        let deadline = Instant::now() + HANDLER_TIMEOUT;
        loop {
            if !active.load(Ordering::Acquire) {
                let _ = write_all_and_flush(&mut writer, b"CANCELLED\n");
                break;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "Windowstyle: request {} cancelled: no viewport was created within {:?}.",
                    request_id.as_deref().unwrap_or("legacy"),
                    HANDLER_TIMEOUT
                );
                active.store(false, Ordering::Release);
                let _ = write_all_and_flush(&mut writer, b"CANCELLED\n");
                break;
            }
            match opened_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(true) => {
                    let _ = write_all_and_flush(&mut writer, b"OPENED\n");
                    break;
                }
                Ok(false) => {
                    let _ = write_all_and_flush(&mut writer, b"REJECTED\n");
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = write_all_and_flush(&mut writer, b"FAILED\n");
                    break;
                }
            }
        }
        remove_pending(&pending, request_id.as_deref());
    }

    fn remove_pending(pending: &PendingRequests, request_id: Option<&str>) {
        if let Some(request_id) = request_id {
            let mut state = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.active.remove(request_id);
            state.cancelled_before_registration.remove(request_id);
            state.completed.insert(request_id.to_owned());
            // Request IDs are unique. Keep the recent completion cache bounded for a long-lived
            // primary while still closing the tiny late-CANCEL race for normal launch bursts.
            if state.completed.len() > 512 {
                state.completed.clear();
            }
        }
    }

    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[cfg(unix)]
pub use imp::{Listener, acquire, current_hyprland_workspace};

#[cfg(not(unix))]
pub struct Listener;

#[cfg(not(unix))]
pub fn acquire(_forward: Option<&std::path::Path>) -> Acquire {
    Acquire::Unavailable
}

#[cfg(not(unix))]
pub fn current_hyprland_workspace() -> Option<String> {
    None
}

#[cfg(not(unix))]
impl Listener {
    pub fn serve(self, _ctx: egui::Context) -> Receiver<OpenRequest> {
        mpsc::channel().1
    }
}
