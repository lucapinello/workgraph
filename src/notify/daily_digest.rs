//! One calm daily digest: the single pacing layer every *proactive* DM flows
//! through, so a human is never nickel-and-dimed by four separate bots.
//!
//! # Why this exists
//!
//! Three proactive senders grew up independently — the [reminder engine]
//! (`⏰ Calendar` rows + ad-hoc "remind me Thursday…"), the meal-[feedback] ask
//! ("how was last night's salmon?"), and errand nudges ("market run at 9"). Left
//! alone, each one DMs on its own schedule and the household gets pinged all day.
//! Panel recommendation #5 settled the more-vs-fewer-nudges debate by making
//! *both* sides right: keep every nudge, but **pace** them.
//!
//! [reminder engine]: crate::notify::reminder
//! [feedback]: crate::notify::meal_feedback
//!
//! # The contract
//!
//! Every proactive item is a [`Nudge`] with an [`Urgency`]:
//!
//! * **[`Urgency::Bundled`]** — non-urgent (a feedback ask, a heads-up, a
//!   soft errand reminder). It never DMs on its own. It waits in a per-human
//!   queue and leaves in **one morning digest** (default 08:00 local, per-person
//!   configurable): `Today: PT check-in at 19:30 · how was last night's salmon? ·
//!   market run at 9 — list attached`. At most one digest per human per day.
//!
//! * **[`Urgency::TimeCritical`]** — explicitly-timed (an errand nudge *at*
//!   departure time, a plan reminder with a real clock). It still fires
//!   standalone the moment it is due — even inside quiet hours — because the
//!   whole point is to reach the human on time. But it **counts against a daily
//!   cap** (default 3 standalone DMs/person/day). Once the cap is spent, further
//!   time-critical items **fold into the next digest with an honest line** rather
//!   than pile on.
//!
//! # Quiet hours
//!
//! Per-person quiet hours (default 22:00–07:30) mean *no proactive DMs* in that
//! window — the morning digest never lands at 3am, and a bundled item queued
//! overnight waits for the digest. The **only** exception is an explicitly-timed
//! time-critical item, which passes through (that is what "explicitly-timed"
//! means — the human asked to be reached then).
//!
//! # Pure and restart-safe
//!
//! Like the reminder engine, everything here is pure over an injected `now`
//! (a family-local wall-clock [`NaiveDateTime`]) and the per-human [`PacingState`]
//! is serialisable, persisted as JSON (`<root>/.casa/digest-state.json`) so the
//! daily cap, the "already sent today's digest" flag, and the pending queue all
//! survive a restart. No clock, no filesystem, and no live bot are needed to
//! unit-test the bundling / cap / quiet-hours / passthrough behaviour.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::atomic_file::write_atomic;

// ---------------------------------------------------------------------------
// The nudge
// ---------------------------------------------------------------------------

/// How time-sensitive a proactive item is — the single bit that decides whether
/// it may DM standalone or must wait for the morning digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// Non-urgent. Always held for the next morning digest; never DMs alone.
    Bundled,
    /// Explicitly-timed. Fires standalone at its due time (even in quiet hours),
    /// subject to the daily standalone cap.
    TimeCritical,
}

/// Which sender a nudge came from — carried for logging/telemetry and so a future
/// policy can pace by source. It does not change the pacing decision today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NudgeKind {
    /// A `⏰` plan reminder or ad-hoc "remind me…" (see [`crate::notify::reminder`]).
    Reminder,
    /// A meal-feedback ask (see [`crate::notify::meal_feedback`]).
    FeedbackAsk,
    /// An errand nudge ("market run at 9 — list attached").
    ErrandNudge,
    /// A conversational-task lifecycle notification — "on it", "done", or an
    /// honest "snag" — reporting back on a task the human *asked for* in chat
    /// (see [`crate::notify::lifecycle`]). Unlike every other kind these are
    /// direct REPLIES to an ask, not proactive pings, so they are exempt from
    /// the daily standalone cap: a reply the human is actively waiting for
    /// always reaches them standalone and never overflows into the morning
    /// digest. (Earlier this kind was capped like the others; that silently
    /// dropped real replies once a burst of asks spent the cap — see
    /// [`DigestStore::offer`].)
    Lifecycle,
    /// Any other future proactive DM. New senders route through here by default.
    Proactive,
}

/// One proactive item offered to the pacing layer: who to reach, when it is due,
/// how urgent it is, and the already-composed family-voice line to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nudge {
    /// Stable de-dupe key (caller-supplied; e.g. a reminder id). The pacing store
    /// never fires or queues the same id twice.
    pub id: String,
    /// Display name of the human to reach, e.g. `"Luca"`.
    pub recipient: String,
    /// Which sender produced it.
    pub kind: NudgeKind,
    /// Bundled vs time-critical.
    pub urgency: Urgency,
    /// When it becomes due (family-local wall clock). Before this, the nudge is
    /// [`Offer::Pending`] — neither sent nor queued.
    pub due: NaiveDateTime,
    /// The clean, family-voice one-liner. Used verbatim as a standalone DM body
    /// and as this item's line inside the digest, e.g. `"market run at 9 — list
    /// attached"` or `"how was last night's salmon?"`.
    pub text: String,
}

impl Nudge {
    /// A concise constructor for a bundled (non-urgent) nudge.
    pub fn bundled(
        id: impl Into<String>,
        recipient: impl Into<String>,
        kind: NudgeKind,
        due: NaiveDateTime,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            recipient: recipient.into(),
            kind,
            urgency: Urgency::Bundled,
            due,
            text: text.into(),
        }
    }

    /// A concise constructor for a time-critical (explicitly-timed) nudge.
    pub fn time_critical(
        id: impl Into<String>,
        recipient: impl Into<String>,
        kind: NudgeKind,
        due: NaiveDateTime,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            recipient: recipient.into(),
            kind,
            urgency: Urgency::TimeCritical,
            due,
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Policy (defaults + per-person overrides)
// ---------------------------------------------------------------------------

/// A resolved set of pacing knobs for one person: when the digest lands, how many
/// standalone DMs a day, and the quiet-hours window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonPolicy {
    /// Wall-clock time the morning digest is delivered (default 08:00).
    pub digest_time: NaiveTime,
    /// Max time-critical standalone DMs per day before overflow folds into the
    /// digest (default 3).
    pub standalone_cap: u32,
    /// Quiet-hours start, inclusive (default 22:00).
    pub quiet_start: NaiveTime,
    /// Quiet-hours end, exclusive (default 07:30).
    pub quiet_end: NaiveTime,
}

impl Default for PersonPolicy {
    fn default() -> Self {
        Self {
            digest_time: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
            standalone_cap: 3,
            quiet_start: NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            quiet_end: NaiveTime::from_hms_opt(7, 30, 0).unwrap(),
        }
    }
}

/// A per-person override of any subset of the [`PersonPolicy`] knobs. Anything
/// left `None` falls back to the household default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersonOverride {
    pub digest_time: Option<NaiveTime>,
    pub standalone_cap: Option<u32>,
    pub quiet_start: Option<NaiveTime>,
    pub quiet_end: Option<NaiveTime>,
}

/// The household pacing policy: one default [`PersonPolicy`] plus per-person
/// overrides (a night-owl teen with later quiet hours, a parent who wants the
/// digest at 07:00, etc.).
#[derive(Debug, Clone, Default)]
pub struct DigestPolicy {
    default: PersonPolicy,
    overrides: BTreeMap<String, PersonOverride>,
}

impl DigestPolicy {
    /// A policy with the standard family defaults and no per-person overrides.
    pub fn new() -> Self {
        Self {
            default: PersonPolicy::default(),
            overrides: BTreeMap::new(),
        }
    }

    /// Replace the household default policy (all people without an override).
    pub fn with_default(mut self, default: PersonPolicy) -> Self {
        self.default = default;
        self
    }

    /// Register a per-person override.
    pub fn with_override(mut self, person: impl Into<String>, ov: PersonOverride) -> Self {
        self.overrides.insert(person.into(), ov);
        self
    }

    /// Resolve the effective policy for `person`, applying any override on top of
    /// the household default.
    pub fn for_person(&self, person: &str) -> PersonPolicy {
        let mut p = self.default;
        if let Some(ov) = self.overrides.get(person) {
            if let Some(t) = ov.digest_time {
                p.digest_time = t;
            }
            if let Some(c) = ov.standalone_cap {
                p.standalone_cap = c;
            }
            if let Some(t) = ov.quiet_start {
                p.quiet_start = t;
            }
            if let Some(t) = ov.quiet_end {
                p.quiet_end = t;
            }
        }
        p
    }
}

/// True when wall-clock time `t` falls in the quiet-hours window `[start, end)`.
///
/// The window normally **wraps midnight** (start 22:00, end 07:30): a time is
/// quiet if it is at/after `start` OR before `end`. When `start == end` the
/// window is empty (never quiet); when `start < end` it is a same-day window.
/// `start` is inclusive and `end` is exclusive so a digest configured exactly at
/// `quiet_end` (07:30) is allowed to land.
pub fn in_quiet_hours(t: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start == end {
        false
    } else if start < end {
        t >= start && t < end
    } else {
        t >= start || t < end
    }
}

// ---------------------------------------------------------------------------
// The pacing decision
// ---------------------------------------------------------------------------

/// What the pacing layer decided to do with one offered [`Nudge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Not due yet — neither sent nor queued. Offer it again after `due`.
    Pending,
    /// Send this exact text as a standalone DM right now (a time-critical item
    /// under the daily cap). The standalone counter has been incremented.
    SendNow(String),
    /// Held for the next morning digest. `overflow` is true when this was a
    /// time-critical item bumped past the daily cap (it gets an honest line in
    /// the digest); false for an ordinary bundled item.
    Queued { overflow: bool },
    /// Already handled (same id offered before) — ignored.
    Duplicate,
}

/// One line waiting in a person's morning digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestItem {
    /// The originating nudge id (de-dupe within the pending queue).
    pub id: String,
    /// The family-voice line to show.
    pub text: String,
    /// Which sender it came from.
    pub kind: NudgeKind,
    /// True when this is a time-critical item that overflowed the daily cap — it
    /// is grouped under an honest "held back so I wouldn't over-ping you" line.
    pub overflow: bool,
}

/// Per-human pacing state for the current day: the standalone counter, whether
/// today's digest has gone out, and the queued digest items. Serialisable so the
/// whole thing is restart-safe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacingState {
    /// The date the counters below apply to (family-local). `None` until the
    /// first offer/tick sets it.
    #[serde(default)]
    day: Option<NaiveDate>,
    /// Time-critical standalone DMs already sent today.
    #[serde(default)]
    standalone_sent: u32,
    /// Whether today's single digest has already been delivered.
    #[serde(default)]
    digest_sent: bool,
    /// Items waiting for the next digest (carried across days until delivered).
    #[serde(default)]
    pending: Vec<DigestItem>,
    /// Every nudge id ever handled (fired standalone or queued), so re-offering
    /// the same id is a no-op — exactly-once, mirroring the reminder [`FiredLog`].
    ///
    /// [`FiredLog`]: crate::notify::reminder::FiredLog
    #[serde(default)]
    seen: Vec<String>,
}

impl PacingState {
    /// Roll the daily counters to `day` if the stored day is older. The pending
    /// queue is **preserved** across the roll (undelivered items still owe a
    /// digest); only the standalone counter and the digest-sent flag reset.
    fn ensure_day(&mut self, day: NaiveDate) {
        match self.day {
            Some(d) if d == day => {}
            _ => {
                self.day = Some(day);
                self.standalone_sent = 0;
                self.digest_sent = false;
            }
        }
    }

    /// Standalone DMs already sent today (test/status helper).
    pub fn standalone_sent(&self) -> u32 {
        self.standalone_sent
    }

    /// Whether today's digest has been delivered (test/status helper).
    pub fn digest_sent(&self) -> bool {
        self.digest_sent
    }

    /// The items currently queued for the next digest (test/status helper).
    pub fn pending(&self) -> &[DigestItem] {
        &self.pending
    }
}

/// The durable, per-human pacing store — recipient display name → [`PacingState`].
/// Persisted as JSON at `<root>/.casa/digest-state.json` so the cap, the
/// digest-sent flag, and the pending queue survive a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DigestStore {
    #[serde(default)]
    people: BTreeMap<String, PacingState>,
}

impl DigestStore {
    /// Standard on-disk path: `<root>/.casa/digest-state.json`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("digest-state.json")
    }

    /// Load the store from `path`, or an empty store when it is missing / empty /
    /// corrupt (a lost store at worst re-paces within a day, never crashes).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    /// Persist the store atomically to `path` (creating `.casa/` as needed).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, json.as_bytes())
    }

    /// Borrow (creating if absent) the pacing state for `recipient`.
    pub fn state_mut(&mut self, recipient: &str) -> &mut PacingState {
        self.people.entry(recipient.to_string()).or_default()
    }

    /// Borrow the pacing state for `recipient`, if any.
    pub fn state(&self, recipient: &str) -> Option<&PacingState> {
        self.people.get(recipient)
    }

    /// Forget one lifecycle pacing decision after its standalone transport was
    /// not confirmed, allowing the next scheduler tick to offer it again.
    ///
    /// Lifecycle replies bypass both the proactive cap and the pending digest,
    /// so re-arming removes only the matching `seen` id. This deliberately does
    /// not apply to queued or cap-counted nudges, whose rollback semantics are
    /// different.
    pub fn rearm_lifecycle(&mut self, recipient: &str, id: &str) -> bool {
        if !id.starts_with("lifecycle:") {
            return false;
        }
        let Some(state) = self.people.get_mut(recipient) else {
            return false;
        };
        if state.pending.iter().any(|item| item.id == id) {
            return false;
        }
        let before = state.seen.len();
        state.seen.retain(|seen_id| seen_id != id);
        state.seen.len() != before
    }

    /// Offer one nudge to the pacing layer at `now`.
    ///
    /// * Not yet due → [`Offer::Pending`] (nothing recorded).
    /// * Already-seen id → [`Offer::Duplicate`].
    /// * Bundled → queued for the next digest ([`Offer::Queued`] `overflow:false`).
    /// * Lifecycle report-back ([`NudgeKind::Lifecycle`]) → always
    ///   [`Offer::SendNow`]: a reply to the human's own ask is never capped and
    ///   never counts against the standalone budget.
    /// * Other time-critical under the daily cap → [`Offer::SendNow`] (counter++),
    ///   even inside quiet hours.
    /// * Other time-critical over the cap → queued as overflow ([`Offer::Queued`]
    ///   `overflow:true`) for the next digest's honest line.
    ///
    /// This is the single choke point recommendation #5 asks every proactive
    /// sender to route through.
    pub fn offer(&mut self, nudge: &Nudge, now: NaiveDateTime, policy: &DigestPolicy) -> Offer {
        if now < nudge.due {
            return Offer::Pending;
        }
        let cap = policy.for_person(&nudge.recipient).standalone_cap;
        let st = self.people.entry(nudge.recipient.clone()).or_default();
        st.ensure_day(now.date());
        if st.seen.iter().any(|id| id == &nudge.id) {
            return Offer::Duplicate;
        }
        match nudge.urgency {
            Urgency::Bundled => {
                st.seen.push(nudge.id.clone());
                st.pending.push(DigestItem {
                    id: nudge.id.clone(),
                    text: nudge.text.clone(),
                    kind: nudge.kind,
                    overflow: false,
                });
                Offer::Queued { overflow: false }
            }
            // A lifecycle report-back is a DIRECT REPLY to an ask the human made
            // in chat — not an unsolicited proactive ping — so it ALWAYS reaches
            // them standalone and is never capped or counted against the
            // proactive standalone budget. The daily cap exists to stop
            // *unsolicited* senders (reminders, errands, feedback asks) from
            // flooding; silencing a reply the human is actively waiting for
            // breaks the conversational contract. This was a live regression:
            // Luca made a burst of meal-swap asks, the cap of 3 was spent, and
            // his "Done — Wednesday is now pesto ✅" was folded into the next
            // morning's digest instead of sent, so the loop looked broken (see
            // .casa/digest-state.json overflow:true entries). If N asks come in,
            // N replies are proportionate — the human opened each loop.
            Urgency::TimeCritical if nudge.kind == NudgeKind::Lifecycle => {
                st.seen.push(nudge.id.clone());
                Offer::SendNow(nudge.text.clone())
            }
            Urgency::TimeCritical if st.standalone_sent < cap => {
                st.seen.push(nudge.id.clone());
                st.standalone_sent += 1;
                Offer::SendNow(nudge.text.clone())
            }
            Urgency::TimeCritical => {
                // Cap spent — fold into the next digest with an honest line
                // rather than pile a fourth standalone ping on the human.
                st.seen.push(nudge.id.clone());
                st.pending.push(DigestItem {
                    id: nudge.id.clone(),
                    text: nudge.text.clone(),
                    kind: nudge.kind,
                    overflow: true,
                });
                Offer::Queued { overflow: true }
            }
        }
    }

    /// Whether `recipient`'s morning digest is ready to send at `now`: it is the
    /// digest hour or later, we are out of quiet hours, today's digest has not
    /// gone yet, and there is at least one pending item (never send an empty
    /// digest).
    pub fn digest_due(&self, recipient: &str, now: NaiveDateTime, policy: &DigestPolicy) -> bool {
        let p = policy.for_person(recipient);
        let st = match self.people.get(recipient) {
            Some(st) => st,
            None => return false,
        };
        // A day roll that has not been applied yet still means "not sent today".
        let digest_sent_today = st.digest_sent && st.day == Some(now.date());
        !digest_sent_today
            && !st.pending.is_empty()
            && now.time() >= p.digest_time
            && !in_quiet_hours(now.time(), p.quiet_start, p.quiet_end)
    }

    /// If `recipient`'s digest is [`digest_due`](Self::digest_due) at `now`,
    /// compose it, mark today's digest delivered, clear the pending queue, and
    /// return the DM text. Otherwise return `None`.
    ///
    /// At most one digest per person per day: a second call the same day (after
    /// the first delivered) returns `None`.
    pub fn emit_digest(
        &mut self,
        recipient: &str,
        now: NaiveDateTime,
        policy: &DigestPolicy,
    ) -> Option<String> {
        if !self.digest_due(recipient, now, policy) {
            return None;
        }
        let st = self.people.get_mut(recipient)?;
        st.ensure_day(now.date());
        let text = compose_digest(&st.pending);
        st.pending.clear();
        st.digest_sent = true;
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Digest composition (family voice)
// ---------------------------------------------------------------------------

/// Compose the morning digest body from the queued items. Ordinary bundled items
/// become one calm line — `Today: <a> · <b> · <c>` — and any overflow
/// (time-critical items that were held back because the daily cap was spent) get
/// a second, honest line so the delay is never hidden. Family voice, no jargon.
///
/// Returns an empty string for no items (callers gate on [`DigestStore::digest_due`]
/// so this is only reached with at least one item in practice).
pub fn compose_digest(items: &[DigestItem]) -> String {
    let normal: Vec<&str> = items
        .iter()
        .filter(|i| !i.overflow)
        .map(|i| i.text.as_str())
        .collect();
    let overflow: Vec<&str> = items
        .iter()
        .filter(|i| i.overflow)
        .map(|i| i.text.as_str())
        .collect();

    let mut lines: Vec<String> = Vec::new();
    if !normal.is_empty() {
        lines.push(format!("Today: {}", normal.join(" \u{b7} ")));
    }
    if !overflow.is_empty() {
        lines.push(format!(
            "Also, I held these back yesterday so I wouldn't over-ping you: {}.",
            overflow.join(" \u{b7} ")
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn t(h: u32, mi: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, mi, 0).unwrap()
    }

    // --- quiet hours -------------------------------------------------------

    #[test]
    fn quiet_hours_wrap_midnight_default_window() {
        let (start, end) = (t(22, 0), t(7, 30));
        // Deep night and early morning are quiet.
        assert!(in_quiet_hours(t(23, 0), start, end));
        assert!(in_quiet_hours(t(3, 0), start, end));
        assert!(in_quiet_hours(t(7, 0), start, end));
        // Start is inclusive; end is exclusive.
        assert!(in_quiet_hours(t(22, 0), start, end));
        assert!(!in_quiet_hours(t(7, 30), start, end));
        // Daytime is not quiet.
        assert!(!in_quiet_hours(t(8, 0), start, end));
        assert!(!in_quiet_hours(t(19, 30), start, end));
        assert!(!in_quiet_hours(t(21, 59), start, end));
    }

    #[test]
    fn quiet_hours_same_day_window_and_empty_window() {
        // Non-wrapping window 13:00-14:00 (a hypothetical nap).
        assert!(in_quiet_hours(t(13, 30), t(13, 0), t(14, 0)));
        assert!(!in_quiet_hours(t(12, 0), t(13, 0), t(14, 0)));
        // Empty window (start == end) is never quiet.
        assert!(!in_quiet_hours(t(9, 0), t(9, 0), t(9, 0)));
    }

    // --- bundling ----------------------------------------------------------

    #[test]
    fn digest_bundles_all_non_urgent_into_one_morning_line() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        // Three non-urgent items queued the evening before / overnight.
        let items = [
            Nudge::bundled("r1", "Luca", NudgeKind::Reminder, dt(2026, 7, 13, 6, 0),
                "PT check-in at 19:30"),
            Nudge::bundled("f1", "Luca", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 6, 0),
                "how was last night's salmon?"),
            Nudge::bundled("e1", "Luca", NudgeKind::ErrandNudge, dt(2026, 7, 13, 6, 0),
                "market run at 9 — list attached"),
        ];
        for n in &items {
            // Offered at 06:30 (inside quiet hours) — all held, none sent.
            assert_eq!(store.offer(n, dt(2026, 7, 13, 6, 30), &policy),
                       Offer::Queued { overflow: false });
        }
        assert_eq!(store.state("Luca").unwrap().pending().len(), 3);
        assert!(!store.state("Luca").unwrap().digest_sent());

        // At 08:00 the single digest lands, bundling all three.
        let msg = store.emit_digest("Luca", dt(2026, 7, 13, 8, 0), &policy).unwrap();
        assert_eq!(
            msg,
            "Today: PT check-in at 19:30 \u{b7} how was last night's salmon? \
             \u{b7} market run at 9 — list attached"
        );
        assert!(store.state("Luca").unwrap().digest_sent());
        assert!(store.state("Luca").unwrap().pending().is_empty());
    }

    #[test]
    fn at_most_one_digest_per_person_per_day() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        store.offer(
            &Nudge::bundled("f1", "Nadin", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 6, 0), "how was dinner?"),
            dt(2026, 7, 13, 6, 0), &policy);
        // First emit at 08:00 delivers.
        assert!(store.emit_digest("Nadin", dt(2026, 7, 13, 8, 0), &policy).is_some());
        // A later item the same day queues but does NOT trigger a second digest.
        store.offer(
            &Nudge::bundled("f2", "Nadin", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 12, 0), "how was lunch?"),
            dt(2026, 7, 13, 12, 0), &policy);
        assert!(store.emit_digest("Nadin", dt(2026, 7, 13, 12, 5), &policy).is_none());
        assert_eq!(store.state("Nadin").unwrap().pending().len(), 1);
    }

    #[test]
    fn digest_not_sent_before_time_or_when_empty() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        // Empty queue → nothing to send even at 09:00.
        store.state_mut("Luca");
        assert!(store.emit_digest("Luca", dt(2026, 7, 13, 9, 0), &policy).is_none());
        // Queued but before the 08:00 digest hour → not yet.
        store.offer(
            &Nudge::bundled("f1", "Luca", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 6, 0), "how was dinner?"),
            dt(2026, 7, 13, 6, 0), &policy);
        assert!(!store.digest_due("Luca", dt(2026, 7, 13, 7, 45), &policy));
        assert!(store.emit_digest("Luca", dt(2026, 7, 13, 7, 45), &policy).is_none());
    }

    // --- time-critical passthrough & cap -----------------------------------

    #[test]
    fn time_critical_fires_standalone_even_in_quiet_hours() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        // An explicitly-timed errand nudge due at 06:15 (inside quiet hours).
        let n = Nudge::time_critical("e-depart", "Luca", NudgeKind::ErrandNudge,
            dt(2026, 7, 13, 6, 15), "leave now for the 6:40 train");
        assert_eq!(
            store.offer(&n, dt(2026, 7, 13, 6, 15), &policy),
            Offer::SendNow("leave now for the 6:40 train".to_string())
        );
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 1);
    }

    #[test]
    fn standalone_cap_then_overflow_folds_into_digest() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new(); // cap = 3
        let day = |h, mi, s: &str| Nudge::time_critical(
            format!("tc-{h}{mi}"), "Luca", NudgeKind::Reminder, dt(2026, 7, 13, h, mi), s);
        // First three fire standalone.
        for (h, mi, s) in [(9, 0, "a"), (10, 0, "b"), (11, 0, "c")] {
            assert!(matches!(store.offer(&day(h, mi, s), dt(2026, 7, 13, h, mi), &policy),
                             Offer::SendNow(_)));
        }
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 3);
        // The fourth overflows into the digest with an honest line.
        assert_eq!(
            store.offer(&day(12, 0, "d — held"), dt(2026, 7, 13, 12, 0), &policy),
            Offer::Queued { overflow: true }
        );
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 3, "cap not exceeded");
        assert_eq!(store.state("Luca").unwrap().pending().len(), 1);
    }

    #[test]
    fn lifecycle_replies_bypass_the_standalone_cap() {
        // A burst of asks spends the proactive cap, then a lifecycle report-back
        // still fires standalone — it is a reply to the human's own ask, not a
        // proactive ping. This is the live regression: the pesto "done" reply was
        // folded into the digest because the cap was spent by earlier asks.
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new(); // cap = 3
        // Spend the whole proactive cap with reminders.
        for (h, s) in [(9, "a"), (10, "b"), (11, "c")] {
            let n = Nudge::time_critical(
                format!("tc-{h}"), "Luca", NudgeKind::Reminder, dt(2026, 7, 13, h, 0), s);
            assert!(matches!(store.offer(&n, dt(2026, 7, 13, h, 0), &policy), Offer::SendNow(_)));
        }
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 3, "cap spent");

        // A further reminder overflows (control) …
        let more = Nudge::time_critical(
            "tc-more", "Luca", NudgeKind::Reminder, dt(2026, 7, 13, 12, 0), "held");
        assert_eq!(
            store.offer(&more, dt(2026, 7, 13, 12, 0), &policy),
            Offer::Queued { overflow: true }
        );

        // … but a lifecycle report-back for the SAME person at the SAME time
        // still sends standalone, and does not touch the proactive counter.
        let reply = Nudge::time_critical(
            "lifecycle:pesto:done", "Luca", NudgeKind::Lifecycle,
            dt(2026, 7, 13, 12, 0), "Done — Wednesday is now pesto ✅");
        assert_eq!(
            store.offer(&reply, dt(2026, 7, 13, 12, 0), &policy),
            Offer::SendNow("Done — Wednesday is now pesto ✅".to_string())
        );
        assert_eq!(
            store.state("Luca").unwrap().standalone_sent(), 3,
            "lifecycle reply does not consume the proactive budget"
        );
        // Exactly-once still holds: re-offering the same reply is a no-op.
        assert_eq!(
            store.offer(&reply, dt(2026, 7, 13, 12, 30), &policy),
            Offer::Duplicate
        );
    }

    #[test]
    fn digest_shows_honest_overflow_line() {
        let items = vec![
            DigestItem { id: "b1".into(), text: "how was dinner?".into(),
                         kind: NudgeKind::FeedbackAsk, overflow: false },
            DigestItem { id: "o1".into(), text: "pharmacy pickup was due at 4".into(),
                         kind: NudgeKind::ErrandNudge, overflow: true },
        ];
        let msg = compose_digest(&items);
        assert_eq!(
            msg,
            "Today: how was dinner?\n\
             Also, I held these back yesterday so I wouldn't over-ping you: \
             pharmacy pickup was due at 4."
        );
    }

    #[test]
    fn overflow_only_digest_has_no_today_line() {
        let items = vec![DigestItem {
            id: "o1".into(), text: "pharmacy pickup".into(),
            kind: NudgeKind::ErrandNudge, overflow: true,
        }];
        let msg = compose_digest(&items);
        assert!(!msg.contains("Today:"));
        assert!(msg.starts_with("Also, I held these back"));
    }

    // --- day roll & exactly-once -------------------------------------------

    #[test]
    fn day_roll_resets_cap_and_digest_flag_but_keeps_pending() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        // Spend the cap and queue one overflow on day 1.
        for (h, s) in [(9, "a"), (10, "b"), (11, "c"), (12, "d")] {
            let n = Nudge::time_critical(format!("d1-{h}"), "Luca", NudgeKind::Reminder,
                dt(2026, 7, 13, h, 0), s);
            store.offer(&n, dt(2026, 7, 13, h, 0), &policy);
        }
        // Deliver day-1 digest so pending clears.
        store.emit_digest("Luca", dt(2026, 7, 13, 13, 0), &policy);
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 3);
        assert!(store.state("Luca").unwrap().digest_sent());

        // Day 2: a fresh time-critical offer resets the counter and fires again.
        let n = Nudge::time_critical("d2-1", "Luca", NudgeKind::Reminder,
            dt(2026, 7, 14, 9, 0), "new day ping");
        assert!(matches!(store.offer(&n, dt(2026, 7, 14, 9, 0), &policy), Offer::SendNow(_)));
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 1);
        assert!(!store.state("Luca").unwrap().digest_sent());
    }

    #[test]
    fn same_id_offered_twice_is_deduped() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        let n = Nudge::time_critical("once", "Luca", NudgeKind::Reminder,
            dt(2026, 7, 13, 9, 0), "ping");
        assert!(matches!(store.offer(&n, dt(2026, 7, 13, 9, 0), &policy), Offer::SendNow(_)));
        assert_eq!(store.offer(&n, dt(2026, 7, 13, 9, 1), &policy), Offer::Duplicate);
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 1, "no double count");
    }

    #[test]
    fn not_yet_due_is_pending_and_records_nothing() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        let n = Nudge::bundled("later", "Luca", NudgeKind::FeedbackAsk,
            dt(2026, 7, 13, 18, 0), "how was dinner?");
        assert_eq!(store.offer(&n, dt(2026, 7, 13, 12, 0), &policy), Offer::Pending);
        assert!(store.state("Luca").is_none() || store.state("Luca").unwrap().pending().is_empty());
    }

    // --- per-person policy -------------------------------------------------

    #[test]
    fn per_person_override_changes_digest_time_and_quiet_hours() {
        let policy = DigestPolicy::new().with_override(
            "Teo",
            PersonOverride {
                digest_time: Some(t(7, 0)),
                quiet_start: Some(t(23, 30)),
                quiet_end: Some(t(6, 30)),
                standalone_cap: Some(1),
                ..Default::default()
            },
        );
        let teo = policy.for_person("Teo");
        assert_eq!(teo.digest_time, t(7, 0));
        assert_eq!(teo.standalone_cap, 1);
        assert_eq!(teo.quiet_start, t(23, 30));
        // Default person is unchanged.
        let luca = policy.for_person("Luca");
        assert_eq!(luca.digest_time, t(8, 0));
        assert_eq!(luca.standalone_cap, 3);

        // Teo's tighter cap: the 2nd time-critical overflows.
        let mut store = DigestStore::default();
        let a = Nudge::time_critical("t1", "Teo", NudgeKind::Reminder, dt(2026, 7, 13, 9, 0), "a");
        let b = Nudge::time_critical("t2", "Teo", NudgeKind::Reminder, dt(2026, 7, 13, 10, 0), "b");
        assert!(matches!(store.offer(&a, dt(2026, 7, 13, 9, 0), &policy), Offer::SendNow(_)));
        assert_eq!(store.offer(&b, dt(2026, 7, 13, 10, 0), &policy), Offer::Queued { overflow: true });
    }

    #[test]
    fn digest_waits_out_quiet_hours_even_past_digest_time() {
        // A person whose digest hour (06:00) falls inside quiet hours: the digest
        // must wait until quiet hours end, never landing mid-quiet.
        let policy = DigestPolicy::new().with_override(
            "Owl",
            PersonOverride { digest_time: Some(t(6, 0)), ..Default::default() },
        );
        let mut store = DigestStore::default();
        store.offer(
            &Nudge::bundled("f1", "Owl", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 5, 0), "how was dinner?"),
            dt(2026, 7, 13, 5, 0), &policy);
        // 06:30 is past the digest hour but still inside default quiet (< 07:30).
        assert!(!store.digest_due("Owl", dt(2026, 7, 13, 6, 30), &policy));
        // 07:30 quiet ends → digest may land.
        assert!(store.digest_due("Owl", dt(2026, 7, 13, 7, 30), &policy));
    }

    // --- persistence round-trip --------------------------------------------

    #[test]
    fn digest_store_json_round_trip_preserves_pacing() {
        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        store.offer(
            &Nudge::time_critical("tc", "Luca", NudgeKind::Reminder, dt(2026, 7, 13, 9, 0), "ping"),
            dt(2026, 7, 13, 9, 0), &policy);
        store.offer(
            &Nudge::bundled("b", "Luca", NudgeKind::FeedbackAsk, dt(2026, 7, 13, 9, 0), "how was dinner?"),
            dt(2026, 7, 13, 9, 0), &policy);
        let json = serde_json::to_string(&store).unwrap();
        let back: DigestStore = serde_json::from_str(&json).unwrap();
        let st = back.state("Luca").unwrap();
        assert_eq!(st.standalone_sent(), 1);
        assert_eq!(st.pending().len(), 1);
        // The de-dupe set survived, so re-offering the fired id is still a no-op.
        let mut back = back;
        let n = Nudge::time_critical("tc", "Luca", NudgeKind::Reminder, dt(2026, 7, 13, 9, 30), "ping");
        assert_eq!(back.offer(&n, dt(2026, 7, 13, 9, 30), &policy), Offer::Duplicate);
    }
}
