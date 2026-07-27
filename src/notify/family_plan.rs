//! Parser for the family weekly-plan markdown files (`plans/*.md`).
//!
//! The family team drafts one plan per ISO week as a markdown document (see
//! `plans/2026-W29-family-plan.md`). The Telegram family commands
//! (`/dinner`, `/shopping`, `/week`) answer from these files — never from
//! placeholders — so this module turns a plan's prose+tables into structured
//! data a compose function can read.
//!
//! # What it extracts
//!
//! * the week's date range (`**Week of Monday 2026-07-13 → Sunday 2026-07-19**`)
//!   and its publish `Status`,
//! * the **dinners** table (`## 1. Dinners (…)`) as one [`Meal`] per day
//!   (`## 1. Meals` remains a supported legacy alias),
//! * the **shopping list** (`## 4. Shopping list`) as [`ShoppingSection`]s
//!   (one per `###` store heading) with their bullet items,
//! * the **workouts** (`## 2. Workouts`) as [`WorkoutDay`]s per person.
//!
//! Parsing is deliberately tolerant: headings are matched by their leading
//! `## N. <keyword>` / `### <text>` shape (a leading emoji on a store heading is
//! fine), table rows are split on `|`, and anything it does not recognise is
//! skipped rather than erroring — a half-written plan still yields whatever
//! sections are present. Everything here is pure (parse-from-string), so the
//! compose functions and their tests never need a live filesystem.

use std::path::Path;

use chrono::{Datelike, NaiveDate, Weekday};

/// True when an H2 title names the weekly meals table. Production plans use a
/// numbered `Dinners` heading while older fixtures and hand-written plans use
/// `Meal plan` or `Meals`; every reader and writer must share this predicate.
pub fn is_meals_section_heading(heading: &str) -> bool {
    heading
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| matches!(word, "meal" | "meals" | "dinner" | "dinners"))
}

/// One dinner slot from the meal-plan table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meal {
    /// Three-letter weekday as written in the plan, e.g. `"Mon"`.
    pub weekday: String,
    /// The concrete date, resolved from the `MM-DD` cell against the plan's
    /// year. `None` if the cell could not be parsed.
    pub date: Option<NaiveDate>,
    /// Slot type, e.g. `"Fish"`, `"Vegetarian"`, `"Leftover / flex"`.
    pub slot: String,
    /// The dish, e.g. `"Prawn & garlic linguine"`.
    pub dish: String,
    /// Prep time as written, e.g. `"~25 min"` (may be empty).
    pub prep: String,
}

/// One store section of the shopping list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShoppingSection {
    /// The `###` heading text, with any leading emoji kept, e.g.
    /// `"🐟 Fishmonger / market (Sat 07-18, fresh)"`.
    pub heading: String,
    /// The bullet items under the heading (leading `- ` stripped).
    pub items: Vec<String>,
}

/// One row of the `## 3. Calendar` projection table.
///
/// The calendar merges cook slots, sessions, and standing events — and, crucially
/// for the reminder engine, any **reminder rows** the family drafts, e.g.
/// `| Tue 07-14 | 19:30 | ⏰ Reminder: Luca PT check-in (if unanswered) | Otto |`.
/// The engine ([`crate::notify::reminder`]) reads these rows and fires the ones
/// shaped like reminders at their `date`+`time`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    /// Three-letter weekday as written, e.g. `"Tue"`.
    pub weekday: String,
    /// Concrete date resolved from the `MM-DD` day cell against the plan year.
    /// `None` when the cell could not be parsed.
    pub date: Option<NaiveDate>,
    /// Clock time as written in the Time column, e.g. `"19:30"` (may be empty).
    pub time: String,
    /// The Event column text, emoji kept, markdown stripped, e.g.
    /// `"⏰ Reminder: Luca PT check-in (if unanswered)"`.
    pub event: String,
    /// The Source column, e.g. `"Otto"` — which voice owns the row.
    pub source: String,
}

/// One workout session from a person's workout table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkoutDay {
    /// The person the block belongs to, e.g. `"Luca"`, `"Nadin"`.
    pub person: String,
    /// Three-letter weekday, e.g. `"Mon"`.
    pub weekday: String,
    /// Session label, e.g. `"Lower (strength)"`.
    pub session: String,
}

/// A parsed weekly plan document.
#[derive(Debug, Clone, Default)]
pub struct PlanDoc {
    /// The ISO week code from the filename, e.g. `"2026-W29"`.
    pub week_code: String,
    /// Monday of the plan week, from the `**Week of Monday …**` line.
    pub start: Option<NaiveDate>,
    /// Sunday of the plan week.
    pub end: Option<NaiveDate>,
    /// Publish status word, e.g. `"DRAFT"`, `"PUBLISHED"` (uppercased first
    /// token of the `**Status:**` line). Empty if absent.
    pub status: String,
    /// Dinners, in day order.
    pub meals: Vec<Meal>,
    /// Shopping list, in store-section order.
    pub shopping: Vec<ShoppingSection>,
    /// Workout sessions, in document order.
    pub workouts: Vec<WorkoutDay>,
    /// Calendar projection rows, in document order (source of reminder rows).
    pub calendar: Vec<CalendarEvent>,
}

/// Which `## N.` section the scanner is currently inside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Meals,
    Shopping,
    Workouts,
    Calendar,
    Other,
}

impl PlanDoc {
    /// Parse a plan from its `week_code` (filename stem) and markdown `content`.
    pub fn parse(week_code: &str, content: &str) -> Self {
        let mut doc = PlanDoc {
            week_code: week_code.to_string(),
            ..Default::default()
        };
        let year = parse_year(week_code);

        let mut section = Section::None;
        // Current workout person (set by the `### <Person> — …` heading inside
        // the Workouts section).
        let mut workout_person: Option<String> = None;

        for raw in content.lines() {
            let line = raw.trim();

            // --- Front-matter lines (any section) -----------------------------
            if doc.start.is_none() && line.contains("Week of") {
                let dates = find_iso_dates(line);
                if dates.len() >= 2 {
                    doc.start = Some(dates[0]);
                    doc.end = Some(dates[1]);
                }
            }
            if doc.status.is_empty() {
                if let Some(rest) = line.strip_prefix("**Status:**") {
                    doc.status = rest
                        .trim()
                        .split(|c: char| c.is_whitespace() || c == '—' || c == '-')
                        .find(|w| !w.is_empty())
                        .unwrap_or("")
                        .trim_matches('*')
                        .to_ascii_uppercase();
                }
            }

            // --- Section headings ---------------------------------------------
            if let Some(h2) = line.strip_prefix("## ") {
                let low = h2.to_ascii_lowercase();
                section = if is_meals_section_heading(h2) {
                    Section::Meals
                } else if low.contains("shopping") {
                    Section::Shopping
                } else if low.contains("workout") {
                    Section::Workouts
                } else if low.contains("calendar") {
                    Section::Calendar
                } else {
                    Section::Other
                };
                workout_person = None;
                continue;
            }

            // A `###` heading: a store section (Shopping) or a person (Workouts).
            if let Some(h3) = line.strip_prefix("### ") {
                match section {
                    Section::Shopping => doc.shopping.push(ShoppingSection {
                        heading: strip_md(h3.trim()),
                        items: Vec::new(),
                    }),
                    Section::Workouts => {
                        // "Luca — strength focus (…)" → person = "Luca".
                        let person = h3.split(['—', '-']).next().unwrap_or(h3).trim().to_string();
                        workout_person = Some(person);
                    }
                    _ => {}
                }
                continue;
            }

            // --- Section bodies ------------------------------------------------
            match section {
                Section::Meals => {
                    if let Some(cells) = table_row(line) {
                        // Columns: Day | Slot | Dish | Prep | …
                        if cells.len() >= 3 && !is_header_or_rule(&cells) {
                            let (weekday, date) = parse_day_cell(&cells[0], year);
                            if !weekday.is_empty() {
                                doc.meals.push(Meal {
                                    weekday,
                                    date,
                                    slot: strip_md(&cells[1]),
                                    dish: strip_md(&cells[2]),
                                    prep: strip_md(cells.get(3).map(|s| s.as_str()).unwrap_or("")),
                                });
                            }
                        }
                    }
                }
                Section::Shopping => {
                    if let Some(item) = line.strip_prefix("- ") {
                        if let Some(sec) = doc.shopping.last_mut() {
                            sec.items.push(strip_md(item.trim()));
                        }
                    }
                }
                Section::Calendar => {
                    if let Some(cells) = table_row(line) {
                        // Columns: Day | Time | Event | Source
                        if cells.len() >= 3 && !is_header_or_rule(&cells) {
                            let (weekday, date) = parse_day_cell(&cells[0], year);
                            if !weekday.is_empty() {
                                doc.calendar.push(CalendarEvent {
                                    weekday,
                                    date,
                                    time: strip_md(&cells[1]),
                                    event: strip_md(&cells[2]),
                                    source: strip_md(
                                        cells.get(3).map(|s| s.as_str()).unwrap_or(""),
                                    ),
                                });
                            }
                        }
                    }
                }
                Section::Workouts => {
                    if let (Some(person), Some(cells)) = (workout_person.as_ref(), table_row(line))
                    {
                        // Columns: Day | Session | Structure
                        if cells.len() >= 2 && !is_header_or_rule(&cells) {
                            let (weekday, _date) = parse_day_cell(&cells[0], year);
                            if !weekday.is_empty() {
                                doc.workouts.push(WorkoutDay {
                                    person: person.clone(),
                                    weekday,
                                    session: cells[1].clone(),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        doc
    }

    /// True when `day` falls within this plan's `[start, end]` week (inclusive).
    /// Always false when the range failed to parse.
    pub fn covers(&self, day: NaiveDate) -> bool {
        match (self.start, self.end) {
            (Some(s), Some(e)) => s <= day && day <= e,
            _ => false,
        }
    }

    /// The dinner for `day`, matched by date when the cell parsed, else by
    /// weekday name against `day`'s weekday.
    pub fn meal_on(&self, day: NaiveDate) -> Option<&Meal> {
        if let Some(m) = self.meals.iter().find(|m| m.date == Some(day)) {
            return Some(m);
        }
        let want = short_weekday(day);
        self.meals
            .iter()
            .find(|m| m.weekday.eq_ignore_ascii_case(want))
    }
}

/// Load and parse the weekly plans under `dir` (the workgraph project root):
/// ONE document per ISO week, sorted by `week_code` so the newest week is last.
/// A missing `plans/` directory yields an empty vec (not an error) — the
/// commands then report honestly that there is no plan yet.
///
/// TWO GATES, both mirroring the gateway (claw3d-bridge/src/weekSource.mjs), and
/// both there because "the newest `.md` in `plans/` whose name carries a week
/// code" is NOT the same thing as "the household's plan of record":
///
///   1. CANDIDACY ([`is_week_plan_candidate_stem`]) — a parked
///      `-dinner-suggestions` note, a `.draft.md` half-file, and the editorial
///      roles (`-review`, `-notes`, `-scratch`, `-wip`, `-summary`, `-check-in`)
///      are not plans. THE LIVE HAZARD this closes: [`current_plan`] falls back
///      to the most recent plan by week code, so a review file for the current
///      week — prose "## Dinners", zero meals — was returned as the plan of
///      record, and `week_start`'s `plan_covering` read it as "this week is
///      already planned".
///   2. SELECTION — several files legitimately target one week (the canonical
///      plan plus `-workouts` / `-recipes` / `-skeleton` companions). Before
///      this collapse both landed in the vec under the same week code and
///      `current_plan` returned whichever `read_dir` happened to yield first, so
///      an empty companion could shadow the real plan. We keep the RICHEST
///      parse per week ([`plan_content_score`]), the canonical `-family-plan`
///      wins ties, then newest mtime. A companion therefore still represents its
///      week when the family plan is missing (flow 35 — the workouts card still
///      renders over an honest not-planned meals surface) and never when it is
///      present.
pub fn load_plans(dir: &Path) -> Vec<PlanDoc> {
    let plans_dir = dir.join("plans");
    // week code → (doc, content score, canonical?, mtime) — the incumbent for
    // that week and the keys that decide whether a later file unseats it.
    let mut best: Vec<(PlanDoc, usize, bool, std::time::SystemTime)> = Vec::new();
    let entries = match std::fs::read_dir(&plans_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if !is_week_plan_candidate_stem(stem) {
            continue;
        }
        // Only weekly-plan files (`2026-W29-family-plan`), keyed by week code.
        let week_code = match week_code_from_stem(stem) {
            Some(w) => w,
            None => continue,
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let doc = PlanDoc::parse(&week_code, &content);
        let score = plan_content_score(&doc);
        let canonical = is_family_plan_stem(stem);
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        match best.iter_mut().find(|(d, ..)| d.week_code == week_code) {
            Some(slot) => {
                let beats = (score, canonical, mtime) > (slot.1, slot.2, slot.3);
                if beats {
                    *slot = (doc, score, canonical, mtime);
                }
            }
            None => best.push((doc, score, canonical, mtime)),
        }
    }
    let mut docs: Vec<PlanDoc> = best.into_iter().map(|(d, ..)| d).collect();
    docs.sort_by(|a, b| a.week_code.cmp(&b.week_code));
    docs
}

/// Pick the plan that is "current" as of `today`: the plan whose week covers
/// today, else the nearest upcoming plan (soonest start after today), else the
/// most recent plan by week code. `None` only when `plans` is empty.
pub fn current_plan<'a>(plans: &'a [PlanDoc], today: NaiveDate) -> Option<&'a PlanDoc> {
    if let Some(p) = plans.iter().find(|p| p.covers(today)) {
        return Some(p);
    }
    // Nearest upcoming (smallest start strictly after today).
    let upcoming = plans
        .iter()
        .filter(|p| p.start.map(|s| s > today).unwrap_or(false))
        .min_by_key(|p| p.start.unwrap());
    if upcoming.is_some() {
        return upcoming;
    }
    // Fall back to the latest week we have (plans is sorted by week code).
    plans.last()
}

/// Full weekday name for a date, e.g. `Monday`.
pub fn long_weekday(day: NaiveDate) -> &'static str {
    match day.weekday() {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// Full weekday name from a three-letter code (`"Mon"` → `"Monday"`). Returns
/// the input unchanged when it is not a recognised abbreviation.
pub fn expand_weekday(short: &str) -> String {
    match short.to_ascii_lowercase().as_str() {
        "mon" => "Monday".to_string(),
        "tue" => "Tuesday".to_string(),
        "wed" => "Wednesday".to_string(),
        "thu" => "Thursday".to_string(),
        "fri" => "Friday".to_string(),
        "sat" => "Saturday".to_string(),
        "sun" => "Sunday".to_string(),
        _ => short.to_string(),
    }
}

/// Three-letter weekday for a date, matching how the plan writes day cells.
fn short_weekday(day: NaiveDate) -> &'static str {
    match day.weekday() {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

/// Strip the common inline-markdown emphasis markers (`**bold**`, `*italic*`,
/// `` `code` ``) so plan text reads cleanly on a phone. Leaves the words intact.
fn strip_md(s: &str) -> String {
    s.replace("**", "").replace('`', "").replace('*', "")
}

/// Split a markdown table row `| a | b | c |` into trimmed cells. Returns
/// `None` for non-table lines.
fn table_row(line: &str) -> Option<Vec<String>> {
    if !line.starts_with('|') {
        return None;
    }
    let inner = line.trim_matches('|');
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

/// True for a header row (`Day | Slot | …`) or a `|---|---|` rule row, which we
/// skip rather than treat as data.
fn is_header_or_rule(cells: &[String]) -> bool {
    let first = cells[0].to_ascii_lowercase();
    if first == "day" {
        return true;
    }
    // Separator rule: every cell is only dashes/colons/spaces.
    cells
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
        || cells
            .iter()
            .any(|c| c.chars().all(|ch| ch == '-') && !c.is_empty())
}

/// Parse a day cell like `"Mon 07-13"` into (`"Mon"`, date). The date is
/// resolved against `year`; `None` if the `MM-DD` part is missing/unparseable.
fn parse_day_cell(cell: &str, year: Option<i32>) -> (String, Option<NaiveDate>) {
    let mut parts = cell.split_whitespace();
    let weekday = parts.next().unwrap_or("").to_string();
    let date = parts.next().and_then(|md| parse_month_day(md, year));
    (weekday, date)
}

/// Parse `"07-13"` against a year into a `NaiveDate`.
fn parse_month_day(md: &str, year: Option<i32>) -> Option<NaiveDate> {
    let year = year?;
    let (m, d) = md.split_once('-')?;
    let month: u32 = m.trim().parse().ok()?;
    let day: u32 = d.trim().parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Pull the 4-digit year out of a week code like `2026-W29`.
fn parse_year(week_code: &str) -> Option<i32> {
    week_code.split('-').next()?.parse().ok()
}

/// Find all `YYYY-MM-DD` dates in a line, in order.
fn find_iso_dates(line: &str) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    for token in line.split(|c: char| !(c.is_ascii_digit() || c == '-')) {
        if token.len() == 10 {
            if let Ok(d) = NaiveDate::parse_from_str(token, "%Y-%m-%d") {
                out.push(d);
            }
        }
    }
    out
}

/// A `plans/<week>-dinner-suggestions.md` note is a SIDE-CHANNEL, never a plan of
/// record: it is where the family's dinner choices are parked for a week that has
/// no plan file yet. Its filename stem carries a week code, so every "is this a
/// plan for week W" test that looks only at the code sees it as one — which is how
/// a parked-suggestions note became a phantom "plan" titled "Dinner suggestions for
/// the week of …". The gateway has excluded the suffix since the note was
/// introduced (`discoverPlanFiles`, claw3d-bridge/src/weekSource.mjs); this is the
/// same rule on the engine side, in the one place both readers share.
pub fn is_sidecar_stem(stem: &str) -> bool {
    let low = stem.to_ascii_lowercase();
    low.ends_with("-dinner-suggestions") || low.ends_with("-dinner-suggestion")
}

/// The EDITORIAL roles a planning agent parks beside a plan: a review, a
/// work-in-progress draft, scratch notes, a summary, a check-in. Mirrors the
/// gateway's `PLAN_AUX_ROLE_RE` (claw3d-bridge/src/weekSource.mjs) plus the
/// `<name>.draft.md` atomic-publish half-file that `discoverPlanFiles` skips.
///
/// Matched by ROLE SUFFIX, never by persona: the incident file was
/// `<week>-nora-review.md`, and the next one will carry a different name. A
/// review of the family plan (`<week>-family-plan-review.md`) is a review — the
/// suffix decides, not the `family-plan` substring.
pub fn is_aux_role_stem(stem: &str) -> bool {
    let low = stem.to_ascii_lowercase();
    if low.ends_with(".draft") {
        return true;
    }
    [
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
    ]
    .iter()
    .any(|role| low.ends_with(role))
}

/// Is this filename stem a WEEK-PLAN CANDIDATE — a file that may represent its
/// ISO week as the plan of record?
///
/// This is the engine half of a rule the gateway has enforced for longer
/// (`discoverPlanFiles` ∧ `isWeekPlanCandidate`, claw3d-bridge/src/weekSource.mjs).
/// The two sides are pinned equivalent by `tests/fixtures/plan_file_candidates.json`
/// — see the test at the bottom of this file and the gateway's
/// `claw3d-bridge/test/planCandidateParity.test.mjs`.
///
/// EXCLUDED: the parked `-dinner-suggestions` side channel, any `.draft.md`
/// half-written file, and the editorial roles ([`is_aux_role_stem`]).
///
/// NOT EXCLUDED, on purpose: a `-workouts` / `-recipes` / `-skeleton` companion.
/// Those carry REAL week content (flow 35: when the family plan is momentarily
/// lost, the surviving workouts companion still renders its Moving card). They
/// simply lose selection to the richer canonical plan on content score — see
/// [`plan_content_score`] and [`load_plans`] — rather than being filtered out.
pub fn is_week_plan_candidate_stem(stem: &str) -> bool {
    if is_sidecar_stem(stem) {
        return false;
    }
    let low = stem.to_ascii_lowercase();
    if low.ends_with(".draft") {
        return false;
    }
    // The canonical week plan always qualifies (mirrors the gateway's early
    // return), so a role word inside the household's own plan name can never
    // disqualify it.
    if low.ends_with("-family-plan") {
        return true;
    }
    !is_aux_role_stem(stem)
}

/// [`is_week_plan_candidate_stem`] at the FILE level: `2026-W31-nora-review.md`
/// rather than its stem. Requires a markdown extension and a readable week code,
/// so this is the complete engine-side answer to the gateway's
/// `discoverPlanFiles` ∧ `isWeekPlanCandidate` — the shape the shared fixture
/// list is written in.
pub fn is_week_plan_candidate_file(name: &str) -> bool {
    let path = Path::new(name);
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
    {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    week_code_from_stem(stem).is_some() && is_week_plan_candidate_stem(stem)
}

/// True for the canonical `…-family-plan` filename, used only as a TIE-BREAK in
/// selection — content wins first, so a family plan that regressed to an empty
/// parse can never out-rank a sibling that actually has the week's content.
fn is_family_plan_stem(stem: &str) -> bool {
    stem.to_ascii_lowercase().contains("family-plan")
}

/// How much real week content a parse yielded — dinners + workout sessions +
/// shopping items + calendar rows. Zero for a companion that carries none of the
/// plan sections. Mirrors the gateway's `planContentScore` (the engine's
/// `PlanDoc` has no `confirmations` domain, so that term is absent here).
pub fn plan_content_score(doc: &PlanDoc) -> usize {
    doc.meals.len()
        + doc.workouts.len()
        + doc.shopping.iter().map(|s| s.items.len()).sum::<usize>()
        + doc.calendar.len()
}

/// Extract the `YYYY-Wnn` week code from a filename stem like
/// `2026-W29-family-plan`. Requires a 4-digit year followed by `W` + digits, and
/// refuses a side-channel note ([`is_sidecar_stem`]).
fn week_code_from_stem(stem: &str) -> Option<String> {
    if is_sidecar_stem(stem) {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const W29: &str = include_str!("../../tests/fixtures/family_plan_w29.md");

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn parses_week_range_and_status() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert_eq!(doc.start, Some(date(2026, 7, 13)));
        assert_eq!(doc.end, Some(date(2026, 7, 19)));
        assert_eq!(doc.status, "DRAFT");
    }

    #[test]
    fn parses_live_dinners_heading_in_day_order() {
        assert!(
            W29.contains("## 1. Dinners ("),
            "canonical fixture must keep the live numbered Dinners heading"
        );
        let doc = PlanDoc::parse("2026-W29", W29);
        assert_eq!(doc.meals.len(), 7, "one dinner per day");
        assert_eq!(doc.meals[0].weekday, "Mon");
        assert_eq!(doc.meals[0].date, Some(date(2026, 7, 13)));
        assert_eq!(doc.meals[0].dish, "Chickpea & spinach curry, brown rice");
        assert_eq!(doc.meals[0].prep, "~35 min");
        assert_eq!(
            doc.meals[1].dish,
            "Baked salmon, roasted potatoes, green beans"
        );
    }

    #[test]
    fn legacy_meals_heading_remains_supported() {
        let legacy = W29
            .lines()
            .map(|line| {
                if line
                    .strip_prefix("## ")
                    .map(is_meals_section_heading)
                    .unwrap_or(false)
                {
                    "## 1. Meals"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let doc = PlanDoc::parse("2026-W29", &legacy);
        assert_eq!(doc.meals.len(), 7, "legacy Meals alias lost dinner rows");
        assert_eq!(doc.meals[0].dish, "Chickpea & spinach curry, brown rice");
    }

    #[test]
    fn meal_on_matches_by_date() {
        let doc = PlanDoc::parse("2026-W29", W29);
        let fri = doc.meal_on(date(2026, 7, 17)).expect("friday dinner");
        assert!(fri.dish.contains("beef"), "got {}", fri.dish);
        assert_eq!(fri.slot, "Red meat");
    }

    #[test]
    fn covers_only_its_own_week() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert!(doc.covers(date(2026, 7, 13)));
        assert!(doc.covers(date(2026, 7, 19)));
        assert!(!doc.covers(date(2026, 7, 12)), "the day before is W28");
        assert!(!doc.covers(date(2026, 7, 20)));
    }

    #[test]
    fn parses_shopping_sections_with_items() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert!(doc.shopping.len() >= 3, "at least three store sections");
        let fish = &doc.shopping[0];
        assert!(fish.heading.contains("Fishmonger"), "got {}", fish.heading);
        assert!(fish.items.iter().any(|i| i.contains("Salmon")));
        // A produce item lands in its own section, not the fish one.
        assert!(
            doc.shopping
                .iter()
                .any(|s| s.items.iter().any(|i| i.contains("Spinach")))
        );
    }

    #[test]
    fn parses_workouts_per_person() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert!(doc.workouts.iter().any(|w| w.person == "Luca"));
        assert!(doc.workouts.iter().any(|w| w.person == "Nadin"));
        let luca_mon = doc
            .workouts
            .iter()
            .find(|w| w.person == "Luca" && w.weekday == "Mon")
            .expect("luca monday");
        assert!(luca_mon.session.contains("Lower"));
    }

    #[test]
    fn parses_calendar_events_including_reminder_row() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert!(doc.calendar.len() >= 10, "calendar rows parsed");
        // The reminder row is present with its time, event text, and source.
        let rem = doc
            .calendar
            .iter()
            .find(|e| e.event.contains("Reminder"))
            .expect("the ⏰ Reminder row");
        assert_eq!(rem.weekday, "Tue");
        assert_eq!(rem.date, Some(date(2026, 7, 14)));
        assert_eq!(rem.time, "19:30");
        assert!(rem.event.contains("Luca PT check-in"), "got {}", rem.event);
        assert_eq!(rem.source, "Otto");
        // A plain cook row is a calendar event too, but not a reminder.
        assert!(doc.calendar.iter().any(|e| e.event.contains("Cook:")));
    }

    #[test]
    fn current_plan_prefers_covering_week() {
        let w28 = PlanDoc {
            week_code: "2026-W28".to_string(),
            start: Some(date(2026, 7, 6)),
            end: Some(date(2026, 7, 12)),
            ..Default::default()
        };
        let w29 = PlanDoc {
            week_code: "2026-W29".to_string(),
            start: Some(date(2026, 7, 13)),
            end: Some(date(2026, 7, 19)),
            ..Default::default()
        };
        let plans = vec![w28, w29];
        // A day inside W28 → W28.
        assert_eq!(
            current_plan(&plans, date(2026, 7, 11)).unwrap().week_code,
            "2026-W28"
        );
        // A day inside W29 → W29.
        assert_eq!(
            current_plan(&plans, date(2026, 7, 15)).unwrap().week_code,
            "2026-W29"
        );
        // Before any plan → nearest upcoming (W28).
        assert_eq!(
            current_plan(&plans, date(2026, 7, 1)).unwrap().week_code,
            "2026-W28"
        );
        // After every plan → latest (W29).
        assert_eq!(
            current_plan(&plans, date(2026, 8, 1)).unwrap().week_code,
            "2026-W29"
        );
    }

    #[test]
    fn week_code_from_stem_accepts_plan_files_only() {
        assert_eq!(
            week_code_from_stem("2026-W29-family-plan"),
            Some("2026-W29".to_string())
        );
        assert_eq!(week_code_from_stem("notes"), None);
        assert_eq!(week_code_from_stem("2026-garbage"), None);
    }

    // ── plan-file selection (task sidecar-is-not) ────────────────────────────
    //
    // A file under `plans/` whose name carries a week code is not automatically a
    // plan. `week-start-engine` closed the parked `-dinner-suggestions` note at
    // two points; the SAME corruption class was still open one lane over, in the
    // glob every other reader goes through: `current_plan` falls back to "the most
    // recent plan by week code", so a review or a companion for the current week
    // came back as the plan of record with no meals in it.

    /// A plan with real content for a week — the plan of record.
    fn family_plan_md(week: &str, monday: &str, sunday: &str) -> String {
        format!(
            "# Household weekly plan · {week}\n\n\
             **Week of Monday {monday} → Sunday {sunday}**\n\
             **Status:** PUBLISHED\n\n\
             ## 1. Dinners\n\n\
             | Day | Slot type | Dinner | Prep |\n\
             |-----|-----------|--------|------|\n\
             | Mon 07-27 | Vegetarian | Miso aubergine noodles | ~30 min |\n\
             | Tue 07-28 | Fish | Baked trout | ~25 min |\n"
        )
    }

    /// THE INCIDENT SHAPE: an editorial review that MASQUERADES as a plan — a
    /// `## Dinners` section carrying prose and bullets, no meals table, and the
    /// week's date range in the header so it parses as "covering" the week.
    fn review_md(week: &str, monday: &str, sunday: &str) -> String {
        format!(
            "# Review of {week}\n\n\
             **Week of Monday {monday} → Sunday {sunday}**\n\n\
             ## Dinners\n\n\
             The fish night landed twice this week; worth moving one to Thursday.\n\
             - Tuesday felt rushed\n\
             - Nobody finished the lentils\n"
        )
    }

    fn write(root: &Path, name: &str, body: &str) {
        std::fs::write(root.join("plans").join(name), body).unwrap();
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("plans")).unwrap();
        dir
    }

    const MON: &str = "2026-07-27";
    const SUN: &str = "2026-08-02";

    #[test]
    fn a_review_never_becomes_the_plan_of_record() {
        let today = date(2026, 7, 28);

        // With the real plan present, the review must not be selected …
        let dir = project();
        write(
            dir.path(),
            "2026-W31-family-plan.md",
            &family_plan_md("2026-W31", MON, SUN),
        );
        write(
            dir.path(),
            "2026-W31-nora-review.md",
            &review_md("2026-W31", MON, SUN),
        );
        let plans = load_plans(dir.path());
        assert_eq!(plans.len(), 1, "one document per ISO week: {plans:#?}");
        let chosen = current_plan(&plans, today).expect("a plan of record");
        assert_eq!(chosen.week_code, "2026-W31");
        assert_eq!(
            chosen.meals.len(),
            2,
            "the family plan's dinners, not the review's prose"
        );

        // … and with NO plan on disk it must not become one either: the honest
        // answer is that this week has no plan, not a plan with zero meals.
        let bare = project();
        write(
            bare.path(),
            "2026-W31-nora-review.md",
            &review_md("2026-W31", MON, SUN),
        );
        assert!(
            load_plans(bare.path()).is_empty(),
            "a review is not a plan, even when it is the only file"
        );
        assert!(current_plan(&load_plans(bare.path()), today).is_none());

        // Nor may it shadow an OLDER real plan by sorting last (the `plans.last()`
        // fallback in current_plan is exactly how a sidecar became "the week").
        let older = project();
        write(
            older.path(),
            "2026-W30-family-plan.md",
            &family_plan_md("2026-W30", "2026-07-20", "2026-07-26"),
        );
        write(
            older.path(),
            "2026-W31-nora-review.md",
            &review_md("2026-W31", MON, SUN),
        );
        let plans = load_plans(older.path());
        assert_eq!(
            current_plan(&plans, today).unwrap().week_code,
            "2026-W30",
            "the last real plan, never the newer review"
        );
    }

    /// THE NEGATIVE CONTROL. The test above passes trivially if the review simply
    /// never parsed as a plan — so pin the hazard itself: run the PRE-FIX glob
    /// (week code from the stem, nothing else) over the same directory and watch
    /// it hand back the review, covering the day, with zero dinners in it. That is
    /// the corruption `is_week_plan_candidate_stem` exists to prevent; if this
    /// control ever stops reproducing, the test above has stopped proving anything.
    #[test]
    fn the_pre_fix_glob_did_return_the_review_as_the_plan() {
        let dir = project();
        write(
            dir.path(),
            "2026-W31-nora-review.md",
            &review_md("2026-W31", MON, SUN),
        );

        let mut docs = Vec::new();
        for entry in std::fs::read_dir(dir.path().join("plans")).unwrap().flatten() {
            let path = entry.path();
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(week) = week_code_from_stem(stem) {
                docs.push(PlanDoc::parse(&week, &std::fs::read_to_string(&path).unwrap()));
            }
        }
        let stale = current_plan(&docs, date(2026, 7, 28)).expect("the pre-fix hazard");
        assert_eq!(stale.week_code, "2026-W31");
        assert!(
            stale.meals.is_empty(),
            "a plan of record with no dinners — the bug, reproduced"
        );
    }

    #[test]
    fn drafts_and_editorial_roles_are_not_loaded() {
        let dir = project();
        for name in [
            "2026-W31-family-plan.draft.md",
            "2026-W31-review.md",
            "2026-W31-otto-notes.md",
            "2026-W31-scratch.md",
            "2026-W31-wip.md",
            "2026-W31-summary.md",
            "2026-W31-check-in.md",
            "2026-W31-dinner-suggestions.md",
            "2026-W31-family-plan-review.md",
        ] {
            write(dir.path(), name, &family_plan_md("2026-W31", MON, SUN));
        }
        assert!(
            load_plans(dir.path()).is_empty(),
            "every excluded role, even when it parses as a rich plan"
        );

        // The canonical plan beside them is still found.
        write(
            dir.path(),
            "2026-W31-family-plan.md",
            &family_plan_md("2026-W31", MON, SUN),
        );
        let plans = load_plans(dir.path());
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].meals.len(), 2);
    }

    #[test]
    fn a_companion_contributes_but_never_outranks_the_family_plan() {
        let workouts = format!(
            "# Moving · 2026-W31\n\n**Week of Monday {MON} → Sunday {SUN}**\n\n\
             ## 2. Workouts\n\n\
             ### Luca\n\n\
             | Day | Session |\n|-----|---------|\n| Mon | Lower (strength) |\n"
        );

        // Alone (flow 35 — the family plan is momentarily lost): the companion
        // still represents its week, so its Moving content still renders.
        let alone = project();
        write(alone.path(), "2026-W31-mira-workouts.md", &workouts);
        let plans = load_plans(alone.path());
        assert_eq!(plans.len(), 1, "the companion is a candidate, not filtered");
        assert!(
            !plans[0].workouts.is_empty(),
            "and it contributes its section"
        );
        assert!(plans[0].meals.is_empty(), "honestly empty on dinners");

        // Beside the canonical plan: the plan wins, every time, regardless of
        // which file `read_dir` yields first or which was touched last.
        let both = project();
        write(both.path(), "2026-W31-mira-workouts.md", &workouts);
        write(
            both.path(),
            "2026-W31-family-plan.md",
            &family_plan_md("2026-W31", MON, SUN),
        );
        // Touch the companion so it is the NEWEST file — mtime must not decide.
        filetime_touch(&both.path().join("plans").join("2026-W31-mira-workouts.md"));
        let plans = load_plans(both.path());
        assert_eq!(plans.len(), 1, "one document per ISO week");
        assert_eq!(
            plans[0].meals.len(),
            2,
            "the richer canonical plan represents the week"
        );
    }

    /// Bump a file's mtime past its siblings without pulling in a new crate:
    /// rewrite it in place after a short pause.
    fn filetime_touch(path: &Path) {
        let body = std::fs::read_to_string(path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path, body).unwrap();
    }

    /// THE ANTI-DRIFT PIN. The gateway (`discoverPlanFiles` ∧
    /// `isWeekPlanCandidate`, claw3d-bridge/src/weekSource.mjs) and the engine
    /// answer the same question in two languages, in two repos that do not import
    /// each other — which is how the engine spent months without the rule at all.
    /// Both are now tested against ONE list; the gateway's half lives in
    /// claw3d-bridge/test/planCandidateParity.test.mjs.
    #[test]
    fn plan_candidate_fixtures_match_gateway_rule() {
        const FIXTURES: &str = include_str!("../../tests/fixtures/plan_file_candidates.json");
        let parsed: serde_json::Value = serde_json::from_str(FIXTURES).expect("fixture json");
        let rows = parsed["candidates"].as_array().expect("candidates array");
        assert!(rows.len() >= 20, "the list must stay comprehensive");
        for row in rows {
            let file = row["file"].as_str().unwrap();
            let want = row["candidate"].as_bool().unwrap();
            assert_eq!(
                is_week_plan_candidate_file(file),
                want,
                "{file}: {}",
                row["why"].as_str().unwrap_or("")
            );
        }
    }

    #[test]
    fn expand_weekday_maps_abbreviations() {
        assert_eq!(expand_weekday("Mon"), "Monday");
        assert_eq!(expand_weekday("sun"), "Sunday");
        assert_eq!(expand_weekday("Whatever"), "Whatever");
    }
}
