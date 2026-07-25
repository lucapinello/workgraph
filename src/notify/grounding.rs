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
// WEEK CONTEXT (task week-grounding-engine) — the gateway forwards the parsed
// Dinners table (day→dish + family-local today/tomorrow markers) via the
// `WG_WEEK_CONTEXT` env var, built by the SAME `weekSource` parser the Week view
// uses. This is the engine-side twin of the anti-fabrication guard: the composer
// answers dinner/meal questions FROM the table (prompt injection, below), and a
// reply that FALSELY claims a planned day is empty — the LIVE Nora bug, "Nothing's
// locked in for Saturday yet" while the table has "Saturday: Baked white fish" —
// is rewritten to the honest answer (the dish). The gateway's never-claim-empty
// guard cannot catch these because engine-composed replies write to the feed via
// FeedMirrorSink in the ENGINE process, so the guard MUST live here.
// ---------------------------------------------------------------------------

/// The parsed `WG_WEEK_CONTEXT`: which weekdays have a planned dinner (keyed by
/// lowercase full weekday name → dish text), plus which weekday "today" and
/// "tomorrow" resolve to (so a relative-day empty-claim — "nothing for tomorrow"
/// — can be checked against the real plan). A day whose dish is blank or the
/// sentinel "not planned yet" is NOT recorded as planned.
#[derive(Debug, Default, Clone)]
pub struct WeekContext {
    by_day: std::collections::HashMap<String, String>,
    today: Option<String>,
    tomorrow: Option<String>,
}

impl WeekContext {
    /// No day carries a planned dish — the guard is a no-op.
    pub fn is_empty(&self) -> bool {
        self.by_day.is_empty()
    }
}

/// Parse the `WG_WEEK_CONTEXT` text the gateway builds (see
/// `weekSource.buildWeekContext`): `- Saturday (July 25): Baked white fish…` day
/// lines, plus `Today is Friday — dinner: …` / `Tomorrow is Saturday — dinner:
/// …` markers. Tolerant and pure — any line it doesn't recognise is ignored, so
/// a future gateway format tweak degrades to "fewer planned days", never a panic.
pub fn parse_week_context(text: &str) -> WeekContext {
    let mut wc = WeekContext::default();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("- ") {
            // "Saturday (July 25): Baked white fish" → day, dish.
            if let Some((left, dish)) = rest.split_once(':') {
                let day = left
                    .split('(')
                    .next()
                    .unwrap_or(left)
                    .trim()
                    .to_lowercase();
                let dish = dish.trim();
                if weekday_token(&day).is_some()
                    && !dish.is_empty()
                    && !dish.eq_ignore_ascii_case("not planned yet")
                {
                    wc.by_day.insert(day, dish.to_string());
                }
            }
        } else if let Some(rest) = line.strip_prefix("Today is ") {
            wc.today = first_weekday_word(rest);
        } else if let Some(rest) = line.strip_prefix("Tomorrow is ") {
            wc.tomorrow = first_weekday_word(rest);
        }
    }
    wc
}

/// The first token in `s` that is a weekday name, lowercased (e.g. from "Friday
/// — dinner: …" → "friday"). `None` when no leading weekday is present.
fn first_weekday_word(s: &str) -> Option<String> {
    for tok in s.split(|c: char| !c.is_alphabetic()) {
        let t = tok.to_lowercase();
        if weekday_token(&t).is_some() {
            return Some(t);
        }
    }
    None
}

/// The compose-prompt block for a forwarded `WG_WEEK_CONTEXT`: the gateway's
/// parsed Dinners table wrapped with the standing instruction to answer
/// dinner/meal questions FROM the table and to NEVER claim a day is empty when it
/// has an entry here. Mirrors the thread-context injection (a raw forwarded block
/// plus an explicit instruction). `None` when the forwarded context is blank so
/// the prompt is unchanged on the Telegram-listener path (env unset).
pub fn week_context_block(week_context: &str) -> Option<String> {
    let text = week_context.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str(
        "THIS WEEK'S DINNERS — the family's real plan, parsed from the Dinners table. \
         Answer any dinner or meal question (today, tonight, tomorrow, or a named day) \
         FROM this table, never from a plan's prose notes or a week \"skeleton\". If a \
         day below has a dish, that day IS planned — NEVER say it is empty, unplanned, \
         not locked in, not set, or undecided:\n",
    );
    out.push_str(text);
    out.push('\n');
    Some(out)
}

// Negation tokens that, alongside a planning word, mark a sentence as asserting
// nothing is planned. Matched as whole words against the NORMALISED sentence
// (apostrophes dropped, so "isn't"→"isnt", "nothing's"→"nothings").
const WEEK_EMPTY_NEGATIONS: &[&str] = &[
    "nothing", "nothings", "no", "not", "none", "nope", "nada", "havent", "hasnt", "hadnt",
    "dont", "doesnt", "didnt", "isnt", "arent", "wasnt", "werent", "cant", "wont", "unplanned",
    "undecided", "tbd", "blank", "empty",
];

// Planning-status stems: a normalised token STARTING with any of these, in a
// sentence that also carries a negation, means "no plan for the meal". Stems (not
// whole words) so "planned/planning", "locked", "scheduled", "decided",
// "cooking", "figured", "eating" all match.
const WEEK_PLAN_STEMS: &[&str] = &[
    "plan", "lock", "schedul", "set", "settl", "decid", "menu", "figur", "nail", "sort",
    "line", "dinner", "supper", "meal", "cook", "eat", "mak", "food",
];

/// Whole-word membership of `word` in the space-separated, already-normalised
/// `norm` (padded so boundaries hold at the ends).
fn norm_has_word(norm: &str, word: &str) -> bool {
    let padded = format!(" {norm} ");
    padded.contains(&format!(" {word} "))
}

/// Any normalised token in `norm` starts with `stem`.
fn norm_has_stem(norm: &str, stem: &str) -> bool {
    norm.split(' ').any(|t| t.starts_with(stem))
}

/// A single (already-normalised) sentence asserts that nothing is planned: it
/// carries BOTH a negation token AND a planning-status stem.
fn sentence_claims_empty(norm: &str) -> bool {
    let has_neg = WEEK_EMPTY_NEGATIONS.iter().any(|w| norm_has_word(norm, w));
    let has_plan = WEEK_PLAN_STEMS.iter().any(|s| norm_has_stem(norm, s));
    has_neg && has_plan
}

/// The terms a draft might use to refer to `weekday` (a lowercase full name):
/// the name itself plus "today"/"tonight"/"tomorrow" when `wc` maps them to it.
fn day_terms_for(wc: &WeekContext, weekday: &str) -> Vec<String> {
    let mut terms = vec![weekday.to_string()];
    if wc.today.as_deref() == Some(weekday) {
        terms.push("today".to_string());
        terms.push("tonight".to_string());
    }
    if wc.tomorrow.as_deref() == Some(weekday) {
        terms.push("tomorrow".to_string());
    }
    terms
}

/// Does `draft` already name the planned `dish`? A dish word of length ≥4 present
/// in the normalised draft means the reply is grounded on the real dish (so it is
/// NOT a false-empty claim, even if some other clause reads as a hedge). This is
/// the safety net that keeps the guard from clobbering a reply that DOES answer.
fn draft_mentions_dish(draft_norm: &str, dish: &str) -> bool {
    normalize(dish)
        .split(' ')
        .filter(|w| w.len() >= 4)
        .any(|w| norm_has_word(draft_norm, w))
}

/// Planned days a `draft` FALSELY claims are empty/unplanned. For each day that
/// has a dish in `wc`: if the draft does NOT already name the dish, and some
/// sentence both asserts nothing is planned AND references that day (by name, or
/// by today/tonight/tomorrow when they map to it), the day is a false-empty
/// claim. Returns `(Capitalised weekday, dish)` pairs in week order. Pure.
pub fn false_empty_week_claims(draft: &str, wc: &WeekContext) -> Vec<(String, String)> {
    if wc.is_empty() {
        return Vec::new();
    }
    let draft_norm = normalize(draft);
    let sentences: Vec<String> = draft
        .split(|c: char| matches!(c, '.' | '!' | '?' | '\n' | ';'))
        .map(normalize)
        .filter(|s| !s.is_empty())
        .collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for (weekday, dish) in &wc.by_day {
        if draft_mentions_dish(&draft_norm, dish) {
            continue;
        }
        let terms = day_terms_for(wc, weekday);
        let hit = sentences.iter().any(|s| {
            sentence_claims_empty(s) && terms.iter().any(|t| norm_has_word(s, t))
        });
        if hit {
            out.push((capitalize_weekday(weekday), dish.clone()));
        }
    }
    out.sort_by_key(|(day, _)| {
        weekday_token(&day.to_lowercase())
            .map(|w| w.num_days_from_monday())
            .unwrap_or(7)
    });
    out
}

/// Uppercase the first letter of a lowercase weekday name.
fn capitalize_weekday(day: &str) -> String {
    let mut chars = day.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The honest rewrite for a reply that falsely claimed a planned day was empty:
/// name the real dish for each falsely-claimed day. Mirrors the anti-fabrication
/// rewrite (a whole-reply replacement), but here we CAN state the truth — the
/// plan is right in front of us — rather than fall back to "I can't see it".
pub fn week_grounding_rewrite(days: &[(String, String)]) -> String {
    let parts: Vec<String> = days
        .iter()
        .map(|(day, dish)| format!("{day}'s dinner is {dish}"))
        .collect();
    let mut line = parts.join(", and ");
    if !line.ends_with('.') {
        line.push('.');
    }
    line
}

// ---------------------------------------------------------------------------
// FAMILY VOICE (task p1-engine-reply-guards) — the engine-side twin of the
// gateway's `claw3d-bridge/src/familyVoice.mjs` finalize gate.
//
// WHY THIS MUST LIVE HERE. The gateway applies six rules to a composed persona
// reply at ONE seam (`gateComposedReply`, called from `gatewayCore.routeChat` /
// `_finalizeGroupReply`): no self-attribution prefix, no off-roster human name,
// no hand-off tail, no infrastructure narration, no ops jargon, no markdown. But
// an ENGINE-originated reply never passes through that seam: `wg telegram listen`
// composes in-process, sends through its own `ReplySink`, and writes the feed via
// `FeedMirrorSink` in the ENGINE process. So every one of those six rules was
// unenforced on the real delivery path — exactly the gap the never-claim-empty
// week guard above was added for, one layer further out.
//
// This section is the Rust twin of those rules: pure functions plus one
// best-effort loader ([`FamilyVoice::load`]), applied at the engine's single
// delivery choke point (`telegram_conversation::deliver_reply`).
//
// TWIN, NOT TRANSLATION. Rust's `regex` has no look-around, so the pair-matching
// rules (markdown emphasis, the attribution prefix, the clause split) are
// hand-written scanners with the same contract rather than transliterated
// regexes. Two deliberate, documented differences from the JS twin are noted at
// their call sites ([`strip_handoff_tail`] keeps terminal punctuation;
// [`gate_family_voice`] never returns empty because Telegram rejects an empty
// send). Everything else is rule-for-rule identical, and the shipped guarantees
// are asserted in this module's tests.
//
// ROSTER-DRIVEN, NEVER HARDCODED (the no-hardcoded-names contract). The persona
// and human names both come from the household's own files — `household.toml`
// `[[agent]]`/`[household] members` and the confirmed Telegram bindings under
// `<workgraph_dir>/agency`. The only hardcoded list is [`RETIRED_PERSONAS`], and
// even those are cross-checked against the live roster so a name that is
// genuinely a member again is never scrubbed. The persona fallback used when no
// `household.toml` is found mirrors `ownership::OwnerMap::casa_default` (persona
// ids are product vocabulary, not a family's personal data).
// ---------------------------------------------------------------------------

/// Personas retired from the product. A retired name surfacing in a composed
/// reply is a tell that the composer invented a teammate; the gate strips it.
/// Lowercased, word-boundary matched — the Rust twin of `ledger.mjs`'s
/// `RETIRED_PERSONAS`.
pub const RETIRED_PERSONAS: &[&str] = &["nadin"];

/// 💬 — the speech-balloon relay-attribution glyph the gateway's group mirror
/// prefixes an agent line with. A composer that echoes the feed format it sees
/// in its context window bakes this into its own reply text; the attribution
/// belongs in the sender/avatar fields ONLY.
const CHAT_MARK: char = '\u{1F4AC}';

/// Honorific first-words that are NOT a name to match a hand-off / attribution
/// on, so "Coach Mira" contributes "mira" but never a bare "coach".
const HANDOFF_NAME_STOPWORDS: &[&str] = &["coach", "chef", "dr", "mr", "mrs", "ms", "the", "a", "an"];

/// Words that look like a capitalised name in an addressee slot but are NOT
/// people — so the roster-driven addressee scan never eats a weekday or a month.
const NOT_A_PERSON: &[&str] = &[
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "january",
    "february", "march", "april", "may", "june", "july", "august", "september", "october",
    "november", "december", "today", "tomorrow", "tonight", "yesterday", "everyone", "someone",
    "anyone", "i", "ill", "id", "ive", "im", "you", "youll", "youd", "we", "well", "us", "them",
    "him", "her", "it", "itll",
];

/// The live roster a composed reply is held to: which persona names may be
/// spoken as a voice (self-attribution + hand-off tails are matched against
/// these) and which human names may be NAMED at all.
///
/// Built from the household's own files by [`FamilyVoice::load`], or directly
/// from name lists by [`FamilyVoice::from_rosters`] for pure/unit use.
#[derive(Debug, Clone, Default)]
pub struct FamilyVoice {
    /// Persona display names + ids in author order (the hand-off / attribution
    /// alternation is built from these).
    persona_names: Vec<String>,
    /// Every allowed name token, lowercased: persona names/ids and their first
    /// words, plus every known human's name/id and first word.
    allowed: std::collections::HashSet<String>,
    /// Known-retired persona names to strip when they are not on the roster.
    retired: Vec<String>,
}

impl FamilyVoice {
    /// Build from explicit rosters: `personas` are persona display names and/or
    /// ids, `humans` are human display names and/or ids. Pure — this is the
    /// constructor the unit tests and any non-filesystem caller use.
    ///
    /// EVERY known human is an allowed name — confirmed AND pending alike (the
    /// JS twin's rule): a persona may legitimately name a still-pending invitee
    /// ("I've asked Pia to join and confirm"), so gating their name would garble
    /// an honest reply. Only a name in NO roster at all is scrubbed.
    pub fn from_rosters<S: AsRef<str>, T: AsRef<str>>(personas: &[S], humans: &[T]) -> Self {
        let mut persona_names = Vec::new();
        let mut allowed = std::collections::HashSet::new();
        let add = |set: &mut std::collections::HashSet<String>, raw: &str| {
            let s = raw.trim().to_lowercase();
            if s.is_empty() {
                return;
            }
            set.insert(s.clone());
            // First word too, so "Coach Mira" also allows "Mira".
            if let Some(first) = s.split_whitespace().next() {
                if !first.is_empty() {
                    set.insert(first.to_string());
                }
            }
        };
        for p in personas {
            let raw = p.as_ref().trim();
            if raw.is_empty() {
                continue;
            }
            persona_names.push(raw.to_string());
            add(&mut allowed, raw);
        }
        for h in humans {
            let raw = h.as_ref().trim();
            if raw.is_empty() {
                continue;
            }
            // A binding id ("human-luca") also allows the bare name ("luca").
            add(&mut allowed, raw);
            if let Some(rest) = raw.strip_prefix("human-") {
                add(&mut allowed, rest);
            }
        }
        Self {
            persona_names,
            allowed,
            retired: RETIRED_PERSONAS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Override the retired-persona list (tests; a household that renamed a
    /// persona out of the product).
    pub fn with_retired<S: AsRef<str>>(mut self, retired: &[S]) -> Self {
        self.retired = retired
            .iter()
            .map(|s| s.as_ref().trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    /// Load the live roster for a household: personas from `<root>/household.toml`
    /// `[[agent]]` (`name` + `id`, author order), humans from that file's
    /// `[household] members` PLUS every Telegram binding under
    /// `<workgraph_dir>/agency` (the same source `family_inviter_name` reads).
    ///
    /// Best-effort by design — the gate must never be the reason a reply fails to
    /// go out. A missing/unparseable `household.toml` falls back to the shipped
    /// persona ids (mirroring `ownership::OwnerMap::casa_default`); unreadable
    /// bindings simply contribute no human names.
    pub fn load(root: &Path, workgraph_dir: &Path) -> Self {
        let (mut personas, mut humans) = household_rosters(root).unwrap_or_default();
        if personas.is_empty() {
            personas = super::ownership::OwnerMap::casa_default()
                .persona_ids()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
        }
        if let Ok(map) =
            crate::agency::TelegramBindingMap::load(&workgraph_dir.join("agency"))
        {
            for b in &map.bindings {
                if !b.name.trim().is_empty() {
                    humans.push(b.name.trim().to_string());
                }
                if !b.agent_id.trim().is_empty() {
                    humans.push(b.agent_id.trim().to_string());
                }
            }
        }
        Self::from_rosters(&personas, &humans)
    }

    /// The persona display names + ids the hand-off / attribution guards match on.
    pub fn persona_names(&self) -> &[String] {
        &self.persona_names
    }

    /// True when `name` is on the live roster (persona or human), case-insensitive.
    pub fn allows(&self, name: &str) -> bool {
        self.allowed.contains(&name.trim().to_lowercase())
    }

    /// No roster at all — the name guards are then no-ops (a pure caller that
    /// supplied nothing must never have its reply mangled).
    pub fn is_empty(&self) -> bool {
        self.persona_names.is_empty() && self.allowed.is_empty()
    }
}

/// Parse `<root>/household.toml` into `(persona names+ids, human member names)`.
/// `None` when the file is absent or unparseable.
fn household_rosters(root: &Path) -> Option<(Vec<String>, Vec<String>)> {
    let body = std::fs::read_to_string(root.join("household.toml")).ok()?;
    let value: toml::Value = body.parse().ok()?;
    let mut personas = Vec::new();
    if let Some(agents) = value.get("agent").and_then(|a| a.as_array()) {
        for a in agents {
            if let Some(name) = a.get("name").and_then(|n| n.as_str()) {
                personas.push(name.to_string());
            }
            if let Some(id) = a.get("id").and_then(|i| i.as_str()) {
                personas.push(id.to_string());
            }
        }
    }
    let mut humans = Vec::new();
    if let Some(members) = value
        .get("household")
        .and_then(|h| h.get("members"))
        .and_then(|m| m.as_array())
    {
        for m in members {
            if let Some(s) = m.as_str() {
                humans.push(s.to_string());
            }
        }
    }
    Some((personas, humans))
}

// ── Rule 0: no self-attribution prefix ───────────────────────────────────────
// A persona reply carries its attribution in the sender/avatar fields ONLY —
// never baked into the visible text. The live bug (Luca, 2026-07-24): the kiosk
// greeting rendered as "The Chiller 💬 Hi! All quiet…" — the persona name AND the
// 💬 relay mark were inside the message text, so the row showed the avatar + name
// twice, once as chrome and once as words.
//
// Conservative by construction: a BARE persona name at the start is peeled ONLY
// when an attribution separator follows (💬 / ":" / an em/en dash), so an ordinary
// reply that merely opens with a name ("Nora says hi!") is untouched.

/// True when `c` reads as an attribution/avatar glyph: a non-alphanumeric,
/// non-whitespace, non-ASCII character (an emoji, a variation selector, a ZWJ).
/// Used instead of `\p{Extended_Pictographic}` (which Rust's `regex` gates behind
/// a Unicode table the crate does not enable) — the predicate is intentionally
/// broad because it is only ever applied to a LEADING or TRAILING run.
fn is_attr_glyph(c: char) -> bool {
    !c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace()
}

/// A colon / em-dash / en-dash: the separators that make a leading bare persona
/// name read as attribution rather than as the first word of a sentence.
fn is_strict_attr_sep(c: char) -> bool {
    matches!(c, ':' | '\u{FF1A}' | '\u{2014}' | '\u{2013}')
}

/// The strict separators plus a plain hyphen, allowed where the 💬 mark has
/// already established that the prefix IS attribution.
fn is_loose_attr_sep(c: char) -> bool {
    is_strict_attr_sep(c) || c == '-'
}

/// Lowercased persona name tokens (display names + their first words), longest
/// first so "coach mira" is tried before "mira", with the honorific stopwords
/// dropped so a bare "coach" never matches.
fn persona_name_tokens<S: AsRef<str>>(persona_names: &[S]) -> Vec<String> {
    let mut set = std::collections::HashSet::new();
    for n in persona_names {
        let s = n.as_ref().trim().to_lowercase();
        if s.chars().count() >= 2 {
            set.insert(s.clone());
        }
        // The first AND last words as aliases, so a household that configures only
        // a display name ("Coach Mira", "The Chiller") still has the bare name
        // ("mira", "chiller") to match a hand-off / attribution on. The honorific
        // stopwords ("coach", "the") never survive this, so a bare title can't
        // stand in for a persona.
        let mut words = s.split_whitespace();
        let first = words.next();
        let last = words.next_back().or(first);
        for alias in [first, last].into_iter().flatten() {
            if alias.chars().count() >= 2 && !HANDOFF_NAME_STOPWORDS.contains(&alias) {
                set.insert(alias.to_string());
            }
        }
    }
    let mut out: Vec<String> = set
        .into_iter()
        .filter(|s| !HANDOFF_NAME_STOPWORDS.contains(&s.as_str()))
        .collect();
    out.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
    out
}

/// Match one of `names` at `chars[i..]`, case-insensitively, requiring a
/// non-alphanumeric boundary after it (so "nora" does not match "Norah").
/// Returns the index just past the matched name.
fn match_name_at(chars: &[char], i: usize, names: &[String]) -> Option<usize> {
    for name in names {
        let nc: Vec<char> = name.chars().collect();
        if i + nc.len() > chars.len() {
            continue;
        }
        let hit = chars[i..i + nc.len()]
            .iter()
            .zip(nc.iter())
            .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()));
        if !hit {
            continue;
        }
        let end = i + nc.len();
        if end < chars.len() && chars[end].is_alphanumeric() {
            continue; // "Norah" is not "Nora"
        }
        return Some(end);
    }
    None
}

fn skip_ws(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

fn skip_glyphs(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && (is_attr_glyph(chars[i]) || chars[i].is_whitespace()) {
        i += 1;
    }
    i
}

/// Peel ONE leading self-attribution prefix, or `None` when the text does not
/// open with one. The four shapes are the JS twin's four alternatives:
///   a. `<glyph>* <Name> 💬`         — the reported shape
///   b. `💬 <Name>[sep]`             — the mark leads, then the name
///   c. `<glyph>* <Name><strict sep>` — a bare name attribution
///   d. `💬[sep]`                    — the bare relay mark
fn peel_self_attribution(text: &str, names: &[String]) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let start = skip_ws(&chars, 0);
    let rest_from = |i: usize| -> String { chars[skip_ws(&chars, i)..].iter().collect() };

    // (a) / (c): an optional glyph run, then the persona's own name.
    if !names.is_empty() {
        let after_glyphs = skip_glyphs(&chars, start);
        if let Some(end) = match_name_at(&chars, after_glyphs, names) {
            let k = skip_ws(&chars, end);
            if k < n && chars[k] == CHAT_MARK {
                return Some(rest_from(k + 1)); // (a) "<Name> 💬 …"
            }
            if k < n && is_strict_attr_sep(chars[k]) {
                return Some(rest_from(k + 1)); // (c) "<Name>: …"
            }
        }
    }

    // (b) / (d): the 💬 mark leads.
    if start < n && chars[start] == CHAT_MARK {
        let after_mark = skip_ws(&chars, start + 1);
        if !names.is_empty() {
            if let Some(end) = match_name_at(&chars, after_mark, names) {
                let mut k = skip_ws(&chars, end);
                if k < n && is_loose_attr_sep(chars[k]) {
                    k += 1;
                }
                return Some(rest_from(k)); // (b) "💬 <Name>: …"
            }
        }
        let mut k = after_mark;
        if k < n && is_loose_attr_sep(chars[k]) {
            k += 1;
        }
        return Some(rest_from(k)); // (d) "💬 …"
    }
    None
}

/// Strip a leading self-attribution prefix from a composed reply. Repeated
/// prefixes are peeled (bounded at three, so a line that is NOTHING but
/// attribution cannot loop); a strip that would empty the reply keeps the
/// original.
pub fn strip_self_attribution<S: AsRef<str>>(text: &str, persona_names: &[S]) -> String {
    let original = text.trim();
    if original.is_empty() {
        return text.to_string();
    }
    let names = persona_name_tokens(persona_names);
    let mut out = original.to_string();
    for _ in 0..3 {
        match peel_self_attribution(&out, &names) {
            Some(next) if next != out => out = next,
            _ => break,
        }
    }
    if out.trim().is_empty() {
        original.to_string()
    } else {
        out.trim().to_string()
    }
}

/// True when `text` opens with a self-attribution prefix — used by the tests and
/// the smoke gate to prove the guarantee holds (a stripped reply reports false).
pub fn has_self_attribution<S: AsRef<str>>(text: &str, persona_names: &[S]) -> bool {
    strip_self_attribution(text, persona_names) != text.trim()
}

// ── Rule 1: no off-roster human name ─────────────────────────────────────────
// A persona may only name a human who is actually in the live roster. The live
// bug: a retired test person ("Nadin") kept resurfacing in composed replies
// ("waiting on you and Nadin to confirm") long after every seed file was
// scrubbed, because freshly-composed LLM text can invent a teammate.

/// Verbs that introduce a person being HANDED a thing or ASKED to confirm — the
/// shapes where a phantom teammate leaks in. Kept to STRONG, unambiguous
/// hand-off/confirm verbs (not a bare "and"/"with"/"to") so ordinary speech is
/// never mistaken for a person. The captured group is the candidate name.
fn addressee_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:hand(?:ed| it)?(?:\s+(?:it|over))?\s+to|pass(?:\s+it)?\s+to|give\s+(?:it\s+)?to|check\s+with|confirm\s+with|waiting\s+(?:on|for))\s+([A-Z][A-Za-zÀ-ÿ'’-]{2,})\b",
        )
        .expect("addressee regex compiles")
    })
}

/// Capitalised addressee names in `text` that are NOT on the roster and don't
/// look like a date word — the phantom people to strip.
pub fn non_roster_addressees(text: &str, voice: &FamilyVoice) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for caps in addressee_re().captures_iter(text) {
        let name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        // The candidate may arrive possessive ("waiting on Tomorrow's list",
        // "hand it to Priya's team"). Test the roster against the BASE name so a
        // roster member / a date word is recognised through the "'s"; strip the
        // whole possessive token when it turns out to be a phantom, so no
        // dangling "'s" is left behind.
        let base = name
            .strip_suffix("'s")
            .or_else(|| name.strip_suffix('\u{2019}'))
            .or_else(|| name.strip_suffix("\u{2019}s"))
            .unwrap_or(name)
            .trim_end_matches(['\'', '\u{2019}']);
        let key = base.to_lowercase();
        if voice.allows(&key) || NOT_A_PERSON.contains(&key.as_str()) {
            continue;
        }
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// The full strip list for a composed reply: the known-retired personas that are
/// NOT currently on the roster and DO appear in this text, plus any capitalised
/// off-roster addressee the text itself names.
pub fn non_roster_ghosts(text: &str, voice: &FamilyVoice) -> Vec<String> {
    let mut ghosts: Vec<String> = Vec::new();
    for r in &voice.retired {
        if voice.allows(r) {
            continue; // genuinely back on the roster
        }
        if word_present(text, r) && !ghosts.iter().any(|g| g.eq_ignore_ascii_case(r)) {
            ghosts.push(r.clone());
        }
    }
    for n in non_roster_addressees(text, voice) {
        if !ghosts.iter().any(|g| g.eq_ignore_ascii_case(&n)) {
            ghosts.push(n);
        }
    }
    ghosts
}

/// True when a composed reply still names a human who is not on the roster —
/// the assertion the tests and the smoke gate make.
pub fn mentions_non_roster(text: &str, voice: &FamilyVoice) -> bool {
    !non_roster_ghosts(text, voice).is_empty()
}

/// Case-insensitive whole-word presence of `needle` in `haystack`.
fn word_present(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let h: Vec<char> = haystack.chars().collect();
    let nd: Vec<char> = needle.chars().collect();
    let mut i = 0;
    while i + nd.len() <= h.len() {
        let hit = h[i..i + nd.len()]
            .iter()
            .zip(nd.iter())
            .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()));
        if hit {
            let left_ok = i == 0 || !h[i - 1].is_alphanumeric();
            let right = i + nd.len();
            let right_ok = right >= h.len() || !h[right].is_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Remove ghost human names from free text, then tidy the dangling connectors
/// they leave behind — the Rust twin of `weekSource.scrubGhostNames`. Driven
/// ENTIRELY by the caller's list; no name is hardcoded here.
pub fn scrub_ghost_names<S: AsRef<str>>(text: &str, ghosts: &[S]) -> String {
    let mut s = text.to_string();
    if ghosts.is_empty() {
        return s;
    }
    for g in ghosts {
        let name = g.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        let esc = regex::escape(name);
        // "w/ Nadin", "with Nadin", "for Nadin", "to Nadin", "and Nadin". The
        // locatives ("on"/"from"/"about") go beyond the JS twin's list so
        // "waiting on Nadin" collapses to "waiting" rather than "waiting on".
        s = replace_all(
            &s,
            &format!(r"(?i)\s*\b(?:w/|with|for|to|by|and|or|on|from|about)\s+{esc}\b"),
            "",
        );
        // "Nadin —" (owner prefix) and "— Nadin" (suffix)
        s = replace_all(&s, &format!(r"(?i)\b{esc}\s*[—-]\s*"), "");
        s = replace_all(&s, &format!(r"(?i)\s*[—-]\s*\b{esc}\b"), "");
        // any remaining standalone mention
        s = replace_all(&s, &format!(r"(?i)\b{esc}\b"), "");
    }
    let s = replace_all(&s, r"\(\s*\)", ""); // emptied "(…)"
    let s = replace_all(&s, r"\s+([.,;:])", "$1"); // space before punctuation
    let s = replace_all(&s, r"\s{2,}", " "); // collapsed doubles
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '—' | ',' | ';' | ':' | '-'))
        .to_string()
}

/// Compile-and-replace helper: a pattern that fails to compile leaves the text
/// untouched, so the gate can never panic on a household-derived name.
fn replace_all(text: &str, pattern: &str, replacement: &str) -> String {
    match regex::Regex::new(pattern) {
        Ok(re) => re.replace_all(text, replacement).into_owned(),
        Err(_) => text.to_string(),
    }
}

// ── Rule 2: no hand-off tail ─────────────────────────────────────────────────
// A DELIVERED answer ends after its content. It must not trail off by handing
// the turn to another persona — the live 19:2x leak was a Nora delivery that
// ended "🦆 Otto's got this one". The family asked ONE assistant and got the
// answer; a hand-off tail reads as buck-passing.

/// Hand-off constructions as templates, `__N__` where a roster persona name
/// goes. Each is checked only where it runs to the END of the reply, so a
/// mid-sentence mention ("I asked Nora and she's on it, plus the plan's set") is
/// never over-stripped.
const HANDOFF_TEMPLATES: &[&str] = &[
    // "Otto's got this (one)", "Nora has got it"
    r"(?:__N__)(?:'s|’s| is| has)?\s+got\s+(?:this|it|that)(?:\s+one)?",
    // "hand/pass/kick/send (this) (off) (over) to Otto"
    r"(?:hand(?:ing)?|pass(?:ing)?|kick(?:ing)?|send(?:ing)?|toss(?:ing)?|leav(?:e|ing))\s+(?:this|it|that)?\s*(?:one\s+)?(?:off\s+)?(?:over\s+)?to\s+(?:__N__)",
    // "over to Otto"
    r"over\s+to\s+(?:__N__)",
    // "Otto can/will/'ll/should take/grab/handle/pick up/cover it (from here)"
    r"(?:__N__)\s+(?:can|could|will|'ll|’ll| ll|should|is\s+gonna|is\s+going\s+to)\s+(?:take|grab|handle|pick\s+up|sort|cover|help\s+with|run\s+with|jump\s+on)\s+(?:it|this|that)(?:\s+(?:one|up|out))?(?:\s+from\s+here)?",
    // "Otto's on it/this/that (one)"
    r"(?:__N__)(?:'s|’s| is)\s+(?:on|got)\s+(?:it|this|that)(?:\s+one)?",
    // "let Otto take/handle/grab it"
    r"let\s+(?:__N__)\s+(?:take|handle|grab|sort|cover|run\s+with)\s+(?:it|this|that)",
    // "Otto'll pick this up", "Otto will pick it up"
    r"(?:__N__)(?:'ll|’ll| will)\s+pick\s+(?:this|it|that)\s+up",
    // "Otto's your/the person/go-to for this"
    r"(?:__N__)(?:'s|’s| is)\s+(?:your|the)\s+(?:go[- ]?to|person|one)\s+for\s+(?:this|that|it)",
];

/// The alternation of roster persona names (+ first-word aliases), regex-escaped
/// and longest-first. `None` when the roster yields no usable name — the tail
/// guard is then a no-op (a pure caller's reply is never mangled).
fn persona_alternation<S: AsRef<str>>(persona_names: &[S]) -> Option<String> {
    let tokens = persona_name_tokens(persona_names);
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|t| regex::escape(t))
            .collect::<Vec<_>>()
            .join("|"),
    )
}

/// True when everything after a hand-off clause is only trailing slack —
/// punctuation, whitespace, an orphaned avatar emoji. This replaces the JS
/// twin's `$`-anchored `[\p{Extended_Pictographic}…]*$` tail: a hand-off is a
/// TAIL only when no further words follow it.
fn is_tail_slack(rest: &str) -> bool {
    rest.chars().all(|c| !c.is_alphanumeric())
}

/// The byte index where a trailing hand-off begins, or `None` when there is none.
fn handoff_cut_index(text: &str, alt: &str) -> Option<usize> {
    let mut cut: Option<usize> = None;
    for tpl in HANDOFF_TEMPLATES {
        let pat = format!("(?i)(?:{})", tpl.replace("__N__", alt));
        let re = match regex::Regex::new(&pat) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for m in re.find_iter(text) {
            if is_tail_slack(&text[m.end()..]) {
                if cut.is_none_or(|c| m.start() < c) {
                    cut = Some(m.start());
                }
                break; // find_iter is left-to-right: this is the earliest tail hit
            }
        }
    }
    cut
}

/// Strip a trailing hand-off to another persona, roster-driven. A reply with no
/// hand-off tail is returned unchanged; a reply that is NOTHING but a hand-off
/// keeps its original text (never emptied).
///
/// DELIBERATE DIFFERENCE from the JS twin: the connector tidy-up keeps terminal
/// punctuation, so "Dinner's set. Otto's got this one." leaves "Dinner's set."
/// with its period rather than the JS twin's bare "Dinner's set". Same rule, one
/// less rough edge in the family's reading.
pub fn strip_handoff_tail<S: AsRef<str>>(text: &str, persona_names: &[S]) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let alt = match persona_alternation(persona_names) {
        Some(a) => a,
        None => return text.to_string(),
    };
    let cut = match handoff_cut_index(text, &alt) {
        Some(c) => c,
        None => return text.to_string(),
    };
    let head = trim_handoff_head(&text[..cut]);
    if head.trim().is_empty() {
        text.trim().to_string()
    } else {
        head
    }
}

/// Subject/auxiliary fragments a removed hand-off clause leaves dangling: "…
/// Tuesday's set. I'll hand it over to Bruno." cuts at "hand", stranding a bare
/// "I'll". Stripped only while the head does NOT already end on a complete
/// sentence, so ordinary words are never eaten.
const DANGLING_TAIL_WORDS: &[&str] = &["and", "but", "so", "then", "plus", "also", "i", "i'll", "we", "we'll"];

/// Tidy the text left in front of a removed hand-off tail: the orphaned avatar
/// emoji, the connector the tail hung off, and any dangling subject/auxiliary.
/// Terminal punctuation is KEPT (the one deliberate difference from the JS twin),
/// and it also acts as the stop condition: once the head reads as a finished
/// sentence, nothing more is trimmed.
fn trim_handoff_head(head: &str) -> String {
    let mut s = head.to_string();
    for _ in 0..4 {
        let t = s
            // an orphaned avatar emoji the tail hung off
            .trim_end_matches(|c: char| is_attr_glyph(c) || c.is_whitespace())
            // the dangling connector it hung off (terminal punctuation is KEPT)
            .trim_end_matches(|c: char| {
                matches!(c, '—' | '–' | '-' | ',' | ';' | ':') || c.is_whitespace()
            })
            .trim_end();
        if t.ends_with(['.', '!', '?', '\u{2026}']) {
            return t.to_string();
        }
        let Some(last) = t.split_whitespace().next_back() else {
            return t.to_string();
        };
        let key: String = last
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'' || *c == '\u{2019}')
            .collect::<String>()
            .to_lowercase()
            .replace('\u{2019}', "'");
        if !DANGLING_TAIL_WORDS.contains(&key.as_str()) {
            return t.to_string();
        }
        s = t[..t.len() - last.len()].to_string();
    }
    s.trim_end().to_string()
}

/// True when `text` ends on a hand-off to a roster persona.
pub fn has_handoff_tail<S: AsRef<str>>(text: &str, persona_names: &[S]) -> bool {
    match persona_alternation(persona_names) {
        Some(alt) => handoff_cut_index(text, &alt).is_some(),
        None => false,
    }
}

// ── Rule 3: no infrastructure narration ──────────────────────────────────────
// A composed reply must NEVER narrate HOW it reaches data. The family hears a
// person, not a program describing its own plumbing: the live 19:2x leak was
// "I'd need to pull from what's actually in the system… Want me to grab that from
// the live gateway so you get the real take?".
//
// CAREFUL WITH "system": it is an ordinary family word ("a good bedtime system",
// "our chore system"). So only the INFRA COLLOCATIONS match — a data-access verb
// or a locative bound to "the system"/a gateway/pipeline/backend/database —
// never "system" standing alone. Tested BOTH directions.
const INFRA_NOUN: &str = r"(?:gateway|pipeline|back[\s-]?end|database|datastore|data\s*store|data\s*feed|the\s+api|(?:the|our|your|this|that)\s+system)";

fn infra_signals() -> &'static Vec<regex::Regex> {
    static RES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    RES.get_or_init(|| {
        let pats = [
            // A data-access verb bound to an infra store: "pull from the system",
            // "grab that from the live gateway", "query the backend".
            format!(
                r"(?i)\b(?:pull|pulling|grab|grabbing|fetch|fetching|query|querying|load|loading|scrape|scraping|sync|syncing|hit|hitting|poll|polling|retrieve|retrieving|read)\b[^.!?]{{0,40}}\b(?:from|off|out\s+of|into|against|to|up|via)\b[^.!?]{{0,24}}{INFRA_NOUN}"
            ),
            // A locative binding an infra store: "in the system", "on the backend".
            format!(
                r"(?i)\b(?:in|from|on|via|through|inside|within|across|over\s+(?:in|on|at))\s+(?:the\s+|our\s+|a\s+|this\s+|that\s+|live\s+)*{INFRA_NOUN}"
            ),
            // A data-fetch verb DIRECTLY on an infra store, no preposition.
            format!(
                r"(?i)\b(?:query|querying|hit|hitting|poll|polling|ping|pinging|scrape|scraping|fetch|fetching|pull|pulling)\b[^.!?]{{0,12}}{INFRA_NOUN}"
            ),
            // Strong bare infra references that are ~never benign in family chat.
            r"(?i)\bthe\s+live\s+gateway\b".to_string(),
            r"(?i)\bdata\s+pipeline\b".to_string(),
        ];
        pats.iter()
            .map(|p| regex::Regex::new(p).expect("infra signal compiles"))
            .collect()
    })
}

/// True if the text narrates data-access infrastructure.
pub fn has_infra_narration(text: &str) -> bool {
    infra_signals().iter().any(|re| re.is_match(text))
}

/// Drop the clauses that narrate infrastructure while preserving the surrounding
/// family voice. Clean lines are returned unchanged via the fast path. A line
/// that was ENTIRELY infra narration scrubs to "" — the caller then substitutes
/// [`infra_fallback_line`] rather than sending nothing.
pub fn scrub_infra_narration(text: &str) -> String {
    if text.trim().is_empty() || !has_infra_narration(text) {
        return text.to_string();
    }
    let clauses = split_clauses(text, &['.', '!', '?', '\u{2026}'], &['-', '•', '·', '—', '–']);
    let kept: Vec<String> = clauses
        .into_iter()
        .filter(|c| !has_infra_narration(c))
        .collect();
    tidy_after_clause_drop(&kept.join(" "))
}

/// The warm, family-voice offer used when a reply was ENTIRELY infrastructure
/// narration. It answers like a person — offers to do the real work — and names
/// no plumbing.
pub fn infra_fallback_line(sender: &str) -> String {
    let who = sender.trim();
    let tail = if who.is_empty() {
        String::new()
    } else {
        format!(", {who}")
    };
    format!("Happy to dig into that{tail} — want me to take a proper look and get you the details?")
}

// ── Rule 4: no ops / orchestration jargon ────────────────────────────────────
// A persona's "status"/"orient" reply can arrive full of developer telemetry — a
// dispatcher line, worker/task-status counts, the model executor name, cron
// schedules, PIDs, raw agent ids. That is console output, not something a family
// writes or reads.

fn ops_signals() -> &'static Vec<regex::Regex> {
    static RES: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    RES.get_or_init(|| {
        [
            r"(?i)\bagent-\d+\b",
            r"(?i)\bdispatcher\b",
            r"(?i)\bclaude[:/][a-z0-9._-]+",
            r"(?i)\bexecutor\b",
            r"(?i)\b(?:max\s+\d+\s+agents?|\d+\s+max\s+(?:agents?|workers?)|max\s+workers?)\b",
            r"(?i)\b\d+\s+agents?\b",
            r"(?i)\b\d+\s+alive\b",
            r"(?i)\b(?:in-progress|in progress)\b",
            r"(?i)\bcron\b",
            r"(?i)\bnext\s+fire\b",
            r"(?i)\buptime\b",
            r"(?i)\bPID\s*\d+",
            r"(?i)\bopenrouter\b",
            r"(?i)\bregistry\s+refresh\b",
            r"(?i)\bdaemon\b",
            r"(?i)\bwg\s+\w+",
            // Task-status tally — a count DIRECTLY on a scheduler word. Kept TIGHT
            // so ordinary family speech ("3 bags ready to go") never trips.
            r"(?i)\b\d+\s+(?:recurring|paused|blocked)\b",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("ops signal compiles"))
        .collect()
    })
}

/// True if the text carries any orchestration-jargon signal.
pub fn has_ops_jargon(text: &str) -> bool {
    ops_signals().iter().any(|re| re.is_match(text))
}

/// Humanize machine week references to family voice: "W29" / "2026-W29" → "next
/// week". A TOKEN-level rewrite (not a clause drop) because a week number sits
/// INSIDE an otherwise-family sentence — the live 2026-07-16 leak ("W29`s still
/// sitting as a draft") is exactly this shape. Idempotent and safe on clean text.
pub fn humanize_week_refs(text: &str) -> String {
    static ISO: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BARE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let iso = ISO.get_or_init(|| regex::Regex::new(r"(?i)\b\d{4}-W\d{1,2}\b").unwrap());
    let bare = BARE.get_or_init(|| regex::Regex::new(r"\bW\d{1,2}\b").unwrap());
    let out = iso.replace_all(text, "next week").into_owned();
    bare.replace_all(&out, "next week").into_owned()
}

/// Strip orchestration telemetry from a human-facing line while preserving the
/// surrounding family voice. Clean messages are returned byte-for-byte via the
/// fast path. When jargon IS present the line is split into clauses, any clause
/// carrying a signal is dropped whole, and the leftover greeting/closing is
/// tidied. A line that was ENTIRELY telemetry scrubs to "".
pub fn scrub_ops_jargon(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    // Humanize machine week refs FIRST — before the fast-path check, so a line
    // whose only jargon is "W29" is still cleaned.
    let weeked = humanize_week_refs(text);
    if !has_ops_jargon(&weeked) {
        return weeked; // fast path: no telemetry clauses to drop
    }
    let clauses = split_clauses(&weeked, &['.', '!', '?'], &['-', '•', '·']);
    let kept: Vec<String> = clauses.into_iter().filter(|c| !has_ops_jargon(c)).collect();
    tidy_after_clause_drop(&kept.join(" "))
}

/// Split a line into clauses on sentence ends and markdown-bullet boundaries.
/// The Rust twin of the JS twin's look-behind split — the feed collapses newlines
/// to spaces, so a status dump arrives as one long "**Label:** …, … — …" line.
fn split_clauses(text: &str, enders: &[char], bullets: &[char]) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        // A sentence ender followed by whitespace ends the clause.
        if enders.contains(&c) {
            cur.push(c);
            i += 1;
            if i < n && chars[i].is_whitespace() {
                i = skip_ws(&chars, i);
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            continue;
        }
        // " - " / " • " / " · " — a bullet item boundary.
        if c.is_whitespace() {
            let j = skip_ws(&chars, i);
            if j < n && bullets.contains(&chars[j]) && j + 1 < n && chars[j + 1].is_whitespace() {
                out.push(cur.trim().to_string());
                cur = String::new();
                i = skip_ws(&chars, j + 1);
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur.trim().to_string());
    out.into_iter()
        .map(|c| {
            // A leading bullet + space the split left behind.
            let t = c.trim();
            let mut ch = t.chars();
            match ch.next() {
                Some(first) if bullets.contains(&first) => {
                    let rest = ch.as_str();
                    if rest.starts_with(char::is_whitespace) {
                        rest.trim().to_string()
                    } else {
                        t.to_string()
                    }
                }
                _ => t.to_string(),
            }
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Tidy the leftover after clauses were dropped: the dangling markdown and
/// label-colons a removed clause leaves behind.
fn tidy_after_clause_drop(joined: &str) -> String {
    let s = joined.replace("**", "").replace('`', "");
    let s = replace_all(&s, r"\s+([.,!?;:])", "$1"); // no space before punctuation
    let s = replace_all(&s, r":\s+([A-Z])", ". $1"); // dangling "…right now:" → break
    let s = replace_all(&s, r"[ \t]{2,}", " ");
    let s = s.trim().to_string();
    // A dangling label colon / connector at the very end → a clean period.
    replace_all(&s, r"[:—–-]\s*$", ".").trim().to_string()
}

// ── Rule 5: plain-text surfaces ──────────────────────────────────────────────
// The conversation pane and the Telegram relay render PLAIN TEXT — a markdown
// marker shows up as a literal asterisk/backtick (live: Nora's "**180–220
// calories**" arrived with the stars visible). Strip the emphasis PAIRS to their
// words; a lone or arithmetic asterisk is not emphasis and must survive
// ("5*7" stays "5*7").

/// Strip markdown emphasis pairs to their words: `**bold**` / `*italic*` /
/// `` `code` `` → the words, markers dropped. EMOJI and ordinary punctuation are
/// untouched, and a marker that is not part of a well-formed emphasis pair
/// survives verbatim.
pub fn strip_markdown(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    // Bold before italic (so "**x**" is not seen as two italic markers), then code.
    let out = strip_emphasis_pairs(text, '*', 2, false);
    let out = strip_emphasis_pairs(&out, '*', 1, true);
    strip_emphasis_pairs(&out, '`', 1, false)
}

/// True when `text` carries a strippable markdown emphasis pair.
pub fn has_markdown(text: &str) -> bool {
    strip_markdown(text) != text
}

/// Strip `marker`-run pairs of exactly `count` markers around a non-empty body
/// that holds no marker and no newline and neither opens nor closes on
/// whitespace. When `boundary` is set the opener must not be preceded — and the
/// closer must not be followed — by a word character, which is what keeps
/// arithmetic ("2*3*4") and snake_case intact.
fn strip_emphasis_pairs(text: &str, marker: char, count: usize, boundary: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < n {
        let opener = i + count <= n
            && chars[i..i + count].iter().all(|&c| c == marker)
            && (i + count >= n || chars[i + count] != marker)
            && (!boundary || i == 0 || (!is_word(chars[i - 1]) && chars[i - 1] != marker));
        if opener {
            // Scan for the closing run: the body may hold neither the marker nor
            // a newline (mirrors the JS twin's `[^*\n]+?`).
            let body_start = i + count;
            let mut j = body_start;
            let mut closer = None;
            while j < n {
                if chars[j] == '\n' {
                    break;
                }
                if chars[j] == marker {
                    let mut k = j;
                    while k < n && chars[k] == marker {
                        k += 1;
                    }
                    if k - j == count {
                        closer = Some(j);
                    }
                    break;
                }
                j += 1;
            }
            if let Some(close) = closer {
                let body: String = chars[body_start..close].iter().collect();
                let after = close + count;
                let right_ok = !boundary
                    || after >= n
                    || (!is_word(chars[after]) && chars[after] != marker);
                if !body.is_empty()
                    && !body.starts_with(|c: char| c.is_whitespace())
                    && !body.ends_with(|c: char| c.is_whitespace())
                    && right_ok
                {
                    out.push_str(&body);
                    i = after;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ── THE GATE ─────────────────────────────────────────────────────────────────

/// THE engine-side family-voice gate: all six rules, in the JS twin's order, on
/// an engine-composed reply about to reach a person. Applied at the engine's one
/// delivery choke point (`telegram_conversation::deliver_reply`) so the Telegram
/// send, the ack edit, and the `FeedMirrorSink` feed line all inherit it.
///
/// DELIBERATE DIFFERENCE from the JS twin: `gateComposedReply` may return "" for
/// a reply that was NOTHING but telemetry (the gateway then records/mirrors
/// nothing). The engine cannot — Telegram rejects an empty send and a silent
/// engine reads as a dead assistant — so a reply the gate empties becomes the
/// honest [`infra_fallback_line`] offer rather than nothing OR the raw jargon.
pub fn gate_family_voice(text: &str, voice: &FamilyVoice) -> String {
    gate_family_voice_with(text, voice, GateOptions::default())
}

/// Per-reply exemptions the gate must honour.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateOptions<'a> {
    /// A hand-off tail the ENGINE ITSELF appended — the single-owner defer line
    /// (`ownership::defer_line`, "Nora's got this one 🥗") that an off-domain
    /// voice adds so a re-routed ask visibly lands with its owner.
    ///
    /// WHY AN EXEMPTION EXISTS. Rule 2 targets a tail the COMPOSER invented
    /// INSTEAD of answering (the live 19:2x leak: Nora delivered the answer, then
    /// tacked on "🦆 Otto's got this one" — buck-passing). The defer line is the
    /// opposite: it is ownership routing made visible, authored by the engine
    /// after the single-owner rule re-routed the task, and stripping it would
    /// leave the family with no idea where their ask went. Only an EXACT suffix
    /// match is honoured, so this cannot be used to smuggle a composer tail
    /// through. Composer-invented tails ELSEWHERE in the same reply are still cut.
    pub authorized_handoff: Option<&'a str>,
}

/// [`gate_family_voice`] with per-reply exemptions ([`GateOptions`]).
pub fn gate_family_voice_with(text: &str, voice: &FamilyVoice, opts: GateOptions<'_>) -> String {
    let original = text.trim();
    if original.is_empty() {
        return text.to_string();
    }
    // An engine-authored defer tail is split off, the reply BODY is gated, and the
    // tail is re-attached verbatim — so the body still loses any composer-invented
    // hand-off while the ownership notice survives.
    if let Some(tail) = opts.authorized_handoff.map(str::trim).filter(|t| !t.is_empty()) {
        if let Some(head) = original.strip_suffix(tail) {
            let head = head.trim();
            if head.is_empty() {
                return tail.to_string();
            }
            let gated_head = gate_family_voice_with(
                head,
                voice,
                GateOptions {
                    authorized_handoff: None,
                },
            );
            return format!("{gated_head}\n\n{tail}");
        }
    }
    let personas = voice.persona_names();

    // 0. NO SELF-ATTRIBUTION PREFIX — peel it FIRST so every gate below (and the
    //    pane / Telegram render) sees de-attributed words.
    let mut out = strip_self_attribution(original, personas);

    // 1. NO OFF-ROSTER HUMAN NAME. If scrubbing would empty the message (a reply
    //    ENTIRELY about a phantom person), keep the de-attributed text.
    let ghosts = non_roster_ghosts(&out, voice);
    if !ghosts.is_empty() {
        let scrubbed = scrub_ghost_names(&out, &ghosts);
        if !scrubbed.trim().is_empty() {
            out = scrubbed;
        }
    }

    // 2. NO HAND-OFF TAIL — a delivered answer ends after its content.
    let tailless = strip_handoff_tail(&out, personas);
    if !tailless.trim().is_empty() {
        out = tailless;
    }

    // 3. NO INFRASTRUCTURE NARRATION. When dropping the offending clauses empties
    //    the reply (it was ENTIRELY fetch-narration, the live 19:2x leak),
    //    substitute a warm family-voice offer rather than leaking the plumbing.
    if has_infra_narration(&out) {
        let de_infra = scrub_infra_narration(&out);
        out = if de_infra.trim().is_empty() {
            infra_fallback_line("")
        } else {
            de_infra
        };
    }

    // 4. NO MACHINE JARGON, then 5. PLAIN TEXT (markdown last, so both the
    //    jargon tidy-up and the emphasis strip land on the final words).
    let gated = strip_markdown(scrub_ops_jargon(&out).trim());
    if gated.trim().is_empty() {
        // The whole reply was telemetry. Say something honest, never nothing and
        // never the raw console dump.
        return infra_fallback_line("");
    }
    gated
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

    // -----------------------------------------------------------------------
    // WEEK CONTEXT / never-claim-empty guard (task week-grounding-engine)
    // -----------------------------------------------------------------------

    /// The gateway's `WG_WEEK_CONTEXT` text (weekSource.buildWeekContext shape).
    fn sample_week_context() -> String {
        "This week's dinners, parsed from the family plan's Dinners table:\n\
         - Friday (July 24): Chicken tray bake\n\
         - Saturday (July 25): Baked white fish with tomato, olives & capers\n\
         - Sunday (July 26): not planned yet\n\
         Today is Friday — dinner: Chicken tray bake.\n\
         Tomorrow is Saturday — dinner: Baked white fish with tomato, olives & capers."
            .to_string()
    }

    #[test]
    fn parse_week_context_records_only_planned_days() {
        let wc = parse_week_context(&sample_week_context());
        assert!(!wc.is_empty());
        assert_eq!(wc.by_day.get("friday").map(String::as_str), Some("Chicken tray bake"));
        assert_eq!(
            wc.by_day.get("saturday").map(String::as_str),
            Some("Baked white fish with tomato, olives & capers")
        );
        // "not planned yet" is NOT a planned day.
        assert!(!wc.by_day.contains_key("sunday"));
        assert_eq!(wc.today.as_deref(), Some("friday"));
        assert_eq!(wc.tomorrow.as_deref(), Some("saturday"));
    }

    /// THE LIVE NORA BUG: "what's for dinner tomorrow?" → "Nothing's locked in for
    /// Saturday yet." while the Dinners table HAS a dish for Saturday. The guard
    /// must catch the false-empty claim and rewrite it to the honest dish.
    #[test]
    fn never_claim_empty_rewrites_false_empty_for_a_planned_day() {
        let wc = parse_week_context(&sample_week_context());
        let draft = "Nothing's locked in for Saturday yet.";
        let claims = false_empty_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "expected one false-empty claim, got {claims:?}");
        assert_eq!(claims[0].0, "Saturday");
        let rewritten = week_grounding_rewrite(&claims);
        assert!(
            rewritten.contains("Baked white fish with tomato, olives & capers"),
            "rewrite must name the real dish:\n{rewritten}"
        );
        assert!(rewritten.starts_with("Saturday's dinner is"), "{rewritten}");
    }

    /// A relative-day empty-claim ("nothing planned for tomorrow") is caught too,
    /// because the context maps tomorrow → Saturday, which is planned.
    #[test]
    fn never_claim_empty_resolves_tomorrow_to_the_planned_weekday() {
        let wc = parse_week_context(&sample_week_context());
        let claims = false_empty_week_claims("We haven't planned dinner for tomorrow.", &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].0, "Saturday");
    }

    /// A reply that ALREADY names the dish is grounded — never rewritten, even if
    /// a nearby clause reads like a hedge.
    #[test]
    fn never_claim_empty_leaves_a_grounded_reply_alone() {
        let wc = parse_week_context(&sample_week_context());
        let draft = "Saturday's dinner is baked white fish — nothing else is locked in yet though.";
        assert!(
            false_empty_week_claims(draft, &wc).is_empty(),
            "a reply that names the dish must not be flagged"
        );
    }

    /// A genuinely unplanned day (Sunday, "not planned yet") is NOT a false claim
    /// — saying it's open is honest, so the guard must leave it alone.
    #[test]
    fn never_claim_empty_allows_an_honestly_empty_day() {
        let wc = parse_week_context(&sample_week_context());
        assert!(
            false_empty_week_claims("Nothing's planned for Sunday yet.", &wc).is_empty(),
            "an honestly-empty day must not be rewritten"
        );
    }

    /// No forwarded context (or an empty one) → the guard and the prompt block are
    /// both no-ops (the Telegram-listener path).
    #[test]
    fn week_context_absent_is_a_noop() {
        let empty = parse_week_context("");
        assert!(empty.is_empty());
        assert!(false_empty_week_claims("Nothing's locked in for Saturday yet.", &empty).is_empty());
        assert!(week_context_block("").is_none());
        assert!(week_context_block("   ").is_none());
    }

    /// The injected prompt block carries the table AND the NEVER-claim-empty
    /// instruction (the prompt-injection half of the fix).
    #[test]
    fn week_context_block_carries_table_and_instruction() {
        let block = week_context_block(&sample_week_context()).expect("block present");
        assert!(block.contains("Baked white fish with tomato, olives & capers"));
        let lower = block.to_lowercase();
        assert!(lower.contains("never say it is empty"), "{block}");
        assert!(lower.contains("from this table"), "{block}");
    }

    // -----------------------------------------------------------------------
    // FAMILY VOICE (task p1-engine-reply-guards) — the six gateway finalize
    // rules, now enforced engine-side on the real delivery path. Each rule is
    // asserted in BOTH directions: the leak is caught, and ordinary family
    // speech that merely resembles it is left alone (the false-positive half is
    // what makes a gate safe to put in front of every send).
    // -----------------------------------------------------------------------

    /// A live-shaped roster: five personas (one with an honorific, one
    /// two-worder) and two humans. Roster-driven throughout — no name in the
    /// guards themselves.
    fn voice() -> FamilyVoice {
        FamilyVoice::from_rosters(
            &["Nora", "nora", "Bruno", "bruno", "Coach Mira", "mira", "Otto", "otto", "The Chiller", "chiller"],
            &["Luca", "human-luca", "Erik"],
        )
    }

    fn personas() -> Vec<String> {
        voice().persona_names().to_vec()
    }

    // ── Rule 0: self-attribution ───────────────────────────────────────────

    /// THE reported bug (Luca, 2026-07-24): the kiosk greeting rendered as
    /// "The Chiller 💬 Hi! All quiet…" — the persona name AND the relay mark
    /// baked into the words, so the row showed the avatar + name twice.
    #[test]
    fn self_attribution_prefix_is_peeled_from_the_text() {
        let p = personas();
        let out = strip_self_attribution("The Chiller \u{1F4AC} Hi! All quiet here.", &p);
        assert_eq!(out, "Hi! All quiet here.");
        assert_eq!(strip_self_attribution("\u{1F4AC} Nora: dinner's at seven.", &p), "dinner's at seven.");
        assert_eq!(strip_self_attribution("Otto — the plan's up.", &p), "the plan's up.");
        assert_eq!(strip_self_attribution("\u{1F957} Nora: pasta tonight.", &p), "pasta tonight.");
        assert_eq!(strip_self_attribution("\u{1F4AC} we're all set.", &p), "we're all set.");
        // Stacked attribution unwinds.
        assert_eq!(
            strip_self_attribution("\u{1F4AC} Otto: Otto: the plan's up.", &p),
            "the plan's up."
        );
    }

    /// A reply that merely OPENS with a persona name is ordinary speech, not
    /// attribution — a separator is REQUIRED before anything is peeled. And a
    /// non-persona capitalised opener is never touched.
    #[test]
    fn a_bare_name_opener_is_not_attribution() {
        let p = personas();
        for line in [
            "Nora says hi!",
            "Otto is on the calendar tonight, by the way.",
            "Dinner: pasta with the good tomatoes.",
            "\u{1F957} Dinner: pasta.",
            "Luca — you're up for the market run.",
        ] {
            assert_eq!(strip_self_attribution(line, &p), line, "over-stripped {line:?}");
            assert!(!has_self_attribution(line, &p), "false positive on {line:?}");
        }
        // A near-name is not the name.
        assert_eq!(strip_self_attribution("Norah: hello", &p), "Norah: hello");
    }

    /// With NO roster the attribution guard still peels a bare 💬 relay mark
    /// (that glyph is the gateway's own, never a family's word) but cannot know
    /// any persona name, so it touches nothing else.
    #[test]
    fn self_attribution_without_a_roster_only_peels_the_relay_mark() {
        let none: [&str; 0] = [];
        assert_eq!(strip_self_attribution("\u{1F4AC} all set.", &none), "all set.");
        assert_eq!(strip_self_attribution("Otto — the plan's up.", &none), "Otto — the plan's up.");
    }

    // ── Rule 1: off-roster human names ─────────────────────────────────────

    /// The exorcise-nadin leak: a composed reply names a retired teammate.
    #[test]
    fn a_retired_persona_name_is_stripped() {
        let v = voice();
        let draft = "Meals are set — still waiting on you and Nadin to confirm.";
        assert!(mentions_non_roster(draft, &v));
        assert_eq!(non_roster_ghosts(draft, &v), vec!["nadin".to_string()]);
        let out = gate_family_voice(draft, &v);
        assert!(!out.to_lowercase().contains("nadin"), "{out}");
        assert!(out.contains("Meals are set"), "the real content survives: {out}");
    }

    /// A capitalised name in a hand-off slot that is on NO roster is a phantom
    /// teammate and goes; a roster HUMAN in the same slot stays.
    #[test]
    fn an_off_roster_addressee_goes_and_a_roster_human_stays() {
        let v = voice();
        let phantom = "I'll hand it to Priya to confirm the market run.";
        assert_eq!(non_roster_addressees(phantom, &v), vec!["Priya".to_string()]);
        assert!(!gate_family_voice(phantom, &v).contains("Priya"));

        let real = "I'll check with Luca before we lock the market run.";
        assert!(non_roster_addressees(real, &v).is_empty(), "a roster human is not a ghost");
        assert_eq!(gate_family_voice(real, &v), real);

        // A weekday / relative day in the addressee slot is not a person.
        for line in ["Waiting on Friday to confirm.", "I'll check with Tomorrow's list."] {
            assert!(non_roster_addressees(line, &v).is_empty(), "date word eaten in {line:?}");
        }
        // A persona in the slot is fine too.
        assert!(non_roster_addressees("I'll pass it to Bruno.", &v).is_empty());
    }

    /// A retired name that is genuinely BACK on the roster is never scrubbed —
    /// the hardcoded list is always cross-checked against the live household.
    #[test]
    fn a_retired_name_back_on_the_roster_is_kept() {
        let v = FamilyVoice::from_rosters(&["Nora"], &["Nadin"]);
        let draft = "Checking with Nadin on the swim times.";
        assert!(!mentions_non_roster(draft, &v));
        assert_eq!(gate_family_voice(draft, &v), draft);
    }

    /// Scrubbing tidies the connectors a removed name leaves behind rather than
    /// leaving "waiting on you and  to confirm".
    #[test]
    fn scrubbing_a_ghost_tidies_the_dangling_connector() {
        let out = scrub_ghost_names("Meals are set — waiting on you and Nadin to confirm.", &["Nadin"]);
        assert!(!out.contains("and  "), "{out}");
        assert!(!out.contains(" ."), "{out}");
        assert!(out.starts_with("Meals are set"), "{out}");
    }

    // ── Rule 2: hand-off tails ─────────────────────────────────────────────

    /// The live 19:2x leak: a Nora DELIVERY that ended "🦆 Otto's got this one".
    /// The family asked one assistant and got the answer; the tail reads as
    /// buck-passing.
    #[test]
    fn a_trailing_handoff_is_stripped() {
        let p = personas();
        for (draft, keep) in [
            ("Dinner's chicken and rice. \u{1F986} Otto's got this one.", "Dinner's chicken and rice."),
            ("Here's the plan for tonight — over to Otto.", "Here's the plan for tonight"),
            ("Tuesday's set. I'll hand it over to Bruno.", "Tuesday's set."),
            ("The times are in — Coach Mira can take it from here.", "The times are in"),
            ("Shopping's frozen. Otto'll pick this up.", "Shopping's frozen."),
            ("It's on the calendar. Let Otto handle it", "It's on the calendar."),
        ] {
            assert!(has_handoff_tail(draft, &p), "missed a hand-off in {draft:?}");
            assert_eq!(strip_handoff_tail(draft, &p), keep, "bad strip of {draft:?}");
        }
    }

    /// A MID-sentence persona mention is not a hand-off — the answer continues
    /// after it, so nothing is cut.
    #[test]
    fn a_mid_sentence_mention_is_not_a_handoff() {
        let p = personas();
        for line in [
            "I asked Nora and she's on it, plus the plan's set for Thursday.",
            "Bruno's got this one covered and I've already added the shallots to the list.",
            "Otto is on the calendar tonight, so Friday is free.",
        ] {
            assert!(!has_handoff_tail(line, &p), "false hand-off in {line:?}");
            assert_eq!(strip_handoff_tail(line, &p), line);
        }
    }

    /// The tail guard is roster-DRIVEN: with no roster it is a no-op, and an
    /// honorific alone ("coach") never counts as a name.
    #[test]
    fn the_handoff_guard_is_roster_driven() {
        let none: [&str; 0] = [];
        let line = "All set. Otto's got this one.";
        assert!(!has_handoff_tail(line, &none));
        assert_eq!(strip_handoff_tail(line, &none), line);
        // "Coach Mira" contributes "mira", never a bare "coach".
        let only_mira = ["Coach Mira".to_string()];
        assert!(!has_handoff_tail("All set. Coach's got this one.", &only_mira));
        assert!(has_handoff_tail("All set. Mira's got this one.", &only_mira));
    }

    /// A reply that is NOTHING but a hand-off keeps its text — the gate never
    /// silences a reply, it only cleans one.
    #[test]
    fn a_pure_handoff_reply_is_never_emptied() {
        let p = personas();
        let out = strip_handoff_tail("Otto's got this one.", &p);
        assert!(!out.trim().is_empty(), "emptied a pure hand-off");
    }

    // ── Rule 3: infrastructure narration ───────────────────────────────────

    /// The live 19:2x leak, verbatim: the family heard a program describing its
    /// own plumbing. The whole reply was fetch-narration, so the gate answers
    /// with a warm family-voice offer instead of leaking it back.
    #[test]
    fn infrastructure_narration_is_dropped() {
        let v = voice();
        let draft = "I'd need to pull from what's actually in the system. \
                     Want me to grab that from the live gateway so you get the real take?";
        assert!(has_infra_narration(draft));
        let out = gate_family_voice(draft, &v);
        let lower = out.to_lowercase();
        assert!(!lower.contains("gateway"), "{out}");
        assert!(!lower.contains("in the system"), "{out}");
        assert!(!out.trim().is_empty(), "the gate must never send nothing");

        // A partial leak keeps the family sentence and drops only the plumbing.
        let mixed = "Thursday is pasta night. Let me query the database for the rest.";
        let cleaned = gate_family_voice(mixed, &v);
        assert!(cleaned.contains("Thursday is pasta night"), "{cleaned}");
        assert!(!cleaned.to_lowercase().contains("database"), "{cleaned}");
    }

    /// "System" is an ordinary family word. Only the INFRA COLLOCATIONS match —
    /// this is the false-positive direction that makes the rule safe.
    #[test]
    fn ordinary_family_speech_about_systems_survives() {
        let v = voice();
        for line in [
            "We need a better bedtime system for the school week.",
            "Her immune system is finally back to normal.",
            "Our chore system works if everyone actually looks at the chart.",
            "I'll check the calendar and call Mom about Sunday.",
            "Want me to check our recipe book for something quicker?",
        ] {
            assert!(!has_infra_narration(line), "false infra hit on {line:?}");
            assert_eq!(gate_family_voice(line, &v), line, "over-gated {line:?}");
        }
    }

    // ── Rule 4: ops jargon ─────────────────────────────────────────────────

    /// The live 2026-07-16 leak, verbatim shape: a machine week number inside an
    /// otherwise-family sentence. A TOKEN rewrite, so the sentence survives.
    #[test]
    fn a_machine_week_number_becomes_family_words() {
        let v = voice();
        assert_eq!(humanize_week_refs("W29's still sitting as a draft"), "next week's still sitting as a draft");
        assert_eq!(humanize_week_refs("2026-W29 is a draft"), "next week is a draft");
        let out = gate_family_voice("W29's still sitting as a draft — want me to publish it?", &v);
        assert!(!out.contains("W29"), "{out}");
        assert!(out.contains("next week"), "{out}");
        assert!(out.contains("publish it"), "the sentence survives: {out}");
    }

    /// A telemetry clause is dropped WHOLE (a token snip would leave a garbled
    /// half-sentence) while the surrounding family voice is kept.
    #[test]
    fn a_telemetry_clause_is_dropped_whole() {
        let v = voice();
        let draft = "Morning! The dispatcher has 3 agents alive and 6 in-progress. \
                     Dinner is chicken and rice tonight.";
        assert!(has_ops_jargon(draft));
        let out = gate_family_voice(draft, &v);
        let lower = out.to_lowercase();
        assert!(!lower.contains("dispatcher"), "{out}");
        assert!(!lower.contains("in-progress"), "{out}");
        assert!(!lower.contains("agents"), "{out}");
        assert!(out.contains("Morning!"), "{out}");
        assert!(out.contains("Dinner is chicken and rice tonight."), "{out}");
    }

    /// Ordinary family speech that carries a NUMBER, a "ready", or a "done" is
    /// not a status dump. It is the count-in-a-tally shape that marks telemetry.
    #[test]
    fn ordinary_family_counts_are_not_telemetry() {
        let v = voice();
        for line in [
            "3 bags are ready to go by the door.",
            "Dinner's ready in ten.",
            "We're done with the market run — two things left on the list.",
            "The kids have 2 swim sessions this week.",
        ] {
            assert!(!has_ops_jargon(line), "false ops hit on {line:?}");
            assert_eq!(gate_family_voice(line, &v), line, "over-gated {line:?}");
        }
    }

    /// A reply that was NOTHING but a console dump becomes an honest offer — the
    /// engine can never send an empty message (Telegram rejects it) and must
    /// never send the raw dump back either.
    #[test]
    fn a_pure_console_dump_becomes_an_honest_offer() {
        let v = voice();
        let dump = "dispatcher: 3 agents alive, 6 in-progress, 1 blocked, executor claude:opus, PID 4412, uptime 3h.";
        let out = gate_family_voice(dump, &v);
        assert!(!out.trim().is_empty(), "the engine must say something");
        assert!(!has_ops_jargon(&out), "still leaking telemetry: {out}");
        assert!(out.contains("Happy to dig into that"), "{out}");
    }

    // ── Rule 5: plain text ─────────────────────────────────────────────────

    /// Live: Nora's "**180–220 calories**" arrived on the pane with the stars
    /// visible, because the pane and the Telegram relay render PLAIN TEXT.
    #[test]
    fn markdown_emphasis_is_stripped_to_its_words() {
        assert_eq!(strip_markdown("About **180–220 calories** each."), "About 180–220 calories each.");
        assert_eq!(strip_markdown("*definitely* worth it"), "definitely worth it");
        assert_eq!(strip_markdown("use the `big pot`"), "use the big pot");
        assert_eq!(strip_markdown("**Dinner:** pasta and *good* bread"), "Dinner: pasta and good bread");
        assert!(has_markdown("**bold**"));
    }

    /// A lone or arithmetic marker is NOT emphasis and must survive verbatim.
    #[test]
    fn a_lone_or_arithmetic_marker_survives() {
        for line in ["5*7 is 35", "2*3*4", "a * b", "an unclosed *marker here", "one ` tick"] {
            assert_eq!(strip_markdown(line), line, "mangled {line:?}");
            assert!(!has_markdown(line), "false markdown hit on {line:?}");
        }
        // Emoji and ordinary punctuation are untouched.
        assert_eq!(strip_markdown("Dinner's at seven \u{1F957} — see you!"), "Dinner's at seven \u{1F957} — see you!");
    }

    // ── The gate as a whole ────────────────────────────────────────────────

    /// One draft carrying ALL SIX leaks at once comes out clean — this is the
    /// contract `deliver_reply` relies on.
    #[test]
    fn the_gate_cleans_all_six_rules_in_one_pass() {
        let v = voice();
        let p = personas();
        let draft = "Nora \u{1F4AC} **W29** is still a draft, and the dispatcher has 3 agents alive. \
                     I'd need to pull from the live gateway. \
                     Waiting on Nadin to confirm. \u{1F986} Otto's got this one.";
        let out = gate_family_voice(draft, &v);

        assert!(!has_self_attribution(&out, &p), "attribution left: {out}");
        assert!(!mentions_non_roster(&out, &v), "off-roster name left: {out}");
        assert!(!has_handoff_tail(&out, &p), "hand-off tail left: {out}");
        assert!(!has_infra_narration(&out), "infra narration left: {out}");
        assert!(!has_ops_jargon(&out), "ops jargon left: {out}");
        assert!(!has_markdown(&out), "markdown left: {out}");
        assert!(!out.contains("W29"), "machine week ref left: {out}");
        assert!(!out.trim().is_empty(), "the gate emptied the reply");
    }

    /// A CLEAN family reply is returned byte-for-byte. The gate sits in front of
    /// EVERY engine send, so a no-op on ordinary speech is load-bearing: the ack
    /// edit, the honest fallbacks, and the plain answers must all pass through
    /// untouched.
    #[test]
    fn a_clean_reply_passes_through_untouched() {
        let v = voice();
        for line in [
            "Dinner's chicken and rice tonight — Luca's cooking.",
            "Nothing's on the calendar for Saturday, so the day is yours.",
            "I've added shallots and the good tomatoes to the list.",
            "Saturday's dinner is baked white fish with tomato, olives & capers.",
            "Happy to dig into that — want me to take a proper look and get you the details?",
            "I don't want to just repeat myself — want me to actually go read the plan?",
        ] {
            assert_eq!(gate_family_voice(line, &v), line, "gate changed a clean reply: {line:?}");
        }
        // Blank in, blank out (the caller decides what to do about it).
        assert_eq!(gate_family_voice("   ", &v), "   ");
    }

    /// The gate is IDEMPOTENT. `finalize_composed_reply` gates before writing the
    /// session outbox and `deliver_reply` gates again at the send, so a second
    /// pass must be a no-op or the outbox and the sent message would diverge.
    #[test]
    fn the_gate_is_idempotent() {
        let v = voice();
        for draft in [
            "Nora \u{1F4AC} **W29** is a draft. I'd need to pull from the live gateway. \u{1F986} Otto's got this one.",
            "Meals are set — waiting on you and Nadin to confirm.",
            "The dispatcher has 3 agents alive. Dinner is pasta.",
            "About **180–220 calories** each.",
        ] {
            let once = gate_family_voice(draft, &v);
            let twice = gate_family_voice(&once, &v);
            assert_eq!(once, twice, "not idempotent for {draft:?}");
        }
    }

    /// The roster comes from the household's OWN files, never a hardcoded name.
    #[test]
    fn the_roster_loads_from_household_toml_and_the_binding_map() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("household.toml"),
            r#"
[household]
name = "Casa Rossi"
members = ["Alex"]

[[agent]]
id = "vera"
name = "Vera"
domains = ["meals"]

[[agent]]
id = "tobi"
name = "Coach Tobi"
domains = ["workouts"]
"#,
        )
        .unwrap();
        let v = FamilyVoice::load(root, root);
        // The household's personas, not the shipped ones.
        assert!(v.allows("vera") && v.allows("Coach Tobi") && v.allows("tobi"));
        assert!(v.allows("alex"), "a household member is an allowed name");
        assert!(!v.allows("nora"), "a foreign roster's persona is not allowed here");
        // And the hand-off guard now matches THIS family's cast.
        let p = v.persona_names().to_vec();
        assert!(has_handoff_tail("All set. Vera's got this one.", &p));
        assert!(has_handoff_tail("All set. Tobi's got this one.", &p));
        assert!(!has_handoff_tail("All set. Otto's got this one.", &p));
    }

    /// With NO `household.toml` the loader still yields the shipped persona ids,
    /// so a fresh deploy's replies are gated rather than un-guarded.
    #[test]
    fn the_roster_falls_back_to_the_shipped_personas() {
        let dir = tempfile::tempdir().unwrap();
        let v = FamilyVoice::load(dir.path(), dir.path());
        assert!(!v.is_empty(), "the fallback roster must not be empty");
        let p = v.persona_names().to_vec();
        assert!(has_handoff_tail("All set. Otto's got this one.", &p));
    }

    /// THE ENGINE'S OWN DEFER LINE SURVIVES. `ownership::defer_line` produces
    /// exactly the shape rule 2 strips ("Nora's got this one 🥗"), but it is the
    /// single-owner routing notice the engine appends after re-routing an ask —
    /// not the composer passing the buck. Stripping it would leave the family with
    /// no idea where their ask went, so an EXACT authorized suffix survives while
    /// the reply body is still fully gated.
    #[test]
    fn an_authorized_defer_tail_survives_but_the_body_is_still_gated() {
        let v = voice();
        let p = personas();
        let defer = "Nora's got this one \u{1F957}";
        let reply = format!("**Nice** protein swap. W29 is a draft.\n\n{defer}");
        let out = gate_family_voice_with(
            &reply,
            &v,
            GateOptions {
                authorized_handoff: Some(defer),
            },
        );
        assert!(out.ends_with(defer), "the ownership notice was stripped: {out}");
        assert!(!has_markdown(&out), "the body was not gated: {out}");
        assert!(!out.contains("W29"), "the body was not gated: {out}");
        assert!(out.contains("next week"), "{out}");

        // Without the authorization, the SAME tail is cut (so the exemption is
        // doing the work, not a hole in rule 2).
        let unauthorized = gate_family_voice(&reply, &v);
        assert!(!unauthorized.contains("got this one"), "{unauthorized}");
        assert!(!has_handoff_tail(&unauthorized, &p));

        // A reply that is NOTHING but the defer line is delivered as-is.
        assert_eq!(
            gate_family_voice_with(defer, &v, GateOptions { authorized_handoff: Some(defer) }),
            defer
        );
        // A composer tail that is NOT the authorized suffix is still cut.
        let smuggled = format!("All set. \u{1F986} Otto's got this one.\n\n{defer}");
        let out2 = gate_family_voice_with(
            &smuggled,
            &v,
            GateOptions { authorized_handoff: Some(defer) },
        );
        assert!(out2.ends_with(defer), "{out2}");
        assert!(!out2.contains("Otto"), "a composer hand-off rode in on the exemption: {out2}");
    }
}
