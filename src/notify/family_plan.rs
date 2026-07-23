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
//! * the **meal plan** table (`## 1. Meal plan`) as one [`Meal`] per day,
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
/// `| Tue 07-14 | 19:30 | ⏰ Reminder: Alex PT check-in (if unanswered) | Otto |`.
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
    /// `"⏰ Reminder: Alex PT check-in (if unanswered)"`.
    pub event: String,
    /// The Source column, e.g. `"Otto"` — which voice owns the row.
    pub source: String,
}

/// One workout session from a person's workout table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkoutDay {
    /// The person the block belongs to, e.g. `"Alex"`, `"Sam"`.
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
                section = if low.contains("meal") {
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
                        // "Alex — strength focus (…)" → person = "Alex".
                        let person = h3
                            .split(['—', '-'])
                            .next()
                            .unwrap_or(h3)
                            .trim()
                            .to_string();
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
                    if let (Some(person), Some(cells)) =
                        (workout_person.as_ref(), table_row(line))
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

/// Load and parse every `plans/*.md` file under `dir` (the workgraph project
/// root). Files are returned sorted by `week_code` so the newest week is last.
/// A missing `plans/` directory yields an empty vec (not an error) — the
/// commands then report honestly that there is no plan yet.
pub fn load_plans(dir: &Path) -> Vec<PlanDoc> {
    let plans_dir = dir.join("plans");
    let mut docs = Vec::new();
    let entries = match std::fs::read_dir(&plans_dir) {
        Ok(e) => e,
        Err(_) => return docs,
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
        // Only weekly-plan files (`2026-W29-family-plan`), keyed by week code.
        let week_code = match week_code_from_stem(stem) {
            Some(w) => w,
            None => continue,
        };
        if let Ok(content) = std::fs::read_to_string(&path) {
            docs.push(PlanDoc::parse(&week_code, &content));
        }
    }
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
        || cells.iter().any(|c| c.chars().all(|ch| ch == '-') && !c.is_empty())
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

/// Extract the `YYYY-Wnn` week code from a filename stem like
/// `2026-W29-family-plan`. Requires a 4-digit year followed by `W` + digits.
fn week_code_from_stem(stem: &str) -> Option<String> {
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
    fn parses_seven_meals_in_day_order() {
        let doc = PlanDoc::parse("2026-W29", W29);
        assert_eq!(doc.meals.len(), 7, "one dinner per day");
        assert_eq!(doc.meals[0].weekday, "Mon");
        assert_eq!(doc.meals[0].date, Some(date(2026, 7, 13)));
        assert_eq!(doc.meals[0].dish, "Chickpea & spinach curry, brown rice");
        assert_eq!(doc.meals[0].prep, "~35 min");
        assert_eq!(doc.meals[1].dish, "Baked salmon, roasted potatoes, green beans");
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

    #[test]
    fn expand_weekday_maps_abbreviations() {
        assert_eq!(expand_weekday("Mon"), "Monday");
        assert_eq!(expand_weekday("sun"), "Sunday");
        assert_eq!(expand_weekday("Whatever"), "Whatever");
    }
}
