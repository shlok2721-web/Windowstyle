//! Four-finger touchpad click.
//!
//! Wayland and X11 never tell an application how many fingers are on a touchpad: libinput
//! folds them into ordinary button events. The kernel's evdev nodes do report it
//! (`BTN_TOOL_QUADTAP`), so this module passively reads (never grabs) the touchpad's
//! `/dev/input/event*` node in a background thread and classifies a four-finger press by
//! how long it is held:
//!
//! - shorter than `CLICK_MIN` (600 ms): ignored. A brief, accidental brush of four fingers
//!   must not do anything.
//! - between `CLICK_MIN` and `CLICK_MAX` (600 ms - 1.2 s): a plain four-finger "click" -
//!   the file list treats it like an ordinary selection click.
//! - between `CLICK_MAX` and `DELETE_HOLD` (1.2 s - 2 s): a dead zone. Neither a click nor
//!   a delete, so a click that runs a little long never turns into an accidental delete.
//! - held for `DELETE_HOLD` (2 s) or more: a delete. This fires as soon as the hold crosses
//!   the threshold - the user does not need to lift their fingers first - like a long-press,
//!   so there is a real, deliberate pause before anything destructive happens.
//!
//! Reading `/dev/input/event*` needs permission: the user must be in the `input` group
//! (`sudo usermod -aG input $USER`, then log out and in). Without it this silently does
//! nothing except print one line to stderr.

use std::fs::File;
use std::io::Read;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EV_KEY: u16 = 0x01;
const BTN_TOOL_QUADTAP: u16 = 0x14f;

/// Below this, a four-finger press is too short to be a deliberate click.
const CLICK_MIN: Duration = Duration::from_millis(600);
/// Above this (and below `DELETE_HOLD`), a four-finger press is in the dead zone: neither
/// a click nor a delete.
const CLICK_MAX: Duration = Duration::from_millis(1200);
/// How long a four-finger press must be held before it counts as a delete.
const DELETE_HOLD: Duration = Duration::from_millis(2000);
/// How often the background ticker checks an in-progress hold against `DELETE_HOLD`.
const TICK: Duration = Duration::from_millis(40);
/// How long a recorded click or delete stays valid for the UI to pick up.
const VALID_MS: u64 = 700;

/// Wall-clock milliseconds of the last four-finger click, 0 when none is waiting.
static LAST_CLICK_MS: AtomicU64 = AtomicU64::new(0);
/// Wall-clock milliseconds of the last four-finger delete, 0 when none is waiting.
static LAST_DELETE_MS: AtomicU64 = AtomicU64::new(0);
/// Wall-clock milliseconds when the current four-finger press began, 0 when fingers are up.
static ACTIVE_SINCE_MS: AtomicU64 = AtomicU64::new(0);
/// Whether the current press has already fired a delete (so it can't also fire on release,
/// and so the ticker only fires once per press).
static DELETE_ARMED: AtomicBool = AtomicBool::new(false);
static START: Once = Once::new();

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
        .max(1)
}

/// Whether a four-finger click happened recently and has not been used yet.
pub fn pending_click() -> bool {
    let at = LAST_CLICK_MS.load(Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < VALID_MS
}

/// Use up the waiting click; `true` for exactly one caller.
pub fn consume_click() -> bool {
    LAST_CLICK_MS.swap(0, Ordering::Relaxed) != 0
}

/// Whether a four-finger delete-hold fired recently and has not been used yet.
pub fn pending_delete() -> bool {
    let at = LAST_DELETE_MS.load(Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < VALID_MS
}

/// Use up the waiting delete; `true` for exactly one caller.
pub fn consume_delete() -> bool {
    LAST_DELETE_MS.swap(0, Ordering::Relaxed) != 0
}

/// Start the reader and ticker threads once (cheap to call every frame).
pub fn ensure_started(ctx: &egui::Context) {
    START.call_once(|| {
        let nodes = touchpad_nodes();
        if nodes.is_empty() {
            return;
        }
        for node in nodes {
            let ctx = ctx.clone();
            let _ = std::thread::Builder::new()
                .name("touchpad-quadtap".into())
                .spawn(move || read_node(&node, &ctx));
        }
        // A held four-finger press generates no further evdev events until release, so a
        // separate ticker is what notices the press crossing `DELETE_HOLD` while it is
        // still down (a true long-press, not something keyed off release).
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("touchpad-quadtap-ticker".into())
            .spawn(move || loop {
                std::thread::sleep(TICK);
                let since = ACTIVE_SINCE_MS.load(Ordering::Relaxed);
                if since != 0
                    && !DELETE_ARMED.load(Ordering::Relaxed)
                    && now_ms().saturating_sub(since) >= DELETE_HOLD.as_millis() as u64
                {
                    DELETE_ARMED.store(true, Ordering::Relaxed);
                    LAST_DELETE_MS.store(now_ms(), Ordering::Relaxed);
                    ctx.request_repaint();
                }
            });
    });
}

/// `/dev/input/eventN` nodes whose capability bitmap contains `BTN_TOOL_QUADTAP`.
fn touchpad_nodes() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut event = None;
        let mut has_quad = false;
        for line in block.lines() {
            if let Some(h) = line.strip_prefix("H: Handlers=") {
                event = h
                    .split_whitespace()
                    .find(|w| w.starts_with("event"))
                    .map(str::to_owned);
            } else if let Some(k) = line.strip_prefix("B: KEY=") {
                // Hex words, most significant first, 64 bits each.
                let words: Vec<u64> = k
                    .split_whitespace()
                    .filter_map(|w| u64::from_str_radix(w, 16).ok())
                    .collect();
                let bit = BTN_TOOL_QUADTAP as usize;
                let from_right = bit / 64;
                if let Some(idx) = words.len().checked_sub(from_right + 1) {
                    has_quad |= words[idx] >> (bit % 64) & 1 == 1;
                }
            }
        }
        if has_quad && let Some(ev) = event {
            out.push(format!("/dev/input/{ev}"));
        }
    }
    out
}

fn read_node(path: &str, ctx: &egui::Context) {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(error) => {
            eprintln!(
                "Four-finger click: cannot read {path} ({error}). Add your user to the \
                 'input' group and log in again to enable it."
            );
            return;
        }
    };
    // struct input_event: timeval (2 words), u16 type, u16 code, i32 value.
    let size = if cfg!(target_pointer_width = "64") { 24 } else { 16 };
    let mut buf = vec![0u8; size];
    while file.read_exact(&mut buf).is_ok() {
        let ty = u16::from_ne_bytes([buf[size - 8], buf[size - 7]]);
        let code = u16::from_ne_bytes([buf[size - 6], buf[size - 5]]);
        let value = i32::from_ne_bytes([buf[size - 4], buf[size - 3], buf[size - 2], buf[size - 1]]);
        if ty != EV_KEY || code != BTN_TOOL_QUADTAP {
            continue;
        }
        match value {
            1 => {
                // Fingers landed: start timing this press. The ticker thread watches for
                // it crossing DELETE_HOLD; this thread decides on release whether it was
                // a plain click.
                DELETE_ARMED.store(false, Ordering::Relaxed);
                ACTIVE_SINCE_MS.store(now_ms(), Ordering::Relaxed);
            }
            0 => {
                let since = ACTIVE_SINCE_MS.swap(0, Ordering::Relaxed);
                if since != 0 && !DELETE_ARMED.load(Ordering::Relaxed) {
                    let held = Duration::from_millis(now_ms().saturating_sub(since));
                    if held >= CLICK_MIN && held <= CLICK_MAX {
                        LAST_CLICK_MS.store(now_ms(), Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                    // Shorter than CLICK_MIN, or in the dead zone above CLICK_MAX: ignored.
                }
            }
            _ => {}
        }
    }
}
