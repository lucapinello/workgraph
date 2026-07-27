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
    /// Take an item back OFF the shopping list: "remove AA batteries again",
    /// "take dishwasher tablets back off", "no baking soda needed after all".
    ///
    /// The live-cert P1 (task engine-shopping-language): adds persisted while every
    /// removal phrasing only COMPOSED a reply — the list could be written to but never
    /// un-written, which is exactly the asymmetry a family notices. Crossing an item
    /// off (= bought) is deliberately NOT this op: that row stays, struck through.
    ShoppingRemove { item: String },
    /// Set a reminder: "remind me to defrost the chicken Friday at 5pm".
    ReminderSet {
        text: String,
        /// The weekday the family NAMED, when they named one. Copy only — the
        /// row is written for `date`.
        day: Option<Weekday>,
        /// The resolved calendar date the reminder fires on. A bare weekday
        /// always resolves FORWARD (the next occurrence on or after today), so a
        /// reminder is never filed on a date that has already elapsed
        /// (date-reminder-fail: "Monday" once meant the week's stale Jul 20).
        date: NaiveDate,
        time: Option<String>,
    },
    /// START THIS WEEK: create the plan of record the calendar-current week does
    /// not have yet, carrying the requests quoted in the ask (task
    /// `week-start-engine`). This is the only op that CREATES a plan file rather
    /// than editing one, and the only one whose failure mode is a week drafted
    /// without the thing the family asked for — so it is applied by
    /// [`super::week_start::draft_week`], which fails closed.
    WeekStart {
        /// The family's own words, quoted into the dispatched ask.
        carried: Vec<String>,
    },
    /// Cancel a pending reminder: "cancel the reminder about the dentist".
    /// NEVER a creation — a cancel phrase that matches nothing (or matches more
    /// than one pending reminder) falls back so the family is ASKED.
    ReminderCancel {
        /// Fuzzy title fragment ("dentist"); empty when only a day was named.
        target: String,
        /// The weekday named, if any — matched against a pending row's own day.
        day: Option<Weekday>,
        /// Today, at classification time. A cancel never deletes an already
        /// elapsed row; only pending reminders can be cancelled.
        not_before: NaiveDate,
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
            FastLaneOp::ShoppingRemove { .. } => "shopping-remove",
            FastLaneOp::WeekStart { .. } => "week-start",
            FastLaneOp::ReminderSet { .. } => "reminder-set",
            FastLaneOp::ReminderCancel { .. } => "reminder-cancel",
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

/// Why a turn is ANSWERED WITH A QUESTION instead of being applied — the safety lanes
/// the live-cert P1 demanded (task engine-shopping-language). Every one of these
/// deliberately writes NOTHING.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskReason {
    /// The named item is not a thing you can buy ("add glorptwax to shopping") — the
    /// plausibility check does not recognize it, so we ask rather than write junk onto
    /// the family's list and confirm it as understood.
    UnknownItem,
    /// The ask is real but explicitly HELD ("don't add it yet — ask me first"). The
    /// write waits for a yes.
    HeldAsk,
    /// A mutation that named no item ("remove it", "no more needed") — we never guess
    /// which row the family meant.
    WhichItem,
}

impl AskReason {
    /// A stable, PII-free label for logs and the JSON seam.
    pub fn slug(self) -> &'static str {
        match self {
            AskReason::UnknownItem => "unknown-item",
            AskReason::HeldAsk => "held-ask",
            AskReason::WhichItem => "which-item",
        }
    }
}

/// The outcome of classifying a chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    FastLane(FastLaneOp),
    /// A closed-set ask the lane OWNS but refuses to apply: `reply` is the question to
    /// send the family, and no plan file is touched. This exists because falling back to
    /// the composer for these shapes is how "Done — glorptwax on the shopping list 🛒"
    /// happened: the model answers, sounding certain, and either writes junk or claims a
    /// write that never occurred.
    Ask { reply: String, reason: AskReason },
    Fallback(FallbackReason),
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Verbs that introduce an *edit* clause — used both to match an operation and,
/// crucially, to detect a compound ask (two edit clauses joined by "and"/"then").
const EDIT_VERBS: &[&str] = &[
    "swap", "change", "switch", "replace", "make", "add", "remove", "drop", "delete", "cancel",
    "skip", "remind", "reminder", "put", "buy", "ditch",
];

/// Phrases that mark an ask as too broad for the fast lane even when it opens
/// with a clean simple edit. "swap Friday to tacos **and rebalance the week**".
const COMPLEX_MARKERS: &[&str] = &[
    "rebalance",
    "re-balance",
    "rebalanc",
    "redo the",
    "re-do the",
    "replan",
    "re-plan",
    "rework",
    "reorganiz",
    "reorganis",
    "rethink",
    "overhaul",
    "shuffle the",
    "sort out the week",
    "plan the whole",
    "plan the week",
    "review the week",
    "optimi",
    "rest of the week",
    "whole week",
    "everything else",
    "the entire week",
    "around the",
    "work around",
];

/// Classify a chat turn against the closed set. Pure; `today` anchors relative
/// day words ("today", "tomorrow", "tonight").
pub fn classify(message: &str, today: NaiveDate) -> Classification {
    let text = message.trim();
    if text.is_empty() {
        return Classification::Fallback(FallbackReason::NotASimpleEdit);
    }

    // START THIS WEEK, FIRST (task `week-start-engine`). This must be tested
    // BEFORE every other shape, for two reasons. The dispatched ask carries the
    // family's original request QUOTED inside it — "…start the week. Keep what I
    // asked for: \"Set Tuesday's dinner to homemade pizza.\"" — so the ordinary
    // meal matcher would happily read that quote as a bare swap and apply it to
    // the NEWEST plan on disk, which on this exact Monday is the week that has
    // already ended: the archived-week write the offer exists to prevent. And
    // "plan the week" sits in COMPLEX_MARKERS, so the ask would otherwise fall
    // through to the composer, which cannot create a plan file at all.
    if let Some(ask) = super::week_start::detect(text) {
        return Classification::FastLane(FastLaneOp::WeekStart {
            carried: ask.carried,
        });
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

    // "Remind me WHAT was in Monday's risotto" is a memory/READ ask that merely
    // opens with the reminder verb. It is answered, never written
    // (date-reminder-fail (a): it once filed a reminder titled "was in risotto").
    if is_reminder_read(&s) {
        return Classification::Fallback(FallbackReason::NotASimpleEdit);
    }

    // A reminder ask that points at an ELAPSED day ("last Monday", "yesterday")
    // cannot be honoured by scheduling anything; writing a past-dated row is the
    // stale-date bug. Ask instead of writing.
    if is_reminder_ask(&s) && names_past_day(&s) {
        return Classification::Fallback(FallbackReason::NotASimpleEdit);
    }

    // SHOPPING MUTATION LANGUAGE (task engine-shopping-language). Removals, negations
    // and implausible items are decided here, ahead of the meal ops which share the
    // "add"/"remove" verbs. Reminders still win — they are checked first, exactly as
    // before, so "remind me to add milk" is a reminder, not a list write.
    if !is_reminder_ask(&s) {
        if let Some(verdict) = shopping_turn(&s, today) {
            return verdict;
        }
    }

    match match_single_op(&s, today) {
        Some(op) => Classification::FastLane(op),
        None => Classification::Fallback(FallbackReason::NotASimpleEdit),
    }
}

/// The shopping-language decision for one turn, or `None` when the turn is not about
/// the list at all (the caller then tries the meal ops, exactly as before).
///
/// This is the ENGINE half of the live-cert P1 "shopping mutation language is not safe
/// enough" and it mirrors the gateway lanes (`gatewayCore._shoppingSafetyLanes`) rule
/// for rule, over the shared vocabulary in
/// [`crate::notify::shopping_language`]:
///
/// * a HOLD ("don't add it yet — ask me first") ASKS and writes nothing;
/// * a CANCEL ("no baking soda needed after all") takes a matching item back off;
/// * a removal phrasing ("remove AA batteries again") really removes;
/// * an implausible item ("glorptwax") is ASKED about, never written;
/// * one SENTENCE yields ONE item ("we are out of olive oil—add olive oil").
///
/// Scope rules that keep it out of the meal ops' way:
/// * when the turn NAMES the list, shopping owns it;
/// * when it does not, the item must be a plausible good AND not a plan-or-list
///   ambiguous dish word, and a turn that names a weekday/meal slot is left alone.
fn shopping_turn(s: &str, today: NaiveDate) -> Option<Classification> {
    use crate::notify::shopping_language as lang;

    let list_scoped = lang::names_the_list(s);
    let day_scoped = find_weekday(s).is_some() || relative_day(s, today).is_some();

    // An UNSCOPED turn may only be a list mutation when the item is unmistakably a
    // purchase: a plausible good, not a dish word, no day named, and no trailing clause
    // that makes the sentence an ACTION rather than a row ("put the chicken in the
    // oven", "grab a bottle of wine on the way home" — both wrote junk rows before that
    // last guard existed).
    let unscoped_ok = |item: &str| -> bool {
        !item.is_empty()
            && !day_scoped
            && lang::plausible_grocery(item)
            && !lang::dish_ambiguous(item)
            && !lang::carries_trailing_clause(item)
    };

    // ── negation first: a negated ask must never reach a write ─────────────
    if let Some(neg) = lang::detect_negation(s) {
        match neg.kind {
            lang::NegationKind::Hold => {
                let item = lang::extract_item(s).map(|(i, _)| i).unwrap_or_default();
                if !list_scoped && !lang::names_supplies(s) {
                    return None;
                }
                if !item.is_empty() && !list_scoped && !unscoped_ok(&item) {
                    return None;
                }
                let reply = if item.is_empty() {
                    "Holding off — tell me when you want it on the shopping list. 🛒".to_string()
                } else {
                    format!(
                        "Holding off on {item} — say the word and it goes on the shopping list. 🛒"
                    )
                };
                return Some(Classification::Ask {
                    reply,
                    reason: AskReason::HeldAsk,
                });
            }
            lang::NegationKind::Cancel => {
                let item = if neg.item.is_empty() {
                    lang::extract_item(s).map(|(i, _)| i).unwrap_or_default()
                } else {
                    neg.item.clone()
                };
                if item.is_empty() {
                    if list_scoped {
                        return Some(Classification::Ask {
                            reply: "Which item should come off the shopping list?".to_string(),
                            reason: AskReason::WhichItem,
                        });
                    }
                    return None;
                }
                if !list_scoped && !unscoped_ok(&item) {
                    return None;
                }
                return Some(Classification::FastLane(FastLaneOp::ShoppingRemove {
                    item,
                }));
            }
        }
    }

    // ── an explicit removal phrasing ──────────────────────────────────────
    if let Some(rem) = lang::detect_remove_intent(s) {
        if rem.pronoun {
            if list_scoped {
                return Some(Classification::Ask {
                    reply: "Which item should come off the shopping list?".to_string(),
                    reason: AskReason::WhichItem,
                });
            }
            return None;
        }
        if list_scoped && !lang::plausible_grocery(&rem.item) {
            return Some(Classification::Ask {
                reply: format!(
                    "I don't see anything like \"{}\" — which item should come off the shopping list?",
                    rem.item
                ),
                reason: AskReason::WhichItem,
            });
        }
        if list_scoped || unscoped_ok(&rem.item) {
            return Some(Classification::FastLane(FastLaneOp::ShoppingRemove {
                item: rem.item,
            }));
        }
        return None;
    }

    // ── an add ────────────────────────────────────────────────────────────
    // A question is answered, not applied ("should we add olive oil?").
    if s.contains('?') {
        return None;
    }
    let (item, unknown) = lang::extract_item(s)?;
    if unknown {
        // Plausibility, not certainty: a nonsense word never lands silently. Only an
        // explicitly list-scoped ask is questioned — an unscoped sentence ("we're out
        // of ideas") stays with the composer rather than earning an absurd question.
        if !list_scoped {
            return None;
        }
        return Some(Classification::Ask {
            reply: format!(
                "I don't know what \"{item}\" is — want it on the shopping list exactly like that?"
            ),
            reason: AskReason::UnknownItem,
        });
    }
    if list_scoped || unscoped_ok(&item) {
        return Some(Classification::FastLane(FastLaneOp::ShoppingAdd { item }));
    }
    None
}

/// True for an interrogative that should be answered, not applied.
fn is_query(s: &str) -> bool {
    const OPENERS: &[&str] = &[
        "what",
        "whats",
        "when",
        "where",
        "who",
        "why",
        "which",
        "whose",
        "how ",
        "is ",
        "are ",
        "was ",
        "were ",
        "do we",
        "does ",
        "did ",
        "should we",
        "should i",
        "any ideas",
        "can we still",
    ];
    OPENERS.iter().any(|o| s.starts_with(o))
}

/// Words that make whatever follows `remind me` a QUESTION rather than a thing
/// to be reminded of. "remind me what was in Monday's risotto" asks the family
/// memory; "remind me when the dentist is" asks the calendar. Both are answered,
/// never filed — and an ambiguous one ("remind me when to leave") is likewise
/// left to the composer, which can ask, rather than written blind.
const REMIND_INTERROGATIVES: &[&str] = &[
    "what", "whats", "what's", "how", "when", "where", "who", "whom", "whose", "which", "why",
    "whether", "if", "was", "were", "did", "does", "is", "are", "do",
];

/// Fillers that may sit between `remind me` and the interrogative
/// ("remind me again what …").
const REMIND_FILLERS: &[&str] = &["again", "please", "quickly", "quick", "once", "briefly"];

/// True when the turn mentions reminders at all.
fn is_reminder_ask(s: &str) -> bool {
    contains_word(s, "remind")
        || contains_word(s, "reminder")
        || contains_word(s, "reminders")
        || contains_word(s, "reminding")
}

/// True when the text is `remind me/us <interrogative> …` — an interrogative
/// after the reminder verb makes the turn a READ, never a write.
fn is_reminder_read(s: &str) -> bool {
    const OPENERS: &[&str] = &["remind me ", "remind us ", "reminder "];
    for opener in OPENERS {
        let Some(idx) = s.find(opener) else { continue };
        let rest = &s[idx + opener.len()..];
        let mut words = rest.split_whitespace().skip_while(|w| {
            REMIND_FILLERS.contains(&w.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        });
        let Some(first) = words.next() else { continue };
        let first = first
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
            .to_ascii_lowercase();
        if REMIND_INTERROGATIVES.contains(&first.as_str()) {
            return true;
        }
    }
    false
}

/// True when the text explicitly points BACKWARD in time ("last Monday",
/// "yesterday", "last night", "this past Friday").
fn names_past_day(s: &str) -> bool {
    if contains_word(s, "yesterday") || s.contains("last night") || s.contains("last week") {
        return true;
    }
    for lead in ["last ", "this past ", "past "] {
        let mut start = 0;
        while let Some(pos) = s[start..].find(lead) {
            let i = start + pos + lead.len();
            let next = s[i..].split_whitespace().next().unwrap_or("");
            if is_day_word(next) {
                return true;
            }
            start = i;
        }
    }
    false
}

/// Verbs that CANCEL something rather than create it. Matched as substrings so
/// multi-word forms ("get rid of", "stop reminding") are caught.
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

/// Lowercase, collapse whitespace, and drop a trailing courtesy so extraction
/// sees a tidy string. Dish/item casing is intentionally not preserved — a plan
/// line and the "Done — tacos Friday" report both read fine in lower case.
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
        segments = segments.into_iter().flat_map(|seg| seg.split(c)).collect();
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
    // Cancellation is checked BEFORE creation: "cancel the reminder about the
    // dentist" carries the reminder keyword and would otherwise be filed as a
    // brand-new reminder titled "dentist" (date-reminder-fail (c)).
    if is_reminder_ask(s) && names_cancel(s) {
        return match_reminder_cancel(s, today);
    }
    if let Some(op) = match_reminder(s, today) {
        return Some(op);
    }
    // NB shopping is NOT matched here any more: `classify` decides the whole shopping
    // lane (add / remove / ask) in `shopping_turn` before reaching this point, because a
    // safe answer is sometimes a QUESTION and this function can only return an op.
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
        "remind me to ",
        "remind me ",
        "remind us to ",
        "remind us ",
        "remind everyone to ",
        "set a reminder to ",
        "set a reminder ",
        "reminder to ",
        "reminder: ",
        "reminder ",
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
    // The date is resolved here, FORWARD from today — a named weekday means its
    // next occurrence, a dayless ask means today. The plan week never anchors it
    // (that is what filed "Monday" on the week's already-elapsed Monday).
    let date = day.map(|wd| upcoming_weekday(today, wd)).unwrap_or(today);
    Some(FastLaneOp::ReminderSet {
        text,
        day,
        date,
        time,
    })
}

/// True when the turn asks to CANCEL rather than create.
fn names_cancel(s: &str) -> bool {
    CANCEL_VERBS.iter().any(|v| s.contains(v))
}

/// Match a reminder CANCELLATION: "cancel the reminder about the dentist",
/// "delete my Monday reminder", "stop reminding me about the bins".
///
/// Returns `None` — a fallback, so the family is asked — when the ask names
/// neither a title fragment nor a day ("cancel my reminders" is too broad to act
/// on blind). It NEVER returns a creation.
fn match_reminder_cancel(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    let verb = CANCEL_VERBS
        .iter()
        .filter_map(|v| s.find(v).map(|i| (i, *v)))
        .min_by_key(|(i, _)| *i)?;
    let tail = &s[verb.0 + verb.1.len()..];
    let (day, tail) = pull_day(tail, today);
    let target = scrub_reminder_words(&tail);
    if target.is_empty() && day.is_none() {
        return None;
    }
    Some(FastLaneOp::ReminderCancel {
        target,
        day,
        not_before: today,
    })
}

/// Strip the reminder nouns and connectors off a cancel tail so what remains is
/// the title fragment to match on: "the reminder about the dentist" → "dentist".
fn scrub_reminder_words(frag: &str) -> String {
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
    ];
    frag.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\''))
        .filter(|w| !w.is_empty() && !NOISE.contains(&w.to_ascii_lowercase().as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn match_meal_remove(s: &str, today: NaiveDate) -> Option<FastLaneOp> {
    let verbs = [
        "remove ",
        "drop ",
        "cancel ",
        "skip ",
        "take off ",
        "get rid of ",
        "delete ",
        "ditch ",
    ];
    let (_, tail) = verbs
        .iter()
        .find_map(|v| s.split_once(v).map(|p| (v, p.1)))?;
    let day = find_weekday(s)
        .map(|(w, _)| w)
        .or_else(|| relative_day(s, today))?;
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
    let day = find_weekday(s)
        .map(|(w, _)| w)
        .or_else(|| relative_day(s, today))?;
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
    let day = find_weekday(s)
        .map(|(w, _)| w)
        .or_else(|| relative_day(s, today))?;
    let swap_verb = [
        "swap ", "change ", "switch ", "replace ", "make ", "cook ", "do ", "have ", "turn ",
    ]
    .iter()
    .any(|v| s.contains(v));
    // A bare " for " is too weak a signal (it appears in questions); require a
    // real swap verb or an explicit target separator.
    let has_target_prep =
        s.contains(" to ") || s.contains(" into ") || s.contains(':') || s.contains('=');
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
            &[
                "let's", "lets", "swap", "change", "switch", "replace", "make", "cook", "do",
                "have", "turn", "us",
            ],
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
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
        ("mon", Weekday::Mon),
        ("tue", Weekday::Tue),
        ("wed", Weekday::Wed),
        ("thu", Weekday::Thu),
        ("fri", Weekday::Fri),
        ("sat", Weekday::Sat),
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

/// The next occurrence of `wd` on or after `today` — a bare weekday always
/// points FORWARD. Today counts when it is that weekday ("remind me Monday",
/// said on a Monday morning, means today), so the resolution matches the
/// gateway's `resolveUpcomingWeekday` and can never land in the past.
fn upcoming_weekday(today: NaiveDate, wd: Weekday) -> NaiveDate {
    let delta = (wd.num_days_from_monday() as i64)
        - (today.weekday().num_days_from_monday() as i64);
    today + Duration::days(delta.rem_euclid(7))
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
    let day = find_weekday(frag)
        .map(|(w, _)| w)
        .or_else(|| relative_day(frag, today));
    let cleaned = scrub_day_phrases(frag);
    (day, cleaned)
}

/// True when a word (tolerating a trailing possessive/punctuation) names a day.
fn is_day_word(w: &str) -> bool {
    const DAYS: &[&str] = &[
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "mon",
        "tue",
        "wed",
        "thu",
        "fri",
        "sat",
        "sun",
        "today",
        "tonight",
        "tomorrow",
    ];
    let w = w
        .trim_end_matches(['.', ',', ':', '?', '!'])
        .trim_end_matches("'s");
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
    let mut out = s
        .trim()
        .trim_matches(|c: char| c == '.' || c == ',' || c == '!' || c == '?')
        .trim()
        .to_string();
    let leading = [
        "to ", "a ", "an ", "some ", "the ", "for ", "us ", "me ", "please ",
    ];
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
    let trailing = [
        " please",
        " thanks",
        " thank you",
        " tonight",
        " today",
        " this week",
    ];
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
    "hey there",
    "hey",
    "hi there",
    "hi",
    "hello",
    "ok",
    "okay",
    "yo",
    "so",
    "please",
    "pls",
    "kindly",
    "just",
    "maybe",
    "actually",
    "can you",
    "could you",
    "would you",
    "can we",
    "could we",
    "will you",
    "i'd like",
    "id like",
    "i would like",
    "i want",
    "we want",
    "we'd like",
    "how about",
    "what about",
    "lets",
    "let's",
    "us to",
    "me to",
    "swap",
    "change",
    "switch",
    "replace",
    "make",
    "cook",
    "do",
    "have",
    "turn",
    "us",
    "it",
    "to",
    "into",
    "with",
    "for",
    "the",
    "a",
    "an",
    "some",
];

/// Trailing junk peeled off the end — dangling connectors and courtesy left over
/// once the day and verb are gone ("something nice **for**", "tacos **please**").
const TRAILING_ASK_JUNK: &[&str] = &[
    "for",
    "with",
    "to",
    "and",
    "or",
    "instead",
    "please",
    "thanks",
    "tonight",
    "today",
    "for dinner",
    "for the week",
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
    if looks_like_dish(&s) { Some(s) } else { None }
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
        "hey", "hi", "hello", "please", "pls", "thanks", "thx", "swap", "switch", "replace",
        "wanna", "gonna",
    ];
    if words.iter().any(|w| BANNED_WORDS.contains(&w.as_str())) {
        return false;
    }

    // Adjacent pairs that only appear in a request ("can you", "i want", …).
    const BANNED_BIGRAMS: &[(&str, &str)] = &[
        ("can", "you"),
        ("could", "you"),
        ("would", "you"),
        ("can", "we"),
        ("will", "you"),
        ("i", "want"),
        ("we", "want"),
        ("i'd", "like"),
        ("how", "about"),
        ("what", "about"),
        ("change", "to"),
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
        "something",
        "anything",
        "everything",
        "whatever",
        "some",
        "any",
        "nice",
        "good",
        "great",
        "tasty",
        "yummy",
        "nicer",
        "better",
        "different",
        "healthy",
        "light",
        "quick",
        "easy",
        "simple",
        "you",
        "like",
        "for",
        "dinner",
        "lunch",
        "supper",
        "meal",
        "food",
        "thing",
        "please",
        "else",
        "it",
        "them",
        "one",
        "that",
        "this",
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
            format!(
                "Done — added {addition} to {} {}",
                weekday_name(*day),
                dish_emoji(addition)
            )
        }
        FastLaneOp::MealRemove { day, target } => {
            format!("Done — dropped {target} from {} ✂️", weekday_name(*day))
        }
        FastLaneOp::ShoppingAdd { item } => {
            format!("Done — {item} on the shopping list 🛒")
        }
        FastLaneOp::ShoppingRemove { item } => {
            format!("Done — took {item} off the shopping list ✂️")
        }
        // The week-drafting path reports with the real dates and everything it
        // preserved ([`super::week_start::report_line`]); this is the shape-only
        // line for a caller that has the op but not the drafted week.
        FastLaneOp::WeekStart { .. } => "Done — this week's plan is started 🗓️".to_string(),
        FastLaneOp::ReminderSet {
            text, day, time, ..
        } => {
            let when = match (day, time) {
                (Some(d), Some(t)) => format!(" {} at {}", weekday_name(*d), t),
                (Some(d), None) => format!(" {}", weekday_name(*d)),
                (None, Some(t)) => format!(" at {}", t),
                (None, None) => String::new(),
            };
            format!("Done — I'll remind you to {text}{when} ⏰")
        }
        FastLaneOp::ReminderCancel { target, day, .. } => {
            let what = match (target.is_empty(), day) {
                (false, _) => format!(" about {target}"),
                (true, Some(d)) => format!(" for {}", weekday_name(*d)),
                (true, None) => String::new(),
            };
            format!("Done — cancelled the reminder{what} ✓")
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

/// Does a plan's day-cell weekday WORD name `want`?
///
/// Households write both forms — `Mon 07-27` and `Monday July 27` — and both are
/// this table's own day column, so an edit must find its row either way. Matched
/// as a whole word against exactly the two spellings (never a prefix), so a first
/// cell reading "Monthly total" is not mistaken for Monday.
fn day_word_names(word: &str, want: Weekday) -> bool {
    word.eq_ignore_ascii_case(weekday_short(want)) || word.eq_ignore_ascii_case(weekday_name(want))
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
        FastLaneOp::ShoppingRemove { item } => {
            remove_shopping_item(content, item).ok_or_else(|| {
                FastLaneError::NotApplicable(format!("'{item}' is not on the shopping list"))
            })?
        }
        FastLaneOp::ReminderSet {
            text, date, time, ..
        } => {
            let owner = calendar_owner
                .map(str::trim)
                .filter(|owner| {
                    !owner.is_empty() && !owner.chars().any(|c| matches!(c, '|' | '\n' | '\r'))
                })
                .ok_or_else(|| {
                    FastLaneError::NotApplicable(
                        "no configured calendar owner for the reminder".into(),
                    )
                })?;
            add_reminder_row(content, text, *date, time.as_deref(), owner).ok_or_else(|| {
                FastLaneError::NotApplicable("no calendar to add a reminder to".into())
            })?
        }
        FastLaneOp::ReminderCancel {
            target,
            day,
            not_before,
        } => remove_reminder_row(week_code, content, target, *day, *not_before)?,
        // Creating a week is a FILE-level draft, not a content edit — it has no
        // document to transform. [`run_fast_lane_with_calendar_owner`] routes it
        // to [`super::week_start::draft_week`] before ever reaching here.
        FastLaneOp::WeekStart { .. } => {
            return Err(FastLaneError::NotApplicable(
                "starting a week creates a plan file; it is not a content edit".into(),
            ));
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
            .find(|m| day_word_names(&m.weekday, wd))
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
        FastLaneOp::ShoppingRemove { item } => {
            // The removed row must be GONE after the re-parse — matched exactly the way
            // the removal found it (fuzzy, so "batteries" clears "AA batteries ×4"),
            // never a bare substring that would call a miss a success.
            let still_there = doc
                .shopping
                .iter()
                .flat_map(|sec| sec.items.iter())
                .any(|it| crate::notify::shopping_language::same_item(it, item));
            if still_there {
                return Err(FastLaneError::RoundTrip(format!(
                    "'{item}' is still on the shopping list after re-parse"
                )));
            }
        }
        FastLaneOp::ReminderCancel {
            target,
            day,
            not_before,
        } => {
            // The cancelled row must be GONE after the re-parse — and exactly one
            // row was ever eligible, so a surviving match means the edit missed.
            let still_there = doc
                .calendar
                .iter()
                .any(|e| cancel_matches(&e.event, e.date, target, *day, *not_before));
            if still_there {
                return Err(FastLaneError::RoundTrip(
                    "the cancelled reminder is still in the calendar after re-parse".into(),
                ));
            }
        }
        FastLaneOp::ReminderSet { text, .. } => {
            let key: String = text
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
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
        // A drafted week is verified against the file that was WRITTEN, by
        // [`super::week_start::draft_week`] — including that every carried
        // request survived into it. There is no in-document edit to re-check
        // here; `apply_to_content` refuses this op before it can reach us.
        FastLaneOp::WeekStart { .. } => {}
    }
    Ok(())
}

/// Edit the dish cell of the first meal-table row whose day matches `day`.
/// Returns `None` when there is no such row (or nothing to remove).
fn edit_meal_dish(content: &str, day: Weekday, edit: &DishEdit) -> Option<String> {
    let mut in_meals = false;
    let mut out: Vec<String> = Vec::new();
    let mut applied = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            in_meals = family_plan::is_meals_section_heading(h2);
            out.push(line.to_string());
            continue;
        }
        if in_meals && !applied && trimmed.starts_with('|') {
            if let Some(cells) = split_cells(trimmed) {
                let day_cell = cells.first().map(|c| c.to_lowercase()).unwrap_or_default();
                let is_row = cells.len() >= 3
                    && day_cell
                        .split_whitespace()
                        .next()
                        .map(|w| day_word_names(w, day))
                        .unwrap_or(false);
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

/// Drop the bullet naming `item` from the shopping list. Returns `None` when no row
/// matches — the caller then answers honestly ("I don't see X on the list") instead of
/// claiming a removal that never happened.
///
/// Matching is the SAME fuzzy item match the gateway's remove path uses
/// ([`crate::notify::shopping_language::same_item`]), so a conversational "take the
/// batteries off" clears the plan's "AA batteries ×4 (Sat)" row. Only ONE row comes off
/// per ask — the first match — so a plural noun cannot quietly empty a section.
fn remove_shopping_item(content: &str, item: &str) -> Option<String> {
    use crate::notify::shopping_language::same_item;

    let mut in_shopping = false;
    let mut removed = false;
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            in_shopping = h2.to_lowercase().contains("shopping");
            out.push(line.to_string());
            continue;
        }
        if !removed && in_shopping {
            if let Some(bullet) = trimmed.strip_prefix("- ") {
                if same_item(bullet, item) {
                    removed = true;
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }
    if removed {
        Some(out.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
    } else {
        None
    }
}

/// Append a `⏰ Reminder` row to the calendar table for the already-resolved
/// `date` (the classifier resolves it forward from today, so this never files a
/// past day); the time defaults to 09:00. Returns `None` when there is no
/// calendar table.
fn add_reminder_row(
    content: &str,
    text: &str,
    target: NaiveDate,
    time: Option<&str>,
    owner: &str,
) -> Option<String> {
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

/// Remove the ONE pending `⏰ Reminder` row a cancel ask matches.
///
/// Fails closed: no match, or more than one, is `NotApplicable` so the turn
/// falls back and the family is asked which reminder they meant. An elapsed row
/// (before `not_before`) or a row whose date will not parse is never eligible —
/// a cancel only touches something still pending.
fn remove_reminder_row(
    week_code: &str,
    content: &str,
    target: &str,
    day: Option<Weekday>,
    not_before: NaiveDate,
) -> Result<String, FastLaneError> {
    let year = year_of_week_code(week_code);
    let mut in_cal = false;
    let mut hits: Vec<usize> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(h2) = trimmed.strip_prefix("## ") {
            in_cal = h2.to_lowercase().contains("calendar");
            continue;
        }
        if !in_cal || !trimmed.starts_with('|') {
            continue;
        }
        let Some(cells) = split_cells(trimmed) else {
            continue;
        };
        if cells.len() < 3 {
            continue;
        }
        let date = calendar_cell_date(&cells[0], year);
        if cancel_matches(&cells[2], date, target, day, not_before) {
            hits.push(i);
        }
    }
    match hits.len() {
        0 => Err(FastLaneError::NotApplicable(
            "no pending reminder matches that".into(),
        )),
        1 => {
            let drop = hits[0];
            let kept: Vec<String> = lines
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != drop)
                .map(|(_, l)| (*l).to_string())
                .collect();
            Ok(kept.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
        }
        _ => Err(FastLaneError::NotApplicable(
            "more than one pending reminder matches that".into(),
        )),
    }
}

/// Does this calendar row name the pending reminder a cancel ask points at?
///
/// A row qualifies only when it IS a reminder, is still pending, and every
/// significant word of the ask's title fragment appears in it. With no fragment
/// the named weekday alone selects it — which is why a dayless, targetless
/// cancel never reaches here (the classifier falls back instead).
fn cancel_matches(
    event: &str,
    date: Option<NaiveDate>,
    target: &str,
    day: Option<Weekday>,
    not_before: NaiveDate,
) -> bool {
    if !super::reminder::is_reminder_event(event) {
        return false;
    }
    let Some(date) = date else {
        return false; // unknown date — cannot prove it is pending, so never touch it
    };
    if date < not_before {
        return false;
    }
    if let Some(wd) = day {
        if date.weekday() != wd {
            return false;
        }
    }
    let ev = event.to_lowercase();
    let mut words = target
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '\''))
        .filter(|w| w.len() > 2)
        .peekable();
    if words.peek().is_none() {
        // No usable fragment — the weekday (checked above) is the whole selector.
        return day.is_some();
    }
    words.all(|w| ev.contains(&w.to_ascii_lowercase()))
}

/// The 4-digit year of a week code like `2026-W29`.
fn year_of_week_code(week_code: &str) -> Option<i32> {
    week_code.split('-').next()?.parse().ok()
}

/// The date of a calendar day cell (`"Mon 07-20"`) against a plan year.
fn calendar_cell_date(cell: &str, year: Option<i32>) -> Option<NaiveDate> {
    let year = year?;
    let md = cell.split_whitespace().nth(1)?;
    let (m, d) = md.split_once('-')?;
    NaiveDate::from_ymd_opt(year, m.trim().parse().ok()?, d.trim().parse().ok()?)
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
    /// The lane OWNED the turn and deliberately wrote NOTHING: an implausible item, a
    /// held ask, or a removal that matched no row. `reply` is the honest question/line to
    /// send; the plan file was not touched. The caller must NOT also run the composer —
    /// that is precisely how a refusal turned back into a confident fabrication.
    Answered { reply: String, lane: String },
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
            candidates.push((
                path,
                week_code.clone(),
                PlanDoc::parse(&week_code, &content),
            ));
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
        // A safety lane owns the turn and writes nothing (task engine-shopping-language).
        Classification::Ask { reply, reason } => {
            return FastLaneResult::Answered {
                reply,
                lane: reason.slug().to_string(),
            };
        }
        Classification::Fallback(reason) => {
            return FastLaneResult::Fallback {
                reason: format!("{reason:?}"),
            };
        }
    };

    // START THIS WEEK (task `week-start-engine`). The one op that CREATES a plan
    // of record rather than editing one, so it runs before the current-plan
    // lookup below — which on this exact Monday would hand back the week that has
    // already ended. `draft_week` assembles, edits and verifies the document in
    // memory and writes only when every carried request has landed, so a week is
    // never drafted without the thing the family asked for.
    if let FastLaneOp::WeekStart { carried } = &op {
        use super::week_start::{self, WeekStartError};
        let ask = week_start::WeekStartAsk {
            carried: carried.clone(),
        };
        return match week_start::draft_week(root, today, &ask, calendar_owner) {
            Ok(drafted) => FastLaneResult::Applied {
                report: week_start::report_line(&drafted),
                week_code: drafted.week_code.clone(),
                op,
            },
            // Already set up: answer honestly and write nothing. Never an
            // overwrite — a second "yes", or a dispatcher refire, must not erase
            // a week the family has already filled in.
            Err(WeekStartError::AlreadyPlanned { .. }) => FastLaneResult::Answered {
                reply: week_start::already_planned_line(),
                lane: "week-already-started".to_string(),
            },
            // The request the family carried could not be put into the new week.
            // Nothing was written; the heavy planning pipeline takes the turn, so
            // the ask is answered by something that CAN honour it — rather than a
            // week landing on disk with the request silently dropped.
            Err(e) => FastLaneResult::Fallback {
                reason: format!("week-start not applied ({e}) — deferring to full pipeline"),
            },
        };
    }

    // A cancel may match a reminder the DM path filed in the ad-hoc store rather
    // than a plan row, so it clears both surfaces.
    let adhoc_cleared = match &op {
        FastLaneOp::ReminderCancel {
            target,
            day,
            not_before,
        } => match cancel_adhoc_reminders(root, target, *day, *not_before) {
            Ok(n) => n,
            Err(reason) => return FastLaneResult::Fallback { reason },
        },
        _ => 0,
    };

    // A reminder is filed on its RESOLVED date, which can fall in the next plan
    // week ("remind me Monday", said on a Sunday). Write it to the plan that
    // actually covers that date when we have one; otherwise the current plan
    // still carries the correct, never-past day cell.
    let located = match &op {
        FastLaneOp::ReminderSet { date, .. } => {
            plan_file_covering(root, *date).or_else(|| current_plan_file(root, today))
        }
        _ => current_plan_file(root, today),
    };
    let (path, week_code, _doc) = match located {
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

    match apply_to_content_with_calendar_owner(&week_code, &content, &op, calendar_owner) {
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
        // A cancel that found nothing in the plan still succeeded when it cleared
        // the ad-hoc reminder the family meant.
        Err(_) if adhoc_cleared > 0 => FastLaneResult::Applied {
            report: report_line(&op),
            op,
            week_code,
        },
        // A removal that matched no row is ANSWERED honestly, not handed to the
        // composer: "took it off" for a row that was never there is the same lie in the
        // other direction (task engine-shopping-language).
        Err(FastLaneError::NotApplicable(_)) => match &op {
            FastLaneOp::ShoppingRemove { item } => FastLaneResult::Answered {
                reply: format!(
                    "I don't see {item} on the shopping list — nothing to take off. Want me to add it instead?"
                ),
                lane: "nothing-to-remove".to_string(),
            },
            _ => FastLaneResult::Fallback {
                reason: "direct edit not applicable — deferring to full pipeline".to_string(),
            },
        },
        Err(e) => FastLaneResult::Fallback {
            reason: format!("direct edit refused ({e}) — deferring to full pipeline"),
        },
    }
}

/// Clear the ad-hoc reminders a cancel ask matches, returning how many went.
///
/// `Err(reason)` means AMBIGUOUS — more than one pending ad-hoc reminder matched
/// — and nothing was removed: the caller falls back so the family is asked which
/// one they meant. A cancel never touches an already-elapsed reminder.
fn cancel_adhoc_reminders(
    root: &Path,
    target: &str,
    day: Option<Weekday>,
    not_before: NaiveDate,
) -> Result<usize, String> {
    use super::reminder::{AdHocStore, CancelRequest};
    let path = AdHocStore::path(root);
    let mut store = AdHocStore::load(&path);
    let req = CancelRequest {
        target: target.to_string(),
        day,
    };
    // Pending as of the START of today, so a reminder due later today is still
    // cancellable — the same day-granular rule the plan rows use.
    let since = not_before.and_hms_opt(0, 0, 0).unwrap_or_default();
    match store.cancel(&req, since) {
        Ok(None) => Ok(0),
        Ok(Some(_)) => match store.save(&path) {
            Ok(()) => Ok(1),
            Err(e) => Err(format!("could not update the ad-hoc reminders: {e}")),
        },
        Err(_) => Err("more than one pending reminder matches that — asking instead".into()),
    }
}

/// The plan file whose week covers `date`, when one exists on disk.
fn plan_file_covering(root: &Path, date: NaiveDate) -> Option<(PathBuf, String, PlanDoc)> {
    let found = current_plan_file(root, date)?;
    found.2.covers(date).then_some(found)
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
    let id =
        crate::notify::lifecycle::derive_task_id(&title, |cand| graph.get_node(cand).is_some());
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

    fn with_meals_heading(replacement: &str) -> String {
        W29.lines()
            .map(|line| {
                if line
                    .strip_prefix("## ")
                    .map(family_plan::is_meals_section_heading)
                    .unwrap_or(false)
                {
                    replacement
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- classification: each fast-lane op -------------------------------

    #[test]
    fn fast_lane_classifies_meal_swap() {
        assert_eq!(
            fast("swap Friday to tacos"),
            FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "tacos".into()
            }
        );
        assert_eq!(
            fast("change Friday's dinner to homemade pizza"),
            FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "homemade pizza".into()
            }
        );
        assert_eq!(
            fast("make Friday tacos"),
            FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "tacos".into()
            }
        );
    }

    #[test]
    fn fast_lane_classifies_meal_add() {
        assert_eq!(
            fast("add a dessert on Tuesday"),
            FastLaneOp::MealAdd {
                day: Weekday::Tue,
                addition: "dessert".into()
            }
        );
        assert_eq!(
            fast("add a side salad to Monday"),
            FastLaneOp::MealAdd {
                day: Weekday::Mon,
                addition: "side salad".into()
            }
        );
    }

    #[test]
    fn fast_lane_classifies_meal_remove() {
        assert_eq!(
            fast("drop the side salad on Monday"),
            FastLaneOp::MealRemove {
                day: Weekday::Mon,
                target: "side salad".into()
            }
        );
        assert_eq!(
            fast("remove Friday's dessert"),
            FastLaneOp::MealRemove {
                day: Weekday::Fri,
                target: "dessert".into()
            }
        );
    }

    #[test]
    fn fast_lane_classifies_shopping_add() {
        assert_eq!(
            fast("add milk to the shopping list"),
            FastLaneOp::ShoppingAdd {
                item: "milk".into()
            }
        );
        assert_eq!(
            fast("put eggs on the list"),
            FastLaneOp::ShoppingAdd {
                item: "eggs".into()
            }
        );
        assert_eq!(
            fast("we need bananas on the shopping list"),
            FastLaneOp::ShoppingAdd {
                item: "bananas".into()
            }
        );
    }

    #[test]
    fn fast_lane_classifies_reminder_set() {
        assert_eq!(
            fast("remind me to defrost the chicken Friday at 5pm"),
            FastLaneOp::ReminderSet {
                text: "defrost the chicken".into(),
                day: Some(Weekday::Fri),
                // today() is Tue 2026-07-14 → the UPCOMING Friday.
                date: NaiveDate::from_ymd_opt(2026, 7, 17).unwrap(),
                time: Some("17:00".into()),
            }
        );
        assert_eq!(
            fast("remind me to call the plumber"),
            FastLaneOp::ReminderSet {
                text: "call the plumber".into(),
                day: None,
                date: today(),
                time: None
            }
        );
    }

    // ---- date-reminder-fail: the three live-cert P0 preflight phrases ------

    #[test]
    fn repro_a_remind_me_what_is_a_read_never_a_write() {
        // (a) "Remind me what was in Monday's risotto" is a MEMORY/READ ask. It
        // must never classify as a write.
        assert_eq!(
            fallback("Remind me what was in Monday's risotto"),
            FallbackReason::NotASimpleEdit
        );
    }

    #[test]
    fn repro_b_bare_weekday_resolves_forward_never_stale() {
        // (b) On Tue 2026-07-14 (inside W29 = Jul 13–19), "Monday" must mean the
        // UPCOMING Monday (Jul 20), never the week's already-elapsed Jul 13.
        let op = fast("remind me Monday to take the bins out");
        let edited = apply_to_content_with_calendar_owner("2026-W29", W29, &op, Some("otto"))
            .expect("reminder row");
        assert!(
            edited.contains("Mon 07-20"),
            "reminder landed on a stale past Monday:\n{edited}"
        );
        assert!(
            !edited.contains("Mon 07-13 | 09:00 | ⏰ Reminder"),
            "reminder wrote a PAST date:\n{edited}"
        );
    }

    #[test]
    fn repro_c_cancel_phrase_never_creates_another_reminder() {
        // (c) A cancel phrase must never be classified as a reminder CREATION.
        match classify("cancel the reminder about the dentist", today()) {
            Classification::FastLane(FastLaneOp::ReminderSet { .. }) => {
                panic!("a cancel phrase created another reminder")
            }
            _ => {}
        }
    }

    #[test]
    fn reminder_read_covers_every_interrogative_after_remind_me() {
        // Each of these ASKS something; none of them files anything.
        for msg in [
            "Remind me what was in Monday's risotto",
            "remind me what we said about the school run",
            "remind me how the oven timer works",
            "remind me when the dentist is",
            "remind me again what Tuesday's dinner was",
            "can you remind me who is picking up the kids friday",
            "remind me why we moved swimming",
        ] {
            assert_eq!(
                classify(msg, today()),
                Classification::Fallback(FallbackReason::NotASimpleEdit),
                "{msg:?} should be answered, never written"
            );
        }
        // …while a real reminder request still fast-lanes.
        assert!(matches!(
            classify("remind me Friday to defrost the chicken", today()),
            Classification::FastLane(FastLaneOp::ReminderSet { .. })
        ));
    }

    #[test]
    fn reminder_forward_resolution_crosses_the_week_boundary() {
        // Sat 2026-07-18: "Monday" is the NEXT week's Monday (Jul 20), and a
        // dayless ask lands on today — never the plan week's Monday (Jul 13).
        let sat = NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let mon = match classify("remind me Monday to take the bins out", sat) {
            Classification::FastLane(op) => op,
            other => panic!("expected a reminder, got {other:?}"),
        };
        assert_eq!(
            mon,
            FastLaneOp::ReminderSet {
                text: "take the bins out".into(),
                day: Some(Weekday::Mon),
                date: NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
                time: None,
            }
        );
        // Said ON a Monday, "Monday" means today (the gateway resolver's rule).
        let on_monday = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        assert!(matches!(
            classify("remind me Monday at 6pm to take the bins out", on_monday),
            Classification::FastLane(FastLaneOp::ReminderSet { date, .. }) if date == on_monday
        ));
    }

    #[test]
    fn reminder_pointing_at_an_elapsed_day_asks_instead_of_writing() {
        for msg in [
            "remind me last Monday to take the bins out",
            "set a reminder for yesterday to call the plumber",
        ] {
            assert_eq!(
                classify(msg, today()),
                Classification::Fallback(FallbackReason::NotASimpleEdit),
                "{msg:?} must not file a past-dated reminder"
            );
        }
    }

    // A W29 plan carrying two pending reminder rows (Thu/Fri) plus one already
    // elapsed (Mon), so cancellation can be tested against real rows.
    fn w29_with_reminders() -> String {
        let owner = "harbor";
        let base = apply_to_content_with_calendar_owner(
            "2026-W29",
            W29,
            &FastLaneOp::ReminderSet {
                text: "book the dentist".into(),
                day: Some(Weekday::Thu),
                date: NaiveDate::from_ymd_opt(2026, 7, 16).unwrap(),
                time: Some("09:00".into()),
            },
            Some(owner),
        )
        .expect("dentist row");
        let base = apply_to_content_with_calendar_owner(
            "2026-W29",
            &base,
            &FastLaneOp::ReminderSet {
                text: "defrost the trout".into(),
                day: Some(Weekday::Fri),
                date: NaiveDate::from_ymd_opt(2026, 7, 17).unwrap(),
                time: Some("17:00".into()),
            },
            Some(owner),
        )
        .expect("trout row");
        apply_to_content_with_calendar_owner(
            "2026-W29",
            &base,
            &FastLaneOp::ReminderSet {
                text: "pay the deposit".into(),
                day: Some(Weekday::Mon),
                date: NaiveDate::from_ymd_opt(2026, 7, 13).unwrap(),
                time: Some("09:00".into()),
            },
            Some(owner),
        )
        .expect("elapsed row")
    }

    #[test]
    fn reminder_cancel_removes_the_matching_pending_row() {
        let plan = w29_with_reminders();
        let op = fast("cancel the reminder about the dentist");
        assert_eq!(
            op,
            FastLaneOp::ReminderCancel {
                target: "dentist".into(),
                day: None,
                not_before: today(),
            }
        );
        let edited = apply_to_content_with_calendar_owner("2026-W29", &plan, &op, Some("harbor"))
            .expect("cancel applies");
        assert!(
            !edited.to_lowercase().contains("book the dentist"),
            "the dentist reminder survived the cancel:\n{edited}"
        );
        // Only that one went.
        assert!(edited.contains("defrost the trout"));
        assert!(edited.contains("pay the deposit"));
        assert_eq!(
            report_line(&op),
            "Done — cancelled the reminder about dentist ✓"
        );
    }

    #[test]
    fn reminder_cancel_by_weekday_only_picks_the_pending_day() {
        let plan = w29_with_reminders();
        let op = fast("delete my Friday reminder");
        let edited = apply_to_content_with_calendar_owner("2026-W29", &plan, &op, Some("harbor"))
            .expect("cancel applies");
        assert!(!edited.contains("defrost the trout"));
        assert!(edited.contains("book the dentist"));
    }

    #[test]
    fn reminder_cancel_never_touches_an_elapsed_reminder() {
        // Monday 07-13 is behind today() (Tue 07-14) — a cancel cannot reach it,
        // so the ask falls back and the family is asked rather than a stale row
        // being silently deleted.
        let plan = w29_with_reminders();
        let op = fast("cancel the reminder about the deposit");
        assert!(matches!(
            apply_to_content_with_calendar_owner("2026-W29", &plan, &op, Some("harbor")),
            Err(FastLaneError::NotApplicable(_))
        ));
    }

    #[test]
    fn ambiguous_cancel_asks_instead_of_deleting() {
        // Two pending reminders, no fragment that separates them → refuse.
        let plan = w29_with_reminders();
        // A cancel that names neither a title nor a day is not a fast-lane op at
        // all — the composer asks which reminder they meant.
        for vague in ["cancel the reminder", "cancel my reminders"] {
            assert_eq!(
                classify(vague, today()),
                Classification::Fallback(FallbackReason::NotASimpleEdit),
                "{vague:?} must ask, not guess"
            );
        }
        // A fragment that matches BOTH pending rows is refused at apply time.
        let both = FastLaneOp::ReminderCancel {
            target: "the".into(), // too short to be significant → day-only selector
            day: None,
            not_before: today(),
        };
        assert!(matches!(
            apply_to_content_with_calendar_owner("2026-W29", &plan, &both, Some("harbor")),
            Err(FastLaneError::NotApplicable(_))
        ));
    }

    #[test]
    fn cancel_phrases_all_route_to_cancellation_never_creation() {
        for msg in [
            "cancel the reminder about the dentist",
            "delete the reminder to defrost the trout",
            "remove my Friday reminder",
            "stop reminding me about the dentist",
            "forget the reminder about the dentist",
        ] {
            match classify(msg, today()) {
                Classification::FastLane(FastLaneOp::ReminderCancel { .. }) => {}
                other => panic!("{msg:?} classified as {other:?}, expected a cancellation"),
            }
        }
    }

    // ---- fallback classification -----------------------------------------

    #[test]
    fn fast_lane_falls_back_on_open_ended_ask() {
        assert_eq!(
            fallback("what's for dinner on Friday?"),
            FallbackReason::NotASimpleEdit
        );
        assert_eq!(
            fallback("rebalance the week around Nadin's travel"),
            FallbackReason::Compound
        );
        assert_eq!(
            fallback("can you plan the whole week for me"),
            FallbackReason::Compound
        );
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
            FastLaneOp::MealSwap {
                day: Weekday::Mon,
                dish: "zuxxhini and tofu".into()
            }
        );
        // A polite request husk on a swap is peeled to the dish.
        assert_eq!(
            fast("can you please make friday tacos"),
            FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "tacos".into()
            }
        );
        // A meal add still resolves to a clean component.
        assert_eq!(
            fast("add pasta thursday"),
            FastLaneOp::MealAdd {
                day: Weekday::Thu,
                addition: "pasta".into()
            }
        );
    }

    #[test]
    fn fast_lane_vague_dish_falls_back_to_full_pipeline() {
        // "swap something nice for friday" carries no actual dish — the sanity
        // gate refuses it so the full pipeline can ask what "nice" means.
        assert_eq!(
            fallback("swap something nice for friday"),
            FallbackReason::NotASimpleEdit
        );
        assert_eq!(
            fallback("change monday to something healthy"),
            FallbackReason::NotASimpleEdit
        );
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
        assert_eq!(
            ask_to_dish("hey can you swap with tacos").as_deref(),
            Some("tacos")
        );
        assert_eq!(
            ask_to_dish("please make it homemade pizza").as_deref(),
            Some("homemade pizza")
        );
        assert_eq!(ask_to_dish("something nice for"), None);
        assert_eq!(ask_to_dish("can you swap it"), None);
    }

    #[test]
    fn fast_lane_shopping_compound_items_stay_single_op() {
        // "milk and eggs" is one shopping add, not a compound ask.
        assert_eq!(
            fast("add milk and eggs to the shopping list"),
            FastLaneOp::ShoppingAdd {
                item: "milk and eggs".into()
            }
        );
    }

    // ---- apply + round-trip against the real parser ----------------------

    #[test]
    fn fast_lane_apply_meal_swap_round_trips() {
        let op = FastLaneOp::MealSwap {
            day: Weekday::Fri,
            dish: "tacos".into(),
        };
        let edited = apply_to_content("2026-W29", W29, &op).expect("swap applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        let fri = doc.meals.iter().find(|m| m.weekday == "Fri").unwrap();
        assert_eq!(fri.dish, "tacos");
        // Untouched days survive.
        assert!(
            doc.meals
                .iter()
                .any(|m| m.weekday == "Mon" && m.dish.contains("curry"))
        );
        assert_eq!(doc.meals.len(), 7, "no rows lost");
    }

    #[test]
    fn fast_lane_apply_meal_swap_accepts_legacy_meals_heading() {
        let legacy = with_meals_heading("## Meals");
        let op = FastLaneOp::MealSwap {
            day: Weekday::Fri,
            dish: "tacos".into(),
        };
        let edited = apply_to_content("2026-W29", &legacy, &op).expect("legacy swap applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        assert_eq!(
            doc.meals.iter().find(|m| m.weekday == "Fri").unwrap().dish,
            "tacos"
        );
    }

    #[test]
    fn fast_lane_meal_edit_without_a_meals_heading_is_refused() {
        let no_heading = with_meals_heading("## Supper notes");
        let op = FastLaneOp::MealSwap {
            day: Weekday::Fri,
            dish: "tacos".into(),
        };
        assert_eq!(
            apply_to_content("2026-W29", &no_heading, &op),
            Err(FastLaneError::DayNotFound)
        );
    }

    #[test]
    fn fast_lane_apply_meal_add_round_trips() {
        let op = FastLaneOp::MealAdd {
            day: Weekday::Wed,
            addition: "garlic bread".into(),
        };
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
            &FastLaneOp::MealAdd {
                day: Weekday::Mon,
                addition: "side salad".into(),
            },
        )
        .unwrap();
        let removed = apply_to_content(
            "2026-W29",
            &added,
            &FastLaneOp::MealRemove {
                day: Weekday::Mon,
                target: "side salad".into(),
            },
        )
        .expect("remove applies");
        let doc = PlanDoc::parse("2026-W29", &removed);
        let mon = doc.meals.iter().find(|m| m.weekday == "Mon").unwrap();
        assert!(!mon.dish.to_lowercase().contains("side salad"));
        assert!(mon.dish.contains("curry"), "the main dish stays");
    }

    #[test]
    fn fast_lane_apply_shopping_add_round_trips() {
        let op = FastLaneOp::ShoppingAdd {
            item: "olive oil".into(),
        };
        let edited = apply_to_content("2026-W29", W29, &op).expect("shopping add applies");
        let doc = PlanDoc::parse("2026-W29", &edited);
        assert!(
            doc.shopping
                .iter()
                .flat_map(|s| s.items.iter())
                .any(|i| i.contains("olive oil"))
        );
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
        let calendar_owner = owners.owner_for_domain(crate::notify::ownership::Domain::Calendar);
        let op = FastLaneOp::ReminderSet {
            text: "defrost the chicken".into(),
            day: Some(Weekday::Fri),
            date: NaiveDate::from_ymd_opt(2026, 7, 17).unwrap(),
            time: Some("17:00".into()),
        };
        let edited = apply_to_content_with_calendar_owner("2026-W29", W29, &op, calendar_owner)
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
        let op = FastLaneOp::MealSwap {
            day: Weekday::Fri,
            dish: "tacos".into(),
        };
        assert_eq!(
            apply_to_content("2026-W29", plan, &op),
            Err(FastLaneError::DayNotFound)
        );
    }

    #[test]
    fn fast_lane_remove_absent_component_errors() {
        let op = FastLaneOp::MealRemove {
            day: Weekday::Fri,
            target: "pineapple".into(),
        };
        let err = apply_to_content("2026-W29", W29, &op).unwrap_err();
        assert!(matches!(err, FastLaneError::NotApplicable(_)));
    }

    // ---- report-back lines -----------------------------------------------

    #[test]
    fn fast_lane_report_lines_are_plain_voice() {
        assert_eq!(
            report_line(&FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "tacos".into()
            }),
            "Done — tacos Friday 🌮"
        );
        assert_eq!(
            report_line(&FastLaneOp::ShoppingAdd {
                item: "milk".into()
            }),
            "Done — milk on the shopping list 🛒"
        );
        assert_eq!(
            report_line(&FastLaneOp::MealRemove {
                day: Weekday::Mon,
                target: "side salad".into()
            }),
            "Done — dropped side salad from Monday ✂️"
        );
    }

    // ---- live-cert shopping mutation language (task engine-shopping-language) ----
    // Every phrase below is VERBATIM from docs/reviews/LIVE-CONVO-CERT-2026-07-26.md
    // (C056–C064), including the corpus's mangled "Don not". Measured on this engine
    // before the fix: the removals reached nothing, "glorptwax" was WRITTEN and
    // confirmed, the held ask was WRITTEN, and the out-of sentence was unrecognized.

    fn ask_reason(msg: &str) -> AskReason {
        match classify(msg, today()) {
            Classification::Ask { reason, .. } => reason,
            other => panic!("expected an ASK for {msg:?}, got {other:?}"),
        }
    }

    #[test]
    fn c056_c057_removal_phrasings_reach_a_real_removal_op() {
        assert_eq!(
            fast("Remove AA batteries again."),
            FastLaneOp::ShoppingRemove {
                item: "aa batteries".into()
            }
        );
        assert_eq!(
            fast("Take dishwasher tablets back off."),
            FastLaneOp::ShoppingRemove {
                item: "dishwasher tablets".into()
            }
        );
        assert_eq!(
            fast("remove the AA batteries from the shopping list"),
            FastLaneOp::ShoppingRemove {
                item: "aa batteries".into()
            }
        );
        assert_eq!(
            fast("scratch the olive oil off the shopping list"),
            FastLaneOp::ShoppingRemove {
                item: "olive oil".into()
            }
        );
    }

    #[test]
    fn c058_a_nonsense_item_is_asked_about_never_written() {
        for phrase in ["Add glorptwax to shopping.", "Add glorptwax to the shopping list."] {
            assert_eq!(ask_reason(phrase), AskReason::UnknownItem);
            // And emphatically NOT a write.
            assert!(
                !matches!(
                    classify(phrase, today()),
                    Classification::FastLane(FastLaneOp::ShoppingAdd { .. })
                ),
                "{phrase:?} must never classify as a shopping ADD"
            );
        }
        // A real good the aisle taxonomy has no entry for still lands normally.
        assert_eq!(
            fast("add freezer bags to the shopping list"),
            FastLaneOp::ShoppingAdd {
                item: "freezer bags".into()
            }
        );
    }

    #[test]
    fn c059_a_held_ask_asks_and_never_writes() {
        // The corpus phrase verbatim, "Don not" and all.
        assert_eq!(
            ask_reason("We are low on baking soda. Don not add it yet—ask me first."),
            AskReason::HeldAsk
        );
        assert_eq!(
            ask_reason("Don't add olive oil to the shopping list yet—ask me first."),
            AskReason::HeldAsk
        );
        assert_eq!(
            ask_reason("We're low on baking soda. Don't add it to the list yet—ask me first."),
            AskReason::HeldAsk
        );
    }

    #[test]
    fn c060_a_cancel_takes_the_item_back_off() {
        assert_eq!(
            fast("no baking soda needed after all"),
            FastLaneOp::ShoppingRemove {
                item: "baking soda".into()
            }
        );
        assert_eq!(
            fast("we don't need the batteries anymore"),
            FastLaneOp::ShoppingRemove {
                item: "batteries".into()
            }
        );
    }

    #[test]
    fn c064_the_out_of_sentence_yields_exactly_one_item() {
        // The misparse the report caught: one SENTENCE, one item — never the
        // duplicated literal "olive oil—add olive oil".
        assert_eq!(
            fast("We are out of olive oil—add olive oil."),
            FastLaneOp::ShoppingAdd {
                item: "olive oil".into()
            }
        );
        assert_eq!(
            fast("We are out of olive oil—add olive oil to the shopping list."),
            FastLaneOp::ShoppingAdd {
                item: "olive oil".into()
            }
        );
    }

    #[test]
    fn crossing_an_item_off_is_never_a_removal_op() {
        // Bought ≠ deleted: the row stays, struck through. Never a fast-lane write.
        assert_eq!(
            fallback("cross the milk off the list"),
            FallbackReason::NotASimpleEdit
        );
        assert_eq!(
            fallback("checked off the eggs on the shopping list"),
            FallbackReason::NotASimpleEdit
        );
    }

    #[test]
    fn a_meal_edit_is_not_hijacked_by_the_shopping_lane() {
        // The shopping lane shares the "add"/"remove" verbs with the meal ops. A turn
        // that names a day and no list is still a MENU change.
        assert_eq!(
            fast("drop Monday's side salad"),
            FastLaneOp::MealRemove {
                day: Weekday::Mon,
                target: "side salad".into()
            }
        );
        assert_eq!(
            fast("add a dessert on Tuesday"),
            FastLaneOp::MealAdd {
                day: Weekday::Tue,
                addition: "dessert".into()
            }
        );
        assert_eq!(
            fast("swap Friday to tacos"),
            FastLaneOp::MealSwap {
                day: Weekday::Fri,
                dish: "tacos".into()
            }
        );
        // An unscoped dish word is plan-OR-list: left to the composer, not applied.
        assert_eq!(
            fallback("remove the pasta"),
            FallbackReason::NotASimpleEdit
        );
    }

    #[test]
    fn an_action_sentence_is_never_a_silent_list_write() {
        // Found by probing the finished lane through `wg telegram shopping`
        // (task shopping-engine-half). Each of these named NO list, yet each wrote a
        // junk row and confirmed it: "chicken in the oven", "to talk about the milk",
        // "to get milk", "bottle of wine on the way home". An unscoped turn that is
        // about DOING something belongs to the composer.
        for phrase in [
            "put the chicken in the oven",
            "we need to talk about the milk",
            "grab a bottle of wine on the way home",
            "we need to buy a gift for the party",
        ] {
            assert_eq!(
                classify(phrase, today()),
                Classification::Fallback(FallbackReason::NotASimpleEdit),
                "{phrase:?} must reach the composer, not the shopping list"
            );
        }
        // The real buy asks still land, and the verb never survives into the row.
        assert_eq!(
            fast("we need to get milk"),
            FastLaneOp::ShoppingAdd {
                item: "milk".into()
            }
        );
        assert_eq!(
            fast("add a gift for the party to the shopping list"),
            FastLaneOp::ShoppingAdd {
                item: "gift for the party".into()
            }
        );
    }

    #[test]
    fn a_reminder_about_the_list_is_still_a_reminder() {
        // Reminders keep priority over the shopping lane.
        assert!(matches!(
            fast("remind me to add milk to the shopping list on Friday"),
            FastLaneOp::ReminderSet { .. }
        ));
    }

    #[test]
    fn apply_shopping_remove_really_removes_and_round_trips() {
        let op = FastLaneOp::ShoppingRemove {
            item: "spinach".into(),
        };
        let before = PlanDoc::parse("2026-W29", W29);
        assert!(
            before
                .shopping
                .iter()
                .flat_map(|s| s.items.iter())
                .any(|i| i.to_lowercase().contains("spinach")),
            "fixture precondition: spinach is on the list"
        );
        let edited = apply_to_content("2026-W29", W29, &op).expect("removal applies");
        let after = PlanDoc::parse("2026-W29", &edited);
        assert!(
            !after
                .shopping
                .iter()
                .flat_map(|s| s.items.iter())
                .any(|i| i.to_lowercase().contains("spinach")),
            "the row is gone after the real parser re-reads the file"
        );
        // Exactly ONE row came off — a plural noun cannot empty a section.
        let before_count: usize = before.shopping.iter().map(|s| s.items.len()).sum();
        let after_count: usize = after.shopping.iter().map(|s| s.items.len()).sum();
        assert_eq!(after_count, before_count - 1);
    }

    #[test]
    fn apply_shopping_remove_of_an_absent_item_is_refused() {
        let op = FastLaneOp::ShoppingRemove {
            item: "glorptwax".into(),
        };
        let err = apply_to_content("2026-W29", W29, &op).expect_err("nothing to remove");
        assert!(matches!(err, FastLaneError::NotApplicable(_)), "got {err:?}");
    }

    #[test]
    fn e2e_a_nonsense_item_and_a_held_ask_leave_the_plan_byte_identical() {
        let dir = std::env::temp_dir().join(format!("fastlane-shop-ask-{}", std::process::id()));
        let plans = dir.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan_path = plans.join("2026-W29-family-plan.md");
        std::fs::write(&plan_path, W29).unwrap();

        for (phrase, want_lane) in [
            ("Add glorptwax to the shopping list.", "unknown-item"),
            (
                "Don't add olive oil to the shopping list yet—ask me first.",
                "held-ask",
            ),
            // A removal that matches no row is answered honestly, not "done".
            ("remove the pineapple from the shopping list", "nothing-to-remove"),
        ] {
            match run_fast_lane(&dir, phrase, today()) {
                FastLaneResult::Answered { reply, lane } => {
                    assert_eq!(lane, want_lane, "lane for {phrase:?}");
                    assert!(!reply.is_empty());
                    // The answer must not claim a write.
                    let low = reply.to_lowercase();
                    assert!(
                        !low.starts_with("done"),
                        "an ask must never open like an applied edit: {reply:?}"
                    );
                }
                other => panic!("expected Answered for {phrase:?}, got {other:?}"),
            }
            assert_eq!(
                std::fs::read_to_string(&plan_path).unwrap(),
                W29,
                "the plan file must be untouched after {phrase:?}"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn e2e_a_conversational_removal_reaches_the_plan_file() {
        let dir = std::env::temp_dir().join(format!("fastlane-shop-rm-{}", std::process::id()));
        let plans = dir.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan_path = plans.join("2026-W29-family-plan.md");
        std::fs::write(&plan_path, W29).unwrap();

        // First put a jotted item on the list the conversational way…
        match run_fast_lane(&dir, "add AA batteries to the shopping list", today()) {
            FastLaneResult::Applied { .. } => {}
            other => panic!("expected the add to apply, got {other:?}"),
        }
        let mid = std::fs::read_to_string(&plan_path).unwrap();
        assert!(mid.to_lowercase().contains("aa batteries"));

        // …then take it back off with the report's own phrasing. THIS is the
        // asymmetry the live-cert found: the add persisted, the removal did not.
        match run_fast_lane(&dir, "Remove AA batteries again.", today()) {
            FastLaneResult::Applied { report, op, .. } => {
                assert!(matches!(op, FastLaneOp::ShoppingRemove { .. }));
                assert!(report.contains("off the shopping list"), "got {report:?}");
            }
            other => panic!("expected the removal to apply, got {other:?}"),
        }
        let after = std::fs::read_to_string(&plan_path).unwrap();
        assert!(
            !after.to_lowercase().contains("aa batteries"),
            "the removal must reach the plan file the /week and kiosk surfaces read"
        );
        // Nothing else was disturbed.
        let doc = PlanDoc::parse("2026-W29", &after);
        assert!(doc.meals.len() >= 5, "the meal table survived the edit");

        std::fs::remove_dir_all(&dir).ok();
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
            FastLaneResult::Applied {
                report,
                op,
                week_code,
            } => {
                assert_eq!(report, "Done — tacos Friday 🌮");
                assert_eq!(week_code, "2026-W29");
                assert!(matches!(op, FastLaneOp::MealSwap { .. }));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        // The file on disk actually changed and still parses.
        let after = std::fs::read_to_string(&plan_path).unwrap();
        let doc = PlanDoc::parse("2026-W29", &after);
        assert_eq!(
            doc.meals.iter().find(|m| m.weekday == "Fri").unwrap().dish,
            "tacos"
        );

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

    // ---- date-reminder-fail end-to-end ------------------------------------

    #[test]
    fn e2e_forward_weekday_writes_the_next_weeks_plan_not_a_stale_row() {
        // Sat 2026-07-18: "Monday" is Jul 20 — which lives in the NEXT plan file.
        // The reminder must land there, dated Mon 07-20, and W29 must be untouched.
        let root = tempfile::tempdir().unwrap();
        let plans = root.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), W29).unwrap();
        let w30 = W29
            .replace("2026-07-13", "2026-07-20")
            .replace("2026-07-19", "2026-07-26")
            .replace("Mon 07-13", "Mon 07-20")
            .replace("Tue 07-14", "Tue 07-21")
            .replace("Wed 07-15", "Wed 07-22")
            .replace("Thu 07-16", "Thu 07-23")
            .replace("Fri 07-17", "Fri 07-24")
            .replace("Sat 07-18", "Sat 07-25")
            .replace("Sun 07-19", "Sun 07-26");
        std::fs::write(plans.join("2026-W30-family-plan.md"), &w30).unwrap();

        let result = run_fast_lane_with_calendar_owner(
            root.path(),
            "remind me Monday to take the bins out",
            NaiveDate::from_ymd_opt(2026, 7, 18).unwrap(),
            Some("harbor"),
        );
        match &result {
            FastLaneResult::Applied { week_code, .. } => assert_eq!(week_code, "2026-W30"),
            other => panic!("expected Applied, got {other:?}"),
        }
        let after30 = std::fs::read_to_string(plans.join("2026-W30-family-plan.md")).unwrap();
        assert!(
            after30.contains("| Mon 07-20 | 09:00 | ⏰ Reminder: take the bins out | harbor |"),
            "the reminder must be filed on the UPCOMING Monday:\n{after30}"
        );
        assert_eq!(
            std::fs::read_to_string(plans.join("2026-W29-family-plan.md")).unwrap(),
            W29,
            "the elapsed week's plan must not be touched"
        );
    }

    #[test]
    fn e2e_cancel_clears_the_adhoc_reminder_the_dm_path_filed() {
        use crate::notify::reminder::{AdHocStore, Reminder, ReminderSource};

        let root = tempfile::tempdir().unwrap();
        let plans = root.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), W29).unwrap();

        let store_path = AdHocStore::path(root.path());
        let mut store = AdHocStore::default();
        store.add(Reminder {
            id: "adhoc:dentist".into(),
            due: NaiveDate::from_ymd_opt(2026, 7, 16)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            recipient: "Household Member".into(),
            bot: "harbor".into(),
            text: "Book the dentist".into(),
            source: ReminderSource::AdHoc,
        });
        store.save(&store_path).unwrap();

        let result = run_fast_lane_with_calendar_owner(
            root.path(),
            "cancel the reminder about the dentist",
            today(),
            Some("harbor"),
        );
        match &result {
            FastLaneResult::Applied { report, op, .. } => {
                assert_eq!(report, "Done — cancelled the reminder about dentist ✓");
                assert_eq!(op.kind_label(), "reminder-cancel");
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert!(
            AdHocStore::load(&store_path).reminders.is_empty(),
            "the ad-hoc reminder should be gone"
        );
    }

    #[test]
    fn e2e_remind_me_what_never_touches_the_plan() {
        let root = tempfile::tempdir().unwrap();
        let plans = root.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan_path = plans.join("2026-W29-family-plan.md");
        std::fs::write(&plan_path, W29).unwrap();

        let result = run_fast_lane_with_calendar_owner(
            root.path(),
            "Remind me what was in Monday's risotto",
            today(),
            Some("harbor"),
        );
        assert!(
            matches!(result, FastLaneResult::Fallback { .. }),
            "a memory question belongs to the composer, got {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&plan_path).unwrap(),
            W29,
            "a read must never write a reminder row"
        );
    }

    // ── the week-start lane (task `week-start-engine`) ──────────────────────

    /// The plan of a week that has ALREADY ENDED — the only thing on disk in the
    /// Monday window the week-start offer is made in.
    const ENDED_W30: &str = "\
# Family week · 2026-W30 · Week of Monday July 20 – Sunday July 26

**Week of Monday 2026-07-20 to Sunday 2026-07-26**
**Status:** PUBLISHED

## 1. Dinners (planner → cook)

| Day | Slot | Dinner | Prep |
|-----|------|--------|------|
| Mon 07-20 | Vegetarian | Chickpea curry | ~35 min |
| Tue 07-21 | Fish | Baked salmon | ~30 min |

## 4. Shopping list — by store

### Greengrocer / produce
- Chard, 1 bunch
";

    /// The exact message the gateway dispatches when the family accepts the
    /// offer, carrying the request that was refused.
    const DISPATCHED: &str = "Please draft this week's family plan — start the week. \
Keep what I just asked for: \"Set Tuesday's dinner to homemade pizza.\"";

    fn monday_w31() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 27).unwrap()
    }

    /// The dispatched ask is OWNED by the closed set — not handed to a composer
    /// that has no way to create a plan file.
    #[test]
    fn the_dispatched_week_start_ask_is_classified_as_week_start() {
        match classify(DISPATCHED, monday_w31()) {
            Classification::FastLane(FastLaneOp::WeekStart { carried }) => {
                assert_eq!(carried, vec!["Set Tuesday's dinner to homemade pizza."]);
            }
            other => panic!("the dispatched week-start ask was not owned: {other:?}"),
        }
    }

    /// THE ARCHIVED-WEEK TRAP. The carried quote reads exactly like a bare meal
    /// swap. If the ordinary matcher saw it first, the edit would land on the
    /// newest plan on disk — which on this Monday is the week that has already
    /// ended, the very write the offer exists to prevent.
    #[test]
    fn the_carried_quote_is_never_applied_to_the_week_that_ended() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("plans")).unwrap();
        let ended = root.path().join("plans/2026-W30-family-plan.md");
        std::fs::write(&ended, ENDED_W30).unwrap();

        let result = run_fast_lane(root.path(), DISPATCHED, monday_w31());
        let FastLaneResult::Applied { week_code, .. } = &result else {
            panic!("the week-start ask was not fulfilled: {result:?}");
        };
        assert_eq!(week_code, "2026-W31");
        assert_eq!(
            std::fs::read_to_string(&ended).unwrap(),
            ENDED_W30,
            "the carried request was written into the week that had already ended",
        );
        let drafted =
            std::fs::read_to_string(root.path().join("plans/2026-W31-family-plan.md")).unwrap();
        let doc = PlanDoc::parse("2026-W31", &drafted);
        assert!(
            doc.meal_on(NaiveDate::from_ymd_opt(2026, 7, 28).unwrap())
                .map(|m| m.dish.to_lowercase().contains("homemade pizza"))
                .unwrap_or(false),
            "the drafted week does not carry the requested edit:\n{drafted}",
        );
    }

    /// Accepting the offer twice never erases the week the first yes created.
    #[test]
    fn a_second_yes_answers_honestly_and_overwrites_nothing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("plans")).unwrap();
        std::fs::write(root.path().join("plans/2026-W30-family-plan.md"), ENDED_W30).unwrap();

        assert!(matches!(
            run_fast_lane(root.path(), DISPATCHED, monday_w31()),
            FastLaneResult::Applied { .. }
        ));
        let first = std::fs::read_to_string(root.path().join("plans/2026-W31-family-plan.md"))
            .unwrap();

        let again = run_fast_lane(root.path(), DISPATCHED, monday_w31());
        let FastLaneResult::Answered { lane, reply } = &again else {
            panic!("a second acceptance was not answered honestly: {again:?}");
        };
        assert_eq!(lane, "week-already-started");
        assert!(!reply.to_lowercase().contains("started this week's plan"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("plans/2026-W31-family-plan.md")).unwrap(),
            first,
            "a repeated acceptance rewrote the week",
        );
    }

    /// A carried request the closed set cannot express writes NO week — the
    /// heavy pipeline takes the turn instead of a plan landing without it.
    #[test]
    fn a_week_is_never_drafted_without_the_request_it_carried() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("plans")).unwrap();
        std::fs::write(root.path().join("plans/2026-W30-family-plan.md"), ENDED_W30).unwrap();

        let result = run_fast_lane(
            root.path(),
            "Please draft this week's family plan — start the week. Keep what I asked for: \
             \"Rebalance the whole week around Nadin's travel.\"",
            monday_w31(),
        );
        assert!(
            matches!(result, FastLaneResult::Fallback { .. }),
            "an unfulfillable carry must defer, got {result:?}",
        );
        assert!(
            !root.path().join("plans/2026-W31-family-plan.md").exists(),
            "a week was drafted without the request the family carried into it",
        );
    }

    /// A long-day-cell household (`Tuesday July 28`) is editable too — the shape
    /// the kiosk's own writer produces.
    #[test]
    fn a_long_day_cell_row_is_editable() {
        let long = "\
# Family week · 2026-W29

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** PUBLISHED

## 1. Meals (planner → cook)

| Day | Kind | Dinner | Time at the stove |
|-----|------|--------|-------------------|
| Tuesday July 14 | Fish | Baked hake | ~25 min |
";
        let edited = apply_to_content(
            "2026-W29",
            long,
            &FastLaneOp::MealSwap {
                day: Weekday::Tue,
                dish: "homemade pizza".into(),
            },
        )
        .expect("a long day cell must be editable");
        assert!(edited.contains("homemade pizza"), "{edited}");
    }

    /// …and a first cell that merely STARTS like a weekday is not a day row.
    #[test]
    fn a_lookalike_first_cell_is_not_a_day_row() {
        let sneaky = "\
# Family week · 2026-W29

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** PUBLISHED

## 1. Meals (planner → cook)

| Day | Kind | Dinner | Prep |
|-----|------|--------|------|
| Monthly total | — | 21 dinners | — |
";
        assert!(apply_to_content(
            "2026-W29",
            sneaky,
            &FastLaneOp::MealSwap {
                day: Weekday::Mon,
                dish: "tacos".into(),
            },
        )
        .is_err());
    }
}
