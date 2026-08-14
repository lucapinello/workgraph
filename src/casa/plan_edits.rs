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
use worksgood::notify::telegram_conversation::durable_telegram_digest_v1;

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

// ── slice 9: starting a week is a plan edit too ───────────────────────────────────────
// The WebFastLane types travel with it. They are the same items whose absence broke the
// first attempt at the web-inbound cluster: they are ours, so they move, and the
// web-inbound code still in upstream's file imports them back.

pub(crate) const WEB_FAST_LANE_OCCURRENCE_DOMAIN: &str = "web-fast-lane";

/// Exact restart payload for one web fast-lane mutation.
///
/// The mutation's report is guarded before this value is persisted, so an
/// `applied` replay sends these exact bytes and never re-runs classification,
/// plan editing, graph stamping, election, or target selection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebFastLaneOutcome {
    pub(crate) op_kind: String,
    pub(crate) report: String,
    pub(crate) bot_id: String,
    pub(crate) chat_id: String,
}

/// Physical-turn key for a gateway-originated group turn.
///
/// A supplied opaque occurrence id distinguishes two later turns with identical
/// words. Missing ids retain the legacy chat + trimmed-body fallback so older
/// gateways remain compatible. The returned fingerprint never exposes the
/// occurrence id, the attempt id, or the message body.
///
/// `attempt_id` is the gateway's canonical ATTEMPT id for this delivery of that
/// occurrence, and it is why the key is `(turn, attempt)` rather than `turn`
/// alone. The two ids answer different questions:
///
///   · the same `(turn, attempt)` arriving twice is one physical delivery
///     redelivered — a dispatcher refire — and the stored outcome must win;
///   · a NEW attempt on the same turn is the gateway SELF-HEALING a delivery
///     that died before the family got an answer. Under a turn-only key that
///     retry matches the dead attempt's ledger entry and is dropped as
///     "already answered", so the self-heal heals nothing and the household is
///     left with the silence it was retrying.
///
/// An absent or blank attempt id keeps the pre-attempt digest material byte for
/// byte, so ledger entries an older gateway already wrote keep replaying.
///
/// One contract, three layers, all keyed the same way from `WG_ATTEMPT_ID`:
/// this key (compose dedupe + the fast-lane/week-start mutation journal),
/// [`worksgood::notify::relay_receipt::attempt_key`] (the delivery receipt
/// ledger), and — deliberately turn-only — the final-answer reservation in
/// [`worksgood::notify::telegram_conversation`], where a late original and a
/// self-heal retry must still produce exactly ONE final message. Admitting the
/// retry here and holding the line there is the point: the retry gets to answer,
/// the family does not get answered twice.
pub(crate) fn web_physical_turn_key(
    reply_chat: &str,
    body: &str,
    turn_id: Option<&str>,
    attempt_id: Option<&str>,
) -> String {
    let (kind, occurrence) = turn_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| ("id", id))
        .unwrap_or_else(|| ("body", body.trim()));
    let digest = match attempt_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(attempt) => durable_telegram_digest_v1(
            "web-physical-turn",
            &[reply_chat, kind, occurrence, "attempt", attempt],
        ),
        None => durable_telegram_digest_v1("web-physical-turn", &[reply_chat, kind, occurrence]),
    };
    format!("web-turn-{digest}")
}

/// Fulfil an accepted week-start offer — the `wg telegram week-start` seam
/// (see [`crate::cli::TelegramCommands::WeekStart`]).
///
/// Runs the exact lane the gateway's dispatched acceptance hits. Without
/// `--apply` it only reports what the engine RECOGNIZES (is this a week-start
/// ask, and what requests does it carry). With `--apply --root <scratch>` it
/// really drafts the week: the plan is assembled, edited with every carried
/// request and verified IN MEMORY, written only if all of that held, and then
/// re-read from disk before this command reports success — so a dead pipeline
/// cannot claim a week it never wrote.
///
/// The draft is journaled against `(turn, attempt)` exactly as the live web turn
/// is, so a dispatcher refire carrying the SAME occurrence AND attempt replays
/// the stored outcome instead of drafting twice, while a gateway self-heal retry
/// — same occurrence, NEW attempt — is answered rather than suppressed.
/// Credential-free throughout.
pub fn run_week_start(
    workgraph_dir: &Path,
    message: &str,
    root: Option<&Path>,
    now: Option<&str>,
    apply: bool,
    turn_id: Option<&str>,
    attempt_id: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::notify::fast_lane::{self, FastLaneResult};
    use worksgood::notify::telegram_occurrence::{OccurrenceJournal, OccurrenceState};
    use worksgood::notify::week_start;

    let today = match now {
        Some(d) => {
            // Accept both a bare date and the `--now` wall-clock form the other
            // pinned seams take, so a scratch run can pin the same string.
            let day = d.split(['T', ' ']).next().unwrap_or(d);
            chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .with_context(|| format!("--now must be YYYY-MM-DD[THH:MM], got {d:?}"))?
        }
        None => chrono::Local::now().date_naive(),
    };

    let ask = week_start::detect(message);
    let (week_code, monday, sunday) = week_start::iso_week_of(today);

    // The correlation key the live turn builds — the SAME derivation, so what
    // this seam proves about dedupe is true of the live turn. `--turn-id` (or
    // WG_TURN_ID) is the gateway's opaque occurrence id; without one the words
    // are the key, the same legacy fallback `web-inbound` keeps for older
    // callers. `--attempt-id` (or WG_ATTEMPT_ID) is the canonical attempt id for
    // this delivery of that occurrence: same (turn, attempt) is a refire and
    // replays, a new attempt on the same turn is a self-heal and is answered.
    let physical_turn_key = web_physical_turn_key("web", message, turn_id, attempt_id);

    let mut applied: Option<serde_json::Value> = None;
    if apply {
        let root = root.ok_or_else(|| anyhow::anyhow!("--apply needs --root <project dir>"))?;
        if ask.is_none() {
            anyhow::bail!("not a week-start ask — nothing to apply");
        }
        // The occurrence journal belongs to the PROJECT whose week is being
        // drafted, not to whatever directory the command was invoked from —
        // otherwise two scratch projects (or two runs of a test) would share one
        // turn ledger and the second would replay the first one's outcome.
        //
        // THE HOLE THAT WAS IN THAT: the scoping only held for a project that
        // ALREADY had a `.wg/`, and a scratch project has none — so every scratch
        // run fell back to the ambient workgraph dir and they all shared one
        // ledger. Observed: the same ask, run against a FRESH project, replayed a
        // previous project's outcome and reported "this week's plan is started"
        // with no plan file anywhere on this disk. Create the journal home under
        // the root instead; a project's turn ledger is the project's.
        let journal_dir = root.join(".wg");
        if !journal_dir.is_dir() {
            std::fs::create_dir_all(&journal_dir).with_context(|| {
                format!("create the occurrence journal at {}", journal_dir.display())
            })?;
        }
        let _ = workgraph_dir;
        let (journal, state) = OccurrenceJournal::<WebFastLaneOutcome>::claim(
            &journal_dir,
            WEB_FAST_LANE_OCCURRENCE_DOMAIN,
            &physical_turn_key,
        )?;
        // A replay is only honest if the thing it claims to have done is STILL
        // THERE. A week-start's whole outcome is one file; if that file is absent,
        // "already started" is a lie told with a ledger entry as its evidence —
        // the dead-pipeline-claims-success failure, arriving through the
        // idempotency guard instead of around it. So a replay whose plan is gone
        // is not a replay: the week is drafted, which is what the family asked
        // for and what the ledger says already happened.
        let plan_path = root
            .join("plans")
            .join(format!("{week_code}-family-plan.md"));
        let replay_is_honest = plan_path.exists();
        let state = match state {
            OccurrenceState::Applied(prior) | OccurrenceState::Delivered(prior)
                if !replay_is_honest =>
            {
                let _ = prior;
                OccurrenceState::New
            }
            other => other,
        };
        applied = Some(match state {
            // The SAME accepted turn arriving again (a dispatcher refire): the
            // durable outcome wins and nothing is drafted a second time.
            OccurrenceState::Applied(prior) | OccurrenceState::Delivered(prior) => {
                serde_json::json!({
                    "outcome": "replayed",
                    "already_delivered": true,
                    "op": prior.op_kind,
                    "report": prior.report,
                    "plan_exists": true,
                })
            }
            OccurrenceState::Incomplete => serde_json::json!({
                "outcome": "incomplete",
                "already_delivered": false,
            }),
            OccurrenceState::PassedThrough => serde_json::json!({
                "outcome": "passed-through",
                "already_delivered": false,
            }),
            OccurrenceState::New => {
                let owner_map = worksgood::notify::ownership::OwnerMap::load(root);
                let owner =
                    owner_map.owner_for_domain(worksgood::notify::ownership::Domain::Calendar);
                match fast_lane::run_fast_lane_at(
                    root,
                    message,
                    crate::casa::plan_edits::web_fast_lane_now(today),
                    owner.as_deref(),
                ) {
                    FastLaneResult::Applied {
                        report,
                        op,
                        week_code,
                    } => {
                        let outcome = WebFastLaneOutcome {
                            op_kind: op.kind_label().to_string(),
                            report: report.clone(),
                            bot_id: String::new(),
                            chat_id: "web".to_string(),
                        };
                        journal.mark_applied(&outcome)?;
                        // Report the PLAN THAT IS ON DISK, re-read here, rather
                        // than the lane's own account of what it did.
                        let path = root
                            .join("plans")
                            .join(format!("{week_code}-family-plan.md"));
                        let written = std::fs::read_to_string(&path).ok();
                        let doc = written
                            .as_deref()
                            .map(|c| worksgood::notify::family_plan::PlanDoc::parse(&week_code, c));
                        serde_json::json!({
                            "outcome": "applied",
                            "already_delivered": false,
                            "report": report,
                            "week": week_code,
                            "plan_path": path.display().to_string(),
                            "plan_exists": path.exists(),
                            "day_rows": doc.as_ref().map(|d| d.meals.len()).unwrap_or(0),
                            "dinners": doc
                                .as_ref()
                                .map(|d| d
                                    .meals
                                    .iter()
                                    .map(|m| serde_json::json!({"day": m.weekday, "dish": m.dish}))
                                    .collect::<Vec<_>>())
                                .unwrap_or_default(),
                        })
                    }
                    FastLaneResult::Answered { reply, lane } => {
                        let outcome = WebFastLaneOutcome {
                            op_kind: format!("ask-{lane}"),
                            report: reply.clone(),
                            bot_id: String::new(),
                            chat_id: "web".to_string(),
                        };
                        journal.mark_applied(&outcome)?;
                        serde_json::json!({
                            "outcome": "answered",
                            "already_delivered": false,
                            "lane": lane,
                            "reply": reply,
                        })
                    }
                    FastLaneResult::Fallback { reason } => {
                        journal.mark_passed_through()?;
                        serde_json::json!({
                            "outcome": "fallback",
                            "already_delivered": false,
                            "reason": reason,
                        })
                    }
                }
            }
        });
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "message": message,
                "lane": if ask.is_some() { "week-start" } else { "none" },
                "recognized": ask.is_some(),
                "carried": ask.as_ref().map(|a| a.carried.clone()).unwrap_or_default(),
                "today": today.to_string(),
                "week": week_code,
                "week_start": monday.to_string(),
                "week_end": sunday.to_string(),
                "turn_id": turn_id,
                "attempt_id": attempt_id,
                "turn_key": physical_turn_key,
                "applied": applied,
            }))?
        );
        return Ok(());
    }

    println!(
        "lane:    {}",
        if ask.is_some() { "week-start" } else { "none" }
    );
    if let Some(a) = &ask {
        for c in &a.carried {
            println!("carried: {c}");
        }
    }
    println!("week:    {week_code} ({monday} – {sunday})");
    println!("turn:    {physical_turn_key}");
    if let Some(a) = &applied {
        println!("apply:   {a}");
    }
    Ok(())
}
