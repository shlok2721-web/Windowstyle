//! One process, many windows.
//!
//! Starting `file-manager` a second time must not create a second process (a second copy of
//! every cache, thread and GL context). The first instance listens on a Unix socket in the
//! user's runtime directory; later launches send their folder to it and exit immediately, and
//! the running instance opens that folder in a new window of the same process.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

pub enum Acquire {
    /// Another instance is running and now owns the request; this process should exit.
    Forwarded,
    /// This process is the instance; hand the listener to the app.
    Primary(Listener),
    /// No single-instance support here (or the socket could not be used): just run.
    Unavailable,
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    pub struct Listener {
        listener: UnixListener,
        sock: PathBuf,
    }

    fn socket_path() -> Option<PathBuf> {
        // The runtime dir is private to the user (0700). Fall back to a per-user state dir
        // rather than a shared /tmp, where another user could squat on the name.
        let dir = dirs::runtime_dir()
            .or_else(dirs::state_dir)
            .or_else(dirs::data_local_dir)?;
        fs::create_dir_all(&dir).ok()?;
        Some(dir.join("file-manager.sock"))
    }

    pub fn acquire(forward: Option<&Path>) -> Acquire {
        let Some(sock) = socket_path() else {
            return Acquire::Unavailable;
        };
        if let Ok(mut stream) = UnixStream::connect(&sock) {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let msg = forward
                .map(|p| p.to_string_lossy().replace('\n', " "))
                .unwrap_or_default();
            // A live instance answered: never touch its socket, even if the write fails.
            return match writeln!(stream, "{msg}").and_then(|()| stream.flush()) {
                Ok(()) => Acquire::Forwarded,
                Err(_) => Acquire::Unavailable,
            };
        }
        // Nobody is listening: any socket file left behind is stale.
        let _ = fs::remove_file(&sock);
        match UnixListener::bind(&sock) {
            Ok(listener) => {
                let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));
                Acquire::Primary(Listener { listener, sock })
            }
            Err(_) => Acquire::Unavailable,
        }
    }

    impl Listener {
        /// Start the accept thread. Each received folder arrives on the returned channel
        /// (an empty path means "no folder given").
        pub fn serve(self, ctx: egui::Context) -> Receiver<PathBuf> {
            let (tx, rx) = mpsc::channel();
            let Listener { listener, sock } = self;
            let spawned = std::thread::Builder::new()
                .name("file-manager-single-instance".into())
                .spawn(move || {
                    let _guard = RemoveOnDrop(sock);
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { continue };
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut line = String::new();
                        // Bounded read: a misbehaving client cannot make us buffer unboundedly.
                        let mut reader = BufReader::new(stream.take(8192));
                        if reader.read_line(&mut line).is_ok() {
                            let path = PathBuf::from(line.trim_end_matches(['\n', '\r']));
                            if tx.send(path).is_err() {
                                break;
                            }
                            ctx.request_repaint();
                        }
                    }
                });
            if let Err(error) = spawned {
                eprintln!("Could not start the single-instance listener: {error}");
            }
            rx
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
pub use imp::{Listener, acquire};

#[cfg(not(unix))]
pub struct Listener;

#[cfg(not(unix))]
pub fn acquire(_forward: Option<&std::path::Path>) -> Acquire {
    Acquire::Unavailable
}

#[cfg(not(unix))]
impl Listener {
    pub fn serve(self, _ctx: egui::Context) -> Receiver<PathBuf> {
        mpsc::channel().1
    }
}
