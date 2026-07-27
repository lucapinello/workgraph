//! The reminder engine: turns the weekly plan (and ad-hoc asks) from a document
//! into a service that actually **reaches** a human at the right time.
//!
//! # What fires
//!
//! Two sources feed one scheduler:
//!
//! * **Plan reminders** — rows in a plan's `## 3. Calendar` table shaped like a
//!   reminder, e.g. `| Tue 07-14 | 19:30 | ⏰ Reminder: Luca PT check-in | Otto |`.
//!   [`Reminder::from_calendar_event`] recognises the `⏰` / `Reminder:` shape,
//!   pulls out the recipient (first known family member named in the row), the
//!   owning voice (the Source column), the due wall-clock time, and a clean body.
//! * **Ad-hoc reminders** — "Otto remind me Thursday to defrost the trout" typed
//!   in Telegram. [`parse_reminder_intent`] detects the `remind` keyword plus a
//!   time expression and produces the same [`Reminder`] shape, persisted to the
//!   ad-hoc store so a later scheduler tick fires it.
//!
//! # Exactly once, restart-safe
//!
//! Every fired (or deliberately dropped) reminder is written to a persistent
//! [`FiredLog`] (`<root>/.casa/reminders-state.json`) **before** the DM is sent,
//! keyed by the reminder's stable [`Reminder::id`]. A restarted scheduler reads
//! the log and never re-fires an id it has already handled — the same
//! record-before-act discipline the cross-bot dedupe uses, made durable.
//!
//! # Missed while down
//!
//! A reminder whose due time passed while the scheduler was down still fires when
//! it comes back **if it is less than [`FirePolicy::late_max`] (2h) late**, tagged
//! `(late)` for honesty. Anything older is dropped (and logged) rather than
//! firing a stale nag hours after it mattered.
//!
//! Everything here is pure over an injected `now` (a wall-clock [`NaiveDateTime`]
//! in the family's timezone), so the whole fire/exactly-once/restart/late
//! behaviour is unit-testable without a clock, a filesystem, or a live bot.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use crate::atomic_file::write_atomic;
use crate::notify::family_plan::{CalendarEvent, PlanDoc};
use crate::notify::ownership::OwnerMap;

/// The alarm-clock emoji that prefixes a reminder, both in the plan and the DM.
const ALARM: char = '\u{23f0}';

/// Where a reminder originated — a plan calendar row, or an ad-hoc chat ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderSource {
    /// A `⏰ Reminder:` row in a weekly plan's calendar table.
    Plan,
    /// Registered from a Telegram "remind me …" message.
    AdHoc,
}

/// One reminder the engine can fire: who to reach, when, in whose voice, and with
/// what body. Independent of timezone — `due` is wall-clock in the family's local
/// zone and is compared against a `now` supplied in that same zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    /// Stable de-dupe key. Two ticks (or two processes across a restart) that see
    /// the *same* reminder derive the *same* id, so the [`FiredLog`] fires it once.
    pub id: String,
    /// Wall-clock instant the reminder is due (family-local time).
    pub due: NaiveDateTime,
    /// Display name of the human to DM, e.g. `"Luca"`. Empty when the row named
    /// no known member (the caller then falls back to the group).
    pub recipient: String,
    /// The voice/bot that owns and sends the reminder, e.g. `"otto"`.
    pub bot: String,
    /// The clean reminder body (no leading emoji, no `Reminder:` label), e.g.
    /// `"Luca PT check-in (if unanswered)"` or `"Defrost the trout"`.
    pub text: String,
    /// Where it came from.
    pub source: ReminderSource,
}

impl Reminder {
    /// Build a plan reminder from a calendar row, or `None` when the row is not
    /// reminder-shaped (no `⏰` and no `Reminder:` label) or has no usable time.
    ///
    /// `week_code` scopes the derived id to its plan week; `members` is the list
    /// of known family display names used to pick the recipient (first one named
    /// in the row).
    pub fn from_calendar_event(
        week_code: &str,
        ev: &CalendarEvent,
        members: &[String],
        owners: &OwnerMap,
    ) -> Option<Reminder> {
        if !is_reminder_event(&ev.event) {
            return None;
        }
        let due_date = ev.date?;
        let time = parse_clock(&ev.time)?;
        let due = due_date.and_time(time);
        let body = clean_reminder_body(&ev.event);
        let recipient = first_member(&ev.event, members).unwrap_or_default();
        let bot = resolve_plan_source(&ev.source, owners)?;
        // Stable id: week + date + time + a hash of the body, so a re-parse of the
        // same plan yields the same key (exactly-once across restarts) but two
        // different reminder rows never collide.
        let id = format!(
            "plan:{}:{}:{}:{:016x}",
            week_code,
            due_date.format("%Y-%m-%d"),
            ev.time.replace(':', ""),
            hash64(&body),
        );
        Some(Reminder {
            id,
            due,
            recipient,
            bot,
            text: body,
            source: ReminderSource::Plan,
        })
    }

    /// The family-voice DM body for a firing. `late` prepends an honest `(late)`
    /// tag; otherwise it reads exactly like the plan: `⏰ <body>`.
    pub fn message(&self, late: bool) -> String {
        if late {
            format!("{ALARM} (late) {}", self.text)
        } else {
            format!("{ALARM} {}", self.text)
        }
    }
}

/// Collect every reminder-shaped row in a parsed plan into [`Reminder`]s.
pub fn reminders_from_plan(plan: &PlanDoc, members: &[String], owners: &OwnerMap) -> Vec<Reminder> {
    plan.calendar
        .iter()
        .filter_map(|ev| Reminder::from_calendar_event(&plan.week_code, ev, members, owners))
        .collect()
}

/// True when a calendar Event cell is shaped like a reminder: it either carries
/// the `⏰` alarm emoji or an explicit `Reminder:` label.
pub fn is_reminder_event(event: &str) -> bool {
    event.contains(ALARM) || event.to_ascii_lowercase().contains("reminder")
}

/// Strip the leading `⏰` emoji and any `Reminder:` label from a plan event so the
/// DM body reads cleanly. `"⏰ Reminder: Luca PT check-in"` → `"Luca PT check-in"`.
fn clean_reminder_body(event: &str) -> String {
    let mut s = event.trim();
    // Drop a leading alarm emoji (and stray whitespace).
    s = s.trim_start_matches(ALARM).trim();
    // Drop a leading "Reminder:" / "reminder -" label, case-insensitively.
    let low = s.to_ascii_lowercase();
    if let Some(rest) = low.strip_prefix("reminder") {
        let cut = s.len() - rest.len();
        let after = s[cut..].trim_start();
        let after = after
            .trim_start_matches(|c: char| c == ':' || c == '-' || c == '\u{2014}')
            .trim();
        s = after;
    }
    s.to_string()
}

/// Resolve a plan Source cell to one stable household persona id.
///
/// Plan authors write display text, which may contain spaces and may change over
/// time. Preserve the first authored reference (before plan annotations such as
/// `/`, `(`, `§`, or `→`) and resolve it through the current household roster.
/// A non-empty unknown or ambiguous reference is rejected rather than guessed
/// from roster order. An empty Source remains empty so delivery may use only the
/// recipient's explicit bot binding.
///
/// `pub(crate)` so the errand engine ([`crate::notify::errand`]) uses the exact
/// same stable-identity rule.
pub(crate) fn resolve_plan_source(source: &str, owners: &OwnerMap) -> Option<String> {
    if source.trim().is_empty() {
        return Some(String::new());
    }
    let reference = source
        .split(['/', '(', '\u{00a7}', '\u{2192}'])
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    owners
        .resolve_unique_persona_ref(reference)
        .map(str::to_string)
}

/// Find the first known member display name that appears in `text`, matched
/// case-insensitively on a word boundary-ish basis.
///
/// `pub(crate)` so the errand engine reuses the identical person-resolution rule
/// to pull the runner out of a `🛒 Market run (Luca)` row.
pub(crate) fn first_member(text: &str, members: &[String]) -> Option<String> {
    let low = text.to_ascii_lowercase();
    members
        .iter()
        .filter(|m| !m.is_empty())
        .find(|m| contains_word(&low, &m.to_ascii_lowercase()))
        .cloned()
}

/// Whole-word-ish containment: `name` occurs in `hay` not flanked by other
/// alphanumerics (so `"luca"` matches `"Luca PT"` but not `"lucas"`).
fn contains_word(hay: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(name) {
        let start = from + pos;
        let end = start + name.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse a `HH:MM` (or `H:MM`) clock cell into a [`NaiveTime`]; `None` when empty
/// or unparseable. `pub(crate)` — the errand engine parses the market-run Time
/// cell with the same rule.
pub(crate) fn parse_clock(cell: &str) -> Option<NaiveTime> {
    let c = cell.trim();
    if c.is_empty() {
        return None;
    }
    NaiveTime::parse_from_str(c, "%H:%M")
        .ok()
        .or_else(|| NaiveTime::parse_from_str(c, "%H.%M").ok())
}

/// A `u64` hash of a string, used to keep reminder ids compact and stable.
/// `pub(crate)` so errand ids share the same compact stable-hash scheme.
pub(crate) fn hash64(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// Fire policy + decision
// ---------------------------------------------------------------------------

/// The windows that decide whether a due reminder fires on time, fires late, or
/// is dropped as stale.
#[derive(Debug, Clone, Copy)]
pub struct FirePolicy {
    /// A reminder fired within this of its due time is "on time" (no `(late)`
    /// tag) — absorbs the scheduler's tick jitter.
    pub on_time_grace: Duration,
    /// The oldest a missed reminder may be and still fire (tagged `(late)`).
    /// Anything older is dropped.
    pub late_max: Duration,
}

impl Default for FirePolicy {
    fn default() -> Self {
        Self {
            on_time_grace: Duration::minutes(2),
            late_max: Duration::hours(2),
        }
    }
}

/// What a single reminder should do at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireDecision {
    /// Due in the future — leave it alone.
    Pending,
    /// Send it now. `late` is true when it is past `on_time_grace` (fire with the
    /// honest `(late)` tag).
    Fire { late: bool },
    /// Too old (> `late_max`) — consume it silently with a log line, never send.
    Drop,
}

/// Decide what a reminder due at `due` should do at `now` under `policy`.
pub fn decide(due: NaiveDateTime, now: NaiveDateTime, policy: &FirePolicy) -> FireDecision {
    if now < due {
        return FireDecision::Pending;
    }
    let late_by = now - due;
    if late_by <= policy.on_time_grace {
        FireDecision::Fire { late: false }
    } else if late_by <= policy.late_max {
        FireDecision::Fire { late: true }
    } else {
        FireDecision::Drop
    }
}

/// One reminder selected to actually send this tick.
#[derive(Debug, Clone)]
pub struct Firing {
    /// The reminder to send.
    pub reminder: Reminder,
    /// Whether it fires with the `(late)` honesty tag.
    pub late: bool,
}

impl Firing {
    /// The exact DM text to send.
    pub fn message(&self) -> String {
        self.reminder.message(self.late)
    }
}

/// The outcome of one scheduler tick: what to send, and what was dropped as stale
/// (so the caller can log it — "the plan said 19:30, it's now past 22:00, skip").
#[derive(Debug, Clone, Default)]
pub struct TickResult {
    /// Reminders to send now, in due order.
    pub fired: Vec<Firing>,
    /// Reminders dropped as too-late this tick (already recorded consumed).
    pub dropped: Vec<Reminder>,
}

/// Run one scheduler tick over `reminders` at `now`, recording every fired or
/// dropped id into `log` (so a restart never repeats them). Returns what to send.
///
/// The log is mutated **in memory**; the caller persists it with [`FiredLog::save`]
/// *before* actually sending, giving restart-safe exactly-once (record-before-act).
pub fn tick(
    reminders: &[Reminder],
    log: &mut FiredLog,
    now: NaiveDateTime,
    policy: &FirePolicy,
) -> TickResult {
    let mut result = TickResult::default();
    for r in reminders {
        if log.contains(&r.id) {
            continue; // already fired or dropped in an earlier tick / before restart
        }
        match decide(r.due, now, policy) {
            FireDecision::Pending => {}
            FireDecision::Fire { late } => {
                log.record(
                    &r.id,
                    now,
                    if late { Outcome::Late } else { Outcome::OnTime },
                );
                result.fired.push(Firing {
                    reminder: r.clone(),
                    late,
                });
            }
            FireDecision::Drop => {
                log.record(&r.id, now, Outcome::Dropped);
                result.dropped.push(r.clone());
            }
        }
    }
    result.fired.sort_by_key(|f| f.reminder.due);
    result
}

// ---------------------------------------------------------------------------
// Persistent fired-log (exactly-once, restart-safe)
// ---------------------------------------------------------------------------

/// How a reminder id was consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Fired within the on-time grace.
    OnTime,
    /// Fired late (missed-while-down, within the late window).
    Late,
    /// Dropped as stale (older than the late window).
    Dropped,
}

/// One recorded firing/drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiredEntry {
    /// Wall-clock instant it was handled (ISO, family-local).
    pub at: String,
    /// What happened.
    pub outcome: Outcome,
}

/// The durable "already handled" set — reminder id → outcome. Persisted as JSON
/// so a restarted scheduler never re-fires. This is the exactly-once backbone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FiredLog {
    #[serde(default)]
    entries: BTreeMap<String, FiredEntry>,
}

impl FiredLog {
    /// Standard on-disk path: `<root>/.casa/reminders-state.json`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("reminders-state.json")
    }

    /// Load the log from `path`, or an empty log when it does not exist / is empty.
    /// A corrupt file yields an empty log rather than erroring — a lost log at
    /// worst re-fires within-window reminders, never crashes the scheduler.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    /// Persist the log atomically to `path` (creating `.casa/` as needed).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, json.as_bytes())
    }

    /// Whether this id has already been fired or dropped.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// Look up how an id was handled, if at all.
    pub fn outcome(&self, id: &str) -> Option<Outcome> {
        self.entries.get(id).map(|e| e.outcome)
    }

    /// Record an id as handled at `now` with `outcome`. Idempotent — re-recording
    /// keeps the first entry (the true first-fire time).
    pub fn record(&mut self, id: &str, now: NaiveDateTime, outcome: Outcome) {
        self.entries.entry(id.to_string()).or_insert(FiredEntry {
            at: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            outcome,
        });
    }

    /// Remove one handled id so a delivery that was not confirmed can be
    /// attempted by the next scheduler tick.
    ///
    /// The scheduler records before transport for crash safety. Its caller must
    /// therefore re-arm the exact id when transport exhausts its retries, then
    /// persist this log before returning. Other ids are left untouched.
    pub fn rearm(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Number of handled ids (for status/tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Ad-hoc reminders: intent parsing + store
// ---------------------------------------------------------------------------

/// A reminder parsed from a chat message, before it is assigned an id and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdHocIntent {
    /// When it should fire (family-local wall clock).
    pub due: NaiveDateTime,
    /// The clean body, e.g. `"defrost the trout"`.
    pub text: String,
    /// A one-line, family-voice confirmation to send back, e.g.
    /// `"Will do — Thursday morning ✓"`.
    pub confirmation: String,
}

/// Detect a reminder request in a plain chat message and resolve its due time.
///
/// Fires only when the message mentions `remind` **and** carries a time
/// expression this parser understands (a weekday, `today`/`tonight`/`tomorrow`,
/// a part-of-day word, or an explicit clock). `now` anchors relative times.
/// Returns `None` for messages that are not reminder requests.
pub fn parse_reminder_intent(text: &str, now: NaiveDateTime) -> Option<AdHocIntent> {
    let low = text.to_ascii_lowercase();
    if !low.contains("remind") {
        return None;
    }
    // FAIL CLOSED (date-reminder-fail). Three shapes carry the reminder verb and
    // are emphatically NOT a request to schedule one:
    //   (a) an interrogative after "remind me" — "remind me what was in Monday's
    //       risotto" asks the family memory; it is answered, never filed;
    //   (b) a day already elapsed — "remind me last Monday …" cannot be honoured
    //       by scheduling anything, so the composer asks instead;
    //   (c) a cancellation — "cancel the reminder about the dentist" must never
    //       create a second reminder (see [`parse_reminder_cancel`]).
    if is_reminder_read(&low) || names_past_day(&low) || parse_reminder_cancel(text).is_some() {
        return None;
    }

    let (date, day_label, day_kind) = resolve_day(&low, now.date());

    // FAIL CLOSED on a typed date that cannot be honoured as written: one already
    // gone, or one its own weekday word contradicts ("Tuesday, August 3, 2026" —
    // August 3 is a Monday). Which half the family meant is genuinely unknown,
    // and guessing puts the reminder on the wrong day; the composer asks instead.
    if day_kind == DayKind::CivilDate {
        if date < now.date() {
            return None;
        }
        if named_weekday(&low).is_some_and(|wd| wd != date.weekday()) {
            return None;
        }
    }

    let (time, time_label, had_time) = resolve_time(&low, now, date);

    // Require *some* time signal — a day word or an explicit clock — so bare
    // "remind me to call mum" (no when) is left to the normal composer.
    if day_label.is_none() && !had_time {
        return None;
    }

    let due = date.and_time(time);
    // Never file a moment that has already passed. With no day word, roll to
    // tomorrow ("remind me at 7", said at 8pm, means the next 7). With a bare
    // weekday, roll a whole week — "remind me Monday at 8am", said on Monday
    // afternoon, means the UPCOMING Monday, never this morning.
    //
    // An EXPLICIT day never rolls: "next Monday" and "August 3" already point at
    // one specific date, and silently adding a week to a date the family typed
    // out is a worse lie than filing it as asked.
    let due = if due <= now {
        match day_kind {
            DayKind::None => (date + Duration::days(1)).and_time(time),
            DayKind::BareWeekday => (date + Duration::days(7)).and_time(time),
            DayKind::Relative | DayKind::NextWeekday | DayKind::CivilDate => due,
        }
    } else {
        due
    };

    let body = extract_body(text);
    let confirmation = confirm_line(&day_label, &time_label, due, now);
    Some(AdHocIntent {
        due,
        text: body,
        confirmation,
    })
}

/// Turn a parsed intent into a stored [`Reminder`] with a stable ad-hoc id.
pub fn intent_to_reminder(intent: &AdHocIntent, recipient: &str, bot: &str) -> Reminder {
    let id = format!(
        "adhoc:{}:{:016x}",
        intent.due.format("%Y%m%dT%H%M"),
        hash64(&format!("{}|{}|{}", recipient, bot, intent.text)),
    );
    Reminder {
        id,
        due: intent.due,
        recipient: recipient.to_string(),
        bot: bot.to_string(),
        text: intent.text.clone(),
        source: ReminderSource::AdHoc,
    }
}

// ---------------------------------------------------------------------------
// Fail-closed guards: a read, a past day, and a cancellation (date-reminder-fail)
// ---------------------------------------------------------------------------

/// Words that turn "remind me …" into a QUESTION about something that already
/// happened or is already known — a read the composer answers, never a write.
const REMIND_INTERROGATIVES: &[&str] = &[
    "what", "whats", "what's", "how", "when", "where", "who", "whom", "whose", "which", "why",
    "whether", "if", "was", "were", "did", "does", "is", "are", "do",
];

/// Fillers between "remind me" and the interrogative ("remind me again what …").
const REMIND_FILLERS: &[&str] = &["again", "please", "quickly", "quick", "once", "briefly"];

/// Verbs that cancel rather than create.
const CANCEL_VERBS: &[&str] = &[
    "stop reminding",
    "don't remind",
    "dont remind",
    "no longer need",
    "get rid of",
    "call off",
    "cancel",
    "delete",
    "remove",
    "scrap",
    "forget",
    "undo",
    "unset",
    "drop",
    "clear",
];

/// True when `low` (already lowercased) reads `remind me <interrogative> …` —
/// the memory/read ask that must never be filed as a reminder.
pub fn is_reminder_read(low: &str) -> bool {
    for opener in ["remind me ", "remind us ", "reminder "] {
        let Some(idx) = low.find(opener) else { continue };
        let rest = &low[idx + opener.len()..];
        let mut words = rest.split_whitespace().skip_while(|w| {
            REMIND_FILLERS.contains(&w.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        });
        let Some(first) = words.next() else { continue };
        let first = first.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'');
        if REMIND_INTERROGATIVES.contains(&first) {
            return true;
        }
    }
    false
}

/// True when the text explicitly points at an ELAPSED day ("last Monday",
/// "yesterday", "last night").
fn names_past_day(low: &str) -> bool {
    if contains_word(low, "yesterday") || low.contains("last night") || low.contains("last week") {
        return true;
    }
    for lead in ["last ", "this past ", "past "] {
        let mut start = 0;
        while let Some(pos) = low[start..].find(lead) {
            let i = start + pos + lead.len();
            let next = low[i..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if WEEKDAYS
                .iter()
                .any(|(names, _)| names.contains(&next))
            {
                return true;
            }
            start = i;
        }
    }
    false
}

/// A request to CANCEL a pending reminder, parsed from a chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    /// Fuzzy title fragment ("dentist"); empty when only a day was named.
    pub target: String,
    /// The weekday named, if any.
    pub day: Option<Weekday>,
}

/// Detect "cancel the reminder about the dentist" / "stop reminding me about
/// the bins" / "delete my Friday reminder".
///
/// Returns `None` for anything that is not a reminder cancellation, and also for
/// a cancellation too vague to act on ("cancel my reminders") — the caller then
/// asks rather than guessing which one to drop.
pub fn parse_reminder_cancel(text: &str) -> Option<CancelRequest> {
    let low = text.to_ascii_lowercase();
    if !low.contains("remind") {
        return None;
    }
    let (idx, verb) = CANCEL_VERBS
        .iter()
        .filter_map(|v| low.find(v).map(|i| (i, *v)))
        .min_by_key(|(i, _)| *i)?;
    let tail = &low[idx + verb.len()..];
    let day = named_weekday(tail);
    let target = scrub_cancel_noise(tail);
    if target.is_empty() && day.is_none() {
        return None;
    }
    Some(CancelRequest { target, day })
}

/// Strip reminder nouns, day words and connectors off a cancel tail, leaving the
/// title fragment: "the reminder about the dentist" → "dentist".
fn scrub_cancel_noise(frag: &str) -> String {
    const NOISE: &[&str] = &[
        "reminder",
        "reminders",
        "remind",
        "reminding",
        "me",
        "us",
        "my",
        "our",
        "the",
        "that",
        "a",
        "an",
        "about",
        "for",
        "to",
        "on",
        "of",
        "please",
        "any",
        "more",
        "anymore",
        "set",
        "have",
        "we",
        "i",
        "today",
        "tonight",
        "tomorrow",
        "morning",
        "afternoon",
        "evening",
        "night",
    ];
    frag.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\''))
        .filter(|w| {
            !w.is_empty()
                && !NOISE.contains(w)
                && !WEEKDAYS.iter().any(|(names, _)| names.contains(w))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// How the day part of a time expression was resolved. The caller needs this to
/// decide whether a due instant that has already passed may be rolled forward:
/// a BARE weekday rolls a whole week, an EXPLICIT date never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DayKind {
    /// No day word at all — the date defaulted to today.
    None,
    /// "today" / "tonight" / "tomorrow".
    Relative,
    /// A bare weekday name ("Monday") — today counts when today is that weekday.
    BareWeekday,
    /// "next Monday" — the STRICTLY following Monday, never today.
    NextWeekday,
    /// An explicit civil date ("Monday, August 3, 2026", "8/3/2026", "2026-08-03").
    CivilDate,
}

/// Resolve the day part of a time expression. Returns the date, a friendly
/// label (`Some("Thursday")`, `Some("tomorrow")`) when a day word was found
/// (`None` label means "no day word — default to today"), and how it resolved.
///
/// Precedence, most specific first:
///
/// 1. an **explicit civil date** — when the family writes "Monday, August 3,
///    2026" the DATE is what they mean; the weekday word beside it is prose, and
///    resolving it as a weekday is how "August 3" once landed on July 27;
/// 2. `today` / `tonight` / `tomorrow`;
/// 3. `next <weekday>` — the strictly FOLLOWING occurrence, never today;
/// 4. a bare `<weekday>` — the next occurrence on or after today.
fn resolve_day(low: &str, today: NaiveDate) -> (NaiveDate, Option<String>, DayKind) {
    if let Some(found) = find_civil_date(low, today) {
        return (
            found.date,
            Some(long_weekday_name(found.date.weekday()).to_string()),
            DayKind::CivilDate,
        );
    }
    if contains_word(low, "tomorrow") {
        return (
            today + Duration::days(1),
            Some("tomorrow".to_string()),
            DayKind::Relative,
        );
    }
    if contains_word(low, "today") || contains_word(low, "tonight") {
        return (today, Some("today".to_string()), DayKind::Relative);
    }
    if let Some(wd) = names_next_weekday(low) {
        return (
            next_weekday_strict(today, wd),
            Some(long_weekday_name(wd).to_string()),
            DayKind::NextWeekday,
        );
    }
    for (names, wd) in WEEKDAYS {
        if names.iter().any(|n| contains_word(low, n)) {
            let date = next_weekday(today, *wd);
            return (
                date,
                Some(long_weekday_name(*wd).to_string()),
                DayKind::BareWeekday,
            );
        }
    }
    (today, None, DayKind::None)
}

/// Weekday match table: (aliases, chrono weekday).
const WEEKDAYS: &[(&[&str], Weekday)] = &[
    (&["monday", "mon"], Weekday::Mon),
    (&["tuesday", "tue", "tues"], Weekday::Tue),
    (&["wednesday", "wed"], Weekday::Wed),
    (&["thursday", "thu", "thurs"], Weekday::Thu),
    (&["friday", "fri"], Weekday::Fri),
    (&["saturday", "sat"], Weekday::Sat),
    (&["sunday", "sun"], Weekday::Sun),
];

/// The next date ON or after `from` whose weekday is `wd` — a bare weekday
/// always points FORWARD. Today counts when it is that weekday; the caller rolls
/// a whole week on when the resolved *instant* has already passed, so "remind me
/// Monday at 8am" said on Monday afternoon lands next Monday, not this morning.
fn next_weekday(from: NaiveDate, wd: Weekday) -> NaiveDate {
    let delta =
        (wd.num_days_from_monday() as i64) - (from.weekday().num_days_from_monday() as i64);
    from + Duration::days(delta.rem_euclid(7))
}

/// The STRICTLY following occurrence of `wd` after `from` — today never counts.
///
/// This is what "next Monday" means and what [`next_weekday`] cannot express: its
/// delta is modulo seven, so on a Monday it returns that same Monday. Said at
/// 03:20 on Monday 27 July, "remind me next Monday at 9" meant 3 August; the
/// modulo rule filed it for 09:00 that same morning, five hours later.
pub(crate) fn next_weekday_strict(from: NaiveDate, wd: Weekday) -> NaiveDate {
    let delta =
        (wd.num_days_from_monday() as i64) - (from.weekday().num_days_from_monday() as i64);
    let ahead = delta.rem_euclid(7);
    from + Duration::days(if ahead == 0 { 7 } else { ahead })
}

/// The weekday of an explicit `next <weekday>` phrase, if the text carries one.
///
/// Only a weekday DIRECTLY introduced by "next" counts, so "next week's Monday
/// plan" (no adjacency) and a bare "Monday" both fall through to the today-or-
/// later rule, which is correct for them.
pub(crate) fn names_next_weekday(low: &str) -> Option<Weekday> {
    let words: Vec<&str> = low
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .collect();
    for (i, w) in words.iter().enumerate() {
        if !w.eq_ignore_ascii_case("next") {
            continue;
        }
        let Some(after) = words.get(i + 1) else {
            continue;
        };
        let after = after.to_ascii_lowercase();
        if let Some((_, wd)) = WEEKDAYS.iter().find(|(names, _)| names.contains(&after.as_str())) {
            return Some(*wd);
        }
    }
    None
}

/// An explicit civil date found in a request, with the byte span it occupied so
/// callers can strip the phrase out of the reminder body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoundDate {
    /// The resolved calendar date.
    pub date: NaiveDate,
    /// Byte range of the matched phrase within the searched string.
    pub start: usize,
    pub end: usize,
}

/// Month names and their common abbreviations.
const MONTHS: &[(&[&str], u32)] = &[
    (&["january", "jan"], 1),
    (&["february", "feb"], 2),
    (&["march", "mar"], 3),
    (&["april", "apr"], 4),
    (&["may"], 5),
    (&["june", "jun"], 6),
    (&["july", "jul"], 7),
    (&["august", "aug"], 8),
    (&["september", "sept", "sep"], 9),
    (&["october", "oct"], 10),
    (&["november", "nov"], 11),
    (&["december", "dec"], 12),
];

/// The date an explicit civil date in `low` names, if any. See [`find_civil_date`].
pub(crate) fn parse_civil_date(low: &str, today: NaiveDate) -> Option<NaiveDate> {
    find_civil_date(low, today).map(|f| f.date)
}

/// Find an explicit civil date in `low` (already lowercased).
///
/// Four shapes, all anchored so ordinary prose cannot be mistaken for a date:
///
/// * `august 3`, `august 3, 2026`, `aug 3rd` — a month NAME with an adjacent
///   day number. The adjacency requirement is what keeps the modal "may" and the
///   verb "march" from reading as months.
/// * `3 august 2026` — the day-first ordering.
/// * `2026-08-03` — ISO, unambiguous, accepted anywhere.
/// * `8/3/2026` — US month/day/year. The two-part `8/3` is accepted ONLY when
///   introduced by "on"/"the", so "add 1/2 cup" stays a quantity, not 2 January.
///
/// With no year, the year is the one that puts the date on or after `today` —
/// "remind me on January 4", said in December, means the January four weeks out,
/// not the one ten months gone.
pub(crate) fn find_civil_date(low: &str, today: NaiveDate) -> Option<FoundDate> {
    // Word spans, so a match can be reported back as a byte range.
    let spans: Vec<(usize, usize, &str)> = low
        .split_whitespace()
        .map(|w| {
            let start = w.as_ptr() as usize - low.as_ptr() as usize;
            (start, start + w.len(), w)
        })
        .collect();

    for (i, (start, end, raw)) in spans.iter().enumerate() {
        let word = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());

        // ISO 2026-08-03.
        if let Some(date) = parse_iso_date(word) {
            return Some(FoundDate {
                date,
                start: *start,
                end: *end,
            });
        }

        // Numeric 8/3/2026, or 8/3 when introduced by "on"/"the".
        if word.contains('/') {
            let introduced = i > 0
                && matches!(
                    spans[i - 1]
                        .2
                        .trim_matches(|c: char| !c.is_ascii_alphanumeric()),
                    "on" | "the"
                );
            if let Some(date) = parse_slash_date(word, today, introduced) {
                return Some(FoundDate {
                    date,
                    start: *start,
                    end: *end,
                });
            }
        }

        // Month name + adjacent day number, in either order, with an optional year.
        let Some(month) = month_number(word) else {
            continue;
        };
        // "august 3[, 2026]"
        if let Some(day) = spans.get(i + 1).and_then(|(_, _, w)| day_number(w)) {
            let (year, last) = match spans.get(i + 2).and_then(|(_, e, w)| year_number(w).map(|y| (y, *e))) {
                Some((y, e)) => (y, e),
                None => (infer_year(month, day, today), spans[i + 1].1),
            };
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                return Some(FoundDate {
                    date,
                    start: *start,
                    end: last,
                });
            }
        }
        // "3 august [2026]"
        if i > 0 {
            if let Some(day) = day_number(spans[i - 1].2) {
                let (year, last) = match spans.get(i + 1).and_then(|(_, e, w)| year_number(w).map(|y| (y, *e))) {
                    Some((y, e)) => (y, e),
                    None => (infer_year(month, day, today), *end),
                };
                if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                    return Some(FoundDate {
                        date,
                        start: spans[i - 1].0,
                        end: last,
                    });
                }
            }
        }
    }
    None
}

/// The month a word names, if it is a month name or a known abbreviation.
fn month_number(word: &str) -> Option<u32> {
    MONTHS
        .iter()
        .find(|(names, _)| names.contains(&word))
        .map(|(_, m)| *m)
}

/// A day-of-month number, tolerating an ordinal suffix and trailing punctuation
/// ("3", "3,", "3rd", "21st").
fn day_number(word: &str) -> Option<u32> {
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let digits: String = w.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &w[digits.len()..];
    if !matches!(rest, "" | "st" | "nd" | "rd" | "th") {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (1..=31).contains(&n).then_some(n)
}

/// A 4-digit calendar year, tolerating trailing punctuation.
fn year_number(word: &str) -> Option<i32> {
    let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if w.len() != 4 || !w.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i32 = w.parse().ok()?;
    (1900..=2200).contains(&y).then_some(y)
}

/// The year that puts `month`/`day` on or after `today` — this year when it has
/// not yet passed, else next year.
fn infer_year(month: u32, day: u32, today: NaiveDate) -> i32 {
    match NaiveDate::from_ymd_opt(today.year(), month, day) {
        Some(d) if d >= today => today.year(),
        _ => today.year() + 1,
    }
}

/// `2026-08-03`.
fn parse_iso_date(word: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = word.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 {
        return None;
    }
    NaiveDate::from_ymd_opt(
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    )
}

/// `8/3/2026`, or `8/3` when `introduced` (directly after "on"/"the"). US
/// month/day ordering — the families this ships to write dates that way.
fn parse_slash_date(word: &str, today: NaiveDate, introduced: bool) -> Option<NaiveDate> {
    let parts: Vec<&str> = word.split('/').collect();
    if !parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let month: u32 = parts.first()?.parse().ok()?;
    let day: u32 = parts.get(1)?.parse().ok()?;
    match parts.len() {
        2 if introduced => NaiveDate::from_ymd_opt(infer_year(month, day, today), month, day),
        3 => {
            let raw = parts[2];
            let year: i32 = raw.parse().ok()?;
            let year = if raw.len() == 2 { 2000 + year } else { year };
            NaiveDate::from_ymd_opt(year, month, day)
        }
        _ => None,
    }
}

/// The weekday a bare day name in `low` refers to, if any.
fn named_weekday(low: &str) -> Option<Weekday> {
    WEEKDAYS.iter().find_map(|(names, wd)| {
        names
            .iter()
            .any(|n| contains_word(low, n))
            .then_some(*wd)
    })
}

/// "August 3" — the month/day half of a spelled-out date, for confirmations.
fn month_day(date: NaiveDate) -> String {
    const LONG: &[&str] = &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let name = LONG
        .get(date.month0() as usize)
        .copied()
        .unwrap_or_default();
    format!("{} {}", name, date.day())
}

fn long_weekday_name(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// Resolve the time part. Returns (time, friendly label, had_explicit_signal).
/// Falls back to a part-of-day default, else 9am. `had` is true when the text
/// carried an explicit clock or part-of-day word (used to require a time signal
/// when there is no day word).
fn resolve_time(
    low: &str,
    _now: NaiveDateTime,
    _date: NaiveDate,
) -> (NaiveTime, Option<String>, bool) {
    if let Some((t, label)) = parse_explicit_time(low) {
        return (t, Some(label), true);
    }
    // Parts of the day.
    if contains_word(low, "morning") {
        return (nt(9, 0), Some("morning".to_string()), true);
    }
    if contains_word(low, "noon") {
        return (nt(12, 0), Some("noon".to_string()), true);
    }
    if contains_word(low, "afternoon") {
        return (nt(14, 0), Some("afternoon".to_string()), true);
    }
    if contains_word(low, "evening") || contains_word(low, "tonight") {
        return (nt(19, 0), Some("evening".to_string()), true);
    }
    if contains_word(low, "night") {
        return (nt(21, 0), Some("night".to_string()), true);
    }
    // No time signal — default to 9am, and report none.
    (nt(9, 0), None, false)
}

/// Parse an explicit clock like `at 7pm`, `7:30`, `19:30`, `at 8`. Returns the
/// time and a friendly label (`"at 7pm"`).
fn parse_explicit_time(low: &str) -> Option<(NaiveTime, String)> {
    // Scan tokens for something clock-shaped.
    let tokens: Vec<&str> = low
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    for (i, tok) in tokens.iter().enumerate() {
        // A token right after "at" is the strongest signal, but we also accept a
        // standalone clock token anywhere.
        let is_after_at = i > 0 && tokens[i - 1] == "at";
        // A meridiem written as its own word — "9:00 a.m.", "7 p.m." — belongs to
        // the clock before it. Without this, "9:00 p.m." parsed as 09:00 and the
        // reminder fired twelve hours early.
        let trailing = tokens.get(i + 1).and_then(|t| meridiem(t));
        if let Some(t) = parse_time_token(&apply_meridiem(tok, trailing)) {
            // Guard bare integers (e.g. "3 eggs"): only accept a lone number when
            // it directly followed "at" or carries its own meridiem.
            let bare_int = !tok.contains(':')
                && !tok.contains("am")
                && !tok.contains("pm")
                && trailing.is_none();
            if bare_int && !is_after_at {
                continue;
            }
            let label = match trailing {
                Some(m) => format!("at {}{}", tok, m),
                None => format!("at {}", tok),
            };
            return Some((t, label));
        }
    }
    None
}

/// `"am"` / `"pm"` when a standalone token is a meridiem in any of the ways a
/// family types it ("am", "a.m.", "A.M.", "pm.").
fn meridiem(tok: &str) -> Option<&'static str> {
    let squashed: String = tok
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    match squashed.as_str() {
        "am" => Some("am"),
        "pm" => Some("pm"),
        _ => None,
    }
}

/// Glue a detached meridiem onto a clock token so one parser handles both
/// `"7pm"` and `"7 p.m."`.
fn apply_meridiem(tok: &str, m: Option<&str>) -> String {
    match m {
        Some(m) if !tok.ends_with("am") && !tok.ends_with("pm") => format!("{tok}{m}"),
        _ => tok.to_string(),
    }
}

/// Parse one clock token: `7`, `7pm`, `7:30`, `19:30`, `7:30am`.
fn parse_time_token(tok: &str) -> Option<NaiveTime> {
    let mut t = tok.trim();
    let mut pm = false;
    let mut am = false;
    if let Some(stripped) = t.strip_suffix("pm") {
        pm = true;
        t = stripped;
    } else if let Some(stripped) = t.strip_suffix("am") {
        am = true;
        t = stripped;
    }
    let t = t.trim_end_matches('.');
    let (h, m) = if let Some((hh, mm)) = t.split_once(':') {
        (hh.parse::<u32>().ok()?, mm.parse::<u32>().ok()?)
    } else {
        (t.parse::<u32>().ok()?, 0)
    };
    if m > 59 {
        return None;
    }
    let mut hour = h;
    if pm && hour < 12 {
        hour += 12;
    }
    if am && hour == 12 {
        hour = 0;
    }
    if hour > 23 {
        return None;
    }
    // A bare 1..=24 with no am/pm and no colon is only a plausible clock, not e.g.
    // a count — callers gate that separately (see `parse_explicit_time`).
    let _ = (am, pm);
    nt_opt(hour, m)
}

/// A friendly part-of-day word for a wall-clock time, for confirmations.
fn part_of_day(t: NaiveTime) -> &'static str {
    match t.hour() {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _ => "night",
    }
}

fn nt(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap_or_default()
}
fn nt_opt(h: u32, m: u32) -> Option<NaiveTime> {
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Extract the reminder body from a request. Prefers the text after `to `
/// (`"remind me Thursday to defrost the trout"` → `"defrost the trout"`), else
/// strips the leading `remind [me]` and known time words.
fn extract_body(text: &str) -> String {
    let low = text.to_ascii_lowercase();
    // Prefer " to <body>" after the first "remind".
    if let Some(rpos) = low.find("remind") {
        if let Some(tpos) = low[rpos..].find(" to ") {
            let start = rpos + tpos + 4;
            let body = text[start..].trim();
            if !body.is_empty() {
                return capitalize(&strip_trailing_time(body));
            }
        }
    }
    // Fallback: drop a leading name + "remind me", keep the rest, strip time words.
    let mut s = text.trim();
    if let Some(pos) = low.find("remind") {
        s = text[pos + "remind".len()..].trim();
    }
    let low2 = s.to_ascii_lowercase();
    if let Some(rest) = low2.strip_prefix("me") {
        let cut = s.len() - rest.len();
        s = s[cut..].trim();
    }
    capitalize(&strip_trailing_time(s))
}

/// Remove trailing/leading day+time words so the body reads cleanly.
fn strip_trailing_time(body: &str) -> String {
    let mut words: Vec<&str> = body.split_whitespace().collect();
    let is_timeword = |w: &str| {
        let w = w
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        matches!(
            w.as_str(),
            "today"
                | "tonight"
                | "tomorrow"
                | "morning"
                | "afternoon"
                | "evening"
                | "night"
                | "noon"
                | "at"
                | "on"
                | "next"
        ) || WEEKDAYS
            .iter()
            .any(|(names, _)| names.contains(&w.as_str()))
            // The pieces of a spelled-out date, so "call the dentist on Monday,
            // August 3, 2026 at 9:00 a.m." files as "Call the dentist".
            || month_number(&w).is_some()
            || year_number(&w).is_some()
            || day_number(&w).is_some()
            || meridiem(&w).is_some()
            || parse_time_token(&w)
                .is_some_and(|_| w.contains(':') || w.ends_with("am") || w.ends_with("pm"))
    };
    // Trim from both ends only (keep interior words intact).
    while words.first().is_some_and(|w| is_timeword(w)) {
        words.remove(0);
    }
    while words.last().is_some_and(|w| is_timeword(w)) {
        words.pop();
    }
    words.join(" ")
}

fn capitalize(s: &str) -> String {
    let s = s.trim();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Build the one-line confirmation, e.g. `"Will do — Thursday morning ✓"`.
fn confirm_line(
    day_label: &Option<String>,
    time_label: &Option<String>,
    due: NaiveDateTime,
    now: NaiveDateTime,
) -> String {
    let day = day_label.clone().unwrap_or_else(|| {
        if due.date() == now.date() {
            "today".to_string()
        } else if due.date() == now.date() + Duration::days(1) {
            "tomorrow".to_string()
        } else {
            long_weekday_name(due.weekday()).to_string()
        }
    });
    // A weekday name alone cannot identify a date more than a week out — "Monday"
    // reads as the one a few days from now, which is exactly the confusion
    // "next Monday" was filed under. Name the date when it is that far ahead.
    let day = if (due.date() - now.date()).num_days() > 6 {
        format!("{}, {}", day, month_day(due.date()))
    } else {
        day
    };
    // With no explicit time word, describe the default slot by part-of-day
    // ("Thursday morning") so the confirmation reads like a human wrote it.
    let time_phrase = time_label
        .clone()
        .unwrap_or_else(|| part_of_day(due.time()).to_string());
    let when = format!("{} {}", day, time_phrase);
    format!("Will do — {} \u{2713}", when.trim())
}

// ---------------------------------------------------------------------------
// Ad-hoc store (persisted list the scheduler merges with plan reminders)
// ---------------------------------------------------------------------------

/// The persisted list of ad-hoc reminders, at `<root>/.casa/reminders-adhoc.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdHocStore {
    #[serde(default)]
    pub reminders: Vec<Reminder>,
}

impl AdHocStore {
    /// Standard on-disk path.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("reminders-adhoc.json")
    }

    /// Load the store, or empty when absent / corrupt.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    /// Persist atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, json.as_bytes())
    }

    /// Add a reminder, de-duplicating on id (a repeated identical request is a
    /// no-op rather than a double reminder).
    pub fn add(&mut self, r: Reminder) -> bool {
        if self.reminders.iter().any(|x| x.id == r.id) {
            return false;
        }
        self.reminders.push(r);
        true
    }

    /// The PENDING reminders a cancel request matches, as of `now`.
    ///
    /// Fuzzy on the title: every significant word of the fragment must appear in
    /// the reminder text. An already-fired/elapsed reminder is never a match —
    /// there is nothing left to cancel — and neither is one on another weekday
    /// when a day was named. Returning the matches (rather than removing them)
    /// lets the caller ASK when more than one qualifies.
    pub fn matching(&self, req: &CancelRequest, now: NaiveDateTime) -> Vec<&Reminder> {
        self.reminders
            .iter()
            .filter(|r| {
                if r.due <= now {
                    return false;
                }
                if let Some(wd) = req.day {
                    if r.due.date().weekday() != wd {
                        return false;
                    }
                }
                let text = r.text.to_ascii_lowercase();
                let mut words = req
                    .target
                    .split_whitespace()
                    .filter(|w| w.len() > 2)
                    .peekable();
                if words.peek().is_none() {
                    return req.day.is_some();
                }
                words.all(|w| text.contains(w))
            })
            .collect()
    }

    /// Cancel the ONE pending reminder a request matches.
    ///
    /// `Ok(Some(r))` removed it; `Ok(None)` matched nothing; `Err(n)` matched `n`
    /// reminders and removed NOTHING — the caller asks which one was meant.
    pub fn cancel(&mut self, req: &CancelRequest, now: NaiveDateTime) -> Result<Option<Reminder>, usize> {
        let ids: Vec<String> = self
            .matching(req, now)
            .into_iter()
            .map(|r| r.id.clone())
            .collect();
        match ids.len() {
            0 => Ok(None),
            1 => {
                let idx = self
                    .reminders
                    .iter()
                    .position(|r| r.id == ids[0])
                    .expect("matched id is present");
                Ok(Some(self.reminders.remove(idx)))
            }
            n => Err(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::family_plan::CalendarEvent;

    fn members() -> Vec<String> {
        vec!["Luca".to_string(), "Nadin".to_string()]
    }

    fn owners() -> OwnerMap {
        OwnerMap::casa_default()
    }

    fn owners_from_toml(body: &str) -> OwnerMap {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("household.toml"), body).unwrap();
        OwnerMap::from_household_toml(dir.path()).expect("valid household fixture")
    }

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn plan_reminder() -> Reminder {
        let ev = CalendarEvent {
            weekday: "Tue".into(),
            date: Some(NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()),
            time: "19:30".into(),
            event: "\u{23f0} Reminder: Luca PT check-in (if unanswered)".into(),
            source: "Otto".into(),
        };
        Reminder::from_calendar_event("2026-W29", &ev, &members(), &owners())
            .expect("reminder-shaped")
    }

    #[test]
    fn parse_plan_reminder_row_extracts_all_fields() {
        let r = plan_reminder();
        assert_eq!(r.due, dt(2026, 7, 14, 19, 30));
        assert_eq!(r.recipient, "Luca");
        assert_eq!(r.bot, "otto");
        assert_eq!(r.text, "Luca PT check-in (if unanswered)");
        assert_eq!(r.source, ReminderSource::Plan);
        assert_eq!(
            r.message(false),
            "\u{23f0} Luca PT check-in (if unanswered)"
        );
    }

    #[test]
    fn non_reminder_calendar_rows_are_ignored() {
        let ev = CalendarEvent {
            weekday: "Mon".into(),
            date: Some(NaiveDate::from_ymd_opt(2026, 7, 13).unwrap()),
            time: "18:30".into(),
            event: "Cook: chickpea & spinach curry".into(),
            source: "Bruno".into(),
        };
        assert!(Reminder::from_calendar_event("2026-W29", &ev, &members(), &owners()).is_none());
    }

    #[test]
    fn plan_reminder_id_is_stable_across_reparse() {
        let a = plan_reminder();
        let b = plan_reminder();
        assert_eq!(a.id, b.id, "same row → same id (exactly-once key)");
    }

    #[test]
    fn fires_on_time_within_grace() {
        let policy = FirePolicy::default();
        let due = dt(2026, 7, 14, 19, 30);
        // Exactly at due, and 1 min after: on time.
        assert_eq!(
            decide(due, due, &policy),
            FireDecision::Fire { late: false }
        );
        assert_eq!(
            decide(due, dt(2026, 7, 14, 19, 31), &policy),
            FireDecision::Fire { late: false }
        );
    }

    #[test]
    fn not_yet_when_due_in_future() {
        let policy = FirePolicy::default();
        let due = dt(2026, 7, 14, 19, 30);
        assert_eq!(
            decide(due, dt(2026, 7, 14, 19, 29), &policy),
            FireDecision::Pending
        );
    }

    #[test]
    fn fires_late_within_two_hours_then_drops() {
        let policy = FirePolicy::default();
        let due = dt(2026, 7, 14, 19, 30);
        // 30 min late → fire (late).
        assert_eq!(
            decide(due, dt(2026, 7, 14, 20, 0), &policy),
            FireDecision::Fire { late: true }
        );
        // Just under 2h late → still fire (late).
        assert_eq!(
            decide(due, dt(2026, 7, 14, 21, 29), &policy),
            FireDecision::Fire { late: true }
        );
        // Over 2h late → drop.
        assert_eq!(
            decide(due, dt(2026, 7, 14, 21, 31), &policy),
            FireDecision::Drop
        );
    }

    #[test]
    fn late_firing_message_carries_honest_tag() {
        let r = plan_reminder();
        assert_eq!(
            r.message(true),
            "\u{23f0} (late) Luca PT check-in (if unanswered)"
        );
    }

    #[test]
    fn tick_fires_exactly_once_even_when_called_twice() {
        let policy = FirePolicy::default();
        let reminders = vec![plan_reminder()];
        let mut log = FiredLog::default();
        let now = dt(2026, 7, 14, 19, 30);

        let first = tick(&reminders, &mut log, now, &policy);
        assert_eq!(first.fired.len(), 1, "fires the first time");
        assert_eq!(log.len(), 1);

        // A later tick (say the scheduler runs again a minute on) must NOT re-fire.
        let second = tick(&reminders, &mut log, dt(2026, 7, 14, 19, 32), &policy);
        assert!(second.fired.is_empty(), "already fired — never twice");
    }

    #[test]
    fn restart_reload_does_not_refire() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = FiredLog::path(root);
        let policy = FirePolicy::default();
        let reminders = vec![plan_reminder()];
        let now = dt(2026, 7, 14, 19, 30);

        // First process: fire, persist BEFORE "sending".
        let mut log = FiredLog::load(&path);
        let res = tick(&reminders, &mut log, now, &policy);
        assert_eq!(res.fired.len(), 1);
        log.save(&path).unwrap();

        // Restart: a brand-new log loaded from disk must remember it fired.
        let mut reloaded = FiredLog::load(&path);
        assert_eq!(reloaded.len(), 1, "state survived the restart");
        let res2 = tick(&reminders, &mut reloaded, dt(2026, 7, 14, 19, 33), &policy);
        assert!(res2.fired.is_empty(), "restart must not re-fire");
    }

    #[test]
    fn missed_while_down_fires_late_on_restart_but_old_ones_drop() {
        let policy = FirePolicy::default();
        // Two reminders both due while the scheduler was down.
        let recent = {
            let mut r = plan_reminder();
            r.id = "recent".into();
            r.due = dt(2026, 7, 14, 19, 30);
            r
        };
        let stale = {
            let mut r = plan_reminder();
            r.id = "stale".into();
            r.due = dt(2026, 7, 14, 12, 0);
            r
        };
        let mut log = FiredLog::default();
        // Scheduler comes back at 20:30: recent is 1h late (fire late), stale is
        // 8.5h late (drop).
        let res = tick(&[recent, stale], &mut log, dt(2026, 7, 14, 20, 30), &policy);
        assert_eq!(res.fired.len(), 1);
        assert!(res.fired[0].late, "missed-recent fires with (late)");
        assert_eq!(res.fired[0].reminder.id, "recent");
        assert_eq!(res.dropped.len(), 1);
        assert_eq!(res.dropped[0].id, "stale");
        // Both are now consumed — a stale drop is also exactly-once (never fires).
        assert_eq!(log.outcome("stale"), Some(Outcome::Dropped));
        assert_eq!(log.outcome("recent"), Some(Outcome::Late));
    }

    #[test]
    fn adhoc_intent_weekday_and_to_body() {
        // Thursday morning default, body after "to".
        let now = dt(2026, 7, 12, 10, 0); // Sunday
        let intent = parse_reminder_intent("Otto remind me Thursday to defrost the trout", now)
            .expect("reminder intent");
        assert_eq!(intent.due.weekday(), Weekday::Thu);
        assert_eq!(intent.due.time(), nt(9, 0), "morning default");
        assert_eq!(intent.text, "Defrost the trout");
        assert_eq!(intent.confirmation, "Will do — Thursday morning \u{2713}");
    }

    #[test]
    fn adhoc_intent_explicit_clock() {
        let now = dt(2026, 7, 12, 10, 0);
        let intent = parse_reminder_intent("remind me tomorrow at 7pm to call the plumber", now)
            .expect("intent");
        assert_eq!(intent.due, dt(2026, 7, 13, 19, 0));
        assert_eq!(intent.text, "Call the plumber");
    }

    #[test]
    fn adhoc_intent_requires_remind_and_time() {
        let now = dt(2026, 7, 12, 10, 0);
        // No "remind": not an intent.
        assert!(parse_reminder_intent("what's for dinner Thursday?", now).is_none());
        // "remind" but no time signal: leave to the normal composer.
        assert!(parse_reminder_intent("remind me to call mum", now).is_none());
    }

    // ---- date-reminder-fail: the DM path fails closed on three shapes -----

    #[test]
    fn remind_me_what_is_a_read_and_files_nothing() {
        // (a) The live-cert phrase. It asks the family memory; it must not file
        // a reminder titled "was in risotto".
        let now = dt(2026, 7, 12, 10, 0);
        assert!(parse_reminder_intent("Remind me what was in Monday's risotto", now).is_none());
        for msg in [
            "remind me how the oven timer works",
            "remind me when the dentist is on Friday",
            "remind me again what Tuesday's dinner was",
            "otto remind me who is picking up the kids monday",
        ] {
            assert!(
                parse_reminder_intent(msg, now).is_none(),
                "{msg:?} is a read — it must never be filed"
            );
        }
        // A genuine request still registers.
        assert!(parse_reminder_intent("remind me Thursday to defrost the trout", now).is_some());
    }

    #[test]
    fn bare_weekday_resolves_forward_never_into_the_past() {
        // (b) On Monday 2026-07-20 at 08:00, "Monday at 6pm" is TODAY at 18:00 —
        // still ahead. The same ask at 20:00 rolls a full week, never to this
        // morning and never to the week's elapsed Monday.
        let morning = dt(2026, 7, 20, 8, 0);
        let ahead = parse_reminder_intent("remind me Monday at 6pm to move the car", morning)
            .expect("intent");
        assert_eq!(ahead.due, dt(2026, 7, 20, 18, 0));

        let evening = dt(2026, 7, 20, 20, 0);
        let rolled = parse_reminder_intent("remind me Monday at 6pm to move the car", evening)
            .expect("intent");
        assert_eq!(rolled.due, dt(2026, 7, 27, 18, 0));
        assert!(rolled.due > evening, "a reminder is never due in the past");

        // Every weekday, from every day of the week, resolves ahead of now.
        for offset in 0..7 {
            let now = dt(2026, 7, 20, 12, 0) + Duration::days(offset);
            for day in ["monday", "wednesday", "friday", "sunday"] {
                let intent =
                    parse_reminder_intent(&format!("remind me {day} to call the vet"), now)
                        .expect("intent");
                assert!(
                    intent.due.date() >= now.date(),
                    "{day} from {now} resolved backwards to {}",
                    intent.due
                );
            }
        }
    }

    // ---- next-weekday-strict: three phrasings, three DIFFERENT dates ---------

    #[test]
    fn next_weekday_is_strictly_after_today_and_a_bare_one_is_not() {
        // The live-cert C052 probe, at its exact pin: Monday 2026-07-27 at 03:20,
        // with 09:00 still five hours ahead. All three phrasings registered for
        // TODAY; only the last one should have.
        let now = dt(2026, 7, 27, 3, 20);
        assert_eq!(now.weekday(), Weekday::Mon);

        let civil = parse_reminder_intent(
            "Remind me to call the dentist on Monday, August 3, 2026 at 9:00 a.m.",
            now,
        )
        .expect("intent");
        assert_eq!(civil.due, dt(2026, 8, 3, 9, 0), "an explicit date is the date");
        assert_eq!(civil.text, "Call the dentist");

        let next = parse_reminder_intent("Remind me to call the dentist next Monday at 9:00 a.m.", now)
            .expect("intent");
        assert_eq!(next.due, dt(2026, 8, 3, 9, 0), "'next Monday' is never today");

        // The distinction: a BARE weekday keeps today-if-the-time-is-ahead.
        let bare = parse_reminder_intent("Remind me to call the dentist Monday at 9:00 a.m.", now)
            .expect("intent");
        assert_eq!(bare.due, dt(2026, 7, 27, 9, 0), "a bare weekday still means today");
        assert!(
            bare.due < next.due,
            "bare and 'next' must not resolve to the same day"
        );
    }

    #[test]
    fn next_weekday_never_lands_on_today_from_any_day_of_the_week() {
        for offset in 0..7 {
            let now = dt(2026, 7, 20, 12, 0) + Duration::days(offset);
            for (word, wd) in [
                ("monday", Weekday::Mon),
                ("wednesday", Weekday::Wed),
                ("friday", Weekday::Fri),
                ("sunday", Weekday::Sun),
            ] {
                let intent =
                    parse_reminder_intent(&format!("remind me next {word} at 9am to call the vet"), now)
                        .expect("intent");
                assert_eq!(intent.due.weekday(), wd);
                assert!(
                    intent.due.date() > now.date(),
                    "next {word} from {now} must be strictly later, got {}",
                    intent.due
                );
                assert!(
                    (intent.due.date() - now.date()).num_days() <= 7,
                    "next {word} from {now} skipped an occurrence: {}",
                    intent.due
                );
            }
        }
    }

    #[test]
    fn explicit_civil_dates_parse_in_the_shapes_families_type() {
        let now = dt(2026, 7, 27, 3, 20);
        for (msg, want) in [
            ("remind me on august 3 at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me on August 3, 2026 at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me aug 3rd at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me on 3 August 2026 at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me on 2026-08-03 at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me on 8/3/2026 at 9am to call", dt(2026, 8, 3, 9, 0)),
            ("remind me on 8/3 at 9am to call", dt(2026, 8, 3, 9, 0)),
            // A month/day already past this year rolls to next year, not backwards.
            ("remind me on january 4 at 9am to call", dt(2027, 1, 4, 9, 0)),
        ] {
            let intent = parse_reminder_intent(msg, now).unwrap_or_else(|| panic!("no intent: {msg}"));
            assert_eq!(intent.due, want, "{msg}");
        }
    }

    #[test]
    fn a_quantity_is_not_a_date_and_a_modal_is_not_a_month() {
        let now = dt(2026, 7, 27, 3, 20);
        // "1/2 cup" is a fraction, not 2 January. It has no other day word, so the
        // ask resolves to today's default slot — never to a spurious date.
        let intent = parse_reminder_intent("remind me at 9am to buy 1/2 cup of cream", now)
            .expect("intent");
        assert_eq!(intent.due, dt(2026, 7, 27, 9, 0));
        // A bare month word with no adjacent day number is prose, not a date.
        let intent =
            parse_reminder_intent("remind me at 9am to ask whether we may go", now).expect("intent");
        assert_eq!(intent.due, dt(2026, 7, 27, 9, 0));
    }

    #[test]
    fn a_detached_meridiem_is_read_as_pm_not_am() {
        // "9:00 p.m." split into two tokens and only "9:00" parsed — the reminder
        // was filed twelve hours early.
        let now = dt(2026, 7, 27, 3, 20);
        let intent = parse_reminder_intent("remind me next monday at 9:00 p.m. to call", now)
            .expect("intent");
        assert_eq!(intent.due, dt(2026, 8, 3, 21, 0));
        let am = parse_reminder_intent("remind me next monday at 9:00 a.m. to call", now)
            .expect("intent");
        assert_eq!(am.due, dt(2026, 8, 3, 9, 0));
    }

    #[test]
    fn an_unhonourable_typed_date_asks_instead_of_guessing() {
        let now = dt(2026, 7, 27, 3, 20);
        // A date already gone cannot be scheduled — and must not silently roll a
        // year forward either, which is what "file it anyway" would amount to.
        assert!(
            parse_reminder_intent("remind me on July 4, 2026 at 9am to call", now).is_none(),
            "an elapsed typed date must not be filed"
        );
        // The weekday word and the date disagree: August 3 2026 is a MONDAY.
        assert!(
            parse_reminder_intent("remind me on Tuesday, August 3, 2026 at 9am to call", now)
                .is_none(),
            "a weekday/date disagreement must be asked about, not resolved"
        );
        // The agreeing form still files.
        assert!(
            parse_reminder_intent("remind me on Monday, August 3, 2026 at 9am to call", now)
                .is_some()
        );
    }

    #[test]
    fn a_bare_weekday_whose_clock_has_passed_rolls_a_week() {
        // The audit's second control: same Monday, but now PAST 09:00.
        let now = dt(2026, 7, 27, 9, 30);
        let intent = parse_reminder_intent("remind me Monday at 9am to call the dentist", now)
            .expect("intent");
        assert_eq!(intent.due, dt(2026, 8, 3, 9, 0));
    }

    #[test]
    fn a_date_more_than_a_week_out_is_named_in_the_confirmation() {
        // "Will do — Monday at 9:00 ✓" for a date eight days away reads as the
        // Monday three days from now. Name it.
        let now = dt(2026, 7, 27, 3, 20);
        let intent = parse_reminder_intent("remind me next monday at 9am to call the dentist", now)
            .expect("intent");
        assert_eq!(
            intent.confirmation,
            "Will do — Monday, August 3 at 9am \u{2713}"
        );
        // Inside the week, the short form is unchanged.
        let near = parse_reminder_intent("Otto remind me Thursday to defrost the trout", dt(2026, 7, 12, 10, 0))
            .expect("intent");
        assert_eq!(near.confirmation, "Will do — Thursday morning \u{2713}");
    }

    #[test]
    fn an_elapsed_day_is_never_scheduled() {
        let now = dt(2026, 7, 22, 10, 0);
        for msg in [
            "remind me last Monday to take the bins out",
            "remind me yesterday to call the plumber",
        ] {
            assert!(
                parse_reminder_intent(msg, now).is_none(),
                "{msg:?} must not schedule anything"
            );
        }
    }

    #[test]
    fn cancel_phrases_never_create_a_second_reminder() {
        // (c) The cancel shapes, none of which may register anything.
        let now = dt(2026, 7, 12, 10, 0);
        for msg in [
            "cancel the reminder about the dentist",
            "delete the reminder to defrost the trout on Thursday",
            "remove my Friday reminder",
            "stop reminding me about the bins tomorrow",
            "forget the reminder about the dentist Thursday morning",
        ] {
            assert!(
                parse_reminder_intent(msg, now).is_none(),
                "{msg:?} created a reminder instead of cancelling one"
            );
            assert!(
                parse_reminder_cancel(msg).is_some(),
                "{msg:?} should parse as a cancellation"
            );
        }
        // Too vague to act on → no request at all, so the caller asks.
        assert!(parse_reminder_cancel("cancel my reminders").is_none());
        // Not about reminders at all.
        assert!(parse_reminder_cancel("cancel Thursday's dinner").is_none());
    }

    #[test]
    fn cancel_drops_the_one_matching_pending_reminder() {
        let now = dt(2026, 7, 12, 10, 0);
        let mut store = AdHocStore::default();
        for (text, due) in [
            ("Book the dentist", dt(2026, 7, 16, 9, 0)),
            ("Defrost the trout", dt(2026, 7, 17, 17, 0)),
            ("Pay the deposit", dt(2026, 7, 10, 9, 0)), // already elapsed
        ] {
            store.add(Reminder {
                id: format!("adhoc:{}", text),
                due,
                recipient: "Luca".into(),
                bot: "otto".into(),
                text: text.into(),
                source: ReminderSource::AdHoc,
            });
        }

        let req = parse_reminder_cancel("cancel the reminder about the dentist").unwrap();
        let gone = store.cancel(&req, now).expect("unambiguous").expect("a hit");
        assert_eq!(gone.text, "Book the dentist");
        assert_eq!(store.reminders.len(), 2);

        // An elapsed reminder has nothing left to cancel.
        let elapsed = parse_reminder_cancel("cancel the reminder about the deposit").unwrap();
        assert_eq!(store.cancel(&elapsed, now), Ok(None));

        // A day-only cancel picks the reminder on that day.
        let friday = parse_reminder_cancel("delete my Friday reminder").unwrap();
        assert_eq!(
            store
                .cancel(&friday, now)
                .expect("unambiguous")
                .expect("a hit")
                .text,
            "Defrost the trout"
        );
    }

    #[test]
    fn an_ambiguous_cancel_removes_nothing() {
        let now = dt(2026, 7, 12, 10, 0);
        let mut store = AdHocStore::default();
        for (id, text) in [("a", "Call the dentist about Ada"), ("b", "Call the dentist back")] {
            store.add(Reminder {
                id: id.into(),
                due: dt(2026, 7, 16, 9, 0),
                recipient: "Luca".into(),
                bot: "otto".into(),
                text: text.into(),
                source: ReminderSource::AdHoc,
            });
        }
        let req = parse_reminder_cancel("cancel the reminder about the dentist").unwrap();
        assert_eq!(store.cancel(&req, now), Err(2));
        assert_eq!(store.reminders.len(), 2, "nothing may be dropped on a guess");
    }

    #[test]
    fn adhoc_intent_becomes_stored_reminder() {
        let now = dt(2026, 7, 12, 10, 0);
        let intent =
            parse_reminder_intent("remind me tomorrow morning to water the plants", now).unwrap();
        let r = intent_to_reminder(&intent, "Luca", "otto");
        assert_eq!(r.recipient, "Luca");
        assert_eq!(r.bot, "otto");
        assert_eq!(r.source, ReminderSource::AdHoc);
        assert!(r.id.starts_with("adhoc:"));
        assert_eq!(r.due, dt(2026, 7, 13, 9, 0));
    }

    #[test]
    fn adhoc_store_dedupes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = AdHocStore::path(dir.path());
        let now = dt(2026, 7, 12, 10, 0);
        let intent =
            parse_reminder_intent("remind me tomorrow morning to water the plants", now).unwrap();
        let r = intent_to_reminder(&intent, "Luca", "otto");

        let mut store = AdHocStore::load(&path);
        assert!(store.add(r.clone()));
        assert!(!store.add(r.clone()), "same id → no duplicate");
        store.save(&path).unwrap();

        let reloaded = AdHocStore::load(&path);
        assert_eq!(reloaded.reminders.len(), 1);
        assert_eq!(reloaded.reminders[0].text, "Water the plants");
    }

    #[test]
    fn multiword_source_resolves_unique_stable_owner() {
        let owners = owners_from_toml(
            r#"
[[agent]]
id = "coordination-anchor-7"
name = "Harbor Keeper"
domains = ["coordination", "calendar"]

[[agent]]
id = "meal-anchor-4"
name = "Pantry Lantern"
domains = ["meals"]
"#,
        );
        let plan = PlanDoc::parse(
            "2026-W31",
            r#"
**Week of Monday 2026-07-27 → Sunday 2026-08-02**

## 1. Dinners (Mon 07-27 → Sun 08-02)
| Day | Slot | Dish | Prep |
|---|---|---|---|
| Mon 07-27 | Vegetarian | Summer pasta | ~20 min |

## 3. Calendar
| Day | Time | Event | Source |
|---|---|---|---|
| Tue 07-28 | 19:30 | ⏰ Reminder: Luca PT check-in | Harbor Keeper (§4) |
"#,
        );
        assert_eq!(
            plan.meals.len(),
            1,
            "the fixture must exercise the production `## 1. Dinners (…)` shape",
        );
        let reminders = reminders_from_plan(&plan, &members(), &owners);
        let reminder = reminders
            .first()
            .expect("live-shaped reminder row resolves");
        assert_eq!(
            reminder.bot, "coordination-anchor-7",
            "the complete authored display name must resolve to its stable id",
        );
    }

    #[test]
    fn ambiguous_or_unknown_nonempty_source_fails_closed() {
        let owners = owners_from_toml(
            r#"
[[agent]]
id = "coordination-anchor-a"
name = "Shared Lantern"
domains = ["coordination"]

[[agent]]
id = "coordination-anchor-b"
name = "Shared Lantern"
domains = ["calendar"]
"#,
        );
        assert_eq!(resolve_plan_source("Shared Lantern", &owners), None);
        assert_eq!(resolve_plan_source("Unknown Lantern", &owners), None);
        assert_eq!(resolve_plan_source("", &owners), Some(String::new()));
    }
}
