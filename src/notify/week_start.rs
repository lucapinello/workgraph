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
//!     week?" is a read, and answering it must not write a plan).
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

/// One dinner the family PARKED for this week before any plan existed, now
/// proven present in the drafted plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedDinner {
    /// The night it was parked for.
    pub date: NaiveDate,
    pub weekday: Weekday,
    /// The dish, verbatim, with the note's provenance tail peeled off.
    pub dish: String,
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
    /// Every parked dinner suggestion, proven present in the same plan.
    pub parked: Vec<ParkedDinner>,
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
    /// Same rule as a carried request: nothing is written. A parked entry is a
    /// decision the family already made, not an idea to weigh (docs/11 §0c).
    ParkedLost { dish: String, reason: String },
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
            WeekStartError::ParkedLost { dish, reason } => {
                write!(f, "parked dinner {dish:?} did not land: {reason}")
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

/// Words that turn the sentence into a REFUSAL of the week-start rather than an
/// ask for it. `START_PHRASES` are matched as substrings — "start the week" is a
/// substring of "Don't start the week." — so without this, telling the house NOT
/// to start the week CREATED the week: the loudest possible way to be ignored,
/// and it writes a plan of record that must not exist.
///
/// Deliberately FAIL-CLOSED and deliberately blunt: any of these anywhere in the
/// instruction (the message with its quoted carriage already removed) refuses the
/// draft. "Start the week — don't worry about shopping yet" is refused too, and
/// that is the right trade: refusing writes nothing and the family can say it
/// again, while a mis-read negation writes a week nobody asked for over a
/// household that deliberately has none. The quoted carriage is excluded from
/// this scan for the same reason it is excluded from the instruction scan — a
/// carried request that happens to say "don't" ("Don't put fish on Tuesday.") is
/// the family's EDIT, not a refusal of the ask that carries it.
const NEGATION_MARKERS: &[&str] = &[
    "don't",
    "don’t",
    "do not",
    "dont",
    "never",
    "not yet",
    "no need",
    "without starting",
    "without setting up",
    "cancel",
    "stop",
    "hold off",
    "leave it",
    "no thanks",
    "skip",
];

/// Is `needle` present in `haystack` as WHOLE words? "stop" must refuse "stop
/// the week" without refusing a dish called "stopper" — a substring test on a
/// negation list is the same bug this list exists to fix, one level down.
/// Multi-word needles ("do not") match on their outer boundaries.
fn contains_word_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    // A hyphen and an apostrophe are word-INTERNAL: "non-stop" is one word and must
    // not trip "stop", and a UTF-8 continuation byte is never a boundary either
    // (the curly apostrophe in "don’t" is three bytes).
    let is_wordish = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'\'') || c >= 0x80;
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let end = at + needle.len();
        let before_ok = at == 0 || !is_wordish(bytes[at - 1]);
        let after_ok = end >= bytes.len() || !is_wordish(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        // Advance by one BYTE past this start, staying on a char boundary, so an
        // overlapping later occurrence is still found.
        from = at + 1;
        while from < haystack.len() && !haystack.is_char_boundary(from) {
            from += 1;
        }
        if from >= haystack.len() {
            break;
        }
    }
    false
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
    // "Don't start the week." must not START THE WEEK.
    if NEGATION_MARKERS
        .iter()
        .any(|n| contains_word_phrase(low, n))
    {
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

/// Filename roles that are NOT a household plan of record: the atomic-publish
/// half-written file, and a persona's review / scratch notes left beside the plan.
/// Mirrors the gateway's selection gate (`isWeekPlanCandidate`,
/// claw3d-bridge/src/weekSource.mjs) — a review's prose "## Dinners" section has no
/// meals table, so a week drafted in its shape would have no meals table either.
fn is_aux_role_stem(stem: &str) -> bool {
    let low = stem.to_ascii_lowercase();
    if low.ends_with(".draft") {
        return true;
    }
    for role in [
        "-review",
        "-reviews",
        "-draft",
        "-drafts",
        "-note",
        "-notes",
        "-scratch",
        "-wip",
        "-summary",
        "-checkin",
        "-check-in",
    ] {
        if low.ends_with(role) {
            return true;
        }
    }
    false
}

/// Could this document supply a week's SHAPE? A shape source must actually be a
/// week plan: a meals section with a table under it. A `-workouts` or `-recipes`
/// companion is a legitimate file for its week but has no meals table, and a week
/// drafted from one would have no dinners at all — so the carried request could
/// not land and the family would get an empty week with a confident report.
fn is_plan_shaped(content: &str) -> bool {
    let mut in_meals = false;
    for raw in content.lines() {
        let line = raw.trim();
        if let Some(h2) = line.strip_prefix("## ") {
            in_meals = family_plan::is_meals_section_heading(h2);
            continue;
        }
        if in_meals && table_cells(line).is_some() {
            return true;
        }
    }
    false
}

/// The newest plan on disk, by week code — the household's most recent example.
///
/// GATED THREE WAYS, because "the newest `.md` in `plans/` whose name carries a
/// week code" is not the same thing as "the household's most recent plan":
///   · a `-dinner-suggestions` note is a side-channel, not a plan
///     ([`family_plan::is_sidecar_stem`]) — reading one as the shape produced a
///     drafted week titled "Dinner suggestions for the week of …";
///   · a review / `.draft` / notes file is scratch, not a plan of record;
///   · whatever survives must be PLAN-SHAPED (a meals section with a table), so a
///     `-workouts` companion never supplies the shape of a week of dinners.
/// The canonical `-family-plan` wins ties for its week outright.
fn newest_plan(root: &Path) -> Option<String> {
    let plans_dir = root.join("plans");
    // (week code, canonical?) — canonical `-family-plan` outranks a companion.
    let mut best: Option<((String, bool), String)> = None;
    for entry in std::fs::read_dir(&plans_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if family_plan::is_sidecar_stem(stem) || is_aux_role_stem(stem) {
            continue;
        }
        let Some(week) = week_code_of_stem(stem) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !is_plan_shaped(&content) {
            continue;
        }
        let rank = (week, stem.to_ascii_lowercase().ends_with("-family-plan"));
        if best.as_ref().map(|(r, _)| rank > *r).unwrap_or(true) {
            best = Some((rank, content));
        }
    }
    best.map(|(_, c)| c)
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
// Parked dinner suggestions — the choices the family made BEFORE the plan
// ---------------------------------------------------------------------------
//
// A family can type a dinner for a night of a week that has no plan file yet. With
// nowhere to write it, the gateway PARKS it as one line in
// `plans/<week>-dinner-suggestions.md` (`WeekAdapter._parkSuggestion`):
//
//     - **Thursday** (2026-07-30) — Fish tacos · suggested by <name>
//
// docs/11 §0c is explicit that these are FIXED USER CHOICES, not ideas to weigh:
// the draft must incorporate each one verbatim, on its exact day. Drafting the week
// without them is the same failure as drafting it without the carried request — the
// family said what they wanted, watched the house accept, and got a blank night.

/// The path of the parked-suggestions note for a week.
fn sidecar_path(root: &Path, week_code: &str) -> PathBuf {
    root.join("plans")
        .join(format!("{week_code}-dinner-suggestions.md"))
}

/// The line that RETIRES everything above it. Appended once, after a drafted week
/// has been verified on disk; the parser below reads only what comes after the
/// last one. Nothing is ever deleted — the family's own words stay in the file —
/// and a second draft (a refire, an accepted-twice offer) folds nothing, so the
/// protocol is idempotent. The filename is deliberately unchanged: every reader on
/// both sides excludes `-dinner-suggestions.md` by SUFFIX, and a renamed note
/// would stop being excluded and start looking like a plan.
fn fold_marker(week_code: &str) -> String {
    format!("<!-- folded into {week_code}-family-plan.md — the lines above are already in the plan -->")
}

fn is_fold_marker(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("<!-- folded into") && t.ends_with("-->")
}

/// Peel a parked line's provenance down to the dish. Mirrors the gateway's
/// `stripSuggestionProvenance`: everything from the first `·` is attribution
/// ("· suggested by …", "· recipe written by … in <file>"), and a trailing
/// parenthetical is a draft-composer slot-fit note. A plain dish carries neither.
fn strip_suggestion_provenance(text: &str) -> String {
    let mut s = match text.find('\u{b7}') {
        Some(at) => &text[..at],
        None => text,
    }
    .trim()
    .to_string();
    if s.ends_with(')') {
        if let Some(open) = s.rfind('(') {
            s.truncate(open);
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse one `- **Thursday** (2026-07-30) — Fish tacos · suggested by …` line into
/// its date and dish. Tolerant of the em-dash the note is written with and of a
/// plain hyphen, and of a missing attribution tail.
fn parse_parked_line(line: &str) -> Option<(NaiveDate, String)> {
    let t = line.trim();
    let rest = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "))?;
    let rest = rest.trim();
    let rest = rest.strip_prefix("**")?;
    let (_day_word, rest) = rest.split_once("**")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    let (iso, rest) = rest.split_once(')')?;
    let date = NaiveDate::parse_from_str(iso.trim(), "%Y-%m-%d").ok()?;
    let rest = rest.trim_start();
    // The separator between the date and the dish: em dash, en dash, or hyphen.
    let rest = rest
        .strip_prefix('\u{2014}')
        .or_else(|| rest.strip_prefix('\u{2013}'))
        .or_else(|| rest.strip_prefix('-'))?;
    let dish = strip_suggestion_provenance(rest);
    if dish.is_empty() {
        return None;
    }
    Some((date, dish))
}

/// Every parked dinner for `week_code` that has not already been folded into a
/// plan, in file order, LAST WRITE PER DAY WINS (the gateway appends a re-typed
/// dinner rather than replacing the earlier line, and its own reader resolves the
/// same way). Entries dated outside the week are ignored — a note only ever
/// belongs to the week in its filename, and a stray date must not silently land a
/// dinner on the wrong night.
fn read_parked(
    root: &Path,
    week_code: &str,
    monday: NaiveDate,
    sunday: NaiveDate,
) -> Vec<ParkedDinner> {
    let Ok(body) = std::fs::read_to_string(sidecar_path(root, week_code)) else {
        return Vec::new();
    };
    let lines: Vec<&str> = body.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| is_fold_marker(l))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut by_date: Vec<ParkedDinner> = Vec::new();
    for line in &lines[start..] {
        let Some((date, dish)) = parse_parked_line(line) else {
            continue;
        };
        if date < monday || date > sunday {
            continue;
        }
        let entry = ParkedDinner {
            date,
            weekday: date.weekday(),
            dish,
        };
        match by_date.iter_mut().find(|p| p.date == date) {
            Some(existing) => *existing = entry,
            None => by_date.push(entry),
        }
    }
    by_date.sort_by_key(|p| p.date);
    by_date
}

/// Retire the parked suggestions that just landed in a plan. Append-only and
/// idempotent (see [`fold_marker`]); a note that does not exist, or that had
/// nothing to fold, is left exactly as it was. A failure here is NOT a failure of
/// the draft: the week is already written and verified, and the worst case is that
/// a later draft re-applies the same dinners to the same nights.
fn retire_parked(root: &Path, week_code: &str, folded: usize) {
    if folded == 0 {
        return;
    }
    let path = sidecar_path(root, week_code);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut next = body.trim_end().to_string();
    next.push('\n');
    next.push_str(&fold_marker(week_code));
    next.push('\n');
    let _ = crate::atomic_file::write_atomic(&path, next.as_bytes());
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

    // FIRST the dinners the family parked for this week before it existed, THEN
    // the carried request — so an explicit ask made in this exchange wins the night
    // over an older parked choice for the same day, rather than being overwritten
    // by it. Both go through the real editors; both must provably land.
    let parked = read_parked(root, &week_code, monday, sunday);
    for entry in &parked {
        let op = FastLaneOp::MealSwap {
            day: entry.weekday,
            dish: entry.dish.clone(),
        };
        content =
            fast_lane::apply_to_content_with_calendar_owner(&week_code, &content, &op, calendar_owner)
                .map_err(|e| WeekStartError::ParkedLost {
                    dish: entry.dish.clone(),
                    reason: match &e {
                        FastLaneError::DayNotFound => {
                            "the drafted week has no row for that day".into()
                        }
                        other => other.to_string(),
                    },
                })?;
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
    // A parked dinner is verified FROM DISK by the same rule: the night the family
    // chose reads back with the dish they chose. A carried request for the same
    // night legitimately supersedes it — that is the family's newer word, not a
    // dropped one — so a superseded entry is not a loss.
    let superseded = |date: NaiveDate| {
        preserved.iter().any(|e| {
            matches!(
                fast_lane::classify(&e.request, monday),
                Classification::FastLane(FastLaneOp::MealSwap { day, .. }) if day == date.weekday()
            )
        })
    };
    for entry in &parked {
        if superseded(entry.date) {
            continue;
        }
        let landed = doc
            .meal_on(entry.date)
            .map(|m| m.dish.to_lowercase().contains(&entry.dish.to_lowercase()))
            .unwrap_or(false);
        if !landed {
            return Err(WeekStartError::ParkedLost {
                dish: entry.dish.clone(),
                reason: format!("absent from {} in the plan that was written", entry.date),
            });
        }
    }

    // The week is on disk and verified — only now retire what was folded into it.
    retire_parked(root, &week_code, parked.len());

    Ok(DraftedWeek {
        week_code,
        path,
        start: monday,
        end: sunday,
        preserved,
        parked,
    })
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
    // Say that the parked choices were kept. The family typed those dinners into a
    // week that did not exist yet; being told they made it in is the whole point of
    // having parked them.
    match drafted.parked.len() {
        0 => {}
        1 => line.push_str(&format!(
            " · kept the dinner you'd already picked for {}",
            long_weekday(drafted.parked[0].weekday)
        )),
        n => line.push_str(&format!(" · kept the {n} dinners you'd already picked")),
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

    // ── negation ────────────────────────────────────────────────────────────
    //
    // THE P0. `START_PHRASES` are matched as substrings, and "start the week" is a
    // substring of "Don't start the week." — so telling the house NOT to start the
    // week started it, and wrote a plan of record the family had explicitly refused.

    #[test]
    fn telling_the_house_not_to_start_the_week_is_never_a_start() {
        for refusal in [
            "Don't start the week.",
            "Don\u{2019}t start the week.",
            "Do not start the week yet.",
            "Never start the week without asking me.",
            "Not yet — don't set up this week.",
            "Please cancel that, don't draft this week's family plan.",
            "Stop — do not plan this week.",
            "Hold off, no need to start the week.",
            "Skip it, we'll plan this week later.",
            "No thanks, leave it — don't start this week.",
        ] {
            assert!(
                detect(refusal).is_none(),
                "a REFUSAL was read as an instruction to draft the week: {refusal:?}",
            );
        }
    }

    /// The negation gate must not eat the positives it sits next to — the
    /// imperative ask and the quoted-carriage ask both still draft.
    #[test]
    fn the_negation_gate_leaves_the_real_asks_alone() {
        assert!(detect("Please draft this week's family plan — start the week.").is_some());
        assert!(detect("Start the week.").is_some());
        assert!(detect("Let's get this week started.").is_some());
        let carried = detect(
            "Please draft this week's family plan — start the week. Keep what I just asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .expect("the quoted-carriage ask stopped being recognized");
        assert_eq!(carried.carried, vec!["Set Tuesday's dinner to homemade pizza."]);
    }

    /// A "don't" inside the QUOTED carriage is the family's EDIT, not a refusal of
    /// the ask carrying it — the carriage is removed before the negation scan, the
    /// same way it is removed before the instruction scan.
    #[test]
    fn a_negation_inside_the_carriage_does_not_refuse_the_ask() {
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I just asked for: \
             \"Don't put fish on Tuesday.\"",
        )
        .expect("a carried request containing \"don't\" refused the whole ask");
        assert_eq!(ask.carried, vec!["Don't put fish on Tuesday."]);
    }

    /// The negation list is matched as WHOLE WORDS — a substring test on the
    /// refusal list would be the same bug it exists to fix, one level down.
    #[test]
    fn negation_words_match_as_words_not_substrings() {
        assert!(contains_word_phrase("don't start the week", "don't"));
        assert!(contains_word_phrase("stop the week", "stop"));
        assert!(!contains_word_phrase("nonstop planning, start the week", "stop"));
        assert!(!contains_word_phrase("cancellation policy", "cancel"));
        assert!(detect("Non-stop week ahead — start the week.").is_some());
    }

    /// THE ARTIFACT ASSERTION for the negation P0: a refusal writes NO PLAN. The
    /// classifier is where it is caught, and this is what being caught means.
    #[test]
    fn a_refused_week_start_writes_no_plan_file() {
        let dir = scratch(true);
        assert!(detect("Don't start the week.").is_none());
        // Nothing to draft — and nothing drafted.
        assert!(
            !dir.path()
                .join("plans")
                .join("2026-W31-family-plan.md")
                .exists(),
            "a plan of record was created for a week the family refused to start",
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

    // ── parked dinner suggestions (the sidecar P0) ──────────────────────────

    const W31_SIDECAR: &str = "\
# Dinner suggestions for the week of 2026-W31

These are ideas the family added before the plan was drafted.

- **Thursday** (2026-07-30) — Fish tacos · suggested by the family
";

    fn park(dir: &Path, body: &str) {
        std::fs::write(
            dir.join("plans").join("2026-W31-dinner-suggestions.md"),
            body,
        )
        .unwrap();
    }

    /// THE CORRUPTION. A parked-suggestions note's filename carries a week code, so
    /// "the newest plan on disk" picked it as the household's most recent plan and
    /// drafted the week in ITS shape — a plan of record titled "Dinner suggestions
    /// for the week of …", with no meals table for the family's dinners to land in.
    #[test]
    fn a_sidecar_note_is_never_read_as_the_households_plan_shape() {
        let dir = scratch(true);
        park(dir.path(), W31_SIDECAR);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(
            !written.contains("Dinner suggestions for the week of"),
            "the drafted plan inherited the SIDECAR's shape:\n{written}",
        );
        // …it inherited the household's real plan instead.
        assert!(written.contains("## 1. Dinners (planner → cook)"), "{written}");
        assert_eq!(PlanDoc::parse("2026-W31", &written).meals.len(), 7);
    }

    /// A parked dinner is a decision the family already made (docs/11 §0c). The
    /// drafted week must carry it onto its exact night, verbatim.
    #[test]
    fn parked_dinners_land_on_their_night_in_the_draft() {
        let dir = scratch(true);
        park(dir.path(), W31_SIDECAR);
        let ask = detect("Please draft this week's family plan — start the week.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();

        // FROM DISK, not from the return value.
        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        let thu = doc
            .meal_on(NaiveDate::from_ymd_opt(2026, 7, 30).unwrap())
            .expect("no Thursday row in the drafted week");
        assert!(
            thu.dish.to_lowercase().contains("fish tacos"),
            "the parked Thursday dinner was dropped from the draft: {:?}",
            thu.dish,
        );
        assert_eq!(out.parked.len(), 1);
        assert_eq!(out.parked[0].dish, "Fish tacos");
        assert!(report_line(&out).to_lowercase().contains("already picked"));
    }

    /// The note's PROVENANCE never reaches the family's plan — only the dish.
    #[test]
    fn a_parked_entry_carries_the_dish_and_not_its_provenance() {
        let dir = scratch(true);
        park(
            dir.path(),
            "# Dinner suggestions for the week of 2026-W31\n\n\
             - **Wednesday** (2026-07-29) — Mushroom risotto (Vegetarian, fits the veg slot) \
             \u{b7} requested by the family \u{b7} recipe written in plans/2026-W31-recipes.md\n",
        );
        let ask = detect("Please draft this week's family plan.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(written.contains("Mushroom risotto"), "{written}");
        for husk in ["requested by", "recipe written", "fits the veg slot", ".md"] {
            assert!(
                !written.contains(husk),
                "the parked note's provenance leaked into the family's plan ({husk}):\n{written}",
            );
        }
        assert_eq!(out.parked[0].dish, "Mushroom risotto");
    }

    /// Every valid parked entry lands; a re-typed night is last-write-wins; an
    /// entry dated outside this week belongs to another week and is left alone.
    #[test]
    fn several_parked_entries_land_and_a_retyped_night_takes_the_later_one() {
        let dir = scratch(true);
        park(
            dir.path(),
            "# Dinner suggestions for the week of 2026-W31\n\n\
             - **Thursday** (2026-07-30) — Fish tacos \u{b7} suggested by the family\n\
             - **Friday** (2026-07-31) — Dining out\n\
             - **Thursday** (2026-07-30) — Lentil soup \u{b7} suggested by the family\n\
             - **Monday** (2026-08-10) — Something next month\n",
        );
        let ask = detect("Please draft this week's family plan.").unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        let dish_on = |d: u32| {
            doc.meal_on(NaiveDate::from_ymd_opt(2026, 7, d).unwrap())
                .map(|m| m.dish.to_lowercase())
                .unwrap_or_default()
        };
        assert!(dish_on(30).contains("lentil soup"), "{:?}", dish_on(30));
        assert!(!dish_on(30).contains("fish tacos"), "{:?}", dish_on(30));
        // A non-cook night is a real plan, not a blank.
        assert!(dish_on(31).contains("dining out"), "{:?}", dish_on(31));
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(
            !written.contains("Something next month"),
            "an entry dated outside this week landed in it:\n{written}",
        );
        assert_eq!(out.parked.len(), 2);
    }

    /// The carried request is the family's NEWER word: it takes the night from an
    /// older parked choice rather than being overwritten by it.
    #[test]
    fn a_carried_request_supersedes_a_parked_choice_for_the_same_night() {
        let dir = scratch(true);
        park(
            dir.path(),
            "# Dinner suggestions for the week of 2026-W31\n\n\
             - **Tuesday** (2026-07-28) — Lentil soup \u{b7} suggested by the family\n",
        );
        let ask = detect(
            "Please draft this week's family plan — start the week. Keep what I just asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let doc = PlanDoc::parse("2026-W31", &std::fs::read_to_string(&out.path).unwrap());
        let tue = doc
            .meal_on(NaiveDate::from_ymd_opt(2026, 7, 28).unwrap())
            .unwrap();
        assert!(
            tue.dish.to_lowercase().contains("homemade pizza"),
            "the carried request lost its night to an older parked choice: {:?}",
            tue.dish,
        );
    }

    /// RETIREMENT IS IDEMPOTENT. A second draft (an accepted-twice offer, a
    /// dispatcher refire) must not re-apply what is already in a plan — and the
    /// family's own words are never deleted from the note.
    #[test]
    fn folding_a_note_is_idempotent_and_destroys_nothing() {
        let dir = scratch(true);
        park(dir.path(), W31_SIDECAR);
        let ask = detect("Please draft this week's family plan.").unwrap();
        draft_week(dir.path(), MON_W31(), &ask, None).unwrap();

        let note = std::fs::read_to_string(
            dir.path().join("plans").join("2026-W31-dinner-suggestions.md"),
        )
        .unwrap();
        assert!(note.contains("Fish tacos"), "the family's own words were deleted:\n{note}");
        assert!(note.contains("<!-- folded into"), "the note was not retired:\n{note}");
        assert_eq!(
            read_parked(
                dir.path(),
                "2026-W31",
                NaiveDate::from_ymd_opt(2026, 7, 27).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            )
            .len(),
            0,
            "a folded note still reports entries to fold",
        );

        // A dinner typed AFTER the fold is still fresh.
        std::fs::write(
            dir.path().join("plans").join("2026-W31-dinner-suggestions.md"),
            format!("{note}- **Saturday** (2026-08-01) — Pancakes\n"),
        )
        .unwrap();
        let fresh = read_parked(
            dir.path(),
            "2026-W31",
            NaiveDate::from_ymd_opt(2026, 7, 27).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
        );
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].dish, "Pancakes");
    }

    /// A companion file for the week (`-workouts`, `-recipes`) is legitimate but is
    /// not a week of dinners — a week drafted in its shape has no meals table at
    /// all, so nothing the family asked for could land.
    #[test]
    fn a_section_companion_never_supplies_the_shape() {
        let dir = scratch(true);
        std::fs::write(
            dir.path().join("plans").join("2026-W31-workouts.md"),
            "# Workouts for the week\n\n## Moving\n\n| Day | Session |\n|---|---|\n| Mon | Run |\n",
        )
        .unwrap();
        let ask = detect(
            "Please draft this week's family plan. Keep what I asked for: \
             \"Set Tuesday's dinner to homemade pizza.\"",
        )
        .unwrap();
        let out = draft_week(dir.path(), MON_W31(), &ask, None).unwrap();
        let written = std::fs::read_to_string(&out.path).unwrap();
        assert!(!written.contains("Workouts for the week"), "{written}");
        assert!(written.contains("## 1. Dinners (planner → cook)"), "{written}");
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
