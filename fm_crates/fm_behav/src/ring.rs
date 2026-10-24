//! Fixed-size event ring and the path interner that keeps events at 16 bytes.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

/// Size of one [`Event`] in memory.
pub const EVENT_BYTES: usize = 16;

/// What the user did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Opened (navigated into) a directory.
    OpenDir,
    /// Opened a file; the event's path is the file.
    OpenFile,
    /// Pinned a directory.
    Pin,
    /// Removed a pin.
    Unpin,
    /// Went back to a directory through history.
    Back,
}

/// One user action: `{ts, dir_id, action}`, 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Unix seconds.
    pub ts: u64,
    /// Interned path id (valid until the next fold).
    pub dir_id: u32,
    /// The action.
    pub action: Action,
}

const _: () = assert!(std::mem::size_of::<Event>() == EVENT_BYTES);

/// Bounded FIFO of events. When full, the oldest event is overwritten. Events are never
/// written to disk; they are folded into aggregates and then dropped.
#[derive(Debug)]
pub struct EventRing {
    buf: VecDeque<Event>,
    cap: usize,
    overwritten: u64,
}

impl EventRing {
    /// A ring holding at most `cap` events (minimum 1).
    pub fn with_capacity(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
            overwritten: 0,
        }
    }

    /// Maximum number of events.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Events currently stored.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Whether the next push would overwrite the oldest event.
    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.cap
    }

    /// Number of events lost to overwriting so far.
    pub fn overwritten(&self) -> u64 {
        self.overwritten
    }

    /// Append an event, overwriting the oldest when full.
    pub fn push(&mut self, event: Event) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
            self.overwritten += 1;
        }
        self.buf.push_back(event);
    }

    /// Remove the oldest event without allocating a temporary collection.
    pub(crate) fn pop_front(&mut self) -> Option<Event> {
        self.buf.pop_front()
    }

    /// Remove and yield every event, oldest first.
    pub fn drain(&mut self) -> std::collections::vec_deque::Drain<'_, Event> {
        self.buf.drain(..)
    }
}

/// Maps paths to small ids for the lifetime of one fold cycle.
#[derive(Debug, Default)]
pub(crate) struct PathInterner {
    // The map and reverse index share the same path allocation. A burst of distinct paths
    // therefore doesn't keep two full copies of every path just to support id lookup.
    ids: HashMap<Arc<Path>, u32>,
    paths: Vec<Arc<Path>>,
}

impl PathInterner {
    pub(crate) fn intern(&mut self, path: &Path) -> Option<u32> {
        if let Some(&id) = self.ids.get(path) {
            return Some(id);
        }
        // An Event stores a u32. Refuse to wrap an id if a caller configures an enormous ring
        // and records more unique paths than that representation can address.
        let id = u32::try_from(self.paths.len()).ok()?;
        let path: Arc<Path> = Arc::from(path);
        self.paths.push(Arc::clone(&path));
        self.ids.insert(path, id);
        Some(id)
    }

    pub(crate) fn get_shared(&self, id: u32) -> Option<Arc<Path>> {
        self.paths.get(id as usize).cloned()
    }

    pub(crate) fn clear(&mut self) {
        self.ids.clear();
        self.paths.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64) -> Event {
        Event {
            ts,
            dir_id: 0,
            action: Action::OpenDir,
        }
    }

    #[test]
    fn events_are_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<Event>(), 16);
    }

    #[test]
    fn full_ring_overwrites_the_oldest_event() {
        let mut r = EventRing::with_capacity(3);
        for ts in 1..=5 {
            r.push(ev(ts));
        }
        assert_eq!((r.len(), r.overwritten()), (3, 2));
        let kept: Vec<u64> = r.drain().map(|e| e.ts).collect();
        assert_eq!(kept, [3, 4, 5]);
        assert!(r.is_empty());
    }

    #[test]
    fn interner_reuses_ids_until_cleared() {
        let mut i = PathInterner::default();
        let a = i.intern(Path::new("/a")).unwrap();
        let b = i.intern(Path::new("/b")).unwrap();
        assert_ne!(a, b);
        assert_eq!(i.intern(Path::new("/a")), Some(a));
        assert_eq!(i.get_shared(b).unwrap().as_ref(), Path::new("/b"));
        i.clear();
        assert!(i.get_shared(a).is_none());
    }

    #[test]
    fn interned_paths_can_be_shared_without_changing_lookup() {
        let mut i = PathInterner::default();
        let id = i.intern(Path::new("/shared/path")).unwrap();
        let shared = i.get_shared(id).unwrap();
        assert_eq!(shared.as_ref(), Path::new("/shared/path"));
        i.clear();
        // Existing aggregate owners may retain a path after the interner is cleared.
        assert_eq!(shared.as_ref(), Path::new("/shared/path"));
    }
}
