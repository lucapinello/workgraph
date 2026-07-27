//! Reading a reminder back — grounded, requester-scoped, and never invented
//! (task `reminder-readback-lane`).
//!
//! THE GAP THIS CLOSES (live-cert C052, bridge seq 34). A reminder could be
//! *filed* through three different paths and then could not be *read back* by
//! any of them:
//!
//! * [`super::grounding`]'s `PLAN_TOPICS` did not list the reminder noun at all,
//!   so "what date is the reminder to call the dentist set for?" was not even
//!   classified as a read — no grounding block was injected;
//! * the grounded block reads the weekly plan model and nothing else — never the
//!   ad-hoc reminders the DM path writes to `.casa/reminders-adhoc.json`;
//! * the fast lane treats every reminder READ as a fallback (correctly — it must
//!   not file one), which hands the turn to the composer;
//! * only the operator's `wg telegram remind --list` seam merged the two sources.
//!
//! Net effect: the answer was composed with no reminder data in front of it, so
//! it echoed whatever date the *question* carried. Ask "is the dentist reminder
//! on Aug 3?" about a reminder that is really on Jul 27 and you were agreed with.
//!
//! This module is the deterministic fix. It is pure over an injected `now` and an
//! already-loaded reminder set; [`answer_for`] is the only impure function and it
//! merely merges the same two sources `--list` merges. It never writes.
//!
//! ## Privacy is a load-bearing rule, not a nicety
//!
//! A reminder carries a `recipient` — the one family member it is FOR. This lane
//! answers only from the reminders [`visible_to`] the person asking: their own,
//! plus rows addressed to nobody (a shared plan row the whole family can already
//! read in the weekly plan). One member asking about another's reminder gets the
//! same honest "nothing set about that" a stranger would — the date is never
//! disclosed, and no hint that a reminder exists leaks out.
//!
//! ## What counts as "a reminder" here
//!
//! The two sources the reminder engine itself schedules and fires: the weekly
//! plan's `⏰ Reminder:` calendar rows and the ad-hoc list. The `/reminders`
//! command additionally surfaces work-graph rows (tasks awaiting a human reply,
//! upcoming crons); those are deliberately NOT merged in here. They carry raw
//! task titles, and a family member asking "what reminders do I have?" would get
//! engineering copy back — the same leak the activity feed had to be taught to
//! drop. `/reminders` remains the surface for those; this lane answers about the
//! reminders the family actually asked to be reminded of.
//!
//! ## Shape of the answer
//!
//! Exactly one line, in family voice, naming what the reminder is about and the
//! exact family-local date and time it is set for, taken from what is on disk:
//!
//! * one match  → "Your reminder — Call the dentist — is set for Monday, Jul 27 at 9:00 am."
//! * no match   → "You don't have a reminder set about that."
//! * several    → the candidates, briefly, and which one did you mean?
//!
//! The body is quoted exactly as it was filed (the reminder engine capitalises
//! it), so the sentence is built around it with dashes rather than folding it
//! into the grammar — a readback that silently re-cased what the family said
//! would be a small lie in a lane whose whole job is not telling them any.

use chrono::{Datelike, NaiveDateTime, Weekday};

use super::reminder::Reminder;

/// How many candidates an ambiguous answer names before it summarises the rest.
const MAX_LISTED: usize = 4;

// ---------------------------------------------------------------------------
// Classification: is this turn asking to READ a reminder back?
// ---------------------------------------------------------------------------

/// A parsed request to read reminders back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadbackQuery {
    /// Significant words naming what the reminder is about ("call dentist");
    /// empty for a broad ask ("do I have any reminders?").
    pub target: Vec<String>,
    /// The weekday named in the question, when one was.
    pub day: Option<Weekday>,
}

impl ReadbackQuery {
    /// True when the question named no subject — "what reminders do I have?".
    pub fn is_broad(&self) -> bool {
        self.target.is_empty()
    }
}

/// Openers that make a turn unambiguously a QUESTION about reminders rather than
/// an instruction to file or drop one. Matched against the start of the
/// normalised message, so an imperative ("set a reminder …", "cancel the
/// reminder …") can never enter this lane through them.
const READ_OPENERS: &[&str] = &[
    "what",
    "whats",
    "what's",
    "when",
    "whens",
    "when's",
    "which",
    "where",
    "is there",
    "is the",
    "is my",
    "are there",
    "are my",
    "do i",
    "do we",
    "does",
    "did",
    "have i",
    "have we",
    "any",
    "how many",
    "list",
    "show",
    "tell me",
    "remind me what",
    "remind me when",
    "can you tell me",
    "could you tell me",
];

/// Verbs that mean the turn CHANGES a reminder. A bare question mark is not
/// enough to enter this lane when one of these is present — "can you set a
/// reminder for the dentist on Friday?" is a write wearing a question mark.
const WRITE_VERBS: &[&str] = &[
    "set a remind",
    "set the remind",
    "set me a remind",
    "add a remind",
    "add the remind",
    "create a remind",
    "make a remind",
    "put a remind",
    "schedule a remind",
    "remind me to",
    "cancel",
    "delete",
    "remove",
    "drop the remind",
    "clear",
    "scrap",
    "stop reminding",
    "unset",
    "move the remind",
    "change the remind",
    "push the remind",
];

/// Noise words stripped from the question to leave the subject: everything that
/// is grammar, reminder vocabulary, or a question word.
const NOISE: &[&str] = &[
    "a", "about", "again", "all", "am", "an", "and", "any", "anything", "are", "at", "be", "been",
    "by", "can", "could", "date", "day", "did", "do", "does", "exact", "exactly", "for", "from",
    "get", "got", "have", "hey", "hi", "how", "i", "is", "it", "just", "know", "list", "many",
    "me", "mine", "moment", "my", "of", "off", "ok", "okay", "on", "one", "or", "our", "please",
    "remind", "reminded", "reminder", "reminders", "reminding", "scheduled", "set", "show",
    "still", "tell", "that", "the", "there", "these", "they", "this", "those", "time", "to",
    "told", "up", "us", "was", "we", "were", "what", "whats", "when", "whens", "where", "which",
    "who", "will", "with", "you", "your", "yours",
];

/// Weekday words → [`Weekday`], for a question that names a day.
const WEEKDAYS: &[(&[&str], Weekday)] = &[
    (&["monday", "mon"], Weekday::Mon),
    (&["tuesday", "tues", "tue"], Weekday::Tue),
    (&["wednesday", "weds", "wed"], Weekday::Wed),
    (&["thursday", "thurs", "thur", "thu"], Weekday::Thu),
    (&["friday", "fri"], Weekday::Fri),
    (&["saturday", "sat"], Weekday::Sat),
    (&["sunday", "sun"], Weekday::Sun),
];

/// Lowercase, collapse whitespace, and drop punctuation that would break word
/// matching (apostrophes are kept — "what's" is a distinct opener).
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '\'' || ch.is_whitespace() {
            out.extend(ch.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `hay` contains `word` as a whole word.
fn contains_word(hay: &str, word: &str) -> bool {
    hay.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .any(|w| w == word)
}

/// Detect a request to READ reminders back, or `None` for every other turn —
/// including every turn that would FILE or DROP one.
///
/// The rule, deliberately narrow so it can never steal a write:
///
/// 1. the message must name the reminder NOUN ("reminder"/"reminders"); the bare
///    verb is not enough, so "remind me what was in Monday's risotto" (a family
///    memory ask) stays with the composer where it belongs;
/// 2. it must either open with a [`READ_OPENERS`] question opener, or be a
///    question mark question that carries no [`WRITE_VERBS`] verb.
pub fn parse_readback(text: &str) -> Option<ReadbackQuery> {
    let norm = normalize(text);
    if norm.is_empty() {
        return None;
    }
    if !contains_word(&norm, "reminder") && !contains_word(&norm, "reminders") {
        return None;
    }
    let opens_read = READ_OPENERS
        .iter()
        .any(|o| norm == *o || norm.starts_with(&format!("{o} ")));
    let has_write_verb = WRITE_VERBS.iter().any(|v| norm.contains(v));
    let question = text.trim_end().ends_with('?');
    if !opens_read && !(question && !has_write_verb) {
        return None;
    }

    let day = WEEKDAYS
        .iter()
        .find(|(names, _)| names.iter().any(|n| contains_word(&norm, n)))
        .map(|(_, wd)| *wd);

    let target: Vec<String> = norm
        .split_whitespace()
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| w.len() > 2)
        .filter(|w| !NOISE.contains(&w.as_str()))
        .filter(|w| {
            !WEEKDAYS
                .iter()
                .any(|(names, _)| names.contains(&w.as_str()))
        })
        .collect();

    Some(ReadbackQuery { target, day })
}

// ---------------------------------------------------------------------------
// Privacy: only what the person asking is allowed to see
// ---------------------------------------------------------------------------

/// The reminders `requester` may be told about: the ones addressed TO them, plus
/// the ones addressed to nobody (a shared plan row already visible to the whole
/// family in the weekly plan).
///
/// A reminder for another member is filtered out here, before any matching runs,
/// so no later stage can leak it — not the "which one did you mean?" list, not a
/// count, not the honest empty line.
pub fn visible_to<'a>(reminders: &'a [Reminder], requester: &str) -> Vec<&'a Reminder> {
    let who = requester.trim();
    reminders
        .iter()
        .filter(|r| {
            let owner = r.recipient.trim();
            owner.is_empty() || owner.eq_ignore_ascii_case(who)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Matching + rendering
// ---------------------------------------------------------------------------

/// The reminders a query matches, most-imminent first, from an already
/// privacy-filtered set.
///
/// Strict first — every subject word must appear in the reminder body — and only
/// when that finds nothing does it fall back to a loose any-word match, so a
/// question carrying an extra descriptive word still finds its reminder without
/// making two unrelated reminders look alike.
fn matches<'a>(visible: &[&'a Reminder], query: &ReadbackQuery) -> Vec<&'a Reminder> {
    let day_ok = |r: &Reminder| match query.day {
        Some(wd) => r.due.date().weekday() == wd,
        None => true,
    };
    let mut pool: Vec<&Reminder> = visible.iter().copied().filter(|r| day_ok(r)).collect();
    pool.sort_by_key(|r| r.due);

    if query.is_broad() {
        return pool;
    }
    let body_of = |r: &Reminder| normalize(&r.text);
    let strict: Vec<&Reminder> = pool
        .iter()
        .copied()
        .filter(|r| {
            let body = body_of(r);
            query.target.iter().all(|w| body.contains(w.as_str()))
        })
        .collect();
    if !strict.is_empty() {
        return strict;
    }
    pool.into_iter()
        .filter(|r| {
            let body = body_of(r);
            query.target.iter().any(|w| body.contains(w.as_str()))
        })
        .collect()
}

/// "Monday, Jul 27 at 9:00 am" — the exact family-local date and time, with the
/// year added only when it is not the year we are standing in (so the everyday
/// answer stays short and the far-off one stays unambiguous).
pub fn when_phrase(due: NaiveDateTime, now: NaiveDateTime) -> String {
    let day = if due.year() == now.year() {
        due.format("%A, %b %-d").to_string()
    } else {
        due.format("%A, %b %-d %Y").to_string()
    };
    let clock = due.format("%-I:%M %p").to_string().to_lowercase();
    format!("{day} at {clock}")
}

/// "call the dentist" — the reminder body as a subject phrase, with a leading
/// "to " stripped so it can follow "your reminder to …" naturally.
fn subject_of(r: &Reminder) -> String {
    let t = r.text.trim();
    t.strip_prefix("to ")
        .or_else(|| t.strip_prefix("To "))
        .unwrap_or(t)
        .trim()
        .to_string()
}

/// One candidate line: "call the dentist — Monday, Jul 27 at 9:00 am".
fn candidate_line(r: &Reminder, now: NaiveDateTime) -> String {
    format!("{} — {}", subject_of(r), when_phrase(r.due, now))
}

/// The deterministic, family-voice answer for one readback question, from the
/// reminders that are actually on file for the person asking.
///
/// Pure: every date in the returned line comes from `reminders`, never from the
/// question. That is the whole point — a question that asserts the wrong date
/// ("is the dentist one on Aug 3?") is answered with the date on disk.
pub fn answer(
    reminders: &[Reminder],
    query: &ReadbackQuery,
    requester: &str,
    now: NaiveDateTime,
) -> String {
    let visible = visible_to(reminders, requester);
    let found = matches(&visible, query);
    let (pending, past): (Vec<&Reminder>, Vec<&Reminder>) =
        found.into_iter().partition(|r| r.due > now);

    if pending.is_empty() {
        // Nothing upcoming. If the thing they asked about already went out, say
        // so honestly rather than claiming it was never set.
        if let Some(gone) = past.last() {
            if !query.is_broad() {
                return format!(
                    "Your reminder — {} — already went out; it was set for {}.",
                    subject_of(gone),
                    when_phrase(gone.due, now)
                );
            }
        }
        return if query.is_broad() {
            "You don't have any reminders set right now.".to_string()
        } else {
            "You don't have a reminder set about that.".to_string()
        };
    }

    if pending.len() == 1 {
        let r = pending[0];
        return format!(
            "Your reminder — {} — is set for {}.",
            subject_of(r),
            when_phrase(r.due, now)
        );
    }

    let listed: Vec<String> = pending
        .iter()
        .take(MAX_LISTED)
        .map(|r| candidate_line(r, now))
        .collect();
    let more = pending.len().saturating_sub(listed.len());
    let tail = if more > 0 {
        format!(" (and {more} more)")
    } else {
        String::new()
    };
    if query.is_broad() {
        format!(
            "You've got {} reminders coming up: {}{}.",
            pending.len(),
            listed.join("; "),
            tail
        )
    } else {
        format!(
            "You've got {} reminders that could be it: {}{}. Which one did you mean?",
            pending.len(),
            listed.join("; "),
            tail
        )
    }
}

/// The same persisted truth as a grounding block for a composer that is NOT
/// short-circuited by this lane — already scoped to the person asking, so the
/// prompt can never carry another member's reminder.
///
/// `None` when the person has nothing upcoming to ground on.
pub fn grounding_block(reminders: &[Reminder], requester: &str, now: NaiveDateTime) -> Option<String> {
    let visible = visible_to(reminders, requester);
    let mut pending: Vec<&Reminder> = visible.into_iter().filter(|r| r.due > now).collect();
    if pending.is_empty() {
        return None;
    }
    pending.sort_by_key(|r| r.due);
    let mut out = String::from(
        "REMINDERS ON FILE — these are the reminders actually set for the person asking, \
         with their exact dates and times. Answer any reminder question FROM this list and \
         nothing else: if the question names a different date, the list is right and the \
         question is wrong. Never invent, shift, or agree with a date that is not here.\n",
    );
    for r in &pending {
        out.push_str(&format!("- {}\n", candidate_line(r, now)));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The one impure entry point: merge what is on disk, answer, write nothing
// ---------------------------------------------------------------------------

/// Every reminder the engine knows about under `root`, as of `now`: the current
/// weekly plan's reminder rows plus the persisted ad-hoc list — exactly the two
/// sources `wg telegram remind --list` merges, so the lane and the operator view
/// can never disagree. Best-effort; a missing plan or store simply contributes
/// nothing. Reads only.
pub fn load_all(
    root: &std::path::Path,
    members: &[String],
    owners: &super::ownership::OwnerMap,
    now: NaiveDateTime,
) -> Vec<Reminder> {
    use super::family_plan;
    use super::reminder::{self, AdHocStore};

    let plans = family_plan::load_plans(root);
    let mut reminders: Vec<Reminder> = family_plan::current_plan(&plans, now.date())
        .map(|p| reminder::reminders_from_plan(p, members, owners))
        .unwrap_or_default();
    reminders.extend(AdHocStore::load(&AdHocStore::path(root)).reminders);
    reminders.sort_by_key(|r| r.due);
    reminders
}

/// Answer a reminder-read turn from what is on disk under `root`, or `None` when
/// the turn is not a reminder read at all (the caller then proceeds exactly as
/// before). Writes nothing, sends nothing.
pub fn answer_for(
    root: &std::path::Path,
    message: &str,
    requester: &str,
    members: &[String],
    owners: &super::ownership::OwnerMap,
    now: NaiveDateTime,
) -> Option<String> {
    let query = parse_readback(message)?;
    let reminders = load_all(root, members, owners, now);
    Some(answer(&reminders, &query, requester, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::reminder::ReminderSource;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn rem(text: &str, recipient: &str, due: NaiveDateTime) -> Reminder {
        Reminder {
            id: format!("adhoc:{}:{}", due.format("%Y%m%dT%H%M"), text.len()),
            due,
            recipient: recipient.to_string(),
            bot: "otto".to_string(),
            text: text.to_string(),
            source: ReminderSource::AdHoc,
        }
    }

    // -- classification ----------------------------------------------------

    #[test]
    fn reminder_questions_classify_as_reads() {
        for q in [
            "What exact date and time is the reminder to call the dentist set for?",
            "When is my dentist reminder?",
            "What time is the reminder about the bins?",
            "Do I have a reminder to call the dentist?",
            "Do I have any reminders?",
            "What reminders do I have this week?",
            "Is there a reminder for the dentist?",
            "List my reminders",
            "Did you cancel my dentist reminder?",
            "Tell me when the dentist reminder is",
        ] {
            assert!(
                parse_readback(q).is_some(),
                "should read back: {q:?}"
            );
        }
    }

    #[test]
    fn writes_and_non_reminder_asks_never_enter_the_lane() {
        for q in [
            // creations
            "Remind me to call the dentist on Monday at 9:00 a.m.",
            "Set a reminder for the dentist Friday at 9",
            "Can you set a reminder for the dentist on Friday?",
            "Add a reminder to take the bins out tonight",
            // cancellations
            "Cancel the reminder about the dentist",
            "Delete my Monday reminder",
            "Stop reminding me about the bins",
            "Can you cancel the dentist reminder?",
            // not about reminders at all
            "Remind me what was in Monday's risotto",
            "What's for dinner on Friday?",
            "",
        ] {
            assert!(
                parse_readback(q).is_none(),
                "must NOT read back: {q:?}"
            );
        }
    }

    #[test]
    fn the_subject_survives_and_the_grammar_does_not() {
        let q = parse_readback("What exact date and time is the reminder to call the dentist set for?")
            .expect("a read");
        assert_eq!(q.target, vec!["call".to_string(), "dentist".to_string()]);
        assert!(q.day.is_none());

        let broad = parse_readback("Do I have any reminders?").expect("a read");
        assert!(broad.is_broad(), "no subject named: {:?}", broad.target);

        let dayed = parse_readback("What's my reminder on Monday?").expect("a read");
        assert_eq!(dayed.day, Some(Weekday::Mon));
    }

    // -- the poisoned prompt (the C052 failure) ----------------------------

    #[test]
    fn a_question_asserting_the_wrong_date_is_answered_from_disk() {
        // The store deliberately disagrees with the question: the question says
        // Aug 3, the persisted reminder is Jul 27. The answer must say Jul 27.
        let store = vec![rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0))];
        let now = at(2026, 7, 27, 3, 20);
        let q = parse_readback(
            "What exact date and time is the reminder to call the dentist set for — Monday, August 3, 2026 at 9:00 a.m.?",
        )
        .expect("a read");
        let line = answer(&store, &q, "Luca", now);
        assert!(
            line.contains("Jul 27"),
            "must answer with the persisted date, got {line:?}"
        );
        assert!(
            !line.contains("Aug 3") && !line.contains("August 3"),
            "must NOT echo the date the question asserted, got {line:?}"
        );
        assert!(line.contains("9:00 am"), "exact time, got {line:?}");
        assert!(
            line.contains("call the dentist"),
            "names what the reminder is about, got {line:?}"
        );
    }

    // -- zero / one / ambiguous -------------------------------------------

    #[test]
    fn one_match_answers_with_the_verb_object_and_exact_time() {
        let store = vec![rem("defrost the trout", "Luca", at(2026, 7, 30, 17, 30))];
        let q = parse_readback("When is my trout reminder?").expect("a read");
        let line = answer(&store, &q, "Luca", at(2026, 7, 27, 3, 20));
        assert_eq!(
            line,
            "Your reminder — defrost the trout — is set for Thursday, Jul 30 at 5:30 pm."
        );
    }

    #[test]
    fn zero_matches_is_answered_honestly_and_invents_nothing() {
        let store = vec![rem("defrost the trout", "Luca", at(2026, 7, 30, 17, 30))];
        let q = parse_readback("What time is the reminder about the dentist?").expect("a read");
        let line = answer(&store, &q, "Luca", at(2026, 7, 27, 3, 20));
        assert_eq!(line, "You don't have a reminder set about that.");

        let none = parse_readback("Do I have any reminders?").expect("a read");
        assert_eq!(
            answer(&[], &none, "Luca", at(2026, 7, 27, 3, 20)),
            "You don't have any reminders set right now."
        );
    }

    #[test]
    fn ambiguous_matches_list_the_candidates_briefly() {
        let store = vec![
            rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0)),
            rem("call the vet", "Luca", at(2026, 7, 28, 10, 0)),
        ];
        let q = parse_readback("When is my call reminder?").expect("a read");
        let line = answer(&store, &q, "Luca", at(2026, 7, 27, 3, 20));
        assert!(line.starts_with("You've got 2 reminders that could be it:"), "{line}");
        assert!(line.contains("call the dentist — Monday, Jul 27 at 9:00 am"), "{line}");
        assert!(line.contains("call the vet — Tuesday, Jul 28 at 10:00 am"), "{line}");
        assert!(line.ends_with("Which one did you mean?"), "{line}");
    }

    #[test]
    fn a_broad_ask_lists_what_is_coming_up() {
        let store = vec![
            rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0)),
            rem("take the bins out", "Luca", at(2026, 7, 28, 7, 0)),
        ];
        let q = parse_readback("What reminders do I have?").expect("a read");
        let line = answer(&store, &q, "Luca", at(2026, 7, 27, 3, 20));
        assert!(line.starts_with("You've got 2 reminders coming up:"), "{line}");
        assert!(!line.contains("Which one"), "a broad list asks nothing back: {line}");
    }

    #[test]
    fn an_elapsed_reminder_is_reported_as_gone_not_as_never_set() {
        let store = vec![rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0))];
        let q = parse_readback("When was my dentist reminder?").expect("a read");
        let line = answer(&store, &q, "Luca", at(2026, 7, 27, 18, 0));
        assert!(line.contains("already went out"), "{line}");
        assert!(line.contains("Jul 27 at 9:00 am"), "{line}");
    }

    // -- privacy -----------------------------------------------------------

    #[test]
    fn one_members_reminder_is_never_disclosed_to_another() {
        let store = vec![rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0))];
        let now = at(2026, 7, 27, 3, 20);
        let q = parse_readback("What date and time is the reminder to call the dentist set for?")
            .expect("a read");

        // The owner is told.
        let owner_line = answer(&store, &q, "Luca", now);
        assert!(owner_line.contains("Jul 27"), "{owner_line}");

        // Another member is told nothing — not the date, not the time, and not
        // that such a reminder exists at all.
        let other_line = answer(&store, &q, "Nadin", now);
        assert_eq!(other_line, "You don't have a reminder set about that.");
        for leak in ["Jul 27", "9:00", "Luca"] {
            assert!(
                !other_line.contains(leak),
                "leaked {leak:?} across members: {other_line}"
            );
        }

        // A broad list is scoped the same way.
        let broad = parse_readback("Do I have any reminders?").expect("a read");
        assert_eq!(
            answer(&store, &broad, "Nadin", now),
            "You don't have any reminders set right now."
        );
        assert!(visible_to(&store, "Nadin").is_empty());
    }

    #[test]
    fn an_unaddressed_reminder_is_shared_family_context() {
        // A plan row that named no member belongs to the whole family — it is
        // already legible to everyone in the weekly plan.
        let store = vec![rem("bin day", "", at(2026, 7, 28, 7, 0))];
        let q = parse_readback("Is there a reminder about the bin?").expect("a read");
        let line = answer(&store, &q, "Nadin", at(2026, 7, 27, 3, 20));
        assert!(line.contains("Jul 28"), "{line}");
    }

    #[test]
    fn the_grounding_block_carries_only_the_askers_reminders() {
        let store = vec![
            rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0)),
            rem("physio", "Nadin", at(2026, 7, 29, 8, 0)),
        ];
        let now = at(2026, 7, 27, 3, 20);
        let block = grounding_block(&store, "Luca", now).expect("a block");
        assert!(block.contains("call the dentist — Monday, Jul 27 at 9:00 am"), "{block}");
        assert!(!block.contains("physio"), "another member leaked: {block}");
        assert!(grounding_block(&[], "Luca", now).is_none());
    }

    // -- zero writes -------------------------------------------------------

    #[test]
    fn a_readback_touches_nothing_on_disk() {
        use crate::notify::reminder::AdHocStore;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut store = AdHocStore::default();
        store.add(rem("call the dentist", "Luca", at(2026, 7, 27, 9, 0)));
        let path = AdHocStore::path(root);
        store.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        let owners = crate::notify::ownership::OwnerMap::casa_default();
        let line = answer_for(
            root,
            "What date is the reminder to call the dentist set for?",
            "Luca",
            &["Luca".to_string()],
            &owners,
            at(2026, 7, 27, 3, 20),
        )
        .expect("the lane owns this turn");
        assert!(line.contains("Jul 27 at 9:00 am"), "{line}");

        assert_eq!(before, std::fs::read(&path).unwrap(), "the file changed");
        assert_eq!(mtime, std::fs::metadata(&path).unwrap().modified().unwrap());
    }

    #[test]
    fn a_non_read_turn_leaves_the_caller_alone() {
        let dir = tempfile::tempdir().unwrap();
        let owners = crate::notify::ownership::OwnerMap::casa_default();
        assert!(
            answer_for(
                dir.path(),
                "Remind me to call the dentist on Monday at 9:00 a.m.",
                "Luca",
                &["Luca".to_string()],
                &owners,
                at(2026, 7, 27, 3, 20),
            )
            .is_none()
        );
    }
}
