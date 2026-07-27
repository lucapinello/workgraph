//! The ENGINE half of the week-start promise (task `week-start-engine`).
//!
//! THE PROMISE. On the Monday before the Sunday ritual has run, every write that
//! belongs to a plan of record is refused with an OFFER — "This week isn't set up
//! yet — want me to start it?" — and a bare "yes" accepts it. The gateway half of
//! that exchange (task `current-week-write`) rewrites the affirmative into an
//! explicit ask and dispatches it, carrying the request the family originally made
//! QUOTED inside the message, because production web inbound drops the structured
//! `continuation` field and the words are all that survive the trip.
//!
//! THE GAP THIS MODULE CLOSES. Nothing on this side consumed that ask. The
//! dispatched message fell through the closed-set classifier to the composer,
//! which has no way to create a plan file — so an accepted offer produced an
//! encouraging sentence and no week. The human flow that "proved" the promise
//! hand-wrote the new plan onto disk itself and then asserted a write landed in
//! it, which proves the gateway dispatches and proves nothing at all about
//! fulfilment. An offer the family accepts and nothing happens is worse than a
//! closed refusal; this module is what makes the question honest.
//!
//! WHAT IT DOES, AND WHAT IT REFUSES TO DO.
//!   · [`detect`] recognizes the dispatched week-start ask and pulls out every
//!     QUOTED carried request. Detection is deliberately narrow: a closed set of
//!     imperative phrasings, never a question about the week ("did you start the
//!     week?" is a read, and answering it must not write a plan), and never a
//!     NEGATED one — "Don't start the week." names the lane precisely in order
//!     to refuse it, and a substring scan for "start the week" read that as
//!     consent and created the plan. [`negated`] reports that refusal so a
//!     caller can say "nothing was created" instead of falling silently through.
//!   · [`draft_week`] builds THIS week's plan document — the ordinary drafting
//!     path: the household's own most recent plan supplies the SHAPE (its section
//!     headings, its meals-table columns, its day-cell style, its store
//!     sections), so a drafted week looks like the family's own plans and not
//!     like a template someone compiled in. Then every carried request is applied
//!     to that draft through the REAL fast-lane editors and round-tripped through
//!     the REAL plan parser.
//!   · IT FAILS CLOSED. The document is assembled, edited, and verified entirely
//!     IN MEMORY; the file is written only once every carried request has
//!     provably landed in it. A carried request the closed set cannot express, or
//!     an edit that does not survive the re-parse, aborts the whole draft — no
//!     half-planned week, no plan on disk that silently dropped what the family
//!     asked for. "Drafted the week but ignored the edit" is the exact failure
//!     this ordering exists to make impossible.
//!   · After the write it RE-READS the file from disk and re-parses it before
//!     reporting success, so a dead pipeline cannot claim a week it never wrote.
//!
//! IT NEVER OVERWRITES. A week that already has a plan of record is answered
//! honestly ("already set up") and left byte-identical: accepting an offer twice,
//! or a dispatcher refire, must not erase a week the family has already filled in.

use std::path::{Path, PathBuf};

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use super::family_plan::{self, PlanDoc};
use super::fast_lane::{self, Classification, FastLaneError, FastLaneOp};

/// A recognized week-start ask, with the requests it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekStartAsk {
    /// Every QUOTED request the dispatched message carried, in order. The
    /// gateway quotes the ask that was refused ("Set Tuesday's dinner to
    /// homemade pizza."), so the new week is drafted WITH it rather than
    /// dropping the thing the family actually wanted.
    pub carried: Vec<String>,
}

/// One carried request that provably landed in the drafted week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedEdit {
    /// The family's own words, exactly as the message carried them.
    pub request: String,
    /// The closed-set operation they resolved to (`meal-swap`, …).
    pub op_kind: String,
    /// The immediate family-voice confirmation for that edit.
    pub report: String,
}

/// A week drafted onto disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftedWeek {
    pub week_code: String,
    pub path: PathBuf,
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// Every carried request, proven present in the plan that is now on disk.
    pub preserved: Vec<PreservedEdit>,
    /// Every dinner the family PARKED for this week before it existed, proven
    /// present in the plan that is now on disk.
    pub parked: Vec<ParkedDinner>,
    /// The sidecar this draft folded in and stamped, when there was one.
    pub sidecar_retired: Option<PathBuf>,
}

/// Why a week-start ask did NOT produce a plan. Every variant leaves the
/// project byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeekStartError {
    /// This week already has a plan of record. Nothing is overwritten.
    AlreadyPlanned { week_code: String },
    /// A carried request is not something the closed set can put into a plan.
    /// The draft is abandoned rather than written without it.
    CarriedNotUnderstood { request: String },
    /// A carried request was understood but did not survive the edit or the
    /// re-parse. Same rule: nothing is written.
    CarriedLost { request: String, reason: String },
    /// A dinner the family PARKED for this week did not survive into the draft.
    /// Same rule as a carried request: the whole draft is abandoned, because a
    /// week that silently dropped a dinner someone typed in is the failure this
    /// module exists to prevent — and it is the exact failure that shipped when
    /// shape discovery read the sidecar as a plan.
    ParkedLost {
        date: NaiveDate,
        dish: String,
        reason: String,
    },
    /// The plan could not be written, or could not be read back afterwards.
    Io(String),
    /// The written file did not parse back to the week we drafted — a dead
    /// pipeline is never reported as a success.
    Unverified(String),
}

impl std::fmt::Display for WeekStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WeekStartError::AlreadyPlanned { week_code } => {
                write!(f, "{week_code} already has a plan of record")
            }
            WeekStartError::CarriedNotUnderstood { request } => {
                write!(f, "carried request not in the closed set: {request:?}")
            }
            WeekStartError::CarriedLost { request, reason } => {
                write!(f, "carried request {request:?} did not land: {reason}")
            }
            WeekStartError::ParkedLost { date, dish, reason } => {
                write!(f, "parked dinner {dish:?} for {date} did not land: {reason}")
            }
            WeekStartError::Io(m) => write!(f, "week-start io: {m}"),
            WeekStartError::Unverified(m) => write!(f, "drafted week failed verification: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// The imperative phrasings that mean "create this week's plan of record". Kept
/// as a closed list, matched against the message with its quoted carriage
/// REMOVED, so a quoted request can never smuggle in the instruction.
const START_PHRASES: &[&str] = &[
    "draft this week's family plan",
    "draft this week’s family plan",
    "draft this week's plan",
    "draft this week’s plan",
    "start the week",
    "start this week",
    "set up this week",
    "set this week up",
    "plan this week",
    "get this week started",
];

/// Openers that make the sentence a QUESTION about the week rather than an
/// instruction to create it. "Did you start the week?" is a read; answering it
/// by drafting a plan would be the same class of bug as writing on a read.
const QUESTION_OPENERS: &[&str] = &[
    "did ", "do ", "does ", "have ", "has ", "is ", "are ", "was ", "were ", "will ", "would ",
    "can ", "could ", "should ", "shall ", "when ", "what ", "why ", "how ", "who ", "where ",
];

/// Cues that TURN THE START PHRASE OFF. Detection is a substring scan for a
/// closed list of imperatives, and a substring scan reads "Don't start the week"
/// as an instruction to start the week — the negation is invisible to it. That
/// is the whole bug: the sentence that most explicitly says *do not create the
/// plan* created the plan.
///
/// These are matched inside the CLAUSE that carries the start phrase, never
/// across the whole message, because a plain "not" somewhere else is usually
/// part of the reason FOR the ask: "this week is not set up — please start the
/// week" is a genuine request and must stay green. For the same reason the list
/// holds no bare `not `: every entry is a multi-word form that cannot be the
/// premise of a positive ask.
const NEGATION_CUES: &[&str] = &[
    "don't",
    "don\u{2019}t",
    "dont",
    "do not",
    "doesn't",
    "doesn\u{2019}t",
    "does not",
    "didn't",
    "didn\u{2019}t",
    "won't",
    "won\u{2019}t",
    "will not",
    "can't",
    "can\u{2019}t",
    "cannot",
    "never",
    "not yet",
    "not now",
    "no need to",
    "rather not",
    "without starting",
    "without setting up",
    "without creating",
    "without drafting",
    "without planning",
    "instead of starting",
    "hold off",
    "cancel",
    "stop",
];

/// A clause that is NOTHING BUT a refusal. "Not yet, start the week later"
/// splits into a bare refusal and a clause the cue scan alone would read as an
/// instruction, so a standalone refusal anywhere in the message turns the whole
/// message off. Matched whole-clause — "no need for a rush" is not "no thanks".
const STANDALONE_REFUSALS: &[&str] = &[
    "not yet",
    "not now",
    "not this week",
    "no thanks",
    "no thank you",
    "never mind",
    "nevermind",
    "hold off",
    "hold on",
    "cancel",
    "cancel that",
    "stop",
    "stop that",
];

/// Split an instruction into clauses. Sentence AND clause punctuation both
/// count: the negation has to be scoped to the part of the sentence that
/// carries the start phrase, or the guard becomes the same blunt substring scan
/// it exists to fix.
fn clauses(low: &str) -> Vec<&str> {
    low.split(|c: char| {
        matches!(
            c,
            '.' | '!' | '?' | ';' | ',' | ':' | '\n' | '\u{2014}' | '\u{2013}' | '-'
        )
    })
    .map(|c| c.trim())
    .filter(|c| !c.is_empty())
    .collect()
}

/// Is this recognized-looking instruction actually the family saying DON'T?
///
/// `low` is the lowercased message with its quoted carriage already removed, so
/// a quoted request can no more negate the instruction than it can smuggle one
/// in.
fn negates_start(low: &str) -> bool {
    for clause in clauses(low) {
        let bare = clause
            .trim_start_matches("please ")
            .trim_start_matches("ok ")
            .trim_start_matches("okay ")
            .trim_end_matches(" please")
            .trim();
        if STANDALONE_REFUSALS.iter().any(|r| bare == *r) {
            return true;
        }
        if START_PHRASES.iter().any(|p| clause.contains(p))
            && NEGATION_CUES.iter().any(|c| clause.contains(c))
        {
            return true;
        }
    }
    false
}

/// Does `message` name the week-start lane but REFUSE it? True only when a start
/// phrase is present and negated — so a caller can answer honestly ("nothing was
/// created") instead of falling silently through to the composer, and a proof
/// can assert the refusal rather than merely the absence of a file.
pub fn negated(message: &str) -> bool {
    let (_carried, instruction) = split_quoted(message);
    let low = instruction.to_lowercase();
    let low = low.trim();
    if !START_PHRASES.iter().any(|p| low.contains(p)) {
        return false;
    }
    negates_start(low)
}

/// Pull every double-quoted span out of `text`, returning (spans, text with the
/// spans removed). Straight and curly quotes both count — the gateway quotes the
/// family's own words and a keyboard may produce either.
fn split_quoted(text: &str) -> (Vec<String>, String) {
    let mut quoted: Vec<String> = Vec::new();
    let mut rest = String::new();
    let mut current: Option<String> = None;
    // The closing mark we are waiting for, for the quote style we opened with.
    let mut closer = '"';
    for ch in text.chars() {
        match &mut current {
            Some(buf) => {
                if ch == closer {
                    let done = std::mem::take(buf);
                    let done = done.trim().to_string();
                    if !done.is_empty() {
                        quoted.push(done);
                    }
                    current = None;
                    rest.push(' ');
                } else {
                    buf.push(ch);
                }
            }
            None => match ch {
                '"' => {
                    closer = '"';
                    current = Some(String::new());
                }
                '\u{201c}' => {
                    closer = '\u{201d}';
                    current = Some(String::new());
                }
                _ => rest.push(ch),
            },
        }
    }
    // An UNCLOSED quote is not carriage — put its text back so the instruction
    // scan still sees it rather than silently swallowing half the message.
    if let Some(buf) = current {
        rest.push('"');
        rest.push_str(&buf);
    }
    (quoted, rest)
}

/// Recognize a dispatched week-start ask. `None` for everything else — including
/// a question ABOUT the week, which must never write a plan.
pub fn detect(message: &str) -> Option<WeekStartAsk> {
    let (carried, instruction) = split_quoted(message);
    let low = instruction.to_lowercase();
    let low = low.trim();
    if low.is_empty() {
        return None;
    }
    if !START_PHRASES.iter().any(|p| low.contains(p)) {
        return None;
    }
    // A question about the week is a READ. Two independent signals, because a
    // family types both shapes: an interrogative opener, or a bare "?" with no
    // imperative "please"/"let's" softener anywhere.
    let opens_question = QUESTION_OPENERS.iter().any(|q| low.starts_with(q));
    if opens_question {
        return None;
    }
    if low.contains('?') && !low.contains("please") && !low.contains("let's") {
        return None;
    }
    // "Don't start the week." NAMES the lane in order to refuse it. A substring
    // scan cannot see that, and the sentence that most explicitly says *do not
    // create the plan* is the one that created it.
    if negates_start(low) {
        return None;
    }
    Some(WeekStartAsk { carried })
}

// ---------------------------------------------------------------------------
// The shape a drafted week inherits from the household's own plans
// ---------------------------------------------------------------------------

/// How a day cell is written in this household's plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DayCellStyle {
    /// `Mon 07-27` — the shape the plan parser dates rows from.
    Short,
    /// `Monday July 27`.
    Long,
}

/// The parts of a plan document a new week copies from the previous one, so a
/// drafted week reads like the family's own plans instead of a compiled-in
/// template. Everything here is STRUCTURE (headings, columns, day-cell style) —
/// never last week's content, which belongs to the week that had it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanShape {
    title_prefix: String,
    meals_heading: String,
    meals_header: Vec<String>,
    day_style: DayCellStyle,
    shopping_heading: String,
    shopping_sections: Vec<String>,
    calendar_heading: Option<String>,
    calendar_header: Vec<String>,
}

impl Default for PlanShape {
    fn default() -> Self {
        PlanShape {
            title_prefix: "Family week".to_string(),
            meals_heading: "1. Dinners".to_string(),
            meals_header: vec![
                "Day".to_string(),
                "Slot".to_string(),
                "Dinner".to_string(),
                "Prep".to_string(),
            ],
            day_style: DayCellStyle::Short,
            shopping_heading: "2. Shopping list".to_string(),
            shopping_sections: vec!["Grocery / general".to_string()],
            calendar_heading: Some("3. Calendar".to_string()),
            calendar_header: vec![
                "Day".to_string(),
                "Time".to_string(),
                "Event".to_string(),
                "Source".to_string(),
            ],
        }
    }
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.starts_with('|') {
        return None;
    }
    Some(
        t.trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect(),
    )
}

fn is_rule_row(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
}

/// Read the household's structure out of one plan document.
fn shape_of(content: &str) -> PlanShape {
    let mut shape = PlanShape::default();
    let mut seen_meals_header = false;
    let mut seen_calendar_header = false;
    let mut first_meal_day: Option<String> = None;
    #[derive(Clone, Copy, PartialEq)]
    enum Sec {
        None,
        Meals,
        Shopping,
        Calendar,
        Other,
    }
    let mut sec = Sec::None;
    let mut shopping_sections: Vec<String> = Vec::new();
    let mut calendar_heading: Option<String> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if let Some(title) = line.strip_prefix("# ") {
            // "Family week · 2026-W30 · Week of …" / "2026-W30 Family Plan" — keep
            // whatever the household leads with, minus this week's own identity.
            let lead = title.split('·').next().unwrap_or(title).trim();
            if !lead.is_empty() && !lead.starts_with(char::is_numeric) {
                shape.title_prefix = lead.to_string();
            } else if let Some(rest) = title.split_once(' ').map(|(_, r)| r) {
                if !rest.trim().is_empty() {
                    shape.title_prefix = rest.trim().to_string();
                }
            }
            continue;
        }
        if let Some(h2) = line.strip_prefix("## ") {
            let low = h2.to_ascii_lowercase();
            sec = if family_plan::is_meals_section_heading(h2) {
                shape.meals_heading = h2.trim().to_string();
                Sec::Meals
            } else if low.contains("shopping") {
                shape.shopping_heading = h2.trim().to_string();
                Sec::Shopping
            } else if low.contains("calendar") {
                calendar_heading = Some(h2.trim().to_string());
                Sec::Calendar
            } else {
                Sec::Other
            };
            continue;
        }
        if let Some(h3) = line.strip_prefix("### ") {
            if sec == Sec::Shopping {
                shopping_sections.push(h3.trim().to_string());
            }
            continue;
        }
        if let Some(cells) = table_cells(line) {
            if is_rule_row(&cells) {
                continue;
            }
            match sec {
                Sec::Meals => {
                    if !seen_meals_header {
                        shape.meals_header = cells;
                        seen_meals_header = true;
                    } else if first_meal_day.is_none() {
                        if let Some(day) = cells.first() {
                            if !day.is_empty() {
                                first_meal_day = Some(day.clone());
                            }
                        }
                    }
                }
                Sec::Calendar => {
                    if !seen_calendar_header {
                        shape.calendar_header = cells;
                        seen_calendar_header = true;
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(day) = first_meal_day {
        // "Mon 07-13" → Short; "Monday July 13" → Long.
        let first = day.split_whitespace().next().unwrap_or("").to_lowercase();
        shape.day_style = if first.len() <= 3 {
            DayCellStyle::Short
        } else {
            DayCellStyle::Long
        };
    }
    if !shopping_sections.is_empty() {
        shape.shopping_sections = shopping_sections;
    }
    if calendar_heading.is_some() {
        shape.calendar_heading = calendar_heading;
    }
    shape
}

/// Files that live in `plans/` under a week-coded name but are NOT plans of
/// record. `plans/2026-W30-dinner-suggestions.md` is the parked-dinner
/// side-channel the "suggest a dinner" affordance writes; the recipe and workout
/// notes are companions a household member keeps beside the week. Every one of
/// them has a week-coded stem, so the name alone cannot tell them apart from the
/// plan — and shape discovery, which only looked at the name, happily read a
/// bullet list of parked dinners as "the household's most recent plan" and
/// drafted the new week in ITS shape: no meals table, no store sections, and the
/// parked dinners themselves dropped on the floor.
const SIDECAR_SUFFIXES: &[&str] = &["dinner-suggestions"];

/// Is this stem a week-coded file that is NOT a plan of record?
fn is_sidecar_stem(stem: &str) -> bool {
    let low = stem.to_ascii_lowercase();
    SIDECAR_SUFFIXES.iter().any(|s| low.ends_with(s))
}

/// Does this document actually READ like a plan of record? The name check above
/// is a closed list and a household may keep a companion nobody enumerated, so
/// shape discovery ALSO insists on structure: a plan has a meals section. A
/// recipe card or a workout note does not, and a shape copied from one would
/// produce a week with nowhere to put dinner.
fn looks_like_plan(content: &str) -> bool {
    content.lines().any(|line| {
        line.trim()
            .strip_prefix("## ")
            .map(family_plan::is_meals_section_heading)
            .unwrap_or(false)
    })
}

/// The newest plan on disk, by week code — the household's most recent example.
/// Sidecars and companions are excluded: the shape a new week inherits must come
/// from a real plan of record or from nothing at all.
fn newest_plan(root: &Path) -> Option<String> {
    let plans_dir = root.join("plans");
    let mut best: Option<(String, String)> = None;
    for entry in std::fs::read_dir(&plans_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if is_sidecar_stem(stem) {
            continue;
        }
        let Some(week) = week_code_of_stem(stem) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !looks_like_plan(&content) {
            continue;
        }
        if best.as_ref().map(|(w, _)| week > *w).unwrap_or(true) {
            best = Some((week, content));
        }
    }
    best.map(|(_, c)| c)
}

// ---------------------------------------------------------------------------
// Parked dinners — the side-channel the family filled in BEFORE the week existed
// ---------------------------------------------------------------------------

/// One dinner the family parked for a night of the week being drafted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedDinner {
    /// The night it was parked for.
    pub date: NaiveDate,
    /// The dish, with its provenance tail (`· suggested by …`) already peeled.
    pub dish: String,
}

/// The sidecar path for a week code.
fn sidecar_path(root: &Path, week_code: &str) -> PathBuf {
    root.join("plans")
        .join(format!("{week_code}-dinner-suggestions.md"))
}

/// Peel the provenance tail off a parked line. The writer appends
/// `· suggested by Luca`, `· requested by …`, `· recipe written by … in \`path\``
/// — none of which is the dish, and a leaked file path in a dinner cell is a
/// sentence the house would have to explain.
fn strip_provenance(raw: &str) -> String {
    raw.split('\u{b7}').next().unwrap_or(raw).trim().to_string()
}

/// Read the dinners parked for `week_code`, in file order, keyed to their date.
/// Format-tolerant by design — it mirrors the writer
/// (`weekAdapter._parkSuggestion`), whose line is
/// `- **Tuesday** (2026-07-28) — Homemade pizza · suggested by Luca`. The ISO
/// date in the parentheses is authoritative; the weekday word beside it is the
/// family's own copy. Never throws: a missing sidecar is simply no parked
/// dinners.
pub fn parked_dinners(root: &Path, week_code: &str) -> Vec<ParkedDinner> {
    let Ok(body) = std::fs::read_to_string(sidecar_path(root, week_code)) else {
        return Vec::new();
    };
    parse_parked(&body)
}

/// Pure parse of a sidecar body. LAST write for a night wins, matching the
/// surface the family typed into: a re-typed dinner replaces the earlier one
/// rather than stacking a second dish onto the same night.
fn parse_parked(body: &str) -> Vec<ParkedDinner> {
    let mut out: Vec<ParkedDinner> = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        // `**Tuesday** (2026-07-28) — dish …`
        let Some(open) = rest.find('(') else { continue };
        let Some(close) = rest[open..].find(')') else {
            continue;
        };
        let iso = rest[open + 1..open + close].trim();
        let Ok(date) = NaiveDate::parse_from_str(iso, "%Y-%m-%d") else {
            continue;
        };
        let tail = rest[open + close + 1..].trim();
        // The em-dash separator, with the en-dash and a plain hyphen tolerated.
        let dish = tail
            .trim_start_matches(['\u{2014}', '\u{2013}', '-'])
            .trim();
        let dish = strip_provenance(dish);
        if dish.is_empty() {
            continue;
        }
        out.retain(|p| p.date != date);
        out.push(ParkedDinner { date, dish });
    }
    out
}

/// Stamp that says a sidecar's dinners are IN the plan now.
const FOLDED_MARKER: &str = "**Folded into:**";

/// Retire the sidecar whose dinners have provably landed in `plan_path`.
///
/// IDEMPOTENT and NON-DESTRUCTIVE: the family's own words stay in the file; a
/// stamp naming the plan they were folded into is inserted after the heading.
/// Running it again finds the stamp and leaves the file byte-identical, so a
/// dispatcher refire cannot double-stamp — and nothing is ever deleted, because
/// a side-channel note the family typed into is not the engine's to throw away.
/// The filename is left ALONE on purpose: every `plans/*.md` with a week-coded
/// name that is not a known sidecar suffix is a candidate plan to the surfaces
/// that read this directory, so a "retired" rename would become a phantom week.
///
/// Returns `Ok(true)` when it wrote, `Ok(false)` when there was nothing to do.
pub fn retire_sidecar(root: &Path, week_code: &str, plan_path: &Path) -> std::io::Result<bool> {
    let path = sidecar_path(root, week_code);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    if body.contains(FOLDED_MARKER) {
        return Ok(false);
    }
    let rel = plan_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("plans/{n}"))
        .unwrap_or_else(|| plan_path.display().to_string());
    let stamp = format!("{FOLDED_MARKER} {rel}");
    let mut out = String::with_capacity(body.len() + stamp.len() + 2);
    let mut stamped = false;
    for line in body.lines() {
        out.push_str(line);
        out.push('\n');
        if !stamped && line.trim_start().starts_with("# ") {
            out.push('\n');
            out.push_str(&stamp);
            out.push('\n');
            stamped = true;
        }
    }
    if !stamped {
        out.push_str(&stamp);
        out.push('\n');
    }
    crate::atomic_file::write_atomic(&path, out.as_bytes())?;
    Ok(true)
}

/// `2026-W31` from a filename stem like `2026-W31-family-plan`.
fn week_code_of_stem(stem: &str) -> Option<String> {
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

/// The ISO week code (`2026-W31`) and Monday/Sunday bounds for `day`.
pub fn iso_week_of(day: NaiveDate) -> (String, NaiveDate, NaiveDate) {
    let iso = day.iso_week();
    let monday = day - Duration::days(day.weekday().num_days_from_monday() as i64);
    let sunday = monday + Duration::days(6);
    (format!("{}-W{:02}", iso.year(), iso.week()), monday, sunday)
}

fn long_month(day: NaiveDate) -> &'static str {
    match day.month() {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        _ => "December",
    }
}

fn long_weekday(w: Weekday) -> &'static str {
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

fn short_weekday(w: Weekday) -> &'static str {
    &long_weekday(w)[..3]
}

fn day_cell(day: NaiveDate, style: DayCellStyle) -> String {
    match style {
        DayCellStyle::Short => format!(
            "{} {:02}-{:02}",
            short_weekday(day.weekday()),
            day.month(),
            day.day()
        ),
        DayCellStyle::Long => format!(
            "{} {} {}",
            long_weekday(day.weekday()),
            long_month(day),
            day.day()
        ),
    }
}

fn render_row(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

fn rule_row(width: usize) -> String {
    let mut cells = Vec::with_capacity(width);
    for _ in 0..width {
        cells.push("---".to_string());
    }
    render_row(&cells)
}

/// Build the markdown for a fresh week. Pure — no I/O, so the whole document can
/// be assembled, edited and verified before anything is written.
fn scaffold(week_code: &str, monday: NaiveDate, sunday: NaiveDate, shape: &PlanShape) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} · {} · Week of {} {} {} – {} {} {}\n\n",
        shape.title_prefix,
        week_code,
        long_weekday(monday.weekday()),
        long_month(monday),
        monday.day(),
        long_weekday(sunday.weekday()),
        long_month(sunday),
        sunday.day(),
    ));
    // The machine-readable bounds line. The human title above carries the same
    // dates in the family's words; this one is what the plan parser dates the
    // week from, so the drafted week is `covers(today)` immediately.
    out.push_str(&format!(
        "**Week of Monday {} to Sunday {}**\n",
        monday.format("%Y-%m-%d"),
        sunday.format("%Y-%m-%d"),
    ));
    out.push_str("**Status:** DRAFT\n\n");

    out.push_str(&format!("## {}\n\n", shape.meals_heading));
    let width = shape.meals_header.len().max(3);
    let mut header = shape.meals_header.clone();
    while header.len() < width {
        header.push(String::new());
    }
    out.push_str(&render_row(&header));
    out.push('\n');
    out.push_str(&rule_row(width));
    out.push('\n');
    for i in 0..7 {
        let day = monday + Duration::days(i);
        let mut cells = vec![day_cell(day, shape.day_style)];
        // An un-planned night is an empty cell, never a placeholder word: the
        // family reads this file, and "TBD" in a dinner column is a sentence the
        // house would have to explain.
        while cells.len() < width {
            cells.push(String::new());
        }
        out.push_str(&render_row(&cells));
        out.push('\n');
    }
    out.push('\n');

    out.push_str(&format!("## {}\n\n", shape.shopping_heading));
    for section in &shape.shopping_sections {
        out.push_str(&format!("### {section}\n\n"));
    }

    if let Some(cal) = &shape.calendar_heading {
        out.push_str(&format!("## {cal}\n\n"));
        let cwidth = shape.calendar_header.len().max(3);
        let mut cheader = shape.calendar_header.clone();
        while cheader.len() < cwidth {
            cheader.push(String::new());
        }
        out.push_str(&render_row(&cheader));
        out.push('\n');
        out.push_str(&rule_row(cwidth));
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Drafting
// ---------------------------------------------------------------------------

/// Does a plan of record already cover `day`?
fn plan_covering(root: &Path, day: NaiveDate) -> Option<String> {
    for doc in family_plan::load_plans(root) {
        if doc.covers(day) {
            return Some(doc.week_code);
        }
    }
    None
}

/// Draft THIS week's plan (the week containing `today`) with every carried
/// request applied, and write it under `root/plans/`.
///
/// Fails closed: the document is assembled, edited and round-tripped in memory,
/// and the file is created only when every carried request has provably landed.
/// After the write the file is re-read and re-parsed, so success is a claim about
/// bytes on disk rather than about control flow.
pub fn draft_week(
    root: &Path,
    today: NaiveDate,
    ask: &WeekStartAsk,
    calendar_owner: Option<&str>,
) -> Result<DraftedWeek, WeekStartError> {
    let (week_code, monday, sunday) = iso_week_of(today);

    // NEVER overwrite a week that already has a plan of record — an accepted
    // offer twice over, or a dispatcher refire, must not erase a filled-in week.
    if let Some(existing) = plan_covering(root, today) {
        return Err(WeekStartError::AlreadyPlanned {
            week_code: existing,
        });
    }
    let path = root.join("plans").join(format!("{week_code}-family-plan.md"));
    if path.exists() {
        return Err(WeekStartError::AlreadyPlanned { week_code });
    }

    let shape = newest_plan(root).map(|c| shape_of(&c)).unwrap_or_default();
    let mut content = scaffold(&week_code, monday, sunday, &shape);

    // FOLD IN THE PARKED DINNERS FIRST. Before this week had a plan, the "suggest
    // a dinner" affordance was the only place to put one, and everything the
    // family typed there went into the sidecar. Drafting the week WITHOUT them
    // silently throws that away — the family types a dinner, the week gets
    // started, and their dinner is gone. Carried requests are applied after, so
    // an explicit ask made NOW beats a suggestion parked earlier for the same
    // night.
    let mut parked: Vec<ParkedDinner> = Vec::new();
    for entry in parked_dinners(root, &week_code) {
        if entry.date < monday || entry.date > sunday {
            continue;
        }
        let op = FastLaneOp::MealSwap {
            day: entry.date.weekday(),
            dish: entry.dish.clone(),
        };
        match fast_lane::apply_to_content_with_calendar_owner(
            &week_code,
            &content,
            &op,
            calendar_owner,
        ) {
            Ok(edited) => {
                content = edited;
                parked.retain(|p| p.date != entry.date);
                parked.push(entry);
            }
            Err(e) => {
                return Err(WeekStartError::ParkedLost {
                    date: entry.date,
                    dish: entry.dish,
                    reason: e.to_string(),
                });
            }
        }
    }

    // Apply the carried requests to the DRAFT, in the family's own words, through
    // the same editors a live chat turn uses. Anything the closed set cannot
    // express aborts the draft — a week that quietly dropped the request is the
    // failure this whole module exists to prevent.
    let mut preserved: Vec<PreservedEdit> = Vec::new();
    for request in &ask.carried {
        let op = match fast_lane::classify(request, monday) {
            Classification::FastLane(op) => op,
            _ => {
                return Err(WeekStartError::CarriedNotUnderstood {
                    request: request.clone(),
                });
            }
        };
        // A carried request that is itself a week-start ask would recurse; the
        // closed set cannot produce one, but be explicit about the exclusion.
        if matches!(op, FastLaneOp::WeekStart { .. }) {
            return Err(WeekStartError::CarriedNotUnderstood {
                request: request.clone(),
            });
        }
        match fast_lane::apply_to_content_with_calendar_owner(
            &week_code,
            &content,
            &op,
            calendar_owner,
        ) {
            Ok(edited) => {
                content = edited;
                preserved.push(PreservedEdit {
                    request: request.clone(),
                    op_kind: op.kind_label().to_string(),
                    report: fast_lane::report_line(&op),
                });
            }
            Err(e) => {
                let reason = match &e {
                    FastLaneError::DayNotFound => "the drafted week has no row for that day".into(),
                    other => other.to_string(),
                };
                return Err(WeekStartError::CarriedLost {
                    request: request.clone(),
                    reason,
                });
            }
        }
    }

    crate::atomic_file::write_atomic(&path, content.as_bytes())
        .map_err(|e| WeekStartError::Io(format!("write {}: {e}", path.display())))?;

    // VERIFY FROM DISK. Everything above could be perfect and the file still be
    // absent, truncated, or unreadable by the parser the family's surfaces use.
    let written = std::fs::read_to_string(&path)
        .map_err(|e| WeekStartError::Io(format!("read back {}: {e}", path.display())))?;
    let doc = PlanDoc::parse(&week_code, &written);
    if !doc.covers(today) {
        return Err(WeekStartError::Unverified(format!(
            "{week_code} on disk does not cover {today}"
        )));
    }
    if doc.meals.len() < 7 {
        return Err(WeekStartError::Unverified(format!(
            "{week_code} on disk has {} day rows, expected 7",
            doc.meals.len()
        )));
    }
    for edit in &preserved {
        verify_preserved(&doc, edit)?;
    }
    // A parked dinner is proven the same way a carried one is — against the
    // BYTES, on the night it was parked for. Only requests applied later may
    // legitimately have replaced it.
    for entry in &parked {
        if preserved.iter().any(|e| replaced_night(e, entry.date, &doc)) {
            continue;
        }
        let landed = doc
            .meal_on(entry.date)
            .map(|m| m.dish.to_lowercase().contains(&entry.dish.to_lowercase()))
            .unwrap_or(false);
        if !landed {
            return Err(WeekStartError::ParkedLost {
                date: entry.date,
                dish: entry.dish.clone(),
                reason: "absent from the plan that was written".into(),
            });
        }
    }

    // The dinners are IN the week now. Stamp the side-channel so a later read
    // does not offer them a second time as if the week were still un-planned.
    // NON-FATAL: the plan above is proven on disk, and reporting "no week" over
    // a stamp that did not write would be the dishonest direction.
    let sidecar_retired = match retire_sidecar(root, &week_code, &path) {
        Ok(true) => Some(sidecar_path(root, &week_code)),
        _ => None,
    };

    Ok(DraftedWeek {
        week_code,
        path,
        start: monday,
        end: sunday,
        preserved,
        parked,
        sidecar_retired,
    })
}

/// Did a carried request legitimately take over `date`'s dinner? A parked
/// dinner it overwrote is not "lost" — it was superseded by an ask the family
/// made afterwards, and both are visible in the report.
fn replaced_night(edit: &PreservedEdit, date: NaiveDate, doc: &PlanDoc) -> bool {
    match fast_lane::classify(&edit.request, doc.start.unwrap_or(date)) {
        Classification::FastLane(FastLaneOp::MealSwap { day, .. }) => day == date.weekday(),
        _ => false,
    }
}

/// Prove one carried request survived into the document that is on disk.
fn verify_preserved(doc: &PlanDoc, edit: &PreservedEdit) -> Result<(), WeekStartError> {
    // Re-derive what to look for from the family's own words, so this check is
    // independent of the value the editor happened to write.
    let op = match fast_lane::classify(&edit.request, doc.start.unwrap_or_default()) {
        Classification::FastLane(op) => op,
        _ => {
            return Err(WeekStartError::CarriedLost {
                request: edit.request.clone(),
                reason: "no longer classifiable after the write".into(),
            });
        }
    };
    let present = match &op {
        FastLaneOp::MealSwap { day, dish } | FastLaneOp::MealAdd { day, addition: dish } => doc
            .meals
            .iter()
            .filter(|m| same_weekday(&m.weekday, *day))
            .any(|m| m.dish.to_lowercase().contains(&dish.to_lowercase())),
        FastLaneOp::ShoppingAdd { item } => doc
            .shopping
            .iter()
            .flat_map(|s| s.items.iter())
            .any(|it| it.to_lowercase().contains(&item.to_lowercase())),
        // A removal or a cancel against a week that did not exist a moment ago is
        // vacuous; the classifier above already refused to carry one.
        _ => true,
    };
    if present {
        Ok(())
    } else {
        Err(WeekStartError::CarriedLost {
            request: edit.request.clone(),
            reason: "absent from the plan that was written".into(),
        })
    }
}

/// Does a parsed day cell's weekday word name `want`? Accepts both the short
/// (`Tue`) and long (`Tuesday`) forms a household may write.
fn same_weekday(cell: &str, want: Weekday) -> bool {
    cell.eq_ignore_ascii_case(short_weekday(want)) || cell.eq_ignore_ascii_case(long_weekday(want))
}

/// The family-voice confirmation for a drafted week.
pub fn report_line(drafted: &DraftedWeek) -> String {
    let mut line = format!(
        "Done — this week's plan is started ({} to {}) 🗓️",
        drafted.start.format("%b %-d"),
        drafted.end.format("%b %-d"),
    );
    for edit in &drafted.preserved {
        line.push_str(&format!(" · {}", edit.report.trim_start_matches("Done — ")));
    }
    // Say what was carried across from the side-channel, by name. The family
    // typed those dinners in before the week existed; "your week is started" with
    // no mention of them reads as if they were lost even when they were not.
    if !drafted.parked.is_empty() {
        let kept: Vec<String> = drafted
            .parked
            .iter()
            .map(|p| format!("{} {}", p.date.format("%a"), p.dish))
            .collect();
        line.push_str(&format!(" · kept your dinners: {}", kept.join(", ")));
    }
    line
}

/// The honest line for a week that is already set up. Nothing is written.
pub fn already_planned_line() -> String {
    "This week's plan is already started — nothing to set up.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const W30: &str = "\
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

## 3. Calendar

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Tue 07-21 | 19:30 | PT check-in | planner |
";

    const MON_W31: fn() -> NaiveDate = || NaiveDate::from_ymd_opt(2026, 7, 27).unwrap();

    fn scratch(with_previous: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        if with_previous {
            std::fs::write(plans.join("2026-W30-family-plan.md"), W30).unwrap();
        }
        dir
    }

    // ── detection ───────────────────────────────────────────────────────────

    #[test]
    fn detects_the_dispatched_ask_and_its_quoted_carriage() {
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I just asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .expect("the dispatched week-start ask was not recognized");
        assert_eq!(ask.carried, vec!["Set Tuesday's dinner to homemade pizza."]);
    }

    #[test]
    fn detects_the_bare_ask_with_no_carriage() {
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        assert!(ask.carried.is_empty());
    }

    #[test]
    fn curly_quotes_carry_too() {
        let ask = detect(
            "Please draft this week\u{2019}s family plan. Keep what I asked for: \
             \u{201c}Set Tuesday\u{2019}s dinner to homemade pizza.\u{201d}",
        )
        .unwrap();
        assert_eq!(
            ask.carried,
            vec!["Set Tuesday\u{2019}s dinner to homemade pizza."]
        );
    }

    /// A QUESTION about the week is a read. Answering it by creating a plan of
    /// record would be a write on a read — the same class of bug the gateway half
    /// was built to remove.
    #[test]
    fn a_question_about_the_week_is_never_a_draft_instruction() {
        assert!(detect("Did you start the week?").is_none());
        assert!(detect("Has anyone set up this week yet?").is_none());
        assert!(detect("is this week planned? start the week maybe").is_none());
    }

    #[test]
    fn ordinary_chatter_is_not_a_week_start() {
        assert!(detect("Swap Friday to tacos").is_none());
        assert!(detect("yes").is_none());
        assert!(detect("How was the week?").is_none());
    }

    /// The instruction may not be smuggled in through the QUOTED carriage — the
    /// quote is the family's request, not a second instruction to the engine.
    #[test]
    fn a_quoted_start_phrase_alone_does_not_trigger() {
        assert!(detect("She said \"start the week\" earlier.").is_none());
    }

    // ── negation (P0) ───────────────────────────────────────────────────────

    /// THE P0. A negated ask names the lane in order to REFUSE it. The substring
    /// scan read "Don't start the week." as consent and drafted the plan — the
    /// one sentence that could not have been clearer about wanting no plan.
    #[test]
    fn a_negated_ask_never_drafts() {
        for msg in [
            "Don't start the week.",
            "Don\u{2019}t start the week.",
            "Do not start the week.",
            "Please don't set up this week.",
            "Never start the week without asking me first.",
            "Not yet — don't draft this week's plan.",
            "Cancel that, don't start the week.",
            "Stop — do not plan this week.",
            "Hold off, don't get this week started.",
            "I can't start the week myself, and you shouldn't either — do not start the week.",
        ] {
            assert!(
                detect(msg).is_none(),
                "a REFUSAL was read as consent and would draft a week: {msg:?}"
            );
            assert!(negated(msg), "the refusal was not reported as one: {msg:?}");
        }
    }

    /// A bare refusal in its own clause turns the whole message off, even when
    /// the clause that carries the start phrase reads clean on its own.
    #[test]
    fn a_standalone_refusal_clause_turns_the_whole_message_off() {
        for msg in [
            "Not yet, start the week later.",
            "Not this week. Plan this week when we're back.",
            "Never mind — set up this week another time.",
            "Cancel: start the week tomorrow instead.",
        ] {
            assert!(detect(msg).is_none(), "read as consent: {msg:?}");
            assert!(negated(msg), "not reported as a refusal: {msg:?}");
        }
    }

    /// THE FALSE-POSITIVE CONTROL. The guard scopes the negation to the clause
    /// that carries the start phrase, because "the week is NOT set up" is the
    /// usual REASON for a genuine ask. A guard that refused these would have
    /// broken the promise in the other direction, silently.
    #[test]
    fn a_negation_elsewhere_in_the_sentence_still_drafts() {
        for msg in [
            "This week is not set up yet — please start the week.",
            "There's no plan on the board, so start this week.",
            "I couldn't do it last night. Please draft this week's family plan.",
            "Don't worry about the shopping list; start the week.",
        ] {
            assert!(
                detect(msg).is_some(),
                "a genuine ask was refused by the negation guard: {msg:?}"
            );
            assert!(!negated(msg), "a genuine ask was reported negated: {msg:?}");
        }
    }

    /// The negation lives in the INSTRUCTION, not in the carriage: a family
    /// request that happens to contain "don't" is still carried into the draft.
    #[test]
    fn a_negation_inside_the_quoted_carriage_does_not_refuse_the_ask() {
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza, don't put fish on Tuesday.\"",
        )
        .expect("the carriage negated the instruction it was quoted inside");
        assert_eq!(ask.carried.len(), 1);
        assert!(ask.carried[0].contains("don't"));
    }

    /// `negated` speaks only about the week-start lane; ordinary chatter with a
    /// "don't" in it is not a refused week-start.
    #[test]
    fn negated_is_silent_about_messages_that_are_not_week_start_asks() {
        assert!(!negated("Don't put fish on Tuesday."));
        assert!(!negated("yes"));
        assert!(!negated("Did you start the week?"));
    }

    /// The refusal reaches the DRAFTING side too: a negated ask leaves the
    /// project byte-identical because it never becomes an ask at all.
    #[test]
    fn a_negated_ask_writes_no_plan_to_disk() {
        let dir = scratch(true);
        let before = std::fs::read_dir(dir.path().join("plans"))
            .unwrap()
            .count();
        assert!(detect("Don't start the week.").is_none());
        assert!(
            !dir.path()
                .join("plans")
                .join("2026-W31-family-plan.md")
                .exists(),
            "a refused week-start left a plan on disk",
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("plans")).unwrap().count(),
            before
        );
    }

    // ── drafting ────────────────────────────────────────────────────────────

    #[test]
    fn drafts_this_week_and_preserves_the_carried_edit() {
        let dir = scratch(true);
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I just asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, Some("planner")).unwrap();

        assert_eq!(out.week_code, "2026-W31");
        assert!(out.path.exists(), "no plan file was written");
        let written = std::fs::read_to_string(&out.path).unwrap();
        let doc = PlanDoc::parse("2026-W31", &written);
        assert!(doc.covers(MON_W31()), "the drafted week does not cover today");
        assert_eq!(doc.meals.len(), 7, "a week has seven nights");
        let tue = doc
            .meal_on(NaiveDate::from_ymd_opt(2026, 7, 28).unwrap())
            .expect("no Tuesday row");
        assert!(
            tue.dish.to_lowercase().contains("homemade pizza"),
            "the requested edit was dropped from the drafted week: {:?}",
            tue.dish
        );
        assert_eq!(out.preserved.len(), 1);
        assert_eq!(out.preserved[0].op_kind, "meal-swap");
    }

    /// The drafted week inherits the household's OWN structure — its headings,
    /// its columns, its day-cell style, its store sections.
    #[test]
    fn the_draft_inherits_the_households_shape() {
        let dir = scratch(true);
        let ask = detect("Please draft this week's family plan.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(written.contains("## 1. Dinners (planner → cook)"), "{written}");
        assert!(written.contains("## 4. Shopping list — by store"), "{written}");
        assert!(written.contains("### Greengrocer / produce"), "{written}");
        assert!(written.contains("| Day | Slot | Dinner | Prep |"), "{written}");
        assert!(written.contains("| Mon 07-27 |"), "{written}");
        // …and it never copies last week's CONTENT into this week.
        assert!(!written.contains("Chickpea curry"), "{written}");
        assert!(!written.contains("Chard"), "{written}");
        assert!(!written.contains("PT check-in"), "{written}");
    }

    /// A household writing long day cells gets long day cells back — otherwise
    /// the edit could not find its row and the week would be drafted without it.
    #[test]
    fn a_long_day_cell_household_keeps_long_day_cells() {
        let dir = scratch(false);
        std::fs::write(
            dir.path().join("plans").join("2026-W30-family-plan.md"),
            "# Family week · 2026-W30\n\n**Week of Monday 2026-07-20 to Sunday 2026-07-26**\n\
             **Status:** PUBLISHED\n\n## 1. Meals (planner → cook)\n\n\
             | Day | Kind | Dinner | Time at the stove |\n|---|---|---|---|\n\
             | Monday July 20 | Vegetarian | Chickpea curry | ~35 min |\n",
        )
        .unwrap();
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(written.contains("| Tuesday July 28 |"), "{written}");
        assert!(written.to_lowercase().contains("homemade pizza"), "{written}");
    }

    /// NO PREVIOUS PLAN — a brand new household still gets a real week.
    #[test]
    fn a_first_ever_week_drafts_from_the_default_shape() {
        let dir = scratch(false);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        assert_eq!(doc.meals.len(), 7);
        assert!(doc.covers(MON_W31()));
    }

    /// THE NEGATIVE THE GATE RESTS ON: a carried request the closed set cannot
    /// express must not produce a week that quietly dropped it. Nothing is
    /// written at all.
    #[test]
    fn a_carried_request_that_cannot_land_writes_no_plan() {
        let dir = scratch(true);
        let ask = WeekStartAsk {
            carried: vec!["Rebalance the whole week around the travel.".to_string()],
        };
        let err = draft_week(dir.path(), MON_W31(), &ask, None).unwrap_err();
        assert!(
            matches!(err, WeekStartError::CarriedNotUnderstood { .. }),
            "{err}"
        );
        assert!(
            !dir.path()
                .join("plans")
                .join("2026-W31-family-plan.md")
                .exists(),
            "a week was drafted WITHOUT the request the family carried into it",
        );
    }

    /// An existing week is never overwritten — and never partially edited.
    #[test]
    fn an_already_planned_week_is_left_byte_identical() {
        let dir = scratch(true);
        let path = dir.path().join("plans").join("2026-W31-family-plan.md");
        let existing = W30.replace("2026-W30", "2026-W31").replace("07-2", "07-2");
        let existing = existing
            .replace("Monday 2026-07-20", "Monday 2026-07-27")
            .replace("Sunday 2026-07-26", "Sunday 2026-08-02");
        std::fs::write(&path, &existing).unwrap();
        let ask = detect(
            "Please draft this week's family plan. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let err = draft_week(dir.path(), MON_W31(), &ask, None).unwrap_err();
        assert!(matches!(err, WeekStartError::AlreadyPlanned { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
    }

    /// Two carried requests both land, or neither does.
    #[test]
    fn several_carried_requests_all_land() {
        let dir = scratch(true);
        let ask = WeekStartAsk {
            carried: vec![
                "Set Tuesday's dinner to homemade pizza.".to_string(),
                "Add olive oil to the shopping list.".to_string(),
            ],
        };
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        let doc = PlanDoc::parse("2026-W31", &written);
        assert!(doc
            .meal_on(NaiveDate::from_ymd_opt(2026, 7, 28).unwrap())
            .map(|m| m.dish.to_lowercase().contains("homemade pizza"))
            .unwrap_or(false));
        assert!(doc
            .shopping
            .iter()
            .flat_map(|s| s.items.iter())
            .any(|i| i.to_lowercase().contains("olive oil")));
        assert_eq!(out.preserved.len(), 2);
    }

    // ── the parked-dinner sidecar (P0) ──────────────────────────────────────

    /// The EXACT bytes `weekAdapter._parkSuggestion` writes, for the week being
    /// drafted. Written by hand from the live note in `plans/` rather than
    /// invented, because a fixture in a format nothing produces proves nothing.
    const W31_SIDECAR: &str = "\
# Dinner suggestions for the week of 2026-W31

These are ideas the family added before the plan was drafted. Otto folds them
into the Sunday draft for this week.

- **Monday** (2026-07-27) — Mushroom risotto (Vegetarian, fits the Monday veg slot) · requested by a household member · recipe written in `plans/2026-W31-recipes.md`
- **Wednesday** (2026-07-29) — We are out, no dinner needed · suggested by a household member
- **Thursday** (2026-07-30) — Pasta al pomodoro · suggested by a household member
";

    fn park(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("plans").join("2026-W31-dinner-suggestions.md");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// THE P0. A sidecar sorts NEWEST by week code, so shape discovery read a
    /// bullet list of parked dinners as "the household's most recent plan" — and
    /// a week drafted in that shape has no meals table at all.
    #[test]
    fn a_sidecar_is_never_mistaken_for_the_households_shape() {
        let dir = scratch(true);
        park(dir.path(), W31_SIDECAR);
        let shape = newest_plan(dir.path()).map(|c| shape_of(&c)).unwrap();
        assert_eq!(
            shape.meals_heading, "1. Dinners (planner → cook)",
            "shape discovery read the parked-dinner sidecar as the plan",
        );
        assert_eq!(shape.shopping_sections, vec!["Greengrocer / produce"]);
    }

    /// LIVE SHAPE. The household's real `plans/` holds recipe cards, workout
    /// notes and a check-in beside the plan, every one of them week-coded. The
    /// plan of record must still be the shape a new week inherits.
    #[test]
    fn companions_beside_the_plan_never_supply_the_shape() {
        let dir = scratch(true);
        let plans = dir.path().join("plans");
        park(dir.path(), W31_SIDECAR);
        std::fs::write(
            plans.join("2026-W31-cook-recipes.md"),
            "# Recipes for 2026-W31\n\n## Mushroom risotto\n\n- Arborio rice\n",
        )
        .unwrap();
        std::fs::write(
            plans.join("2026-W31-workouts.md"),
            "# Workouts 2026-W31\n\n## Monday\n\n- 5k\n",
        )
        .unwrap();
        let shape = newest_plan(dir.path()).map(|c| shape_of(&c)).unwrap();
        assert_eq!(shape.meals_heading, "1. Dinners (planner → cook)");
        assert!(shape.meals_header.contains(&"Prep".to_string()), "{shape:?}");
    }

    #[test]
    fn parked_lines_parse_with_their_provenance_peeled() {
        let parked = parse_parked(W31_SIDECAR);
        assert_eq!(parked.len(), 3);
        assert_eq!(parked[0].date, NaiveDate::from_ymd_opt(2026, 7, 27).unwrap());
        assert_eq!(
            parked[0].dish,
            "Mushroom risotto (Vegetarian, fits the Monday veg slot)",
            "the provenance tail leaked into the dish",
        );
        assert_eq!(parked[1].dish, "We are out, no dinner needed");
        assert_eq!(parked[2].dish, "Pasta al pomodoro");
    }

    /// A night typed twice keeps the LAST one — the same rule the surface the
    /// family typed into applies, so the plan matches what they last saw.
    #[test]
    fn a_re_parked_night_keeps_the_last_dinner() {
        let parked = parse_parked(
            "- **Tuesday** (2026-07-28) — Homemade pizza · suggested by a household member\n\
             - **Tuesday** (2026-07-28) — Lentil soup · suggested by a household member\n",
        );
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].dish, "Lentil soup");
    }

    /// THE BUG, END TO END: the dinners the family parked BEFORE the week
    /// existed are IN the week that gets drafted — read back from disk, on their
    /// own nights.
    #[test]
    fn parked_dinners_land_in_the_drafted_week() {
        let dir = scratch(true);
        park(dir.path(), W31_SIDECAR);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();

        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        let on = |d: u32| {
            doc.meal_on(NaiveDate::from_ymd_opt(2026, 7, d).unwrap())
                .map(|m| m.dish.to_lowercase())
                .unwrap_or_default()
        };
        assert!(on(27).contains("mushroom risotto"), "Monday: {:?}", on(27));
        assert!(on(29).contains("no dinner needed"), "Wednesday: {:?}", on(29));
        assert!(on(30).contains("pasta al pomodoro"), "Thursday: {:?}", on(30));
        assert_eq!(out.parked.len(), 3);
        let line = report_line(&out);
        assert!(line.to_lowercase().contains("risotto"), "{line}");
    }

    /// A dinner parked for a night OUTSIDE the week being drafted belongs to
    /// that other week and is left where it is.
    #[test]
    fn a_dinner_parked_for_another_week_is_not_pulled_in() {
        let dir = scratch(true);
        park(
            dir.path(),
            "# Dinner suggestions for the week of 2026-W31\n\n\
             - **Monday** (2026-08-03) — Next week's chili · suggested by a household member\n",
        );
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        assert!(out.parked.is_empty());
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(!written.to_lowercase().contains("chili"), "{written}");
    }

    /// A request carried into the ask NOW beats a dinner parked for that night
    /// earlier — and the parked one is not reported as lost.
    #[test]
    fn a_carried_request_supersedes_a_parked_dinner_for_the_same_night() {
        let dir = scratch(true);
        park(
            dir.path(),
            "# Dinner suggestions for the week of 2026-W31\n\n\
             - **Tuesday** (2026-07-28) — Lentil soup · suggested by a household member\n",
        );
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        let tue = doc
            .meal_on(NaiveDate::from_ymd_opt(2026, 7, 28).unwrap())
            .unwrap();
        assert!(tue.dish.to_lowercase().contains("homemade pizza"), "{tue:?}");
        assert!(!tue.dish.to_lowercase().contains("lentil"), "{tue:?}");
    }

    /// RETIREMENT IS IDEMPOTENT and never destroys the family's words: the
    /// stamp lands once, a second run is byte-identical, and every parked line
    /// is still readable in the file.
    #[test]
    fn sidecar_retirement_stamps_once_and_keeps_every_line() {
        let dir = scratch(true);
        let sidecar = park(dir.path(), W31_SIDECAR);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        assert_eq!(out.sidecar_retired.as_deref(), Some(sidecar.as_path()));

        let stamped = std::fs::read_to_string(&sidecar).unwrap();
        assert!(
            stamped.contains("**Folded into:** plans/2026-W31-family-plan.md"),
            "{stamped}"
        );
        assert!(stamped.contains("Pasta al pomodoro"), "{stamped}");
        assert_eq!(stamped.matches(FOLDED_MARKER).count(), 1, "{stamped}");

        // Retiring again — the dispatcher refire — changes nothing at all.
        assert!(!retire_sidecar(dir.path(), "2026-W31", &out.path).unwrap());
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), stamped);
    }

    /// A refused draft leaves the sidecar untouched: nothing is stamped as
    /// folded into a plan that was never written.
    #[test]
    fn an_abandoned_draft_never_stamps_the_sidecar() {
        let dir = scratch(true);
        let sidecar = park(dir.path(), W31_SIDECAR);
        let before = std::fs::read_to_string(&sidecar).unwrap();
        let ask = WeekStartAsk {
            carried: vec!["Rebalance the whole week around the travel.".to_string()],
        };
        assert!(draft_week(dir.path(), MON_W31(), &ask, None).is_err());
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), before);
    }

    #[test]
    fn a_week_with_no_sidecar_drafts_and_retires_nothing() {
        let dir = scratch(true);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        assert!(out.parked.is_empty());
        assert!(out.sidecar_retired.is_none());
    }

    #[test]
    fn the_report_names_the_week_and_what_it_kept() {
        let dir = scratch(true);
        let ask = detect(
            "Please draft this week's family plan. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let line = report_line(&out);
        assert!(line.to_lowercase().contains("started"), "{line}");
        assert!(line.to_lowercase().contains("pizza"), "{line}");
    }
}
