//! Grounded, non-repetitive, corrigible conversation.
//!
//! Luca's 2026-07-15 transcript exposed four composer diseases in the fork
//! conversation layer. Otto was asked "Plans for tomorrow?" and stalled four
//! times with a near-identical "meals are set, waiting on confirmations from you
//! and Nadin, want the rundown?" — never once *reading the plan* — until he was
//! literally commanded "You need to read the calendar", at which point his fifth
//! reply (with data) was excellent. That quality has to be turn one.
//!
//! This module is the pure, testable core of the fix. Nothing here spawns a
//! model, touches the network, or blocks; the thin filesystem wrapper
//! ([`fetch`]) is the only impure function and it is best-effort. The four rules:
//!
//! 1. **Grounding** — [`is_read_shaped`] recognises a question/read-shaped ask
//!    about plans/calendar/meals/schedule (the read-side twin of the fast lane's
//!    edit-shaped classifier). On a hit the caller injects [`plan_digest`] — the
//!    real week model — into the compose context so the answer is turn-one
//!    grounded instead of a stall.
//! 2. **Repetition guard** — [`similarity`] (character-trigram Jaccard) plus
//!    [`is_repetitive`] catch a drafted reply that is substantially the same as
//!    the persona's previous reply; the caller then answers honestly
//!    ([`repetition_fallback_line`]) rather than emit the same summary a third
//!    time.
//! 3. **Corrections stick** — [`detect_correction`] spots "Nadin is not logged,
//!    ignore this" and friends; the caller persists it and [`corrections_block`]
//!    replays every recorded correction into the prompt so it is honoured for the
//!    rest of the window and every future turn.
//! 4. **Style** — [`strip_trailing_formulaic`] and [`is_formulaic_question`] kill
//!    the reflexive "Anything specific…?" tail that ended every single turn;
//!    the caller allows at most one per conversation.
//!
//! Luca's 2026-07-15 follow-up ("just answer — no questions back, no week-dumps,
//! and don't tell me about breakfast at 3pm") sharpens the read path into four
//! more rules, all pure and testable here:
//!
//! - **Answer first, directly** — [`grounded_block`] leads the injected context
//!   with an answer-first, brevity-budgeted instruction header.
//! - **No unsolicited questions (hard)** — [`enforce_answer_shape`] strips ANY
//!   trailing question from a plain read-reply; [`is_deliberation_request`] is
//!   the one escape hatch ("let's think about the day" earns a question back).
//! - **Scope to the question** — [`detect_scope`] resolves today / tomorrow /
//!   a named weekday / the week, and [`grounded_block`] filters the plan to
//!   exactly that, so "today" is never a week-dump.
//! - **Clock-aware** — [`event_has_passed`] (via the household `chrono::Local`
//!   seam) drops today's already-passed events; when the day is spent the block
//!   says so in one line and offers tomorrow's first item.

use std::path::Path;

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

use super::family_plan::{self, PlanDoc};

// ---------------------------------------------------------------------------
// Rule 1 — grounding: classify read-shaped asks, fetch the week model
// ---------------------------------------------------------------------------

/// Topic words that mean the family is asking *about the plan/schedule* — the
/// same subject matter the week model answers. Kept lowercase; matched as
/// substrings against the normalised message.
const PLAN_TOPICS: &[&str] = &[
    "plan",
    "calendar",
    "schedule",
    "agenda",
    "meal",
    "dinner",
    "lunch",
    "breakfast",
    "cook",
    "food",
    "eat",
    "shopping",
    "grocer",
    "workout",
    "exercise",
    "training",
    "appointment",
    "week",
    "weekend",
    "tomorrow",
    "today",
    "tonight",
];

/// Standalone phrases that are *always* a request to read the plan out, even
/// without an explicit topic word or question mark: "give me the rundown",
/// "walk me through it", "what's the recap".
const READ_TRIGGERS: &[&str] = &[
    "rundown",
    "run down",
    "walk me through",
    "walk through",
    "recap",
    "overview",
    "summary",
    "summarize",
    "summarise",
    "catch me up",
    "what's on",
    "whats on",
    "what do we have",
    "what have we got",
    "what's happening",
    "whats happening",
    "read the calendar",
    "read the plan",
];

/// Read-verb openers: a message that *starts* with one of these is asking to be
/// told something (as opposed to asking the family to DO something).
const READ_VERBS: &[&str] = &[
    "what", "when", "where", "which", "who", "how", "show", "tell", "give",
    "remind", "list", "do we", "are there", "is there", "any", "anything",
    "got any", "whats", "what's",
];

/// True when `message` is a question/read-shaped ask about the plan, calendar,
/// meals, or schedule — the read-side analogue of the fast lane's edit-shaped
/// classifier. A hit tells the composer to fetch and inject the week model so
/// the reply is grounded on turn one.
///
/// The rule: an explicit [`READ_TRIGGERS`] phrase always qualifies; otherwise
/// the message must both name a [`PLAN_TOPICS`] subject AND be shaped like a
/// question (ends with `?`, or opens with a [`READ_VERBS`] read verb). This
/// deliberately does *not* fire on pure action asks ("swap Friday to tacos")
/// which carry no question shape and are the fast lane's job — but grounding is
/// additive context, so an occasional false positive only helps.
pub fn is_read_shaped(message: &str) -> bool {
    let norm = normalize(message);
    if norm.is_empty() {
        return false;
    }
    if READ_TRIGGERS.iter().any(|t| norm.contains(t)) {
        return true;
    }
    let has_topic = PLAN_TOPICS.iter().any(|t| norm.contains(t));
    if !has_topic {
        return false;
    }
    let is_question = message.trim_end().ends_with('?')
        || READ_VERBS.iter().any(|v| {
            norm == *v || norm.starts_with(&format!("{v} ")) || norm.starts_with(&format!("{v}'"))
        });
    is_question
}

/// Load the current week model under `root` and render it as a compact
/// grounding block, or `None` when there is no plan to read. Best-effort
/// filesystem read — the only impure function in this module.
pub fn fetch(root: &Path, today: NaiveDate) -> Option<String> {
    let plans = family_plan::load_plans(root);
    let doc = family_plan::current_plan(&plans, today)?;
    Some(plan_digest(doc, today))
}

/// Render a [`PlanDoc`] as a compact, model-facing grounding block: the real
/// meals, appointments, shopping, and workouts for the week, prefixed with an
/// explicit instruction to answer FROM the data rather than stall. Pure.
pub fn plan_digest(doc: &PlanDoc, today: NaiveDate) -> String {
    let mut out = String::new();
    out.push_str(
        "GROUNDING — this is the family's ACTUAL current plan. Answer the question directly \
         and specifically from it. Do NOT stall, do NOT say you are \"waiting on confirmations\" \
         or offer a \"rundown\" instead of giving one — just tell them what is on the plan.\n",
    );

    let span = match (doc.start, doc.end) {
        (Some(s), Some(e)) => format!(
            "{} ({} – {})",
            doc.week_code,
            s.format("%a %b %-d"),
            e.format("%a %b %-d")
        ),
        _ => doc.week_code.clone(),
    };
    out.push_str(&format!("Week: {span}\n"));
    out.push_str(&format!(
        "Today is {} {}.\n",
        family_plan::long_weekday(today),
        today.format("%b %-d")
    ));

    if !doc.meals.is_empty() {
        out.push_str("Meals (dinner per day):\n");
        for m in &doc.meals {
            let dish = if m.dish.trim().is_empty() {
                "(not set)"
            } else {
                m.dish.trim()
            };
            out.push_str(&format!("- {}: {}\n", m.weekday, dish));
        }
    }

    if !doc.calendar.is_empty() {
        out.push_str("Appointments & calendar:\n");
        for e in &doc.calendar {
            let when = if e.time.trim().is_empty() {
                e.weekday.clone()
            } else {
                format!("{} {}", e.weekday, e.time.trim())
            };
            out.push_str(&format!("- {}: {}\n", when, e.event.trim()));
        }
    }

    if !doc.shopping.is_empty() {
        let count: usize = doc.shopping.iter().map(|s| s.items.len()).sum();
        out.push_str(&format!(
            "Shopping list: {count} items across {} sections.\n",
            doc.shopping.len()
        ));
    }

    if !doc.workouts.is_empty() {
        out.push_str("Workouts:\n");
        for w in &doc.workouts {
            out.push_str(&format!("- {} {}: {}\n", w.person, w.weekday, w.session));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Rule 2 — repetition guard
// ---------------------------------------------------------------------------

/// Similarity threshold above which two replies count as "the same answer".
/// Character-trigram Jaccard of ~0.6 already means heavily overlapping phrasing;
/// 0.55 catches the transcript's near-identical stalls while leaving genuinely
/// distinct answers (a stall vs. the real grounded rundown) well clear.
pub const REPETITION_THRESHOLD: f64 = 0.55;

/// Character-trigram Jaccard similarity of two strings, in `[0.0, 1.0]`.
/// Normalised (lowercased, punctuation dropped, whitespace collapsed) so that
/// re-punctuated or lightly reworded restatements still register as similar.
/// Two empty (or sub-trigram) strings are `1.0` identical / `0.0` otherwise.
pub fn similarity(a: &str, b: &str) -> f64 {
    let sa = trigrams(&normalize(a));
    let sb = trigrams(&normalize(b));
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

/// True when `candidate` is substantially the same as `previous` — the third
/// identical summary the guard exists to stop.
pub fn is_repetitive(candidate: &str, previous: &str) -> bool {
    similarity(candidate, previous) >= REPETITION_THRESHOLD
}

/// The honest line to send instead of repeating a summary: own that the answer
/// already went out and offer to actually go read the source. Never itself a
/// carbon copy of the summary it replaces.
pub fn repetition_fallback_line() -> String {
    "I already told you what I've got on that — I'm not going to keep repeating it. \
     Want me to actually open the week and read it line by line?"
        .to_string()
}

// ---------------------------------------------------------------------------
// Rule 3 — corrections stick
// ---------------------------------------------------------------------------

/// Phrases that mark the human correcting a fact mid-conversation. Matched as
/// substrings against the normalised message.
// NOTE: markers are matched against `normalize`d text, which strips apostrophes
// ("isn't" -> "isnt"), so every entry here is written apostrophe-free.
const CORRECTION_MARKERS: &[&str] = &[
    "ignore this",
    "ignore that",
    "is not logged",
    "isnt logged",
    "not logged",
    "thats wrong",
    "thats not right",
    "not correct",
    "incorrect",
    "thats not true",
    "not true",
    "forget what i said",
    "forget that",
    "scratch that",
    "disregard",
    "no longer",
    "stop saying",
    "quit saying",
    "youre wrong",
];

/// If `message` corrects a fact ("Nadin is not logged, ignore this"), return the
/// normalised correction text to persist and replay; otherwise `None`. The
/// returned string is the human's own words (whitespace-collapsed) so the
/// downstream prompt block quotes them verbatim.
pub fn detect_correction(message: &str) -> Option<String> {
    let norm = normalize(message);
    if CORRECTION_MARKERS.iter().any(|m| norm.contains(m)) {
        let cleaned = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    } else {
        None
    }
}

/// Prefix under which corrections are stored in the durable preference store, so
/// they can be told apart from ordinary standing preferences on read-back.
pub const CORRECTION_PREFIX: &str = "Correction: ";

/// Build the prompt block that replays recorded corrections so the persona
/// honours them on every subsequent turn. `corrections` is the list of stored
/// correction texts (already stripped of [`CORRECTION_PREFIX`]). `None` when
/// there is nothing to replay.
pub fn corrections_block(corrections: &[String]) -> Option<String> {
    let items: Vec<&String> = corrections.iter().filter(|c| !c.trim().is_empty()).collect();
    if items.is_empty() {
        return None;
    }
    let mut out = String::from(
        "CORRECTIONS the family has made — these are authoritative. Honour every one and \
         NEVER repeat a claim they have corrected:\n",
    );
    for c in items {
        out.push_str(&format!("- {}\n", c.trim()));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Rule 4 — style: at most one formulaic trailing question per conversation
// ---------------------------------------------------------------------------

/// Openers of the reflexive filler question that ended every turn in the
/// transcript. A trailing sentence beginning with one of these (and ending in a
/// `?`) is treated as formulaic.
const FORMULAIC_OPENERS: &[&str] = &[
    "anything specific",
    "anything else",
    "anything in particular",
    "anything i can",
    "anything you",
    "anything on",
    "want the rundown",
    "want a rundown",
    "want me to",
    "would you like",
    "let me know if",
    "is there anything",
    "shall i",
    "should i",
    "can i help",
    "how can i help",
    "need anything",
    "sound good",
    "sound ok",
];

/// True when `sentence` (a single trailing clause) is a formulaic filler
/// question — the "Anything specific…?" reflex.
pub fn is_formulaic_question(sentence: &str) -> bool {
    let s = sentence.trim();
    if !s.ends_with('?') {
        return false;
    }
    let norm = normalize(s);
    FORMULAIC_OPENERS.iter().any(|o| norm.starts_with(o) || norm.contains(o))
}

/// Split a reply's final sentence off and, if it is a formulaic filler
/// question, return `(body_without_it, Some(the_question))`. Otherwise return
/// `(reply, None)` unchanged. Pure; the caller decides whether to drop it based
/// on whether the conversation has already spent its one allowed filler.
pub fn strip_trailing_formulaic(reply: &str) -> (String, Option<String>) {
    let trimmed = reply.trim_end();
    // Find the start of the final sentence: after the last sentence terminator
    // that is not the very last character.
    let last_q = match trimmed.rfind('?') {
        Some(i) if i + 1 >= trimmed.trim_end().len() => i,
        _ => return (reply.to_string(), None),
    };
    let before = &trimmed[..last_q];
    let split_at = before
        .rfind(|c| c == '.' || c == '!' || c == '?' || c == '\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let tail = trimmed[split_at..].trim();
    if !is_formulaic_question(tail) {
        return (reply.to_string(), None);
    }
    let body = trimmed[..split_at].trim_end().to_string();
    (body, Some(tail.to_string()))
}

/// Enforce the "at most one filler question per conversation" rule. If the reply
/// ends in a formulaic question AND one has already been used earlier in the
/// conversation, drop it (returning the plain-ended body); otherwise leave the
/// reply intact. A body that would be left empty is returned unchanged (never
/// send nothing).
pub fn enforce_style(reply: &str, already_used: bool) -> String {
    if !already_used {
        return reply.to_string();
    }
    let (body, stripped) = strip_trailing_formulaic(reply);
    match stripped {
        Some(_) if !body.trim().is_empty() => body,
        _ => reply.to_string(),
    }
}

/// Count how many of `replies` end in a formulaic filler question — used to
/// decide whether the conversation has already spent its one allowance.
pub fn count_formulaic(replies: &[String]) -> usize {
    replies
        .iter()
        .filter(|r| strip_trailing_formulaic(r).1.is_some())
        .count()
}

// ---------------------------------------------------------------------------
// Rule 2 (hard) — answer first: no unsolicited trailing questions
// ---------------------------------------------------------------------------

/// Phrases that mark the human *inviting* the agent to deliberate, weigh
/// options, or plan together — the one case where a question back is fair play
/// ("let's think about the day", "help me decide what to cook"). Matched as
/// substrings against the normalised message (apostrophe-free, see `normalize`).
const DELIBERATION_MARKERS: &[&str] = &[
    "lets think",
    "let us think",
    "think about",
    "lets plan",
    "help me plan",
    "help me decide",
    "help me choose",
    "help me figure",
    "figure out",
    "lets figure",
    "lets discuss",
    "lets brainstorm",
    "brainstorm",
    "talk through",
    "what should we",
    "what should i",
    "what do you think",
    "your thoughts",
    "any ideas",
    "cant decide",
    "not sure what",
    "weigh in",
];

/// True when the human's own message asks the agent to deliberate/plan rather
/// than simply read the plan out. Only then may a composed reply end with a
/// question (rule 2, case (a)). A plain read-ask ("what's for dinner today?")
/// is NOT deliberation and must be answered, not bounced back.
pub fn is_deliberation_request(message: &str) -> bool {
    let norm = normalize(message);
    DELIBERATION_MARKERS.iter().any(|m| norm.contains(m))
}

/// Split a reply's final sentence off and, if it is *any* question (ends with
/// `?`), return `(body_without_it, Some(the_question))`. The generalisation of
/// [`strip_trailing_formulaic`] that backs the hard no-question rule: a plain
/// read-ask reply must end on a statement, not just avoid the *formulaic*
/// filler. Pure. `(reply, None)` when the reply does not end in a question.
pub fn strip_trailing_question(reply: &str) -> (String, Option<String>) {
    let trimmed = reply.trim_end();
    let last_q = match trimmed.rfind('?') {
        Some(i) if i + 1 >= trimmed.trim_end().len() => i,
        _ => return (reply.to_string(), None),
    };
    let before = &trimmed[..last_q];
    let split_at = before
        .rfind(|c| c == '.' || c == '!' || c == '?' || c == '\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let body = trimmed[..split_at].trim_end().to_string();
    let tail = trimmed[split_at..].trim().to_string();
    (body, Some(tail))
}

/// Enforce rule 2 as a hard rule for read-shaped, non-deliberation asks: the
/// reply must end on a statement. When `allow_question` is false and the reply
/// ends in a question, strip that trailing question — UNLESS doing so would
/// leave nothing to send (a reply that is *only* a question is kept, so we
/// never send an empty message; grounding upstream makes this vanishingly
/// rare). When `allow_question` is true (the human asked us to deliberate), the
/// reply is returned untouched.
pub fn enforce_answer_shape(reply: &str, allow_question: bool) -> String {
    if allow_question {
        return reply.to_string();
    }
    let (body, stripped) = strip_trailing_question(reply);
    match stripped {
        Some(_) if !body.trim().is_empty() => body,
        _ => reply.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Rule 6 — no dangling-promise deferral tails on a DELIVERED answer
// ---------------------------------------------------------------------------

/// Deferral phrases that turn a DELIVERED answer into a fresh dangling promise.
///
/// The 17:5x repro (task owner-pin-engine): Nora acked "on it", the compose
/// delivered the calorie answer — and then tacked on "…let me get Nora's exact
/// take". On a delivered turn there is no one left to defer to: the persona IS
/// the voice and already answered, so a trailing clause opening with one of
/// these is a new promise that will never be kept. Matched as substrings against
/// the NORMALISED trailing clause (apostrophe-free — see [`normalize`] — so
/// "I'll" -> "ill", "Nora's" -> "noras"). Kept SPECIFIC (multi-word, never a bare
/// "let me get") so a legitimate action-ack ("on it, I'll change the week") is
/// never mistaken for a deferral.
const DEFERRAL_MARKERS: &[&str] = &[
    "get back to you",
    "getting back to you",
    "get back to u",
    "circle back",
    "ill follow up",
    "follow up with you",
    "ill loop in",
    "let me check with",
    "let me confirm with",
    "let me double check with",
    "let me verify with",
    "let me ask nora",
    "let me get the exact",
    "let me get you the exact",
    "let me get an exact",
    "let me get their exact",
    "let me get her exact",
    "let me get his exact",
    "let me pull the exact",
    "let me pull her exact",
    "let me pull their exact",
    "let me find out",
    "let me confirm the exact",
    "exact take",
    "get their exact",
    "get her exact",
    "get his exact",
    "ill get you the exact",
    "ill get the exact",
    "ill get their exact",
    "ill get her exact",
    "ill get his exact",
    "ill get back to you",
    "ill find out",
    "ill check with",
    "ill confirm with",
    "ill get you exact numbers",
    "get you the exact numbers",
    "get the exact numbers",
];

/// True when `clause` (a single trailing sentence/clause) is a dangling-promise
/// deferral — a fresh "let me get X's exact take / I'll get back to you" tacked
/// onto an answer that was already delivered. Pure.
pub fn is_deferral_tail(clause: &str) -> bool {
    let norm = normalize(clause);
    if norm.is_empty() {
        return false;
    }
    DEFERRAL_MARKERS.iter().any(|m| norm.contains(m))
}

/// Byte offset where the reply's final clause begins: just after the last
/// clause-boundary character. Unlike [`strip_trailing_question`]'s splitter this
/// also treats the ellipsis (`…` and the ASCII run in `...`), the em-dash (`—`),
/// and the semicolon as boundaries, because a deferral is usually *appended*
/// with one of those rather than a full stop ("…let me get Nora's exact take.").
/// A boundary that is the very last non-space char is skipped (a terminal `.`
/// does not start an empty final clause). Returns 0 when there is no boundary.
fn final_clause_start(trimmed: &str) -> usize {
    let end = trimmed.trim_end().len();
    let mut start = 0usize;
    for (i, c) in trimmed.char_indices() {
        if i >= end {
            break;
        }
        if matches!(c, '.' | '!' | '?' | '\n' | '…' | ';' | '—') {
            let next = i + c.len_utf8();
            if next < end {
                start = next;
            }
        }
    }
    start
}

/// Split a reply's final clause off and, if it is a dangling-promise deferral,
/// return `(body_without_it, Some(the_deferral))`. Otherwise `(reply, None)`
/// unchanged. Pure; the caller (a delivered-answer guard) decides whether to
/// drop it, and never empties the reply.
pub fn strip_deferral_tail(reply: &str) -> (String, Option<String>) {
    let trimmed = reply.trim_end();
    if trimmed.is_empty() {
        return (reply.to_string(), None);
    }
    let start = final_clause_start(trimmed);
    let tail = trimmed[start..].trim();
    if tail.is_empty() || !is_deferral_tail(tail) {
        return (reply.to_string(), None);
    }
    // Trim the boundary char that introduced the tail if it was a *soft*
    // connector ("…", "—", ";") — leaving it dangling ("It's 450 calories …")
    // reads as a truncation. A hard sentence end ("." "!" "?") is kept so the
    // body still terminates cleanly.
    let body = trimmed[..start]
        .trim_end()
        .trim_end_matches(['…', '—', ';', '\n'])
        .trim_end()
        .to_string();
    (body, Some(tail.to_string()))
}

/// Enforce the "no dangling-promise tail on a DELIVERED answer" rule (rule 6,
/// sibling of the anti-fabrication rewrite). If the reply ends in a deferral
/// clause, drop it (returning the plain-ended body); a body that would be left
/// empty — a reply that is *only* a deferral — is returned unchanged, so we
/// never send nothing (the compose prompt's first-person/no-promise instruction
/// keeps that degenerate case vanishingly rare).
pub fn enforce_no_deferral(reply: &str) -> String {
    let (body, stripped) = strip_deferral_tail(reply);
    match stripped {
        Some(_) if !body.trim().is_empty() => body,
        _ => reply.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Rule 3 — scope: answer exactly the day/window the human asked about
// ---------------------------------------------------------------------------

/// The time window a read-ask is about. "Today" means today, not the rest of
/// the week; week-scope only when the ask says so. Resolved against the day the
/// question was asked so the caller can filter the plan to exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskScope {
    /// A single concrete day (today, tomorrow, or a named weekday).
    Day(NaiveDate),
    /// The whole plan week — only when the ask says "week"/"weekend".
    Week,
}

/// Match a whole token against a weekday name/abbreviation. Token equality (not
/// substring) so "monday"/"mon" hit but "money" does not.
fn weekday_token(tok: &str) -> Option<Weekday> {
    match tok {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" | "tues" => Some(Weekday::Tue),
        "wednesday" | "wed" | "weds" => Some(Weekday::Wed),
        "thursday" | "thu" | "thur" | "thurs" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

/// The first date on or after `from` whose weekday is `wd` (so "Wednesday"
/// asked on a Wednesday resolves to today, not next week).
fn next_on_or_after(from: NaiveDate, wd: Weekday) -> NaiveDate {
    let mut d = from;
    for _ in 0..7 {
        if d.weekday() == wd {
            return d;
        }
        d = d.succ_opt().unwrap_or(d);
    }
    from
}

/// A one-paragraph grounding block that anchors the composer to TODAY's local
/// date and resolves every relative day term in `message` to a concrete date.
///
/// This is the fix for Luca's 2026-07-17 transcript: "plan for branzino for
/// tomorrow night" was sent on Friday July 17, and the persona answered "I'll
/// slot it in for Thursday" — a day in the PAST. The composer had no date
/// anchor for a plan-change (write) ask; only read-shaped asks got the scoped
/// week block. So the model guessed a weekday and guessed wrong.
///
/// Unlike [`fetch_scoped`], this block is ALWAYS emitted — a plan-change ask
/// needs the same "today is …, tomorrow = …" anchor as a read-ask. It states
/// the local weekday + date, resolves each relative term present ("tomorrow",
/// "tonight"/"today", "day after tomorrow", and any named weekday → its next
/// occurrence on or after today, never the past), and forbids scheduling into a
/// day that has already passed. `now` is the household local time
/// (`chrono::Local::now()` in production; a fixed clock under test).
pub fn date_anchor(message: &str, now: NaiveDateTime) -> String {
    let today = now.date();
    let fmt = |d: NaiveDate| format!("{}, {}", family_plan::long_weekday(d), d.format("%b %-d"));

    let mut out = format!(
        "Today is {}, {} (the household's local date).",
        family_plan::long_weekday(today),
        today.format("%b %-d, %Y"),
    );

    let norm = normalize(message);
    let mut parts: Vec<String> = Vec::new();
    if norm.contains("day after tomorrow") {
        let d = today
            .succ_opt()
            .and_then(|d| d.succ_opt())
            .unwrap_or(today);
        parts.push(format!("\"day after tomorrow\" = {}", fmt(d)));
    } else if norm.contains("tomorrow") || norm.contains("tmrw") || norm.contains("tmw") {
        let d = today.succ_opt().unwrap_or(today);
        // "tomorrow night" is still tomorrow's date — the evening OF that day.
        parts.push(format!("\"tomorrow\" (incl. \"tomorrow night\") = {}", fmt(d)));
    }
    if norm.contains("tonight") || norm.contains("today") || norm.contains("this evening") {
        parts.push(format!("\"tonight\"/\"today\" = {} (today)", fmt(today)));
    }
    // Named weekdays → the next occurrence on or after today, so a weekday that
    // already passed this week resolves to the coming one, never a past day.
    let mut seen_wd: Vec<Weekday> = Vec::new();
    for tok in norm.split_whitespace() {
        if let Some(wd) = weekday_token(tok) {
            if seen_wd.contains(&wd) {
                continue;
            }
            seen_wd.push(wd);
            let d = next_on_or_after(today, wd);
            parts.push(format!("\"{}\" = {}", family_plan::long_weekday(d), fmt(d)));
        }
    }

    if !parts.is_empty() {
        out.push_str(" Resolve the dates in this message against today: ");
        out.push_str(&parts.join("; "));
        out.push('.');
    }
    out.push_str(" Never schedule anything for a day that has already passed.\n");
    out
}

/// Detect the scope of a read-ask relative to `today`. Priority, most specific
/// first: an explicit weekday name → that day; "day after tomorrow" → today+2;
/// "tomorrow" → today+1; "week"/"weekend" → the whole week; otherwise (the
/// default, including "today"/"tonight"/no time word at all) → today. This is
/// what makes "what's the plan today" mean *today* and not a week-dump.
pub fn detect_scope(message: &str, today: NaiveDate) -> AskScope {
    let norm = normalize(message);
    if let Some(wd) = norm.split_whitespace().find_map(weekday_token) {
        return AskScope::Day(next_on_or_after(today, wd));
    }
    if norm.contains("day after tomorrow") {
        let mut d = today;
        for _ in 0..2 {
            d = d.succ_opt().unwrap_or(d);
        }
        return AskScope::Day(d);
    }
    if norm.contains("tomorrow") || norm.contains("tmrw") || norm.contains("tmw") {
        return AskScope::Day(today.succ_opt().unwrap_or(today));
    }
    if norm.contains("week") || norm.contains("weekend") || norm.contains("coming days")
        || norm.contains("next few days") || norm.contains("days ahead")
        || norm.contains("rest of")
    {
        return AskScope::Week;
    }
    AskScope::Day(today)
}

// ---------------------------------------------------------------------------
// Rule 4 — clock-aware: within the asked day, only what is still coming up
// ---------------------------------------------------------------------------

/// Parse a plan Time-column cell into a wall-clock time. Handles the plan's
/// native 24-hour `"HH:MM"` (`"19:30"`, `"9:00"`), bare hours (`"7"`), and
/// am/pm forms (`"7:30pm"`, `"7 pm"`). `None` for empty/unparseable cells —
/// which the clock filter treats as *not* past (all-day / keep).
pub fn parse_time_of_day(cell: &str) -> Option<NaiveTime> {
    let s = cell.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let (is_pm, is_am, core) = if let Some(rest) = s.strip_suffix("pm") {
        (true, false, rest.trim().to_string())
    } else if let Some(rest) = s.strip_suffix("am") {
        (false, true, rest.trim().to_string())
    } else if let Some(rest) = s.strip_suffix("p.m.") {
        (true, false, rest.trim().to_string())
    } else if let Some(rest) = s.strip_suffix("a.m.") {
        (false, true, rest.trim().to_string())
    } else {
        (false, false, s.clone())
    };
    let core = core.replace('.', ":").replace('h', ":");
    let (h_str, m_str) = match core.split_once(':') {
        Some((h, m)) => (h.trim(), m.trim()),
        None => (core.trim(), "0"),
    };
    let mut hour: u32 = h_str.parse().ok()?;
    let minute: u32 = if m_str.is_empty() { 0 } else { m_str.parse().ok()? };
    if is_pm && hour < 12 {
        hour += 12;
    }
    if is_am && hour == 12 {
        hour = 0;
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// True when an event on `event_day` at clock cell `time_cell` has already
/// passed as of `now` (the household's local time). A day strictly before
/// today's date is past; on today itself, an event is past only when its parsed
/// time is at or before `now`'s time. An empty/unparseable time on today (or
/// any future day) is never past — all-day items are always still "coming up".
pub fn event_has_passed(time_cell: &str, event_day: NaiveDate, now: NaiveDateTime) -> bool {
    if event_day < now.date() {
        return true;
    }
    if event_day > now.date() {
        return false;
    }
    match parse_time_of_day(time_cell) {
        Some(t) => t <= now.time(),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Scoped, clock-aware grounding block (assembles rules 1-4 for the composer)
// ---------------------------------------------------------------------------

/// Does calendar/workout row `row_weekday` (a 3-letter code) fall on `day`?
fn weekday_matches(row_weekday: &str, day: NaiveDate) -> bool {
    family_plan::expand_weekday(row_weekday)
        .eq_ignore_ascii_case(family_plan::long_weekday(day))
}

/// Calendar events for `day`, matched by concrete date when present else by
/// weekday name, in document order.
fn calendar_on<'a>(doc: &'a PlanDoc, day: NaiveDate) -> Vec<&'a family_plan::CalendarEvent> {
    doc.calendar
        .iter()
        .filter(|e| e.date == Some(day) || (e.date.is_none() && weekday_matches(&e.weekday, day)))
        .collect()
}

/// Workout sessions scheduled on `day`, in document order.
fn workouts_on<'a>(doc: &'a PlanDoc, day: NaiveDate) -> Vec<&'a family_plan::WorkoutDay> {
    doc.workouts
        .iter()
        .filter(|w| weekday_matches(&w.weekday, day))
        .collect()
}

/// The single-line "coming up" description of a calendar event.
fn event_line(e: &family_plan::CalendarEvent) -> String {
    if e.time.trim().is_empty() {
        format!("- {}", e.event.trim())
    } else {
        format!("- {} {}", e.time.trim(), e.event.trim())
    }
}

/// The instruction header shared by every grounded read-reply: answer first,
/// keep it to a few lines, and do not bounce a question back. Rule 2's prompt
/// half (the hard post-filter is [`enforce_answer_shape`]); rules 3 & 4 are
/// realised by the *data* below it already being scoped and clock-filtered.
fn answer_shape_header(now: NaiveDateTime) -> String {
    format!(
        "GROUNDING — this is the family's ACTUAL plan for exactly what they asked about. \
         Answer the question directly and specifically from it, then STOP. Rules: \
         (1) lead with the answer — \"Here's today: …\" — no preamble, no \"want the rundown?\". \
         (2) Keep it to 2-5 short lines; do not re-list anything they can already see. \
         (3) Do NOT end with a question — end on a statement (the only exception is if they \
         asked you to help decide or plan). \
         (4) This is already scoped to what they asked and to what is still upcoming as of \
         {} — do not add other days or events that have already passed.\n",
        now.format("%H:%M"),
    )
}

/// Build the scoped, clock-aware grounding block for a read-ask: detect the
/// asked scope from `message`, filter [`PlanDoc`] to exactly that, drop
/// today's already-passed events against `now`, and render a compact block the
/// composer injects. This supersedes [`plan_digest`] for the conversation path.
/// Pure — `now` is injected so behaviour is fully testable with a fixed clock.
pub fn grounded_block(doc: &PlanDoc, now: NaiveDateTime, message: &str) -> String {
    let today = now.date();
    let scope = detect_scope(message, today);
    let mut out = answer_shape_header(now);

    match scope {
        AskScope::Week => {
            // Whole-week ask: the full model (unchanged rule-1 behaviour), but
            // still drop days/events already behind us so "the week" means the
            // rest of it, not Monday's done lunch.
            out.push_str(&plan_digest(doc, today));
            return out;
        }
        AskScope::Day(day) => {
            let is_today = day == today;
            out.push_str(&format!(
                "You are answering about {} {}{}.\n",
                family_plan::long_weekday(day),
                day.format("%b %-d"),
                if is_today { " (today)" } else { "" },
            ));

            let mut lines: Vec<String> = Vec::new();

            if let Some(m) = doc.meal_on(day) {
                let dish = m.dish.trim();
                if !dish.is_empty() {
                    lines.push(format!("Dinner: {dish}"));
                }
            }

            let upcoming: Vec<&family_plan::CalendarEvent> = calendar_on(doc, day)
                .into_iter()
                .filter(|e| !(is_today && event_has_passed(&e.time, day, now)))
                .collect();
            if !upcoming.is_empty() {
                lines.push(if is_today {
                    "Still coming up today:".to_string()
                } else {
                    "On the calendar:".to_string()
                });
                for e in upcoming {
                    lines.push(event_line(e));
                }
            }

            let workouts = workouts_on(doc, day);
            if !workouts.is_empty() {
                lines.push("Workouts:".to_string());
                for w in workouts {
                    lines.push(format!("- {}: {}", w.person, w.session.trim()));
                }
            }

            if lines.is_empty() {
                // Rule 4 tail: nothing left on the asked day. Say so in one line
                // and, for "today", offer tomorrow's first item.
                if is_today {
                    out.push_str(
                        "Nothing left on today's plan — everything is already done for the day.\n",
                    );
                    if let Some(next) = next_item_after(doc, today) {
                        out.push_str(&format!("Next up — {next}\n"));
                    }
                } else {
                    out.push_str("Nothing is on the plan for that day.\n");
                }
            } else {
                for l in lines {
                    out.push_str(&l);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// The first upcoming item strictly after `today`, scanning day by day up to a
/// week out: a dinner or a calendar event, whichever a day carries first.
/// Rendered as a short "Thu: 09:00 Dentist" style string. `None` if the plan
/// holds nothing in the coming week.
fn next_item_after(doc: &PlanDoc, today: NaiveDate) -> Option<String> {
    let mut day = today.succ_opt()?;
    for _ in 0..7 {
        let label = family_plan::long_weekday(day);
        let cal = calendar_on(doc, day);
        if let Some(e) = cal.first() {
            let when = if e.time.trim().is_empty() {
                label.to_string()
            } else {
                format!("{} {}", label, e.time.trim())
            };
            return Some(format!("{}: {}", when, e.event.trim()));
        }
        if let Some(m) = doc.meal_on(day) {
            let dish = m.dish.trim();
            if !dish.is_empty() {
                return Some(format!("{label}: {dish} for dinner"));
            }
        }
        day = day.succ_opt()?;
    }
    None
}

/// Load the current week model under `root` and render it as a scoped,
/// clock-aware grounding block for `message` as of `now`. The conversation
/// path's replacement for [`fetch`]: same best-effort filesystem read, but the
/// block is filtered to the asked scope and to what is still upcoming. `None`
/// when there is no plan to read.
pub fn fetch_scoped(root: &Path, now: NaiveDateTime, message: &str) -> Option<String> {
    let plans = family_plan::load_plans(root);
    let doc = family_plan::current_plan(&plans, now.date())?;
    Some(grounded_block(doc, now, message))
}

// ---------------------------------------------------------------------------
// Rule 5 (docs/20 §6.7) — anti-fabrication: never assert an UNSOURCED schedule fact
// ---------------------------------------------------------------------------
//
// Luca's transcript also caught Otto *volunteering* "you've got a birthday and
// back-to-back meetings — packed day" when the calendar was EMPTY. That is a
// trust-killer distinct from the stall: it is a fabrication, and — critically —
// it was not asked for, so the rule-1 read-shaped grounding never fired on it.
// This rule is the Rust twin of the in-repo JS guard (`composerGuard.mjs` §6.7)
// and it runs on EVERY composed reply (greeting chatter, 1:1, group), at a
// STRICTER tolerance than generic repetition: a single unsourced schedule claim
// rejects the whole draft. Absent/empty grounding is strict — "I can't see the
// calendar" must never license inventing one.

/// SPECIFIC event nouns — a "meeting", a "birthday", an "appointment", a named
/// happening. Groundable ONLY when the SAME noun literally appears in a real
/// calendar title. A reply that says "birthday" with no birthday on the calendar
/// is a fabrication. (Normalisation drops hyphens/apostrophes, see [`normalize`].)
pub const SCHEDULE_EVENT_NOUNS: &[&str] = &[
    "meeting",
    "meetings",
    "birthday",
    "birthdays",
    "appointment",
    "appointments",
    "appt",
    "appts",
    "interview",
    "interviews",
    "deadline",
    "deadlines",
    "anniversary",
    "anniversaries",
    "reservation",
    "reservations",
    "party",
    "parties",
    "checkup",
    "check up",
];

/// QUALITATIVE load claims — "back-to-back", "packed", "busy day". Not a title
/// you can string-match; they assert the day is HEAVY. Groundable ONLY when the
/// grounding actually holds MULTIPLE events (`>= LOAD_CLAIM_MIN_EVENTS`). With an
/// empty (or single-event) calendar, "you're packed today" is a fabrication.
pub const SCHEDULE_LOAD_PHRASES: &[&str] = &[
    "back to back",
    "back-to-back",
    "packed",
    "jam packed",
    "jam-packed",
    "slammed",
    "swamped",
    "wall to wall",
    "wall-to-wall",
    "crammed",
    "crazy busy",
    "busy day",
    "busy today",
    "busy morning",
    "busy afternoon",
    "packed day",
    "full day",
    "fully booked",
    "booked solid",
    "hectic",
];

/// Bare single-word load adjectives, caught as a fallback so a rephrase the
/// phrase list misses ("today's pretty busy" → "busy" is too generic, but
/// "packed"/"booked"/"hectic" alone still trip). Matched as whole tokens.
const BARE_LOAD_WORDS: &[&str] = &[
    "back to back",
    "packed",
    "slammed",
    "swamped",
    "hectic",
    "booked",
];

/// The density at/above which a QUALITATIVE load claim is considered grounded.
/// One event does not make a "back-to-back" day.
pub const LOAD_CLAIM_MIN_EVENTS: usize = 2;

/// The grounding token bag against which a drafted reply's schedule claims are
/// checked: a normalised haystack of every real event title (for noun matching)
/// plus the event COUNT (for load claims). Built from the SAME plan calendar the
/// read block uses. `Default` is the EMPTY/strict grounding — every schedule
/// claim is then unsourced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleGrounding {
    /// Normalised, ` | `-joined haystack of real event titles.
    pub text: String,
    /// Number of real events in the grounded window (for load-claim density).
    pub count: usize,
}

/// Build a [`ScheduleGrounding`] from a list of real event titles (the same
/// event shape `/calendar.json` / the Week view / [`grounded_block`] consume).
/// Blank titles are dropped; the count is the number of real titles.
pub fn build_schedule_grounding(titles: &[String]) -> ScheduleGrounding {
    let kept: Vec<&str> = titles
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    ScheduleGrounding {
        text: normalize(&kept.join(" | ")),
        count: kept.len(),
    }
}

/// True when `term` (already normalised, may be multi-word) appears as a whole
/// token run in the normalised `hay`. Space-padded so "appt" != "apptx".
fn contains_phrase(hay: &str, term: &str) -> bool {
    let t = term.trim();
    if t.is_empty() {
        return false;
    }
    format!(" {hay} ").contains(&format!(" {t} "))
}

/// The schedule/calendar claims in `draft` that have NO support in `grounding`.
/// Empty means the draft asserts nothing the calendar cannot back. A non-empty
/// result is the list of offending terms — a SINGLE one is enough to reject the
/// draft. Specific nouns are unsourced unless the same noun is literally on the
/// calendar; load claims are unsourced unless the window holds `>=2` events.
pub fn find_unsourced_schedule_claims(draft: &str, grounding: &ScheduleGrounding) -> Vec<String> {
    let text = normalize(draft);
    if text.is_empty() {
        return Vec::new();
    }
    let mut offenders: Vec<String> = Vec::new();

    // Specific event nouns — unsourced unless the SAME noun is on the calendar.
    for noun in SCHEDULE_EVENT_NOUNS {
        let n = normalize(noun);
        if contains_phrase(&text, &n) && !contains_phrase(&grounding.text, &n) {
            offenders.push((*noun).to_string());
        }
    }

    // Qualitative load claims — unsourced unless the day genuinely has >=2 events.
    if grounding.count < LOAD_CLAIM_MIN_EVENTS {
        for phrase in SCHEDULE_LOAD_PHRASES {
            let p = normalize(phrase);
            if contains_phrase(&text, &p) && !offenders.iter().any(|o| normalize(o) == p) {
                offenders.push((*phrase).to_string());
            }
        }
        for bare in BARE_LOAD_WORDS {
            let b = normalize(bare);
            if contains_phrase(&text, &b) && !offenders.iter().any(|o| normalize(o) == b) {
                offenders.push((*bare).to_string());
            }
        }
    }

    offenders
}

/// True when `draft` asserts a schedule/calendar fact absent from `grounding`.
pub fn fabricates_schedule(draft: &str, grounding: &ScheduleGrounding) -> bool {
    !find_unsourced_schedule_claims(draft, grounding).is_empty()
}

/// The honest, factually-EMPTY line sent in place of a fabricated schedule
/// reply: warm, but volunteering NO invented specifics — it says plainly there
/// is nothing on the calendar and hands the turn back. Like the repetition
/// fallback it may end on a question, because it offers to do the real work
/// rather than assert a fact it cannot back.
pub fn grounding_fallback_line() -> String {
    "I'm not seeing anything on the calendar for that — I don't want to make something up. \
     Want me to open it and take a proper look?"
        .to_string()
}

/// Build the anti-fabrication [`ScheduleGrounding`] for a composed reply: the
/// REAL calendar events in the window the reply is about, as of `now`. Scope is
/// resolved from `message` the same way [`grounded_block`] scopes the injected
/// context (so the guard checks against exactly what the model was shown): a
/// day-ask grounds on that day's still-upcoming events; a week-ask grounds on
/// every still-upcoming event in the week; a greeting (default scope) grounds on
/// today. Pure — `now` is injected for fixed-clock tests.
pub fn schedule_grounding_for(doc: &PlanDoc, now: NaiveDateTime, message: &str) -> ScheduleGrounding {
    let today = now.date();
    let titles = match detect_scope(message, today) {
        AskScope::Day(day) => upcoming_titles_on(doc, day, now),
        AskScope::Week => {
            let mut all: Vec<String> = Vec::new();
            let mut day = today;
            for _ in 0..7 {
                all.extend(upcoming_titles_on(doc, day, now));
                match day.succ_opt() {
                    Some(d) => day = d,
                    None => break,
                }
            }
            all
        }
    };
    build_schedule_grounding(&titles)
}

/// Real event titles on `day` that are still upcoming as of `now` (today's
/// already-passed events are dropped; future days keep everything).
fn upcoming_titles_on(doc: &PlanDoc, day: NaiveDate, now: NaiveDateTime) -> Vec<String> {
    let is_today = day == now.date();
    calendar_on(doc, day)
        .into_iter()
        .filter(|e| !(is_today && event_has_passed(&e.time, day, now)))
        .map(|e| e.event.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Load the current week model under `root` and build the anti-fabrication
/// [`ScheduleGrounding`] for `message` as of `now`. Best-effort filesystem read;
/// when there is NO plan the grounding is EMPTY (strict): every schedule claim in
/// the drafted reply is then treated as unsourced. This is the guard's data seam,
/// the twin of [`fetch_scoped`] for the prompt-injection seam.
pub fn fetch_schedule_grounding(root: &Path, now: NaiveDateTime, message: &str) -> ScheduleGrounding {
    let plans = family_plan::load_plans(root);
    match family_plan::current_plan(&plans, now.date()) {
        Some(doc) => schedule_grounding_for(doc, now, message),
        None => ScheduleGrounding::default(),
    }
}

/// The ALWAYS-ON calendar-truth line injected into every compose prompt so the
/// model has the real, clock-scoped calendar in front of it BEFORE it drafts —
/// the root cause of the fabrication was that the calendar was simply not in the
/// context for non-read-shaped chatter. Names today's real upcoming events (or
/// states plainly that the day is clear) and forbids inventing any other
/// meeting/appointment/birthday or calling the day packed/back-to-back. Pure.
pub fn schedule_context_line(doc: Option<&PlanDoc>, now: NaiveDateTime) -> String {
    let today = now.date();
    let label = format!("{} {}", family_plan::long_weekday(today), today.format("%b %-d"));
    let titles = doc
        .map(|d| upcoming_titles_on(d, today, now))
        .unwrap_or_default();
    if titles.is_empty() {
        format!(
            "CALENDAR ({label}) — there is NOTHING on the calendar today. Do NOT invent a \
             meeting, appointment, birthday, or any event, and do NOT say the day is \
             busy/packed/back-to-back. If asked, say the calendar is clear.\n"
        )
    } else {
        format!(
            "CALENDAR ({label}) — the ONLY real events today are: {}. Mention ONLY these; do \
             NOT invent any other meeting, appointment, or birthday, and only call the day \
             busy/packed if there are genuinely several.\n",
            titles.join("; ")
        )
    }
}

/// Load the current week model under `root` and render [`schedule_context_line`]
/// for it as of `now`. Best-effort; a missing plan yields the empty-calendar
/// (strict) truth line so the model is still told the day is clear.
pub fn fetch_schedule_context_line(root: &Path, now: NaiveDateTime) -> String {
    let plans = family_plan::load_plans(root);
    let doc = family_plan::current_plan(&plans, now.date());
    schedule_context_line(doc, now)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Lowercase, drop non-alphanumeric (keeping spaces), collapse whitespace.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if ch == '\'' || ch == '\u{2019}' {
            // Drop apostrophes so contractions normalise to one token:
            // "that's" -> "thats", "what's" -> "whats".
            continue;
        } else {
            // Any other separator (whitespace or punctuation) collapses to a
            // single space so word boundaries are preserved.
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        }
    }
    out.trim().to_string()
}

/// The set of character trigrams of an already-normalised string.
fn trigrams(norm: &str) -> std::collections::HashSet<[char; 3]> {
    let chars: Vec<char> = norm.chars().collect();
    let mut set = std::collections::HashSet::new();
    if chars.len() < 3 {
        return set;
    }
    for w in chars.windows(3) {
        set.insert([w[0], w[1], w[2]]);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::family_plan::PlanDoc;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn dt(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(hh, mm, 0).unwrap()
    }

    /// THE LIVE REGRESSION (Luca, 2026-07-17): on Friday July 17, "tomorrow
    /// night" must resolve to SATURDAY July 18, never a past weekday (the
    /// persona answered "Thursday"). The anchor states today AND resolves the
    /// relative term, and forbids scheduling into the past.
    #[test]
    fn date_anchor_resolves_tomorrow_night_to_the_next_day_never_the_past() {
        // Fixed fake "today" = Friday, July 17 2026.
        let now = dt(2026, 7, 17, 9, 0);
        let anchor = date_anchor("hey plan for branzino for tomorrow night", now);
        assert!(
            anchor.contains("Today is Friday, Jul 17, 2026"),
            "anchor must state today's local date, got: {anchor}"
        );
        assert!(
            anchor.contains("Saturday, Jul 18"),
            "'tomorrow night' from Fri Jul 17 must resolve to Sat Jul 18, got: {anchor}"
        );
        // The wrong answer Otto gave — a PAST weekday — must never appear.
        assert!(
            !anchor.contains("Thursday"),
            "'tomorrow' must never resolve to a past weekday, got: {anchor}"
        );
        assert!(
            anchor.contains("already passed"),
            "anchor must forbid scheduling into the past, got: {anchor}"
        );
    }

    /// A named weekday that already went by this week resolves to the COMING
    /// one, not the past instance — the same "never a past day" guarantee.
    #[test]
    fn date_anchor_named_weekday_resolves_forward_only() {
        // Friday July 17: "Thursday" this week (Jul 16) is in the past, so the
        // next Thursday is Jul 23.
        let now = dt(2026, 7, 17, 9, 0);
        let anchor = date_anchor("can we do salmon on thursday", now);
        assert!(
            anchor.contains("Thursday, Jul 23"),
            "a passed weekday must resolve to next week, got: {anchor}"
        );
    }

    /// "tonight"/"today" stay on today's date (the evening OF today).
    #[test]
    fn date_anchor_tonight_is_today() {
        let now = dt(2026, 7, 17, 9, 0);
        let anchor = date_anchor("what's for dinner tonight", now);
        assert!(anchor.contains("(today)"), "tonight resolves to today, got: {anchor}");
        assert!(anchor.contains("Friday, Jul 17"), "got: {anchor}");
        assert!(!anchor.contains("Jul 18"), "tonight is not tomorrow, got: {anchor}");
    }

    const PLAN: &str = "\
# 2026-W29 Family Plan

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** DRAFT

## 1. Meals

| Day | Slot | Dinner | Prep |
|-----|------|--------|------|
| Mon 07-13 | Vegetarian | Chickpea & spinach curry, brown rice | ~35 min |
| Tue 07-14 | Fish | Baked salmon, roasted potatoes, green beans | ~30 min |
| Wed 07-15 | Flex | Leftovers | ~10 min |

## 2. Workouts

### Luca — strength
| Day | Session |
|-----|---------|
| Mon | Lower (strength) |

### Nadin — cardio
| Day | Session |
|-----|---------|
| Tue | Intervals |

## 3. Calendar

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Tue 07-14 | 19:30 | Luca PT check-in | Otto |
| Thu 07-16 | 09:00 | Dentist — Nadin | Otto |

## 4. Shopping list

### Market
- Salmon
- Spinach
- Brown rice
";

    // -- Rule 1: grounding classification -----------------------------------

    #[test]
    fn ground_read_shaped_detects_the_transcript_asks() {
        // Every one of Luca's read-shaped asks from the fixture must classify.
        for ask in [
            "Plans for tomorrow?",
            "walk me through it",
            "what about the calendar",
            "give me the rundown",
            "You need to read the calendar",
            "what's for dinner tomorrow?",
            "how's the week looking?",
            "what's on the schedule today?",
        ] {
            assert!(is_read_shaped(ask), "should be read-shaped: {ask:?}");
        }
    }

    #[test]
    fn ground_read_shaped_ignores_pure_edits_and_chitchat() {
        for ask in [
            "swap Friday to tacos",
            "add milk to the shopping list",
            "thanks, that's great",
            "good morning!",
            "lol nice",
        ] {
            assert!(!is_read_shaped(ask), "should NOT be read-shaped: {ask:?}");
        }
    }

    #[test]
    fn ground_plan_digest_carries_meals_calendar_and_instruction() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        let digest = plan_digest(&doc, date(2026, 7, 15));
        // The anti-stall instruction is present.
        assert!(digest.to_lowercase().contains("do not stall"));
        // Concrete meals and a real appointment are in the block.
        assert!(digest.contains("Chickpea & spinach curry, brown rice"));
        assert!(digest.contains("Baked salmon"));
        assert!(digest.contains("Luca PT check-in"));
        assert!(digest.contains("Dentist — Nadin"));
        assert!(digest.contains("2026-W29"));
    }

    #[test]
    fn ground_fetch_reads_current_plan_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), PLAN).unwrap();

        let digest = fetch(dir.path(), date(2026, 7, 15)).expect("plan should be found");
        assert!(digest.contains("Baked salmon"));
        // No plans dir → None, never a panic.
        let empty = tempfile::tempdir().unwrap();
        assert!(fetch(empty.path(), date(2026, 7, 15)).is_none());
    }

    // -- Rule 2: repetition guard -------------------------------------------

    #[test]
    fn ground_repetition_flags_near_identical_stalls() {
        // The transcript's repeated stall, lightly reworded each time.
        let a = "Meals are set, just waiting on confirmations from you and Nadin. Want the rundown?";
        let b = "Meals are all set — still waiting on confirmations from you and Nadin. Want a rundown?";
        assert!(is_repetitive(b, a), "sim={}", similarity(a, b));
    }

    #[test]
    fn ground_repetition_allows_the_real_grounded_answer() {
        let stall = "Meals are set, waiting on confirmations from you and Nadin. Want the rundown?";
        let real = "Tomorrow (Tue) it's baked salmon with roasted potatoes and green beans, \
                    and you've got your PT check-in at 7:30pm.";
        assert!(!is_repetitive(real, stall), "sim={}", similarity(stall, real));
    }

    #[test]
    fn ground_repetition_fallback_is_honest_and_distinct() {
        let stall = "Meals are set, waiting on confirmations from you and Nadin. Want the rundown?";
        let fallback = repetition_fallback_line();
        assert!(!is_repetitive(&fallback, stall));
        assert!(fallback.to_lowercase().contains("read"));
    }

    // -- Rule 3: corrections stick ------------------------------------------

    #[test]
    fn ground_correction_detected_from_the_fixture_line() {
        let c = detect_correction("Nadin is not logged so ignore this");
        assert!(c.is_some());
        assert!(c.unwrap().contains("Nadin"));
    }

    #[test]
    fn ground_correction_detects_common_shapes() {
        for msg in [
            "no, that's wrong",
            "actually that's not right",
            "forget what I said",
            "scratch that",
            "Nadin isn't logged",
            "disregard the last one",
        ] {
            assert!(detect_correction(msg).is_some(), "should detect: {msg:?}");
        }
    }

    #[test]
    fn ground_correction_ignores_ordinary_messages() {
        for msg in ["what's for dinner?", "thanks!", "swap Friday to tacos"] {
            assert!(detect_correction(msg).is_none(), "should NOT detect: {msg:?}");
        }
    }

    #[test]
    fn ground_corrections_block_replays_every_correction() {
        let corr = vec![
            "Nadin is not logged so ignore this".to_string(),
            "the dentist is Thursday not Friday".to_string(),
        ];
        let block = corrections_block(&corr).expect("block");
        assert!(block.contains("Nadin is not logged"));
        assert!(block.contains("dentist is Thursday"));
        assert!(block.to_lowercase().contains("never repeat"));
        assert!(corrections_block(&[]).is_none());
    }

    // -- Rule 4: style ------------------------------------------------------

    #[test]
    fn ground_style_strips_formulaic_tail_when_already_used() {
        let reply = "Tomorrow it's salmon. Anything specific you want to know?";
        let (body, tail) = strip_trailing_formulaic(reply);
        assert_eq!(tail.as_deref(), Some("Anything specific you want to know?"));
        assert_eq!(body, "Tomorrow it's salmon.");
        // Second use in the conversation → dropped.
        assert_eq!(enforce_style(reply, true), "Tomorrow it's salmon.");
        // First use → kept.
        assert_eq!(enforce_style(reply, false), reply);
    }

    #[test]
    fn ground_style_leaves_substantive_questions_alone() {
        let reply = "Do you want salmon or curry on Wednesday?";
        assert!(strip_trailing_formulaic(reply).1.is_none());
        assert_eq!(enforce_style(reply, true), reply);
    }

    #[test]
    fn ground_style_counts_formulaic_across_history() {
        let history = vec![
            "Meals are set. Want the rundown?".to_string(),
            "Still waiting on Nadin. Anything else?".to_string(),
            "Tomorrow it's salmon.".to_string(),
        ];
        assert_eq!(count_formulaic(&history), 2);
    }

    #[test]
    fn ground_style_never_empties_a_reply() {
        // A reply that is ONLY a formulaic question must not be emptied.
        let reply = "Anything specific you want to know?";
        assert_eq!(enforce_style(reply, true), reply);
    }

    // -- Answer-shape: scope, clock, and the hard no-question rule -----------

    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(hh, mm, 0).unwrap()
    }

    // (a) A plain read-ask reply ends on a statement — zero trailing question.
    #[test]
    fn shape_read_reply_has_zero_trailing_question() {
        // The ask is read-shaped and NOT a deliberation → question stripped.
        assert!(is_read_shaped("what's the plan today?"));
        assert!(!is_deliberation_request("what's the plan today?"));

        let drafted = "Here's today: leftovers for dinner and your lower-body \
                       session. Anything else you want to know?";
        let shaped = enforce_answer_shape(drafted, false);
        assert!(!shaped.trim_end().ends_with('?'), "still a question: {shaped:?}");
        assert_eq!(shaped, "Here's today: leftovers for dinner and your lower-body session.");

        // A non-formulaic trailing question is stripped just the same — the
        // rule is hard, not limited to the "Anything specific…?" reflex.
        let q2 = "Dinner is salmon. Want me to walk through the workouts too?";
        assert_eq!(enforce_answer_shape(q2, false), "Dinner is salmon.");

        // A reply already ending on a statement is untouched.
        let plain = "Here's today: leftovers for dinner.";
        assert_eq!(enforce_answer_shape(plain, false), plain);
    }

    // (b) A "today" ask never includes other days' items.
    #[test]
    fn shape_today_ask_scopes_to_today_only() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Asked at noon on Wed 07-15; Wed's dinner is Leftovers.
        let block = grounded_block(&doc, at(2026, 7, 15, 12, 0), "what's the plan today?");
        assert!(block.contains("Leftovers"), "should have today's dinner:\n{block}");
        assert!(block.contains("Wednesday"));
        // NOTHING from other days may leak in.
        assert!(!block.contains("Baked salmon"), "Tue meal leaked:\n{block}");
        assert!(!block.contains("Chickpea"), "Mon meal leaked:\n{block}");
        assert!(!block.contains("Dentist"), "Thu appt leaked:\n{block}");
        assert!(!block.contains("Luca PT check-in"), "Tue appt leaked:\n{block}");
    }

    #[test]
    fn shape_detect_scope_reads_the_asked_window() {
        let today = date(2026, 7, 15); // Wednesday
        assert_eq!(detect_scope("what's for dinner today?", today), AskScope::Day(today));
        assert_eq!(detect_scope("what's the plan?", today), AskScope::Day(today));
        assert_eq!(
            detect_scope("plans for tomorrow?", today),
            AskScope::Day(date(2026, 7, 16))
        );
        assert_eq!(
            detect_scope("anything on friday?", today),
            AskScope::Day(date(2026, 7, 17))
        );
        // A weekday that is today resolves to today, not next week.
        assert_eq!(detect_scope("what's on wednesday?", today), AskScope::Day(today));
        assert_eq!(detect_scope("how's the week looking?", today), AskScope::Week);
        assert_eq!(detect_scope("anything this weekend?", today), AskScope::Week);
    }

    #[test]
    fn shape_tomorrow_ask_shows_tomorrows_items() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Asked on Wed; tomorrow = Thu 07-16 → Dentist at 09:00, no meal row.
        let block = grounded_block(&doc, at(2026, 7, 15, 12, 0), "what's on tomorrow?");
        assert!(block.contains("Thursday"));
        assert!(block.contains("Dentist"), "Thu appt missing:\n{block}");
        assert!(!block.contains("Leftovers"), "today's meal leaked into tomorrow:\n{block}");
    }

    // (c) Clock-aware: past-time events drop given a fixed fake now.
    #[test]
    fn shape_clock_filter_drops_past_events() {
        // Pure predicate: Tue 07-14 has the 19:30 PT check-in.
        let day = date(2026, 7, 14);
        // Before 19:30 → still coming up.
        assert!(!event_has_passed("19:30", day, at(2026, 7, 14, 15, 0)));
        // After 19:30 → passed.
        assert!(event_has_passed("19:30", day, at(2026, 7, 14, 20, 0)));
        // A day already behind us is entirely past regardless of clock.
        assert!(event_has_passed("19:30", day, at(2026, 7, 15, 8, 0)));
        // A future day is never past.
        assert!(!event_has_passed("09:00", date(2026, 7, 16), at(2026, 7, 14, 23, 0)));
        // Empty / all-day time on today is kept (not past).
        assert!(!event_has_passed("", day, at(2026, 7, 14, 23, 0)));

        let doc = PlanDoc::parse("2026-W29", PLAN);
        // At 15:00 on Tue the 19:30 check-in is still ahead → present.
        let early = grounded_block(&doc, at(2026, 7, 14, 15, 0), "what's the plan today?");
        assert!(early.contains("Luca PT check-in"), "should still be upcoming:\n{early}");
        // At 20:00 on Tue it has passed → gone from "still coming up".
        let late = grounded_block(&doc, at(2026, 7, 14, 20, 0), "what's the plan today?");
        assert!(!late.contains("Luca PT check-in"), "past event should be dropped:\n{late}");
    }

    #[test]
    fn shape_time_parser_handles_common_forms() {
        assert_eq!(parse_time_of_day("19:30"), NaiveTime::from_hms_opt(19, 30, 0));
        assert_eq!(parse_time_of_day("9:00"), NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(parse_time_of_day("7:30pm"), NaiveTime::from_hms_opt(19, 30, 0));
        assert_eq!(parse_time_of_day("7 pm"), NaiveTime::from_hms_opt(19, 0, 0));
        assert_eq!(parse_time_of_day("12am"), NaiveTime::from_hms_opt(0, 0, 0));
        assert_eq!(parse_time_of_day("12pm"), NaiveTime::from_hms_opt(12, 0, 0));
        assert_eq!(parse_time_of_day(""), None);
        assert_eq!(parse_time_of_day("whenever"), None);
    }

    // Rule 4 tail: everything today has passed → say so, offer tomorrow.
    #[test]
    fn shape_spent_day_offers_tomorrow() {
        // A minimal plan where "today" (Fri 07-17) carries only an 08:00 event
        // and no dinner/workout; Saturday has the next item.
        const PLAN_TAIL: &str = "\
# 2026-W29 Family Plan

**Week of Monday 2026-07-13 to Sunday 2026-07-19**

## 3. Calendar

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Fri 07-17 | 08:00 | Early call | Otto |
| Sat 07-18 | 10:00 | Farmers market | Otto |
";
        let doc = PlanDoc::parse("2026-W29", PLAN_TAIL);
        // Asked Fri at 18:00 — the 08:00 call is long done.
        let block = grounded_block(&doc, at(2026, 7, 17, 18, 0), "what's left today?");
        assert!(!block.contains("Early call"), "past event leaked:\n{block}");
        assert!(
            block.to_lowercase().contains("nothing left"),
            "should announce the day is spent:\n{block}"
        );
        assert!(block.contains("Farmers market"), "should offer tomorrow's item:\n{block}");
    }

    // (d) A deliberation ask IS allowed to end with a question.
    #[test]
    fn shape_deliberation_ask_keeps_its_question() {
        for ask in [
            "let's think about the day",
            "help me decide what to cook",
            "what should we do this weekend?",
            "not sure what to make — any ideas?",
        ] {
            assert!(is_deliberation_request(ask), "should be deliberation: {ask:?}");
        }
        // When deliberation is invited, the trailing question is preserved.
        let reply = "We could do salmon or the curry. Which sounds better tonight?";
        assert_eq!(enforce_answer_shape(reply, true), reply);
        // And a plain read-ask is NOT mistaken for deliberation.
        assert!(!is_deliberation_request("what's for dinner today?"));
    }

    #[test]
    fn shape_week_ask_stays_a_week_view() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        let block = grounded_block(&doc, at(2026, 7, 13, 9, 0), "how's the week looking?");
        // A week ask still surfaces multiple days' meals.
        assert!(block.contains("Baked salmon"));
        assert!(block.contains("Chickpea"));
    }

    // -- Rule 5 (§6.7): anti-fabrication grounding guard --------------------

    // The exact transcript fabrication: Otto volunteers a birthday + back-to-back
    // meetings + a packed day on an EMPTY calendar. EVERY invented specific must
    // be flagged so the draft is rejected.
    #[test]
    fn ground_fabrication_rejects_the_transcript_invention_on_empty_calendar() {
        let empty = ScheduleGrounding::default();
        let draft = "Morning! You've got a birthday today and back-to-back meetings — \
                     pretty packed day ahead.";
        let offenders = find_unsourced_schedule_claims(draft, &empty);
        assert!(fabricates_schedule(draft, &empty), "must be flagged: {offenders:?}");
        // Each distinct invented specific is caught.
        assert!(offenders.iter().any(|o| o == "birthday"), "birthday missed: {offenders:?}");
        assert!(offenders.iter().any(|o| o == "meetings"), "meetings missed: {offenders:?}");
        assert!(
            offenders.iter().any(|o| o.contains("back") || o == "packed" || o == "packed day"),
            "load claim missed: {offenders:?}"
        );
    }

    // Absent grounding is STRICT: a lone schedule noun with no calendar is a
    // fabrication. "I can't see the calendar" never licenses inventing one.
    #[test]
    fn ground_fabrication_empty_grounding_is_strict() {
        let empty = ScheduleGrounding::default();
        for draft in [
            "You have a meeting at 3.",
            "Don't forget the appointment tomorrow.",
            "It's going to be a hectic day.",
            "Your day is booked solid.",
        ] {
            assert!(fabricates_schedule(draft, &empty), "should reject: {draft:?}");
        }
    }

    // A claim SOURCED by the real calendar passes: the noun is literally on it,
    // or the load claim has the >=2 events to back it.
    #[test]
    fn ground_fabrication_allows_sourced_claims() {
        // Two real events → "back-to-back" is grounded; "meeting" is on a title.
        let g = build_schedule_grounding(&[
            "Team meeting".to_string(),
            "Dentist — Nadin".to_string(),
        ]);
        assert!(!fabricates_schedule("You've got a meeting then the dentist — a busy day.", &g));
        assert!(!fabricates_schedule("Back-to-back today: the meeting and the dentist.", &g));
        // But a birthday nobody scheduled is STILL a fabrication even here.
        assert!(fabricates_schedule("And it's someone's birthday too.", &g));
    }

    // One event does NOT make a back-to-back / packed day.
    #[test]
    fn ground_fabrication_single_event_is_not_packed() {
        let g = build_schedule_grounding(&["Dentist — Nadin".to_string()]);
        assert_eq!(g.count, 1);
        assert!(fabricates_schedule("You're totally packed today.", &g));
        assert!(fabricates_schedule("It's back to back all day.", &g));
        // Mentioning the one real thing (no load claim, no invented noun) is fine.
        assert!(!fabricates_schedule("You've got the dentist today.", &g));
    }

    // Ordinary, schedule-free chatter is never touched.
    #[test]
    fn ground_fabrication_ignores_ordinary_chatter() {
        let empty = ScheduleGrounding::default();
        for draft in [
            "Doing great, thanks for asking! How are you?",
            "Dinner tonight is salmon — sounds delicious.",
            "Good morning! Hope you slept well.",
        ] {
            assert!(!fabricates_schedule(draft, &empty), "should NOT reject: {draft:?}");
        }
    }

    // The guard's grounding is built from the REAL plan, scoped like the prompt.
    #[test]
    fn ground_schedule_grounding_scopes_from_the_plan() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Greeting on Tue at 15:00 → today's still-upcoming event = PT check-in.
        let g = schedule_grounding_for(&doc, at(2026, 7, 14, 15, 0), "how's your day going?");
        assert_eq!(g.count, 1, "Tue has one upcoming event: text={:?}", g.text);
        assert!(g.text.contains("pt check in") || g.text.contains("check in"), "text={:?}", g.text);
        // A reply grounded in that real event passes; an invented meeting fails.
        assert!(!fabricates_schedule("You've got your PT check-in at 7:30.", &g));
        assert!(fabricates_schedule("You've got a meeting at noon.", &g));
        // After 19:30 the check-in has passed → empty grounding → strict again.
        let spent = schedule_grounding_for(&doc, at(2026, 7, 14, 20, 0), "how's your day going?");
        assert_eq!(spent.count, 0);
        assert!(fabricates_schedule("You've still got your check-in and a meeting.", &spent));
    }

    // The fallback is honest and volunteers no invented specifics.
    #[test]
    fn ground_fabrication_fallback_invents_nothing() {
        let empty = ScheduleGrounding::default();
        let fallback = grounding_fallback_line();
        assert!(!fabricates_schedule(&fallback, &empty), "fallback must be clean: {fallback}");
        assert!(fallback.to_lowercase().contains("calendar"));
    }

    // The ALWAYS-ON context line names real events, or states the day is clear —
    // and always forbids inventing. This is the root-cause fix: the calendar is
    // now in the compose context even for non-read-shaped chatter.
    #[test]
    fn ground_schedule_context_line_states_the_truth() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Tue at 15:00 → names the real upcoming event, forbids invention.
        let with = schedule_context_line(Some(&doc), at(2026, 7, 14, 15, 0));
        assert!(with.contains("PT check-in"), "should name the real event:\n{with}");
        assert!(with.to_lowercase().contains("do not invent") || with.to_lowercase().contains("do not"), "{with}");
        // No plan at all → explicit empty-calendar truth.
        let none = schedule_context_line(None, at(2026, 7, 14, 15, 0));
        assert!(none.to_lowercase().contains("nothing on the calendar"), "{none}");
        assert!(none.to_lowercase().contains("do not invent"), "{none}");
        // A spent day (asked Thu 20:00, after the 09:00 dentist) → clear.
        let spent = schedule_context_line(Some(&doc), at(2026, 7, 16, 20, 0));
        assert!(spent.to_lowercase().contains("nothing on the calendar"), "{spent}");
    }

    // --- Rule 6: no dangling-promise deferral tail (task owner-pin-engine) ---

    /// THE 17:5x REPRO: the answer lands, then dangles "…let me get Nora's exact
    /// take" — a fresh promise to no one. The guard strips ONLY that trailing
    /// clause and leaves the delivered answer intact.
    #[test]
    fn deferral_tail_is_stripped_from_delivered_answer() {
        // The exact repro tail (ellipsis-appended, third-person self-reference).
        let repro = "Pasta pomodoro is solid at 400-450 calories a plate. Let me get Nora's exact take.";
        let cleaned = enforce_no_deferral(repro);
        assert!(
            cleaned.contains("400-450"),
            "the delivered answer must survive the strip:\n{cleaned}"
        );
        assert!(
            !cleaned.to_lowercase().contains("exact take"),
            "the deferral tail must be gone:\n{cleaned}"
        );

        // A variety of deferral shapes, each appended to a real answer.
        for tail in [
            "…let me get her exact take",
            "I'll get back to you with the exact numbers.",
            "Let me check with Nora on that.",
            "let me confirm the exact figure",
            "I'll circle back on it.",
        ] {
            let reply = format!("It's about 450 calories. {tail}");
            let out = enforce_no_deferral(&reply);
            assert!(
                out.to_lowercase().contains("450 calories"),
                "answer lost for tail {tail:?}:\n{out}"
            );
            assert!(
                out.len() < reply.len(),
                "tail {tail:?} was not stripped:\n{out}"
            );
        }
    }

    /// A legitimate action-ack ("on it, I'll change the week") is NOT a deferral
    /// — the markers are specific enough not to swallow real commitments, and a
    /// plain answer with no tail is returned unchanged.
    #[test]
    fn deferral_guard_leaves_legitimate_replies_untouched() {
        for ok in [
            "On it — I'll change the week to duck on Thursday.",
            "Pasta pomodoro is about 450 calories a plate.",
            "Sounds good, see you tonight!",
            "I'll add it to the shopping list right now.",
        ] {
            assert_eq!(
                enforce_no_deferral(ok),
                ok,
                "a legitimate reply was mangled by the deferral guard:\n{ok}"
            );
        }
    }

    /// A reply that is ONLY a deferral (no body left) is returned unchanged — the
    /// guard never sends nothing.
    #[test]
    fn deferral_guard_never_empties_the_reply() {
        let only = "Let me get Nora's exact take.";
        assert_eq!(enforce_no_deferral(only), only, "must never strip to empty");

        // strip_deferral_tail still reports the tail for a body-bearing reply,
        // and reports None when there is no deferral.
        let (body, tail) = strip_deferral_tail("It's 450 calories. Let me get her exact take.");
        assert!(body.contains("450"), "{body}");
        assert!(tail.is_some(), "tail should be detected");
        let (body2, tail2) = strip_deferral_tail("It's 450 calories, enjoy!");
        assert_eq!(body2, "It's 450 calories, enjoy!");
        assert!(tail2.is_none());
    }
}
