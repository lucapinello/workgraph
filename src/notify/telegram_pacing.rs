//! Timing policies that keep the group listener from over-replying:
//! startup stale-backlog suppression (Fix #1) and burst coalescing (Fix #2).
//!
//! Both are pure and clock-free — the caller passes Unix-second timestamps — so
//! they are fully unit-testable without a live listener. They are *defense in
//! depth* behind the Fix #0 bot-loop guard: even with the storm's root cause
//! fixed, the household should never see a bot answer a 20-minute-old message on
//! restart, nor fire four separate roster replies for a rapid burst.

use std::collections::HashMap;

/// Default staleness threshold: a message sent more than this many seconds
/// before the listener started is treated as backlog, not a live turn.
pub const DEFAULT_STALE_SECS: i64 = 300; // ~5 minutes

/// Default burst window: repeated collective (or same-agent) elections within
/// this many seconds collapse to a single reply.
pub const DEFAULT_BURST_SECS: i64 = 30;

/// True when a message sent at `sent_at` is **stale backlog** relative to the
/// listener's start (`listener_start`): older than `threshold_secs` before the
/// listener came up.
///
/// Fix #1 — on startup the long-poll drains whatever queued while the listener
/// was down. Those messages are not live conversation and must not be
/// conversationally answered (the household would get replies to questions they
/// asked and moved on from long ago). A live message sent *after* startup has
/// `sent_at >= listener_start`, so it is never stale by this test; only genuine
/// pre-start backlog is caught.
pub fn is_stale_backlog(sent_at: i64, listener_start: i64, threshold_secs: i64) -> bool {
    sent_at < listener_start.saturating_sub(threshold_secs)
}

/// One compact, family-voice line (in Otto's concierge voice) posted ONCE if any
/// stale backlog was skipped on startup, so the skip is visible rather than
/// silent. No jargon, no ids (docs/04).
pub fn backlog_skipped_line() -> String {
    "Just caught up after a quiet spell — I skipped some older messages so I don't reply to \
     things you've moved on from. Ping me again if anything still needs me. \u{1f44b}"
        .to_string()
}

/// Burst coalescer (Fix #2): collapse repeated elections within a short window to
/// a single reply. Anchored to the last *admitted* reply — a suppressed election
/// does not extend the window, so `window_secs` after the reply that went out,
/// the next election is admitted again.
#[derive(Debug, Clone)]
pub struct BurstCoalescer {
    window_secs: i64,
    /// Timestamp of the last admitted **collective** (roster) reply.
    last_collective: Option<i64>,
    /// Timestamp of the last admitted **named** reply, per elected agent id.
    last_named: HashMap<String, i64>,
}

impl BurstCoalescer {
    /// New coalescer with the given burst window in seconds.
    pub fn new(window_secs: i64) -> Self {
        Self {
            window_secs,
            last_collective: None,
            last_named: HashMap::new(),
        }
    }

    /// Decide whether a **collective** roster reply should proceed at `now`.
    /// Returns `true` to send (and records it as the new anchor), `false` to
    /// coalesce into the recent one. The first call always admits.
    pub fn admit_collective(&mut self, now: i64) -> bool {
        match self.last_collective {
            Some(prev) if now.saturating_sub(prev) < self.window_secs => false,
            _ => {
                self.last_collective = Some(now);
                true
            }
        }
    }

    /// Decide whether a **named** reply to `agent` should proceed at `now`.
    /// Same anchoring as [`admit_collective`], but keyed per agent so a burst of
    /// messages to Nora coalesces without muting a message to Bruno.
    pub fn admit_named(&mut self, agent: &str, now: i64) -> bool {
        let admit = match self.last_named.get(agent) {
            Some(&prev) if now.saturating_sub(prev) < self.window_secs => false,
            _ => true,
        };
        if admit {
            self.last_named.insert(agent.to_string(), now);
        }
        admit
    }
}

impl Default for BurstCoalescer {
    fn default() -> Self {
        Self::new(DEFAULT_BURST_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_backlog_catches_only_pre_start_old_messages() {
        let start = 1_000_000;
        // 6 minutes before start → stale.
        assert!(is_stale_backlog(start - 360, start, DEFAULT_STALE_SECS));
        // 2 minutes before start → fresh enough, answered.
        assert!(!is_stale_backlog(start - 120, start, DEFAULT_STALE_SECS));
        // Exactly at the threshold boundary is NOT stale (strictly older).
        assert!(!is_stale_backlog(start - DEFAULT_STALE_SECS, start, DEFAULT_STALE_SECS));
        // A live message sent after startup is never stale.
        assert!(!is_stale_backlog(start + 5, start, DEFAULT_STALE_SECS));
    }

    #[test]
    fn collective_burst_collapses_to_one_within_window() {
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_collective(100), "first collective admits");
        assert!(!c.admit_collective(110), "10s later → coalesced");
        assert!(!c.admit_collective(129), "29s later → coalesced");
        assert!(c.admit_collective(131), "31s after the admitted one → admits again");
        assert!(!c.admit_collective(140), "within window of the new anchor → coalesced");
    }

    #[test]
    fn named_burst_is_per_agent() {
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_named("nora", 100), "first to nora admits");
        assert!(!c.admit_named("nora", 105), "quick repeat to nora coalesces");
        // A different agent in the same window is unaffected.
        assert!(c.admit_named("bruno", 106), "bruno is independent");
        // After the window, nora admits again.
        assert!(c.admit_named("nora", 131), "nora window elapsed");
    }

    #[test]
    fn suppressed_calls_do_not_extend_the_window() {
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_collective(100));
        // A flood of suppressed calls must NOT push the anchor forward.
        for t in [105, 110, 120, 125] {
            assert!(!c.admit_collective(t));
        }
        // 31s after the ADMITTED one (100), not after the last suppressed (125).
        assert!(c.admit_collective(131));
    }
}
