//! Casa's lifecycle tick: telling the chat a conversational task came from how it went.
//!
//! Extracted from `commands/telegram.rs` as slice 9 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). Nineteen items, all ours, needing nothing of upstream's —
//! the largest clean cluster the corrected slice test found.

use crate::casa::remind::parse_naive_now;
use crate::casa::reply_delivery::{
    BorrowedReplySink, FamilyReplyDelivery, GuardPolicy, RecordingSink, ReplyScope,
};
use crate::commands::telegram::load_telegram_config;
use crate::commands::telegram::project_root;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use worksgood::notify::ownership;
use worksgood::notify::telegram::TelegramConfig;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LifecycleDeliverySummary {
    pub(crate) sent: usize,
    undelivered: usize,
    pub(crate) alerted: usize,
    pub(crate) rearmed: usize,
}

/// One exact lifecycle suppressor that must be removed before a notification can
/// be retried. Operator alerts have no pacing entry; family report-backs carry
/// the one recipient whose `DigestStore.seen` entry must be reconciled.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub(crate) struct LifecycleRearmEntry {
    notification_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipient: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct LifecycleRearmJournal {
    #[serde(default)]
    pub(crate) entries: Vec<LifecycleRearmEntry>,
}

pub(crate) fn lifecycle_rearm_path(log_path: &Path) -> PathBuf {
    log_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("lifecycle-rearm.json")
}

pub(crate) fn load_lifecycle_rearm_journal(path: &Path) -> Result<LifecycleRearmJournal> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LifecycleRearmJournal::default());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let mut journal: LifecycleRearmJournal = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    for entry in &journal.entries {
        let id = entry.notification_id.trim();
        if id.is_empty()
            || (!id.starts_with("lifecycle:") && !id.starts_with("lifecycle-alert:"))
            || entry
                .recipient
                .as_deref()
                .is_some_and(|recipient| recipient.trim().is_empty())
            || (entry.recipient.is_some() && !id.starts_with("lifecycle:"))
        {
            anyhow::bail!(
                "refusing invalid lifecycle reconciliation entry in {}",
                path.display()
            );
        }
    }
    journal.entries.sort();
    journal.entries.dedup();
    Ok(journal)
}

pub(crate) fn save_lifecycle_rearm_journal(
    path: &Path,
    journal: &LifecycleRearmJournal,
) -> Result<()> {
    let body = serde_json::to_vec_pretty(journal)
        .context("failed to serialize lifecycle reconciliation record")?;
    worksgood::atomic_file::write_atomic(path, body)
        .with_context(|| format!("failed to persist {}", path.display()))
}

pub(crate) fn clear_lifecycle_rearms(path: &Path) -> Result<()> {
    // Atomically replace with an empty journal instead of unlinking. A crash can
    // therefore expose either the complete old set or the complete empty set,
    // never a torn/partly-cleared record.
    save_lifecycle_rearm_journal(path, &LifecycleRearmJournal::default())
}

/// Stage a new exact reconciliation set before either suppressor store changes.
///
/// A non-empty prior journal means startup reconciliation was skipped or failed;
/// overwriting it could lose an older undelivered id, so fail loudly instead.
pub(crate) fn stage_lifecycle_rearms(
    path: &Path,
    mut entries: Vec<LifecycleRearmEntry>,
) -> Result<()> {
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        return Ok(());
    }
    if !load_lifecycle_rearm_journal(path)?.entries.is_empty() {
        anyhow::bail!(
            "pending lifecycle reconciliation in {}; retry after it succeeds",
            path.display()
        );
    }
    save_lifecycle_rearm_journal(path, &LifecycleRearmJournal { entries })
}

pub(crate) fn apply_lifecycle_rearms(
    entries: &[LifecycleRearmEntry],
    log: &mut worksgood::notify::reminder::FiredLog,
    store: &mut worksgood::notify::daily_digest::DigestStore,
) {
    for entry in entries {
        log.rearm(&entry.notification_id);
        if let Some(recipient) = &entry.recipient {
            store.rearm_lifecycle(recipient, &entry.notification_id);
        }
    }
}

/// Finish an interrupted two-file update before computing the next tick.
///
/// The journal contains only exact undelivered notification ids and their one
/// pacing recipient. Reapplying removals is idempotent. The record remains until
/// BOTH state files save, so a failure after either save is recoverable on the
/// following process start without broad replay.
pub(crate) fn reconcile_lifecycle_rearms(
    log_path: &Path,
    store_path: &Path,
    log: &mut worksgood::notify::reminder::FiredLog,
    store: &mut worksgood::notify::daily_digest::DigestStore,
) -> Result<usize> {
    let journal_path = lifecycle_rearm_path(log_path);
    let journal = load_lifecycle_rearm_journal(&journal_path)?;
    if journal.entries.is_empty() {
        return Ok(0);
    }
    apply_lifecycle_rearms(&journal.entries, log, store);
    log.save(log_path).with_context(|| {
        format!(
            "failed to reconcile lifecycle state in {}",
            log_path.display()
        )
    })?;
    store.save(store_path).with_context(|| {
        format!(
            "failed to reconcile lifecycle pacing state in {}",
            store_path.display()
        )
    })?;
    clear_lifecycle_rearms(&journal_path)?;
    Ok(journal.entries.len())
}

pub(crate) fn lifecycle_result_rearms(
    result: &worksgood::notify::lifecycle::LifecycleTickResult,
) -> Vec<LifecycleRearmEntry> {
    let family = result
        .fired
        .iter()
        .chain(result.capped.iter())
        .map(|fire| LifecycleRearmEntry {
            notification_id: worksgood::notify::lifecycle::notification_id(
                &fire.task_id,
                fire.event,
            ),
            recipient: Some(fire.origin.requester.clone()),
        });
    let alerts = result
        .operator_alerts
        .iter()
        .map(|alert| LifecycleRearmEntry {
            notification_id: alert.notification_id.clone(),
            recipient: None,
        });
    family.chain(alerts).collect()
}

/// The persona name(s) doing a task's work, for the "on it" line: the task's
/// assignee display name when it reads like a plain roster name (not an agent
/// content-hash), else the origin persona so the line still names a voice.
pub(crate) fn lifecycle_workers(task: &worksgood::graph::Task) -> Vec<String> {
    // The SAME family-voice gate the composer enforces (morning-taco-bugs): a
    // raw worker id ("agent-2972"), task id, or content hash is not a speakable
    // name, so the "on it" line falls back to the owning persona instead of
    // leaking "Agent-2972 is on it 🍳" into the family group.
    if let Some(a) = task
        .assigned
        .as_deref()
        .filter(|a| worksgood::notify::lifecycle::is_family_safe_name(a))
    {
        return vec![a.to_string()];
    }
    Vec::new()
}

/// The lifecycle report-back's delivery line. The event slug and task id are
/// work-graph identifiers, not household ones — they stay, and they are what an
/// operator actually reads this line for.
pub(crate) fn lifecycle_delivery_line(
    event: &str,
    task_id: &str,
    chat_id: &str,
    bot_id: &str,
    message_id: &str,
    text: &str,
) -> String {
    use worksgood::notify::telegram::{redact_body, redact_chat_id, redact_message_id};
    format!(
        "lifecycle {} for {} → {} via {} ({}): {}",
        event,
        task_id,
        redact_chat_id(chat_id),
        bot_id,
        redact_message_id(message_id),
        redact_body(text),
    )
}

/// The owner's DM chat for a dead-end escalation, and the bot that speaks it.
///
/// Uses only the explicit positive legacy top-level operator chat.
///
/// A helper bot's `chat_id` identifies a conversation, not who owns that
/// conversation. Even when the helper is assigned the coordination domain, a
/// positive id proves only that the target is a DM; it does not prove that the
/// DM belongs to the household operator. Owner-facing alerts contain task
/// details, so every per-helper target fails closed to the loud stderr record.
/// Group/negative/empty legacy targets fail closed too. `None` means there is no
/// proven private operator target; no persona or recipient is guessed.
pub(crate) fn operator_alert_route(
    config: &TelegramConfig,
    _coordination_owner: Option<&str>,
) -> Option<(String, String)> {
    // Legacy single-bot operator chat, only when it is explicitly a private id.
    if !config.bot_token.trim().is_empty()
        && worksgood::notify::telegram::is_dm_chat_id(&config.chat_id)
    {
        return Some((String::new(), config.chat_id.clone()));
    }
    // A positive id proves only that a chat is private, not that its member is
    // the coordination owner. Without an owner-bound or legacy operator target,
    // deliberately fall through to the log-only path.
    None
}

/// Deliver ONE dead-end operator alert — a family-origin task that failed with
/// no retry behind it. The family already heard the honest "I've flagged it"
/// line; this is the flag being raised, so the ask is never a dead end.
///
/// Loud on stderr FIRST (that record survives a missing/broken bot config),
/// then best-effort DM'd through the explicit legacy operator target. Errors are
/// reported, never propagated: a failed escalation must not abort the remaining
/// report-backs.
pub(crate) async fn deliver_operator_alert(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    config: &TelegramConfig,
    coordination_owner: Option<&str>,
    alert: &worksgood::notify::lifecycle::OperatorAlert,
) -> bool {
    // DELIBERATELY NOT REDACTED, unlike the routing lines below it. This is the
    // escalation of last resort: it fires precisely when the DM cannot be sent, and
    // an operator reading it needs to know WHAT the family asked that dead-ended.
    // Reducing it to a hash would leave the alert with nothing to act on. It
    // carries no chat id, no sender id and no message id — the requester is a
    // roster name the operator must be able to read.
    eprintln!(
        "[{}] DEAD-END family ask {} ({}): {}",
        chrono::Utc::now().format("%H:%M:%S"),
        alert.task_id,
        if alert.requester.trim().is_empty() {
            "unknown requester"
        } else {
            alert.requester.trim()
        },
        alert.text,
    );
    let Some((bot_id, chat_id)) = operator_alert_route(config, coordination_owner) else {
        eprintln!(
            "[{}] operator alert for {} not DM'd: no telegram bot/chat configured (logged only)",
            chrono::Utc::now().format("%H:%M:%S"),
            alert.task_id,
        );
        return false;
    };
    match sink.send(&bot_id, &chat_id, &alert.text).await {
        Ok(_) => {
            // The owner's DM chat id is the most personal identifier in this file.
            println!(
                "[{}] operator alert for {} → owner {} via {}",
                chrono::Utc::now().format("%H:%M:%S"),
                alert.task_id,
                worksgood::notify::telegram::redact_chat_id(&chat_id),
                if bot_id.is_empty() {
                    "legacy bot"
                } else {
                    &bot_id
                },
            );
            true
        }
        Err(e) => {
            eprintln!(
                "[{}] operator alert for {} FAILED to send: {}",
                chrono::Utc::now().format("%H:%M:%S"),
                alert.task_id,
                worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
            );
            false
        }
    }
}

/// Deliver ONE lifecycle report-back through the same one-path writer the
/// conversation replies use — the fix for docs/20 ("every origin writes to the
/// ledger; Telegram is a mirror") and the "sends must verify delivery" rule.
///
/// 1. **Send + verify** — `sink.send` resolves the origin persona's bot, calls
///    the Telegram API, and returns `Ok(message_id)` ONLY when the API confirmed
///    `ok:true` (see [`TelegramChannel::api_call`]); any other outcome is `Err`.
///    On a transport/API failure it retries **once** before giving up.
/// 2. **Ledger mirror** — on a confirmed send of a GROUP report-back, it appends
///    an `agent` line to the canonical `.casa/group-feed.jsonl` the constellation
///    pane reads, via the SAME [`casa_feed`] writer the conversation replies use.
///    Gated to group origins (the pane is "our end of the family group chat"); a
///    1:1 DM report-back never leaks into the shared pane. A feed-write failure is
///    logged and swallowed so a full disk can't lose the Telegram delivery.
///
/// Exactly-once is the caller's FiredLog (`notification_id` = the source id,
/// recorded before the send): a delivered `(task, event)` is never re-sent, so
/// the line lands in the pane and in Telegram exactly once. Returns `Ok(())` when
/// delivered, `Err` when BOTH attempts failed — the caller re-arms the FiredLog so
/// a later tick retries rather than the human silently never hearing back.
pub(crate) async fn deliver_lifecycle_fire(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    delivery: &FamilyReplyDelivery,
    fire: &worksgood::notify::lifecycle::LifecycleFire,
) -> Result<()> {
    use worksgood::graph::OriginChannel;
    use worksgood::notify::telegram_conversation::ReplySink as _;

    // Send AS the origin persona's bot (bot_id when known, else the persona id):
    // the reply leaves via the same voice the human addressed, never a wrong face.
    let bot_id = fire
        .origin
        .bot_id
        .clone()
        .unwrap_or_else(|| fire.origin.persona.clone());
    let scope = if matches!(fire.origin.channel, OriginChannel::TelegramGroup) {
        ReplyScope::Group
    } else {
        ReplyScope::Private
    };
    let sink = delivery.wrap(BorrowedReplySink(sink), scope, GuardPolicy::Enforce);

    // DELIVERY VERIFICATION with a single retry. `send` bails on a non-`ok`
    // Telegram response, so `Ok` here means the API accepted the message.
    let mut result = sink.send(&bot_id, &fire.origin.chat_id, &fire.text).await;
    if let Err(first) = &result {
        eprintln!(
            "[{}] lifecycle {} for {} send failed (attempt 1/2), retrying: {}",
            chrono::Utc::now().format("%H:%M:%S"),
            fire.event.slug(),
            fire.task_id,
            worksgood::notify::telegram::redact_bot_token(&format!("{first:#}")),
        );
        result = sink.send(&bot_id, &fire.origin.chat_id, &fire.text).await;
    }
    let message_id = result?.unwrap_or_default();

    println!(
        "[{}] {}",
        chrono::Utc::now().format("%H:%M:%S"),
        lifecycle_delivery_line(
            fire.event.slug(),
            &fire.task_id,
            &fire.origin.chat_id,
            &bot_id,
            &message_id.to_string(),
            &fire.text,
        ),
    );

    Ok(())
}

/// Deliver one tick's report-backs and alerts, then durably re-arm every
/// notification whose transport was not confirmed.
///
/// `lifecycle_tick` records ids before transport so a process crash cannot
/// duplicate a message that Telegram accepted. Once the process is still alive
/// and both attempts have failed, keeping that id would instead turn a transient
/// failure into permanent silence. Remove only the failed id from the fired log
/// and lifecycle pacing set, then persist both before returning so the next tick
/// can try again.
pub(crate) fn deliver_lifecycle_tick_result(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    family_delivery: &FamilyReplyDelivery,
    config: &TelegramConfig,
    coordination_owner: Option<&str>,
    result: &worksgood::notify::lifecycle::LifecycleTickResult,
    log: &mut worksgood::notify::reminder::FiredLog,
    log_path: &Path,
    store: &mut worksgood::notify::daily_digest::DigestStore,
    store_path: &Path,
) -> Result<LifecycleDeliverySummary> {
    let mut summary = LifecycleDeliverySummary::default();
    let mut failed_rearms = Vec::new();
    if result.fired.is_empty() && result.operator_alerts.is_empty() {
        return Ok(summary);
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    rt.block_on(async {
        // DEAD-END ESCALATION FIRST. A family-origin task that failed with no
        // retry behind it just told the family "I've flagged it so it isn't
        // forgotten" — raising the flag is what makes that line true, so it
        // goes out even if a report-back delivery below fails.
        for alert in &result.operator_alerts {
            if deliver_operator_alert(sink, config, coordination_owner, alert).await {
                summary.alerted += 1;
            } else {
                failed_rearms.push(LifecycleRearmEntry {
                    notification_id: alert.notification_id.clone(),
                    recipient: None,
                });
            }
        }
        for fire in &result.fired {
            match deliver_lifecycle_fire(sink, family_delivery, fire).await {
                Ok(()) => summary.sent += 1,
                Err(e) => {
                    // Both attempts failed — surface it LOUDLY (matching the
                    // web-inbound "make failure visible" rule) so a dropped
                    // report-back can never masquerade as delivered in the log.
                    summary.undelivered += 1;
                    let notification_id =
                        worksgood::notify::lifecycle::notification_id(&fire.task_id, fire.event);
                    failed_rearms.push(LifecycleRearmEntry {
                        notification_id,
                        recipient: Some(fire.origin.requester.clone()),
                    });
                    eprintln!(
                        "[{}] UNDELIVERED lifecycle {} for {} after 2 attempts; re-armed for the next tick: {}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        fire.event.slug(),
                        fire.task_id,
                        worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                    );
                }
            }
        }
    });

    if !failed_rearms.is_empty() {
        let journal_path = lifecycle_rearm_path(log_path);
        stage_lifecycle_rearms(&journal_path, failed_rearms.clone())?;
        for entry in &failed_rearms {
            if log.rearm(&entry.notification_id) {
                summary.rearmed += 1;
            }
            if let Some(recipient) = &entry.recipient {
                store.rearm_lifecycle(recipient, &entry.notification_id);
            }
        }
        log.save(log_path).with_context(|| {
            format!(
                "failed to persist re-armed lifecycle state to {}",
                log_path.display()
            )
        })?;
        store.save(store_path).with_context(|| {
            format!(
                "failed to persist re-armed lifecycle pacing state to {}",
                store_path.display()
            )
        })?;
        clear_lifecycle_rearms(&journal_path)?;
    }
    Ok(summary)
}

/// Persist the record-before-transport state as a recoverable two-file update.
///
/// Nothing has been sent yet, so every result id is safe to re-arm if either
/// state save fails. The exact journal is cleared before transport only after
/// both saves succeed.
pub(crate) fn persist_lifecycle_state_before_transport(
    result: &worksgood::notify::lifecycle::LifecycleTickResult,
    log: &worksgood::notify::reminder::FiredLog,
    log_path: &Path,
    store: &worksgood::notify::daily_digest::DigestStore,
    store_path: &Path,
) -> Result<()> {
    let journal_path = lifecycle_rearm_path(log_path);
    let entries = lifecycle_result_rearms(result);
    stage_lifecycle_rearms(&journal_path, entries.clone())?;
    log.save(log_path).with_context(|| {
        format!(
            "failed to persist lifecycle state to {}",
            log_path.display()
        )
    })?;
    store
        .save(store_path)
        .with_context(|| format!("failed to persist pacing state to {}", store_path.display()))?;
    if !entries.is_empty() {
        clear_lifecycle_rearms(&journal_path)?;
    }
    Ok(())
}

/// Report conversational tasks' progress back to the chats they came from — the
/// `wg telegram lifecycle` seam (see [`crate::cli::TelegramCommands::Lifecycle`]).
///
/// Scans origin-stamped tasks (or the single `task_id`), derives each one's
/// start/done/fail event from live status, renders the family-voice line in the
/// composing persona's voice, and fires it exactly once — paced through the
/// daily-digest choke point (time-critical but capped) and delivered to the
/// origin chat via the origin persona's bot. `--dry-run` prints what would be
/// sent where and touches no state; the real path persists the FiredLog +
/// pacing store FIRST (restart-safe), then sends. A delivery that exhausts its
/// retries is removed from both exactly-once stores and persisted again so the
/// next tick retries it.
pub fn run_lifecycle(
    workgraph_dir: &Path,
    task_id: Option<&str>,
    dry_run: bool,
    now_override: Option<&str>,
    json: bool,
    mock_send: bool,
) -> Result<()> {
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore};
    use worksgood::notify::lifecycle::{self, LifecycleInput};
    use worksgood::notify::reminder::FiredLog;
    use worksgood::notify::telegram_conversation::{BotReplySink, ReplySink};

    let root = project_root(workgraph_dir);
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Live graph: gather origin-stamped tasks that owe a notification. A missing
    // graph is not an error — there is simply nothing to report yet.
    let graph_path = crate::commands::graph_path(workgraph_dir);
    let graph = if graph_path.exists() {
        worksgood::parser::load_graph(&graph_path).context("failed to load the task graph")?
    } else {
        worksgood::WorkGraph::new()
    };
    let mut inputs: Vec<LifecycleInput> = Vec::new();
    for task in graph.tasks() {
        if let Some(want) = task_id {
            if task.id != want {
                continue;
            }
        }
        // Workers doing the work, for the "Nora and Bruno are on it" line: the
        // assignee display name when it reads as a name, else the origin persona.
        let workers = lifecycle_workers(task);
        if let Some(input) = LifecycleInput::from_task(task, workers) {
            inputs.push(input);
        }
    }

    let log_path = FiredLog::path(&root);
    let store_path = DigestStore::path(&root);
    let mut log = FiredLog::load(&log_path);
    let mut store = DigestStore::load(&store_path);
    if !dry_run {
        let reconciled = reconcile_lifecycle_rearms(&log_path, &store_path, &mut log, &mut store)?;
        if reconciled > 0 {
            eprintln!(
                "[{}] reconciled {} undelivered lifecycle notification(s) before tick",
                chrono::Utc::now().format("%H:%M:%S"),
                reconciled,
            );
        }
    }
    let policy = DigestPolicy::default();
    let config = load_telegram_config().unwrap_or_default();
    let owner_map = ownership::OwnerMap::load(&root);
    let coordination_owner = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .map(str::to_string);

    if dry_run {
        // Compute against throwaway copies so a dry run records nothing.
        let mut dry_log = log.clone();
        let mut dry_store = store.clone();
        let result = lifecycle::lifecycle_tick(&inputs, &mut dry_log, &mut dry_store, now, &policy);
        if json {
            let rows: Vec<_> = result
                .fired
                .iter()
                .chain(result.capped.iter())
                .map(|f| {
                    serde_json::json!({
                        "task": f.task_id,
                        "event": f.event.slug(),
                        "chat": f.origin.chat_id,
                        "persona": f.origin.persona,
                        "bot": f.origin.bot_id,
                        "text": f.text,
                        "capped": result.capped.iter().any(|c| c.task_id == f.task_id && c.event == f.event),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            let alerts: Vec<_> = result
                .operator_alerts
                .iter()
                .map(|a| {
                    let route =
                        operator_alert_route(&config, coordination_owner.as_deref());
                    serde_json::json!({
                        "task": a.task_id,
                        "operator_alert": true,
                        "requester": a.requester,
                        "bot": route.as_ref().map(|(bot, _)| if bot.is_empty() { "legacy bot" } else { bot.as_str() }),
                        "chat": route.as_ref().map(|(_, chat)| chat.as_str()),
                        "text": a.text,
                    })
                })
                .collect();
            if !alerts.is_empty() {
                println!("{}", serde_json::to_string_pretty(&alerts)?);
            }
        } else if result.fired.is_empty()
            && result.capped.is_empty()
            && result.operator_alerts.is_empty()
        {
            println!(
                "Nothing to report at {} (family-local; the telegram.log delivery lines are UTC).",
                now.format("%Y-%m-%d %H:%M")
            );
        } else {
            for f in &result.fired {
                println!("{}", lifecycle::dry_run_line(f));
            }
            for f in &result.capped {
                println!(
                    "[dry-run] (capped → folds into digest) {}",
                    lifecycle::dry_run_line(f)
                );
            }
            for a in &result.operator_alerts {
                let route = operator_alert_route(&config, coordination_owner.as_deref());
                println!(
                    "{}",
                    lifecycle::dry_run_alert_line(a, route.as_ref().map(|(bot, _)| bot.as_str()),)
                );
            }
        }
        return Ok(());
    }

    // Real firing: persist exactly-once + pacing state FIRST, then deliver.
    let result = lifecycle::lifecycle_tick(&inputs, &mut log, &mut store, now, &policy);
    persist_lifecycle_state_before_transport(&result, &log, &log_path, &store, &store_path)?;

    let config = load_telegram_config().unwrap_or_default();
    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, &config);
    // The ONE-PATH writer: lifecycle report-backs leave through the same
    // `ReplySink` the conversation replies use, so a group report-back both
    // reaches Telegram AND lands in the canonical `.casa/group-feed.jsonl` the
    // constellation pane reads — no more sends that bypass the ledger (docs/20).
    // `--mock-send` swaps in a network-free recorder so the cross-surface smoke
    // exercises the real tick + real feed mirror without a live bot.
    let sink: Box<dyn ReplySink> = if mock_send {
        Box::new(RecordingSink::default())
    } else {
        Box::new(BotReplySink::new(config.clone()))
    };
    let delivery_summary = deliver_lifecycle_tick_result(
        sink.as_ref(),
        &family_delivery,
        &config,
        coordination_owner.as_deref(),
        &result,
        &mut log,
        &log_path,
        &mut store,
        &store_path,
    )?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "fired": result.fired.len(),
                "sent": delivery_summary.sent,
                "undelivered": delivery_summary.undelivered,
                "capped": result.capped.len(),
                "operator_alerts": result.operator_alerts.len(),
                "operator_alerts_sent": delivery_summary.alerted,
            })
        );
    } else if result.fired.is_empty()
        && result.capped.is_empty()
        && result.operator_alerts.is_empty()
    {
        println!(
            "Nothing to report at {} (family-local; the telegram.log delivery lines are UTC).",
            now.format("%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}
