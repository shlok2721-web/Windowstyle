//! Watches systemd-logind's `PrepareForSleep` / `PrepareForShutdown` signals by reading the
//! output of `dbus-monitor` (or `gdbus monitor` as a fallback). No D-Bus library is linked.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
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
            let Some(position) = find_member(text, name) else {
                continue;
            };
            // Once a new recognized member line arrives, it must not inherit a pending
            // argument from a previous dbus-monitor header.
            self.pending = None;
            let rest = text[position + name.len()..].trim_start();
            if let Some(args) = rest.strip_prefix('(') {
                return parse_gdbus_boolean(args).and_then(|value| Self::finish(kind, value));
            }
            if is_header {
                self.pending = Some(kind);
            }
            return None;
        }
        if let (Some(kind), true) = (self.pending, text.starts_with("boolean ")) {
            self.pending = None;
            let value = match text.strip_prefix("boolean ").map(str::trim) {
                Some("true") => true,
                Some("false") => false,
                _ => return None,
            };
            return Self::finish(kind, value);
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

/// Locate a member name as a complete token rather than accepting a substring of an
/// unrelated member such as `PrepareForSleepChanged`.
fn find_member(text: &str, member: &str) -> Option<usize> {
    text.match_indices(member).find_map(|(position, _)| {
        let before = text[..position].chars().next_back();
        let after = text[position + member.len()..].chars().next();
        let valid_before = before.is_none_or(|ch| ch == '.' || ch == '=');
        let valid_after = after.is_none_or(|ch| ch.is_whitespace() || ch == '(' || ch == ';');
        (valid_before && valid_after).then_some(position)
    })
}

/// Parse the first boolean argument emitted by `gdbus monitor`. Malformed values must not
/// be interpreted as `false`, since that could synthesize a misleading Resume event.
fn parse_gdbus_boolean(arguments: &str) -> Option<bool> {
    let value = arguments.trim_start();
    for (literal, parsed) in [("true", true), ("false", false)] {
        if let Some(rest) = value.strip_prefix(literal) {
            if matches!(rest.trim_start().chars().next(), Some(',') | Some(')')) {
                return Some(parsed);
            }
        }
    }
    None
}

// These arguments are static, so candidate retries do not allocate nested Vec<String>s.
const CANDIDATES: &[(&str, &[&str])] = &[
    // `stdbuf -oL` keeps dbus-monitor from block-buffering its output when piped.
    ("stdbuf", &["-oL", "dbus-monitor", "--system", RULE]),
    ("dbus-monitor", &["--system", RULE]),
    (
        "gdbus",
        &[
            "monitor",
            "--system",
            "--dest",
            "org.freedesktop.login1",
            "--object-path",
            "/org/freedesktop/login1",
        ],
    ),
];

/// Start the monitor thread. `active` is true while a monitor process is running; the running
/// child is parked in `child_slot` so the owner can kill it on shutdown.
pub(crate) fn spawn(
    tx: Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
    child_slot: Arc<Mutex<Option<Child>>>,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("file-manager-logind".into())
        .spawn(move || {
            for &(program, args) in CANDIDATES {
                // Serialize the stop check and child registration with Lifecycle::drop.
                // This closes the race where shutdown sees an empty slot just before a new
                // monitor process is started and consequently cannot kill that process.
                let mut slot = child_slot.lock().unwrap_or_else(|e| e.into_inner());
                if stop.load(Ordering::Acquire) {
                    break;
                }
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
                *slot = Some(child);
                active.store(true, Ordering::Release);
                drop(slot);
                let started = Instant::now();
                let mut parser = Parser::default();
                let mut receiver_closed = false;
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if let Some(event) = parser.feed(&line) {
                        if tx.send(event).is_err() {
                            receiver_closed = true;
                            break;
                        }
                        wake();
                    }
                }
                let finished = child_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(mut child) = finished {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                active.store(false, Ordering::Release);
                if receiver_closed || stop.load(Ordering::Acquire) {
                    break;
                }
                // A tool that ran for a while and then ended is not retried with another tool;
                // one that died at once (missing binary behind `stdbuf`, no bus) is.
                if started.elapsed() > Duration::from_secs(5) {
                    break;
                }
            }
            active.store(false, Ordering::Release);
        })
        .ok()
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

    #[test]
    fn malformed_gdbus_booleans_are_not_treated_as_resume() {
        let events = run(&[
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (garbage,)",
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (trueish,)",
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleep (falseX,)",
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn similarly_named_members_are_ignored() {
        let events = run(&[
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.PrepareForSleepChanged (false,)",
            "/org/freedesktop/login1: org.freedesktop.login1.Manager.NotPrepareForSleep (false,)",
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_dbus_boolean_clears_pending_argument() {
        let events = run(&[
            "signal time=1.0 sender=:1 -> destination=(null destination) serial=1 path=/org/freedesktop/login1; interface=org.freedesktop.login1.Manager; member=PrepareForSleep",
            "boolean unknown",
            "boolean true",
        ]);
        assert!(events.is_empty());
    }
}
