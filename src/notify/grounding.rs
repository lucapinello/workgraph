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

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Weekday};
use regex::{Captures, Regex};

use super::family_plan::{self, PlanDoc};
use crate::agency::TelegramBindingMap;

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
    // A reminder question is a READ of the family's own schedule — the plan's
    // `⏰ Reminder:` calendar rows answer it. Its absence here is half of why
    // "what date is the reminder to call the dentist set for?" was composed with
    // nothing in front of it and echoed the date the question carried (task
    // reminder-readback-lane). The NOUN only: the bare verb ("remind me to …")
    // is a write, and the deterministic, requester-scoped answer for a reminder
    // read lives in [`super::reminder_readback`].
    "reminder",
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
    "what",
    "when",
    "where",
    "which",
    "who",
    "how",
    "show",
    "tell",
    "give",
    "remind",
    "list",
    "do we",
    "are there",
    "is there",
    "any",
    "anything",
    "got any",
    "whats",
    "what's",
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
    let items: Vec<&String> = corrections
        .iter()
        .filter(|c| !c.trim().is_empty())
        .collect();
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
    FORMULAIC_OPENERS
        .iter()
        .any(|o| norm.starts_with(o) || norm.contains(o))
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
/// The production repro delivered the answer and then tacked on a promise to
/// ask another configured persona for their exact take. On a delivered turn
/// there is no one left to defer to: the persona IS the voice and already
/// answered. Generic promises are matched as substrings against the NORMALISED
/// trailing clause (apostrophe-free — see [`normalize`] — so "I'll" -> "ill").
/// Persona-specific "let me ask …" promises are matched separately against the
/// project-local [`FamilyVoiceRoster`], never against names compiled here.
/// Markers stay SPECIFIC (multi-word, never a bare "let me get") so a legitimate
/// action-ack ("on it, I'll change the week") is never mistaken for a deferral.
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

/// Tokenise a family-visible clause for configured-identity matching.
///
/// Unlike [`normalize`], punctuation (including apostrophes) is a boundary.
/// That makes an authored name match both `Blue Lantern` and
/// `Blue Lantern's`, without weakening the boundary enough for `Arc` to match
/// inside `Parcel`.
fn identity_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            word.extend(ch.to_lowercase());
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// True when a narrow ask-deferral lead-in is immediately followed by a
/// configured persona id, authored display name, or roster-derived alias.
///
/// Human names are intentionally excluded: asking a household member is not
/// third-person self-deferral by an engine persona. An empty/malformed roster
/// therefore fails open and never guesses that an unfamiliar word is a persona.
fn configured_persona_deferral(clause: &str, roster: &FamilyVoiceRoster) -> bool {
    const LEAD_INS: &[&[&str]] = &[&["let", "me", "ask"], &["i", "ll", "ask"]];

    let clause_words = identity_words(clause);
    if clause_words.is_empty() {
        return false;
    }

    roster.persona_names.iter().any(|configured| {
        let name_words = identity_words(configured);
        if name_words.is_empty() {
            return false;
        }
        LEAD_INS.iter().any(|lead_in| {
            let needed = lead_in.len() + name_words.len();
            clause_words.windows(needed).any(|window| {
                window[..lead_in.len()]
                    .iter()
                    .map(String::as_str)
                    .eq(lead_in.iter().copied())
                    && window[lead_in.len()..] == name_words
            })
        })
    })
}

/// True when `clause` (a single trailing sentence/clause) is a dangling-promise
/// deferral — a fresh "let me get X's exact take / I'll get back to you" tacked
/// onto an answer that was already delivered. Persona-specific forms are
/// resolved only from `roster`. Pure.
pub fn is_deferral_tail(clause: &str, roster: &FamilyVoiceRoster) -> bool {
    let norm = normalize(clause);
    if norm.is_empty() {
        return false;
    }
    DEFERRAL_MARKERS.iter().any(|m| norm.contains(m)) || configured_persona_deferral(clause, roster)
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
pub fn strip_deferral_tail(reply: &str, roster: &FamilyVoiceRoster) -> (String, Option<String>) {
    let trimmed = reply.trim_end();
    if trimmed.is_empty() {
        return (reply.to_string(), None);
    }
    let start = final_clause_start(trimmed);
    let tail = trimmed[start..].trim();
    if tail.is_empty() || !is_deferral_tail(tail, roster) {
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
pub fn enforce_no_deferral(reply: &str, roster: &FamilyVoiceRoster) -> String {
    let (body, stripped) = strip_deferral_tail(reply, roster);
    match stripped {
        Some(_) if !body.trim().is_empty() => body,
        _ => reply.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Family-visible reply gate — the engine-side twin of the gateway seam
// ---------------------------------------------------------------------------

/// Names the engine is allowed to treat as real household members.
///
/// Persona names come from the project's `household.toml`; human names come
/// from the live Telegram binding map, with the gateway's configured humans as
/// the same bare-checkout fallback it uses. `household.members` is deliberately
/// excluded: it seeds persona prompts but is not the live roster. The guard
/// never bakes a particular family's names into the binary. Without an
/// authoritative human-roster source, an unfamiliar capitalised word is not
/// proof of a phantom person.
#[derive(Debug, Clone, Default)]
pub struct FamilyVoiceRoster {
    persona_names: Vec<String>,
    allowed_names: HashSet<String>,
    has_evidence: bool,
}

impl FamilyVoiceRoster {
    /// Build a roster from caller-supplied persona and human names. Public so
    /// the pure guard can be tested with a household-independent fixture.
    pub fn from_names<P, H, PS, HS>(personas: P, humans: H) -> Self
    where
        P: IntoIterator<Item = PS>,
        H: IntoIterator<Item = HS>,
        PS: Into<String>,
        HS: Into<String>,
    {
        let mut roster = Self::default();
        for name in personas {
            roster.add_persona(name.into());
        }
        for name in humans {
            roster.add_allowed(name.into());
        }
        roster
    }

    fn add_persona(&mut self, name: String) {
        if name.trim().is_empty() {
            return;
        }
        add_name_aliases(&mut self.allowed_names, &name);
        let trimmed = name.trim();
        let alias = trimmed
            .split_whitespace()
            .find(|part| !is_name_alias_stopword(&part.to_lowercase()));
        for pattern in [Some(trimmed), alias].into_iter().flatten() {
            if pattern.chars().count() >= 2
                && !self
                    .persona_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(pattern))
            {
                self.persona_names.push(pattern.to_string());
            }
        }
    }

    fn add_allowed(&mut self, name: String) {
        if name.trim().is_empty() {
            return;
        }
        self.has_evidence = true;
        add_name_aliases(&mut self.allowed_names, &name);
    }

    fn allows(&self, name: &str) -> bool {
        self.allowed_names.contains(&name.trim().to_lowercase())
    }
}

/// Load the family-voice roster from the same project-local sources that define
/// the running household. Best-effort: malformed or absent files yield fewer
/// names, never a panic and never a compiled-in fallback roster.
pub fn load_family_voice_roster(project_root: &Path, workgraph_dir: &Path) -> FamilyVoiceRoster {
    let mut roster = FamilyVoiceRoster::default();

    if let Ok(body) = std::fs::read_to_string(project_root.join("household.toml")) {
        if let Ok(value) = body.parse::<toml::Value>() {
            if let Some(agents) = value.get("agent").and_then(toml::Value::as_array) {
                for agent in agents {
                    if let Some(id) = agent.get("id").and_then(toml::Value::as_str) {
                        roster.add_persona(id.to_string());
                    }
                    if let Some(name) = agent.get("name").and_then(toml::Value::as_str) {
                        roster.add_persona(name.to_string());
                    }
                }
            }
        }
    }

    let bindings = TelegramBindingMap::load(&workgraph_dir.join("agency"))
        .ok()
        .map(|map| map.bindings)
        .unwrap_or_default();
    if bindings.is_empty() {
        // HumansSource in the gateway uses configured humans only when there
        // are no agency bindings. Mirror that precedence instead of merging a
        // stale fallback name into the live roster.
        let gateway_config = [
            project_root.join("casa-gateway.toml"),
            project_root.join("claw3d-bridge").join("casa-gateway.toml"),
        ]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|body| body.parse::<toml::Value>().ok())
        });
        if let Some(value) = gateway_config {
            roster.has_evidence = true;
            if let Some(humans) = value.get("humans").and_then(toml::Value::as_array) {
                for human in humans {
                    if let Some(id) = human.get("id").and_then(toml::Value::as_str) {
                        roster.add_allowed(id.to_string());
                        if let Some(id) = id.strip_prefix("human-") {
                            roster.add_allowed(id.to_string());
                        }
                    }
                    if let Some(label) = human.get("label").and_then(toml::Value::as_str) {
                        roster.add_allowed(label.to_string());
                    }
                }
            }
        }
    } else {
        roster.has_evidence = true;
        for binding in bindings {
            roster.add_allowed(binding.name);
            roster.add_allowed(binding.agent_id.clone());
            if let Some(id) = binding.agent_id.strip_prefix("human-") {
                roster.add_allowed(id.to_string());
            }
        }
    }
    roster
}

fn add_name_aliases(allowed: &mut HashSet<String>, raw: &str) {
    let name = raw.trim().to_lowercase();
    if name.is_empty() {
        return;
    }
    allowed.insert(name.clone());
    if let Some(alias) = name
        .split_whitespace()
        .find(|part| !is_name_alias_stopword(part))
    {
        allowed.insert(alias.to_string());
    }
}

fn is_name_alias_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "coach"
            | "chef"
            | "dr"
            | "dr."
            | "mr"
            | "mr."
            | "mrs"
            | "mrs."
            | "ms"
            | "ms."
    )
}

fn persona_name_patterns(names: &[String]) -> Vec<String> {
    let mut patterns = HashSet::new();
    for raw in names {
        let name = raw.trim();
        if name.chars().count() >= 2 {
            patterns.insert(name.to_string());
        }
    }
    let mut out: Vec<String> = patterns.into_iter().collect();
    out.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
    out
}

/// Return the remaining text when `text` starts with `name`, case-insensitively
/// and on a token boundary. Works for UTF-8 names without deriving byte offsets
/// from a lower-cased string whose width may differ.
fn strip_name_prefix_ci<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let count = name.chars().count();
    let end = text
        .char_indices()
        .nth(count)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let candidate = text.get(..end)?;
    if candidate.to_lowercase() != name.to_lowercase() {
        return None;
    }
    let rest = &text[end..];
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(rest)
}

fn trim_attribution_separator(text: &str) -> &str {
    text.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, ':' | '：' | '—' | '–' | '-')
    })
}

static ATTR_AVATAR_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[\p{So}\p{Sk}\u{FE0F}\u{200D}]\s*)+").expect("valid attribution-avatar regex")
});

/// Strip a leading persona attribution already carried by the sender/avatar
/// fields: `Name 💬 …`, `💬 Name: …`, `Name: …`, or a bare leading `💬`.
/// Persona matching is entirely roster-driven.
pub fn strip_self_attribution(reply: &str, roster: &FamilyVoiceRoster) -> String {
    let original = reply.trim();
    if original.is_empty() {
        return original.to_string();
    }
    let names = persona_name_patterns(&roster.persona_names);
    let mut out = original;
    for _ in 0..3 {
        let before = out;
        let plain = out.trim_start();

        if let Some(rest) = plain.strip_prefix('💬') {
            let rest = trim_attribution_separator(rest);
            let mut after_name = None;
            for name in &names {
                if let Some(tail) = strip_name_prefix_ci(rest, name) {
                    after_name = Some(trim_attribution_separator(tail));
                    break;
                }
            }
            out = after_name.unwrap_or(rest);
        } else {
            // A leading avatar/symbol is ignored only while probing for a
            // roster name. No text is changed unless the name is followed by
            // an attribution marker or required punctuation.
            let probe = ATTR_AVATAR_PREFIX_RE
                .find(plain)
                .map(|prefix| &plain[prefix.end()..])
                .unwrap_or(plain);
            for name in &names {
                let Some(tail) = strip_name_prefix_ci(probe, name) else {
                    continue;
                };
                let spaced = tail.trim_start();
                if let Some(rest) = spaced.strip_prefix('💬') {
                    out = trim_attribution_separator(rest);
                    break;
                }
                if spaced
                    .chars()
                    .next()
                    .is_some_and(|c| matches!(c, ':' | '：' | '—' | '–'))
                {
                    out = trim_attribution_separator(spaced);
                    break;
                }
            }
        }

        if out == before {
            break;
        }
    }
    out.trim().to_string()
}

static TRANSFER_ADDRESSEE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?P<action>(?i:\b(?:
            hand(?:ed|\s+it)?(?:\s+(?:it|over))? |
            pass(?:\s+it)? |
            give(?:\s+it)?
        )))\s+(?i:to)\s+
        (?P<name>\p{Lu}[\p{L}'’\-]{2,}(?:\s+\p{Lu}[\p{L}'’\-]{2,}){0,2})\b",
    )
    .expect("valid transfer-addressee regex")
});

static CHECK_ADDRESSEE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?P<action>(?i:\b(?:check|confirm)))\s+(?i:with)\s+
        (?P<name>\p{Lu}[\p{L}'’\-]{2,}(?:\s+\p{Lu}[\p{L}'’\-]{2,}){0,2})\b",
    )
    .expect("valid check-addressee regex")
});

static WAIT_ADDRESSEE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?i:\bwaiting\s+(?:on|for))\s+
        (?:(?i:you|me|us|them|him|her)\s*,?\s*(?:(?i:and)|&)\s+)?
        (?P<name>\p{Lu}[\p{L}'’\-]{2,}(?:\s+\p{Lu}[\p{L}'’\-]{2,}){0,2})\b",
    )
    .expect("valid waiting-addressee regex")
});

static DECLARATIVE_PERSON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \b(?P<name>\p{Lu}[\p{L}'’\-]{2,}(?:\s+\p{Lu}[\p{L}'’\-]{2,}){0,2})\s+
        (?:
            (?i:(?:will|can|could|might|should)\s+
                (?:join|come|confirm|reply|answer|help|bring|meet|call)) |
            (?i:(?:is|was|will\s+be|has\s+been)\s+
                (?:joining|coming|confirming|replying|answering|helping|bringing|meeting|calling)) |
            (?i:said|asked|confirmed|replied|answered|offered|promised)
        )\b",
    )
    .expect("valid declarative-person regex")
});

fn is_not_a_person(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "monday"
            | "tuesday"
            | "wednesday"
            | "thursday"
            | "friday"
            | "saturday"
            | "sunday"
            | "january"
            | "february"
            | "march"
            | "april"
            | "may"
            | "june"
            | "july"
            | "august"
            | "september"
            | "october"
            | "november"
            | "december"
            | "today"
            | "tomorrow"
            | "tonight"
            | "yesterday"
            | "everyone"
            | "someone"
            | "anyone"
            | "you"
            | "we"
            | "them"
            | "him"
            | "her"
            | "it"
    )
}

fn scrub_addressee_pattern(text: &str, pattern: &Regex, roster: &FamilyVoiceRoster) -> String {
    pattern
        .replace_all(text, |caps: &Captures<'_>| {
            let name = caps.name("name").map(|m| m.as_str()).unwrap_or("");
            if roster.allows(name) || is_not_a_person(name) {
                return caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string();
            }
            caps.name("action")
                .map(|m| m.as_str())
                .unwrap_or("")
                .to_string()
        })
        .to_string()
}

fn has_off_roster_match(text: &str, pattern: &Regex, roster: &FamilyVoiceRoster) -> bool {
    pattern.captures_iter(text).any(|caps| {
        let name = caps.name("name").map(|m| m.as_str()).unwrap_or("");
        !roster.allows(name) && !is_not_a_person(name)
    })
}

fn tidy_family_text(text: &str) -> String {
    static EMPTY_PARENS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(\s*\)").expect("valid empty-parens regex"));
    static SPACE_PUNCT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s+([.,;:!?])").expect("valid punctuation regex"));
    static MULTISPACE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[ \t]{2,}").expect("valid multispace regex"));

    let out = EMPTY_PARENS_RE.replace_all(text, "");
    let out = SPACE_PUNCT_RE.replace_all(&out, "$1");
    let out = MULTISPACE_RE.replace_all(&out, " ");
    out.trim_matches(|c: char| c.is_whitespace() || matches!(c, '—' | '–' | ',' | ';' | ':' | '-'))
        .trim()
        .to_string()
}

/// Remove a capitalised person reference that is absent from a real roster, but
/// only in strong, unambiguous transfer/check/waiting constructions or beside a
/// narrowly human social action ("will join", "confirmed", "replied"). A clause
/// that depends on a phantom person is dropped whole so the rewrite cannot
/// leave malformed copy such as "pass when ready", "we're to confirm", or "will
/// join us". With no roster evidence this is a no-op; ordinary names elsewhere
/// in a sentence are never guessed at or rewritten.
pub fn scrub_off_roster_addressees(reply: &str, roster: &FamilyVoiceRoster) -> String {
    if !roster.has_evidence {
        return reply.to_string();
    }
    let out = scrub_addressee_pattern(reply, &CHECK_ADDRESSEE_RE, roster);
    let has_phantom_clause = has_off_roster_match(&out, &TRANSFER_ADDRESSEE_RE, roster)
        || has_off_roster_match(&out, &WAIT_ADDRESSEE_RE, roster)
        || has_off_roster_match(&out, &DECLARATIVE_PERSON_RE, roster);
    if !has_phantom_clause {
        return if out == reply {
            reply.to_string()
        } else {
            tidy_family_text(&out)
        };
    }
    let out = family_clauses(&out)
        .into_iter()
        .filter(|clause| {
            !has_off_roster_match(clause, &TRANSFER_ADDRESSEE_RE, roster)
                && !has_off_roster_match(clause, &WAIT_ADDRESSEE_RE, roster)
                && !has_off_roster_match(clause, &DECLARATIVE_PERSON_RE, roster)
        })
        .collect::<Vec<_>>()
        .join(" ");
    tidy_family_text(&out)
}

fn handoff_patterns(name_alt: &str) -> Vec<String> {
    vec![
        format!(r"(?:{name_alt})(?:'s|’s|\s+is|\s+has)?\s+got\s+(?:this|it|that)(?:\s+one)?"),
        format!(
            r"(?:hand(?:ing)?|pass(?:ing)?|kick(?:ing)?|send(?:ing)?|toss(?:ing)?|leav(?:e|ing))\s+(?:this|it|that)?\s*(?:one\s+)?(?:off\s+)?(?:over\s+)?to\s+(?:{name_alt})"
        ),
        format!(r"over\s+to\s+(?:{name_alt})"),
        format!(
            r"(?:{name_alt})\s+(?:can|could|will|'ll|’ll|should|is\s+gonna|is\s+going\s+to)\s+(?:take|grab|handle|pick\s+up|sort|cover|help\s+with|run\s+with|jump\s+on)\s+(?:it|this|that)(?:\s+(?:one|up|out))?(?:\s+from\s+here)?"
        ),
        format!(r"(?:{name_alt})(?:'s|’s|\s+is)\s+on\s+(?:it|this|that)(?:\s+one)?"),
        format!(
            r"let\s+(?:{name_alt})\s+(?:take|handle|grab|sort|cover|run\s+with)\s+(?:it|this|that)"
        ),
        format!(r"(?:{name_alt})(?:'ll|’ll|\s+will)\s+pick\s+(?:this|it|that)\s+up"),
        format!(
            r"(?:{name_alt})(?:'s|’s|\s+is)\s+(?:your|the)\s+(?:go[- ]?to|person|one)\s+for\s+(?:this|that|it)"
        ),
    ]
}

static ORPHANED_AVATAR_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[\p{So}\p{Sk}\u{FE0F}\u{200D}]\s*)+$").expect("valid handoff-avatar regex")
});

/// Subject/connector fragments a removed handoff can strand before its verb.
/// The list is deliberately closed and is consulted only after a terminal,
/// roster-driven handoff matched.
const DANGLING_HANDOFF_WORDS: &[&str] = &[
    "and", "but", "so", "then", "plus", "also", "i", "i'll", "we", "we'll",
];

fn trim_handoff_head(head: &str) -> String {
    let without_avatar = ORPHANED_AVATAR_SUFFIX_RE.replace(head.trim_end(), "");
    let mut out = without_avatar.to_string();
    for _ in 0..4 {
        let trimmed = out
            .trim_end_matches(|c: char| {
                c.is_whitespace() || matches!(c, '—' | '–' | '-' | ',' | ';' | ':')
            })
            .trim_end();
        if trimmed.ends_with(['.', '!', '?', '…']) {
            return trimmed.to_string();
        }
        let Some(last) = trimmed.split_whitespace().next_back() else {
            return trimmed.to_string();
        };
        let key = last
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '\'' | '’'))
            .collect::<String>()
            .to_lowercase()
            .replace('’', "'");
        if !DANGLING_HANDOFF_WORDS.contains(&key.as_str()) {
            return trimmed.to_string();
        }
        out = trimmed[..trimmed.len() - last.len()].to_string();
    }
    out.trim_end().to_string()
}

/// Strip a terminal handoff to a configured persona. Patterns are end-anchored,
/// so a factual mid-sentence mention remains intact.
pub fn strip_handoff_tail(reply: &str, roster: &FamilyVoiceRoster) -> String {
    let original = reply.to_string();
    let names = persona_name_patterns(&roster.persona_names);
    if names.is_empty() || reply.trim().is_empty() {
        return original;
    }
    let name_alt = names
        .iter()
        .map(|name| regex::escape(name))
        .collect::<Vec<_>>()
        .join("|");
    let mut cut: Option<usize> = None;
    for pattern in handoff_patterns(&name_alt) {
        let Ok(re) = Regex::new(&format!(
            r"(?i)(?:^|[^\p{{L}}\p{{N}}_])({pattern})[\s\p{{P}}\p{{S}}\u{{FE0F}}\u{{200D}}]*$"
        )) else {
            continue;
        };
        if let Some(found) = re.captures(reply).and_then(|captures| captures.get(1)) {
            cut = Some(cut.map_or(found.start(), |current| current.min(found.start())));
        }
    }
    let Some(cut) = cut else {
        return original;
    };
    let head = trim_handoff_head(&reply[..cut]);
    if head.is_empty() { String::new() } else { head }
}

static INFRA_SIGNALS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let store = r"(?:gateway|pipeline|back[\s-]?end|database|datastore|data\s*store|data\s*feed|the\s+api|(?:the|our|your|this|that)\s+system)";
    [
        format!(
            r"(?i)\b(?:pull|pulling|grab|grabbing|fetch|fetching|query|querying|load|loading|scrape|scraping|sync|syncing|hit|hitting|poll|polling|retrieve|retrieving|read)\b[^.!?…]{{0,40}}\b(?:from|off|out\s+of|into|against|to|up|via|through)\b[^.!?…]{{0,40}}{store}"
        ),
        r"(?i)\b(?:in|from|on|via|through|inside|within|across|over\s+(?:in|on|at))\s+(?:(?:the|our|a|this|that|live)\s+)*(?:gateway|pipeline|back[\s-]?end|database|datastore|data\s*store|data\s*feed|api)\b"
            .to_string(),
        format!(
            r"(?i)\b(?:query|querying|hit|hitting|poll|polling|ping|pinging|scrape|scraping|fetch|fetching|pull|pulling)\b[^.!?…]{{0,12}}{store}"
        ),
        r"(?i)\bthe\s+live\s+gateway\b".to_string(),
        r"(?i)\bdata\s+pipeline\b".to_string(),
        r"(?i)\b(?:in|from|inside|within)\s+(?:the|our|your|this|that)\s+system\b"
            .to_string(),
    ]
    .into_iter()
    .map(|pattern| Regex::new(&pattern).expect("valid infrastructure regex"))
    .collect()
});

/// True when the reply narrates its data plumbing rather than speaking in
/// family terms. A bare `system` is intentionally not enough.
pub fn has_infra_narration(reply: &str) -> bool {
    INFRA_SIGNALS.iter().any(|re| re.is_match(reply))
}

static CLAUSE_SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[.!?…;；]\s+|\s+[-•·—–]\s+|\r?\n+)").expect("valid family-clause splitter")
});

fn family_clauses(text: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut start = 0usize;
    for separator in CLAUSE_SPLIT_RE.find_iter(text) {
        let raw = &text[separator.start()..separator.end()];
        let first = raw.chars().next();
        let end = if first.is_some_and(|c| matches!(c, '.' | '!' | '?' | '…')) {
            separator.start() + first.unwrap().len_utf8()
        } else {
            separator.start()
        };
        let mut clause = text[start..end].trim().to_string();
        if first.is_some_and(|c| matches!(c, ';' | '；')) && !clause.ends_with(['.', '!', '?', '…'])
        {
            clause.push('.');
        }
        if !clause.is_empty() {
            clauses.push(clause);
        }
        start = separator.end();
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        clauses.push(tail.to_string());
    }
    clauses
}

/// Drop clauses that narrate infrastructure while keeping any ordinary family
/// content around them.
pub fn scrub_infra_narration(reply: &str) -> String {
    if reply.trim().is_empty() || !has_infra_narration(reply) {
        return reply.to_string();
    }
    tidy_family_text(
        &family_clauses(reply)
            .into_iter()
            .filter(|clause| !has_infra_narration(clause))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

static WEEK_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:\d{4}-)?W\d{1,2}\b").expect("valid week regex"));

static OPS_SIGNALS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\bagent-\d+\b",
        r"(?i)\bdispatcher\b",
        r"(?i)\bclaude[:/][a-z0-9._-]+",
        r"(?i)\bexecutor\b",
        r"(?i)\b(?:max\s+\d+\s+agents?|\d+\s+max\s+(?:agents?|workers?)|max\s+workers?)\b",
        r"(?i)\b\d+\s+agents?\b",
        r"(?i)\b\d+\s+alive\b",
        r"(?i)\b\d+\s+(?:in-progress|in\s+progress)\b",
        r"(?i)\bcron\b",
        r"(?i)\bnext\s+fire\b",
        r"(?i)\buptime\b",
        r"(?i)\bPID\s*\d+",
        r"(?i)\bopenrouter\b",
        r"(?i)\bregistry\s+refresh\b",
        r"(?i)\bdaemon\b",
        r"(?i)\bwg\s+\w+",
        r"(?i)\b\d+\s+(?:recurring|paused|blocked)\b",
        // THE C011 LEAK (live-cert run 2): a promise-correction told the family
        // "I've flagged it for the coordinator so it doesn't slip." The coordinator
        // is a machine role no family asked about, and "flagged it for X" is how it
        // reaches them. Both are refused here so no composed reply — or future
        // correction copy — can carry them again.
        r"(?i)\bcoordinator\b",
        r"(?i)\bflagged\s+(?:it|that|this)\s+(?:for|with|to)\b",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("valid operations-jargon regex"))
    .collect()
});

pub fn has_ops_jargon(reply: &str) -> bool {
    OPS_SIGNALS.iter().any(|re| re.is_match(reply))
}

/// Rewrite machine week tokens and drop orchestration/telemetry clauses.
pub fn scrub_ops_jargon(reply: &str) -> String {
    if reply.trim().is_empty() {
        return reply.to_string();
    }
    let weeked = WEEK_REF_RE.replace_all(reply, "next week").to_string();
    if !has_ops_jargon(&weeked) {
        return weeked;
    }
    tidy_family_text(
        &family_clauses(&weeked)
            .into_iter()
            .filter(|clause| !has_ops_jargon(clause))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

static MD_BOLD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*(\S(?:[^*\n]*\S)?)\*\*").expect("valid bold-markdown regex"));
static MD_ITALIC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^\p{L}\p{N}_*])\*(\S(?:[^*\n]*\S)?)\*($|[^\p{L}\p{N}_*])")
        .expect("valid italic-markdown regex")
});
static MD_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`(\S(?:[^`\n]*\S)?)`").expect("valid code-markdown regex"));

/// Strip paired emphasis/code markers from the plain-text family surfaces.
/// Lone and arithmetic asterisks remain unchanged.
pub fn strip_markdown(reply: &str) -> String {
    let mut out = MD_BOLD_RE.replace_all(reply, "$1").to_string();
    loop {
        let next = MD_ITALIC_RE.replace_all(&out, "$1$2$3").to_string();
        if next == out {
            break;
        }
        out = next;
    }
    MD_CODE_RE.replace_all(&out, "$1").to_string()
}

pub fn family_voice_fallback_line() -> String {
    "I don't have a useful answer to share yet.".to_string()
}

/// Per-reply exceptions authored by the engine after composition.
#[derive(Debug, Clone, Copy, Default)]
pub struct FamilyVoiceOptions<'a> {
    /// An exact terminal ownership notice produced by
    /// [`crate::notify::ownership::defer_line`]. The composer cannot authorize
    /// its own handoff: callers must supply the exact engine-authored suffix.
    pub authorized_handoff: Option<&'a str>,
}

/// Apply every family-visible copy guard at the last engine seam before a
/// reply is persisted and delivered. Clean replies take the no-op path through
/// each transformer; a draft reduced to only plumbing/telemetry becomes an
/// honest neutral line rather than an empty send or a raw console dump.
pub fn enforce_family_voice(reply: &str, roster: &FamilyVoiceRoster) -> String {
    enforce_family_voice_with(reply, roster, FamilyVoiceOptions::default())
}

/// [`enforce_family_voice`] with narrow, per-reply engine-authored exceptions.
pub fn enforce_family_voice_with(
    reply: &str,
    roster: &FamilyVoiceRoster,
    options: FamilyVoiceOptions<'_>,
) -> String {
    let original = reply.trim();
    if original.is_empty() {
        return family_voice_fallback_line();
    }
    // Ownership routing appends one trusted handoff *after* composition. Split
    // only an exact suffix supplied by that call site, fully guard the body,
    // then restore the trusted bytes. An invented tail elsewhere is still
    // stripped; an unauthorized handoff-only draft still becomes the fallback.
    if let Some(tail) = options
        .authorized_handoff
        .map(str::trim)
        .filter(|tail| !tail.is_empty())
    {
        if let Some(head) = original.strip_suffix(tail) {
            let head = head.trim();
            if head.is_empty() {
                return tail.to_string();
            }
            let guarded_head = enforce_family_voice_with(
                head,
                roster,
                FamilyVoiceOptions {
                    authorized_handoff: None,
                },
            );
            return format!("{guarded_head}\n\n{tail}");
        }
    }
    // Paired markdown can wrap the very tokens the roster-aware rules inspect
    // (`**Name** 💬`, `check with **Name**`). Normalize it before those rules,
    // then once more at the end for idempotence.
    let mut out = strip_markdown(original);
    out = strip_self_attribution(&out, roster);
    out = scrub_off_roster_addressees(&out, roster);
    out = strip_handoff_tail(&out, roster);
    if has_infra_narration(&out) {
        let clean = scrub_infra_narration(&out);
        out = if clean.trim().is_empty() {
            "Happy to dig into that — want me to take a proper look and get you the details?"
                .to_string()
        } else {
            clean
        };
    }
    out = scrub_ops_jargon(&out);
    out = strip_markdown(&out);
    let out = out.trim();
    if out.is_empty() {
        family_voice_fallback_line()
    } else {
        out.to_string()
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
        let d = today.succ_opt().and_then(|d| d.succ_opt()).unwrap_or(today);
        parts.push(format!("\"day after tomorrow\" = {}", fmt(d)));
    } else if norm.contains("tomorrow") || norm.contains("tmrw") || norm.contains("tmw") {
        let d = today.succ_opt().unwrap_or(today);
        // "tomorrow night" is still tomorrow's date — the evening OF that day.
        parts.push(format!(
            "\"tomorrow\" (incl. \"tomorrow night\") = {}",
            fmt(d)
        ));
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
    if norm.contains("week")
        || norm.contains("weekend")
        || norm.contains("coming days")
        || norm.contains("next few days")
        || norm.contains("days ahead")
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
    let minute: u32 = if m_str.is_empty() {
        0
    } else {
        m_str.parse().ok()?
    };
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
    family_plan::expand_weekday(row_weekday).eq_ignore_ascii_case(family_plan::long_weekday(day))
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
pub fn schedule_grounding_for(
    doc: &PlanDoc,
    now: NaiveDateTime,
    message: &str,
) -> ScheduleGrounding {
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
pub fn fetch_schedule_grounding(
    root: &Path,
    now: NaiveDateTime,
    message: &str,
) -> ScheduleGrounding {
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
    // Callers that cannot know about an external feed keep the historical behaviour.
    schedule_context_line_scoped(doc, now, false)
}

/// [`schedule_context_line`], told whether an EXTERNAL calendar feed exists whose events never
/// reach the plan document. When one does, an empty plan is not evidence of a free day.
pub fn schedule_context_line_scoped(
    doc: Option<&PlanDoc>,
    now: NaiveDateTime,
    external_feed_configured: bool,
) -> String {
    let today = now.date();
    let label = format!(
        "{} {}",
        family_plan::long_weekday(today),
        today.format("%b %-d")
    );
    let titles = doc
        .map(|d| upcoming_titles_on(d, today, now))
        .unwrap_or_default();
    if titles.is_empty() && !external_feed_configured {
        format!(
            "CALENDAR ({label}) — there is NOTHING on the calendar today. Do NOT invent a \
             meeting, appointment, birthday, or any event, and do NOT say the day is \
             busy/packed/back-to-back. If asked, say the calendar is clear.\n"
        )
    } else if titles.is_empty() {
        // The plan carries no rows for today AND this house syncs an external calendar whose
        // events never reach the plan document. "Clear" would then be a confident claim about
        // something we cannot see — which is exactly the failure this house already fixed once on
        // the gateway (task safety-critical-fast, 2026-07-20) and shipped again here: on
        // 2026-08-18 the family was told "Calendar's clear — nothing on the books" while a real
        // school pickup sat in the linked Google calendar.
        //
        // The recorded principle: an empty result from a source that is not authoritative is
        // answered by a HEDGE, never by "you are free".
        format!(
            "CALENDAR ({label}) — the week plan lists no events for today, but this household \
             syncs an EXTERNAL calendar that is NOT visible to you here. You therefore do NOT \
             know whether the day is free. Never describe the day as clear, empty or free, and do \
             NOT invent an event either. If asked what is on, say plainly that you can see \
             nothing on the plan for today and that anything in the linked calendar would not \
             show up here.\n"
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

/// Does this household sync an EXTERNAL calendar (a Google/iCal feed) whose events do not land in
/// the week plan?
///
/// The gateway keeps the secret feed URL in `.casa/calendar.toml` and merges its occurrences into
/// `/calendar.json` and the Week view. None of that reaches a `PlanDoc`, so from here the plan is
/// an incomplete view of the family's day whenever this returns true. Presence of a non-empty
/// The key is `ics_url` — the name the gateway itself writes
/// (`claw3d-bridge/src/calendarSource.mjs writeCalendarConfig`: `[calendar]\nics_url = "…"`). I
/// first wrote this probe against a guessed `url` and it silently reported "no feed" on the live
/// house, which would have left the hedge below dormant and shipped the same bug again; a test
/// against the real config caught it. `url` is still accepted, so a hand-written config using the
/// shorter key is not ignored. The URL itself never enters a prompt or a log.
pub fn external_calendar_configured(root: &Path) -> bool {
    let path = root.join(".casa").join("calendar.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .any(|(key, value)| {
            let key = key.trim();
            (key.eq_ignore_ascii_case("ics_url") || key.eq_ignore_ascii_case("url"))
                && value.trim().trim_matches('"').len() > 8
        })
}

/// How stale the gateway's calendar snapshot may be before this process stops trusting it.
///
/// The gateway confirms the feed every 5 minutes ([`DEFAULT_POLL_MS`] in
/// `calendarSource.mjs`), so a live house is always minutes old. An hour is twelve missed
/// polls: by then the gateway is down or the feed is unreachable, the family may well have
/// added something since, and the honest answer goes back to the hedge.
const CALENDAR_SNAPSHOT_MAX_AGE_SECS: i64 = 60 * 60;

/// What this process can see of the household's real calendar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarView {
    /// No snapshot on disk, or one too old to trust — the feed is configured but invisible
    /// from here, so an empty plan is NOT evidence of a free day.
    Blind,
    /// A snapshot this process trusts. `titles` are today's remaining events, in order.
    /// Empty means the day genuinely is clear on BOTH the plan and the calendar.
    Visible { titles: Vec<String> },
}

/// Read the gateway's merged calendar snapshot (`.casa/calendar/synced-events.json`) and
/// return what it says about `now`'s day.
///
/// WHY A FILE AND NOT A FETCH. The Google feed is fetched by the GATEWAY and kept in memory;
/// this process never saw it, which is why Otto could only ever answer "anything in the
/// linked calendar would not show up here" — true, and useless to a family whose whole reason
/// for linking a calendar is that the house should know what is on it. The gateway now writes
/// the merged, already-family-voiced list beside the family quick-add store, and this reads
/// it. Deliberately NOT the raw ICS: a second recurrence expander here would be a twin of
/// `icsParser.mjs`, and the two would drift on exactly the RRULE shapes that matter — an
/// expired repeating "pickup the kids" entry is how this was found.
///
/// Freshness is judged on `fetchedAt` (when the FEED was last confirmed), never on the file's
/// own mtime or `writtenAt`: a failed poll leaves the previous snapshot in place, and reading
/// the write time would make an unreachable calendar look live. `feed: "none"` needs no
/// freshness at all — with no external feed the family's own entries ARE the whole calendar,
/// and they are complete the moment they are written.
pub fn calendar_snapshot_view(root: &Path, now: NaiveDateTime) -> CalendarView {
    let path = root
        .join(".casa")
        .join("calendar")
        .join("synced-events.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return CalendarView::Blind;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return CalendarView::Blind;
    };
    let feed_configured = json.get("feed").and_then(|v| v.as_str()) != Some("none");
    if feed_configured {
        // A configured feed must have been CONFIRMED recently for this snapshot to speak for it.
        let Some(fetched_ms) = json.get("fetchedAt").and_then(|v| v.as_i64()) else {
            return CalendarView::Blind;
        };
        let Some(fetched) = chrono::DateTime::from_timestamp_millis(fetched_ms) else {
            return CalendarView::Blind;
        };
        // `now` is household LOCAL civil time; `fetched` is an absolute instant. Interpreting
        // the first as UTC (`now.and_utc()`) compares two different clocks and is wrong by the
        // zone offset — on this box, -4h, which read as a snapshot from the future and blinded
        // a calendar that had just been written. The unit tests could not see it: they built
        // `fetchedAt` with the same mistaken conversion, so the error cancelled on both sides.
        // The live-house probe is what caught it.
        let Some(now_abs) = Local
            .from_local_datetime(&now)
            .earliest()
            .map(|t| t.to_utc())
        else {
            return CalendarView::Blind;
        };
        let age = now_abs.signed_duration_since(fetched).num_seconds();
        // A stamp from the FUTURE is a clock disagreement between the two processes, not
        // freshness; treat it as trustworthy only within the same window.
        if age > CALENDAR_SNAPSHOT_MAX_AGE_SECS || age < -CALENDAR_SNAPSHOT_MAX_AGE_SECS {
            return CalendarView::Blind;
        }
    }
    let today = now.date();
    let mut titles = Vec::new();
    for e in json
        .get("events")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        let Some(start) = e.get("start").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(when) = chrono::DateTime::parse_from_rfc3339(start) else {
            continue;
        };
        // TO HOUSEHOLD LOCAL TIME, not to the offset the string happened to carry. The gateway
        // writes `new Date(...).toISOString()`, which is always UTC with a `Z`, while `now`
        // here is `chrono::Local` civil time (the same clock the plan's own dates are in). A
        // bare `naive_local()` on the parsed value keeps the +00:00 offset, so a 5pm pickup in
        // a UTC-4 house would read as 9pm — the "has it passed?" test and the day boundary
        // would both be wrong by the offset, silently.
        let local = when.with_timezone(&chrono::Local).naive_local();
        if local.date() != today {
            continue;
        }
        // An all-day entry has no clock to have passed; a timed one that is over is not
        // "what's on today" any more, matching how the plan's own rows are filtered.
        let all_day = e.get("allDay").and_then(|v| v.as_bool()).unwrap_or(false);
        if !all_day && local < now {
            continue;
        }
        let title = e
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let stamped = if all_day {
            title
        } else {
            format!("{} ({})", title, local.format("%-I:%M%P"))
        };
        if !titles.contains(&stamped) {
            titles.push(stamped);
        }
    }
    CalendarView::Visible { titles }
}

/// Load the current week model under `root` and render [`schedule_context_line`]
/// for it as of `now`. Best-effort; a missing plan yields the empty-calendar
/// (strict) truth line so the model is still told the day is clear.
///
/// The household's REAL calendar is folded in here (see [`calendar_snapshot_view`]): when the
/// gateway's snapshot is fresh, its events are named alongside the plan's and the hedge is
/// dropped, because there is no longer anything this process cannot see. When it is missing or
/// stale the hedge stands, which is the same honest answer as before.
pub fn fetch_schedule_context_line(root: &Path, now: NaiveDateTime) -> String {
    let plans = family_plan::load_plans(root);
    let doc = family_plan::current_plan(&plans, now.date());
    match calendar_snapshot_view(root, now) {
        CalendarView::Visible { titles } => {
            schedule_context_line_with_calendar(doc, now, &titles)
        }
        CalendarView::Blind => {
            schedule_context_line_scoped(doc, now, external_calendar_configured(root))
        }
    }
}

/// [`schedule_context_line`] for a house whose real calendar IS readable here: the plan's rows
/// and the calendar's are one list, and an empty list means the day is genuinely clear rather
/// than merely unseen. No hedge — hedging while holding the answer is its own small dishonesty.
pub fn schedule_context_line_with_calendar(
    doc: Option<&PlanDoc>,
    now: NaiveDateTime,
    calendar_titles: &[String],
) -> String {
    let today = now.date();
    let label = format!(
        "{} {}",
        family_plan::long_weekday(today),
        today.format("%b %-d")
    );
    let mut titles = doc
        .map(|d| upcoming_titles_on(d, today, now))
        .unwrap_or_default();
    for t in calendar_titles {
        // The plan and the calendar can carry the same commitment; say it once.
        let already = titles.iter().any(|existing| {
            let a = existing.to_lowercase();
            let b = t.to_lowercase();
            a.contains(&b) || b.contains(&a)
        });
        if !already {
            titles.push(t.clone());
        }
    }
    if titles.is_empty() {
        format!(
            "CALENDAR ({label}) — there is NOTHING on the calendar today, and this includes \
             the household's linked calendar, which IS visible to you here. Do NOT invent a \
             meeting, appointment, birthday, or any event, and do NOT say the day is \
             busy/packed/back-to-back. If asked, say the calendar is clear.\n"
        )
    } else {
        format!(
            "CALENDAR ({label}) — the ONLY real events today are: {}. This list already \
             includes the household's linked calendar, so it is COMPLETE: mention ONLY these, \
             do NOT invent any other meeting, appointment, or birthday, do NOT tell the family \
             you cannot see their calendar, and only call the day busy/packed if there are \
             genuinely several.\n",
            titles.join("; ")
        )
    }
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
// the scoped family-reply sink in the ENGINE process, so the guard MUST live here.
// ---------------------------------------------------------------------------

static HISTORICAL_WEEK_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^Requested historical dinner plan \((\d{4}-\d{2}-\d{2}) through (\d{4}-\d{2}-\d{2})\),$",
    )
    .expect("valid historical-week header regex")
});

static HISTORICAL_WEEK_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^- (Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday) \(([^()\n]+)\): (.+)$",
    )
    .expect("valid historical-week row regex")
});

static HISTORICAL_MONTH_DAY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\.?\s+(\d{1,2})(?:st|nd|rd|th)?$",
    )
    .expect("valid historical month-day regex")
});

static HISTORICAL_WEEK_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:(?:give|show)\s+me(?:\s+all)?(?:\s+the)?|list(?:\s+all)?(?:\s+the)?)\s+(january|february|march|april|may|june|july|august|september|sept|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|oct|nov|dec)\.?\s+(\d{1,2})(?:st|nd|rd|th)?\s*(?:-|–|—|through|to)\s*(?:(january|february|march|april|may|june|july|august|september|sept|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|oct|nov|dec)\.?\s+)?(\d{1,2})(?:st|nd|rd|th)?(?:\s*,?\s*(\d{4}))?\s+(?:plan(?:['’]s)?\s+)?dinners?\s+in\s+(?:(?:date|chronological)\s+)?order\s*[.!?]*$",
    )
    .expect("valid historical-week request regex")
});

// RUN-3 C075 (2026-07-31): a dated whole-week training SUMMARY is still a
// composed lane, but it is a READ, not a request to create a background task.
// Keep the grammar in lock-step with claw3d-bridge's typed detector. The exact
// request recognition is deliberately independent of the context parser so a
// recognized request with missing/malformed context can fail closed instead of
// falling back to composer prose (the live failure created a real TASK_CREATE).
static HISTORICAL_WORKOUT_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^summarize\s+(?:the\s+)?(january|february|march|april|may|june|july|august|september|sept|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|oct|nov|dec)\.?\s+(\d{1,2})(?:st|nd|rd|th)?\s*(?:-|–|—|through|to)\s*(?:(january|february|march|april|may|june|july|august|september|sept|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|oct|nov|dec)\.?\s+)?(\d{1,2})(?:st|nd|rd|th)?(?:\s*,?\s*(\d{4}))?\s+(?:training|workouts?)\s*[.!?]*$",
    )
    .expect("valid historical-workout request regex")
});

static HISTORICAL_WORKOUT_WEEK_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^week_key=(\d{4})-W(\d{2})$").expect("valid historical-workout week-key regex")
});

static HISTORICAL_WORKOUT_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^row=(\d{4}-\d{2}-\d{2})\|(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\|([01]\d|2[0-3]):([0-5]\d)\|([^|\r\n]{1,160})$",
    )
    .expect("valid historical-workout row regex")
});

static STANDALONE_AMPERSAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+&\s+").expect("valid standalone ampersand regex"));

static HISTORICAL_NO_COOK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bno\s+cooking\b").expect("valid no-cook regex"));

static HISTORICAL_OUT_CONTRACTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?P<subject>\p{L}[\p{L}-]*)['’]s\s+out\b")
        .expect("valid historical out-contraction regex")
});

static HISTORICAL_THIS_PERIOD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bthis\s+(morning|afternoon|evening|night)\b")
        .expect("valid historical period regex")
});

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoricalDinnerRow {
    day: String,
    date: NaiveDate,
    dish: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoricalWorkoutRow {
    day: String,
    time: NaiveTime,
    title: String,
}

/// Tri-state result for the C075 guard. `NotApplicable` preserves every other
/// composed turn. `InvalidContext` is intentionally distinct from it: once the
/// exact dated training-summary grammar is recognized, missing or malformed
/// gateway evidence must never fall back to model prose or task creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoricalWorkoutReply {
    NotApplicable,
    Grounded(String),
    InvalidContext,
}

pub(crate) const HISTORICAL_WORKOUT_REFUSAL: &str =
    "I couldn't verify that historical training week safely.";

fn historical_month_number(value: &str) -> Option<u32> {
    match value
        .chars()
        .take(3)
        .collect::<String>()
        .to_ascii_lowercase()
        .as_str()
    {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

fn historical_month_name(month: u32) -> Option<&'static str> {
    match month {
        1 => Some("January"),
        2 => Some("February"),
        3 => Some("March"),
        4 => Some("April"),
        5 => Some("May"),
        6 => Some("June"),
        7 => Some("July"),
        8 => Some("August"),
        9 => Some("September"),
        10 => Some("October"),
        11 => Some("November"),
        12 => Some("December"),
        _ => None,
    }
}

fn historical_request_matches_range(human_message: &str, start: NaiveDate, end: NaiveDate) -> bool {
    let compact = human_message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let Some(captures) = HISTORICAL_WEEK_REQUEST_RE.captures(&compact) else {
        return false;
    };
    let Some(start_month) = captures
        .get(1)
        .and_then(|value| historical_month_number(value.as_str()))
    else {
        return false;
    };
    let Some(start_day) = captures
        .get(2)
        .and_then(|value| value.as_str().parse::<u32>().ok())
    else {
        return false;
    };
    let end_month = captures
        .get(3)
        .and_then(|value| historical_month_number(value.as_str()))
        .unwrap_or(start_month);
    let Some(end_day) = captures
        .get(4)
        .and_then(|value| value.as_str().parse::<u32>().ok())
    else {
        return false;
    };
    if let Some(year) = captures
        .get(5)
        .and_then(|value| value.as_str().parse::<i32>().ok())
        && (start.year() != year || end.year() != year)
    {
        return false;
    }
    start.month() == start_month
        && start.day() == start_day
        && end.month() == end_month
        && end.day() == end_day
}

fn historical_workout_request_matches_range(
    human_message: &str,
    start: NaiveDate,
    end: NaiveDate,
) -> bool {
    let compact = human_message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let Some(captures) = HISTORICAL_WORKOUT_REQUEST_RE.captures(&compact) else {
        return false;
    };
    let Some(start_month) = captures
        .get(1)
        .and_then(|value| historical_month_number(value.as_str()))
    else {
        return false;
    };
    let Some(start_day) = captures
        .get(2)
        .and_then(|value| value.as_str().parse::<u32>().ok())
    else {
        return false;
    };
    let end_month = captures
        .get(3)
        .and_then(|value| historical_month_number(value.as_str()))
        .unwrap_or(start_month);
    let Some(end_day) = captures
        .get(4)
        .and_then(|value| value.as_str().parse::<u32>().ok())
    else {
        return false;
    };
    if let Some(year) = captures
        .get(5)
        .and_then(|value| value.as_str().parse::<i32>().ok())
        && (start.year() != year || end.year() != year)
    {
        return false;
    }
    start.month() == start_month
        && start.day() == start_day
        && end.month() == end_month
        && end.day() == end_day
}

fn exact_historical_workout_rows(
    human_message: &str,
    week_context: &str,
    local_date: NaiveDate,
) -> Option<(NaiveDate, NaiveDate, Vec<HistoricalWorkoutRow>)> {
    let lines: Vec<&str> = week_context
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != 9
        || lines.first().copied() != Some("WG_HISTORICAL_WORKOUT_CONTEXT_V1")
        || lines.last().copied() != Some("END_WG_HISTORICAL_WORKOUT_CONTEXT_V1")
    {
        return None;
    }

    let week_key = HISTORICAL_WORKOUT_WEEK_KEY_RE.captures(lines[1])?;
    let week_year = week_key.get(1)?.as_str().parse::<i32>().ok()?;
    let week_number = week_key.get(2)?.as_str().parse::<u32>().ok()?;
    let start = lines[2]
        .strip_prefix("range_start=")
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())?;
    let end = lines[3]
        .strip_prefix("range_end=")
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())?;
    let current_week_monday = local_date.checked_sub_days(chrono::Days::new(
        local_date.weekday().num_days_from_monday().into(),
    ))?;
    let start_iso = start.iso_week();
    let end_iso = end.iso_week();
    if start.weekday() != Weekday::Mon
        || end.weekday() != Weekday::Sun
        || end.signed_duration_since(start).num_days() != 6
        || end >= current_week_monday
        || start_iso.year() != week_year
        || start_iso.week() != week_number
        || end_iso.year() != week_year
        || end_iso.week() != week_number
        || !historical_workout_request_matches_range(human_message, start, end)
    {
        return None;
    }

    let mut seen_dates = HashSet::new();
    let mut rows = Vec::with_capacity(4);
    let mut prior_date = None;
    for line in &lines[4..8] {
        let captures = HISTORICAL_WORKOUT_ROW_RE.captures(line)?;
        let date = NaiveDate::parse_from_str(captures.get(1)?.as_str(), "%Y-%m-%d").ok()?;
        let day = captures.get(2)?.as_str().to_string();
        let hour = captures.get(3)?.as_str().parse::<u32>().ok()?;
        let minute = captures.get(4)?.as_str().parse::<u32>().ok()?;
        let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
        let title = captures
            .get(5)?
            .as_str()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if date < start
            || date > end
            || family_plan::long_weekday(date).to_ascii_lowercase() != day
            || !seen_dates.insert(date)
            || prior_date.is_some_and(|prior| date <= prior)
            || title.is_empty()
        {
            return None;
        }
        prior_date = Some(date);
        rows.push(HistoricalWorkoutRow { day, time, title });
    }
    (rows.len() == 4).then_some((start, end, rows))
}

fn historical_count_word(count: usize) -> Option<&'static str> {
    match count {
        0 => Some("zero"),
        1 => Some("one"),
        2 => Some("two"),
        3 => Some("three"),
        4 => Some("four"),
        _ => None,
    }
}

fn historical_workout_range_label(start: NaiveDate, end: NaiveDate) -> Option<String> {
    let start_month = historical_month_name(start.month())?;
    if start.month() == end.month() && start.year() == end.year() {
        Some(format!("{start_month} {}-{}", start.day(), end.day()))
    } else {
        Some(format!(
            "{start_month} {}-{} {}",
            start.day(),
            historical_month_name(end.month())?,
            end.day(),
        ))
    }
}

fn historical_workout_title(title: &str) -> Option<String> {
    let folded = title
        .trim_matches(&['.', '!', '?'][..])
        .replace(['(', ')'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if folded.is_empty() {
        return None;
    }
    // "Lower (strength)" uses strength as a redundant category qualifier;
    // the week-shape clause already calls these lifting sessions. Other
    // qualifiers remain visible, so mutating the source changes the final.
    Some(if folded == "lower strength" {
        "lower".to_string()
    } else {
        folded
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoricalWorkoutKind {
    Lifting,
    ActiveRecovery,
    Other,
}

/// Classify only workout titles with positive evidence. The C075 source uses
/// conventional lifting split labels (`Lower (strength)`, `Upper (push)`, and
/// `Upper (pull)`), while recovery names itself. Everything else remains
/// faithfully visible in the row enumeration but must not be relabelled as
/// lifting merely because it is not recovery (for example a tempo run or
/// putting practice).
fn historical_workout_kind(title: &str) -> HistoricalWorkoutKind {
    let words: Vec<String> = title
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.iter().any(|word| word == "recovery") {
        return HistoricalWorkoutKind::ActiveRecovery;
    }
    let explicit_lifting = words.iter().any(|word| {
        matches!(
            word.as_str(),
            "strength" | "lifting" | "weightlifting" | "weights" | "resistance"
        )
    });
    let split_lifting = matches!(
        words.first().map(String::as_str),
        Some("upper") | Some("lower")
    ) && words
        .iter()
        .any(|word| matches!(word.as_str(), "push" | "pull"));
    if explicit_lifting || split_lifting {
        HistoricalWorkoutKind::Lifting
    } else {
        HistoricalWorkoutKind::Other
    }
}

fn historical_workout_time(time: NaiveTime) -> String {
    let hour = time.hour();
    let display_hour = hour % 12;
    let display_hour = if display_hour == 0 { 12 } else { display_hour };
    let period = if hour < 12 { "a.m." } else { "p.m." };
    if time.minute() == 0 {
        format!("{display_hour} {period}")
    } else {
        format!("{display_hour}:{:02} {period}", time.minute())
    }
}

/// Deterministically format a composed historical whole-week workout summary
/// from the gateway's exact, validated row protocol. The composer still runs
/// (and may emit the normal engine acknowledgement), but its prose and any
/// hidden `TASK_CREATE` are discarded before promise/action auditing.
///
/// Unlike the older dinner helper's `Option`, this is tri-state. Once the exact
/// dated-summary request is recognized, absent/malformed/current-week context is
/// `InvalidContext`, never ordinary model fallback: the live C075 miss otherwise
/// turned the word "Summarize" into a real background task and household writes.
pub(crate) fn historical_week_workout_reply(
    human_message: &str,
    week_context: &str,
    local_date: NaiveDate,
) -> HistoricalWorkoutReply {
    let compact = human_message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !HISTORICAL_WORKOUT_REQUEST_RE.is_match(&compact) {
        return HistoricalWorkoutReply::NotApplicable;
    }
    let Some((start, end, rows)) =
        exact_historical_workout_rows(human_message, week_context, local_date)
    else {
        return HistoricalWorkoutReply::InvalidContext;
    };

    let kinds: Vec<HistoricalWorkoutKind> = rows
        .iter()
        .map(|row| historical_workout_kind(&row.title))
        .collect();
    let lifting_count = kinds
        .iter()
        .filter(|kind| **kind == HistoricalWorkoutKind::Lifting)
        .count();
    let recovery_count = kinds
        .iter()
        .filter(|kind| **kind == HistoricalWorkoutKind::ActiveRecovery)
        .count();
    let Some(range) = historical_workout_range_label(start, end) else {
        return HistoricalWorkoutReply::InvalidContext;
    };
    let shape = if kinds
        .iter()
        .all(|kind| *kind != HistoricalWorkoutKind::Other)
    {
        let Some(lifting_word) = historical_count_word(lifting_count) else {
            return HistoricalWorkoutReply::InvalidContext;
        };
        let Some(recovery_word) = historical_count_word(recovery_count) else {
            return HistoricalWorkoutReply::InvalidContext;
        };
        let lifting_noun = if lifting_count == 1 {
            "lifting session"
        } else {
            "lifting sessions"
        };
        let recovery_noun = if recovery_count == 1 {
            "active recovery"
        } else {
            "active recovery sessions"
        };
        format!(
            "The {range} training had {lifting_word} {lifting_noun} and {recovery_word} {recovery_noun}."
        )
    } else {
        let Some(session_word) = historical_count_word(rows.len()) else {
            return HistoricalWorkoutReply::InvalidContext;
        };
        let session_noun = if rows.len() == 1 {
            "session"
        } else {
            "sessions"
        };
        format!("The {range} training had {session_word} {session_noun}.")
    };

    let mut sessions = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(title) = historical_workout_title(&row.title) else {
            return HistoricalWorkoutReply::InvalidContext;
        };
        sessions.push(format!(
            "{} {title} at {}",
            capitalize_weekday(&row.day),
            historical_workout_time(row.time),
        ));
    }
    let days = match sessions.as_slice() {
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let (last, initial) = sessions.split_last().expect("four validated rows");
            format!("{}, and {last}", initial.join(", "))
        }
    };
    // Every formatted clock ends in the a.m./p.m. abbreviation's period, so
    // the day clause is already sentence-terminal. Appending another period
    // would produce the live-visible `a.m..` typo.
    HistoricalWorkoutReply::Grounded(format!("{shape} {days}"))
}

fn historical_row_date(label: &str, year: i32) -> Option<NaiveDate> {
    let captures = HISTORICAL_MONTH_DAY_RE.captures(label.trim())?;
    let month = historical_month_number(captures.get(1)?.as_str())?;
    let day = captures.get(2)?.as_str().parse::<u32>().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn exact_historical_dinner_rows(
    human_message: &str,
    week_context: &str,
    local_date: NaiveDate,
) -> Option<Vec<HistoricalDinnerRow>> {
    let lines: Vec<&str> = week_context
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let headers: Vec<_> = lines
        .iter()
        .filter_map(|line| HISTORICAL_WEEK_HEADER_RE.captures(line))
        .collect();
    if headers.len() != 1
        || lines
            .iter()
            .any(|line| line.starts_with("Today is ") || line.starts_with("Tomorrow is "))
        || lines.iter().any(|line| line.starts_with("\u{2022} "))
    {
        return None;
    }
    let header = &headers[0];
    let start = NaiveDate::parse_from_str(header.get(1)?.as_str(), "%Y-%m-%d").ok()?;
    let end = NaiveDate::parse_from_str(header.get(2)?.as_str(), "%Y-%m-%d").ok()?;
    let current_week_monday = local_date.checked_sub_days(chrono::Days::new(
        local_date.weekday().num_days_from_monday().into(),
    ))?;
    if start.weekday() != Weekday::Mon
        || end.weekday() != Weekday::Sun
        || end.signed_duration_since(start).num_days() != 6
        || end >= current_week_monday
        || !historical_request_matches_range(human_message, start, end)
    {
        return None;
    }

    let row_lines: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.starts_with("- "))
        .collect();
    if row_lines.len() != 7 {
        return None;
    }
    const ORDER: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let mut seen_days = HashSet::new();
    let mut seen_dates = HashSet::new();
    let mut rows = Vec::with_capacity(7);
    for (index, line) in row_lines.into_iter().enumerate() {
        let captures = HISTORICAL_WEEK_ROW_RE.captures(line)?;
        let day = captures.get(1)?.as_str().to_ascii_lowercase();
        let expected = start.checked_add_days(chrono::Days::new(index as u64))?;
        let date = historical_row_date(captures.get(2)?.as_str(), expected.year())?;
        let dish = captures.get(3)?.as_str().trim();
        if day != ORDER[index]
            || date != expected
            || !seen_days.insert(day.clone())
            || !seen_dates.insert(date)
            || dish.is_empty()
            || dish.eq_ignore_ascii_case("not planned yet")
        {
            return None;
        }
        rows.push(HistoricalDinnerRow {
            day,
            date,
            dish: dish.to_string(),
        });
    }
    Some(rows)
}

fn historical_no_cook_detail(dish: &str) -> Option<String> {
    let without_status = HISTORICAL_NO_COOK_RE.replace_all(dish, "");
    let detail = without_status
        .trim_matches(|character: char| {
            character.is_whitespace()
                || matches!(
                    character,
                    '—' | '–' | '-' | ',' | ';' | ':' | '/' | '|' | '.' | '!' | '?'
                )
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() || detail.eq_ignore_ascii_case("out") {
        return None;
    }
    let detail = HISTORICAL_OUT_CONTRACTION_RE.replace_all(&detail, "${subject} was out");
    let detail = HISTORICAL_THIS_PERIOD_RE.replace_all(&detail, "that $1");
    let detail = detail
        .trim_end_matches(&['.', '!', '?'][..])
        .trim()
        .to_string();
    (!detail.is_empty()).then_some(detail)
}

/// Deterministically format a composed historical whole-week dinner read from the
/// gateway's exact, already-selected seven-row block. This is intentionally NOT an
/// instant read: the caller still runs the real composer and acknowledgement/edit
/// lifecycle, then uses these data-fed clauses as the final family-visible bytes.
///
/// The capability is the conjunction of the narrow dated request and the validated
/// historical block. Any missing, duplicate, reordered, misdated, current-or-future
/// week, or mismatched-range input returns `None`; the ordinary composed reply path
/// remains in force and no partial rows are exposed as an authoritative answer.
pub(crate) fn historical_week_dinner_reply(
    human_message: &str,
    week_context: &str,
    local_date: NaiveDate,
) -> Option<String> {
    let rows = exact_historical_dinner_rows(human_message, week_context, local_date)?;
    let clauses = rows
        .into_iter()
        .map(|row| {
            let dish = STANDALONE_AMPERSAND_RE
                .replace_all(&row.dish, " and ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches(&['.', '!', '?'][..])
                .to_string();
            let low = dish.to_lowercase();
            let is_out = low
                .split(|character: char| !character.is_alphanumeric())
                .any(|word| word == "out");
            if HISTORICAL_NO_COOK_RE.is_match(&low) && is_out {
                let month = historical_month_name(row.date.month())?;
                let prefix = format!(
                    "{}, {month} {} was out, no cooking",
                    capitalize_weekday(&row.day),
                    row.date.day(),
                );
                match historical_no_cook_detail(&dish) {
                    Some(detail) => Some(format!("{prefix} — {detail}.")),
                    None => Some(format!("{prefix}.")),
                }
            } else {
                Some(format!("{} was {dish}.", capitalize_weekday(&row.day)))
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some(clauses.join(" "))
}

/// The parsed `WG_WEEK_CONTEXT`: which weekdays have a planned dinner (keyed by
/// lowercase full weekday name → dish text), plus which weekday "today" and
/// "tomorrow" resolve to (so a relative-day empty-claim — "nothing for tomorrow"
/// — can be checked against the real plan). A day whose dish is blank or the
/// sentinel "not planned yet" is NOT recorded as planned.
#[derive(Debug, Default, Clone)]
pub struct WeekContext {
    by_day: std::collections::HashMap<String, String>,
    /// The LUNCH the plan gives a weekday, when it states one (task
    /// meal-read-lane). The gateway used to forward the Dinners table alone, so a
    /// lunch question reached the composer with nothing to answer from and the
    /// wrong-slot guard had to treat EVERY lunch claim as wrong. Both are now
    /// answerable: this map is the plan's own lunches, keyed like `by_day`.
    lunch_by_day: std::collections::HashMap<String, String>,
    /// Weekdays the plan marks as no-cook (out / takeaway / leftovers). Not dishes:
    /// a sentence about cooking must never be "named" by one and rewritten.
    non_cook: std::collections::HashSet<String>,
    /// The dinners the plan has LINED UP but the family has NOT agreed to — the ⏳
    /// rows (task engine-twin-grounding, docs/20 §6 rule 12). Held in the block's own
    /// order so a multi-day correction names the days the way the plan lists them.
    /// Deliberately NOT merged into `by_day`: the "- " dinner row already carries the
    /// dish, and a pending row is a QUALIFIER on that dinner, never a second one.
    pending: Vec<PendingWeekRow>,
    today: Option<String>,
    tomorrow: Option<String>,
}

/// One dinner the plan has lined up while the family has not said yes to it: the
/// `\u{2022} Awaiting the family's OK \u{2014} Friday (Jul 24): Salmon\u{2026}` row the gateway
/// states in words (`weekSource.buildWeekContext`). The JS twin's `pendingRowsOf`
/// shape, field for field, so the two guards reason over the same rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWeekRow {
    /// Capitalised weekday ("Friday") — the shape the truth line states.
    pub day: String,
    /// The slot the row is for. The gateway states pending DINNERS, so this is
    /// "dinner"; it is carried explicitly because the truth line names it and the
    /// JS twin's rows carry a slot too.
    pub slot: String,
    /// The dish exactly as the plan spells it.
    pub dish: String,
}

impl WeekContext {
    /// No day carries a planned dish in ANY slot — the guard is a no-op.
    pub fn is_empty(&self) -> bool {
        self.by_day.is_empty() && self.lunch_by_day.is_empty()
    }

    /// Every (weekday, dish, slot) the plan gives a dish, dinners first. No-cook
    /// nights are excluded on purpose: "no cooking \u{2014} we're out this evening" is not
    /// a dish, and treating it as one lets an ordinary sentence about cooking be
    /// "named" by it and rewritten.
    fn planned_slots(&self) -> Vec<(&String, &String, &'static str)> {
        let mut out: Vec<(&String, &String, &'static str)> = self
            .by_day
            .iter()
            .filter(|(day, _)| !self.non_cook.contains(*day))
            .map(|(day, dish)| (day, dish, "dinner"))
            .collect();
        out.extend(
            self.lunch_by_day
                .iter()
                .map(|(day, dish)| (day, dish, "lunch")),
        );
        out
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
        // The slot-complete block (task meal-read-lane) adds "\u{2022} "-bulleted lines
        // BESIDE the unchanged "- " dinner rows: a lunch the plan names, and a night
        // with no cooking. A "- " line is still a DINNER and nothing else, so an
        // engine build that predates this still reads exactly the map it always did.
        if let Some(rest) = line.strip_prefix("\u{2022} ") {
            // \u{23f3} PENDING DINNERS (task engine-twin-grounding, docs/20 §6 rule 12). The
            // gateway states each unagreed dinner on its own bulleted line, LEADING with
            // "Awaiting the family's OK \u{2014} " — which is precisely why the generic branch
            // below cannot read it (the head before the colon holds no weekday) and so why
            // the gateway could ship its half alone without changing what any engine
            // grounded on. Read now, as a QUALIFIER on the "- " dinner row: `by_day` is
            // untouched, because the dish is already there and a proposal is not a second
            // dinner.
            if let Some(row) = strip_pending_prefix(rest) {
                if let Some((left, dish)) = row.split_once(':') {
                    let day = left.split('(').next().unwrap_or(left).trim().to_lowercase();
                    let dish = dish.trim();
                    if weekday_token(&day).is_some()
                        && !dish.is_empty()
                        && !dish.eq_ignore_ascii_case("not planned yet")
                    {
                        let day = capitalize_weekday(&day);
                        if !wc.pending.iter().any(|p| p.day == day) {
                            wc.pending.push(PendingWeekRow {
                                day,
                                slot: "dinner".to_string(),
                                dish: dish.to_string(),
                            });
                        }
                    }
                }
                continue;
            }
            if let Some((left, dish)) = rest.split_once(':') {
                let head = left.trim().to_lowercase();
                let day = head.split('(').next().unwrap_or(&head).trim().to_string();
                let dish = dish.trim();
                if weekday_token(&day).is_some() && !dish.is_empty() {
                    if head.ends_with("lunch") {
                        wc.lunch_by_day.insert(day, dish.to_string());
                    } else if dish.to_lowercase().contains("no cooking") {
                        wc.non_cook.insert(day);
                    }
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            // "Saturday (July 25): Baked white fish" → day, dish.
            if let Some((left, dish)) = rest.split_once(':') {
                let day = left.split('(').next().unwrap_or(left).trim().to_lowercase();
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

/// The lead phrase the gateway puts on a \u{23f3} pending-dinner row, byte-identical to
/// `weekSource.buildWeekContext`'s template. Both apostrophe forms are accepted so a
/// typographic pass on the gateway copy cannot silently mute the guard.
const PENDING_ROW_LEADS: &[&str] = &[
    "Awaiting the family's OK \u{2014} ",
    "Awaiting the family\u{2019}s OK \u{2014} ",
];

/// The `Friday (Jul 24): dish` remainder of a pending row, or `None` when this
/// bulleted line is not one (a lunch row, a no-cook night, anything later).
fn strip_pending_prefix(rest: &str) -> Option<&str> {
    PENDING_ROW_LEADS
        .iter()
        .find_map(|lead| rest.strip_prefix(lead))
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
        "THIS WEEK'S MEALS — the family's real plan, parsed from the plan file. \
         Answer any meal question (today, tonight, tomorrow, or a named day; DINNER or \
         LUNCH) FROM this list, never from a plan's prose notes or a week \"skeleton\", \
         and never from another day's row. If a day below has an entry, that day IS \
         planned — NEVER say it is empty, unplanned, not locked in, not set, or \
         undecided. Keep the SLOT the person asked about: a lunch question is answered \
         with that day's lunch, never with its dinner. A night marked as no cooking is \
         the answer for that day — say so plainly, never substitute another day's dish:\n",
    );
    out.push_str(text);
    out.push('\n');
    Some(out)
}

// Negation tokens that, alongside a planning word, mark a sentence as asserting
// nothing is planned. Matched as whole words against the NORMALISED sentence
// (apostrophes dropped, so "isn't"→"isnt", "nothing's"→"nothings").
const WEEK_EMPTY_NEGATIONS: &[&str] = &[
    "nothing",
    "nothings",
    "no",
    "not",
    "none",
    "nope",
    "nada",
    "havent",
    "hasnt",
    "hadnt",
    "dont",
    "doesnt",
    "didnt",
    "isnt",
    "arent",
    "wasnt",
    "werent",
    "cant",
    "wont",
    "unplanned",
    "undecided",
    "tbd",
    "blank",
    "empty",
];

// Planning-status stems: a normalised token STARTING with any of these, in a
// sentence that also carries a negation, means "no plan for the meal". Stems (not
// whole words) so "planned/planning", "locked", "scheduled", "decided",
// "cooking", "figured", "eating" all match.
const WEEK_PLAN_STEMS: &[&str] = &[
    "plan", "lock", "schedul", "set", "settl", "decid", "menu", "figur", "nail", "sort", "line",
    "dinner", "supper", "meal", "cook", "eat", "mak", "food",
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
        let hit = sentences
            .iter()
            .any(|s| sentence_claims_empty(s) && terms.iter().any(|t| norm_has_word(s, t)));
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
// WRONG-PLACEMENT WEEK CLAIMS (task meal-claim-slot) — live-cert finding C004.
//
// THE GAP. The never-claim-empty guard above covers ONE lie direction: "nothing's
// planned" for a day the Dinners table fills. C004 is the OTHER direction, and it
// slipped straight through: Bruno told Luca to enjoy "that frittata tomorrow …
// for lunch" while the live plan had that frittata on SUNDAY, at DINNER. The dish
// was real, no day was called empty, nothing was invented — the claim simply MOVED
// the dish: wrong day AND wrong slot. The family reads that as fact and eats the
// wrong meal on the wrong day.
//
// THE RULE. When a reply asserts dish X at day D or slot S, and the forwarded
// `WG_WEEK_CONTEXT` carries X at a DIFFERENT day (or the table's only slot is
// dinner and the reply says lunch/breakfast/brunch/snack), the offending SENTENCE
// is replaced with the truthful placement. Everything else in the reply survives —
// this is a surgical splice, not a whole-reply clobber, because the rest of the
// reply is usually fine and the family should not lose it over one bad clause.
//
// WHAT MUST PASS UNTOUCHED (the guard's blast radius is the reason it is safe):
//   * a CASUAL mention with no day and no slot claim — "the frittata was great" —
//     asserts no placement, so there is nothing to contradict;
//   * a TRUTHFUL placement — "Sunday's frittata" — the claimed day IS the real one;
//   * a LEFTOVERS line — "the leftover frittata for lunch" — that is a claim about
//     eating leftovers, not about where the plan puts the meal;
//   * an AMBIGUOUS mention — the matched words fit two different days' dishes, so
//     no single truth can be named;
//   * a PROPOSAL or a QUESTION — "want me to put the frittata on Wednesday?", "we
//     could do it again next week" — which asserts nothing about this week's table,
//     so "correcting" it would clobber the cook's own offer;
//   * generic prep/filler overlap ("keep Wednesday warm and easy" against a dish
//     with "warm" in it) — filler words never identify a dish.
// ---------------------------------------------------------------------------

/// Dish words that identify NO dish: prep verbs, textures, serving nouns, meal
/// words, and ordinary filler. A sentence matching only these has not named the
/// dish, so it can never trip the placement guard (the false-positive floor).
const DISH_FILLER_WORDS: &[&str] = &[
    "with",
    "over",
    "under",
    "onto",
    "into",
    "from",
    "plus",
    "and",
    "the",
    "for",
    "warm",
    "warmed",
    "cold",
    "chilled",
    "easy",
    "quick",
    "simple",
    "fresh",
    "leftover",
    "leftovers",
    "night",
    "nights",
    "style",
    "some",
    "that",
    "this",
    "then",
    "made",
    "make",
    "sheet",
    "tray",
    "oven",
    "stove",
    "pan",
    "bowl",
    "plate",
    "side",
    "sides",
    "served",
    "serve",
    "extra",
    "homemade",
    "dinner",
    "dinners",
    "supper",
    "lunch",
    "breakfast",
    "brunch",
    "snack",
    "meal",
    "meals",
    "food",
    "mins",
    "minutes",
];

/// Non-dinner meal-slot stems and the label to report them by. The forwarded week
/// context is the DINNERS table, so a dish found there placed at any of these
/// slots is a wrong-slot claim. `dinner`/`supper` are deliberately absent — they
/// are the truthful slot.
const MEAL_SLOT_STEMS: &[(&str, &str)] = &[
    ("breakfast", "breakfast"),
    ("brunch", "brunch"),
    ("lunch", "lunch"),
    ("midday", "midday"),
    ("snack", "snack"),
    // dinner/supper are here since task meal-read-lane: with LUNCHES in the map the
    // test is no longer "is this slot dinner?" but "is this the slot the plan gives
    // this dish?", so a LUNCH dish claimed at dinner is a wrong-slot claim too.
    ("dinner", "dinner"),
    ("supper", "dinner"),
];

/// Cues that a sentence is about eating LEFTOVERS rather than asserting where the
/// plan puts a meal. "The leftover frittata makes a good lunch" is true and must
/// not be rewritten.
const LEFTOVER_CUES: &[&str] = &["leftover", "leftovers", "left over", "reheat", "reheated"];

/// Cues that a sentence PROPOSES or supposes a placement rather than ASSERTING one.
/// A cook offering "want me to put the frittata on Wednesday?" or "we could do the
/// frittata again next week" states no fact about this week's table, so correcting it
/// would clobber the offer. Only assertive sentences are claims. (A question mark is
/// handled separately — see `SpannedSentence::asks`.)
const PLACEMENT_PROPOSAL_PHRASES: &[&str] = &[
    "want me to",
    "want to",
    "should i",
    "should we",
    "shall i",
    "shall we",
    "how about",
    "what about",
    "we could",
    "i could",
    "we can",
    "i can",
    "we might",
    "let me",
    "next week",
    "if you",
];

/// Single-word proposal cues, matched as WHOLE words (not substrings) so a dish like
/// "pork cutlets" cannot exempt itself by containing "lets".
const PLACEMENT_PROPOSAL_WORDS: &[&str] = &[
    "maybe", "lets", "instead", "moving", "move", "swap", "swapping", "again", "could", "might",
    "would",
];

/// A misplaced-placement claim: the dish, where the plan REALLY puts it, what the
/// draft claimed instead, and the byte span of the offending sentence in the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MisplacedWeekClaim {
    /// The dish exactly as the plan's Dinners table spells it.
    pub dish: String,
    /// The capitalised weekday the plan actually holds the dish on.
    pub true_day: String,
    /// The slot the plan actually holds the dish at ("dinner" / "lunch").
    pub true_slot: String,
    /// The capitalised weekday the draft placed it on, when that is the wrong day.
    pub claimed_day: Option<String>,
    /// The non-dinner slot the draft placed it at ("lunch"), when it named one.
    pub claimed_slot: Option<String>,
    /// Byte range of the offending sentence within the draft.
    pub span: (usize, usize),
}

/// A sentence of the draft plus its byte span, so a rewrite can splice one clause
/// and leave the rest of the reply byte-identical.
struct SpannedSentence {
    norm: String,
    /// The sentence asks rather than asserts (it carries a question mark), so it
    /// cannot be a false placement CLAIM.
    asks: bool,
    start: usize,
    end: usize,
}

/// Split `draft` into sentences carrying their byte spans. Terminators (`.!?;` and
/// newlines) stay INSIDE the span they close, so replacing a span yields clean
/// prose. Leading whitespace is excluded from the span. Em dashes are NOT
/// terminators: "enjoy that frittata tomorrow — perfect for lunch" is ONE claim,
/// and splitting it would hide the slot half from the day half.
fn split_sentences_with_spans(draft: &str) -> Vec<SpannedSentence> {
    let is_term = |c: char| matches!(c, '.' | '!' | '?' | ';' | '\n');
    let mut out: Vec<SpannedSentence> = Vec::new();
    let chars: Vec<(usize, char)> = draft.char_indices().collect();
    let mut seg_start: Option<usize> = None;
    let mut idx = 0usize;
    while idx < chars.len() {
        let (pos, ch) = chars[idx];
        let start = match seg_start {
            Some(s) => s,
            None => {
                if ch.is_whitespace() {
                    idx += 1;
                    continue;
                }
                seg_start = Some(pos);
                pos
            }
        };
        if is_term(ch) {
            // Swallow a run of terminators ("!?", "…") into the sentence they close.
            let mut end = pos + ch.len_utf8();
            let mut j = idx + 1;
            while j < chars.len() && is_term(chars[j].1) {
                end = chars[j].0 + chars[j].1.len_utf8();
                j += 1;
            }
            let raw = &draft[start..end];
            let norm = normalize(raw);
            if !norm.is_empty() {
                out.push(SpannedSentence {
                    norm,
                    asks: raw.contains('?'),
                    start,
                    end,
                });
            }
            seg_start = None;
            idx = j;
            continue;
        }
        idx += 1;
    }
    if let Some(start) = seg_start {
        // A final sentence with no terminator: the span stops at the last
        // non-whitespace byte so trailing space stays outside any splice.
        let end = start + draft[start..].trim_end().len();
        let raw = &draft[start..end];
        let norm = normalize(raw);
        if !norm.is_empty() {
            out.push(SpannedSentence {
                norm,
                asks: raw.contains('?'),
                start,
                end,
            });
        }
    }
    out
}

/// The full lowercase name of a weekday — the key shape `WeekContext::by_day`
/// uses, so an abbreviation in a draft canonicalises to the table's key.
fn weekday_full_name(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        Weekday::Thu => "thursday",
        Weekday::Fri => "friday",
        Weekday::Sat => "saturday",
        Weekday::Sun => "sunday",
    }
}

/// The dish's identifying words: normalised tokens of ≥4 chars that are not
/// generic prep/filler. ONE of these in a sentence identifies the dish, because
/// families use shorthand ("that frittata") and never the table's full phrase.
fn dish_content_tokens(dish: &str) -> Vec<String> {
    normalize(dish)
        .split(' ')
        .filter(|w| w.len() >= 4 && !DISH_FILLER_WORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Does the normalised `sentence` name this dish (by any identifying word)? The
/// plural/possessive form counts too: normalisation drops apostrophes, so "that
/// frittata's tomorrow" arrives as the token "frittatas".
fn sentence_names_dish(sentence: &str, dish: &str) -> bool {
    dish_content_tokens(dish)
        .iter()
        .any(|t| norm_has_word(sentence, t) || norm_has_word(sentence, &format!("{t}s")))
}

/// Every weekday the normalised `sentence` places something on, in the order the
/// terms appear: a weekday name, or "today"/"tonight"/"tomorrow" resolved through
/// the week context's own family-local markers. A relative term the context does
/// not resolve is skipped (we cannot know which day it meant).
fn claimed_days_in(sentence: &str, wc: &WeekContext) -> Vec<String> {
    let mut days: Vec<String> = Vec::new();
    for raw in sentence.split(' ') {
        // Normalisation drops apostrophes, so the family's own phrasing arrives
        // possessive-glued: "Saturday's dinner" → "saturdays", "tomorrow's" →
        // "tomorrows". Try the bare token, then the de-pluralised form.
        let stripped = raw.strip_suffix('s').unwrap_or(raw);
        let resolved = match (raw, stripped) {
            ("today" | "tonight", _) | (_, "today" | "tonight") => wc.today.clone(),
            ("tomorrow", _) | (_, "tomorrow") => wc.tomorrow.clone(),
            // Canonicalise to the FULL lowercase name so an abbreviation ("Sat")
            // compares equal to the table's key ("saturday").
            _ => weekday_token(raw)
                .or_else(|| weekday_token(stripped))
                .map(|wd| weekday_full_name(wd).to_string()),
        };
        if let Some(day) = resolved {
            if !days.contains(&day) {
                days.push(day);
            }
        }
    }
    days
}

/// The non-dinner slot the normalised `sentence` names, if any.
fn claimed_slot_in(sentence: &str) -> Option<String> {
    for tok in sentence.split(' ') {
        for (stem, label) in MEAL_SLOT_STEMS {
            if tok.starts_with(stem) {
                return Some((*label).to_string());
            }
        }
    }
    None
}

/// True when the sentence frames the dish as leftovers/reheated — a claim about
/// eating again, not about where the plan puts the meal.
fn mentions_leftovers(sentence: &str) -> bool {
    LEFTOVER_CUES.iter().any(|cue| sentence.contains(cue))
}

/// Sentences of `draft` that place a planned dish on the WRONG day or at the WRONG
/// meal slot, against the forwarded week context. Conservative by construction: a
/// sentence must NAME a dish the table holds and then contradict its placement.
/// Casual mentions, truthful placements, leftovers lines, and mentions that fit
/// two days at once are all left alone. Pure; spans are non-overlapping and sorted.
pub fn misplaced_week_claims(draft: &str, wc: &WeekContext) -> Vec<MisplacedWeekClaim> {
    if wc.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<MisplacedWeekClaim> = Vec::new();
    for sentence in split_sentences_with_spans(draft) {
        // A question or a proposal asserts nothing about this week's table — a cook
        // offering "want me to put the frittata on Wednesday?" must keep the offer.
        if sentence.asks
            || PLACEMENT_PROPOSAL_PHRASES
                .iter()
                .any(|cue| sentence.norm.contains(cue))
            || PLACEMENT_PROPOSAL_WORDS
                .iter()
                .any(|w| norm_has_word(&sentence.norm, w))
        {
            continue;
        }
        // Which planned dishes does this sentence name? Exactly one, or we cannot
        // name a single truth (two days' dishes sharing a word ⇒ leave it alone).
        let planned = wc.planned_slots();
        let named: Vec<&(&String, &String, &'static str)> = planned
            .iter()
            .filter(|(_, dish, _)| sentence_names_dish(&sentence.norm, dish))
            .collect();
        if named.len() != 1 {
            continue;
        }
        let (true_day, dish, true_slot) = *named[0];
        if mentions_leftovers(&sentence.norm) {
            continue;
        }
        let days = claimed_days_in(&sentence.norm, wc);
        let claimed_slot = claimed_slot_in(&sentence.norm);
        let right_day = days.iter().any(|d| d == true_day);
        let right_slot = claimed_slot.as_deref().map_or(true, |s| s == true_slot);
        // GROUNDED when the sentence names the real day AND either names no slot or
        // names the right one. "Saturday's lunch is the panzanella" is provably true
        // now that the plan's lunches ride in the context, and must survive.
        if right_day && right_slot {
            continue;
        }
        let claimed_day = days.first().map(|d| capitalize_weekday(d));
        // No day claim and no slot claim \u{21d2} a casual mention. Nothing to correct \u{2014} and a
        // sentence with no day that names the dish's OWN slot claims no placement either.
        if claimed_day.is_none() && (claimed_slot.is_none() || right_slot) {
            continue;
        }
        out.push(MisplacedWeekClaim {
            dish: dish.clone(),
            true_day: capitalize_weekday(true_day),
            true_slot: true_slot.to_string(),
            claimed_day,
            claimed_slot,
            span: (sentence.start, sentence.end),
        });
    }
    out.sort_by_key(|c| c.span.0);
    out
}

/// The truthful placement sentence for one misplaced claim — the same shape the
/// never-claim-empty rewrite uses, so both guards speak with one voice.
pub fn week_placement_truth_line(claim: &MisplacedWeekClaim) -> String {
    let slot = if claim.true_slot.is_empty() {
        "dinner"
    } else {
        claim.true_slot.as_str()
    };
    format!("{}'s {} is {}.", claim.true_day, slot, claim.dish)
}

/// Rewrite `draft` so each misplaced claim's sentence states the truthful
/// placement, leaving every other byte of the reply exactly as composed. Claims
/// must come from `misplaced_week_claims` on the SAME draft (sorted, disjoint
/// spans). Idempotent: the replacement names the real day, so a second pass finds
/// nothing to fix.
pub fn week_placement_rewrite(draft: &str, claims: &[MisplacedWeekClaim]) -> String {
    if claims.is_empty() {
        return draft.to_string();
    }
    let mut out = String::with_capacity(draft.len() + 32);
    let mut cursor = 0usize;
    for claim in claims {
        let (start, end) = claim.span;
        if start < cursor || end > draft.len() {
            continue; // defensive: never splice with a stale span
        }
        out.push_str(&draft[cursor..start]);
        out.push_str(&week_placement_truth_line(claim));
        cursor = end;
    }
    out.push_str(&draft[cursor..]);
    // A spliced-away trailing clause can leave dangling separators ("… — ").
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// PENDING-DINNER GUARD (task engine-twin-grounding) — docs/20 §6 rule 12 on the
// HEAVY lane, the ENGINE half. The twin of `composerGuard.mjs` §6.14
// (`pendingMealClaims` / `pendingMealTruthLine` / `applyPendingMealRewrite`).
//
// THE FOURTH LIE DIRECTION. The three guards above cover DENYING a planned dish,
// INVENTING one, and MOVING one. This one covers a dinner that is real, on the
// right day, at the right slot — and NOT AGREED TO. A dinner carrying the plan's
// own ⏳ is LINED UP, and the Week view renders it as an actionable "needs your OK"
// chip; a reply that reports it as settled ("Friday's salmon is all set") makes the
// same dinner a question on one surface and a fact on the other, which is how a
// household shops for a Friday nobody said yes to.
//
// THE GATEWAY SHIPPED ITS HALF FIRST (task carry-the-pending): `buildWeekContext`
// now states the ⏳ rows in words, on "• Awaiting the family's OK — Day (date):
// dish" lines chosen precisely so an engine WITHOUT this guard drops them. Live
// replies are composed HERE (CONTRIBUTING §2, the "both twins" rule), so until this
// existed the enforcing copy was missing on the path that delivers most replies.
//
// THE RULE. A sentence that ASSERTS a pending dinner — declares it final ("all set",
// "locked in"), or simply states it flat with its day/slot as this week's fact — is
// corrected to name the dish, the day AND that it still needs the family's OK.
// Exactly what §6.12 exempts is exempt here, for the same reasons, plus one more: a
// sentence that ALREADY says it is not agreed ("still needs your OK", "pencilled
// in", "a proposal") is the honest form we are steering toward and is never
// rewritten — which is also what makes both rewrites idempotent.
//
// TWO SHAPES, because a sentence can name more than one pending dish and then there
// is no single truth to substitute: exactly one → the sentence is SPLICED with the
// truthful line (§6.12's surgical shape); more than one → the sentence survives and
// the reply gains the fast read lane's own clause. The wording of both is
// byte-identical to the JS twin and to `latencyTier.weekPendingClause`, so no two
// lanes tell the family this in different words.
// ---------------------------------------------------------------------------

/// Cues that a sentence ALREADY states the dinner is not agreed. Matched against
/// NORMALISED text, so apostrophes are gone ("hasn't" → "hasnt"). Byte-identical to
/// the JS twin's `PENDING_ACK_CUES`.
const PENDING_ACK_CUES: &[&str] = &[
    "still needs",
    "still need",
    "needs your ok",
    "need your ok",
    "needs an ok",
    "needs the ok",
    "your ok",
    "not yet agreed",
    "not agreed",
    "to confirm",
    "confirm with",
    "unconfirmed",
    "pencilled",
    "penciled",
    "pencil",
    "tentative",
    "proposal",
    "proposed",
    "propose",
    "provisional",
    "up to you",
    "if youre happy",
    "sound ok",
    "sounds ok",
    "sound good",
    "say the word",
    "lined up",
    "waiting on",
    "awaiting",
    "not locked",
    "nothing locked",
];

/// Cues that a sentence declares the dinner DECIDED. Narrow on purpose: each is a
/// claim of finality that a ⏳ row does not back. The JS twin's `SETTLED_CUES`.
const PENDING_SETTLED_CUES: &[&str] = &[
    "all set",
    "is set",
    "are set",
    "all sorted",
    "sorted",
    "locked in",
    "settled",
    "confirmed",
    "good to go",
    "all done",
    "taken care of",
    "nailed down",
    "squared away",
    "decided",
    "final",
    "definitely",
    "for sure",
    "no need to",
];

/// A sentence that presents PENDING dinners as decided: the pending rows it names,
/// the days they fall on (deduped, in the plan's order), and the sentence's byte
/// span in the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWeekClaim {
    /// The pending rows this sentence names. Exactly one ⇒ the sentence is spliced
    /// with the truth; more than one ⇒ the days ride in a trailing clause instead.
    pub rows: Vec<PendingWeekRow>,
    /// The capitalised weekdays those rows fall on, deduped, plan order.
    pub days: Vec<String>,
    /// Byte range of the offending sentence within the draft.
    pub span: (usize, usize),
}

/// Sentences of `draft` that report a PENDING dinner as settled, against the
/// forwarded week context. `[]` when the context states nothing pending — the guard
/// is opt-in, so every settled week and every non-food turn is unchanged. Pure;
/// spans are sorted and disjoint.
pub fn pending_week_claims(draft: &str, wc: &WeekContext) -> Vec<PendingWeekClaim> {
    if wc.pending.is_empty() {
        return Vec::new();
    }
    // The multi-dish correction is a REPLY-level clause ("Friday and Sunday still
    // need your OK."), so a reply-level acknowledgement anywhere already satisfies it
    // — which is what makes appending the clause idempotent. A single-dish splice
    // stays sentence-level: a reply that says "all set" in one breath and "still
    // needs your OK" in the next is contradicting itself, and the settled half is
    // still the half the family will believe.
    let draft_acks = {
        let whole = normalize(draft);
        PENDING_ACK_CUES.iter().any(|cue| whole.contains(cue))
    };
    let mut out: Vec<PendingWeekClaim> = Vec::new();
    for sentence in split_sentences_with_spans(draft) {
        // A question or an offer PROPOSES rather than asserts — the shape we want.
        if sentence.asks
            || PLACEMENT_PROPOSAL_PHRASES
                .iter()
                .any(|cue| sentence.norm.contains(cue))
            || PLACEMENT_PROPOSAL_WORDS
                .iter()
                .any(|w| norm_has_word(&sentence.norm, w))
        {
            continue;
        }
        // …and a sentence that already names the pending state is the honest form.
        if PENDING_ACK_CUES
            .iter()
            .any(|cue| sentence.norm.contains(cue))
        {
            continue;
        }
        let named: Vec<PendingWeekRow> = wc
            .pending
            .iter()
            .filter(|row| sentence_names_dish(&sentence.norm, &row.dish))
            .cloned()
            .collect();
        if named.is_empty() || (named.len() > 1 && draft_acks) {
            continue;
        }
        if mentions_leftovers(&sentence.norm) {
            continue;
        }
        // ASSERTIVE either by declaring finality, or by placing the dish on a
        // day/slot as this week's fact — the flat listing that reads exactly like a
        // settled dinner. A casual mention with neither ("that salmon was a hit")
        // claims nothing and is left alone.
        let assertive = PENDING_SETTLED_CUES
            .iter()
            .any(|cue| sentence.norm.contains(cue))
            || !claimed_days_in(&sentence.norm, wc).is_empty()
            || claimed_slot_in(&sentence.norm).is_some();
        if !assertive {
            continue;
        }
        let mut days: Vec<String> = Vec::new();
        for row in &named {
            if !days.contains(&row.day) {
                days.push(row.day.clone());
            }
        }
        out.push(PendingWeekClaim {
            rows: named,
            days,
            span: (sentence.start, sentence.end),
        });
    }
    out.sort_by_key(|c| c.span.0);
    out
}

/// The truthful sentence for ONE pending dinner — the placement §6.12 would state,
/// plus the fact that makes it a proposal. NO emoji: this splices into a reply that
/// already carries the persona's own glyph (as `week_placement_truth_line` does).
/// Byte-identical to the JS twin's `pendingMealTruthLine`.
pub fn week_pending_truth_line(row: &PendingWeekRow) -> String {
    let slot = if row.slot.is_empty() {
        "dinner"
    } else {
        row.slot.as_str()
    };
    format!(
        "{}'s {} is {}, but it still needs your OK.",
        row.day, slot, row.dish
    )
}

/// "Friday and Sunday still need your OK." — the clause appended when a sentence
/// names more than one pending dinner. Byte-identical wording to the JS twin's
/// `pendingMealNoteLine` and to the fast read lane's `latencyTier.weekPendingClause`,
/// so the lanes never disagree about how this is said.
pub fn week_pending_note_line(days: &[String]) -> String {
    let list = if days.len() <= 1 {
        days.first().cloned().unwrap_or_default()
    } else {
        format!(
            "{} and {}",
            days[..days.len() - 1].join(", "),
            days[days.len() - 1]
        )
    };
    let verb = if days.len() == 1 { "needs" } else { "need" };
    format!("{list} still {verb} your OK.")
}

/// Rewrite `draft` so each single-dish pending claim states the truth in place, and
/// any multi-dish claim's days are named once in a trailing clause. Every other byte
/// survives. Claims must come from `pending_week_claims` on the SAME draft (sorted,
/// disjoint spans). Idempotent: both corrections acknowledge the pending state, so a
/// second pass finds nothing to fix.
pub fn week_pending_rewrite(draft: &str, claims: &[PendingWeekClaim]) -> String {
    if claims.is_empty() {
        return draft.to_string();
    }
    let mut out = String::with_capacity(draft.len() + 48);
    let mut cursor = 0usize;
    let mut note_days: Vec<String> = Vec::new();
    for claim in claims {
        if claim.rows.len() != 1 {
            for day in &claim.days {
                if !note_days.contains(day) {
                    note_days.push(day.clone());
                }
            }
            continue;
        }
        let (start, end) = claim.span;
        if start < cursor || end > draft.len() {
            continue; // defensive: never splice with a stale span
        }
        out.push_str(&draft[cursor..start]);
        out.push_str(&week_pending_truth_line(&claim.rows[0]));
        cursor = end;
    }
    out.push_str(&draft[cursor..]);
    let out = out.trim().to_string();
    if note_days.is_empty() {
        out
    } else {
        format!("{out} {}", week_pending_note_line(&note_days))
    }
}

// ---------------------------------------------------------------------------
// TIER-1 DURABLE MEMORY (task p1-engine-memory-reader) — the engine-side READER
// for the block the gateway already builds and forwards.
//
// THE GAP THIS CLOSES. The gateway builds an acting-member-SCOPED,
// NON-AUTHORITATIVE, token-budgeted family-memory block
// (`claw3d-bridge/src/memoryInject.mjs` `buildMemoryContext`) and forwards it to
// the engine over the `WG_MEMORY_CONTEXT` env var
// (`claw3d-bridge/src/webInbound.mjs`), exactly like `WG_THREAD_CONTEXT` /
// `WG_WEEK_CONTEXT`. The hermetic human-flow stub reflects it, so the flow suite
// was green — but the PRODUCTION Rust compose path read only the thread and week
// vars, so on a real deploy the block was built, budgeted, logged … and dropped
// on the floor. Every remembered preference was invisible to the model that
// actually answers the family.
//
// THE CARDINAL RULE (docs/39 §5.3): **live wins**. Tier-1 memory is a soft prior,
// never state. If memory says "gym is usually Wednesday" and the live calendar is
// clear, the persona states the live truth and at most OFFERS the pattern. Two
// mechanisms enforce it here:
//
//   1. ORDER (docs/39 §6): the block is injected LAST — after the always-on
//      calendar-truth line, after the read-shaped week grounding, after the
//      forwarded Dinners table, and after the family's corrections (which
//      outrank distilled facts, docs/39 §5.2). Precedence is legible to the
//      model in the order it reads.
//   2. LABEL: the block leads with an explicit "soft priors, NOT live state —
//      everything above wins" instruction, so a remembered pattern can never be
//      presented as this week's schedule.
//
// BUDGET (docs/39 §6 + the no-silent-caps posture): the gateway budgets the block
// before forwarding, but the engine is a separate process reading an env var it
// does not own, so it guards again — deterministic, line-bounded truncation with a
// VISIBLE note in the block itself, never a silent trim.
// ---------------------------------------------------------------------------

/// Character ceiling for the forwarded memory block, ~4 chars/token against the
/// Tier-1 budget of ≈1500 tokens (docs/39 §6, the same rule of thumb the
/// gateway's `estimateTokens` uses). A block at or under this is passed through
/// whole; a longer one is truncated at a line boundary with a visible note.
pub const MEMORY_CONTEXT_MAX_CHARS: usize = 6000;

/// The compose-prompt block for a forwarded `WG_MEMORY_CONTEXT`: the gateway's
/// scoped Tier-1 memory wrapped with the standing "these are soft priors, the
/// live facts above win" instruction (docs/39 §5.3). `None` when the forwarded
/// context is blank — the Telegram-listener path and any deploy with nothing
/// remembered leave the prompt byte-for-byte unchanged.
///
/// Over-budget input is truncated DETERMINISTICALLY at a line boundary and the
/// block says so, so a distiller bug shows up as a visible note instead of a
/// silently ballooned prompt.
pub fn memory_context_block(memory_context: &str) -> Option<String> {
    let text = memory_context.trim();
    if text.is_empty() {
        return None;
    }
    let (body, trimmed_lines) = clamp_memory_context(text);
    if body.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str(
        "FAMILY MEMORY — remembered preferences and patterns, NOT the current schedule. \
         These are soft priors the family has settled over time. Everything above (today's \
         calendar, this week's dinners, the family's corrections) is the LIVE truth and it \
         WINS: if memory disagrees with it, say the live fact and at most OFFER the \
         remembered pattern (\"the calendar's clear then — want me to pencil in your \
         usual?\"). Never present a remembered pattern as this week's plan, never invent a \
         schedule from it, and never read this list out as if it were news:\n",
    );
    out.push_str(&body);
    out.push('\n');
    if trimmed_lines > 0 {
        out.push_str(&format!(
            "(…and {trimmed_lines} more remembered line(s) left out to keep this small — \
             treat this list as partial.)\n",
        ));
    }
    Some(out)
}

/// Clamp the forwarded memory text to [`MEMORY_CONTEXT_MAX_CHARS`], cutting only
/// at line boundaries so a fact is never sliced mid-sentence. Returns the kept
/// body and how many lines were dropped. At least the first line always survives
/// (a pathological single-line block over budget is kept whole rather than
/// vanishing — losing an allergy line to a budget is the bug the gateway's
/// injector protects against, and this guard honours the same posture).
fn clamp_memory_context(text: &str) -> (String, usize) {
    if text.len() <= MEMORY_CONTEXT_MAX_CHARS {
        return (text.to_string(), 0);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in &lines {
        // +1 for the newline the join re-adds.
        let cost = line.len() + 1;
        if !kept.is_empty() && used + cost > MEMORY_CONTEXT_MAX_CHARS {
            break;
        }
        kept.push(line);
        used += cost;
    }
    let dropped = lines.len().saturating_sub(kept.len());
    (kept.join("\n"), dropped)
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
        assert!(
            anchor.contains("(today)"),
            "tonight resolves to today, got: {anchor}"
        );
        assert!(anchor.contains("Friday, Jul 17"), "got: {anchor}");
        assert!(
            !anchor.contains("Jul 18"),
            "tonight is not tomorrow, got: {anchor}"
        );
    }

    const PLAN: &str = "\
# 2026-W29 Family Plan

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** DRAFT

## 1. Dinners (planner → cook)

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

    /// A reminder QUESTION is a read of the family's own schedule and must reach
    /// the grounded block — the omission that let "what date is the reminder to
    /// call the dentist set for?" be composed with no data in front of it (task
    /// reminder-readback-lane). The reminder WRITE is untouched: it carries no
    /// reminder noun and stays with the fast lane.
    #[test]
    fn ground_read_shaped_covers_reminder_questions_but_not_reminder_writes() {
        for ask in [
            "What exact date and time is the reminder to call the dentist set for?",
            "When is my dentist reminder?",
            "Do I have a reminder about the bins?",
            "what reminders do I have",
        ] {
            assert!(is_read_shaped(ask), "should be read-shaped: {ask:?}");
        }
        for write in ["set a reminder for the dentist", "cancel the reminder"] {
            assert!(
                !is_read_shaped(write),
                "a reminder write is not a read: {write:?}"
            );
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
        let a =
            "Meals are set, just waiting on confirmations from you and Nadin. Want the rundown?";
        let b = "Meals are all set — still waiting on confirmations from you and Nadin. Want a rundown?";
        assert!(is_repetitive(b, a), "sim={}", similarity(a, b));
    }

    #[test]
    fn ground_repetition_allows_the_real_grounded_answer() {
        let stall = "Meals are set, waiting on confirmations from you and Nadin. Want the rundown?";
        let real = "Tomorrow (Tue) it's baked salmon with roasted potatoes and green beans, \
                    and you've got your PT check-in at 7:30pm.";
        assert!(
            !is_repetitive(real, stall),
            "sim={}",
            similarity(stall, real)
        );
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
            assert!(
                detect_correction(msg).is_none(),
                "should NOT detect: {msg:?}"
            );
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
        assert!(
            !shaped.trim_end().ends_with('?'),
            "still a question: {shaped:?}"
        );
        assert_eq!(
            shaped,
            "Here's today: leftovers for dinner and your lower-body session."
        );

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
        assert!(
            block.contains("Leftovers"),
            "should have today's dinner:\n{block}"
        );
        assert!(block.contains("Wednesday"));
        // NOTHING from other days may leak in.
        assert!(!block.contains("Baked salmon"), "Tue meal leaked:\n{block}");
        assert!(!block.contains("Chickpea"), "Mon meal leaked:\n{block}");
        assert!(!block.contains("Dentist"), "Thu appt leaked:\n{block}");
        assert!(
            !block.contains("Luca PT check-in"),
            "Tue appt leaked:\n{block}"
        );
    }

    #[test]
    fn shape_detect_scope_reads_the_asked_window() {
        let today = date(2026, 7, 15); // Wednesday
        assert_eq!(
            detect_scope("what's for dinner today?", today),
            AskScope::Day(today)
        );
        assert_eq!(
            detect_scope("what's the plan?", today),
            AskScope::Day(today)
        );
        assert_eq!(
            detect_scope("plans for tomorrow?", today),
            AskScope::Day(date(2026, 7, 16))
        );
        assert_eq!(
            detect_scope("anything on friday?", today),
            AskScope::Day(date(2026, 7, 17))
        );
        // A weekday that is today resolves to today, not next week.
        assert_eq!(
            detect_scope("what's on wednesday?", today),
            AskScope::Day(today)
        );
        assert_eq!(
            detect_scope("how's the week looking?", today),
            AskScope::Week
        );
        assert_eq!(
            detect_scope("anything this weekend?", today),
            AskScope::Week
        );
    }

    #[test]
    fn shape_tomorrow_ask_shows_tomorrows_items() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Asked on Wed; tomorrow = Thu 07-16 → Dentist at 09:00, no meal row.
        let block = grounded_block(&doc, at(2026, 7, 15, 12, 0), "what's on tomorrow?");
        assert!(block.contains("Thursday"));
        assert!(block.contains("Dentist"), "Thu appt missing:\n{block}");
        assert!(
            !block.contains("Leftovers"),
            "today's meal leaked into tomorrow:\n{block}"
        );
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
        assert!(!event_has_passed(
            "09:00",
            date(2026, 7, 16),
            at(2026, 7, 14, 23, 0)
        ));
        // Empty / all-day time on today is kept (not past).
        assert!(!event_has_passed("", day, at(2026, 7, 14, 23, 0)));

        let doc = PlanDoc::parse("2026-W29", PLAN);
        // At 15:00 on Tue the 19:30 check-in is still ahead → present.
        let early = grounded_block(&doc, at(2026, 7, 14, 15, 0), "what's the plan today?");
        assert!(
            early.contains("Luca PT check-in"),
            "should still be upcoming:\n{early}"
        );
        // At 20:00 on Tue it has passed → gone from "still coming up".
        let late = grounded_block(&doc, at(2026, 7, 14, 20, 0), "what's the plan today?");
        assert!(
            !late.contains("Luca PT check-in"),
            "past event should be dropped:\n{late}"
        );
    }

    #[test]
    fn shape_time_parser_handles_common_forms() {
        assert_eq!(
            parse_time_of_day("19:30"),
            NaiveTime::from_hms_opt(19, 30, 0)
        );
        assert_eq!(parse_time_of_day("9:00"), NaiveTime::from_hms_opt(9, 0, 0));
        assert_eq!(
            parse_time_of_day("7:30pm"),
            NaiveTime::from_hms_opt(19, 30, 0)
        );
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
        assert!(
            block.contains("Farmers market"),
            "should offer tomorrow's item:\n{block}"
        );
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
            assert!(
                is_deliberation_request(ask),
                "should be deliberation: {ask:?}"
            );
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
        assert!(
            fabricates_schedule(draft, &empty),
            "must be flagged: {offenders:?}"
        );
        // Each distinct invented specific is caught.
        assert!(
            offenders.iter().any(|o| o == "birthday"),
            "birthday missed: {offenders:?}"
        );
        assert!(
            offenders.iter().any(|o| o == "meetings"),
            "meetings missed: {offenders:?}"
        );
        assert!(
            offenders
                .iter()
                .any(|o| o.contains("back") || o == "packed" || o == "packed day"),
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
            assert!(
                fabricates_schedule(draft, &empty),
                "should reject: {draft:?}"
            );
        }
    }

    // A claim SOURCED by the real calendar passes: the noun is literally on it,
    // or the load claim has the >=2 events to back it.
    #[test]
    fn ground_fabrication_allows_sourced_claims() {
        // Two real events → "back-to-back" is grounded; "meeting" is on a title.
        let g =
            build_schedule_grounding(&["Team meeting".to_string(), "Dentist — Nadin".to_string()]);
        assert!(!fabricates_schedule(
            "You've got a meeting then the dentist — a busy day.",
            &g
        ));
        assert!(!fabricates_schedule(
            "Back-to-back today: the meeting and the dentist.",
            &g
        ));
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
            assert!(
                !fabricates_schedule(draft, &empty),
                "should NOT reject: {draft:?}"
            );
        }
    }

    // The guard's grounding is built from the REAL plan, scoped like the prompt.
    #[test]
    fn ground_schedule_grounding_scopes_from_the_plan() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Greeting on Tue at 15:00 → today's still-upcoming event = PT check-in.
        let g = schedule_grounding_for(&doc, at(2026, 7, 14, 15, 0), "how's your day going?");
        assert_eq!(g.count, 1, "Tue has one upcoming event: text={:?}", g.text);
        assert!(
            g.text.contains("pt check in") || g.text.contains("check in"),
            "text={:?}",
            g.text
        );
        // A reply grounded in that real event passes; an invented meeting fails.
        assert!(!fabricates_schedule(
            "You've got your PT check-in at 7:30.",
            &g
        ));
        assert!(fabricates_schedule("You've got a meeting at noon.", &g));
        // After 19:30 the check-in has passed → empty grounding → strict again.
        let spent = schedule_grounding_for(&doc, at(2026, 7, 14, 20, 0), "how's your day going?");
        assert_eq!(spent.count, 0);
        assert!(fabricates_schedule(
            "You've still got your check-in and a meeting.",
            &spent
        ));
    }

    // The fallback is honest and volunteers no invented specifics.
    #[test]
    fn ground_fabrication_fallback_invents_nothing() {
        let empty = ScheduleGrounding::default();
        let fallback = grounding_fallback_line();
        assert!(
            !fabricates_schedule(&fallback, &empty),
            "fallback must be clean: {fallback}"
        );
        assert!(fallback.to_lowercase().contains("calendar"));
    }

    // The ALWAYS-ON context line names real events, or states the day is clear —
    // and always forbids inventing. This is the root-cause fix: the calendar is
    // now in the compose context even for non-read-shaped chatter.
    #[test]
    /// THE LIVE FAILURE (2026-08-18). Asked "What's in for today?", the house answered
    /// "Calendar's clear — nothing on the books" while a school pickup sat in the family's linked
    /// Google calendar. The grounding line is where that came from: it reads the PLAN document,
    /// and on an empty plan it instructed the model to "say the calendar is clear".
    ///
    /// The plan is not the whole calendar when an external feed is configured — its events reach
    /// /calendar.json and the Week view, never a PlanDoc. This is the same class of bug the
    /// gateway fixed on 2026-07-20 (task safety-critical-fast), whose recorded principle is that
    /// an empty result from a non-authoritative source must HEDGE, never say "you are free".
    #[test]
    fn an_external_feed_forbids_claiming_the_day_is_clear() {
        let now = at(2026, 7, 14, 15, 0);

        // No feed: the plan IS the calendar, so "clear" is honest and stays.
        let authoritative = schedule_context_line_scoped(None, now, false);
        assert!(
            authoritative.contains("NOTHING on the calendar"),
            "without a feed the strict truth line must stay: {authoritative}"
        );
        assert!(authoritative.contains("say the calendar is clear"));

        // A feed exists and the plan is empty: we cannot see the family's day.
        let hedged = schedule_context_line_scoped(None, now, true);
        assert!(
            !hedged.contains("say the calendar is clear"),
            "the model was still licensed to claim a clear day: {hedged}"
        );
        assert!(
            !hedged.contains("NOTHING on the calendar"),
            "the absence was still asserted as fact: {hedged}"
        );
        assert!(
            hedged.contains("NOT visible to you here"),
            "the hedge must say WHY it cannot know: {hedged}"
        );
        assert!(
            hedged.contains("do NOT invent") || hedged.contains("NOT invent an event"),
            "hedging must not license invention either: {hedged}"
        );
    }

    /// THE FOLLOW-ON FAILURE (2026-08-20). With the hedge in place the house stopped lying —
    /// and started saying "Can't see your linked calendar from here, so anything there will not
    /// show up." Honest, and useless: the family linked a calendar precisely so the house would
    /// know what is on it. The Google feed is fetched by the GATEWAY and kept in memory, so this
    /// process genuinely could not see an event. It can now: the gateway writes the merged list
    /// to `.casa/calendar/synced-events.json` and this reads it.
    #[test]
    fn a_fresh_snapshot_replaces_the_hedge_with_the_real_calendar() {
        let dir = tempfile::tempdir().unwrap();
        let cal = dir.path().join(".casa").join("calendar");
        std::fs::create_dir_all(&cal).unwrap();
        // The feed IS configured — this is the exact house that got the hedge.
        std::fs::write(
            dir.path().join(".casa").join("calendar.toml"),
            "[calendar]\nics_url = \"https://example.invalid/private-feed.ics\"\n",
        )
        .unwrap();
        let now = at(2026, 7, 14, 15, 0);
        // A TRUE local->absolute conversion, which is what the gateway's `Date.now()` is.
        // Writing `now.and_utc().timestamp_millis()` here instead would reproduce the exact
        // confusion this file had in production and CANCEL IT OUT on both sides — the test
        // would pass while a live house was blinded by its own zone offset. That is what
        // happened, and only the live-house probe caught it.
        let local_ms = |t: NaiveDateTime| {
            Local
                .from_local_datetime(&t)
                .earliest()
                .expect("unambiguous local instant")
                .timestamp_millis()
        };
        let confirmed = local_ms(now);
        // Rendered exactly as the gateway renders them — `new Date(x).toISOString()`, i.e. UTC
        // with a `Z` — from LOCAL instants, so this fixture cannot drift into a shape
        // production never writes (and so it passes in any timezone the suite runs in).
        let iso = |t: NaiveDateTime| {
            Local
                .from_local_datetime(&t)
                .single()
                .expect("unambiguous local instant")
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let later = iso(at(2026, 7, 14, 17, 0));
        let earlier = iso(at(2026, 7, 14, 9, 0));
        let tomorrow = iso(at(2026, 7, 15, 9, 0));

        // Nothing on disk yet → still blind, still hedging. This is the control: without it,
        // the assertions below could pass on a reader that always claims to see.
        assert_eq!(calendar_snapshot_view(dir.path(), now), CalendarView::Blind);
        assert!(fetch_schedule_context_line(dir.path(), now).contains("NOT visible to you here"));

        // The gateway's snapshot: a school pickup later today, one already past, one tomorrow.
        std::fs::write(
            cal.join("synced-events.json"),
            format!(
                r#"{{"version":1,"writtenAt":{confirmed},"feed":"configured","fetchedAt":{confirmed},
                    "status":"ok","windowDays":14,"events":[
                    {{"title":"Pick up the kids","start":"{later}","end":"{later}","allDay":false,"source":"google"}},
                    {{"title":"Dentist","start":"{earlier}","end":"{earlier}","allDay":false,"source":"google"}},
                    {{"title":"Sports day","start":"{tomorrow}","end":"{tomorrow}","allDay":true,"source":"family"}}]}}"#
            ),
        )
        .unwrap();

        let view = calendar_snapshot_view(dir.path(), now);
        let CalendarView::Visible { titles } = view else {
            panic!("a fresh snapshot was not trusted: {view:?}");
        };
        // Today's REMAINING event only: the 9am dentist has passed, sports day is tomorrow.
        assert_eq!(titles, vec!["Pick up the kids (5:00pm)".to_string()]);

        let line = fetch_schedule_context_line(dir.path(), now);
        assert!(line.contains("Pick up the kids"), "{line}");
        assert!(
            !line.contains("NOT visible to you here"),
            "still hedging while holding the answer: {line}"
        );
        assert!(
            line.contains("COMPLETE"),
            "the model was not told the list is the whole calendar: {line}"
        );
        assert!(
            line.contains("do NOT tell the family you cannot see their calendar"),
            "nothing stops the old sentence being said anyway: {line}"
        );
    }

    #[test]
    fn a_stale_or_unreadable_snapshot_goes_back_to_hedging() {
        let dir = tempfile::tempdir().unwrap();
        let cal = dir.path().join(".casa").join("calendar");
        std::fs::create_dir_all(&cal).unwrap();
        std::fs::write(
            dir.path().join(".casa").join("calendar.toml"),
            "[calendar]\nics_url = \"https://example.invalid/private-feed.ics\"\n",
        )
        .unwrap();
        let now = at(2026, 7, 14, 15, 0);

        // Confirmed two hours ago: the gateway has missed ~24 polls, so the family may well
        // have added something since. Trusting it would be the same overconfidence in a new
        // place — an empty list from a source that has stopped reporting is not evidence.
        let local_ms = |t: NaiveDateTime| {
            Local
                .from_local_datetime(&t)
                .earliest()
                .expect("unambiguous local instant")
                .timestamp_millis()
        };
        let stale = local_ms(now - chrono::Duration::hours(2));
        let pickup = {
            Local
                .from_local_datetime(&at(2026, 7, 14, 17, 0))
                .single()
                .expect("unambiguous local instant")
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let write = |fetched: String| {
            std::fs::write(
                cal.join("synced-events.json"),
                format!(
                    r#"{{"version":1,"feed":"configured","fetchedAt":{fetched},"events":[
                       {{"title":"Pick up the kids","start":"{pickup}","allDay":false,"source":"google"}}]}}"#
                ),
            )
            .unwrap();
        };
        write(stale.to_string());
        assert_eq!(calendar_snapshot_view(dir.path(), now), CalendarView::Blind);
        assert!(fetch_schedule_context_line(dir.path(), now).contains("NOT visible to you here"));

        // No stamp at all, and a corrupt file: both blind, neither a panic.
        write("null".to_string());
        assert_eq!(calendar_snapshot_view(dir.path(), now), CalendarView::Blind);
        std::fs::write(cal.join("synced-events.json"), "{ not json").unwrap();
        assert_eq!(calendar_snapshot_view(dir.path(), now), CalendarView::Blind);

        // FRESH is the control on all of the above — same file, current stamp, and the event
        // comes through. Otherwise these could pass on a reader that is simply always blind.
        write(local_ms(now).to_string());
        assert!(matches!(
            calendar_snapshot_view(dir.path(), now),
            CalendarView::Visible { ref titles } if titles.len() == 1
        ));
    }

    #[test]
    fn with_no_feed_the_family_own_entries_need_no_freshness() {
        // The other half of the invisibility: with no Google feed, the family's quick-adds ARE
        // the whole calendar — and were equally unreadable here. They are complete when written,
        // so an old stamp must not blind us to them.
        let dir = tempfile::tempdir().unwrap();
        let cal = dir.path().join(".casa").join("calendar");
        std::fs::create_dir_all(&cal).unwrap();
        let now = at(2026, 7, 14, 15, 0);
        let swim = {
            Local
                .from_local_datetime(&at(2026, 7, 14, 18, 0))
                .single()
                .expect("unambiguous local instant")
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        std::fs::write(
            cal.join("synced-events.json"),
            format!(
                r#"{{"version":1,"feed":"none","fetchedAt":null,"events":[
                   {{"title":"Swimming","start":"{swim}","allDay":false,"source":"family"}}]}}"#
            ),
        )
        .unwrap();
        let CalendarView::Visible { titles } = calendar_snapshot_view(dir.path(), now) else {
            panic!("a feed-less snapshot was treated as blind");
        };
        assert_eq!(titles, vec!["Swimming (6:00pm)".to_string()]);
        let line = fetch_schedule_context_line(dir.path(), now);
        assert!(line.contains("Swimming"), "{line}");
        assert!(!line.contains("NOT visible to you here"), "{line}");
    }

    #[test]
    fn a_commitment_in_both_the_plan_and_the_calendar_is_said_once() {
        const PLAN_PICKUP: &str = "\
# 2026-W29 Family Plan

**Week of Monday 2026-07-13 to Sunday 2026-07-19**

## 3. Calendar

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Tue 07-14 | 17:00 | Pick up the kids | Otto |
";
        let now = at(2026, 7, 14, 15, 0);
        let doc = PlanDoc::parse("2026-W29", PLAN_PICKUP);
        let line = schedule_context_line_with_calendar(
            Some(&doc),
            now,
            &["Pick up the kids (5:00pm)".to_string()],
        );
        assert_eq!(
            line.matches("Pick up the kids").count(),
            1,
            "the same commitment was listed twice: {line}"
        );
    }

    #[test]
    fn the_feed_probe_reads_a_real_calendar_toml() {
        let dir = tempfile::tempdir().unwrap();
        let casa = dir.path().join(".casa");
        std::fs::create_dir_all(&casa).unwrap();

        // No file at all.
        assert!(!external_calendar_configured(dir.path()));

        // A config with only comments and an empty url is NOT a feed.
        std::fs::write(
            casa.join("calendar.toml"),
            "# Family calendar\n# url = \"https://example.invalid/commented-out.ics\"\n[calendar]\nurl = \"\"\n",
        )
        .unwrap();
        assert!(
            !external_calendar_configured(dir.path()),
            "a commented-out or empty url must not count as a configured feed"
        );

        // The key the GATEWAY writes is `ics_url` (calendarSource.mjs writeCalendarConfig). Pinning
        // it here because the first version of this probe guessed `url`, reported "no feed" on the
        // live house, and would have left the hedge dormant.
        std::fs::write(
            casa.join("calendar.toml"),
            "[calendar]\nics_url = \"https://calendar.google.com/calendar/ical/EXAMPLE/basic.ics\"\n",
        )
        .unwrap();
        assert!(
            external_calendar_configured(dir.path()),
            "the gateway's own ics_url key must be recognised"
        );

        // A hand-written short key still counts.
        std::fs::write(
            casa.join("calendar.toml"),
            "[calendar]\nurl = \"https://calendar.google.com/calendar/ical/EXAMPLE/basic.ics\"\n",
        )
        .unwrap();
        assert!(external_calendar_configured(dir.path()));
    }

    fn ground_schedule_context_line_states_the_truth() {
        let doc = PlanDoc::parse("2026-W29", PLAN);
        // Tue at 15:00 → names the real upcoming event, forbids invention.
        let with = schedule_context_line(Some(&doc), at(2026, 7, 14, 15, 0));
        assert!(
            with.contains("PT check-in"),
            "should name the real event:\n{with}"
        );
        assert!(
            with.to_lowercase().contains("do not invent") || with.to_lowercase().contains("do not"),
            "{with}"
        );
        // No plan at all → explicit empty-calendar truth.
        let none = schedule_context_line(None, at(2026, 7, 14, 15, 0));
        assert!(
            none.to_lowercase().contains("nothing on the calendar"),
            "{none}"
        );
        assert!(none.to_lowercase().contains("do not invent"), "{none}");
        // A spent day (asked Thu 20:00, after the 09:00 dentist) → clear.
        let spent = schedule_context_line(Some(&doc), at(2026, 7, 16, 20, 0));
        assert!(
            spent.to_lowercase().contains("nothing on the calendar"),
            "{spent}"
        );
    }

    // --- Rule 6: no dangling-promise deferral tail (task owner-pin-engine) ---

    fn fixture_deferral_roster() -> FamilyVoiceRoster {
        FamilyVoiceRoster::from_names(
            ["garden-7", "Blue Lantern", "pantry-4", "Copper Finch"],
            std::iter::empty::<&str>(),
        )
    }

    /// The answer lands, then dangles a promise to get a configured persona's
    /// exact take. The guard strips ONLY that trailing clause and leaves the
    /// delivered answer intact.
    #[test]
    fn deferral_tail_is_stripped_from_delivered_answer() {
        let roster = fixture_deferral_roster();
        let repro = "Pasta pomodoro is solid at 400-450 calories a plate. Let me ask Blue Lantern's exact take.";
        let cleaned = enforce_no_deferral(repro, &roster);
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
            "Let me check with someone on that.",
            "let me confirm the exact figure",
            "I'll circle back on it.",
            "I'll ask garden-7 for a second look.",
        ] {
            let reply = format!("It's about 450 calories. {tail}");
            let out = enforce_no_deferral(&reply, &roster);
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

    /// Permanent config contract: authored display names and opaque ids come
    /// from household.toml, so renaming either one changes the guard without a
    /// binary change. Unconfigured or partial-token lookalikes remain ordinary
    /// family copy.
    #[test]
    fn configured_persona_deferral_is_roster_derived() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[[agent]]
id = "garden-7"
name = "Blue Lantern"

[[agent]]
id = "pantry-4"
name = "Copper Finch"
"#,
        )
        .unwrap();
        let roster = load_family_voice_roster(dir.path(), &dir.path().join(".wg"));

        for tail in [
            "Let me ask Blue Lantern about that.",
            "Let me ask Blue Lantern's exact take.",
            "I'll ask pantry-4 and get back to you.",
        ] {
            assert!(
                is_deferral_tail(tail, &roster),
                "configured deferral was not detected: {tail:?}"
            );
            let reply = format!("Dinner is already settled. {tail}");
            assert_eq!(
                enforce_no_deferral(&reply, &roster),
                "Dinner is already settled.",
                "configured deferral was not stripped: {tail:?}"
            );
        }

        for safe in [
            "Let me ask a question about Friday.",
            "Let me ask Marigold about that.",
            "I asked Blue Lantern and dinner is already settled.",
        ] {
            assert!(
                !is_deferral_tail(safe, &roster),
                "ordinary or unconfigured ask was misclassified: {safe:?}"
            );
        }

        let overlap = FamilyVoiceRoster::from_names(["arc-2", "Arc"], std::iter::empty::<&str>());
        assert!(
            !is_deferral_tail("Let me ask Parcel about that.", &overlap),
            "a configured name must not match inside another name"
        );
    }

    /// A legitimate action-ack ("on it, I'll change the week") is NOT a deferral
    /// — the markers are specific enough not to swallow real commitments, and a
    /// plain answer with no tail is returned unchanged.
    #[test]
    fn deferral_guard_leaves_legitimate_replies_untouched() {
        let roster = fixture_deferral_roster();
        for ok in [
            "On it — I'll change the week to duck on Thursday.",
            "Pasta pomodoro is about 450 calories a plate.",
            "Sounds good, see you tonight!",
            "I'll add it to the shopping list right now.",
            "Let me ask a question about the recipe.",
        ] {
            assert_eq!(
                enforce_no_deferral(ok, &roster),
                ok,
                "a legitimate reply was mangled by the deferral guard:\n{ok}"
            );
        }
    }

    /// A reply that is ONLY a deferral (no body left) is returned unchanged — the
    /// guard never sends nothing.
    #[test]
    fn deferral_guard_never_empties_the_reply() {
        let roster = fixture_deferral_roster();
        let only = "Let me ask Blue Lantern's exact take.";
        assert_eq!(
            enforce_no_deferral(only, &roster),
            only,
            "must never strip to empty"
        );

        // strip_deferral_tail still reports the tail for a body-bearing reply,
        // and reports None when there is no deferral.
        let (body, tail) =
            strip_deferral_tail("It's 450 calories. Let me get her exact take.", &roster);
        assert!(body.contains("450"), "{body}");
        assert!(tail.is_some(), "tail should be detected");
        let (body2, tail2) = strip_deferral_tail("It's 450 calories, enjoy!", &roster);
        assert_eq!(body2, "It's 450 calories, enjoy!");
        assert!(tail2.is_none());
    }

    // -----------------------------------------------------------------------
    // FAMILY-VISIBLE ENGINE REPLY GATE
    // -----------------------------------------------------------------------

    fn fixture_voice_roster() -> FamilyVoiceRoster {
        FamilyVoiceRoster::from_names(
            ["hearth", "The Hearth", "wayfinder", "The Wayfinder"],
            ["Household Member"],
        )
    }

    #[test]
    fn family_voice_roster_uses_live_humans_not_prompt_seed_members() {
        let dir = tempfile::tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(dir.path().join("claw3d-bridge")).unwrap();
        std::fs::create_dir_all(wg.join("agency")).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[household]
members = ["Prompt Seed"]

[[agent]]
id = "hearth"
name = "The Hearth"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("claw3d-bridge").join("casa-gateway.toml"),
            r#"
[[humans]]
id = "human-fallback"
label = "Fallback Member"
"#,
        )
        .unwrap();

        let fallback = load_family_voice_roster(dir.path(), &wg);
        assert!(fallback.has_evidence);
        assert!(fallback.allows("The Hearth"));
        assert!(fallback.allows("Hearth"));
        assert!(!fallback.allows("The"), "articles are not name aliases");
        assert!(fallback.allows("Fallback Member"));
        assert!(!fallback.allows("Prompt Seed"));

        let agency_dir = wg.join("agency");
        let mut bindings = TelegramBindingMap::default();
        let pending = crate::agency::TelegramBinding::new(
            "123",
            "human-bound",
            "Bound Member",
            Some("hearth".to_string()),
            chrono::Utc::now(),
        );
        assert!(
            !pending.confirmed,
            "the live-roster proof must exercise a pending binding"
        );
        bindings.add(pending).unwrap();
        bindings.save(&agency_dir).unwrap();

        let live = load_family_voice_roster(dir.path(), &wg);
        assert!(live.allows("Bound Member"));
        assert!(live.allows("bound"));
        assert!(
            !live.allows("Fallback Member"),
            "agency bindings replace, rather than merge with, fallback humans"
        );
        assert!(
            !live.allows("Prompt Seed"),
            "household.members is prompt context, not roster evidence"
        );
        assert_eq!(
            scrub_off_roster_addressees(
                "Dinner is ready. We're waiting on you and Bound Member to confirm.",
                &live,
            ),
            "Dinner is ready. We're waiting on you and Bound Member to confirm.",
            "a pending live binding is a trusted human"
        );
        assert_eq!(
            scrub_off_roster_addressees(
                "Dinner is ready. We're waiting on you and Prompt Seed to confirm.",
                &live,
            ),
            "Dinner is ready.",
            "a stale prompt-seed member is not a live human"
        );
    }

    #[test]
    fn family_voice_self_attribution_is_roster_driven() {
        let roster = fixture_voice_roster();
        assert_eq!(
            strip_self_attribution("The Hearth 💬 Hi! All calm here.", &roster),
            "Hi! All calm here."
        );
        assert_eq!(
            strip_self_attribution("💬 The Hearth: All calm here.", &roster),
            "All calm here."
        );
        assert_eq!(
            strip_self_attribution("The Hearth says the room is calm.", &roster),
            "The Hearth says the room is calm.",
            "a name without an attribution separator is ordinary content"
        );
        let quoted = "“The Hearth: A Family Guide” is on the shelf.";
        assert_eq!(
            strip_self_attribution(quoted, &roster),
            quoted,
            "leading quotation punctuation is not an avatar"
        );

        let alias_roster =
            FamilyVoiceRoster::from_names(["fitness", "Coach Rowan"], ["Household Member"]);
        assert_eq!(
            strip_self_attribution("Rowan 💬 Let's take a walk.", &alias_roster),
            "Let's take a walk.",
            "a bare alias derived from an honorific persona name is attribution too",
        );
        assert_eq!(
            strip_self_attribution("Coach Rowan 💬 Let's take a walk.", &alias_roster,),
            "Let's take a walk.",
        );
        let ordinary = "Rowan says a walk sounds good.";
        assert_eq!(
            strip_self_attribution(ordinary, &alias_roster),
            ordinary,
            "a bare persona alias without an attribution separator stays ordinary content",
        );
    }

    #[test]
    fn family_voice_handoff_is_terminal_and_roster_driven() {
        let roster = fixture_voice_roster();
        assert_eq!(
            strip_handoff_tail("Dinner is ready. 🧭 The Wayfinder's got this one.", &roster),
            "Dinner is ready."
        );
        let mid = "I asked The Wayfinder and the plan is already settled.";
        assert_eq!(
            strip_handoff_tail(mid, &roster),
            mid,
            "a mid-sentence roster mention is not a handoff tail"
        );
        assert_eq!(
            strip_handoff_tail(
                "Dinner is ready. 🧭 The Wayfinder's got this one.",
                &FamilyVoiceRoster::default()
            ),
            "Dinner is ready. 🧭 The Wayfinder's got this one.",
            "without roster evidence no persona name is guessed"
        );
        assert_eq!(
            strip_handoff_tail("Dinner (easy). 🧭 The Wayfinder's got this one.", &roster),
            "Dinner (easy).",
            "handoff cleanup does not eat balanced punctuation"
        );
        assert_eq!(
            strip_handoff_tail(
                "Dinner is ready. The Wayfinder's your person for this.",
                &roster
            ),
            "Dinner is ready.",
            "the gateway's person-for-this handoff shape is covered"
        );
        assert_eq!(
            strip_handoff_tail("Dinner is ready. I'll hand this to The Wayfinder.", &roster,),
            "Dinner is ready.",
            "removing a first-person handoff does not strand its auxiliary"
        );
        assert_eq!(
            enforce_family_voice("The Wayfinder's got this one.", &roster),
            family_voice_fallback_line(),
            "a handoff-only draft becomes neutral instead of leaking intact"
        );
    }

    #[test]
    fn family_voice_handoff_never_matches_inside_a_human_name() {
        let roster = FamilyVoiceRoster::from_names(["Mira"], ["Samira"]);
        let human = "Samira's got this one.";
        assert_eq!(
            strip_handoff_tail(human, &roster),
            human,
            "a configured human name containing a persona suffix must remain whole",
        );
        assert_eq!(
            enforce_family_voice(human, &roster),
            human,
            "the full delivery guard must preserve the configured human too",
        );
        assert_eq!(
            strip_handoff_tail("Dinner is ready. Mira's got this one.", &roster),
            "Dinner is ready.",
            "the same phrase beginning with the standalone persona remains a handoff",
        );
    }

    #[test]
    fn family_voice_off_roster_scan_is_narrow_and_configuration_backed() {
        let roster = fixture_voice_roster();
        assert_eq!(
            scrub_off_roster_addressees(
                "Check with Zephyra before serving, then pass it to Household Member.",
                &roster
            ),
            "Check before serving, then pass it to Household Member."
        );
        assert_eq!(
            scrub_off_roster_addressees("Dinner is ready; Zephyra will join us.", &roster,),
            "Dinner is ready.",
            "a semicolon boundary preserves the grounded clause before a phantom claim",
        );
        let ordinary = "Dinner is ready; dessert follows, with fruit and cream.";
        assert_eq!(
            scrub_off_roster_addressees(ordinary, &roster),
            ordinary,
            "safe semicolon and comma punctuation stays byte-identical",
        );
        let unconfigured = FamilyVoiceRoster::default();
        let raw = "Check with Zephyra before serving.";
        assert_eq!(
            scrub_off_roster_addressees(raw, &unconfigured),
            raw,
            "an empty roster cannot prove a name is off-roster"
        );
        let ordinary = "Friday starts with a capital and stays untouched.";
        assert_eq!(
            scrub_off_roster_addressees(ordinary, &roster),
            ordinary,
            "ordinary capitalised words outside an addressee slot are untouched"
        );
        let date_slot = "Check with Friday before serving.";
        assert_eq!(
            scrub_off_roster_addressees(date_slot, &roster),
            date_slot,
            "a weekday in an addressee-shaped slot is not a person"
        );
        assert_eq!(
            scrub_off_roster_addressees(
                "Dinner is ready. We're waiting on Zephyra Moon to confirm.",
                &roster
            ),
            "Dinner is ready.",
            "a phantom-dependent waiting clause is removed without broken grammar"
        );
        assert_eq!(
            scrub_off_roster_addressees(
                "Dinner is ready. We're waiting on you and Quillon Vale to confirm.",
                &roster
            ),
            "Dinner is ready.",
            "a coordinated multiword phantom is removed without a hardcoded roster"
        );
        assert_eq!(
            scrub_off_roster_addressees("Dinner is ready. Pass it to Zephyra when warm.", &roster),
            "Dinner is ready.",
            "a phantom transfer clause is removed instead of leaving a bare verb"
        );
        assert_eq!(
            scrub_off_roster_addressees("Dinner is ready. Zephyra will join us.", &roster,),
            "Dinner is ready.",
            "a declarative phantom-person clause is removed instead of being stated as fact"
        );
        assert_eq!(
            scrub_off_roster_addressees("Dinner is ready. Household Member will join us.", &roster,),
            "Dinner is ready. Household Member will join us.",
            "the same declarative shape survives for a roster-listed person"
        );
        let weekday_action = "Friday will join the two lists.";
        assert_eq!(
            scrub_off_roster_addressees(weekday_action, &roster),
            weekday_action,
            "a date word is not reclassified as an off-roster person"
        );
        let clean_multiline = "Dinner is in progress.\nFriday still works.";
        assert_eq!(
            scrub_off_roster_addressees(clean_multiline, &roster),
            clean_multiline,
            "a clean reply takes the byte-for-byte fast path"
        );
    }

    #[test]
    fn family_voice_infra_guard_keeps_benign_system_language() {
        for leak in [
            "I'd need to pull that from the live gateway.",
            "That lives over in the pipeline.",
            "Let me query the database.",
        ] {
            assert!(has_infra_narration(leak), "should flag: {leak}");
        }
        for clean in [
            "We've got a good bedtime system.",
            "This soup is kind to the immune system.",
            "Want me to check our recipe book?",
        ] {
            assert!(!has_infra_narration(clean), "should preserve: {clean}");
        }
        assert_eq!(
            scrub_infra_narration("Dinner is ready. I'd need to pull that from the live gateway."),
            "Dinner is ready."
        );
    }

    #[test]
    fn family_voice_ops_and_markdown_are_plain_family_text() {
        assert_eq!(
            scrub_ops_jargon(
                "Next week is taking shape. Dispatcher healthy — 2 agents. \
                 6 in-progress. W31 is still a draft."
            ),
            "Next week is taking shape. next week is still a draft."
        );
        assert_eq!(
            scrub_ops_jargon("Dinner is in progress."),
            "Dinner is in progress.",
            "ordinary family progress is not a scheduler tally"
        );
        assert_eq!(
            strip_markdown("A peach is about **60 calories** — a *light* snack. Use `filter`."),
            "A peach is about 60 calories — a light snack. Use filter."
        );
        assert_eq!(strip_markdown("5*7 is 35"), "5*7 is 35");
    }

    #[test]
    fn family_voice_full_gate_composes_all_rules_without_leaking() {
        let roster = fixture_voice_roster();
        let raw = "**The Hearth** 💬 **Dinner is ready.** Check with **Zephyra** before serving. \
                   That lives over in the pipeline. **Service:** dispatcher healthy — 2 agents. \
                   🧭 The Wayfinder's got this one.";
        assert_eq!(
            enforce_family_voice(raw, &roster),
            "Dinner is ready. Check before serving."
        );
    }

    #[test]
    fn family_voice_only_preserves_the_exact_authorized_ownership_handoff() {
        let roster = fixture_voice_roster();
        let handoff = "The Wayfinder's got this one. 🧭";
        assert_eq!(
            enforce_family_voice(handoff, &roster),
            family_voice_fallback_line(),
            "composer-authored handoff-only copy must not pass through"
        );
        assert_eq!(
            enforce_family_voice_with(
                handoff,
                &roster,
                FamilyVoiceOptions {
                    authorized_handoff: Some(handoff),
                },
            ),
            handoff,
            "the exact engine-authored ownership line survives byte-for-byte"
        );
        let body = format!("**Dinner is ready.** The Hearth's got this one.\n\n{handoff}");
        assert_eq!(
            enforce_family_voice_with(
                &body,
                &roster,
                FamilyVoiceOptions {
                    authorized_handoff: Some(handoff),
                },
            ),
            format!("Dinner is ready.\n\n{handoff}"),
            "authorizing the final suffix must not authorize a composer handoff in the body"
        );
        assert_eq!(
            enforce_family_voice_with(
                handoff,
                &roster,
                FamilyVoiceOptions {
                    authorized_handoff: Some("The Wayfinder's got this one."),
                },
            ),
            family_voice_fallback_line(),
            "a near match is not an authorization"
        );
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

    fn historical_w30_dinner_context() -> String {
        "Requested historical dinner plan (2026-07-20 through 2026-07-26),\n\
         selected from the one exact indexed plan for that civil-date range. Answer the\n\
         dated request FROM these seven rows, in the order shown. Do not substitute the\n\
         current week, another week, or another day's row:\n\
         - Monday (Jul 20): Mushroom risotto, finished with spinach & lemon\n\
         - Tuesday (Jul 21): Pan-seared duck breast, roast potatoes & a quick salad\n\
         - Wednesday (Jul 22): No cooking \u{2014} Luca's out this evening\n\
         - Thursday (Jul 23): Pasta al pomodoro\n\
         - Friday (Jul 24): Pan seared pork\n\
         - Saturday (Jul 25): Pizza margherita, homemade dough\n\
         - Sunday (Jul 26): Clear-the-fridge frittata, greens folded through"
            .to_string()
    }

    fn historical_w30_workout_context() -> String {
        "WG_HISTORICAL_WORKOUT_CONTEXT_V1\n\
         week_key=2026-W30\n\
         range_start=2026-07-20\n\
         range_end=2026-07-26\n\
         row=2026-07-20|monday|07:00|Lower (strength)\n\
         row=2026-07-22|wednesday|07:00|Upper (push)\n\
         row=2026-07-24|friday|07:00|Upper (pull)\n\
         row=2026-07-26|sunday|10:00|Active recovery\n\
         END_WG_HISTORICAL_WORKOUT_CONTEXT_V1"
            .to_string()
    }

    #[test]
    fn historical_week_workout_reply_is_exact_and_row_fed() {
        let prompt = "Summarize the July 20\u{2013}26 training.";
        let context = historical_w30_workout_context();
        let local_date = NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        let expected = "The July 20-26 training had three lifting sessions and one active recovery. \
                        Monday lower at 7 a.m., Wednesday upper push at 7 a.m., \
                        Friday upper pull at 7 a.m., and Sunday active recovery at 10 a.m.";
        assert_eq!(
            historical_week_workout_reply(prompt, &context, local_date),
            HistoricalWorkoutReply::Grounded(expected.to_string()),
        );

        let changed_time = context.replacen("07:00|Upper (pull)", "08:30|Upper (pull)", 1);
        let HistoricalWorkoutReply::Grounded(changed_time_reply) =
            historical_week_workout_reply(prompt, &changed_time, local_date)
        else {
            panic!("mutated clock should remain structurally valid");
        };
        assert_ne!(changed_time_reply, expected);
        assert!(changed_time_reply.contains("8:30 a.m."));

        // An unclassified session keeps its exact row-derived label, while the
        // overview becomes a neutral count instead of treating every
        // non-recovery workout as lifting.
        for (source, replacement, expected_title) in [
            ("Upper (push)", "Upper (power)", "Wednesday upper power"),
            (
                "Active recovery",
                "Tempo run",
                "Sunday tempo run at 10 a.m.",
            ),
            (
                "Upper (push)",
                "Putting practice",
                "Wednesday putting practice at 7 a.m.",
            ),
        ] {
            let changed = context.replacen(source, replacement, 1);
            let HistoricalWorkoutReply::Grounded(changed_reply) =
                historical_week_workout_reply(prompt, &changed, local_date)
            else {
                panic!("mutated row should remain structurally valid: {source}");
            };
            assert!(
                changed_reply.starts_with("The July 20-26 training had four sessions."),
                "unknown workout was assigned a made-up category: {changed_reply}",
            );
            assert!(
                changed_reply.contains(expected_title),
                "new row title absent from {changed_reply}",
            );
            assert!(
                !changed_reply.contains("lifting session"),
                "an unknown workout was counted as lifting: {changed_reply}",
            );
        }
    }

    #[test]
    fn historical_workout_categories_require_positive_row_evidence() {
        assert_eq!(
            historical_workout_kind("Lower (strength)"),
            HistoricalWorkoutKind::Lifting,
        );
        assert_eq!(
            historical_workout_kind("Upper (push)"),
            HistoricalWorkoutKind::Lifting,
        );
        assert_eq!(
            historical_workout_kind("Upper (pull)"),
            HistoricalWorkoutKind::Lifting,
        );
        assert_eq!(
            historical_workout_kind("Active recovery"),
            HistoricalWorkoutKind::ActiveRecovery,
        );
        for title in ["Tempo run", "Putting practice", "Yoga", "Swim intervals"] {
            assert_eq!(
                historical_workout_kind(title),
                HistoricalWorkoutKind::Other,
                "{title:?} was relabelled as lifting",
            );
        }
    }

    #[test]
    fn historical_week_workout_reply_is_tri_state_and_fails_closed() {
        let prompt = "Summarize the July 20-26 workouts?";
        let context = historical_w30_workout_context();
        let local_date = NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        assert_eq!(
            historical_week_workout_reply("How was training?", &context, local_date),
            HistoricalWorkoutReply::NotApplicable,
        );
        assert_eq!(
            historical_week_workout_reply(prompt, "", local_date),
            HistoricalWorkoutReply::InvalidContext,
            "recognized request with absent evidence fell through",
        );

        let cases = [
            (
                "wrong request range",
                "Summarize the July 13-19 workouts?".to_string(),
                context.clone(),
            ),
            (
                "wrong week key",
                prompt.to_string(),
                context.replace("week_key=2026-W30", "week_key=2026-W31"),
            ),
            (
                "missing row",
                prompt.to_string(),
                context.replace("row=2026-07-24|friday|07:00|Upper (pull)\n", ""),
            ),
            (
                "duplicate or misdated row",
                prompt.to_string(),
                context.replace("2026-07-22|wednesday", "2026-07-20|wednesday"),
            ),
            (
                "rows out of order",
                prompt.to_string(),
                context
                    .replace("row=2026-07-20|monday|07:00|Lower (strength)", "__MONDAY__")
                    .replace(
                        "row=2026-07-22|wednesday|07:00|Upper (push)",
                        "row=2026-07-20|monday|07:00|Lower (strength)",
                    )
                    .replace("__MONDAY__", "row=2026-07-22|wednesday|07:00|Upper (push)"),
            ),
            (
                "extra line",
                prompt.to_string(),
                context.replace(
                    "END_WG_HISTORICAL_WORKOUT_CONTEXT_V1",
                    "note=untrusted\nEND_WG_HISTORICAL_WORKOUT_CONTEXT_V1",
                ),
            ),
            (
                "delimiter injection",
                prompt.to_string(),
                context.replace("Upper (push)", "Upper|push"),
            ),
        ];
        for (label, request, block) in cases {
            assert_eq!(
                historical_week_workout_reply(&request, &block, local_date),
                HistoricalWorkoutReply::InvalidContext,
                "{label} was accepted or fell through",
            );
        }

        let current_context = context
            .replace("week_key=2026-W30", "week_key=2026-W31")
            .replace("2026-07-20", "2026-07-27")
            .replace("2026-07-22", "2026-07-29")
            .replace("2026-07-24", "2026-07-31")
            .replace("2026-07-26", "2026-08-02");
        assert_eq!(
            historical_week_workout_reply(
                "Summarize the July 27-August 2 training.",
                &current_context,
                NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
            ),
            HistoricalWorkoutReply::InvalidContext,
            "current-week context was accepted as historical",
        );
    }

    #[test]
    fn historical_week_dinner_reply_is_exact_ordered_and_row_fed() {
        let prompt = "Give me the July 20\u{2013}26 dinners in order.";
        let context = historical_w30_dinner_context();
        let local_date = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let expected = "Monday was Mushroom risotto, finished with spinach and lemon. \
                        Tuesday was Pan-seared duck breast, roast potatoes and a quick salad. \
                        Wednesday, July 22 was out, no cooking \u{2014} Luca was out that evening. \
                        Thursday was Pasta al pomodoro. \
                        Friday was Pan seared pork. \
                        Saturday was Pizza margherita, homemade dough. \
                        Sunday was Clear-the-fridge frittata, greens folded through.";

        assert_eq!(
            historical_week_dinner_reply(prompt, &context, local_date).as_deref(),
            Some(expected),
        );

        let mutations = [
            (
                "Mushroom risotto, finished with spinach & lemon",
                "Porcini barley with parsley",
                "Mushroom risotto",
                "Porcini barley",
            ),
            (
                "Pan-seared duck breast, roast potatoes & a quick salad",
                "Roast aubergine with couscous",
                "Pan-seared duck breast",
                "Roast aubergine",
            ),
            (
                "No cooking \u{2014} Luca's out this evening",
                "No cooking \u{2014} Renata's out this afternoon",
                "Luca was out that evening",
                "Renata was out that afternoon",
            ),
            (
                "Pasta al pomodoro",
                "Pumpkin ravioli",
                "Pasta al pomodoro",
                "Pumpkin ravioli",
            ),
            (
                "Pan seared pork",
                "Grilled halloumi",
                "Pan seared pork",
                "Grilled halloumi",
            ),
            (
                "Pizza margherita, homemade dough",
                "Focaccia sandwiches",
                "Pizza margherita",
                "Focaccia sandwiches",
            ),
            (
                "Clear-the-fridge frittata, greens folded through",
                "Lentil soup with herbs",
                "Clear-the-fridge frittata",
                "Lentil soup",
            ),
        ];
        for (source, replacement, old_words, new_words) in mutations {
            let changed = context.replace(source, replacement);
            let changed_reply = historical_week_dinner_reply(prompt, &changed, local_date)
                .expect("changed row stays valid");
            assert_ne!(
                changed_reply, expected,
                "mutating `{source}` did not change the final",
            );
            assert!(
                !changed_reply.contains(old_words),
                "old row words `{old_words}` survived: {changed_reply}",
            );
            assert!(
                changed_reply.contains(new_words),
                "new row words `{new_words}` are absent: {changed_reply}",
            );
        }
    }

    #[test]
    fn historical_week_dinner_reply_rejects_current_iso_week_exact_block() {
        let prompt = "Give me the July 27\u{2013}August 2 dinners in order.";
        let context = "Requested historical dinner plan (2026-07-27 through 2026-08-02),\n\
                       selected from the one exact indexed plan for that civil-date range. Answer the\n\
                       dated request FROM these seven rows, in the order shown. Do not substitute the\n\
                       current week, another week, or another day's row:\n\
                       - Monday (Jul 27): Lentil soup\n\
                       - Tuesday (Jul 28): Roast aubergine\n\
                       - Wednesday (Jul 29): Pasta primavera\n\
                       - Thursday (Jul 30): Chicken thighs\n\
                       - Friday (Jul 31): Baked fish\n\
                       - Saturday (Aug 1): Homemade pizza\n\
                       - Sunday (Aug 2): Vegetable frittata";

        assert_eq!(
            historical_week_dinner_reply(
                prompt,
                context,
                NaiveDate::from_ymd_opt(2026, 7, 30).unwrap(),
            ),
            None,
            "a current-week block was accepted as historical",
        );
        assert!(
            historical_week_dinner_reply(
                prompt,
                context,
                NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            )
            .is_some(),
            "the exact same block should become historical after that ISO week",
        );
    }

    #[test]
    fn historical_week_dinner_reply_fails_closed_on_any_incomplete_or_misaligned_block() {
        let prompt = "Give me the July 20\u{2013}26 dinners in order.";
        let context = historical_w30_dinner_context();
        let local_date = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let sunday = "- Sunday (Jul 26): Clear-the-fridge frittata, greens folded through";
        let tuesday = "- Tuesday (Jul 21): Pan-seared duck breast, roast potatoes & a quick salad";
        let wednesday = "- Wednesday (Jul 22): No cooking \u{2014} Luca's out this evening";

        let cases = [
            (
                "wrong request range",
                "Give me the July 27\u{2013}August 2 dinners in order.".to_string(),
                context.clone(),
            ),
            (
                "non-historical context",
                prompt.to_string(),
                context.replacen(
                    "Requested historical dinner plan",
                    "This week's dinner plan",
                    1,
                ),
            ),
            (
                "missing row",
                prompt.to_string(),
                context.replace(&format!("\n{sunday}"), ""),
            ),
            (
                "duplicate weekday/date",
                prompt.to_string(),
                context.replace(
                    tuesday,
                    "- Monday (Jul 20): Pan-seared duck breast, roast potatoes & a quick salad",
                ),
            ),
            (
                "rows out of order",
                prompt.to_string(),
                context
                    .replace(tuesday, "__TUESDAY__")
                    .replace(wednesday, tuesday)
                    .replace("__TUESDAY__", wednesday),
            ),
            (
                "wrong row date",
                prompt.to_string(),
                context.replace(
                    wednesday,
                    "- Wednesday (Jul 23): No cooking \u{2014} Luca's out this evening",
                ),
            ),
            (
                "eighth row",
                prompt.to_string(),
                format!("{context}\n- Monday (Jul 20): Duplicate"),
            ),
        ];

        for (label, request, block) in cases {
            assert_eq!(
                historical_week_dinner_reply(&request, &block, local_date),
                None,
                "{label} was accepted",
            );
        }
    }

    #[test]
    fn parse_week_context_records_only_planned_days() {
        let wc = parse_week_context(&sample_week_context());
        assert!(!wc.is_empty());
        assert_eq!(
            wc.by_day.get("friday").map(String::as_str),
            Some("Chicken tray bake")
        );
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
        assert_eq!(
            claims.len(),
            1,
            "expected one false-empty claim, got {claims:?}"
        );
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
        assert!(
            false_empty_week_claims("Nothing's locked in for Saturday yet.", &empty).is_empty()
        );
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
        assert!(lower.contains("from this list"), "{block}");
        // SLOT-COMPLETE (task meal-read-lane): the instruction covers lunch and the
        // no-cook nights too. A dinner-only instruction is what let a lunch question be
        // answered with a dinner row, and a Wednesday-out question with another day's dish.
        assert!(lower.contains("dinner or lunch"), "{block}");
        assert!(lower.contains("keep the slot"), "{block}");
        assert!(lower.contains("no cooking"), "{block}");
    }

    // -----------------------------------------------------------------------
    // WRONG-PLACEMENT claims (task meal-claim-slot) — live-cert C004
    // -----------------------------------------------------------------------

    /// The C004 week: the frittata is on SUNDAY, and Sunday is TODAY — so "that
    /// frittata tomorrow" moves it to Monday, and "for lunch" moves the slot.
    /// Saturday and Monday carry distinct dishes so a mis-attributed match is loud.
    fn c004_week_context() -> String {
        "This week's dinners, parsed from the family plan's Dinners table:\n\
         - Saturday (July 25): Sheet-pan margherita pizza\n\
         - Sunday (July 26): Zucchini & potato frittata\n\
         - Monday (July 27): Chickpea & spinach curry\n\
         Today is Sunday — dinner: Zucchini & potato frittata.\n\
         Tomorrow is Monday — dinner: Chickpea & spinach curry."
            .to_string()
    }

    /// THE C004 BUG, exactly as it went out: Bruno told Luca to enjoy "that
    /// frittata tomorrow … for lunch" while the plan holds it SUNDAY at DINNER.
    /// The claim moved BOTH day and slot; the guard names the truthful placement
    /// and leaves the rest of the reply — greeting, sign-off emoji — untouched.
    #[test]
    fn misplaced_claim_moving_day_and_slot_is_rewritten_to_the_truth() {
        let wc = parse_week_context(&c004_week_context());
        let draft = "Hope the weekend's treating you well, Luca. Enjoy that frittata tomorrow — perfect for lunch! 🍳";
        let claims = misplaced_week_claims(draft, &wc);
        assert_eq!(
            claims.len(),
            1,
            "expected one misplaced claim, got {claims:?}"
        );
        assert_eq!(claims[0].true_day, "Sunday");
        assert_eq!(
            claims[0].claimed_day.as_deref(),
            Some("Monday"),
            "{claims:?}"
        );
        assert_eq!(
            claims[0].claimed_slot.as_deref(),
            Some("lunch"),
            "{claims:?}"
        );
        let fixed = week_placement_rewrite(draft, &claims);
        assert!(
            fixed.contains("Sunday's dinner is Zucchini & potato frittata."),
            "the truthful placement must land:\n{fixed}"
        );
        assert!(
            !fixed.contains("tomorrow") && !fixed.contains("lunch"),
            "the moved day/slot claim survived:\n{fixed}"
        );
        assert!(
            fixed.starts_with("Hope the weekend's treating you well, Luca."),
            "the rest of the reply must survive byte-for-byte:\n{fixed}"
        );
        assert!(fixed.ends_with("🍳"), "the sign-off must survive:\n{fixed}");
    }

    /// A CASUAL mention asserts no placement — no day, no slot — so there is
    /// nothing to contradict and the reply passes through untouched.
    #[test]
    fn casual_dish_mention_without_a_placement_claim_passes_untouched() {
        let wc = parse_week_context(&c004_week_context());
        let draft = "That frittata was a hit — glad it landed. 🍳";
        assert!(
            misplaced_week_claims(draft, &wc).is_empty(),
            "a casual mention must not be flagged"
        );
        assert_eq!(week_placement_rewrite(draft, &[]), draft);
    }

    /// A TRUTHFUL placement (the real day, the real slot) is never touched.
    #[test]
    fn truthful_placement_is_left_alone() {
        let wc = parse_week_context(&c004_week_context());
        for draft in [
            "Sunday's dinner is the zucchini & potato frittata.",
            "Tonight's frittata is on the stove at 18:30.",
            "The frittata is dinner tonight, not lunch.",
        ] {
            assert!(
                misplaced_week_claims(draft, &wc).is_empty(),
                "a truthful placement was flagged: {draft}"
            );
        }
    }

    /// WRONG SLOT ALONE — right day, wrong meal. The table is the DINNERS table,
    /// so "the frittata's your lunch" misplaces the slot even with no day named.
    #[test]
    fn wrong_slot_alone_is_corrected() {
        let wc = parse_week_context(&c004_week_context());
        let draft = "The frittata's your lunch.";
        let claims = misplaced_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].claimed_slot.as_deref(), Some("lunch"));
        assert_eq!(claims[0].claimed_day, None, "no day was claimed");
        assert_eq!(
            week_placement_rewrite(draft, &claims),
            "Sunday's dinner is Zucchini & potato frittata."
        );
    }

    /// WRONG DAY ALONE — the dish is right and the slot is right, but the day is
    /// moved (a named weekday, not a relative one).
    #[test]
    fn wrong_day_alone_is_corrected() {
        let wc = parse_week_context(&c004_week_context());
        let draft = "Saturday's dinner is the zucchini frittata.";
        let claims = misplaced_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].claimed_day.as_deref(), Some("Saturday"));
        assert_eq!(claims[0].true_day, "Sunday");
        assert_eq!(
            week_placement_rewrite(draft, &claims),
            "Sunday's dinner is Zucchini & potato frittata."
        );
    }

    /// LEFTOVERS are not a placement claim — eating Sunday's frittata again on
    /// Monday for lunch is true, and the guard must not "correct" it.
    #[test]
    fn leftovers_line_is_left_alone() {
        let wc = parse_week_context(&c004_week_context());
        assert!(
            misplaced_week_claims("The leftover frittata makes a great lunch tomorrow.", &wc)
                .is_empty(),
            "a leftovers line must not be rewritten"
        );
    }

    /// TEETH against false positives: generic prep/filler words in a dish never
    /// identify it, so an unrelated sentence that happens to share one is safe.
    #[test]
    fn generic_prep_words_never_identify_a_dish() {
        let wc = parse_week_context(
            "- Friday (July 24): Pan-seared salmon over warm Puy lentils\n\
             Today is Thursday — dinner: Halloumi traybake.\n\
             Tomorrow is Friday — dinner: Pan-seared salmon over warm Puy lentils.",
        );
        assert!(!wc.is_empty(), "the fixture must hold Friday's dish");
        for draft in [
            "Let's keep Monday warm and easy — sheet pan, one bowl, done.",
            "I'll serve lunch on the tray Saturday.",
        ] {
            assert!(
                misplaced_week_claims(draft, &wc).is_empty(),
                "filler-word overlap must never trip the guard: {draft}"
            );
        }
    }

    /// A PROPOSAL or a QUESTION asserts nothing about this week's table — the cook's
    /// own offer to move or repeat a dish must survive verbatim.
    #[test]
    fn a_proposal_or_question_is_not_a_placement_claim() {
        let wc = parse_week_context(&c004_week_context());
        for draft in [
            "Want me to put the frittata on Wednesday next week?",
            "We could do that frittata again next week, maybe Tuesday.",
            "Should I move the frittata to Saturday lunch?",
            "How about the frittata for Monday instead?",
        ] {
            assert!(
                misplaced_week_claims(draft, &wc).is_empty(),
                "a proposal must not be rewritten: {draft}"
            );
        }
        // TEETH: the SAME dish and day asserted flatly IS still corrected, so the
        // proposal carve-out is not a blanket amnesty.
        let flat = "The frittata is on Wednesday.";
        assert_eq!(
            misplaced_week_claims(flat, &wc).len(),
            1,
            "an assertive wrong-day line must still be caught"
        );
    }

    /// AMBIGUITY is left alone: when the named words fit TWO days' dishes there is
    /// no single truthful placement to state, so the reply is not rewritten.
    #[test]
    fn ambiguous_dish_across_two_days_is_left_alone() {
        let wc = parse_week_context(
            "- Tuesday (July 21): Grilled salmon skewers\n\
             - Friday (July 24): Salmon rice bowls\n\
             Today is Sunday — dinner: not planned yet.\n\
             Tomorrow is Monday — dinner: not planned yet.",
        );
        assert!(
            misplaced_week_claims("The salmon is tomorrow, for lunch.", &wc).is_empty(),
            "an ambiguous dish match must not be rewritten to a guessed day"
        );
    }

    /// No forwarded context → the placement guard is a no-op (Telegram-listener
    /// path, env unset), and an unresolvable relative day claims nothing.
    #[test]
    fn placement_guard_is_a_noop_without_a_forwarded_context() {
        let empty = parse_week_context("");
        let draft = "Enjoy that frittata tomorrow — perfect for lunch!";
        assert!(misplaced_week_claims(draft, &empty).is_empty());
        assert_eq!(week_placement_rewrite(draft, &[]), draft);
        // A table with no today/tomorrow markers cannot resolve "tomorrow", so a
        // relative-day claim is not treated as naming a (wrong) day.
        let no_markers = parse_week_context("- Sunday (July 26): Zucchini & potato frittata");
        let claims = misplaced_week_claims("Enjoy that frittata tomorrow.", &no_markers);
        assert!(
            claims.is_empty(),
            "an unresolvable relative day must not be guessed: {claims:?}"
        );
    }

    /// The rewrite is IDEMPOTENT and composes with the never-claim-empty rewrite:
    /// both guards emit "<Day>'s dinner is <dish>.", which names the real day, so a
    /// second pass finds nothing to fix.
    #[test]
    fn placement_rewrite_is_idempotent_and_composes_with_never_claim_empty() {
        let wc = parse_week_context(&c004_week_context());
        let draft = "Enjoy that frittata tomorrow for lunch!";
        let once = week_placement_rewrite(draft, &misplaced_week_claims(draft, &wc));
        let twice = week_placement_rewrite(&once, &misplaced_week_claims(&once, &wc));
        assert_eq!(once, twice, "the rewrite must be idempotent");
        // The never-claim-empty rewrite's own output is likewise stable here.
        let false_empty = false_empty_week_claims("Nothing's planned for tonight.", &wc);
        assert_eq!(false_empty.len(), 1, "{false_empty:?}");
        let honest = week_grounding_rewrite(&false_empty);
        assert!(
            misplaced_week_claims(&honest, &wc).is_empty(),
            "the honest never-claim-empty line must not be re-flagged: {honest}"
        );
    }

    // -----------------------------------------------------------------------
    // SLOT-COMPLETE week context (task meal-read-lane) — live-cert run 2, C023/C024
    // -----------------------------------------------------------------------

    /// The live W30 week as the gateway now forwards it: the unchanged "- " DINNER
    /// rows, plus "\u{2022} "-bulleted lines for the lunches the plan states and the night
    /// with no cooking. Saturday carries BOTH slots — pizza at dinner, panzanella at
    /// lunch — which is the pair the run-2 P0 confused.
    fn w30_slotted_context() -> String {
        "This week's meals, parsed from the family plan:\n\
         - Wednesday (Jul 22): No cooking \u{2014} we're out this evening\n\
         - Saturday (Jul 25): Pizza margherita, homemade dough\n\
         - Sunday (Jul 26): Clear-the-fridge frittata, greens folded through\n\
         Lunches the plan names \u{2014} a day not listed here has no lunch planned at home:\n\
         \u{2022} Saturday (Jul 25) lunch: a big tomato-and-bread panzanella\n\
         \u{2022} Sunday (Jul 26) lunch: a simple soup or a cheese-and-tomato toastie\n\
         Nights with NO cooking planned \u{2014} answer these as themselves, never with another\n\
         day's dish:\n\
         \u{2022} Wednesday (Jul 22): out \u{2014} no cooking (No cooking \u{2014} we're out this evening)\n\
         Today is Sunday \u{2014} dinner: Clear-the-fridge frittata, greens folded through.\n\
         Tomorrow is Monday \u{2014} no dinner planned yet."
            .to_string()
    }

    /// The BIGGER BLOCK PASSES THROUGH: the dinner map the engine always parsed is
    /// byte-for-byte what it was, and the new bulleted lines add the lunches and the
    /// no-cook night rather than corrupting a dinner row.
    #[test]
    fn parse_week_context_reads_lunches_and_no_cook_nights_without_disturbing_dinners() {
        let wc = parse_week_context(&w30_slotted_context());
        assert_eq!(
            wc.by_day.get("saturday").map(String::as_str),
            Some("Pizza margherita, homemade dough"),
            "the DINNER rows must parse exactly as before"
        );
        assert_eq!(
            wc.lunch_by_day.get("saturday").map(String::as_str),
            Some("a big tomato-and-bread panzanella")
        );
        assert_eq!(
            wc.lunch_by_day.get("sunday").map(String::as_str),
            Some("a simple soup or a cheese-and-tomato toastie")
        );
        assert!(
            !wc.lunch_by_day.contains_key("wednesday"),
            "no lunch may be invented"
        );
        assert!(
            wc.non_cook.contains("wednesday"),
            "the no-cook night is marked"
        );
        assert_eq!(wc.today.as_deref(), Some("sunday"));
        assert_eq!(wc.tomorrow.as_deref(), Some("monday"));
        // FORWARD COMPATIBILITY IN BOTH DIRECTIONS: the dinner-only block an older
        // gateway sends still parses, and carries no lunches rather than failing.
        let old = parse_week_context(&c004_week_context());
        assert_eq!(old.lunch_by_day.len(), 0);
        assert!(old.non_cook.is_empty());
        assert!(!old.is_empty());
    }

    /// A TRUE lunch placement survives. Before the lunches rode in the context the
    /// guard had to treat every non-dinner slot as wrong, so a correct answer to a
    /// lunch question would have been "corrected" into a dinner.
    #[test]
    fn a_true_lunch_placement_is_never_rewritten() {
        let wc = parse_week_context(&w30_slotted_context());
        for draft in [
            "Saturday's lunch was a big tomato-and-bread panzanella.",
            "The panzanella is Saturday's lunch.",
            "We had the panzanella for lunch.",
        ] {
            assert!(
                misplaced_week_claims(draft, &wc).is_empty(),
                "a true lunch line was rewritten: {draft}"
            );
        }
    }

    /// …and a dish placed at a slot the plan does NOT give it is still corrected, in
    /// both directions, with the truth line naming the real slot.
    #[test]
    fn a_dish_at_the_wrong_slot_or_day_is_corrected_with_its_real_slot() {
        let wc = parse_week_context(&w30_slotted_context());
        // The C024 lie said in full: Saturday's DINNER offered as the lunch.
        let draft = "Saturday's lunch was pizza margherita with homemade dough.";
        let claims = misplaced_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].true_slot, "dinner");
        assert_eq!(claims[0].claimed_slot.as_deref(), Some("lunch"));
        assert_eq!(
            week_placement_rewrite(draft, &claims),
            "Saturday's dinner is Pizza margherita, homemade dough."
        );
        // The mirror image: a LUNCH dish moved to another day.
        let moved = "Sunday's lunch was the panzanella.";
        let claims = misplaced_week_claims(moved, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].true_day, "Saturday");
        assert_eq!(claims[0].true_slot, "lunch");
        assert_eq!(
            week_placement_rewrite(moved, &claims),
            "Saturday's lunch is a big tomato-and-bread panzanella.",
            "the truth line must name the LUNCH it really is, not a dinner it never was"
        );
    }

    /// A NO-COOK night is not a dish. Treating "No cooking \u{2014} we're out this evening" as
    /// one lets any sentence carrying "cooking"/"evening" be named by it and rewritten.
    #[test]
    fn a_no_cook_night_is_not_a_dish() {
        let wc = parse_week_context(&w30_slotted_context());
        for draft in [
            "I'll get the cooking started early Saturday.",
            "Wednesday is a night out \u{2014} no cooking planned.",
        ] {
            assert!(
                misplaced_week_claims(draft, &wc).is_empty(),
                "a no-cook row was treated as a dish: {draft}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // PENDING DINNERS (task engine-twin-grounding) — docs/20 §6 rule 12, the
    // engine twin of composerGuard §6.14.
    // -----------------------------------------------------------------------

    /// THE REAL BLOCK, byte-for-byte as `weekSource.buildWeekContext` emits it for a
    /// week whose Friday and Sunday dinners carry the plan's ⏳ and whose Saturday
    /// does not (meals fixture + `nowMs` = 2026-07-24T12:00:00Z, TZ=UTC). Saturday is
    /// the negative control that proves the guard is keyed on the FLAG and not on "is
    /// this a dinner". The byte identity is not decorative: it is re-derived from the
    /// live gateway builder by `tests/smoke/scenarios/engine_pending_dinner_cross_impl.sh`,
    /// so a wording change on either side of the wire fails loudly instead of silently
    /// muting this guard.
    fn pending_week_context() -> String {
        "This week's meals, parsed from the family plan. Answer any meal question (today,\n\
         tomorrow, a named day, dinner OR lunch) FROM this list \u{2014} never from the plan's prose\n\
         notes or a week \"skeleton\", and never from another day's row. If a day below has an\n\
         entry, that day is planned:\n\
         - Friday (Jul 24): Salmon over warm Puy lentils\n\
         - Saturday (Jul 25): Sheet-pan margherita pizza\n\
         - Sunday (Jul 26): Clear-the-fridge frittata\n\
         These dinners are LINED UP but the family has NOT agreed to them yet (the plan\n\
         marks each with \u{23f3} and the Week view shows it as a \"needs your OK\" chip). Treat\n\
         them as PROPOSALS: name the day and the dish if asked, say plainly that it still\n\
         needs their OK, and never report one as settled, sorted, locked in or good to go:\n\
         \u{2022} Awaiting the family's OK \u{2014} Friday (Jul 24): Salmon over warm Puy lentils\n\
         \u{2022} Awaiting the family's OK \u{2014} Sunday (Jul 26): Clear-the-fridge frittata\n\
         The plan names no lunches this week \u{2014} say so plainly if asked; do not answer a\n\
         lunch question with a dinner.\n\
         Today is Friday \u{2014} dinner: Salmon over warm Puy lentils.\n\
         Tomorrow is Saturday \u{2014} dinner: Sheet-pan margherita pizza."
            .to_string()
    }

    /// The drafts the cross-impl gate compares the two implementations on. Each is a
    /// live reply SHAPE, not a synthetic string: a settled claim, a flat listing, the
    /// settled-dinner control, the four truthful forms, a casual mention, a multi-dish
    /// listing, and the moved-dish chain (§6.12 then rule 12).
    fn pending_parity_drafts() -> Vec<&'static str> {
        vec![
            "Friday's dinner is the salmon over Puy lentils \u{2014} all set. \u{1f373}",
            "Sunday: clear-the-fridge frittata.",
            "Saturday's dinner is the sheet-pan margherita pizza \u{2014} all set. \u{1f355}",
            "I've lined up salmon over Puy lentils for Friday, but it still needs your OK.",
            "Friday's salmon is pencilled in \u{2014} happy with it, or shall I swap it?",
            "Want me to put the clear-the-fridge frittata on Sunday?",
            "Sunday's frittata is a proposal \u{2014} say the word and I'll lock it in.",
            "That salmon was a hit last time. \u{1f373}",
            "This week's dinners: Friday salmon over Puy lentils, Saturday pizza, Sunday the clear-the-fridge frittata.",
            "Enjoy that clear-the-fridge frittata tomorrow \u{2014} perfect for lunch! \u{1f373}",
        ]
    }

    /// The guard chain `finalize_composed_reply` runs, in its order: the placement
    /// rewrite first, then rule 12 ON ITS OUTPUT. Used by the parity fixture so the
    /// cross-impl comparison is against the JS `finalizeComposedReply` chain and not
    /// against one function in isolation.
    fn pending_guard_chain(draft: &str, wc: &WeekContext) -> String {
        let text = week_placement_rewrite(draft, &misplaced_week_claims(draft, wc));
        week_pending_rewrite(&text, &pending_week_claims(&text, wc))
    }

    /// THE READ HALF. The gateway states its ⏳ rows in words; the engine used to drop
    /// them on the floor (its "• " branch records only a lunch head or a "no cooking"
    /// dish), which is exactly what made the gateway's half safe to ship alone. They
    /// are read now, day and dish, in the plan's own order.
    #[test]
    fn parse_week_context_reads_the_pending_rows_the_gateway_states() {
        let wc = parse_week_context(&pending_week_context());
        assert_eq!(
            wc.pending,
            vec![
                PendingWeekRow {
                    day: "Friday".to_string(),
                    slot: "dinner".to_string(),
                    dish: "Salmon over warm Puy lentils".to_string(),
                },
                PendingWeekRow {
                    day: "Sunday".to_string(),
                    slot: "dinner".to_string(),
                    dish: "Clear-the-fridge frittata".to_string(),
                },
            ],
            "the \u{23f3} rows must be read, in the plan's order"
        );
        // The SETTLED dinner is not among them — the flag is the key.
        assert!(
            !wc.pending.iter().any(|p| p.day == "Saturday"),
            "a settled dinner was recorded as pending: {:?}",
            wc.pending
        );
    }

    /// …AND THE DINNER MAP IS UNCHANGED BY THOSE LINES. A pending row is a QUALIFIER
    /// on the "- " dinner row, not a second dinner: if it leaked into `by_day` the
    /// placement guard would see two rows for one day and a truthful reply could start
    /// being "corrected". Proven against the same block with the pending lines
    /// deleted, so this is an identity, not a spot check.
    #[test]
    fn pending_rows_leave_the_dinner_map_untouched() {
        let full = pending_week_context();
        let without: String = full
            .lines()
            .filter(|l| !l.trim_start().starts_with("\u{2022} Awaiting"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            without.len() < full.len(),
            "the fixture must actually carry pending lines"
        );
        let wc = parse_week_context(&full);
        let bare = parse_week_context(&without);
        let mut got: Vec<(&String, &String)> = wc.by_day.iter().collect();
        let mut want: Vec<(&String, &String)> = bare.by_day.iter().collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "the pending lines changed the DINNER map");
        assert_eq!(wc.by_day.len(), 3, "{:?}", wc.by_day);
        assert_eq!(
            wc.by_day.get("friday").map(String::as_str),
            Some("Salmon over warm Puy lentils")
        );
        // …and they invent no lunch and no no-cook night either.
        assert!(wc.lunch_by_day.is_empty(), "{:?}", wc.lunch_by_day);
        assert!(wc.non_cook.is_empty(), "{:?}", wc.non_cook);
        assert_eq!(wc.today.as_deref(), Some("friday"));
        assert_eq!(wc.tomorrow.as_deref(), Some("saturday"));
        // The pre-pending block parses to NO pending rows, so an older gateway (and
        // every settled week) leaves this guard inert.
        assert!(bare.pending.is_empty(), "{:?}", bare.pending);
        assert!(parse_week_context(&c004_week_context()).pending.is_empty());
    }

    /// THE LIE RULE 12 FORBIDS: a dinner nobody agreed to, reported as decided. The
    /// offending sentence is spliced with the truth; the persona's glyph survives.
    #[test]
    fn a_pending_dinner_asserted_as_settled_is_corrected() {
        let wc = parse_week_context(&pending_week_context());
        let draft = "Friday's dinner is the salmon over Puy lentils \u{2014} all set. \u{1f373}";
        let claims = pending_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(claims[0].days, vec!["Friday".to_string()]);
        assert_eq!(claims[0].rows.len(), 1);
        let fixed = week_pending_rewrite(draft, &claims);
        assert_eq!(
            fixed,
            "Friday's dinner is Salmon over warm Puy lentils, but it still needs your OK. \u{1f373}"
        );
        assert!(fixed.ends_with('\u{1f373}'), "{fixed}");
        // IDEMPOTENT: the correction itself acknowledges the pending state.
        assert!(
            pending_week_claims(&fixed, &wc).is_empty(),
            "the correction was re-flagged"
        );
    }

    /// A FLAT LISTING is an assertion too — no settled vocabulary at all, the lie is
    /// that a proposal is stated with exactly the confidence of a decision. This is
    /// the live shape rule 12 was written for.
    #[test]
    fn a_flat_listing_of_a_pending_dinner_is_an_assertion_too() {
        let wc = parse_week_context(&pending_week_context());
        let draft = "Sunday: clear-the-fridge frittata.";
        let claims = pending_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(
            week_pending_rewrite(draft, &claims),
            "Sunday's dinner is Clear-the-fridge frittata, but it still needs your OK."
        );
        assert_eq!(
            week_pending_truth_line(&PendingWeekRow {
                day: "Sunday".to_string(),
                slot: "dinner".to_string(),
                dish: "Clear-the-fridge frittata".to_string(),
            }),
            "Sunday's dinner is Clear-the-fridge frittata, but it still needs your OK."
        );
    }

    /// THE NEGATIVE CONTROL. A SETTLED dinner said in exactly the flagged shapes is
    /// delivered as composed — and a week with nothing pending can never fire,
    /// whatever the reply says.
    #[test]
    fn pending_guard_never_touches_a_settled_dinner_the_flag_is_the_key() {
        let wc = parse_week_context(&pending_week_context());
        for draft in [
            "Saturday's dinner is the sheet-pan margherita pizza \u{2014} all set. \u{1f355}",
            "Saturday: sheet-pan margherita pizza.",
        ] {
            assert!(
                pending_week_claims(draft, &wc).is_empty(),
                "flagged a settled dinner: {draft}"
            );
            assert_eq!(week_pending_rewrite(draft, &[]), draft);
        }
        let settled: String = pending_week_context()
            .lines()
            .filter(|l| !l.trim_start().starts_with("\u{2022} Awaiting"))
            .collect::<Vec<_>>()
            .join("\n");
        let settled = parse_week_context(&settled);
        assert!(
            pending_week_claims(
                "Friday's dinner is the salmon over Puy lentils \u{2014} all set.",
                &settled
            )
            .is_empty(),
            "a week with no \u{23f3} row fired the guard"
        );
        // The env-unset path (no block at all) is inert too.
        assert!(
            pending_week_claims("Friday's salmon is all set.", &parse_week_context("")).is_empty()
        );
    }

    /// A TRUTHFUL reply about the same pending dinner passes untouched — the honest
    /// forms are what the guard steers toward, so rewriting one would be a regression
    /// AND would break idempotence.
    #[test]
    fn a_truthful_pending_reply_passes_untouched() {
        let wc = parse_week_context(&pending_week_context());
        for draft in [
            "I've lined up salmon over Puy lentils for Friday, but it still needs your OK.",
            "Friday's salmon is pencilled in \u{2014} happy with it, or shall I swap it?",
            "Want me to put the clear-the-fridge frittata on Sunday?",
            "Sunday's frittata is a proposal \u{2014} say the word and I'll lock it in.",
            // A casual, past-tense mention claims nothing about the plan either.
            "That salmon was a hit last time. \u{1f373}",
        ] {
            assert!(
                pending_week_claims(draft, &wc).is_empty(),
                "flagged a truthful line: {draft}"
            );
            assert_eq!(week_pending_rewrite(draft, &[]), draft);
        }
    }

    /// TWO pending dishes in one sentence: there is no single truth to substitute, so
    /// the sentence survives and the reply gains the fast read lane's own clause.
    #[test]
    fn a_multi_dish_pending_assertion_gains_the_note_clause() {
        let wc = parse_week_context(&pending_week_context());
        let draft = "This week's dinners: Friday salmon over Puy lentils, Saturday pizza, Sunday the clear-the-fridge frittata.";
        let claims = pending_week_claims(draft, &wc);
        assert_eq!(claims.len(), 1, "{claims:?}");
        assert_eq!(
            claims[0].days,
            vec!["Friday".to_string(), "Sunday".to_string()]
        );
        let fixed = week_pending_rewrite(draft, &claims);
        assert_eq!(
            fixed,
            format!("{draft} Friday and Sunday still need your OK.")
        );
        assert_eq!(
            week_pending_note_line(&["Friday".to_string(), "Sunday".to_string()]),
            "Friday and Sunday still need your OK."
        );
        assert_eq!(
            week_pending_note_line(&["Friday".to_string()]),
            "Friday still needs your OK."
        );
        // IDEMPOTENT: the appended clause is itself an acknowledgement.
        assert!(pending_week_claims(&fixed, &wc).is_empty(), "{fixed}");
    }

    /// THE CHAIN, in `finalize_composed_reply`'s order. §6.12 fires first and states a
    /// bare "Sunday's dinner is …" — which on a ⏳ row is itself the settled claim rule
    /// 12 forbids, so rule 12 must run ON ITS OUTPUT or the guard chain manufactures
    /// the very lie it exists to stop.
    #[test]
    fn a_moved_pending_dish_is_put_back_and_kept_a_proposal() {
        let wc = parse_week_context(&pending_week_context());
        let draft =
            "Enjoy that clear-the-fridge frittata tomorrow \u{2014} perfect for lunch! \u{1f373}";
        let moved = misplaced_week_claims(draft, &wc);
        assert_eq!(moved.len(), 1, "{moved:?}");
        let placed = week_placement_rewrite(draft, &moved);
        assert_eq!(
            placed, "Sunday's dinner is Clear-the-fridge frittata. \u{1f373}",
            "the placement guard's own output states it as settled"
        );
        let fixed = pending_guard_chain(draft, &wc);
        assert_eq!(
            fixed,
            "Sunday's dinner is Clear-the-fridge frittata, but it still needs your OK. \u{1f373}"
        );
        assert!(!fixed.to_lowercase().contains("tomorrow"), "{fixed}");
    }

    /// THE CROSS-IMPL FIXTURE. Prints the block and every draft's chained output so
    /// `tests/smoke/scenarios/engine_pending_dinner_cross_impl.sh` can diff this
    /// implementation against the JS twin's `finalizeComposedReply` on the SAME bytes.
    /// It asserts too — a printer that asserted nothing could print anything — but the
    /// judgement of parity is the scenario's, run with `-- --nocapture`.
    #[test]
    fn pending_parity_fixture_prints_both_the_block_and_every_output() {
        let block = pending_week_context();
        let wc = parse_week_context(&block);
        assert_eq!(wc.pending.len(), 2, "the fixture must carry \u{23f3} rows");
        println!(
            "PARITY-BLOCK {}",
            serde_json::to_string(&block).expect("block json")
        );
        let mut corrected = 0usize;
        for draft in pending_parity_drafts() {
            let out = pending_guard_chain(draft, &wc);
            if out != draft {
                corrected += 1;
            }
            println!(
                "PARITY-CASE {}",
                serde_json::json!({ "draft": draft, "out": out })
            );
        }
        assert!(
            corrected >= 4,
            "the parity corpus must contain corrected cases, not only inert ones"
        );
    }

    // -----------------------------------------------------------------------
    // TIER-1 DURABLE MEMORY (task p1-engine-memory-reader) — the engine-side
    // reader for the gateway's forwarded `WG_MEMORY_CONTEXT` block.
    // -----------------------------------------------------------------------

    /// The gateway's real `buildMemoryContext` output shape
    /// (`claw3d-bridge/src/memoryInject.mjs`): banner + preamble + one line per
    /// scoped fact, the person's own facts tagged "(you)".
    fn sample_memory_context() -> String {
        "Family memory — remembered preferences & patterns, NOT the current schedule.\n\
         These are things the family has said or settled over time. They are soft priors, \
         not live state: if a live fact says otherwise, the LIVE fact is correct.\n\
         \n\
         - Nina is allergic to peanuts\n\
         - Prefers fish twice a week (you)\n\
         - Gym is usually Wednesday evening (you)"
            .to_string()
    }

    /// Nothing forwarded → no block at all, so the Telegram-listener path (env
    /// unset) and a deploy with nothing remembered keep the prompt unchanged.
    #[test]
    fn memory_context_absent_is_a_noop() {
        assert!(memory_context_block("").is_none());
        assert!(memory_context_block("   \n  \n").is_none());
    }

    /// The injected block carries the remembered facts AND the explicit
    /// live-wins labelling (docs/39 §5.3, the cardinal rule): soft priors, the
    /// live facts win, offer — never assert — a remembered pattern.
    #[test]
    fn memory_context_block_carries_facts_and_the_live_wins_label() {
        let block = memory_context_block(&sample_memory_context()).expect("block present");
        assert!(block.contains("Nina is allergic to peanuts"), "{block}");
        assert!(
            block.contains("Gym is usually Wednesday evening (you)"),
            "{block}"
        );
        let lower = block.to_lowercase();
        assert!(lower.contains("not the current schedule"), "{block}");
        assert!(lower.contains("live truth and it wins"), "{block}");
        assert!(lower.contains("soft priors"), "{block}");
        // A pattern is OFFERED, never asserted as this week's plan.
        assert!(lower.contains("offer the"), "{block}");
        assert!(
            lower.contains("never present a remembered pattern as this week's plan"),
            "{block}"
        );
        // No truncation note when the block is inside budget.
        assert!(!lower.contains("left out to keep this small"), "{block}");
    }

    /// BUDGET GUARD (docs/39 §6, no-silent-caps): an over-budget forwarded block
    /// — always a distiller/injector bug upstream — is truncated
    /// DETERMINISTICALLY at a line boundary, the earliest (highest keep-priority)
    /// lines survive, and the block SAYS it is partial rather than silently
    /// ballooning the compose prompt.
    #[test]
    fn memory_context_over_budget_is_trimmed_loudly_at_line_boundaries() {
        let mut huge = String::from("- Nina is allergic to peanuts\n");
        for i in 0..600 {
            huge.push_str(&format!(
                "- remembered filler fact number {i} about the week\n"
            ));
        }
        assert!(huge.len() > MEMORY_CONTEXT_MAX_CHARS);

        let block = memory_context_block(&huge).expect("block present");
        // The first (protected) line always survives a budget trim.
        assert!(block.contains("Nina is allergic to peanuts"), "{block}");
        // Loud, not silent.
        assert!(
            block.contains("more remembered line(s) left out to keep this small"),
            "{block}"
        );
        // Bounded, and cut only at line boundaries — no half-sentence facts.
        assert!(
            block.len() < MEMORY_CONTEXT_MAX_CHARS + 1200,
            "block len {}",
            block.len()
        );
        for line in block
            .lines()
            .filter(|l| l.starts_with("- remembered filler"))
        {
            assert!(
                line.ends_with("about the week"),
                "a fact was sliced mid-line: {line}"
            );
        }
        // Deterministic: the same input trims to the same block every time.
        assert_eq!(block, memory_context_block(&huge).expect("block present"));
    }

    // -----------------------------------------------------------------------
}

#[cfg(test)]
mod live_house_probe {
    /// Does the probe recognise THIS household's real configuration? Skips loudly rather than
    /// passing vacuously when the live tree is not present (CI, a fresh clone, another machine).
    #[test]
    fn the_live_house_is_recognised_as_having_an_external_feed() {
        let root = std::path::Path::new("/Users/lp698/Projects/weekly_planner");
        if !root.join(".casa/calendar.toml").exists() {
            eprintln!("SKIP: no live .casa/calendar.toml here — nothing to recognise");
            return;
        }
        assert!(
            super::external_calendar_configured(root),
            "the live house syncs a Google calendar but the probe did not see it — the hedge would \
             not engage and 'the calendar is clear' would ship again"
        );
    }

    /// The other half of the same live check: this house's calendar must be READABLE from here,
    /// not merely known to exist. Same skip rule, so it is inert anywhere but this box.
    ///
    /// Recognising the feed is what turned the lie into a hedge; reading it is what turns the
    /// hedge into an answer. On 2026-08-20 the family asked "tell me what's in for today" and got
    /// "Can't see your linked calendar from here" while `Luca pickup Oliver/Elliot` sat in that
    /// calendar for 5pm the same day.
    #[test]
    fn the_live_house_can_actually_read_its_calendar() {
        let root = std::path::Path::new("/Users/lp698/Projects/weekly_planner");
        if !root.join(".casa/calendar/synced-events.json").exists() {
            eprintln!("SKIP: no live calendar snapshot here — the gateway writes it on sync");
            return;
        }
        let now = chrono::Local::now().naive_local();
        let view = super::calendar_snapshot_view(root, now);
        assert!(
            matches!(view, super::CalendarView::Visible { .. }),
            "the live snapshot exists but is not trusted ({view:?}) — the family would still be \
             told the calendar cannot be seen from here"
        );
        // And the always-on context line must NOT carry the sentence the family read.
        let line = super::fetch_schedule_context_line(root, now);
        assert!(
            !line.contains("NOT visible to you here"),
            "the compose prompt still tells the model it cannot see the calendar: {line}"
        );
    }
}
