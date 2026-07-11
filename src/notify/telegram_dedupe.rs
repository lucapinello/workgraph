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
//! # The key — a content fingerprint, NOT `message_id`
//!
//! The obvious key would be `(chat_id, message_id)`, and an earlier version used
//! exactly that on the assumption that Telegram reports the *same*
//! `message.message_id` to every bot that receives a given group message. **That
//! assumption is false on the wire.** Observed on 2026-07-11: one human message
//! yielded FOUR DIFFERENT `message_id`s (58 / 36 / 45 / 39) across the four
//! bots' `getUpdates` responses. Each bot lives in its own message-id space, so
//! `(chat_id, message_id)` can *never* collide across pollers and the dedupe
//! never fires — the exact multiplicity bug this module exists to kill.
//!
//! What IS identical across all four deliveries of one physical message is its
//! *content*: the chat it landed in, the human who sent it (`from.id`), the
//! second it was sent (`message.date`), and the text. So the key is a **content
//! fingerprint** — `(chat_id, from.id, date, hash(text))`. First delivery wins;
//! the other three are dropped silently. `message_id` is kept out of the key
//! entirely and is used only for per-bot logging and reply-threading.
//!
//! Why each field earns its place in the fingerprint:
//! - `chat_id` — two groups can carry identical text; keep them distinct.
//! - `from.id` — two people can say "ok" in the same second; keep them distinct.
//! - `date` (second granularity) — the same person repeating the same text a
//!   minute later is a NEW turn that must be answered, not a stale duplicate.
//!   Telegram's `date` is the send time and is identical across all four
//!   deliveries, so it separates re-sends without splitting one delivery.
//! - `hash(text)` — the actual content; folded to a `u64` so the retained key
//!   stays tiny regardless of message length.
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

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The dedupe key: a **content fingerprint** of one physical message —
/// `(chat_id, sender_id, date, text_hash)`. These four fields are identical
/// across every bot that receives the same group message (only the per-bot
/// `message_id` / `update_id` differ), so the fingerprint collides exactly on
/// the duplicate deliveries and on nothing else. Build one with
/// [`DedupeKey::from_content`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupeKey {
    /// The chat the message arrived in (Telegram `chat.id`, as a string).
    pub chat_id: String,
    /// The sender's stable id (Telegram `from.id`). When the transport did not
    /// surface a numeric id the caller passes the display sender instead — still
    /// identical across the four deliveries, which is all the fingerprint needs.
    pub sender_id: String,
    /// The send time in whole seconds (Telegram `message.date`). Separates a
    /// later re-send of identical text (a new turn) from the four simultaneous
    /// deliveries of one turn (which all share the same `date`).
    pub date: i64,
    /// A `u64` hash of the message text, so the key stays small regardless of
    /// message length. Computed by [`hash_text`].
    pub text_hash: u64,
}

impl DedupeKey {
    /// Build the content fingerprint from a message's stable content fields:
    /// its chat, its sender, its send-time (whole seconds), and its text. All
    /// four are identical across the four privacy-off deliveries of one physical
    /// message, so equal fingerprints ⇔ same physical message.
    pub fn from_content(
        chat_id: impl Into<String>,
        sender_id: impl Into<String>,
        date: i64,
        text: &str,
    ) -> Self {
        Self {
            chat_id: chat_id.into(),
            sender_id: sender_id.into(),
            date,
            text_hash: hash_text(text),
        }
    }
}

/// Fold arbitrary message text down to a `u64` fingerprint component.
///
/// Uses the std `DefaultHasher` (SipHash-1-3 with fixed zero keys), which is
/// deterministic within — and, with these fixed keys, across — process runs.
/// Cross-process stability is not actually required here (the set lives only
/// for one listener's lifetime), but determinism within the run is, and this
/// provides it.
pub fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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

/// A bounded, thread-safe seen-set for content-fingerprint keys.
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

    // A canonical group chat and human for the fixtures below.
    const CHAT: &str = "-100999";
    const LUCA: &str = "555";

    #[test]
    fn first_delivery_true_once_then_false() {
        let set = DedupeSet::new();
        let k = DedupeKey::from_content(CHAT, LUCA, 1_700_000_000, "who's cooking?");
        assert!(set.first_delivery(k.clone()), "first delivery wins");
        assert!(!set.first_delivery(k.clone()), "2nd copy is a duplicate");
        assert!(!set.first_delivery(k.clone()), "3rd copy is a duplicate");
        assert!(!set.first_delivery(k), "4th copy is a duplicate");
    }

    /// THE regression fixture for this fix. The four privacy-off deliveries of
    /// ONE physical message carry FOUR DIFFERENT `message_id`s (the wire-observed
    /// 58/36/45/39) but identical content. Keying on `message_id` would let all
    /// four through (four elections → quadruple replies); the content
    /// fingerprint collapses them to exactly one first-delivery.
    #[test]
    fn same_content_different_message_ids_elects_once() {
        let set = DedupeSet::new();
        let (chat, sender, date, text) = (CHAT, LUCA, 1_700_000_000_i64, "what's for dinner?");
        // The four per-bot deliveries. `message_id` differs on every one; it is
        // deliberately NOT part of the key, so it is not even passed here.
        let _observed_message_ids = ["58", "36", "45", "39"];
        let elected = (0..4)
            .filter(|_| set.first_delivery(DedupeKey::from_content(chat, sender, date, text)))
            .count();
        assert_eq!(
            elected, 1,
            "four deliveries with different message_ids but identical content → exactly one election"
        );
        assert_eq!(set.len(), 1, "only the one fingerprint is retained");
    }

    #[test]
    fn distinct_messages_each_get_first_delivery() {
        let set = DedupeSet::new();
        let date = 1_700_000_000;
        assert!(set.first_delivery(DedupeKey::from_content(CHAT, LUCA, date, "one")));
        // Same sender & time, different text — distinct.
        assert!(set.first_delivery(DedupeKey::from_content(CHAT, LUCA, date, "two")));
        // Same text & time, different chat — distinct.
        assert!(set.first_delivery(DedupeKey::from_content("-100888", LUCA, date, "one")));
        // Same text, chat & time, different sender — distinct.
        assert!(set.first_delivery(DedupeKey::from_content(CHAT, "777", date, "one")));
    }

    /// The same person repeating the same words a moment later is a NEW turn
    /// (different `date`) and must be processed, not swallowed as a duplicate.
    #[test]
    fn same_text_later_second_is_a_new_turn() {
        let set = DedupeSet::new();
        assert!(set.first_delivery(DedupeKey::from_content(CHAT, LUCA, 1_700_000_000, "ok")));
        assert!(
            set.first_delivery(DedupeKey::from_content(CHAT, LUCA, 1_700_000_001, "ok")),
            "identical text one second later is a fresh turn, not a dupe"
        );
    }

    #[test]
    fn ttl_expiry_lets_a_key_reappear() {
        let ttl = Duration::from_secs(60);
        let set = DedupeSet::with_config(ttl, 100);
        let t0 = Instant::now();
        let k = DedupeKey::from_content(CHAT, LUCA, 1_700_000_000, "hi");
        assert!(set.first_delivery_at(k.clone(), t0));
        assert!(!set.first_delivery_at(k.clone(), t0 + Duration::from_secs(30)));
        // Past the TTL the original was evicted, so this counts as fresh again.
        assert!(set.first_delivery_at(k, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn lru_capacity_evicts_oldest() {
        let set = DedupeSet::with_config(Duration::from_secs(3600), 2);
        let t0 = Instant::now();
        let k1 = DedupeKey::from_content(CHAT, LUCA, 1, "a");
        let k2 = DedupeKey::from_content(CHAT, LUCA, 2, "b");
        let k3 = DedupeKey::from_content(CHAT, LUCA, 3, "c");
        assert!(set.first_delivery_at(k1.clone(), t0));
        assert!(set.first_delivery_at(k2, t0));
        // Inserting a 3rd evicts k1 (oldest).
        assert!(set.first_delivery_at(k3.clone(), t0));
        assert_eq!(set.len(), 2);
        // k1 was evicted, so it reads as a first delivery again.
        assert!(set.first_delivery_at(k1, t0));
        // k3 is still live.
        assert!(!set.first_delivery_at(k3, t0));
    }

    /// THE race test: the same message arriving on four queues (four threads)
    /// must yield EXACTLY ONE first-delivery. This is the core all-bots-off
    /// invariant — exactly-once handling of a 4-queue duplicate.
    #[test]
    fn four_queues_elect_exactly_once() {
        let set = Arc::new(DedupeSet::new());
        let key = DedupeKey::from_content(CHAT, LUCA, 1_700_000_000, "race me");

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
    /// message (same content, different per-bot `message_id`) once, in sequence
    /// through the single consumer. Exactly one is processed.
    #[test]
    fn sequential_four_bot_delivery_processes_once() {
        let set = DedupeSet::new();
        let (chat, sender, date, text) = (CHAT, LUCA, 1_700_000_000_i64, "hello family");
        let deliveries = ["nora", "bruno", "mira", "otto"]; // the polling bot
        let processed: Vec<&str> = deliveries
            .iter()
            .filter(|_bot| set.first_delivery(DedupeKey::from_content(chat, sender, date, text)))
            .copied()
            .collect();
        assert_eq!(
            processed,
            vec!["nora"],
            "only the first bot to deliver is processed; the rest are dropped"
        );
    }

    /// `/standup` posted once must fire exactly once even though all four bots
    /// (privacy off) deliver the command — each with a different `message_id`.
    /// Dedupe gates the command so the roster check-in isn't posted four times.
    #[test]
    fn standup_command_deduped_exactly_once_all_bots_off() {
        use crate::notify::telegram_standup::is_standup_command;
        let set = DedupeSet::new();
        let (chat, sender, date, text) = (CHAT, LUCA, 1_700_000_000_i64, "/standup");
        assert!(is_standup_command(text));

        let mut runs = 0;
        for _bot in ["nora", "bruno", "mira", "otto"] {
            // The listener's real order: dedupe FIRST, then command intercept.
            if set.first_delivery(DedupeKey::from_content(chat, sender, date, text))
                && is_standup_command(text)
            {
                runs += 1;
            }
        }
        assert_eq!(runs, 1, "/standup fires once with all four bots polling");
    }

    #[test]
    fn hash_text_is_deterministic_and_content_sensitive() {
        assert_eq!(hash_text("dinner?"), hash_text("dinner?"));
        assert_ne!(hash_text("dinner?"), hash_text("dinner!"));
    }
}
