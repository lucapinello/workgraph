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

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};
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
    let today = now.date();
    let label = format!(
        "{} {}",
        family_plan::long_weekday(today),
        today.format("%b %-d")
    );
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
// the scoped family-reply sink in the ENGINE process, so the guard MUST live here.
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
        assert!(lower.contains("from this table"), "{block}");
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
