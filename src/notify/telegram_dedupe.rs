//! Cross-bot message de-duplication for the all-bots-privacy-off listener.
//!
//! # Why this exists
//!
//! Luca runs **all four** family bots (nora, bruno, mira, otto) with BotFather
//! privacy mode **off**. With privacy off, Telegram delivers *every* plain group
//! message to *every* bot's `getUpdates` queue. The listener (post
//! `fix-poll-all-bots`) long-polls all four queues concurrently and funnels them
//! into one routing pipeline — so the same physical message arrives **four
//! times**, once per bot. Without dedupe the election below would fire four
//! times and the family would get quadruple replies / quadruple actions.
//!
//! # The key
//!
//! A group message has ONE `message_id` within its chat, and Telegram reports
//! the *same* `chat.id` + `message.message_id` to every bot that receives it
//! (only the per-bot `update_id` differs). So `(chat_id, message_id)` uniquely
//! identifies the physical message across all four deliveries. First delivery
//! wins; the other three are dropped silently.
//!
//! # Race-safety
//!
//! [`DedupeSet::first_delivery`] does check-and-insert under a single mutex, so
//! even if the four per-bot pollers ever call it concurrently (today they funnel
//! into one single-threaded consumer, but this must not *rely* on that) exactly
//! one caller observes `true` for a given key. The concurrency test
//! `four_queues_elect_exactly_once` proves this by hammering one key from many
//! threads and asserting exactly one `true`.
//!
//! # Bounding
//!
//! The set is bounded two ways so a long-lived listener never grows without
//! limit: a **TTL** (entries older than [`DedupeSet`]'s `ttl` are evicted — a
//! duplicate can only lag the original by a few seconds of poll latency, so a
//! short TTL is ample) and an **LRU capacity** cap (a hard ceiling on entries;
//! the oldest are dropped first). Both are tunable at construction.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The dedupe key: `(chat_id, message_id)`. Both are the transport's own string
/// ids; together they identify one physical message across every bot that
/// received it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupeKey {
    pub chat_id: String,
    pub message_id: String,
}

impl DedupeKey {
    pub fn new(chat_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            chat_id: chat_id.into(),
            message_id: message_id.into(),
        }
    }
}

/// Default time-to-live for a seen key. A duplicate can only trail the first
/// delivery by the poll round-trip of the other bots — a few seconds at most —
/// so 5 minutes is generously safe while keeping the set tiny.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Default hard ceiling on retained keys (LRU eviction beyond this). A busy
/// family group sees far fewer than this in any 5-minute window; the cap only
/// exists as a backstop against unbounded growth.
pub const DEFAULT_CAPACITY: usize = 4096;

struct Entry {
    key: DedupeKey,
    seen_at: Instant,
}

struct Inner {
    /// Insertion-ordered ring of live keys (front = oldest). Drives both TTL
    /// eviction (evict from the front while expired) and LRU capacity eviction
    /// (evict from the front while over capacity).
    order: VecDeque<Entry>,
    /// Membership index for O(1) `contains`.
    set: HashSet<DedupeKey>,
}

/// A bounded, thread-safe seen-set for `(chat_id, message_id)` keys.
///
/// Construct once per listener; call [`DedupeSet::first_delivery`] for each
/// inbound message. See the module docs for the design rationale.
pub struct DedupeSet {
    inner: Mutex<Inner>,
    ttl: Duration,
    capacity: usize,
}

impl DedupeSet {
    /// A set with the [`DEFAULT_TTL`] and [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_config(DEFAULT_TTL, DEFAULT_CAPACITY)
    }

    /// A set with an explicit TTL and capacity (both tunable).
    pub fn with_config(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                order: VecDeque::new(),
                set: HashSet::new(),
            }),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// Record `key` as seen and report whether THIS is its first delivery.
    ///
    /// Returns `true` exactly once per distinct key (within the TTL window) —
    /// the caller that gets `true` should process the message; callers that get
    /// `false` are looking at a duplicate and must drop it silently. The
    /// check-and-insert is atomic under the internal mutex, so concurrent
    /// callers racing on the same key still see exactly one `true`.
    ///
    /// Uses the wall-clock via `Instant::now()`. For deterministic tests use
    /// [`DedupeSet::first_delivery_at`].
    pub fn first_delivery(&self, key: DedupeKey) -> bool {
        self.first_delivery_at(key, Instant::now())
    }

    /// [`DedupeSet::first_delivery`] with an injected `now`, for testing TTL
    /// eviction deterministically.
    pub fn first_delivery_at(&self, key: DedupeKey, now: Instant) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // 1. Evict everything past its TTL (front = oldest). `checked_duration`
        //    guards against a `now` earlier than an entry (can't normally
        //    happen with a monotonic clock, but never underflow).
        while let Some(front) = inner.order.front() {
            let expired = now
                .checked_duration_since(front.seen_at)
                .map(|age| age >= self.ttl)
                .unwrap_or(false);
            if expired {
                let e = inner.order.pop_front().unwrap();
                inner.set.remove(&e.key);
            } else {
                break;
            }
        }

        // 2. Already seen and still live → duplicate.
        if inner.set.contains(&key) {
            return false;
        }

        // 3. First delivery: record it.
        inner.set.insert(key.clone());
        inner.order.push_back(Entry { key, seen_at: now });

        // 4. Enforce the LRU capacity ceiling (drop oldest beyond cap).
        while inner.order.len() > self.capacity {
            if let Some(e) = inner.order.pop_front() {
                inner.set.remove(&e.key);
            }
        }

        true
    }

    /// Current number of retained (unexpired, uncapped) keys. Test/introspection
    /// only.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .order
            .len()
    }

    /// Whether the set currently retains no keys.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for DedupeSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn first_delivery_true_once_then_false() {
        let set = DedupeSet::new();
        let k = DedupeKey::new("-100999", "42");
        assert!(set.first_delivery(k.clone()), "first delivery wins");
        assert!(!set.first_delivery(k.clone()), "2nd copy is a duplicate");
        assert!(!set.first_delivery(k.clone()), "3rd copy is a duplicate");
        assert!(!set.first_delivery(k), "4th copy is a duplicate");
    }

    #[test]
    fn distinct_messages_each_get_first_delivery() {
        let set = DedupeSet::new();
        assert!(set.first_delivery(DedupeKey::new("-100999", "1")));
        assert!(set.first_delivery(DedupeKey::new("-100999", "2")));
        // Same message id, different chat — still distinct.
        assert!(set.first_delivery(DedupeKey::new("-100888", "1")));
    }

    #[test]
    fn ttl_expiry_lets_a_key_reappear() {
        let ttl = Duration::from_secs(60);
        let set = DedupeSet::with_config(ttl, 100);
        let t0 = Instant::now();
        let k = DedupeKey::new("c", "m");
        assert!(set.first_delivery_at(k.clone(), t0));
        assert!(!set.first_delivery_at(k.clone(), t0 + Duration::from_secs(30)));
        // Past the TTL the original was evicted, so this counts as fresh again.
        assert!(set.first_delivery_at(k, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn lru_capacity_evicts_oldest() {
        let set = DedupeSet::with_config(Duration::from_secs(3600), 2);
        let t0 = Instant::now();
        assert!(set.first_delivery_at(DedupeKey::new("c", "1"), t0));
        assert!(set.first_delivery_at(DedupeKey::new("c", "2"), t0));
        // Inserting a 3rd evicts "1" (oldest).
        assert!(set.first_delivery_at(DedupeKey::new("c", "3"), t0));
        assert_eq!(set.len(), 2);
        // "1" was evicted, so it reads as a first delivery again.
        assert!(set.first_delivery_at(DedupeKey::new("c", "1"), t0));
        // "2" and "3" are still live.
        assert!(!set.first_delivery_at(DedupeKey::new("c", "3"), t0));
    }

    /// THE race test: the same message arriving on four queues (four threads)
    /// must yield EXACTLY ONE first-delivery. This is the core all-bots-off
    /// invariant — exactly-once handling of a 4-queue duplicate.
    #[test]
    fn four_queues_elect_exactly_once() {
        let set = Arc::new(DedupeSet::new());
        let key = DedupeKey::new("-100999", "777");

        // Spawn many threads (far more than 4) all racing to claim the same
        // key, to make an accidental double-claim overwhelmingly likely if the
        // check-and-insert weren't atomic.
        let mut handles = Vec::new();
        for _ in 0..64 {
            let set = Arc::clone(&set);
            let key = key.clone();
            handles.push(thread::spawn(move || set.first_delivery(key)));
        }
        let firsts = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();

        assert_eq!(firsts, 1, "exactly one thread may win the dedupe race");
    }

    /// The realistic shape: four bot pollers, each delivering the identical
    /// (chat, message) once, in sequence through the single consumer. Exactly
    /// one is processed.
    #[test]
    fn sequential_four_bot_delivery_processes_once() {
        let set = DedupeSet::new();
        let key = DedupeKey::new("-100999", "500");
        let deliveries = ["nora", "bruno", "mira", "otto"]; // the polling bot
        let processed: Vec<&str> = deliveries
            .iter()
            .filter(|_bot| set.first_delivery(key.clone()))
            .copied()
            .collect();
        assert_eq!(
            processed,
            vec!["nora"],
            "only the first bot to deliver is processed; the rest are dropped"
        );
    }

    /// `/standup` posted once must fire exactly once even though all four bots
    /// (privacy off) deliver the command. Dedupe gates the command so the
    /// roster check-in isn't posted four times over.
    #[test]
    fn standup_command_deduped_exactly_once_all_bots_off() {
        use crate::notify::telegram_standup::is_standup_command;
        let set = DedupeSet::new();
        let key = DedupeKey::new("-100999", "900");
        let text = "/standup";
        assert!(is_standup_command(text));

        let mut runs = 0;
        for _bot in ["nora", "bruno", "mira", "otto"] {
            // The listener's real order: dedupe FIRST, then command intercept.
            if set.first_delivery(key.clone()) && is_standup_command(text) {
                runs += 1;
            }
        }
        assert_eq!(runs, 1, "/standup fires once with all four bots polling");
    }
}
