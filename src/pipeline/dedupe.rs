//! Message de-duplication.
//!
//! OneBot guarantees at-least-once delivery across reconnects, so the bot keeps two
//! layers of de-duplication:
//!
//! * [`Dedupe`] — a bounded in-memory memory of recently seen `(bot_id,
//!   message_id)` keys. Cheap and lost on restart.
//! * `seen_messages` in SQLite — the durable layer, which keeps only message ids
//!   (never bodies) and is pruned after 7 days.
//!
//! The in-memory layer must therefore never be treated as authoritative: after a
//! restart the store still knows what has been handled.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Default number of remembered keys. At peak group chatter this is a few hours of
/// history, which comfortably exceeds any reconnect window.
pub const DEFAULT_CAPACITY: usize = 20_000;

/// Bounded, insertion ordered de-duplication set.
#[derive(Debug)]
pub struct Dedupe {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    seen: HashMap<String, u64>,
    order: VecDeque<String>,
    /// Monotonic counter standing in for a clock; tests can use it deterministically.
    tick: u64,
}

/// Outcome of a [`Dedupe::observe`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// First time this key is observed inside the in-memory window.
    New,
    /// Already observed; the caller must drop the event.
    Duplicate,
}

impl Dedupe {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Observe a key, returning whether it is new.
    pub fn observe(&self, key: &str) -> Seen {
        let mut inner = self.inner.lock().expect("dedupe 锁被中毒");
        inner.tick += 1;
        let tick = inner.tick;
        if inner.seen.contains_key(key) {
            return Seen::Duplicate;
        }
        inner.seen.insert(key.to_string(), tick);
        inner.order.push_back(key.to_string());
        while inner.order.len() > self.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.seen.remove(&oldest);
            }
        }
        Seen::New
    }

    /// Number of keys currently remembered.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("dedupe 锁被中毒").seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forget everything (used when the operator resets a session).
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("dedupe 锁被中毒");
        inner.seen.clear();
        inner.order.clear();
    }
}

/// Build the store key for one event.
pub fn dedupe_key(bot_id: &str, message_id: &str) -> String {
    format!("{bot_id}:{message_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_is_new_and_second_is_duplicate() {
        let d = Dedupe::new(8);
        assert_eq!(d.observe(&dedupe_key("1", "10")), Seen::New);
        assert_eq!(d.observe(&dedupe_key("1", "10")), Seen::Duplicate);
        // Different bot is a different key.
        assert_eq!(d.observe(&dedupe_key("2", "10")), Seen::New);
    }

    #[test]
    fn capacity_is_enforced_so_memory_cannot_grow_without_bound() {
        let d = Dedupe::new(2);
        assert_eq!(d.observe("a"), Seen::New);
        assert_eq!(d.observe("b"), Seen::New);
        assert_eq!(d.observe("c"), Seen::New);
        assert_eq!(d.len(), 2);
        // "a" was evicted, so it looks new again — that is exactly why the durable
        // `seen_messages` table is the authoritative layer.
        assert_eq!(d.observe("a"), Seen::New);
    }

    #[test]
    fn clear_forgets_everything() {
        let d = Dedupe::new(8);
        d.observe("x");
        assert!(!d.is_empty());
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.observe("x"), Seen::New);
    }

    #[test]
    fn a_zero_capacity_is_clamped_rather_than_emptying_the_window() {
        // Capacity 0 would evict every key the moment it arrived, which is the same as
        // disabling de-duplication entirely. The constructor clamps to 1 instead.
        let d = Dedupe::new(0);
        assert_eq!(d.observe("only"), Seen::New);
        assert_eq!(d.len(), 1);
        assert_eq!(d.observe("only"), Seen::Duplicate);
        // A second key does evict the first at capacity 1; the durable layer is what
        // makes that survivable.
        assert_eq!(d.observe("next"), Seen::New);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn a_duplicate_does_not_grow_the_window_or_refresh_its_position() {
        let d = Dedupe::new(2);
        assert_eq!(d.observe("a"), Seen::New);
        assert_eq!(d.observe("b"), Seen::New);
        assert_eq!(d.observe("a"), Seen::Duplicate);
        assert_eq!(d.len(), 2);
        // `a` is still the oldest insertion, so `c` evicts it: a repeated observation
        // re-inserts a key at the back, but nothing here does that.
        assert_eq!(d.observe("c"), Seen::New);
        assert_eq!(d.observe("a"), Seen::New);
        assert_eq!(d.observe("c"), Seen::Duplicate);
    }

    #[test]
    fn the_window_never_exceeds_its_capacity() {
        let d = Dedupe::new(16);
        for i in 0..1_000 {
            d.observe(&format!("k{i}"));
            assert!(d.len() <= 16, "第 {i} 次观察后窗口长到了 {}", d.len());
        }
        assert_eq!(d.len(), 16);
        // The most recent keys are still remembered.
        assert_eq!(d.observe("k999"), Seen::Duplicate);
    }

    #[test]
    fn the_default_capacity_is_the_documented_one() {
        assert_eq!(DEFAULT_CAPACITY, 20_000);
        let d = Dedupe::new(DEFAULT_CAPACITY);
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn the_key_namespaces_both_ids_and_keeps_them_distinguishable() {
        assert_eq!(dedupe_key("1", "10"), "1:10");
        assert_ne!(dedupe_key("1", "10"), dedupe_key("11", "0"));
        assert_ne!(dedupe_key("1", "10"), dedupe_key("2", "10"));
        assert_eq!(dedupe_key("", ""), ":");
        // Digits that would otherwise collide across a different split stay distinct.
        assert_ne!(dedupe_key("12", "3"), dedupe_key("1", "23"));
    }
}
