//! The conversational feedback loop — asks report back.
//!
//! Luca's north-star interaction is the *back-and-forth*: he asks a persona in
//! chat to tweak the week, the family does the work, and he hears about it —
//! "on it", then "done, here's what changed". Before this module the pipeline
//! did the work **invisibly**: a 1:1 to Otto created a real task, but Luca then
//! heard nothing — no "they started", no "done", and his explicit "let me know
//! when they're done" had no mechanism to be honored. To him it looked like a
//! bug. This module closes that loop.
//!
//! # What lives here (pure + restart-safe)
//!
//! Everything is pure over injected inputs (the task's [`TaskOrigin`], its
//! status, an injected `now`) so the whole loop is unit-testable without a live
//! bot, clock, or graph:
//!
//! * **Origin** — [`TaskOrigin`] (defined in `graph`) is stamped on a task by
//!   the composer's task-creation path ([`extract_task_directive`] pulls the
//!   `TASK_CREATE:` tail out of a composed reply, mirroring the photo pipeline's
//!   `SHOPPING_UPDATE:` contract).
//! * **Lifecycle events** — [`LifecycleEvent`] (`Started` / `Done` / `Failed`),
//!   derived from a task's live status by [`event_for_task`].
//! * **The lines** — [`render_line`] composes the family-voice notification for
//!   each event, in the composing persona's voice ("Nora and Bruno are on it
//!   🍳" / "Done! …, the week's updated" / an honest, never-technical snag line).
//! * **"Are they done yet?"** — [`is_status_question`] + [`answer_status`]
//!   answer a status question from LIVE task state, not a generic chat turn.
//! * **"Let me know when…"** — [`is_follow_request`] + [`FOLLOW_ACK`] acknowledge
//!   an explicit follow request ("Will do — I'll ping you here.").
//! * **Pacing** — a lifecycle notification is offered to the daily-digest choke
//!   point as [`NudgeKind::Lifecycle`] / [`Urgency::TimeCritical`]: it fires
//!   standalone (a direct reply to an ask), but stays **capped** so a burst of
//!   asks can't flood the chat. [`lifecycle_tick`] wires exactly-once firing (a
//!   [`FiredLog`], keyed on `(task, event)`) to that pacing.

use chrono::NaiveDateTime;

use crate::graph::{Status, Task, TaskOrigin};
use crate::notify::daily_digest::{DigestPolicy, DigestStore, Nudge, NudgeKind, Offer};
use crate::notify::reminder::{FiredLog, Outcome};

// ---------------------------------------------------------------------------
// Lifecycle events
// ---------------------------------------------------------------------------

/// A point in a conversational task's life worth reporting back to the human who
/// asked. Exactly the three the north-star interaction wants: *they started*,
/// *they're done*, and *it didn't work out*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// The task was claimed / started — the "on it" heads-up.
    Started,
    /// The task finished — the payoff, with what changed.
    Done,
    /// The task failed / was abandoned — an honest, never-technical one-liner.
    Failed,
}

impl LifecycleEvent {
    /// Stable slug for the [`FiredLog`] / digest de-dupe id and the `--dry-run`
    /// seam.
    pub fn slug(self) -> &'static str {
        match self {
            LifecycleEvent::Started => "started",
            LifecycleEvent::Done => "done",
            LifecycleEvent::Failed => "failed",
        }
    }
}

/// The lifecycle event a task's current [`Status`] warrants, or `None` for a
/// status that owes no notification yet (still `Open`/`Waiting`/etc.).
///
/// `PendingEval`/`PendingValidation` count as still-running (they follow a
/// `wg done` but the human's payoff should wait for the *real* `Done`), so they
/// map to `Started` — the human still deserves the "on it" if they haven't had
/// it, but never a premature "done".
pub fn event_for_status(status: Status) -> Option<LifecycleEvent> {
    match status {
        // Running, or still being resolved after a soft done/fail — the human
        // gets "on it" but never a premature "done"/"failed" while the gate runs.
        Status::InProgress
        | Status::PendingValidation
        | Status::PendingEval
        | Status::FailedPendingEval => Some(LifecycleEvent::Started),
        Status::Done => Some(LifecycleEvent::Done),
        Status::Failed | Status::Abandoned => Some(LifecycleEvent::Failed),
        // Not yet claimed, or an ambiguous/terminal-unclear state — owe nothing.
        Status::Open | Status::Waiting | Status::Blocked | Status::Incomplete => None,
    }
}

/// The lifecycle event a task warrants, given its live status.
pub fn event_for_task(task: &Task) -> Option<LifecycleEvent> {
    event_for_status(task.status)
}

// ---------------------------------------------------------------------------
// The composer's task-creation tail (`TASK_CREATE:`)
// ---------------------------------------------------------------------------

/// The machine directive a composing persona appends, on its own final line,
/// when a conversational turn should create a task — the twin of the photo
/// pipeline's `SHOPPING_UPDATE:` tail. The warm family-voice reply and the
/// machine directive travel together in one composed message; the directive is
/// parsed out and stripped so the human only ever sees the reply.
pub const TASK_CREATE_MARKER: &str = "TASK_CREATE:";

/// A composed reply split into the human-facing text and an optional task the
/// turn asked to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDirective {
    /// The reply with any `TASK_CREATE:` line removed and trailing space trimmed.
    pub reply: String,
    /// The task title the turn asked to create, if the tail was present and
    /// non-empty.
    pub title: Option<String>,
}

/// Split a composed reply into its human-facing text and an optional
/// `TASK_CREATE:` directive. Case-insensitive on the marker; the directive must
/// be on its own line. When no (or an empty) directive is present, `title` is
/// `None` and `reply` is the input trimmed.
pub fn extract_task_directive(reply: &str) -> TaskDirective {
    let mut kept: Vec<&str> = Vec::new();
    let mut title: Option<String> = None;
    for line in reply.lines() {
        let trimmed = line.trim();
        if trimmed
            .to_ascii_lowercase()
            .starts_with(&TASK_CREATE_MARKER.to_ascii_lowercase())
        {
            let rest = trimmed[TASK_CREATE_MARKER.len()..].trim();
            if !rest.is_empty() {
                title = Some(rest.to_string());
            }
            continue; // never echo the directive to the human
        }
        kept.push(line);
    }
    TaskDirective {
        reply: kept.join("\n").trim().to_string(),
        title,
    }
}

// ---------------------------------------------------------------------------
// Rendering the family-voice lines
// ---------------------------------------------------------------------------

/// Title-case a single lowercase roster token for display ("otto" → "Otto").
fn pretty(name: &str) -> String {
    let mut chars = name.trim().chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Join worker names naturally: `["nora"]` → "Nora"; `["nora","bruno"]` →
/// "Nora and Bruno"; `["nora","bruno","mira"]` → "Nora, Bruno and Mira".
fn join_names(names: &[String]) -> String {
    let pretties: Vec<String> = names
        .iter()
        .map(|n| pretty(n))
        .filter(|n| !n.is_empty())
        .collect();
    match pretties.len() {
        0 => String::new(),
        1 => pretties[0].clone(),
        _ => {
            let (last, head) = pretties.split_last().unwrap();
            format!("{} and {}", head.join(", "), last)
        }
    }
}

/// Compose the family-voice notification for `event`, in the origin persona's
/// voice.
///
/// * `Started` — "Nora and Bruno are on it 🍳" when the workers are known, else
///   the composing persona's own "on it".
/// * `Done` — "Done! {what changed}" when a change summary was recorded, else a
///   warm generic completion.
/// * `Failed` — an honest, never-technical one-liner.
///
/// `workers` are the persona names doing the work (from the task's assignee, at
/// notification time); `summary` is the family-voice "what changed" line.
pub fn render_line(
    origin: &TaskOrigin,
    event: LifecycleEvent,
    workers: &[String],
    summary: Option<&str>,
) -> String {
    match event {
        LifecycleEvent::Started => {
            let names = join_names(workers);
            if names.is_empty() {
                let who = pretty(&origin.persona);
                if who.is_empty() {
                    "On it 🍳".to_string()
                } else {
                    format!("{who}'s on it 🍳")
                }
            } else {
                let is_are = if workers.len() > 1 { "are" } else { "is" };
                format!("{names} {is_are} on it 🍳")
            }
        }
        LifecycleEvent::Done => match summary.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => {
                let s = s.trim_end_matches(['.', ' ']);
                format!("Done! {s} ✅")
            }
            None => "All done — that's sorted ✅".to_string(),
        },
        LifecycleEvent::Failed => {
            "Ran into a snag on that one — I'll take another crack at it. Sorry for the wait!"
                .to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// "Are they done yet?" — answering a status question from live state
// ---------------------------------------------------------------------------

/// A curated set of phrases that signal a human is asking about the *progress*
/// of something they asked for, not making small talk. Matched
/// case-insensitively as substrings so "is it done yet?" and "any news on the
/// week?" both trip it, while ordinary chat ("dinner's done") does not (that
/// needs "done yet" / a progress cue).
const STATUS_CUES: &[&str] = &[
    "done yet",
    "they done",
    "it done",
    "are they done",
    "is it done",
    "did they finish",
    "did they do it",
    "finished yet",
    "ready yet",
    "any update",
    "any news",
    "how's it going",
    "hows it going",
    "how is it going",
    "how's that going",
    "hows that going",
    "what's the status",
    "whats the status",
    "any progress",
];

/// Whether `message` reads like a "are they done yet?" status check. The
/// composer uses this — together with the sender having recent origin-stamped
/// tasks — to answer from live graph state instead of a generic chat turn.
pub fn is_status_question(message: &str) -> bool {
    let low = message.to_ascii_lowercase();
    STATUS_CUES.iter().any(|cue| low.contains(cue))
}

/// A lightweight, test-friendly snapshot of one origin-stamped task's state —
/// enough to answer a status question without constructing a full [`Task`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    /// Family-facing description of the ask (e.g. "tweak this week's meals").
    pub what: String,
    /// The lifecycle stage, or `None` while still queued (Open/Waiting).
    pub event: Option<LifecycleEvent>,
    /// The "what changed" summary, when the task is done and recorded one.
    pub summary: Option<String>,
}

impl TaskState {
    /// Derive a status snapshot from a live task: its human-facing ask, its
    /// lifecycle stage, and any recorded change summary.
    pub fn from_task(task: &Task) -> Self {
        Self {
            what: task_what(task),
            event: event_for_task(task),
            summary: summary_for_task(task),
        }
    }
}

/// Answer a "are they done yet?" question from the requester's live task states.
/// Returns `None` when there is nothing to report (no origin tasks), so the
/// caller falls back to an ordinary chat turn.
///
/// The answer is family-voice and honest about the mix: still-queued, in
/// progress, and done are each reported plainly, newest concerns first.
pub fn answer_status(states: &[TaskState]) -> Option<String> {
    if states.is_empty() {
        return None;
    }
    let mut done: Vec<&TaskState> = Vec::new();
    let mut running: Vec<&TaskState> = Vec::new();
    let mut queued: Vec<&TaskState> = Vec::new();
    let mut failed: Vec<&TaskState> = Vec::new();
    for st in states {
        match st.event {
            Some(LifecycleEvent::Done) => done.push(st),
            Some(LifecycleEvent::Started) => running.push(st),
            Some(LifecycleEvent::Failed) => failed.push(st),
            None => queued.push(st),
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if !running.is_empty() {
        parts.push(format!(
            "They're on it now ({}) — I'll ping you the moment it's done.",
            list_whats(&running)
        ));
    }
    if !queued.is_empty() {
        parts.push(format!(
            "{} queued up — they'll get to it shortly.",
            cap_first(&list_whats(&queued))
        ));
    }
    if !done.is_empty() {
        let payoff = done
            .iter()
            .find_map(|st| st.summary.as_deref())
            .map(|s| format!(": {}", s.trim_end_matches(['.', ' '])))
            .unwrap_or_default();
        parts.push(format!("All done{payoff} ✅"));
    }
    if !failed.is_empty() {
        parts.push(
            "One of them hit a snag — I'm getting it sorted and will let you know.".to_string(),
        );
    }
    Some(parts.join(" "))
}

fn list_whats(states: &[&TaskState]) -> String {
    let whats: Vec<String> = states.iter().map(|s| s.what.clone()).collect();
    whats.join(", ")
}

fn cap_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// "Let me know when…" — acknowledging an explicit follow request
// ---------------------------------------------------------------------------

/// Phrases that register an explicit "keep me posted" intent. The payoff is
/// already guaranteed by the Done notification (2b); the composer only needs to
/// *acknowledge* the request so the human knows it was heard.
const FOLLOW_CUES: &[&str] = &[
    "let me know when",
    "let me know once",
    "let me know how it goes",
    "tell me when",
    "ping me when",
    "keep me posted",
    "keep me updated",
    "text me when",
    "message me when",
];

/// Whether `message` explicitly asks to be told when the work is done.
pub fn is_follow_request(message: &str) -> bool {
    let low = message.to_ascii_lowercase();
    FOLLOW_CUES.iter().any(|cue| low.contains(cue))
}

/// The acknowledgement for a "let me know when…" request — the composer appends
/// it so the human's ask is honored out loud.
pub const FOLLOW_ACK: &str = "Will do — I'll ping you here when it's done.";

// ---------------------------------------------------------------------------
// Summaries + human-facing "what" from a task
// ---------------------------------------------------------------------------

/// The log-message prefix a worker uses to record the family-voice "what
/// changed" line for the Done payoff, e.g.
/// `wg log <task> "LIFECYCLE_SUMMARY: Carbonara Wednesday, eggs Tuesday…"`.
/// Kept out of band from the technical breadcrumbs so the payoff stays warm.
pub const SUMMARY_LOG_PREFIX: &str = "LIFECYCLE_SUMMARY:";

/// Extract the family-voice change summary a worker recorded via a
/// `LIFECYCLE_SUMMARY:` log line, if any (the most recent one wins).
pub fn summary_for_task(task: &Task) -> Option<String> {
    task.log
        .iter()
        .rev()
        .find_map(|entry| {
            let msg = entry.message.trim();
            msg.strip_prefix(SUMMARY_LOG_PREFIX)
                .map(|rest| rest.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

/// A short, human-facing description of what the task is *about*, for status
/// answers. Prefers the task title, humanized (dashes → spaces, id-ish suffixes
/// left as-is is fine for a family read).
pub fn task_what(task: &Task) -> String {
    let t = task.title.trim();
    if t.is_empty() {
        return "your request".to_string();
    }
    t.replace('-', " ")
}

// ---------------------------------------------------------------------------
// Pacing: offer a lifecycle notification to the daily-digest choke point
// ---------------------------------------------------------------------------

/// The exactly-once / pacing id for a `(task, event)` notification. The same
/// pair always derives the same id, so both the [`FiredLog`] and the digest
/// `seen` set fire it at most once.
pub fn notification_id(task_id: &str, event: LifecycleEvent) -> String {
    format!("lifecycle:{task_id}:{}", event.slug())
}

/// Build the pacing [`Nudge`] for a rendered lifecycle notification: time-critical
/// (a direct reply to an ask) but capped, addressed to the requester.
pub fn to_nudge(
    task_id: &str,
    event: LifecycleEvent,
    origin: &TaskOrigin,
    text: &str,
    now: NaiveDateTime,
) -> Nudge {
    Nudge::time_critical(
        notification_id(task_id, event),
        origin.requester.clone(),
        NudgeKind::Lifecycle,
        now,
        text,
    )
}

// ---------------------------------------------------------------------------
// The tick: exactly-once firing wired to pacing
// ---------------------------------------------------------------------------

/// One task's live lifecycle input to the [`lifecycle_tick`]: its id, where it
/// came from, the event its status warrants, who is doing the work, and any
/// recorded change summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleInput {
    pub task_id: String,
    pub origin: TaskOrigin,
    pub event: LifecycleEvent,
    pub workers: Vec<String>,
    pub summary: Option<String>,
}

impl LifecycleInput {
    /// Build the input for a task, or `None` when it is not origin-stamped or
    /// owes no notification yet. `workers` is the persona name(s) doing the work
    /// (resolved by the caller from the task's assignee, falling back to the
    /// origin persona).
    pub fn from_task(task: &Task, workers: Vec<String>) -> Option<Self> {
        let origin = task.origin.clone()?;
        let event = event_for_task(task)?;
        Some(Self {
            task_id: task.id.clone(),
            origin,
            event,
            workers,
            summary: summary_for_task(task),
        })
    }

    /// The rendered family-voice line for this input.
    pub fn render(&self) -> String {
        render_line(&self.origin, self.event, &self.workers, self.summary.as_deref())
    }
}

/// One notification the tick decided to deliver: where it goes, as whom, and the
/// text. Produced for both dry-run (printed) and real (sent) firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleFire {
    pub task_id: String,
    pub event: LifecycleEvent,
    pub origin: TaskOrigin,
    pub text: String,
}

/// The outcome of a lifecycle tick over a set of origin-stamped tasks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleTickResult {
    /// Notifications to deliver standalone now (under the cap).
    pub fired: Vec<LifecycleFire>,
    /// `(task, event)` notifications skipped because the pacing cap was spent —
    /// folded into the next digest rather than piling on standalone pings.
    pub capped: Vec<LifecycleFire>,
}

/// Scan `inputs`, firing each not-yet-sent `(task, event)` notification exactly
/// once, paced through the daily-digest choke point.
///
/// Restart-safe: an id is recorded in `log` **before** it would be sent (the
/// caller persists `log`/`store` after this returns, then delivers `fired`), so
/// a crash between record and send at worst drops a single notification rather
/// than duplicating it — mirroring the reminder/errand engines.
///
/// * Already in `log` → skipped (exactly-once).
/// * Under the per-person cap → [`LifecycleTickResult::fired`], recorded `Sent`.
/// * Over the cap → [`LifecycleTickResult::capped`], recorded `Sent` too (it was
///   handled — folded into the digest by the pacing layer — so it never re-fires).
pub fn lifecycle_tick(
    inputs: &[LifecycleInput],
    log: &mut FiredLog,
    store: &mut DigestStore,
    now: NaiveDateTime,
    policy: &DigestPolicy,
) -> LifecycleTickResult {
    let mut result = LifecycleTickResult::default();
    for input in inputs {
        if input.origin.requester.trim().is_empty() {
            // No one to report back to — nothing to pace or fire.
            continue;
        }
        let id = notification_id(&input.task_id, input.event);
        if log.contains(&id) {
            continue;
        }
        let text = input.render();
        let nudge = to_nudge(&input.task_id, input.event, &input.origin, &text, now);
        let fire = LifecycleFire {
            task_id: input.task_id.clone(),
            event: input.event,
            origin: input.origin.clone(),
            text: text.clone(),
        };
        match store.offer(&nudge, now, policy) {
            Offer::SendNow(_) => {
                log.record(&id, now, Outcome::OnTime);
                result.fired.push(fire);
            }
            Offer::Queued { .. } => {
                // Cap spent — the pacing layer folded it into the digest. Record
                // it as handled so we never re-offer the same (task, event).
                log.record(&id, now, Outcome::OnTime);
                result.capped.push(fire);
            }
            // Not yet due can't happen (due == now); a duplicate means another
            // path already handled it — record so our FiredLog agrees.
            Offer::Pending | Offer::Duplicate => {
                log.record(&id, now, Outcome::OnTime);
            }
        }
    }
    result
}

/// Format a single lifecycle notification for the `wg telegram lifecycle
/// --dry-run <task>` seam: what would be sent, where, and as whom.
pub fn dry_run_line(fire: &LifecycleFire) -> String {
    let bot = fire.origin.bot_id.as_deref().unwrap_or(&fire.origin.persona);
    format!(
        "[dry-run] {} → chat {} via bot '{}' as '{}': {}",
        fire.event.slug(),
        fire.origin.chat_id,
        bot,
        fire.origin.persona,
        fire.text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{LogEntry, OriginChannel, Status, Task, TaskOrigin};

    fn origin() -> TaskOrigin {
        TaskOrigin::new(
            OriginChannel::TelegramDirect,
            "12345",
            "Luca",
            "otto",
            Some("otto".to_string()),
        )
    }

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-07-13T12:58:00", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn task_with(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: "tweak w29 meals".to_string(),
            status,
            origin: Some(origin()),
            ..Default::default()
        }
    }

    // -- origin round-trips through serde (the field survives save/load) -------

    #[test]
    fn lifecycle_origin_round_trips_through_json() {
        let t = task_with("tweak-w29-meals", Status::Open);
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"telegram-1:1\""), "channel label: {json}");
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.origin, Some(origin()));
    }

    #[test]
    fn lifecycle_origin_absent_serializes_nothing() {
        let mut t = task_with("x", Status::Open);
        t.origin = None;
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains("origin"), "no origin key when unstamped: {json}");
    }

    // -- event derivation from status -----------------------------------------

    #[test]
    fn lifecycle_event_derives_from_status() {
        assert_eq!(event_for_status(Status::Open), None);
        assert_eq!(event_for_status(Status::Waiting), None);
        assert_eq!(
            event_for_status(Status::InProgress),
            Some(LifecycleEvent::Started)
        );
        assert_eq!(
            event_for_status(Status::PendingEval),
            Some(LifecycleEvent::Started),
            "soft-done must not fire a premature 'done'"
        );
        assert_eq!(event_for_status(Status::Done), Some(LifecycleEvent::Done));
        assert_eq!(event_for_status(Status::Failed), Some(LifecycleEvent::Failed));
        assert_eq!(
            event_for_status(Status::Abandoned),
            Some(LifecycleEvent::Failed)
        );
    }

    // -- TASK_CREATE tail parsing ---------------------------------------------

    #[test]
    fn lifecycle_extracts_task_create_tail_and_strips_it() {
        let reply = "On it — I'll get the week tweaked.\nTASK_CREATE: tweak this week's meals — carbonara Wed, eggs Tue";
        let d = extract_task_directive(reply);
        assert_eq!(d.reply, "On it — I'll get the week tweaked.");
        assert_eq!(
            d.title.as_deref(),
            Some("tweak this week's meals — carbonara Wed, eggs Tue")
        );
        assert!(!d.reply.contains("TASK_CREATE"));
    }

    #[test]
    fn lifecycle_task_create_is_case_insensitive_and_optional() {
        let d = extract_task_directive("just chatting, no task here");
        assert_eq!(d.title, None);
        assert_eq!(d.reply, "just chatting, no task here");

        let d2 = extract_task_directive("Sure!\ntask_create:   do the thing  ");
        assert_eq!(d2.title.as_deref(), Some("do the thing"));
        assert_eq!(d2.reply, "Sure!");

        // Empty tail → no task, marker still stripped.
        let d3 = extract_task_directive("hi\nTASK_CREATE:");
        assert_eq!(d3.title, None);
        assert_eq!(d3.reply, "hi");
    }

    // -- family-voice rendering -----------------------------------------------

    #[test]
    fn lifecycle_started_names_the_workers() {
        let o = origin();
        assert_eq!(
            render_line(&o, LifecycleEvent::Started, &["nora".into(), "bruno".into()], None),
            "Nora and Bruno are on it 🍳"
        );
        assert_eq!(
            render_line(&o, LifecycleEvent::Started, &["nora".into()], None),
            "Nora is on it 🍳"
        );
        // No known workers → the composing persona's own "on it".
        assert_eq!(
            render_line(&o, LifecycleEvent::Started, &[], None),
            "Otto's on it 🍳"
        );
    }

    #[test]
    fn lifecycle_done_carries_what_changed() {
        let o = origin();
        let summary = "Carbonara Wednesday, eggs Tuesday, and fish for Saturday lunch — the week's updated.";
        assert_eq!(
            render_line(&o, LifecycleEvent::Done, &[], Some(summary)),
            "Done! Carbonara Wednesday, eggs Tuesday, and fish for Saturday lunch — the week's updated ✅"
        );
        assert_eq!(
            render_line(&o, LifecycleEvent::Done, &[], None),
            "All done — that's sorted ✅"
        );
    }

    #[test]
    fn lifecycle_failed_is_honest_never_technical() {
        let line = render_line(&origin(), LifecycleEvent::Failed, &[], None);
        assert!(line.to_lowercase().contains("snag"));
        for jargon in ["panic", "error", "exit", "stderr", "None", "unwrap", "task"] {
            assert!(!line.contains(jargon), "no jargon '{jargon}' in: {line}");
        }
    }

    // -- status questions ------------------------------------------------------

    #[test]
    fn lifecycle_detects_status_questions() {
        for q in [
            "are they done yet?",
            "is it done?",
            "any update on the week?",
            "how's it going with dinner plan",
            "did they finish that",
        ] {
            assert!(is_status_question(q), "should detect: {q}");
        }
        for not in ["dinner's done, come eat", "thanks!", "make carbonara wednesday"] {
            assert!(!is_status_question(not), "should NOT detect: {not}");
        }
    }

    #[test]
    fn lifecycle_answers_status_from_live_state() {
        // Nothing stamped → no answer, fall back to a normal chat turn.
        assert_eq!(answer_status(&[]), None);

        let running = TaskState {
            what: "tweak this week's meals".into(),
            event: Some(LifecycleEvent::Started),
            summary: None,
        };
        let a = answer_status(&[running]).unwrap();
        assert!(a.contains("on it now"), "{a}");
        assert!(a.contains("tweak this week's meals"), "{a}");

        let done = TaskState {
            what: "tweak this week's meals".into(),
            event: Some(LifecycleEvent::Done),
            summary: Some("carbonara Wednesday, eggs Tuesday".into()),
        };
        let a2 = answer_status(&[done]).unwrap();
        assert!(a2.starts_with("All done"), "{a2}");
        assert!(a2.contains("carbonara Wednesday"), "{a2}");
    }

    // -- follow requests -------------------------------------------------------

    #[test]
    fn lifecycle_detects_follow_requests() {
        assert!(is_follow_request("let me know when they are done"));
        assert!(is_follow_request("ping me when it's ready"));
        assert!(is_follow_request("keep me posted"));
        assert!(!is_follow_request("make the change please"));
    }

    // -- summary extraction from a task ---------------------------------------

    #[test]
    fn lifecycle_summary_reads_the_worker_log_line() {
        let log_line = |msg: &str| LogEntry {
            timestamp: "2026-07-13T12:00:00".into(),
            actor: None,
            user: None,
            message: msg.into(),
        };
        let mut t = task_with("x", Status::Done);
        t.log.push(log_line("Starting implementation"));
        t.log.push(log_line(
            "LIFECYCLE_SUMMARY: carbonara Wednesday, eggs Tuesday, fish Saturday lunch",
        ));
        assert_eq!(
            summary_for_task(&t).as_deref(),
            Some("carbonara Wednesday, eggs Tuesday, fish Saturday lunch")
        );
        let mut plain = task_with("y", Status::Done);
        plain.log.push(log_line("did stuff"));
        assert_eq!(summary_for_task(&plain), None);
    }

    // -- the tick: exactly-once + pacing cap ----------------------------------

    fn input(task_id: &str, event: LifecycleEvent) -> LifecycleInput {
        LifecycleInput {
            task_id: task_id.to_string(),
            origin: origin(),
            event,
            workers: vec!["nora".into(), "bruno".into()],
            summary: None,
        }
    }

    #[test]
    fn lifecycle_tick_fires_each_event_exactly_once() {
        let mut log = FiredLog::default();
        let mut store = DigestStore::default();
        let policy = DigestPolicy::default();

        let inputs = vec![input("t1", LifecycleEvent::Started)];
        let r1 = lifecycle_tick(&inputs, &mut log, &mut store, now(), &policy);
        assert_eq!(r1.fired.len(), 1);
        assert_eq!(r1.fired[0].text, "Nora and Bruno are on it 🍳");

        // Same event again → already fired, nothing new.
        let r2 = lifecycle_tick(&inputs, &mut log, &mut store, now(), &policy);
        assert!(r2.fired.is_empty(), "must not re-fire a sent notification");

        // The Done event for the SAME task is a distinct id → fires once.
        let done = vec![input("t1", LifecycleEvent::Done)];
        let r3 = lifecycle_tick(&done, &mut log, &mut store, now(), &policy);
        assert_eq!(r3.fired.len(), 1);
    }

    #[test]
    fn lifecycle_tick_respects_the_pacing_cap() {
        let mut log = FiredLog::default();
        let mut store = DigestStore::default();
        let policy = DigestPolicy::default(); // standalone_cap = 3

        // Four distinct notifications to the same person at once: three fire
        // standalone, the fourth is capped (folded into the digest), never lost.
        let inputs = vec![
            input("a", LifecycleEvent::Started),
            input("b", LifecycleEvent::Started),
            input("c", LifecycleEvent::Started),
            input("d", LifecycleEvent::Started),
        ];
        let r = lifecycle_tick(&inputs, &mut log, &mut store, now(), &policy);
        assert_eq!(r.fired.len(), 3, "cap of 3 standalone");
        assert_eq!(r.capped.len(), 1, "the 4th folds into the digest");
        // All four are recorded so a re-tick fires none of them again.
        let r2 = lifecycle_tick(&inputs, &mut log, &mut store, now(), &policy);
        assert!(r2.fired.is_empty() && r2.capped.is_empty());
    }

    #[test]
    fn lifecycle_tick_skips_unaddressed_origins() {
        let mut log = FiredLog::default();
        let mut store = DigestStore::default();
        let policy = DigestPolicy::default();
        let mut inp = input("t", LifecycleEvent::Started);
        inp.origin.requester = String::new(); // nobody to reach
        let r = lifecycle_tick(&[inp], &mut log, &mut store, now(), &policy);
        assert!(r.fired.is_empty() && r.capped.is_empty());
    }

    #[test]
    fn lifecycle_input_from_task_needs_origin_and_an_event() {
        let stamped_running = task_with("t", Status::InProgress);
        assert!(LifecycleInput::from_task(&stamped_running, vec![]).is_some());

        let mut unstamped = task_with("t", Status::InProgress);
        unstamped.origin = None;
        assert!(LifecycleInput::from_task(&unstamped, vec![]).is_none());

        let stamped_open = task_with("t", Status::Open); // no event yet
        assert!(LifecycleInput::from_task(&stamped_open, vec![]).is_none());
    }

    #[test]
    fn lifecycle_dry_run_line_shows_where_and_as_whom() {
        let fire = LifecycleFire {
            task_id: "tweak-w29-meals".into(),
            event: LifecycleEvent::Started,
            origin: origin(),
            text: "Nora and Bruno are on it 🍳".into(),
        };
        let line = dry_run_line(&fire);
        assert!(line.contains("chat 12345"));
        assert!(line.contains("bot 'otto'"));
        assert!(line.contains("Nora and Bruno are on it"));
    }
}
