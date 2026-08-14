//! Casa's reminder tick — `wg telegram remind` (task reminder-engine).
//!
//! Extracted from `commands/telegram.rs` as the third slice of the Casa/upstream split
//! (see docs/UPSTREAM-DIVERGENCE.md). That file is upstream's: 869 lines at our fork
//! point, of which we deleted 677 and added 15,349. Nothing in this tick is a `wg`
//! concern — it reads the reminders the family set, decides which are due, and delivers
//! each on the surface its recipient uses. None of the four items moved here exist
//! upstream at all, at the fork point or on `gwwg/main` today.
//!
//! Borrows from `commands::telegram`, which it does not own: `load_telegram_config`
//! (upstream's) and `project_root`, both already shared; plus `resolve_dm_target`, a
//! Casa DM-routing helper this slice had to expose. That last one is a TEMPORARY seam —
//! it has one caller and two tests still in that file, and it follows them out when the
//! DM path is extracted. It is called out here so the exposure is a recorded debt
//! rather than something a later reader has to infer.

use anyhow::{Context, Result};
use std::path::Path;

use crate::casa::digest::resolve_dm_target;
use crate::casa::reply_delivery::{FamilyReplyDelivery, ReplyScope};
use crate::commands::telegram::{load_telegram_config, project_root};
use worksgood::notify::family_plan;
use worksgood::notify::ownership;
use worksgood::notify::telegram::{TelegramBotConfig, TelegramConfig};

/// Parse a `YYYY-MM-DDTHH:MM` (or space-separated) local wall-clock instant.
pub(crate) fn parse_naive_now(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

/// Resolve which bot fronts a reminder's recipient and the chat to DM.
///
/// A non-empty plan Source is already a roster-resolved stable persona id, so
/// its matching bot must win. If that bot is not configured, fail closed rather
/// than speaking in an unrelated voice. Only a reminder with no Source may use
/// the recipient's explicitly bound bot.
pub(crate) fn resolve_reminder_target(
    config: &TelegramConfig,
    bindings: &worksgood::agency::TelegramBindingMap,
    rem: &worksgood::notify::reminder::Reminder,
) -> Option<(String, String, TelegramBotConfig)> {
    resolve_dm_target(config, bindings, &rem.recipient, &rem.bot)
}

/// Fire due errand nudges as part of the `wg telegram remind` tick.
///
/// Builds errands from the current-week plan, ticks them (exactly-once + drop-if-
/// stale via a dedicated `.casa/errand-fired.json` log), renders each from **live**
/// shopping state fetched *now* (`GET /shopping.json`), routes every firing through
/// the daily-digest pacing layer — time-critical, so under the daily standalone cap
/// it DMs the runner standalone (via the errand's owning bot, e.g. Otto), and over
/// the cap it folds into the morning digest — and returns how many were DM'd.
///
/// A live-shopping fetch failure **skips the whole tick without recording anything**
/// (`Ok(0)`), so the one nudge is never burned on a stale or empty render.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_errands(
    workgraph_dir: &Path,
    root: &Path,
    now: chrono::NaiveDateTime,
    current: Option<&worksgood::notify::family_plan::PlanDoc>,
    members: &[String],
    owner_map: &ownership::OwnerMap,
    bindings: &worksgood::agency::TelegramBindingMap,
    config: &TelegramConfig,
    dry_run: bool,
) -> Result<usize> {
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore, Offer};
    use worksgood::notify::errand::{self, ShoppingModel};
    use worksgood::notify::reminder::{FirePolicy, FiredLog};

    let plan = match current {
        Some(p) => p,
        None => return Ok(0),
    };
    let errands = errand::errands_from_plan(plan, members, owner_map, errand::resolve_lead());
    if errands.is_empty() {
        return Ok(0);
    }

    // Which errands are due now? Own FiredLog namespace so it never collides with
    // the reminder log (ids are `errand:…` vs `⏰` reminder ids anyway).
    let log_path = root.join(".casa").join("errand-fired.json");
    let mut work_log = FiredLog::load(&log_path);
    let firings = errand::errand_tick(&errands, &mut work_log, now, &FirePolicy::default());
    if firings.is_empty() {
        return Ok(0);
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    // Live shopping state, fetched AT FIRE TIME. A failure skips the tick (no nudge
    // burned) — the body must never render from stale/empty state.
    let base = worksgood::notify::telegram_photo::gateway_base_url();
    let url = format!("{base}/shopping.json?back=0");
    let shopping: ShoppingModel = match rt.block_on(async {
        let body = reqwest::get(&url).await?.text().await?;
        Ok::<_, anyhow::Error>(ShoppingModel::from_json(&body))
    }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "[{}] errand tick: live shopping fetch failed ({e:#}) — skipping, no nudge burned",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            return Ok(0);
        }
    };

    if dry_run {
        for f in &firings {
            let body = errand::route_errand_nudge(
                f,
                &shopping,
                &mut DigestStore::default(),
                now,
                &DigestPolicy::new(),
            )
            .0;
            let who = if f.errand.recipient.is_empty() {
                "(group)".to_string()
            } else {
                f.errand.recipient.clone()
            };
            println!(
                "WOULD ERRAND-NUDGE {} via {}: {}",
                who,
                f.errand.bot,
                body.replace('\n', " · "),
            );
        }
        return Ok(0);
    }

    // Real firing: persist the fired-log FIRST (restart-safe exactly-once), then pace + send.
    work_log
        .save(&log_path)
        .with_context(|| format!("failed to persist errand state to {}", log_path.display()))?;

    let digest_path = DigestStore::path(root);
    let mut digest = DigestStore::load(&digest_path);
    let policy = DigestPolicy::new();

    let mut sent = 0usize;
    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, config);
    rt.block_on(async {
        for f in &firings {
            let (_, offer) = errand::route_errand_nudge(f, &shopping, &mut digest, now, &policy);
            match offer {
                Offer::SendNow(text) => {
                    let (target, bot_id, _bot) = match resolve_dm_target(
                        config,
                        bindings,
                        &f.errand.recipient,
                        &f.errand.bot,
                    ) {
                        Some(t) => t,
                        None => {
                            eprintln!(
                                "[{}] no bound bot/chat for errand recipient '{}' — skipping DM",
                                chrono::Utc::now().format("%H:%M:%S"),
                                f.errand.recipient,
                            );
                            continue;
                        }
                    };
                    match family_delivery
                        .send(ReplyScope::Private, &bot_id, &target, &text)
                        .await
                    {
                        Ok(_) => {
                            sent += 1;
                            println!(
                                "[{}] errand-nudged {} via {} ({} still needed)",
                                chrono::Utc::now().format("%H:%M:%S"),
                                f.errand.recipient,
                                bot_id,
                                shopping.remaining_count(),
                            );
                        }
                        Err(e) => eprintln!(
                            "[{}] failed to DM errand to {}: {e:#}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            f.errand.recipient,
                        ),
                    }
                }
                Offer::Queued { overflow } => println!(
                    "[{}] errand for {} folded into the morning digest (overflow={overflow})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    f.errand.recipient,
                ),
                Offer::Pending | Offer::Duplicate => {}
            }
        }
    });

    // Persist the pacing state (standalone counter + any queued overflow) after the tick.
    if let Err(e) = digest.save(&digest_path) {
        eprintln!(
            "[{}] warning: failed to persist digest pacing state: {e:#}",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }
    Ok(sent)
}

/// `wg telegram remind` — the reminder engine's CLI seam.
///
/// Gathers the current plan's reminder rows plus the ad-hoc store, then either
/// lists them (`--list`), shows what would fire at `--now` (`--dry-run`),
/// registers a new ad-hoc reminder (`--add`), READS one back for one family
/// member (`--ask "…" --as <member>`), or — with no flag — fires the due ones for
/// real, DMing each recipient via their bound bot and recording each in the
/// persistent fired-log first so it fires exactly once.
#[allow(clippy::too_many_arguments)]
pub fn run_remind(
    workgraph_dir: &Path,
    list: bool,
    dry_run: bool,
    add: Option<&str>,
    recipient: Option<&str>,
    ask: Option<&str>,
    asker: Option<&str>,
    now_override: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::reminder::{self, AdHocStore, FirePolicy, FiredLog, Reminder};

    let root = project_root(workgraph_dir);
    let owner_map = ownership::OwnerMap::load(&root);
    let coordination_owner = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .unwrap_or_default()
        .to_string();
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Known family members (recipients we can name/DM), from the agency bindings.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let members: Vec<String> = bindings
        .bindings
        .iter()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    // --ask: READ a reminder back for ONE family member (task
    // reminder-readback-lane). The same merge `--list` shows, filtered to the
    // person asking, rendered as the single deterministic line the family sees on
    // chat. Every date comes off disk, so a question that asserts the wrong date
    // is corrected rather than echoed. Nothing is written, nothing is sent.
    if let Some(question) = ask {
        use worksgood::notify::reminder_readback;

        let requester = asker.map(|s| s.trim()).unwrap_or_default();
        if requester.is_empty() {
            anyhow::bail!(
                "--ask needs --as <family member>: the answer is scoped to one person's own \
                 reminders, so there is no safe answer without knowing who is asking"
            );
        }
        let query = match reminder_readback::parse_readback(question) {
            Some(q) => q,
            None => {
                let msg = "That didn't look like a question about a reminder — try \
                           \"when is my dentist reminder?\".";
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "answered": false, "reason": msg })
                    );
                } else {
                    println!("{msg}");
                }
                return Ok(());
            }
        };
        let all = reminder_readback::load_all(&root, &members, &owner_map, now);
        let visible = reminder_readback::visible_to(&all, requester).len();
        let answer = reminder_readback::answer(&all, &query, requester, now);
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "answered": true,
                    "lane": "reminder-read",
                    "requester": requester,
                    "question": question,
                    "answer": answer,
                    "visible": visible,
                    "known": all.len(),
                    "now": now.format("%Y-%m-%dT%H:%M").to_string(),
                })
            );
        } else {
            println!("{answer}");
        }
        return Ok(());
    }

    // --add: register an ad-hoc reminder and print the confirmation.
    if let Some(request) = add {
        let intent = match reminder::parse_reminder_intent(request, now) {
            Some(i) => i,
            None => {
                let msg = "That didn't look like a reminder — try \"remind me Thursday to …\".";
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "registered": false, "reason": msg })
                    );
                } else {
                    println!("{msg}");
                }
                return Ok(());
            }
        };
        // Recipient: explicit, else the first known member, else "you".
        let who = recipient
            .map(|s| s.to_string())
            .or_else(|| members.first().cloned())
            .unwrap_or_else(|| "you".to_string());
        let bot = bindings
            .find_by_name_ci(&who)
            .and_then(|b| b.bot_id.clone())
            .unwrap_or_else(|| coordination_owner.clone());
        let rem = reminder::intent_to_reminder(&intent, &who, &bot);

        let path = AdHocStore::path(&root);
        let mut store = AdHocStore::load(&path);
        let added = store.add(rem.clone());
        store
            .save(&path)
            .with_context(|| format!("failed to persist ad-hoc reminder to {}", path.display()))?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "registered": added,
                    "confirmation": intent.confirmation,
                    "recipient": who,
                    "due": rem.due.format("%Y-%m-%dT%H:%M").to_string(),
                    "text": rem.text,
                })
            );
        } else {
            println!("{}", intent.confirmation);
        }
        return Ok(());
    }

    // Gather the reminder set: current-week plan rows + ad-hoc store.
    let plans = family_plan::load_plans(&root);
    let current = family_plan::current_plan(&plans, now.date());
    let mut reminders: Vec<Reminder> = current
        .map(|p| reminder::reminders_from_plan(p, &members, &owner_map))
        .unwrap_or_default();
    let store = AdHocStore::load(&AdHocStore::path(&root));
    reminders.extend(store.reminders.iter().cloned());
    reminders.sort_by_key(|r| r.due);

    let log_path = FiredLog::path(&root);
    let log = FiredLog::load(&log_path);
    let policy = FirePolicy::default();

    // --list: every reminder with its state, no firing.
    if list {
        if json {
            let rows: Vec<_> = reminders
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "due": r.due.format("%Y-%m-%dT%H:%M").to_string(),
                        "recipient": r.recipient,
                        "bot": r.bot,
                        "text": r.text,
                        "source": format!("{:?}", r.source),
                        "state": log.outcome(&r.id).map(|o| format!("{o:?}")).unwrap_or_else(|| "pending".to_string()),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if reminders.is_empty() {
            println!("No reminders in the current plan or ad-hoc store.");
        } else {
            println!("Reminders ({} total):", reminders.len());
            for r in &reminders {
                let state = match log.outcome(&r.id) {
                    Some(o) => format!("{o:?}").to_lowercase(),
                    None => "pending".to_string(),
                };
                let who = if r.recipient.is_empty() {
                    "(group)".to_string()
                } else {
                    r.recipient.clone()
                };
                println!(
                    "  [{}] {} → {}  ⏰ {}  ({})",
                    state,
                    r.due.format("%a %m-%d %H:%M"),
                    who,
                    r.text,
                    r.bot,
                );
            }
        }
        return Ok(());
    }

    // Decide firings at `now`. We always tick over a clone; `--dry-run` simply
    // never persists it (nor sends), while the real path saves it BEFORE sending.
    let mut work_log = log.clone();
    let result = reminder::tick(&reminders, &mut work_log, now, &policy);

    if dry_run {
        if json {
            let fired: Vec<_> = result
                .fired
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "recipient": f.reminder.recipient,
                        "bot": f.reminder.bot,
                        "late": f.late,
                        "message": f.message(),
                    })
                })
                .collect();
            let dropped: Vec<_> = result
                .dropped
                .iter()
                .map(|r| serde_json::json!({ "id": r.id, "text": r.text }))
                .collect();
            println!(
                "{}",
                serde_json::json!({ "would_fire": fired, "would_drop": dropped })
            );
        } else if result.fired.is_empty() && result.dropped.is_empty() {
            println!("Nothing due at {}.", now.format("%Y-%m-%d %H:%M"));
        } else {
            for f in &result.fired {
                let who = if f.reminder.recipient.is_empty() {
                    "(group)".to_string()
                } else {
                    f.reminder.recipient.clone()
                };
                println!(
                    "WOULD SEND to {} via {}: {}",
                    who,
                    f.reminder.bot,
                    f.message()
                );
            }
            for r in &result.dropped {
                println!("WOULD DROP (too late): ⏰ {}", r.text);
            }
        }
        // Show errands that WOULD nudge too (renders from live shopping state).
        if !json {
            let config = load_telegram_config().unwrap_or_default();
            if let Err(e) = fire_errands(
                workgraph_dir,
                &root,
                now,
                current,
                &members,
                &owner_map,
                &bindings,
                &config,
                true,
            ) {
                eprintln!("errand dry-run skipped: {e:#}");
            }
        }
        return Ok(());
    }

    // Real firing: persist state FIRST (restart-safe exactly-once), then DM.
    work_log
        .save(&log_path)
        .with_context(|| format!("failed to persist reminder state to {}", log_path.display()))?;
    for r in &result.dropped {
        eprintln!(
            "[{}] reminder dropped as stale (>2h late): {}",
            chrono::Utc::now().format("%H:%M:%S"),
            r.text,
        );
    }
    let config = load_telegram_config().unwrap_or_default();
    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, &config);
    let mut sent = 0usize;
    if !result.fired.is_empty() {
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        rt.block_on(async {
            for f in &result.fired {
                let (target, bot_id, _bot) =
                    match resolve_reminder_target(&config, &bindings, &f.reminder) {
                        Some(t) => t,
                        None => {
                            eprintln!(
                                "[{}] no bound bot/chat for reminder recipient '{}' — skipping DM",
                                chrono::Utc::now().format("%H:%M:%S"),
                                f.reminder.recipient,
                            );
                            continue;
                        }
                    };
                match family_delivery
                    .send(ReplyScope::Private, &bot_id, &target, &f.message())
                    .await
                {
                    Ok(_) => {
                        sent += 1;
                        println!(
                            "[{}] reminded {} via {}: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            f.reminder.recipient,
                            bot_id,
                            f.message(),
                        );
                    }
                    Err(e) => eprintln!(
                        "[{}] failed to DM reminder to {}: {e:#}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        f.reminder.recipient,
                    ),
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // Errands ride the same tick: rendered from live shopping state, paced through
    // the daily-digest layer, and DM'd standalone via the configured owning bot.
    let errand_sent = fire_errands(
        workgraph_dir,
        &root,
        now,
        current,
        &members,
        &owner_map,
        &bindings,
        &config,
        false,
    )?;

    if result.fired.is_empty() && errand_sent == 0 && !json {
        println!("Nothing due at {}.", now.format("%Y-%m-%d %H:%M"));
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "fired": result.fired.len(), "sent": sent, "errands_sent": errand_sent })
        );
    }
    Ok(())
}
