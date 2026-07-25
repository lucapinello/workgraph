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
    ) -> Option<Reminder> {
        if !is_reminder_event(&ev.event) {
            return None;
        }
        let due_date = ev.date?;
        let time = parse_clock(&ev.time)?;
        let due = due_date.and_time(time);
        let body = clean_reminder_body(&ev.event);
        let recipient = first_member(&ev.event, members).unwrap_or_default();
        let bot = normalize_bot(&ev.source);
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
pub fn reminders_from_plan(plan: &PlanDoc, members: &[String]) -> Vec<Reminder> {
    plan.calendar
        .iter()
        .filter_map(|ev| Reminder::from_calendar_event(&plan.week_code, ev, members))
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

/// Lower-case a Source cell into a bot/agent id: `"Otto"` → `"otto"`, and
/// `"Mira/Otto"` → the first voice (`"mira"`) which owns the row.
///
/// `pub(crate)` so the errand engine ([`crate::notify::errand`]) resolves the
/// owning voice of a `🛒 Market run` row with the identical rule.
pub(crate) fn normalize_bot(source: &str) -> String {
    source
        .split(['/', '(', ' '])
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase()
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
                log.record(&r.id, now, if late { Outcome::Late } else { Outcome::OnTime });
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

    let (date, day_label) = resolve_day(&low, now.date());
    let (time, time_label, had_time) = resolve_time(&low, now, date);

    // Require *some* time signal — a day word or an explicit clock — so bare
    // "remind me to call mum" (no when) is left to the normal composer.
    if day_label.is_none() && !had_time {
        return None;
    }

    let due = date.and_time(time);
    // If everything resolved to a moment already in the past today, roll to
    // tomorrow so "remind me at 7" late in the evening still means the next 7.
    let due = if due <= now && day_label.is_none() {
        (date + Duration::days(1)).and_time(time)
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
pub fn intent_to_reminder(
    intent: &AdHocIntent,
    recipient: &str,
    bot: &str,
) -> Reminder {
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

/// Resolve the day part of a time expression. Returns the date and a friendly
/// label (`Some("Thursday")`, `Some("tomorrow")`) when a day word was found;
/// `None` label means "no day word — default to today".
fn resolve_day(low: &str, today: NaiveDate) -> (NaiveDate, Option<String>) {
    if contains_word(low, "tomorrow") {
        return (today + Duration::days(1), Some("tomorrow".to_string()));
    }
    if contains_word(low, "today") || contains_word(low, "tonight") {
        return (today, Some("today".to_string()));
    }
    for (names, wd) in WEEKDAYS {
        if names.iter().any(|n| contains_word(low, n)) {
            let date = next_weekday(today, *wd);
            return (date, Some(long_weekday_name(*wd).to_string()));
        }
    }
    (today, None)
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

/// The next date on or after `from` whose weekday is `wd`, always strictly in the
/// future when `from` itself is that weekday (so "remind me Monday" on a Monday
/// means next Monday, not this morning).
fn next_weekday(from: NaiveDate, wd: Weekday) -> NaiveDate {
    let mut d = from + Duration::days(1);
    for _ in 0..7 {
        if d.weekday() == wd {
            return d;
        }
        d += Duration::days(1);
    }
    from
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
fn resolve_time(low: &str, _now: NaiveDateTime, _date: NaiveDate) -> (NaiveTime, Option<String>, bool) {
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
        if let Some(t) = parse_time_token(tok) {
            // Guard bare integers (e.g. "3 eggs"): only accept a lone number when
            // it directly followed "at".
            let bare_int = !tok.contains(':') && !tok.contains("am") && !tok.contains("pm");
            if bare_int && !is_after_at {
                continue;
            }
            let label = format!("at {}", tok);
            return Some((t, label));
        }
    }
    None
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
        let w = w.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
        matches!(
            w.as_str(),
            "today" | "tonight" | "tomorrow" | "morning" | "afternoon" | "evening"
                | "night" | "noon" | "at" | "on"
        ) || WEEKDAYS.iter().any(|(names, _)| names.contains(&w.as_str()))
            || parse_time_token(&w).is_some_and(|_| w.contains(':') || w.ends_with("am") || w.ends_with("pm"))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::family_plan::CalendarEvent;

    fn members() -> Vec<String> {
        vec![
            "Luca".to_string(),
            "Nadin".to_string(),
        ]
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
        Reminder::from_calendar_event("2026-W29", &ev, &members()).expect("reminder-shaped")
    }

    #[test]
    fn parse_plan_reminder_row_extracts_all_fields() {
        let r = plan_reminder();
        assert_eq!(r.due, dt(2026, 7, 14, 19, 30));
        assert_eq!(r.recipient, "Luca");
        assert_eq!(r.bot, "otto");
        assert_eq!(r.text, "Luca PT check-in (if unanswered)");
        assert_eq!(r.source, ReminderSource::Plan);
        assert_eq!(r.message(false), "\u{23f0} Luca PT check-in (if unanswered)");
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
        assert!(Reminder::from_calendar_event("2026-W29", &ev, &members()).is_none());
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
        assert_eq!(decide(due, due, &policy), FireDecision::Fire { late: false });
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
        let intent = parse_reminder_intent(
            "Otto remind me Thursday to defrost the trout",
            now,
        )
        .expect("reminder intent");
        assert_eq!(intent.due.weekday(), Weekday::Thu);
        assert_eq!(intent.due.time(), nt(9, 0), "morning default");
        assert_eq!(intent.text, "Defrost the trout");
        assert_eq!(intent.confirmation, "Will do — Thursday morning \u{2713}");
    }

    #[test]
    fn adhoc_intent_explicit_clock() {
        let now = dt(2026, 7, 12, 10, 0);
        let intent =
            parse_reminder_intent("remind me tomorrow at 7pm to call the plumber", now)
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

    #[test]
    fn adhoc_intent_becomes_stored_reminder() {
        let now = dt(2026, 7, 12, 10, 0);
        let intent =
            parse_reminder_intent("remind me tomorrow morning to water the plants", now)
                .unwrap();
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
            parse_reminder_intent("remind me tomorrow morning to water the plants", now)
                .unwrap();
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
    fn source_with_slash_picks_first_voice() {
        assert_eq!(normalize_bot("Mira/Otto"), "mira");
        assert_eq!(normalize_bot("Otto (§4)"), "otto");
    }
}
