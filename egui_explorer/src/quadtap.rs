//! Four-finger touchpad click.
//!
//! Wayland and X11 never tell an application how many fingers are on a touchpad: libinput
//! folds them into ordinary button events. The kernel's evdev nodes do report it
//! (`BTN_TOOL_QUADTAP`), so this module passively reads (never grabs) the touchpad's
//! `/dev/input/event*` node in a background thread and records "a four-finger click or tap
//! just happened". The file list then deletes the item under the pointer (after the usual
//! confirmation window).
//!
//! Reading `/dev/input/event*` needs permission: the user must be in the `input` group
//! (`sudo usermod -aG input $USER`, then log out and in). Without it this silently does
//! nothing except print one line to stderr.

use std::fs::File;
use std::io::Read;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EV_KEY: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_QUADTAP: u16 = 0x14f;

/// A four-finger tap shorter than this counts as a click even without a physical press.
const TAP_MAX: Duration = Duration::from_millis(250);
/// How long a recorded four-finger click stays valid for the UI to pick up.
const VALID_MS: u64 = 700;

/// Wall-clock milliseconds of the last four-finger click, 0 when none is waiting.
static LAST_CLICK_MS: AtomicU64 = AtomicU64::new(0);
static START: Once = Once::new();

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
        .max(1)
}

/// Whether a four-finger click happened recently and has not been used yet.
pub fn pending() -> bool {
    let at = LAST_CLICK_MS.load(Ordering::Relaxed);
    at != 0 && now_ms().saturating_sub(at) < VALID_MS
}

/// Use up the waiting click; `true` for exactly one caller.
pub fn consume() -> bool {
    LAST_CLICK_MS.swap(0, Ordering::Relaxed) != 0
}

/// Start the reader threads once (cheap to call every frame).
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
    let mut active = false;
    let mut fired = false;
    let mut since = Instant::now();
    let fire = |ctx: &egui::Context| {
        LAST_CLICK_MS.store(now_ms(), Ordering::Relaxed);
        ctx.request_repaint();
    };
    while file.read_exact(&mut buf).is_ok() {
        let ty = u16::from_ne_bytes([buf[size - 8], buf[size - 7]]);
        let code = u16::from_ne_bytes([buf[size - 6], buf[size - 5]]);
        let value = i32::from_ne_bytes([buf[size - 4], buf[size - 3], buf[size - 2], buf[size - 1]]);
        if ty != EV_KEY {
            continue;
        }
        match (code, value) {
            (BTN_TOOL_QUADTAP, 1) => {
                active = true;
                fired = false;
                since = Instant::now();
            }
            (BTN_TOOL_QUADTAP, 0) => {
                if active && !fired && since.elapsed() < TAP_MAX {
                    fire(ctx);
                }
                active = false;
            }
            (BTN_LEFT, 1) if active && !fired => {
                fired = true;
                fire(ctx);
            }
            _ => {}
        }
    }
}
