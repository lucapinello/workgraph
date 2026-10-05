//! `wg feedback …` — the operable surface of the meal-feedback loop.
//!
//! The pure logic lives in [`crate::notify::meal_feedback`]; this module wires it
//! to the filesystem and the plan parser so the listener / gateway (and tests)
//! can drive the loop end to end:
//!
//! * `wg feedback ask`     — compose the rate-limited "how was dinner?" line
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
    // Only an ask about THIS evening may be marked answered — see ask_a_rating_may_answer.
    // A rating for a night the house never asked about must leave the ledger alone rather than
    // falsify it, which is what the old newest-ts flip did.
    let _ = meal_feedback::mark_ask_answered(&ask_log, &rating.dish, rating.ts);

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

#[cfg(test)]
mod tests {
    use super::*;
    use worksgood::notify::telegram;

    /// A week whose Tuesday dinner is the one the family is answering about.
    const PLAN: &str = "\
# Family week · 2026-W29 · Week of Monday July 13 – Sunday July 19

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** PUBLISHED

## 1. Dinners (planner → cook)

| Day | Slot | Dinner | Prep |
|-----|------|--------|------|
| Mon 07-13 | Vegetarian | Chickpea curry | ~35 min |
| Tue 07-14 | Fish | Baked salmon | ~30 min |
";

    fn scratch_with_plan() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), PLAN).unwrap();
        dir
    }

    /// A raw update in the exact wire shape Telegram sends for a reaction. Written out in full
    /// rather than built from a helper, because the SHAPE is half of what this test is about: the
    /// decoder is pinned to the real field names, not to a convenient approximation of them.
    fn reaction_update(message_id: i64, emoji: &str) -> serde_json::Value {
        serde_json::json!({
            "update_id": 900,
            "message_reaction": {
                "chat": { "id": -1001234567890_i64, "type": "group" },
                "message_id": message_id,
                "user": { "id": 42, "username": "luca" },
                "date": 1_752_500_000_i64,
                "new_reaction": [ { "type": "emoji", "emoji": emoji } ]
            }
        })
    }

    /// THE FLIP, end to end and hermetically: a real Telegram reaction — the wire shape the
    /// listener actually receives — through the REAL decoder, into a real rating, and onto the
    /// RIGHT evening of the ask ledger.
    ///
    /// This is the half of the dinner loop that can be proved without a live family. The other
    /// half — one real ask and one real thumb in the real group — is Luca's, and cannot be
    /// simulated here without lying about what was proved.
    #[test]
    fn a_thumb_reaction_reaches_a_rating_on_the_right_evening() {
        let dir = scratch_with_plan();
        let root = dir.path();

        // The ledger holds an OLD ask (well out of range) and the one the thumb actually answers.
        // Timestamps are relative to the real clock because `run_record` stamps the rating with
        // `Utc::now()` — the one input a file-level test cannot pin without changing production
        // code to suit the test.
        let now_ms = Utc::now().timestamp_millis();
        let hour_ms = 60 * 60 * 1000;
        let day_ms = 24 * hour_ms;
        let ask_log = meal_feedback::ask_log_path_for(root);
        meal_feedback::append_ask(
            &ask_log,
            &meal_feedback::AskRecord {
                ts: now_ms - 20 * day_ms,
                dish: "Roast chicken".into(),
                responded: false,
            },
        )
        .unwrap();
        meal_feedback::append_ask(
            &ask_log,
            &meal_feedback::AskRecord {
                ts: now_ms - hour_ms,
                dish: "Baked salmon".into(),
                responded: false,
            },
        )
        .unwrap();

        // 1. THE EAR. A reaction carries no text, so the emoji IS the body — and its message_id is
        //    the message reacted TO, which must ride as `reply_to` or the answer cannot be tied to
        //    the ask. Before KNOWN-GAPS #9 was closed this returned None: `allowed_updates` never
        //    subscribed `message_reaction`, and this function had no branch for it.
        let msg = telegram::decode_update(&reaction_update(555, "👍"), "telegram:nora")
            .expect("a thumb on the dinner ask must decode — the ear was the bug");
        assert_eq!(msg.body, "👍", "the emoji is the body");
        assert_eq!(
            msg.reply_to.as_ref().map(|m| m.0.as_str()),
            Some("555"),
            "the reaction's message_id is the ask it answers"
        );
        assert!(!msg.sender_is_bot, "a family member's thumb is not a bot's");
        assert!(msg.message_id.is_none(), "a reaction is not itself a message");

        // 2. THE FLIP. The decoded body goes through the SAME routing a typed reply takes.
        run_record(root, &msg.sender, &msg.body, None, Some("2026-07-14"), true)
            .expect("recording a legible thumb must not fail");

        // 3. What landed is one rating, for TONIGHT'S dish, and a LIKED one.
        let ratings = meal_feedback::load_ratings(&meal_feedback::feedback_path_for(root));
        assert_eq!(ratings.len(), 1, "exactly one rating — not zero, not two");
        assert_eq!(ratings[0].dish, "Baked salmon", "the dish came from the plan");
        assert_eq!(ratings[0].verdict, meal_feedback::Verdict::Liked, "👍 is a Liked");

        // 4. AND THE RIGHT EVENING WAS FLIPPED. This is the assertion that makes the rest mean
        //    something: a flip is only correct if it lands on the ask the family actually answered.
        //    The old newest-ts flip marked whatever ask was most recent, which falsifies the ledger
        //    the gate reads — the house would believe it had been answered when it had not.
        let asks = meal_feedback::load_asks(&ask_log);
        assert_eq!(asks.len(), 2);
        assert!(asks[1].responded, "last night's ask — the one the thumb answered — must flip");
        assert!(
            !asks[0].responded,
            "an ask from 20 days ago must NOT be flipped by tonight's thumb"
        );
    }

    /// A DEGRADED EVENT MUST NOT BECOME ENGAGEMENT — asserted in the only place that matters:
    /// the ledger must be left exactly as it was.
    #[test]
    fn a_reaction_that_is_not_an_answer_never_records_one() {
        // The wire names the new reactions `new_reaction`. A payload that misspells it must decode
        // to NOTHING: a decoder that fuzzily "finds something" would turn a malformed update into
        // family-visible engagement that never happened — which is worse than missing it, because
        // nothing downstream can tell the difference afterwards.
        let wrong_field = serde_json::json!({
            "update_id": 901,
            "message_reaction": {
                "chat": { "id": -1001234567890_i64 },
                "message_id": 555,
                "user": { "id": 42, "username": "luca" },
                "new_reactions": [ { "type": "emoji", "emoji": "👍" } ]
            }
        });
        assert!(
            telegram::decode_update(&wrong_field, "telegram:nora").is_none(),
            "a misnamed field must not decode into a rating"
        );

        // A thumb REMOVED carries no emoji, and an un-thumb is not an answer.
        let removed = serde_json::json!({
            "update_id": 902,
            "message_reaction": {
                "chat": { "id": -1001234567890_i64 },
                "message_id": 555,
                "user": { "id": 42, "username": "luca" },
                "new_reaction": []
            }
        });
        assert!(
            telegram::decode_update(&removed, "telegram:nora").is_none(),
            "an un-thumb must not fake engagement"
        );

        // A non-emoji reaction type is not a sentiment we can read.
        let custom = serde_json::json!({
            "update_id": 903,
            "message_reaction": {
                "chat": { "id": -1001234567890_i64 },
                "message_id": 555,
                "user": { "id": 42, "username": "luca" },
                "new_reaction": [ { "type": "custom_emoji", "custom_emoji_id": "5368324170671202286" } ]
            }
        });
        assert!(telegram::decode_update(&custom, "telegram:nora").is_none());

        // And the ledger is untouched by a rating for a night the house never asked about: the
        // flip must leave it ALONE rather than mark the nearest ask answered.
        let dir = scratch_with_plan();
        let root = dir.path();
        let ask_log = meal_feedback::ask_log_path_for(root);
        meal_feedback::append_ask(
            &ask_log,
            &meal_feedback::AskRecord {
                ts: 1_752_400_000_000,
                dish: "Roast chicken".into(),
                responded: false,
            },
        )
        .unwrap();
        let flipped = meal_feedback::mark_ask_answered(&ask_log, "Baked salmon", 1_752_900_000_000)
            .unwrap();
        assert!(!flipped, "no ask about that evening — nothing to flip");
        assert!(
            meal_feedback::load_asks(&ask_log).iter().all(|a| !a.responded),
            "the ledger must be byte-for-byte as it was"
        );
        assert!(
            meal_feedback::load_ratings(&meal_feedback::feedback_path_for(root)).is_empty(),
            "no rating was invented for a night nobody asked about"
        );
    }
}
