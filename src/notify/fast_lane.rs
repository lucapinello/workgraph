//! Fast lane for simple conversational plan edits.
//!
//! Every conversational ask today spawns a full agent — a worktree, a plan-file
//! edit, tests, an eval. That is the right machine for "rebalance the week around
//! Nadin's travel"; it is absurd for "swap Friday to tacos", which should take a
//! minute, not twenty.
//!
//! This module is that fast lane. It classifies a chat turn against a **closed
//! set** of structured plan edits — a single meal swap / add / remove, a shopping
//! add, a reminder — and, for a clean hit, applies the change **directly** to the
//! week's plan markdown (the same file the kiosk/`/week` surfaces read) within the
//! turn itself. Every edit is a pure string transform that is then **round-tripped
//! through the real plan parser** ([`super::family_plan::PlanDoc::parse`]) before
//! it is committed: if the edited document does not parse back to the change we
//! intended, we refuse and fall back rather than write a corrupt plan.
//!
//! Anything outside the closed set — or a compound ask that pairs a simple edit
//! with open-ended work ("swap Friday to tacos *and rebalance the week*") — is
//! [`Classification::Fallback`], and the caller runs the full task pipeline exactly
//! as today. The report-back for a fast-lane edit is immediate and in plain family
//! voice: `Done — tacos Friday 🌮`.
//!
//! Everything here is pure (parse/edit-from-string) except the thin
//! [`run_fast_lane`] orchestrator, which reads the current plan file, applies, and
//! writes it back atomically. The classifier and the editors take their input as
//! strings so the tests never need a live filesystem, gateway, or bot.

use std::path::{Path, PathBuf};

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use super::family_plan::{self, PlanDoc};

// ---------------------------------------------------------------------------
// The closed set of fast-lane operations
// ---------------------------------------------------------------------------

/// A structured, directly-appliable plan edit — the whole closed set the fast
/// lane understands. Anything a chat turn cannot be classified into one of these
/// falls back to the full task pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastLaneOp {
    /// Replace a day's dinner: "swap Friday to tacos".
    MealSwap { day: Weekday, dish: String },
    /// Add a component to a day's dinner: "add a dessert on Tuesday".
    MealAdd { day: Weekday, addition: String },
    /// Remove a named component from a day's dinner: "drop Monday's side salad".
    MealRemove { day: Weekday, target: String },
    /// Add an item to the shopping list: "add milk to the shopping list".
    ShoppingAdd { item: String },
    /// Set a reminder: "remind me to defrost the chicken Friday at 5pm".
    ReminderSet {
        text: String,
        day: Option<Weekday>,
        time: Option<String>,
    },
}

impl FastLaneOp {
    /// A stable, PII-free label for logs and the graph node title.
    pub fn kind_label(&self) -> &'static str {
        match self {
            FastLaneOp::MealSwap { .. } => "meal-swap",
            FastLaneOp::MealAdd { .. } => "meal-add",
            FastLaneOp::MealRemove { .. } => "meal-remove",
            FastLaneOp::ShoppingAdd { .. } => "shopping-add",
            FastLaneOp::ReminderSet { .. } => "reminder-set",
        }
    }
}

/// Why a message was not fast-laned — kept distinct so the caller can log the
/// reason and so tests can assert *why* something fell through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// Not one of the closed-set operations at all (open-ended ask, question,
    /// small talk, a plan change too broad to structure).
    NotASimpleEdit,
    /// A simple edit tangled with a second, open-ended instruction — e.g.
    /// "swap Friday to tacos and rebalance the week". The whole thing goes to the
    /// full pipeline so the compound intent is honoured, not half-applied.
    Compound,
}

/// The outcome of classifying a chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    FastLane(FastLaneOp),
    Fallback(FallbackReason),
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Verbs that introduce an *edit* clause — used both to match an operation and,
/// crucially, to detect a compound ask (two edit clauses joined by "and"/"then").
const EDIT_VERBS: &[&str] = &[
    "swap", "change", "switch", "replace", "make", "add", "remove", "drop",
    "delete", "cancel", "skip", "remind", "reminder", "put", "buy", "ditch",
];

/// Phrases that mark an ask as too broad for the fast lane even when it opens
/// with a clean simple edit. "swap Friday to tacos **and rebalance the week**".
const COMPLEX_MARKERS: &[&str] = &[
    "rebalance", "re-balance", "rebalanc", "redo the", "re-do the", "replan",
    "re-plan", "rework", "reorganiz", "reorganis", "rethink", "overhaul",
    "shuffle the", "sort out the week", "plan the whole", "plan the week",
    "review the week", "optimi", "rest of the week", "whole week",
    "everything else", "the entire week", "around the", "work around",
];

/// Classify a chat turn against the closed set. Pure; `today` anchors relative
/// day words ("today", "tomorrow", "tonight").
pub fn classify(message: &str, today: NaiveDate) -> Classification {
    let text = message.trim();
    if text.is_empty() {
        return Classification::Fallback(FallbackReason::NotASimpleEdit);
    }
    let s = normalize(text);

    // A broad, open-ended second clause takes the whole ask to the full pipeline,
    // even though it may open with a clean simple edit.
    if is_compound(&s) {
        return Classification::Fallback(FallbackReason::Compound);
    }

    // A wh-question ("what's for dinner on Friday?") is a query, not an edit —
    // even though it names a day. Polite imperatives ("can you swap …") do not
    // open with these words, so they are unaffected.
    if is_query(&s) {
        return Classification::Fallback(FallbackReason::NotASimpleEdit);
    }

    match match_single_op(&s, today) {
        Some(op) => Classification::FastLane(op),
        None => Classification::Fallback(FallbackReason::NotASimpleEdit),
    }
}

/// True for an interrogative that should be answered, not applied.
fn is_query(s: &str) -> bool {
    const OPENERS: &[&str] = &[
        "what", "whats", "when", "where", "who", "why", "which", "whose",
        "how ", "is ", "are ", "was ", "were ", "do we", "does ", "did ",
        "should we", "should i", "any ideas", "can we still",
    ];
    OPENERS.iter().any(|o| s.starts_with(o))
}

/// Lowercase, collapse whitespace, and drop a trailing courtesy so extraction
/// sees a tidy string. Dish/item casing is intentionally not preserved — a plan
/// line and the "Done — tacos Friday" report both read fine in lower case.
fn normalize(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when the ask pairs a simple edit with a second open-ended instruction,
/// or names an explicitly week-scale operation.
fn is_compound(s: &str) -> bool {
    if COMPLEX_MARKERS.iter().any(|m| s.contains(m)) {
        return true;
    }
    // Two independent edit clauses joined by a connector → mixed ask.
    let connectors = [" and ", " then ", " & ", "; ", " also ", " plus "];
    let mut segments: Vec<&str> = vec![s];
    for c in connectors {
        segments = segments
            .into_iter()
            .flat_map(|seg| seg.split(c))
            .collect();
    }
    if segments.len() < 2 {
        return false;
    }
    let edit_segments = segments.iter().filter(|seg| segment_is_edit(seg)).count();
    edit_segments >= 2
}

/// A segment "is an edit" if it carries an edit verb, or names a weekday paired
/// with a swap preposition ("saturday to pizza") — the second half of a
/// two-swap compound with only one leading verb.
fn segment_is_edit(seg: &str) -> bool {
    let seg = seg.trim();
    if EDIT_VERBS.iter().any(|v| contains_word(seg, v)) {
        return true;
    }
    if find_weekday(seg).is_some() && (seg.contains(" to ") || seg.contains(" into ")) {
        return true;
    }
    false
}

/// Match exactly one operation, in priority order. Reminder and shopping are
/// checked before the meal ops because they share verbs ("add") but are pinned
/// by their own keywords ("remind", "list"/"shopping").
fn match_single_op(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    if let Some(op) = match_reminder(s, today) {
        return Some(op);
    }
    if let Some(op) = match_shopping(s) {
        return Some(op);
    }
    if let Some(op) = match_meal_remove(s, today) {
        return Some(op);
    }
    if let Some(op) = match_meal_add(s, today) {
        return Some(op);
    }
    if let Some(op) = match_meal_swap(s, today) {
        return Some(op);
    }
    None
}

fn match_reminder(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    if !contains_word(s, "remind") && !s.starts_with("reminder") && !s.contains("reminder") {
        return None;
    }
    let markers = [
        "remind me to ", "remind me ", "remind us to ", "remind us ",
        "remind everyone to ", "set a reminder to ", "set a reminder ",
        "reminder to ", "reminder: ", "reminder ",
    ];
    let body = markers
        .iter()
        .find_map(|m| s.split_once(m).map(|(_, rest)| rest))
        .unwrap_or(s);

    let (day, body) = pull_day(body, today);
    let (time, body) = pull_time(&body);
    let text = scrub_fillers(&body);
    if text.is_empty() {
        return None;
    }
    Some(FastLaneOp::ReminderSet { text, day, time })
}

fn match_shopping(s: &str) -> Option<FastLaneOp> {
    let list_scoped = s.contains("shopping list")
        || s.contains("grocery list")
        || s.contains("groceries")
        || s.contains("shopping")
        || s.contains(" list");
    if !list_scoped {
        return None;
    }
    let verbs = ["add ", "put ", "buy ", "need ", "get ", "grab ", "pick up "];
    let (_, tail) = verbs.iter().find_map(|v| s.split_once(v).map(|p| (v, p.1)))?;

    // Cut the trailing "… to/on the (shopping) list" phrase off the item.
    let cuts = [
        " to the shopping", " to the grocery", " on the shopping", " on the grocery",
        " to the list", " on the list", " to my list", " on my list",
        " to shopping", " to groceries", " to the fridge list", " onto the",
        " to the", " on the", " to my", " on my",
    ];
    let mut item = tail.to_string();
    for c in cuts {
        if let Some(idx) = item.find(c) {
            item.truncate(idx);
            break;
        }
    }
    let item = scrub_fillers(&item);
    // Guard against "add it to the shopping list" with no real noun.
    if item.is_empty() || item == "list" || item == "it" || item == "them" {
        return None;
    }
    Some(FastLaneOp::ShoppingAdd { item })
}

fn match_meal_remove(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    let verbs = ["remove ", "drop ", "cancel ", "skip ", "take off ", "get rid of ", "delete ", "ditch "];
    let (_, tail) = verbs.iter().find_map(|v| s.split_once(v).map(|p| (v, p.1)))?;
    let day = find_weekday(s).map(|(w, _)| w).or_else(|| relative_day(s, today))?;
    let (_, tail) = pull_day(tail, today);
    let target = scrub_fillers(&tail);
    if target.is_empty() {
        return None;
    }
    Some(FastLaneOp::MealRemove { day, target })
}

fn match_meal_add(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    // Shopping already consumed list-scoped "add"s; here "add" means a dish/side.
    let (_, tail) = s.split_once("add ")?;
    let day = find_weekday(s).map(|(w, _)| w).or_else(|| relative_day(s, today))?;
    let (_, tail) = pull_day(tail, today);
    let addition = scrub_fillers(&tail);
    if addition.is_empty() {
        return None;
    }
    // Same ask → dish gate as the swap path: the addition must read like a dish
    // component, not the leftover of a raw request.
    let addition = ask_to_dish(&addition)?;
    Some(FastLaneOp::MealAdd { day, addition })
}

fn match_meal_swap(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    let day = find_weekday(s).map(|(w, _)| w).or_else(|| relative_day(s, today))?;
    let swap_verb = ["swap ", "change ", "switch ", "replace ", "make ", "cook ", "do ", "have ", "turn "]
        .iter()
        .any(|v| s.contains(v));
    // A bare " for " is too weak a signal (it appears in questions); require a
    // real swap verb or an explicit target separator.
    let has_target_prep = s.contains(" to ") || s.contains(" into ") || s.contains(':') || s.contains('=');
    if !swap_verb && !has_target_prep {
        return None;
    }

    // Dish extraction, most-specific separator first.
    let dish = if let Some((_, after)) = s.rsplit_once(" into ") {
        after.to_string()
    } else if let Some((_, after)) = s.rsplit_once(" to ") {
        after.to_string()
    } else if let Some((_, after)) = s.split_once(": ") {
        after.to_string()
    } else if let Some((_, after)) = s.split_once('=') {
        after.to_string()
    } else {
        // "make friday tacos" / "friday dinner tacos" — dish is what follows the
        // day word and the leading swap verb (with an optional "dinner"/"lunch").
        let (_, after) = pull_day(s, today);
        let after = strip_leading_words(
            &after,
            &["let's", "lets", "swap", "change", "switch", "replace", "make", "cook", "do", "have", "turn", "us"],
        );
        after
            .trim_start_matches("dinner")
            .trim_start_matches("lunch")
            .trim_start_matches("supper")
            .trim()
            .to_string()
    };
    // Strip a day phrase and fillers that leaked into the dish fragment.
    let (_, dish) = pull_day(&dish, today);
    let dish = scrub_fillers(&dish);
    let dish = dish
        .trim_end_matches(" instead")
        .trim_end_matches(" for dinner")
        .trim_end_matches(" for the week")
        .trim()
        .to_string();
    if dish.is_empty() || dish == "it" {
        return None;
    }
    // Ask → dish: strip any greeting/request husk that leaked through and gate
    // on the result reading like a dish. A miss falls back to the full pipeline
    // rather than writing the raw sentence into the meal cell.
    let dish = ask_to_dish(&dish)?;
    Some(FastLaneOp::MealSwap { day, dish })
}

// ---------------------------------------------------------------------------
// Word / day / time helpers
// ---------------------------------------------------------------------------

/// Whole-word containment (so "add" does not match "ladder"). Case handled by
/// the caller having lowercased `s`.
fn contains_word(s: &str, word: &str) -> bool {
    let bytes = s.as_bytes();
    let mut start = 0;
    while let Some(pos) = s[start..].find(word) {
        let i = start + pos;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after = i + word.len();
        let after_ok = after >= s.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = i + word.len();
    }
    false
}

/// The first weekday named in `s`, returned with the matched token's length so
/// callers can strip it.
fn find_weekday(s: &str) -> Option<(Weekday, usize)> {
    const NAMES: &[(&str, Weekday)] = &[
        ("monday", Weekday::Mon), ("tuesday", Weekday::Tue), ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu), ("friday", Weekday::Fri), ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
        ("mon", Weekday::Mon), ("tue", Weekday::Tue), ("wed", Weekday::Wed),
        ("thu", Weekday::Thu), ("fri", Weekday::Fri), ("sat", Weekday::Sat),
        ("sun", Weekday::Sun),
    ];
    let mut best: Option<(usize, Weekday, usize)> = None;
    for (name, wd) in NAMES {
        // Match the word, tolerating a possessive ("friday's").
        let bytes = s.as_bytes();
        let mut start = 0;
        while let Some(pos) = s[start..].find(name) {
            let i = start + pos;
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = i + name.len();
            let after_ok = after >= s.len()
                || !bytes[after].is_ascii_alphanumeric()
                || s[after..].starts_with("'s")
                || s[after..].starts_with("day"); // "fri" inside "friday" handled by longer entry
            if before_ok && after_ok {
                if best.map(|(bi, _, _)| i < bi).unwrap_or(true) {
                    best = Some((i, *wd, name.len()));
                }
                break;
            }
            start = i + name.len();
        }
    }
    best.map(|(_, wd, len)| (wd, len))
}

/// A relative day word ("today"/"tonight"/"this evening" → today,
/// "tomorrow" → today+1), if present.
fn relative_day(s: &str, today: NaiveDate) -> Option<Weekday> {
    if contains_word(s, "tomorrow") {
        return Some((today + Duration::days(1)).weekday());
    }
    if contains_word(s, "today")
        || contains_word(s, "tonight")
        || s.contains("this evening")
        || s.contains("this morning")
    {
        return Some(today.weekday());
    }
    None
}

/// Pull the first day reference (named weekday or relative word) out of `frag`,
/// returning the resolved weekday and the fragment with the day phrase removed.
fn pull_day(frag: &str, today: NaiveDate) -> (Option<Weekday>, String) {
    let day = find_weekday(frag).map(|(w, _)| w).or_else(|| relative_day(frag, today));
    let cleaned = scrub_day_phrases(frag);
    (day, cleaned)
}

/// True when a word (tolerating a trailing possessive/punctuation) names a day.
fn is_day_word(w: &str) -> bool {
    const DAYS: &[&str] = &[
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday",
        "mon", "tue", "wed", "thu", "fri", "sat", "sun",
        "today", "tonight", "tomorrow",
    ];
    let w = w.trim_end_matches(['.', ',', ':', '?', '!']).trim_end_matches("'s");
    DAYS.contains(&w)
}

/// Remove day-referring tokens and any preposition that introduces them
/// ("on friday", "for tuesday", "friday's") from a fragment, word by word.
fn scrub_day_phrases(frag: &str) -> String {
    let words: Vec<&str> = frag.split_whitespace().collect();
    let preps = ["on", "for", "this", "next", "to"];
    let mut out: Vec<&str> = Vec::new();
    for (idx, w) in words.iter().enumerate() {
        if is_day_word(w) {
            continue;
        }
        // Drop a preposition only when it directly introduces a day word.
        if preps.contains(&w.to_lowercase().as_str())
            && words.get(idx + 1).map(|n| is_day_word(n)).unwrap_or(false)
        {
            continue;
        }
        out.push(w);
    }
    out.join(" ")
}

/// Strip any of `words` from the front of `frag`, repeatedly, so a leading verb
/// or filler ("make", "let's") is removed before dish extraction.
fn strip_leading_words(frag: &str, words: &[&str]) -> String {
    let mut out = frag.trim().to_string();
    let mut changed = true;
    while changed {
        changed = false;
        for w in words {
            let prefix = format!("{w} ");
            if let Some(rest) = out.strip_prefix(&prefix) {
                out = rest.trim().to_string();
                changed = true;
            }
        }
    }
    out
}

/// Pull a clock time ("at 5pm", "5:30pm", "at 17:00", "at noon") out of a
/// fragment, returning `HH:MM` and the fragment with the time phrase removed.
fn pull_time(frag: &str) -> (Option<String>, String) {
    let words: Vec<&str> = frag.split_whitespace().collect();
    let mut out_words: Vec<String> = Vec::new();
    let mut time: Option<String> = None;
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        // "at <time>" — drop the "at" if the next token parses as a time.
        if w == "at" && i + 1 < words.len() {
            if let Some(t) = parse_clock(words[i + 1]) {
                time = Some(t);
                i += 2;
                continue;
            }
            if words[i + 1] == "noon" {
                time = Some("12:00".to_string());
                i += 2;
                continue;
            }
            if words[i + 1] == "midnight" {
                time = Some("00:00".to_string());
                i += 2;
                continue;
            }
        }
        if let Some(t) = parse_clock(w) {
            time = Some(t);
            i += 1;
            continue;
        }
        out_words.push(w.to_string());
        i += 1;
    }
    (time, out_words.join(" "))
}

/// Parse a single time token: "5pm", "5:30pm", "17:00", "9am". Returns `HH:MM`.
fn parse_clock(tok: &str) -> Option<String> {
    let t = tok.trim_end_matches(['.', ',']);
    let (body, ampm) = if let Some(b) = t.strip_suffix("pm") {
        (b, Some("pm"))
    } else if let Some(b) = t.strip_suffix("am") {
        (b, Some("am"))
    } else {
        (t, None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => {
            // A bare number is only a time when an am/pm suffix disambiguates it,
            // else "add 3 eggs" would parse "3" as 03:00.
            if ampm.is_none() {
                return None;
            }
            (body.parse::<u32>().ok()?, 0)
        }
    };
    if m > 59 || h > 23 {
        return None;
    }
    let h = match ampm {
        Some("pm") if h < 12 => h + 12,
        Some("am") if h == 12 => 0,
        _ => h,
    };
    if h > 23 {
        return None;
    }
    Some(format!("{h:02}:{m:02}"))
}

/// Strip leading articles/prepositions and trailing courtesies from an extracted
/// noun phrase.
fn scrub_fillers(s: &str) -> String {
    let mut out = s.trim().trim_matches(|c: char| c == '.' || c == ',' || c == '!' || c == '?').trim().to_string();
    let leading = ["to ", "a ", "an ", "some ", "the ", "for ", "us ", "me ", "please "];
    let mut changed = true;
    while changed {
        changed = false;
        for l in leading {
            if let Some(rest) = out.strip_prefix(l) {
                out = rest.trim().to_string();
                changed = true;
            }
        }
    }
    let trailing = [" please", " thanks", " thank you", " tonight", " today", " this week"];
    changed = true;
    while changed {
        changed = false;
        for t in trailing {
            if let Some(rest) = out.strip_suffix(t) {
                out = rest.trim().to_string();
                changed = true;
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Ask → dish transform + semantic sanity gate
// ---------------------------------------------------------------------------
//
// The round-trip parse check ([`verify_round_trip`]) proves an edit is
// *syntactically* placeable — it parses back to the change we intended. It does
// **not** prove the dish title reads like a dish. That gap is how a raw ask like
// "hey can you swap monday with zuxxhini and tofu" once landed verbatim in a meal
// cell (marked SET): the dish extraction handed the sentence straight through and
// the round-trip check happily confirmed the sentence was in the cell.
//
// These two helpers close that gap with *semantics*: before an extracted dish is
// accepted, [`ask_to_dish`] strips the greeting / request-verb / politeness husk
// off it and [`looks_like_dish`] asserts what remains actually reads like a dish
// (no "hey"/"can you"/"please", no question mark, a sensible length, and not a
// bare vagueness like "something nice"). If the transform cannot confidently
// produce a dish, it returns `None` and the matcher falls back — the ask goes to
// the full pipeline rather than writing a sentence into the plan.

/// Leading ask-shaped tokens/phrases peeled off the front of an extracted dish
/// fragment. Greetings, polite request verbs, the swap verbs themselves, and the
/// little connectors ("with"/"to"/"for") that trail them.
const LEADING_ASK_PHRASES: &[&str] = &[
    "hey there", "hey", "hi there", "hi", "hello", "ok", "okay", "yo", "so",
    "please", "pls", "kindly", "just", "maybe", "actually",
    "can you", "could you", "would you", "can we", "could we", "will you",
    "i'd like", "id like", "i would like", "i want", "we want", "we'd like",
    "how about", "what about", "lets", "let's", "us to", "me to",
    "swap", "change", "switch", "replace", "make", "cook", "do", "have", "turn",
    "us", "it", "to", "into", "with", "for", "the", "a", "an", "some",
];

/// Trailing junk peeled off the end — dangling connectors and courtesy left over
/// once the day and verb are gone ("something nice **for**", "tacos **please**").
const TRAILING_ASK_JUNK: &[&str] = &[
    "for", "with", "to", "and", "or", "instead", "please", "thanks",
    "tonight", "today", "for dinner", "for the week",
];

/// Transform a raw dish fragment pulled from an ask into a clean dish title:
/// strip the leading greeting / request-verb / politeness husk and any trailing
/// dangling connector, then apply [`looks_like_dish`]. Returns `None` when it
/// cannot confidently produce something that reads like a dish, so the caller
/// falls back to the full pipeline instead of writing the raw ask.
fn ask_to_dish(raw: &str) -> Option<String> {
    let mut s = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ',' | ';' | ':'))
        .trim()
        .to_lowercase();

    let mut changed = true;
    while changed {
        changed = false;
        for p in LEADING_ASK_PHRASES {
            if let Some(rest) = s.strip_prefix(&format!("{p} ")) {
                s = rest.trim().to_string();
                changed = true;
            }
        }
        for t in TRAILING_ASK_JUNK {
            if let Some(rest) = s.strip_suffix(&format!(" {t}")) {
                s = rest.trim().to_string();
                changed = true;
            } else if s == *t {
                s.clear();
                changed = true;
            }
        }
    }

    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if looks_like_dish(&s) {
        Some(s)
    } else {
        None
    }
}

/// The semantic sanity gate: does `title` read like a dish rather than the raw
/// ask that produced it? Rejects empty/over-long strings, question marks,
/// leftover greeting/request markers, and bare vagueness. Kept pure and
/// conservative — a false reject just sends the ask to the full pipeline (safe),
/// while a false accept is exactly the garbage-in-the-plan bug this closes.
fn looks_like_dish(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() || t.contains('?') {
        return false;
    }
    // A dish is a handful of words, not a sentence.
    if t.chars().count() > 60 {
        return false;
    }
    let words: Vec<String> = t
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();
    if words.is_empty() || words.len() > 8 {
        return false;
    }

    // Single words that betray a raw ask rather than a dish.
    const BANNED_WORDS: &[&str] = &[
        "hey", "hi", "hello", "please", "pls", "thanks", "thx",
        "swap", "switch", "replace", "wanna", "gonna",
    ];
    if words.iter().any(|w| BANNED_WORDS.contains(&w.as_str())) {
        return false;
    }

    // Adjacent pairs that only appear in a request ("can you", "i want", …).
    const BANNED_BIGRAMS: &[(&str, &str)] = &[
        ("can", "you"), ("could", "you"), ("would", "you"), ("can", "we"),
        ("will", "you"), ("i", "want"), ("we", "want"), ("i'd", "like"),
        ("how", "about"), ("what", "about"), ("change", "to"),
    ];
    if words
        .windows(2)
        .any(|w| BANNED_BIGRAMS.iter().any(|(a, b)| w[0] == *a && w[1] == *b))
    {
        return false;
    }

    // A "dish" made only of placeholders — "something nice", "anything",
    // "whatever you like" — carries no actual food and cannot be applied.
    !is_vague_dish(&words)
}

/// True when every word is a vague placeholder/adjective with no concrete food.
fn is_vague_dish(words: &[String]) -> bool {
    const VAGUE: &[&str] = &[
        "something", "anything", "everything", "whatever", "some", "any",
        "nice", "good", "great", "tasty", "yummy", "nicer", "better", "different",
        "healthy", "light", "quick", "easy", "simple", "you", "like", "for",
        "dinner", "lunch", "supper", "meal", "food", "thing", "please", "else",
        "it", "them", "one", "that", "this",
    ];
    words.iter().all(|w| VAGUE.contains(&w.as_str()))
}

// ---------------------------------------------------------------------------
// Report-back rendering
// ---------------------------------------------------------------------------

/// The immediate, plain-voice confirmation line for an applied edit.
pub fn report_line(op: &FastLaneOp) -> String {
    match op {
        FastLaneOp::MealSwap { day, dish } => {
            format!("Done — {dish} {} {}", weekday_name(*day), dish_emoji(dish))
        }
        FastLaneOp::MealAdd { day, addition } => {
            format!("Done — added {addition} to {} {}", weekday_name(*day), dish_emoji(addition))
        }
        FastLaneOp::MealRemove { day, target } => {
            format!("Done — dropped {target} from {} ✂️", weekday_name(*day))
        }
        FastLaneOp::ShoppingAdd { item } => {
            format!("Done — {item} on the shopping list 🛒")
        }
        FastLaneOp::ReminderSet { text, day, time } => {
            let when = match (day, time) {
                (Some(d), Some(t)) => format!(" {} at {}", weekday_name(*d), t),
                (Some(d), None) => format!(" {}", weekday_name(*d)),
                (None, Some(t)) => format!(" at {}", t),
                (None, None) => String::new(),
            };
            format!("Done — I'll remind you to {text}{when} ⏰")
        }
    }
}

fn dish_emoji(dish: &str) -> &'static str {
    let d = dish.to_lowercase();
    if d.contains("taco") {
        "🌮"
    } else if d.contains("pizza") {
        "🍕"
    } else if d.contains("pasta") || d.contains("spaghetti") || d.contains("linguine") {
        "🍝"
    } else if d.contains("sushi") {
        "🍣"
    } else if d.contains("burger") {
        "🍔"
    } else if d.contains("salad") {
        "🥗"
    } else if d.contains("soup") {
        "🍲"
    } else if d.contains("curry") {
        "🍛"
    } else {
        "🍽️"
    }
}

fn weekday_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastLaneError {
    /// No plan file exists for the current week — nothing to edit directly.
    NoPlan,
    /// The referenced day has no row in the meal table.
    DayNotFound,
    /// The edit did not apply (e.g. a removal target that is not present).
    NotApplicable(String),
    /// The edited document failed to round-trip through the plan parser — the
    /// change is refused rather than written.
    RoundTrip(String),
    /// A filesystem error reading or writing the plan.
    Io(String),
}

impl std::fmt::Display for FastLaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FastLaneError::NoPlan => write!(f, "no plan file for the current week"),
            FastLaneError::DayNotFound => write!(f, "no meal row for that day"),
            FastLaneError::NotApplicable(m) => write!(f, "edit not applicable: {m}"),
            FastLaneError::RoundTrip(m) => write!(f, "plan round-trip failed: {m}"),
            FastLaneError::Io(m) => write!(f, "plan io error: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure edit + round-trip validation
// ---------------------------------------------------------------------------

/// How a meal row's dish cell is transformed.
enum DishEdit {
    Replace(String),
    Append(String),
    Remove(String),
}

/// Apply one fast-lane op to a plan document's markdown `content` (identified by
/// `week_code` for the parser). Returns the edited content, having **round-tripped
/// it through the real plan parser** to prove the change landed. Pure — no I/O.
pub fn apply_to_content(
    week_code: &str,
    content: &str,
    op: &FastLaneOp,
) -> Result<String, FastLaneError> {
    apply_to_content_with_calendar_owner(week_code, content, op, None)
}

/// Apply one fast-lane operation with the project-configured owner for calendar
/// rows. Reminder writes fail closed when no owner is available; every other
/// operation is unchanged. Keeping this value caller-supplied prevents the plan
/// from depending on a compiled household persona.
pub fn apply_to_content_with_calendar_owner(
    week_code: &str,
    content: &str,
    op: &FastLaneOp,
    calendar_owner: Option<&str>,
) -> Result<String, FastLaneError> {
    let edited = match op {
        FastLaneOp::MealSwap { day, dish } => {
            edit_meal_dish(content, *day, &DishEdit::Replace(dish.clone()))
                .ok_or(FastLaneError::DayNotFound)?
        }
        FastLaneOp::MealAdd { day, addition } => {
            edit_meal_dish(content, *day, &DishEdit::Append(addition.clone()))
                .ok_or(FastLaneError::DayNotFound)?
        }
        FastLaneOp::MealRemove { day, target } => {
            edit_meal_dish(content, *day, &DishEdit::Remove(target.clone())).ok_or_else(|| {
                FastLaneError::NotApplicable(format!("'{target}' not found on that day"))
            })?
        }
        FastLaneOp::ShoppingAdd { item } => add_shopping_item(content, item)
            .ok_or_else(|| FastLaneError::NotApplicable("no shopping list to add to".into()))?,
        FastLaneOp::ReminderSet { text, day, time } => {
            let owner = calendar_owner
                .map(str::trim)
                .filter(|owner| {
                    !owner.is_empty()
                        && !owner
                            .chars()
                            .any(|c| matches!(c, '|' | '\n' | '\r'))
                })
                .ok_or_else(|| {
                    FastLaneError::NotApplicable(
                        "no configured calendar owner for the reminder".into(),
                    )
                })?;
            add_reminder_row(content, week_code, text, *day, time.as_deref(), owner)
                .ok_or_else(|| FastLaneError::NotApplicable("no calendar to add a reminder to".into()))?
        }
    };

    // Round-trip: the edited document MUST parse back to the change we intended.
    let doc = PlanDoc::parse(week_code, &edited);
    verify_round_trip(&doc, op, calendar_owner)?;
    Ok(edited)
}

/// Assert the parsed, re-read document actually reflects the operation.
fn verify_round_trip(
    doc: &PlanDoc,
    op: &FastLaneOp,
    calendar_owner: Option<&str>,
) -> Result<(), FastLaneError> {
    let dish_on = |wd: Weekday| -> Option<String> {
        doc.meals
            .iter()
            .find(|m| m.weekday.eq_ignore_ascii_case(weekday_short(wd)))
            .map(|m| m.dish.to_lowercase())
    };
    match op {
        FastLaneOp::MealSwap { day, dish } => {
            let got = dish_on(*day).ok_or(FastLaneError::DayNotFound)?;
            if !got.contains(&dish.to_lowercase()) {
                return Err(FastLaneError::RoundTrip(format!(
                    "swapped dish '{dish}' absent after re-parse (got '{got}')"
                )));
            }
        }
        FastLaneOp::MealAdd { day, addition } => {
            let got = dish_on(*day).ok_or(FastLaneError::DayNotFound)?;
            if !got.contains(&addition.to_lowercase()) {
                return Err(FastLaneError::RoundTrip(format!(
                    "addition '{addition}' absent after re-parse (got '{got}')"
                )));
            }
        }
        FastLaneOp::MealRemove { day, target } => {
            let got = dish_on(*day).ok_or(FastLaneError::DayNotFound)?;
            if got.contains(&target.to_lowercase()) {
                return Err(FastLaneError::RoundTrip(format!(
                    "removal target '{target}' still present after re-parse (got '{got}')"
                )));
            }
        }
        FastLaneOp::ShoppingAdd { item } => {
            let present = doc
                .shopping
                .iter()
                .flat_map(|sec| sec.items.iter())
                .any(|it| it.to_lowercase().contains(&item.to_lowercase()));
            if !present {
                return Err(FastLaneError::RoundTrip(format!(
                    "item '{item}' absent from shopping list after re-parse"
                )));
            }
        }
        FastLaneOp::ReminderSet { text, .. } => {
            let key: String = text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            let present = doc.calendar.iter().any(|e| {
                let ev = e.event.to_lowercase();
                ev.contains("reminder")
                    && ev.contains(&key.to_lowercase())
                    && calendar_owner
                        .map(|owner| e.source.eq_ignore_ascii_case(owner.trim()))
                        .unwrap_or(false)
            });
            if !present {
                return Err(FastLaneError::RoundTrip(
                    "reminder row or configured owner absent after re-parse".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Edit the dish cell of the first meal-table row whose day matches `day`.
/// Returns `None` when there is no such row (or nothing to remove).
fn edit_meal_dish(content: &str, day: Weekday, edit: &DishEdit) -> Option<String> {
    let short = weekday_short(day).to_lowercase();
    let mut in_meals = false;
    let mut out: Vec<String> = Vec::new();
    let mut applied = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            in_meals = h2.to_lowercase().contains("meal");
            out.push(line.to_string());
            continue;
        }
        if in_meals && !applied && trimmed.starts_with('|') {
            if let Some(cells) = split_cells(trimmed) {
                let day_cell = cells.first().map(|c| c.to_lowercase()).unwrap_or_default();
                let is_row = cells.len() >= 3
                    && day_cell.split_whitespace().next() == Some(short.as_str());
                if is_row {
                    let mut cells = cells;
                    let dish = cells[2].trim().to_string();
                    let new_dish = match edit {
                        DishEdit::Replace(d) => d.clone(),
                        DishEdit::Append(a) => format!("{dish} + {a}"),
                        DishEdit::Remove(t) => remove_component(&dish, t)?,
                    };
                    cells[2] = new_dish;
                    // A swapped dish makes any per-dish note (e.g. the iron note)
                    // stale — blank it to the plan's own "no note" marker.
                    if matches!(edit, DishEdit::Replace(_)) && cells.len() >= 5 {
                        cells[4] = "—".to_string();
                    }
                    out.push(render_row(&cells));
                    applied = true;
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }

    if applied {
        Some(out.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
    } else {
        None
    }
}

/// Remove a named component from a dish string, trimming the joining separator.
/// Returns `None` when the target is not present or removing it empties the dish.
fn remove_component(dish: &str, target: &str) -> Option<String> {
    let low = dish.to_lowercase();
    let t = target.to_lowercase();
    let idx = low.find(&t)?;
    let end = idx + t.len();
    // Absorb an adjacent separator so we don't leave "a, " or " + " dangling.
    let (mut lo, mut hi) = (idx, end);
    let bytes = dish.as_bytes();
    // Leading " + " / ", " / " and ".
    for sep in [" + ", ", ", " and "] {
        if lo >= sep.len() && dish[..lo].to_lowercase().ends_with(sep) {
            lo -= sep.len();
            break;
        }
    }
    // Trailing separator if we removed a leading item.
    if lo == idx {
        for sep in [" + ", ", ", " and "] {
            if dish[hi..].to_lowercase().starts_with(sep) {
                hi += sep.len();
                break;
            }
        }
    }
    let _ = bytes;
    let mut result = String::new();
    result.push_str(&dish[..lo]);
    result.push_str(&dish[hi..]);
    let result = result.trim().trim_end_matches(',').trim().to_string();
    if result.is_empty() || result.to_lowercase() == low {
        None
    } else {
        Some(result)
    }
}

/// Append `item` as a bullet under the first `###` store section of the shopping
/// list. Returns `None` when there is no shopping section to add to.
fn add_shopping_item(content: &str, item: &str) -> Option<String> {
    let mut in_shopping = false;
    let mut seen_section = false;
    let mut inserted = false;
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            // Leaving the shopping section before inserting? Insert now.
            if in_shopping && seen_section && !inserted {
                out.push(format!("- {item}"));
                inserted = true;
            }
            in_shopping = h2.to_lowercase().contains("shopping");
            seen_section = false;
            out.push(line.to_string());
            continue;
        }
        if in_shopping && trimmed.starts_with("### ") {
            seen_section = true;
        }
        out.push(line.to_string());
        // Insert after the last bullet of the first store section: when the next
        // line is no longer a bullet and we're inside the first section.
        if in_shopping && seen_section && !inserted && trimmed.starts_with("- ") {
            let next_is_bullet = lines
                .get(i + 1)
                .map(|l| l.trim().starts_with("- "))
                .unwrap_or(false);
            if !next_is_bullet {
                out.push(format!("- {item}"));
                inserted = true;
            }
        }
    }
    if in_shopping && seen_section && !inserted {
        out.push(format!("- {item}"));
        inserted = true;
    }
    if inserted {
        Some(out.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
    } else {
        None
    }
}

/// Append a `⏰ Reminder` row to the calendar table. The day defaults to the
/// plan's Monday when unspecified; the time defaults to 09:00. Returns `None`
/// when there is no calendar table.
fn add_reminder_row(
    content: &str,
    week_code: &str,
    text: &str,
    day: Option<Weekday>,
    time: Option<&str>,
    owner: &str,
) -> Option<String> {
    // Resolve the concrete date for the day cell from the plan week.
    let doc = PlanDoc::parse(week_code, content);
    let start = doc.start?;
    let target = day
        .map(|wd| {
            let delta = (wd.num_days_from_monday() as i64) - (start.weekday().num_days_from_monday() as i64);
            start + Duration::days(delta.rem_euclid(7))
        })
        .unwrap_or(start);
    let short = weekday_short(target.weekday());
    let mmdd = format!("{:02}-{:02}", target.month(), target.day());
    let time = time.unwrap_or("09:00");
    let row = format!("| {short} {mmdd} | {time} | ⏰ Reminder: {text} | {owner} |");

    // Insert as the last row of the calendar table.
    let mut in_cal = false;
    let mut out: Vec<String> = Vec::new();
    let mut inserted = false;
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            if in_cal && !inserted {
                out.push(row.clone());
                inserted = true;
            }
            in_cal = h2.to_lowercase().contains("calendar");
            out.push(line.to_string());
            continue;
        }
        out.push(line.to_string());
        if in_cal && !inserted && trimmed.starts_with('|') {
            let next_is_row = lines
                .get(i + 1)
                .map(|l| l.trim().starts_with('|'))
                .unwrap_or(false);
            if !next_is_row {
                out.push(row.clone());
                inserted = true;
            }
        }
    }
    if in_cal && !inserted {
        out.push(row.clone());
        inserted = true;
    }
    if inserted {
        Some(out.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
    } else {
        None
    }
}

/// Split a markdown table row `| a | b | c |` into its cell strings (trimmed).
fn split_cells(line: &str) -> Option<Vec<String>> {
    if !line.starts_with('|') {
        return None;
    }
    let inner = line.trim().trim_matches('|');
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

/// Re-render a table row from trimmed cells.
fn render_row(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

// ---------------------------------------------------------------------------
// File-level orchestrator
// ---------------------------------------------------------------------------

/// The result of a fast-lane attempt against the live plan file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastLaneResult {
    /// The edit was applied directly; `report` is the immediate confirmation.
    Applied {
        report: String,
        op: FastLaneOp,
        week_code: String,
    },
    /// Not a fast-lane ask (or the direct edit could not be applied) — the caller
    /// runs the full task pipeline as today. `reason` is for logging only.
    Fallback { reason: String },
}

/// Locate the plan file that is "current" as of `today`, returning its path,
/// week code, and parsed doc. Mirrors [`family_plan::current_plan`] but keeps the
/// path so the file can be edited in place.
fn current_plan_file(root: &Path, today: NaiveDate) -> Option<(PathBuf, String, PlanDoc)> {
    let plans_dir = root.join("plans");
    let mut candidates: Vec<(PathBuf, String, PlanDoc)> = Vec::new();
    for entry in std::fs::read_dir(&plans_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let week_code = match week_code_of(stem) {
            Some(w) => w,
            None => continue,
        };
        if let Ok(content) = std::fs::read_to_string(&path) {
            candidates.push((path, week_code.clone(), PlanDoc::parse(&week_code, &content)));
        }
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| a.1.cmp(&b.1));

    // Covers today?
    if let Some(idx) = candidates.iter().position(|(_, _, d)| d.covers(today)) {
        return Some(candidates.swap_remove(idx));
    }
    // Nearest upcoming.
    if let Some(idx) = candidates
        .iter()
        .enumerate()
        .filter(|(_, (_, _, d))| d.start.map(|s| s > today).unwrap_or(false))
        .min_by_key(|(_, (_, _, d))| d.start.unwrap())
        .map(|(i, _)| i)
    {
        return Some(candidates.swap_remove(idx));
    }
    // Latest we have.
    candidates.pop()
}

/// The `YYYY-Wnn` code from a filename stem like `2026-W29-family-plan`.
fn week_code_of(stem: &str) -> Option<String> {
    let mut it = stem.splitn(3, '-');
    let year = it.next()?;
    let week = it.next()?;
    if year.len() == 4
        && year.chars().all(|c| c.is_ascii_digit())
        && (week.starts_with('W') || week.starts_with('w'))
        && week.len() >= 2
        && week[1..].chars().all(|c| c.is_ascii_digit())
    {
        Some(format!("{}-{}", year, week.to_ascii_uppercase()))
    } else {
        None
    }
}

/// Attempt the fast lane end-to-end against the live plan file under `root`.
///
/// Classifies `message`; on a clean hit, edits the current week's plan file in
/// place (round-trip validated) and returns [`FastLaneResult::Applied`] with the
/// immediate report. Any miss — not a simple edit, a compound ask, a missing
/// plan, an edit that would not round-trip — returns [`FastLaneResult::Fallback`]
/// and the caller runs the full pipeline exactly as today.
pub fn run_fast_lane(root: &Path, message: &str, today: NaiveDate) -> FastLaneResult {
    run_fast_lane_with_calendar_owner(root, message, today, None)
}

/// Run the file-level fast lane with the project-configured calendar owner.
/// The caller derives this from `household.toml`; a missing owner makes only a
/// reminder operation fall back without changing the plan.
pub fn run_fast_lane_with_calendar_owner(
    root: &Path,
    message: &str,
    today: NaiveDate,
    calendar_owner: Option<&str>,
) -> FastLaneResult {
    let op = match classify(message, today) {
        Classification::FastLane(op) => op,
        Classification::Fallback(reason) => {
            return FastLaneResult::Fallback {
                reason: format!("{reason:?}"),
            };
        }
    };

    let (path, week_code, _doc) = match current_plan_file(root, today) {
        Some(t) => t,
        None => {
            return FastLaneResult::Fallback {
                reason: "no current plan file".into(),
            };
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return FastLaneResult::Fallback {
                reason: format!("read plan: {e}"),
            };
        }
    };

    match apply_to_content_with_calendar_owner(
        &week_code,
        &content,
        &op,
        calendar_owner,
    ) {
        Ok(edited) => {
            if let Err(e) = crate::atomic_file::write_atomic(&path, edited.as_bytes()) {
                return FastLaneResult::Fallback {
                    reason: format!("write plan: {e}"),
                };
            }
            FastLaneResult::Applied {
                report: report_line(&op),
                op,
                week_code,
            }
        }
        Err(e) => FastLaneResult::Fallback {
            reason: format!("direct edit refused ({e}) — deferring to full pipeline"),
        },
    }
}

/// Record a fast-lane edit as a brief `queued → done` node in the work graph so
/// the change still shows up in the constellation/timeline with its origin — the
/// same visibility a full-pipeline ask gets, minus the twenty minutes.
///
/// Best-effort: a graph write failure never blocks the (already-applied) edit or
/// its report.
pub fn stamp_graph_node(
    workgraph_dir: &Path,
    origin: &crate::graph::TaskOrigin,
    op: &FastLaneOp,
    report: &str,
) {
    use crate::graph::{Node, Status, Task, WorkGraph};
    let path = workgraph_dir.join("graph.jsonl");
    let mut graph = if path.exists() {
        match crate::parser::load_graph(&path) {
            Ok(g) => g,
            Err(_) => return,
        }
    } else {
        WorkGraph::new()
    };
    let title = format!("Fast-lane {}: {report}", op.kind_label());
    let id = crate::notify::lifecycle::derive_task_id(&title, |cand| graph.get_node(cand).is_some());
    let now = chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let task = Task {
        id,
        title,
        description: Some(format!(
            "Applied directly via the fast lane from a {} chat request.",
            origin.channel.label(),
        )),
        status: Status::Done,
        created_at: Some(now.clone()),
        completed_at: Some(now.clone()),
        last_interaction_at: Some(now),
        tags: vec!["fast-lane".to_string()],
        origin: Some(origin.clone()),
        ..Default::default()
    };
    graph.add_node(Node::Task(task));
    let _ = crate::parser::save_graph(&graph, &path);
}

#[cfg(test)]
mod tests {
    use super::*;

    const W29: &str = include_str!("../../tests/fixtures/family_plan_w29.md");

    fn today() -> NaiveDate {
        // A Tuesday inside the W29 plan week (2026-07-14).
        NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()
    }

    fn fast(msg: &str) -> FastLaneOp {
        match classify(msg, today()) {
            Classification::FastLane(op) => op,
            other => panic!("expected fast lane for {msg:?}, got {other:?}"),
        }
    }

    fn fallback(msg: &str) -> FallbackReason {
        match classify(msg, today()) {
            Classification::Fallback(r) => r,
            other => panic!("expected fallback for {msg:?}, got {other:?}"),
        }
    }

    // ---- classification: each fast-lane op -------------------------------

    #[test]
    fn fast_lane_classifies_meal_swap() {
        assert_eq!(
            fast("swap Friday to tacos"),
            FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() }
        );
        assert_eq!(
            fast("change Friday's dinner to homemade pizza"),
            FastLaneOp::MealSwap { day: Weekday::Fri, dish: "homemade pizza".into() }
        );
        assert_eq!(
            fast("make Friday tacos"),
            FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() }
        );
    }

    #[test]
    fn fast_lane_classifies_meal_add() {
        assert_eq!(
            fast("add a dessert on Tuesday"),
            FastLaneOp::MealAdd { day: Weekday::Tue, addition: "dessert".into() }
        );
        assert_eq!(
            fast("add a side salad to Monday"),
            FastLaneOp::MealAdd { day: Weekday::Mon, addition: "side salad".into() }
        );
    }

    #[test]
    fn fast_lane_classifies_meal_remove() {
        assert_eq!(
            fast("drop the side salad on Monday"),
            FastLaneOp::MealRemove { day: Weekday::Mon, target: "side salad".into() }
        );
        assert_eq!(
            fast("remove Friday's dessert"),
            FastLaneOp::MealRemove { day: Weekday::Fri, target: "dessert".into() }
        );
    }

    #[test]
    fn fast_lane_classifies_shopping_add() {
        assert_eq!(
            fast("add milk to the shopping list"),
            FastLaneOp::ShoppingAdd { item: "milk".into() }
        );
        assert_eq!(
            fast("put eggs on the list"),
            FastLaneOp::ShoppingAdd { item: "eggs".into() }
        );
        assert_eq!(
            fast("we need bananas on the shopping list"),
            FastLaneOp::ShoppingAdd { item: "bananas".into() }
        );
    }

    #[test]
    fn fast_lane_classifies_reminder_set() {
        assert_eq!(
            fast("remind me to defrost the chicken Friday at 5pm"),
            FastLaneOp::ReminderSet {
                text: "defrost the chicken".into(),
                day: Some(Weekday::Fri),
                time: Some("17:00".into()),
            }
        );
        assert_eq!(
            fast("remind me to call the plumber"),
            FastLaneOp::ReminderSet { text: "call the plumber".into(), day: None, time: None }
        );
    }

    // ---- fallback classification -----------------------------------------

    #[test]
    fn fast_lane_falls_back_on_open_ended_ask() {
        assert_eq!(fallback("what's for dinner on Friday?"), FallbackReason::NotASimpleEdit);
        assert_eq!(fallback("rebalance the week around Nadin's travel"), FallbackReason::Compound);
        assert_eq!(fallback("can you plan the whole week for me"), FallbackReason::Compound);
        assert_eq!(fallback("thanks so much!"), FallbackReason::NotASimpleEdit);
    }

    #[test]
    fn fast_lane_mixed_ask_swap_plus_rebalance_falls_back_to_full_pipeline() {
        // The headline mixed-ask case: a clean swap tangled with open-ended work.
        assert_eq!(
            fallback("swap Friday to tacos and rebalance the week"),
            FallbackReason::Compound
        );
    }

    #[test]
    fn fast_lane_mixed_two_edits_falls_back() {
        // Two independent simple edits in one breath is still "compound" — the
        // full pipeline handles the pair rather than half-applying one.
        assert_eq!(
            fallback("swap Friday to tacos and add milk to the shopping list"),
            FallbackReason::Compound
        );
        assert_eq!(
            fallback("change Friday to pizza and Saturday to sushi"),
            FallbackReason::Compound
        );
    }

    // ---- ask → dish transform + semantic sanity gate ---------------------

    #[test]
    fn fast_lane_asks_become_dishes_not_sentences() {
        // Luca's exact sentence (2026-07-14): a raw greeting+request must be
        // transformed into a dish, never applied verbatim as the meal title.
        assert_eq!(
            fast("hey can you swap monday with zuxxhini and tofu"),
            FastLaneOp::MealSwap { day: Weekday::Mon, dish: "zuxxhini and tofu".into() }
        );
        // A polite request husk on a swap is peeled to the dish.
        assert_eq!(
            fast("can you please make friday tacos"),
            FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() }
        );
        // A meal add still resolves to a clean component.
        assert_eq!(
            fast("add pasta thursday"),
            FastLaneOp::MealAdd { day: Weekday::Thu, addition: "pasta".into() }
        );
    }

    #[test]
    fn fast_lane_vague_dish_falls_back_to_full_pipeline() {
        // "swap something nice for friday" carries no actual dish — the sanity
        // gate refuses it so the full pipeline can ask what "nice" means.
        assert_eq!(fallback("swap something nice for friday"), FallbackReason::NotASimpleEdit);
        assert_eq!(fallback("change monday to something healthy"), FallbackReason::NotASimpleEdit);
    }

    #[test]
    fn looks_like_dish_gate_rejects_asks_and_accepts_dishes() {
        // Accepts real dishes.
        assert!(looks_like_dish("zucchini & tofu stir-fry"));
        assert!(looks_like_dish("tacos"));
        assert!(looks_like_dish("homemade margherita pizza"));
        // Rejects raw asks, questions, vagueness, and sentences.
        assert!(!looks_like_dish("hey can you swap with zuxxhini and tofu"));
        assert!(!looks_like_dish("what's for dinner?"));
        assert!(!looks_like_dish("something nice"));
        assert!(!looks_like_dish(""));
        assert!(!looks_like_dish(
            "please could you change it to a really long rambling sentence about dinner tonight"
        ));
    }

    #[test]
    fn ask_to_dish_strips_husk_or_refuses() {
        assert_eq!(ask_to_dish("hey can you swap with tacos").as_deref(), Some("tacos"));
        assert_eq!(ask_to_dish("please make it homemade pizza").as_deref(), Some("homemade pizza"));
        assert_eq!(ask_to_dish("something nice for"), None);
        assert_eq!(ask_to_dish("can you swap it"), None);
    }

    #[test]
    fn fast_lane_shopping_compound_items_stay_single_op() {
        // "milk and eggs" is one shopping add, not a compound ask.
        assert_eq!(
            fast("add milk and eggs to the shopping list"),
            FastLaneOp::ShoppingAdd { item: "milk and eggs".into() }
        );
    }

    // ---- apply + round-trip against the real parser ----------------------

    #[test]
    fn fast_lane_apply_meal_swap_round_trips() {
        let op = FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() };
        let edited = apply_to_content("2026-W29", W29, &op).expect("swap applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        let fri = doc.meals.iter().find(|m| m.weekday == "Fri").unwrap();
        assert_eq!(fri.dish, "tacos");
        // Untouched days survive.
        assert!(doc.meals.iter().any(|m| m.weekday == "Mon" && m.dish.contains("curry")));
        assert_eq!(doc.meals.len(), 7, "no rows lost");
    }

    #[test]
    fn fast_lane_apply_meal_add_round_trips() {
        let op = FastLaneOp::MealAdd { day: Weekday::Wed, addition: "garlic bread".into() };
        let edited = apply_to_content("2026-W29", W29, &op).expect("add applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        let wed = doc.meals.iter().find(|m| m.weekday == "Wed").unwrap();
        assert!(wed.dish.contains("garlic bread"));
        assert!(wed.dish.contains("Lentil"), "original dish preserved");
    }

    #[test]
    fn fast_lane_apply_meal_remove_round_trips() {
        // Monday's dish has a "side salad" only after an add; build that first.
        let added = apply_to_content(
            "2026-W29",
            W29,
            &FastLaneOp::MealAdd { day: Weekday::Mon, addition: "side salad".into() },
        )
        .unwrap();
        let removed = apply_to_content(
            "2026-W29",
            &added,
            &FastLaneOp::MealRemove { day: Weekday::Mon, target: "side salad".into() },
        )
        .expect("remove applies");
        let doc = PlanDoc::parse("2026-W29", &removed);
        let mon = doc.meals.iter().find(|m| m.weekday == "Mon").unwrap();
        assert!(!mon.dish.to_lowercase().contains("side salad"));
        assert!(mon.dish.contains("curry"), "the main dish stays");
    }

    #[test]
    fn fast_lane_apply_shopping_add_round_trips() {
        let op = FastLaneOp::ShoppingAdd { item: "olive oil".into() };
        let edited = apply_to_content("2026-W29", W29, &op).expect("shopping add applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        assert!(doc
            .shopping
            .iter()
            .flat_map(|s| s.items.iter())
            .any(|i| i.contains("olive oil")));
    }

    #[test]
    fn fast_lane_apply_reminder_round_trips() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("household.toml"),
            r#"
[[agent]]
id = "harbor"
domains = ["calendar", "coordination"]

[[agent]]
id = "cedar"
domains = ["meals"]
"#,
        )
        .unwrap();
        let owners = crate::notify::ownership::OwnerMap::load(root.path());
        let calendar_owner =
            owners.owner_for_domain(crate::notify::ownership::Domain::Calendar);
        let op = FastLaneOp::ReminderSet {
            text: "defrost the chicken".into(),
            day: Some(Weekday::Fri),
            time: Some("17:00".into()),
        };
        let edited = apply_to_content_with_calendar_owner(
            "2026-W29",
            W29,
            &op,
            calendar_owner,
        )
        .expect("reminder applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        assert!(doc.calendar.iter().any(|e| {
            e.event.to_lowercase().contains("reminder")
                && e.event.to_lowercase().contains("defrost")
                && e.weekday == "Fri"
                && e.time == "17:00"
                && e.source == "harbor"
        }));
    }

    #[test]
    fn fast_lane_apply_unknown_day_errors_not_corrupts() {
        // A plan without that weekday row → DayNotFound, never a silent no-op write.
        let plan = "# Plan\n\n**Week of Monday 2026-07-13 → Sunday 2026-07-19**\n\n## 1. Meal plan\n\n| Day | Slot | Dish |\n|---|---|---|\n| Mon 07-13 | Veg | Curry |\n";
        let op = FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() };
        assert_eq!(apply_to_content("2026-W29", plan, &op), Err(FastLaneError::DayNotFound));
    }

    #[test]
    fn fast_lane_remove_absent_component_errors() {
        let op = FastLaneOp::MealRemove { day: Weekday::Fri, target: "pineapple".into() };
        let err = apply_to_content("2026-W29", W29, &op).unwrap_err();
        assert!(matches!(err, FastLaneError::NotApplicable(_)));
    }

    // ---- report-back lines -----------------------------------------------

    #[test]
    fn fast_lane_report_lines_are_plain_voice() {
        assert_eq!(
            report_line(&FastLaneOp::MealSwap { day: Weekday::Fri, dish: "tacos".into() }),
            "Done — tacos Friday 🌮"
        );
        assert_eq!(
            report_line(&FastLaneOp::ShoppingAdd { item: "milk".into() }),
            "Done — milk on the shopping list 🛒"
        );
        assert_eq!(
            report_line(&FastLaneOp::MealRemove { day: Weekday::Mon, target: "side salad".into() }),
            "Done — dropped side salad from Monday ✂️"
        );
    }

    // ---- end-to-end orchestrator against a temp plan dir -----------------

    #[test]
    fn fast_lane_run_end_to_end_applies_and_reports() {
        let dir = std::env::temp_dir().join(format!("fastlane-e2e-{}", std::process::id()));
        let plans = dir.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan_path = plans.join("2026-W29-family-plan.md");
        std::fs::write(&plan_path, W29).unwrap();

        let result = run_fast_lane(&dir, "swap Friday to tacos", today());
        match &result {
            FastLaneResult::Applied { report, op, week_code } => {
                assert_eq!(report, "Done — tacos Friday 🌮");
                assert_eq!(week_code, "2026-W29");
                assert!(matches!(op, FastLaneOp::MealSwap { .. }));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // The file on disk actually changed and still parses.
        let after = std::fs::read_to_string(&plan_path).unwrap();
        let doc = PlanDoc::parse("2026-W29", &after);
        assert_eq!(doc.meals.iter().find(|m| m.weekday == "Fri").unwrap().dish, "tacos");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fast_lane_run_falls_back_when_not_simple() {
        let dir = std::env::temp_dir().join(format!("fastlane-fb-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("plans")).unwrap();
        std::fs::write(dir.join("plans/2026-W29-family-plan.md"), W29).unwrap();

        let result = run_fast_lane(&dir, "swap Friday to tacos and rebalance the week", today());
        assert!(matches!(result, FastLaneResult::Fallback { .. }));
        // The plan on disk is untouched by a fallback.
        let after = std::fs::read_to_string(dir.join("plans/2026-W29-family-plan.md")).unwrap();
        assert!(after.contains("beef & vegetable stir-fry"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fast_lane_run_falls_back_with_no_plan_dir() {
        let dir = std::env::temp_dir().join(format!("fastlane-noplan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = run_fast_lane(&dir, "swap Friday to tacos", today());
        assert!(matches!(result, FastLaneResult::Fallback { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fast_lane_reminder_without_calendar_owner_leaves_plan_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let plans = root.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan_path = plans.join("2026-W29-family-plan.md");
        std::fs::write(&plan_path, W29).unwrap();
        let before = std::fs::read(&plan_path).unwrap();

        let result = run_fast_lane_with_calendar_owner(
            root.path(),
            "remind me to defrost the chicken Friday at 5pm",
            today(),
            None,
        );
        assert!(
            matches!(result, FastLaneResult::Fallback { .. }),
            "a missing project owner must fall back, got {result:?}"
        );
        assert_eq!(
            std::fs::read(&plan_path).unwrap(),
            before,
            "a refused reminder must not mutate the plan"
        );
    }
}
