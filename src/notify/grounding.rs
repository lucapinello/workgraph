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

use std::path::Path;

use chrono::NaiveDate;

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
}
