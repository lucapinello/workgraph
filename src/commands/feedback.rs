//! `wg feedback …` — the operable surface of the meal-feedback loop.
//!
//! The pure logic lives in [`crate::notify::meal_feedback`]; this module wires it
//! to the filesystem and the plan parser so the listener / gateway (and tests)
//! can drive the loop end to end:
//!
//! * `wg feedback ask`     — compose Bruno's rate-limited "how was dinner?" line
//!   for tonight's dish (from the plan), and record that the ask went out.
//! * `wg feedback record`  — route a family reply/reaction into a structured
//!   rating and append it to `plans/feedback.jsonl` (the durable memory).
//! * `wg feedback summary` — print the ratings digest the Sunday drafter reads
//!   (`--session` prints the warm one-liner for the personas' session summaries).
//!
//! Every path resolves `plans/` from the project root (the parent of `.wg`), the
//! same root the family commands read from.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, Utc};

use worksgood::notify::{family_plan, meal_feedback};

/// The project root that holds `plans/`. The graph dir is `<root>/.wg`, so when
/// `workgraph_dir` is a `.wg`/`.workgraph` subdir we step up to its parent;
/// otherwise we treat it as the root itself. (Mirrors the helper in
/// `commands::telegram` so the two agree on where `plans/` lives.)
fn project_root(workgraph_dir: &Path) -> PathBuf {
    match workgraph_dir.file_name().and_then(|n| n.to_str()) {
        Some(".wg") | Some(".workgraph") => workgraph_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| workgraph_dir.to_path_buf()),
        _ => workgraph_dir.to_path_buf(),
    }
}

/// Resolve the date to reason about: an explicit `--today YYYY-MM-DD`, else the
/// local calendar day.
fn resolve_today(today: Option<&str>) -> Result<NaiveDate> {
    match today {
        Some(s) => NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .with_context(|| format!("--today must be YYYY-MM-DD, got '{s}'")),
        None => Ok(Local::now().date_naive()),
    }
}

/// Tonight's dish from the plan covering `day`, if any.
fn dish_for(root: &Path, day: NaiveDate) -> Option<String> {
    let plans = family_plan::load_plans(root);
    let plan = plans.iter().find(|p| p.covers(day))?;
    plan.meal_on(day).map(|m| m.dish.clone())
}

/// `wg feedback ask` — compose the (rate-limited) evening ask for tonight's
/// dinner and record that it was sent. Prints the family-voice line to stdout so
/// the listener can relay it; on a suppressed ask it prints nothing to send and
/// (in `--json`) reports why.
pub fn run_ask(
    workgraph_dir: &Path,
    dish: Option<&str>,
    today: Option<&str>,
    force: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let root = project_root(workgraph_dir);
    let day = resolve_today(today)?;

    // Resolve the dish: explicit override, else tonight's plan meal.
    let dish = match dish {
        Some(d) if !d.trim().is_empty() => Some(d.trim().to_string()),
        _ => dish_for(&root, day),
    };
    let dish_str = dish.clone().unwrap_or_default();

    // The nag gate — unless the operator forces it.
    let ask_log = meal_feedback::ask_log_path_for(&root);
    let asks = meal_feedback::load_asks(&ask_log);
    let now = Utc::now().timestamp_millis();
    let decision = meal_feedback::gate(&asks, now);
    if !force && !decision.should_send() {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "sent": false,
                    "reason": decision.reason(),
                })
            );
        } else {
            println!("(no ask — {})", decision.reason());
        }
        return Ok(());
    }

    let line = meal_feedback::compose_ask(&dish_str);

    if !dry_run {
        let record = meal_feedback::AskRecord {
            ts: now,
            dish: dish_str.clone(),
            responded: false,
        };
        meal_feedback::append_ask(&ask_log, &record)
            .with_context(|| format!("failed to record ask in {}", ask_log.display()))?;
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "sent": !dry_run,
                "dish": dish_str,
                "message": line,
                "forced": force,
            })
        );
    } else {
        println!("{line}");
    }
    Ok(())
}

/// `wg feedback record` — route a family reply into a rating and persist it.
/// `--dish` overrides the plan lookup (useful when the reply threads a specific
/// meal). A reply with no legible sentiment is reported and NOT recorded (we
/// never invent a rating).
pub fn run_record(
    workgraph_dir: &Path,
    rater: &str,
    reply: &str,
    dish: Option<&str>,
    today: Option<&str>,
    json: bool,
) -> Result<()> {
    let root = project_root(workgraph_dir);
    let day = resolve_today(today)?;

    let dish = match dish {
        Some(d) if !d.trim().is_empty() => Some(d.trim().to_string()),
        _ => dish_for(&root, day),
    };
    let dish = match dish {
        Some(d) => d,
        None => {
            anyhow::bail!(
                "could not determine which dish this rates — pass --dish (no plan meal for {day})"
            );
        }
    };

    let parsed = meal_feedback::parse_rating_reply(reply);
    let (verdict, note) = match parsed {
        Some(v) => v,
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "recorded": false, "reason": "no legible rating in reply" })
                );
            } else {
                println!("(no rating recorded — the reply carried no clear thumbs-up/down)");
            }
            return Ok(());
        }
    };

    let rating = meal_feedback::MealRating {
        ts: Utc::now().timestamp_millis(),
        dish: dish.clone(),
        rater: rater.trim().to_string(),
        verdict,
        note: note.clone(),
    };
    let path = meal_feedback::feedback_path_for(&root);
    meal_feedback::append_rating(&path, &rating)
        .with_context(|| format!("failed to record rating in {}", path.display()))?;

    // The family engaged — flip the latest ask to answered so the gate reopens.
    let ask_log = meal_feedback::ask_log_path_for(&root);
    let _ = meal_feedback::mark_latest_ask_answered(&ask_log);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "recorded": true,
                "dish": dish,
                "rater": rating.rater,
                "verdict": verdict.as_str(),
                "note": note,
            })
        );
    } else {
        println!(
            "Recorded: {} rated \"{}\" {} {}",
            rating.rater,
            dish,
            verdict.emoji(),
            verdict.as_str()
        );
    }
    Ok(())
}

/// `wg feedback summary` — print the ratings digest. Default is the plan briefing
/// (the block the Sunday drafter consumes); `--session` prints the warm one-liner
/// for the personas' session summaries. `--json` returns structured winners/losers.
pub fn run_summary(workgraph_dir: &Path, session: bool, json: bool) -> Result<()> {
    let root = project_root(workgraph_dir);
    let path = meal_feedback::feedback_path_for(&root);
    let ratings = meal_feedback::load_ratings(&path);

    if json {
        let winners: Vec<_> = meal_feedback::winners(&ratings)
            .into_iter()
            .map(|d| serde_json::json!({ "dish": d.dish, "score": d.score, "count": d.count, "notes": d.notes }))
            .collect();
        let losers: Vec<_> = meal_feedback::losers(&ratings)
            .into_iter()
            .map(|d| serde_json::json!({ "dish": d.dish, "score": d.score, "count": d.count, "notes": d.notes }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "ratings": ratings.len(),
                "winners": winners,
                "losers": losers,
                "briefing": meal_feedback::render_plan_briefing(&ratings),
                "session_note": meal_feedback::render_session_note(&ratings),
            })
        );
        return Ok(());
    }

    let text = if session {
        meal_feedback::render_session_note(&ratings)
    } else {
        meal_feedback::render_plan_briefing(&ratings)
    };
    if text.is_empty() {
        println!("(no dinner ratings yet)");
    } else {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}
