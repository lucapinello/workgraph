//! Casa's fast-lane plan-edit seam: a shopping sentence in family words, applied.
//!
//! Extracted from `commands/telegram.rs` as slice 8 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). Both items are ours and neither needs anything of upstream's.
//!
//! `web_fast_lane_now` travels here because this seam owns it, and the web-inbound code that
//! stays in their file imports it back. That direction is deliberate: leaving a helper behind
//! to keep an import tidy is how a marker gets added, and markers are the thing this split is
//! trying to remove.

use anyhow::{Context, Result};
use std::path::Path;

/// Apply and deliver one web fast-lane occurrence with restart-safe ordering.
///
/// Record-before-act is intentional:
///
/// 1. reserve the opaque occurrence;
/// 2. apply the plan edit and graph stamp once;
/// 3. persist the exact guarded reply + original route;
/// 4. claim/send through the transport delivery ledger;
/// 5. mark the occurrence delivered.
///
/// A crash in step 2 leaves `reserved` and a replay fails closed because it
/// cannot know whether the plan edit reached disk. A transport failure in step
/// 4 leaves `applied`; the replay sends the stored bytes to the stored route
/// without touching the plan again. The transport ledger itself claims before
/// send, closing the send-success/journal-mark crash window.
/// The wall clock to reason about reminders with on the web path, which is handed
/// a DATE rather than an instant. The live clock when that date is really today;
/// the start of the named day when a caller pinned `--today` for a test, so a
/// pinned run sees that whole day's reminders as still ahead of it.
pub(crate) fn web_fast_lane_now(today: chrono::NaiveDate) -> chrono::NaiveDateTime {
    let live = chrono::Local::now().naive_local();
    if live.date() == today {
        live
    } else {
        today.and_hms_opt(0, 0, 0).unwrap_or(live)
    }
}

/// What does a shopping sentence DO? — the `wg telegram shopping` seam
/// (see [`crate::cli::TelegramCommands::Shopping`]).
///
/// Prints the verdict of the exact shopping lane a family message hits: `add`,
/// `remove`, `ask` (with the reason — an implausible item, a held ask, or "which
/// item?"), or `none`. Pure by default. `--apply --root <scratch>` runs the REAL
/// write so a scratch project can prove a removal removes and an ask writes nothing.
pub fn run_shopping_language(
    text: &str,
    root: Option<&Path>,
    today: Option<&str>,
    now: Option<&str>,
    calendar_owner: Option<&str>,
    apply: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::fast_lane::{self, Classification, FastLaneResult};

    // `--now` is the full wall-clock pin (the reminder lane's elapsed-clock
    // contract cannot be tested without one); `--today` keeps the older
    // date-only behaviour for every caller that has no clock to pin.
    let now = match now {
        Some(stamp) => Some(
            chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M")
                .with_context(|| format!("--now must be YYYY-MM-DDTHH:MM, got {stamp:?}"))?,
        ),
        None => None,
    };
    let today = match (now, today) {
        (Some(n), _) => n.date(),
        (None, Some(d)) => chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .with_context(|| format!("--today must be YYYY-MM-DD, got {d:?}"))?,
        (None, None) => chrono::Local::now().date_naive(),
    };
    let now = now.unwrap_or_else(|| web_fast_lane_now(today));

    let (lane, item, reply, reason) = match fast_lane::classify_at(text, now) {
        Classification::FastLane(op) => {
            let item = match &op {
                fast_lane::FastLaneOp::ShoppingAdd { item }
                | fast_lane::FastLaneOp::ShoppingRemove { item } => Some(item.clone()),
                _ => None,
            };
            (
                op.kind_label().to_string(),
                item,
                fast_lane::report_line(&op),
                None,
            )
        }
        Classification::Ask { reply, reason } => {
            ("ask".to_string(), None, reply, Some(reason.slug()))
        }
        Classification::Fallback(r) => (
            "none".to_string(),
            None,
            String::new(),
            Some(match r {
                fast_lane::FallbackReason::Compound => "compound",
                fast_lane::FallbackReason::NotASimpleEdit => "not-a-simple-edit",
            }),
        ),
    };

    // The real write, against a SCRATCH project — the live proof seam.
    let applied = if apply {
        let root = root.ok_or_else(|| anyhow::anyhow!("--apply needs --root <project dir>"))?;
        match fast_lane::run_fast_lane_at(root, text, now, calendar_owner) {
            FastLaneResult::Applied {
                report, week_code, ..
            } => Some(serde_json::json!({
                "outcome": "applied",
                "report": report,
                "week": week_code,
            })),
            FastLaneResult::Answered { reply, lane } => Some(serde_json::json!({
                "outcome": "answered",
                "reply": reply,
                "lane": lane,
            })),
            FastLaneResult::Fallback { reason } => Some(serde_json::json!({
                "outcome": "fallback",
                "reason": reason,
            })),
        }
    } else {
        None
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "text": text,
                "lane": lane,
                "item": item,
                "reply": reply,
                "reason": reason,
                "today": today.to_string(),
                "now": now.format("%Y-%m-%dT%H:%M").to_string(),
                "applied": applied,
            }))?
        );
        return Ok(());
    }

    println!("lane:   {lane}");
    if let Some(i) = &item {
        println!("item:   {i}");
    }
    if let Some(r) = reason {
        println!("reason: {r}");
    }
    if !reply.is_empty() {
        println!("reply:  {reply}");
    }
    if let Some(a) = &applied {
        println!("apply:  {a}");
    }
    Ok(())
}
