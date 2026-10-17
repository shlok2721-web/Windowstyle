//! Watches systemd-logind's `PrepareForSleep` / `PrepareForShutdown` signals by reading the
//! output of `dbus-monitor` (or `gdbus monitor` as a fallback). No D-Bus library is linked.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::Event;

const RULE: &str =
    "type='signal',sender='org.freedesktop.login1',interface='org.freedesktop.login1.Manager'";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sleep,
    Shutdown,
}

/// Turns monitor output lines into events. Understands both `dbus-monitor` (a header line
/// naming the member followed by a `boolean true|false` line) and `gdbus monitor`
/// (`... Manager.PrepareForSleep (true,)` on one line).
#[derive(Debug, Default)]
pub(crate) struct Parser {
    pending: Option<Kind>,
}

impl Parser {
    pub(crate) fn feed(&mut self, line: &str) -> Option<Event> {
        let text = line.trim();
        let is_header = text.starts_with("signal ");
        if is_header {
            self.pending = None;
        }
        for (name, kind) in [
            ("PrepareForSleep", Kind::Sleep),
            ("PrepareForShutdown", Kind::Shutdown),
        ] {
            let Some(position) = text.find(name) else {
                continue;
            };
            let rest = text[position + name.len()..].trim_start();
            if let Some(args) = rest.strip_prefix('(') {
                return Self::finish(kind, args.trim_start().starts_with("true"));
            }
            if is_header {
                self.pending = Some(kind);
            }
            return None;
        }
        if let (Some(kind), Some(value)) = (self.pending, text.strip_prefix("boolean ")) {
            self.pending = None;
            return Self::finish(kind, value.trim() == "true");
        }
        None
    }

    fn finish(kind: Kind, on: bool) -> Option<Event> {
        match (kind, on) {
            (Kind::Sleep, true) => Some(Event::Suspend),
            (Kind::Sleep, false) => Some(Event::Resume),
            (Kind::Shutdown, true) => Some(Event::Shutdown),
            (Kind::Shutdown, false) => None,
        }
    }
}

fn candidates() -> Vec<Vec<String>> {
    let to_strings = |parts: &[&str]| {
        parts
            .iter()
            .map(|p| (*p).to_owned())
            .collect::<Vec<String>>()
    };
    vec![
        // `stdbuf -oL` keeps dbus-monitor from block-buffering its output when piped.
        to_strings(&["stdbuf", "-oL", "dbus-monitor", "--system", RULE]),
        to_strings(&["dbus-monitor", "--system", RULE]),
        to_strings(&[
            "gdbus",
            "monitor",
            "--system",
            "--dest",
            "org.freedesktop.login1",
            "--object-path",
            "/org/freedesktop/login1",
        ]),
    ]
}

/// Start the monitor thread. `active` is true while a monitor process is running; the running
/// child is parked in `child_slot` so the owner can kill it on shutdown.
pub(crate) fn spawn(
    tx: Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
    child_slot: Arc<Mutex<Option<Child>>>,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new()
        .name("file-manager-logind".into())
        .spawn(move || {
            for argv in candidates() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some((program, args)) = argv.split_first() else {
                    continue;
                };
                let spawned = Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn();
                let Ok(mut child) = spawned else { continue };
                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill();
                    let _ = child.wait();
                    continue;
                };
                *child_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                active.store(true, Ordering::Relaxed);
                let started = Instant::now();
                let mut parser = Parser::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(event) = parser.feed(&line) {
                        if tx.send(event).is_err() {
                            return;
                        }
                        wake();
                    }
                }
                active.store(false, Ordering::Relaxed);
                let finished = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(mut child) = finished {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                // A tool that ran for a while and then ended is not retried with another tool;
                // one that died at once (missing binary behind `stdbuf`, no bus) is.
                if started.elapsed() > Duration::from_secs(5) {
                    break;
                }
            }
            active.store(false, Ordering::Relaxed);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<Event> {
        let mut parser = Parser::default();
        lines.iter().filter_map(|l| parser.feed(l)).collect()
    }

    #[test]
    fn dbus_monitor_format() {
        let events = run(&[
            "signal time=1.0 sender=:1.5 -> destination=(null destination) serial=9 path=/org/freedesktop/login1; interface=org.freedesktop.login1.Manager; member=PrepareForSleep",
            "   boolean true",
            "signal time=2.0 sender=:1.5 -> destination=(null destination) serial=10 path=/org/freedesktop/login1; interface=org.freedesktop.login1.Manager; member=PrepareForSleep",
            "   boolean false",
            "signal time=3.0 sender=:1.5 -> destination=(null destination) serial=11 path=/org/freedesktop/login1; interface=org.freedesktop.login1.Manager; member=PrepareForShutdown",
            "   boolean true",
        ]);
        assert_eq!(events, [Event::Suspend, Event::Resume, Event::Shutdown]);
    }

    #[test]
    fn gdbus_format() {
        let events = run(&[
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (true,)",
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (false,)",
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForShutdown (false,)",
        ]);
        assert_eq!(events, [Event::Suspend, Event::Resume]);
    }

    #[test]
    fn unrelated_signals_and_stray_booleans_are_ignored() {
        let events = run(&[
            "signal time=1.0 sender=:1.5 -> destination=(null destination) serial=9 path=/x; interface=org.freedesktop.login1.Manager; member=SessionNew",
            "   boolean true",
            "   boolean true",
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn header_for_another_member_cancels_a_pending_sleep() {
        let events = run(&[
            "signal time=1.0 sender=:1 -> destination=(null destination) serial=1 path=/x; interface=i; member=PrepareForSleep",
            "signal time=2.0 sender=:1 -> destination=(null destination) serial=2 path=/x; interface=i; member=Other",
            "   boolean true",
        ]);
        assert!(events.is_empty());
    }
}
