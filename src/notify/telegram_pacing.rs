//! Timing policies that keep the group listener from over-replying:
//! startup stale-backlog suppression (Fix #1) and burst coalescing (Fix #2).
//!
//! Both are pure and clock-free — the caller passes Unix-second timestamps — so
//! they are fully unit-testable without a live listener. They are *defense in
//! depth* behind the Fix #0 bot-loop guard: even with the storm's root cause
//! fixed, the household should never see a bot answer a 20-minute-old message on
//! restart, nor fire four separate roster replies for a rapid burst.

use std::collections::{HashMap, HashSet};

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

/// Burst coalescer (Fix #2): collapse repeated elections that arrive **while a
/// prior turn is still composing** into a single reply. Anchored to the last
/// *admitted* reply — a suppressed election does not extend the window.
///
/// **Pending-only coalescing (BUG 2 fix, 2026-07-12).** Coalescing is valid ONLY
/// while the prior turn is still *pending* — elected but not yet sent. The window
/// exists to swallow a rapid flurry that lands during composition, not to swallow
/// a genuine follow-up that arrives *after* the bots already answered. Once a
/// turn's reply has been sent, the caller MUST record it via
/// [`mark_collective_sent`] / [`mark_named_sent`]; the next message then starts a
/// fresh turn regardless of the 30 s window. This is the live failure where
/// "why they don't reply?" arrived 21 s after the previous reply had already gone
/// out and was wrongly logged "concierge coalesced (burst)" — no answer at all.
///
/// The window still bounds a *pending* turn: if a turn is admitted but never
/// marked sent (a composer that hangs), a later message beyond `window_secs`
/// still admits, so the group is never muted indefinitely by a stuck turn.
#[derive(Debug, Clone)]
pub struct BurstCoalescer {
    window_secs: i64,
    /// Timestamp of the last admitted **collective** (roster) reply.
    last_collective: Option<i64>,
    /// Whether that collective turn is still *pending* (admitted, reply not yet
    /// sent). Only a pending turn coalesces a follow-up.
    collective_pending: bool,
    /// Timestamp of the last admitted **named** reply, per elected agent id.
    last_named: HashMap<String, i64>,
    /// Agents whose last admitted reply is still *pending* (composing).
    named_pending: HashSet<String>,
}

impl BurstCoalescer {
    /// New coalescer with the given burst window in seconds.
    pub fn new(window_secs: i64) -> Self {
        Self {
            window_secs,
            last_collective: None,
            collective_pending: false,
            last_named: HashMap::new(),
            named_pending: HashSet::new(),
        }
    }

    /// Decide whether a **collective** roster reply should proceed at `now`.
    /// Returns `true` to send (records it as the new anchor and marks it
    /// pending), `false` to coalesce into the still-composing one. Coalesces ONLY
    /// while the prior collective turn is pending AND within the window; a message
    /// that arrives after the prior reply was [`mark_collective_sent`] always
    /// admits. The first call always admits.
    pub fn admit_collective(&mut self, now: i64) -> bool {
        match self.last_collective {
            Some(prev)
                if self.collective_pending && now.saturating_sub(prev) < self.window_secs =>
            {
                false
            }
            _ => {
                self.last_collective = Some(now);
                self.collective_pending = true;
                true
            }
        }
    }

    /// Record that the last admitted **collective** reply has been sent, ending
    /// its pending turn. A subsequent [`admit_collective`] then starts a fresh
    /// turn regardless of the window.
    pub fn mark_collective_sent(&mut self) {
        self.collective_pending = false;
    }

    /// Decide whether a **named** reply to `agent` should proceed at `now`.
    /// Same pending-only anchoring as [`admit_collective`], keyed per agent so a
    /// burst to Nora coalesces without muting a message to Bruno.
    pub fn admit_named(&mut self, agent: &str, now: i64) -> bool {
        let admit = match self.last_named.get(agent) {
            Some(&prev)
                if self.named_pending.contains(agent)
                    && now.saturating_sub(prev) < self.window_secs =>
            {
                false
            }
            _ => true,
        };
        if admit {
            self.last_named.insert(agent.to_string(), now);
            self.named_pending.insert(agent.to_string());
        }
        admit
    }

    /// Record that the last admitted **named** reply to `agent` has been sent,
    /// ending its pending turn. A subsequent [`admit_named`] for the same agent
    /// then starts a fresh turn regardless of the window. A no-op if the agent has
    /// no pending turn.
    pub fn mark_named_sent(&mut self, agent: &str) {
        self.named_pending.remove(agent);
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

    // ---- BUG 2: pending-only coalescing (2026-07-12 swallowed follow-up) --

    #[test]
    fn collective_reply_sent_then_new_message_within_window_is_new_turn() {
        // The live failure: a reply went out, then a follow-up arrived 21s later
        // (well within the 30s window). Once the prior reply is SENT it is no
        // longer pending, so the follow-up must start a NEW turn, not coalesce.
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_collective(100), "first collective admits");
        c.mark_collective_sent(); // the roster reply went out
        assert!(
            c.admit_collective(121),
            "21s later, but the prior reply was SENT → new turn, not coalesced"
        );
    }

    #[test]
    fn named_reply_sent_then_new_message_within_window_is_new_turn() {
        // The exact reported case: "why they don't reply?" 21s after otto's reply
        // was already sent was wrongly logged "concierge coalesced (burst)".
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_named("otto", 11_48_15), "first concierge turn admits");
        c.mark_named_sent("otto"); // otto's reply was sent
        assert!(
            c.admit_named("otto", 11_48_36),
            "a follow-up after the reply was SENT must start a new turn"
        );
    }

    #[test]
    fn pending_turn_still_coalesces_a_flurry_during_composition() {
        // The dedup still works while a turn is composing (not yet sent): a rapid
        // flurry within the window collapses to one reply.
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_collective(100), "first admits, now pending");
        assert!(!c.admit_collective(105), "arrives during composition → coalesced");
        assert!(!c.admit_collective(120), "still composing → coalesced");
        // Same for named.
        assert!(c.admit_named("otto", 200), "first named admits, pending");
        assert!(!c.admit_named("otto", 210), "during composition → coalesced");
    }

    #[test]
    fn pending_turn_beyond_window_still_admits_so_a_stuck_composer_never_mutes() {
        // A turn admitted but never marked sent (a hung composer) must not mute
        // the group forever: a message past the window still admits.
        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_collective(100));
        // Never marked sent, but 31s on → admits anyway.
        assert!(c.admit_collective(131), "window bounds even a pending turn");

        let mut c = BurstCoalescer::new(30);
        assert!(c.admit_named("otto", 100));
        assert!(c.admit_named("otto", 131), "window bounds a pending named turn");
    }
}
