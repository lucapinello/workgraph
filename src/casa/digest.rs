//! Casa's daily digest tick and the DM routing it needs (task daily-digest-pacing-layer).
//!
//! Extracted from `commands/telegram.rs` as slice 5 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). Nothing here is a `wg` concern: it decides what the family
//! is told once a day, and which bot fronts each recipient.
//!
//! THIS SLICE PAYS OFF A RECORDED DEBT. Slice 3 had to expose `resolve_dm_target` as
//! `pub(crate)` in upstream's file so `casa::remind` could reach it, and said it would
//! "follow the DM path out when that path is extracted". This is that extraction:
//! `run_digest` was its last caller there, so the function moves here, its two tests come
//! with it, and the marker in their file goes away.

use crate::casa::remind::parse_naive_now;
use crate::casa::reply_delivery::{
    BorrowedReplySink, FamilyReplyDelivery, GuardPolicy, RecordingSink, ReplyScope,
};
use anyhow::{Context, Result};
use std::path::Path;
use worksgood::notify::ownership;
use worksgood::notify::telegram::{TelegramBotConfig, TelegramConfig};

use crate::commands::telegram::{load_telegram_config, project_root};

/// The project-authored voice for proactive coordination messages.
///
/// An empty result is intentional: callers then persist no guessed persona and
/// [`resolve_dm_target`] may use only the recipient's explicit bot binding.
pub(crate) fn coordination_owner_hint(root: &Path) -> String {
    ownership::OwnerMap::load(root)
        .owner_for_domain(ownership::Domain::Coordination)
        .unwrap_or_default()
        .to_string()
}

/// Resolve the DM target (chat + bot) for a proactive nudge to `recipient`, sent
/// in the stable voice `bot`. A resolved Source voice wins; if it has no
/// configured bot, delivery fails closed. With an empty Source only, the
/// recipient's explicit bot binding may send. Shared by the reminder and errand
/// ticks so neither path depends on roster or map iteration order.
// EXPOSED FOR `casa::remind` (slice 3 of the Casa/upstream split). Temporary: this
// helper is Casa's, not upstream's, and it follows the DM path out of this file when
// that path is extracted. Its one caller and two tests are still here.
pub(crate) fn resolve_dm_target(
    config: &TelegramConfig,
    bindings: &worksgood::agency::TelegramBindingMap,
    recipient: &str,
    bot: &str,
) -> Option<(String, String, TelegramBotConfig)> {
    // A queue key is whatever the sender held: usually a binding name, sometimes
    // the raw telegram user id. Both name the same human, so both must resolve —
    // a name-only lookup turns an id-keyed bucket into an undeliverable orphan.
    let binding = bindings
        .find_by_name_ci(recipient)
        .or_else(|| bindings.find_by_user(recipient))?;
    let target = binding.telegram_user.clone();
    let bots = config.all_bots();
    if !bot.trim().is_empty() {
        return bots
            .iter()
            .find(|(id, b)| {
                id.eq_ignore_ascii_case(bot) || {
                    b.agent_id
                        .as_deref()
                        .is_some_and(|agent_id| agent_id.eq_ignore_ascii_case(bot))
                }
            })
            .map(|(id, b)| (target, id.clone(), b.clone()));
    }
    binding.bot_id.as_ref().and_then(|bound_id| {
        bots.iter()
            .find(|(id, _)| id == bound_id)
            .map(|(id, b)| (target, id.clone(), b.clone()))
    })
}

/// Deliver one private morning digest through the same scoped writer lifecycle
/// report-backs use (see [`deliver_lifecycle_fire`]): guard, send, verify, and
/// retry once.
///
/// The digest is delivered to one member's bound 1:1 chat. The shared
/// conversation feed is the family group's history, so this private text must
/// never be copied there.
/// Returns `Ok(())` on confirmed delivery, `Err` when BOTH send attempts failed
/// (the caller then leaves the pending queue intact for the next tick).
pub(crate) async fn deliver_digest_fire(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    delivery: &FamilyReplyDelivery,
    bot_id: &str,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    use worksgood::notify::telegram_conversation::ReplySink as _;

    // DELIVERY VERIFICATION with a single retry (matches the lifecycle path).
    let sink = delivery.wrap(
        BorrowedReplySink(sink),
        ReplyScope::Private,
        GuardPolicy::Enforce,
    );
    let mut result = sink.send(bot_id, chat_id, text).await;
    if let Err(first) = &result {
        eprintln!(
            "[{}] digest send to {} failed (attempt 1/2), retrying: {}",
            chrono::Utc::now().format("%H:%M:%S"),
            chat_id,
            worksgood::notify::telegram::redact_bot_token(&format!("{first:#}")),
        );
        result = sink.send(bot_id, chat_id, text).await;
    }
    let _message_id = result?.unwrap_or_default();

    Ok(())
}

/// `wg telegram digest` — flush each family member's ONE calm morning digest.
///
/// This is the missing production caller the daily 12:00 UTC `daily-digest` cron
/// runs (task `re-arm-the`). The digest ENGINE (`DigestStore`, one-calm-daily)
/// already accumulates every bundled + overflow proactive item per person, but
/// nothing ever EMITTED the bundled morning message: `emit_digest` had no
/// caller, so even when the cron fired it sent nothing (and the cron itself was
/// registered with an empty description, so a cleanup sweep read it as junk and
/// abandoned it — the friendly fire this task fixes).
///
/// For each known member whose digest is due at `now` — past the digest hour,
/// out of quiet hours, pending non-empty, not already sent today (see
/// [`DigestStore::digest_due`]) — compose the calm `Today: …` line, deliver it
/// through the SAME scoped writer the lifecycle report-backs use
/// ([`deliver_digest_fire`]: guard, send, verify, and retry once), and — ONLY on a
/// confirmed delivery — mark the digest sent + clear the queue. A failed send
/// leaves the queue intact so the next tick retries; at most one per person/day.
///
/// `--dry-run` prints what would go to whom and touches no state. `--mock-send`
/// runs the real tick against a network-free recorder so a smoke/test proves the
/// guarded private-delivery path without a bot.
pub fn run_digest(
    workgraph_dir: &Path,
    dry_run: bool,
    now_override: Option<&str>,
    json: bool,
    mock_send: bool,
) -> Result<()> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::daily_digest::{DigestItem, DigestPolicy, DigestStore, compose_digest};
    use worksgood::notify::telegram_conversation::{BotReplySink, ReplySink};

    /// One morning message and every queue bucket that feeds it. Usually one
    /// bucket, but a person keyed two ways (name and raw telegram id) collapses
    /// into a single group so they still receive ONE calm digest.
    struct DueGroup {
        /// Identity of the chat this reaches — the grouping key.
        route: String,
        /// The name shown in logs and `--dry-run`.
        label: String,
        /// Every store key folded in; ALL are marked sent on a confirmed delivery.
        keys: Vec<String>,
        items: Vec<DigestItem>,
    }

    let root = project_root(workgraph_dir);
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Known family members (recipients we can name/DM), from the agency bindings.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let mut members: Vec<String> = bindings
        .bindings
        .iter()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    let config = load_telegram_config().unwrap_or_default();
    let coordination_owner = coordination_owner_hint(&root);
    let policy = DigestPolicy::default();
    let store_path = DigestStore::path(&root);
    let mut store = DigestStore::load(&store_path);

    // The roster above names only bindings, but `DigestStore::offer` keys the queue
    // by whatever string the SENDER held — a binding name, a raw telegram id, or the
    // literal role `operator` that `spawn_breaker` queues under. A name-only reader
    // never visits those buckets, so their items are told "folded into the next
    // digest" and then dropped forever. Union in every key that actually holds
    // something: a key that still cannot be routed now fails LOUDLY below (one
    // "no bound bot/chat" line per tick) instead of silently.
    for key in store.queued_recipients() {
        if !members.iter().any(|m| m.eq_ignore_ascii_case(&key)) {
            members.push(key);
        }
    }

    // Peek (without mutating) each member's due digest so a --dry-run and the
    // real send agree on exactly what would go out. Buckets are grouped by the
    // chat they actually reach, so one person keyed two ways still gets ONE
    // message rather than a duplicate pair.
    let mut groups: Vec<DueGroup> = Vec::new();
    for m in &members {
        if !store.digest_due(m, now, &policy) {
            continue;
        }
        let Some(items) = store.state(m).map(|st| st.pending().to_vec()) else {
            continue;
        };
        // Unroutable keys group under themselves so they never merge with a real
        // recipient's message.
        let route = resolve_dm_target(&config, &bindings, m, &coordination_owner)
            .map(|(chat, bot_id, _)| format!("{bot_id}/{chat}"))
            .unwrap_or_else(|| format!("unrouted:{m}"));
        match groups.iter_mut().find(|g| g.route == route) {
            Some(g) => {
                g.keys.push(m.clone());
                g.items.extend(items);
            }
            None => groups.push(DueGroup {
                route,
                label: m.clone(),
                keys: vec![m.clone()],
                items,
            }),
        }
    }
    let due: Vec<(DueGroup, String)> = groups
        .into_iter()
        .map(|g| {
            let text = compose_digest(&g.items);
            (g, text)
        })
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();

    if dry_run {
        if json {
            let rows: Vec<_> = due
                .iter()
                .map(|(g, text)| serde_json::json!({ "recipient": g.label, "text": text }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if due.is_empty() {
            println!(
                "No digest due at {} (nothing pending, already sent, or quiet hours).",
                now.format("%Y-%m-%d %H:%M")
            );
        } else {
            for (g, text) in &due {
                println!("WOULD DIGEST to {}: {}", g.label, text.replace('\n', " · "));
            }
        }
        return Ok(());
    }

    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, &config);
    // `--mock-send` swaps in a network-free recorder so the private delivery
    // path is exercised without a live bot.
    let sink: Box<dyn ReplySink> = if mock_send {
        Box::new(RecordingSink::default())
    } else {
        Box::new(BotReplySink::new(config.clone()))
    };

    let mut sent = 0usize;
    let mut undelivered = 0usize;
    if !due.is_empty() {
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        rt.block_on(async {
            for (group, text) in &due {
                let member = &group.label;
                // Resolve the recipient's DM target through the project-authored
                // coordination owner. Without one, only the recipient's explicit
                // bot binding may send; roster/map order is never a fallback.
                let (target, bot_id, _bot) =
                    match resolve_dm_target(&config, &bindings, member, &coordination_owner) {
                        Some(t) => t,
                        None => {
                            eprintln!(
                                "[{}] no bound bot/chat for digest recipient '{}' — {} item(s) STAY QUEUED for the next tick",
                                chrono::Utc::now().format("%H:%M:%S"),
                                member,
                                group.items.len(),
                            );
                            undelivered += 1;
                            continue;
                        }
                    };
                match deliver_digest_fire(sink.as_ref(), &family_delivery, &bot_id, &target, text)
                    .await
                {
                    Ok(()) => {
                        // Confirmed delivery: NOW mark today's digest sent and
                        // clear the queue (restart-safe — a failed send above
                        // leaves the queue intact for the next tick to retry).
                        // EVERY bucket folded into this one message is drained,
                        // or a merged-in key would re-send its items tomorrow.
                        for key in &group.keys {
                            store.emit_digest(key, now, &policy);
                        }
                        sent += 1;
                        println!(
                            "[{}] digest → {} via {}: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            member,
                            bot_id,
                            text.replace('\n', " · "),
                        );
                    }
                    Err(e) => {
                        undelivered += 1;
                        eprintln!(
                            "[{}] UNDELIVERED digest for {} after 2 attempts: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            member,
                            worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                        );
                    }
                }
            }
        });
    }

    // Persist the pacing store after the tick (digest_sent flags + drained queues
    // for confirmed deliveries; untouched for failed ones).
    store
        .save(&store_path)
        .with_context(|| format!("failed to persist digest state to {}", store_path.display()))?;

    if json {
        println!(
            "{}",
            serde_json::json!({ "due": due.len(), "sent": sent, "undelivered": undelivered })
        );
    } else if due.is_empty() {
        println!(
            "No digest due at {} (nothing pending, already sent, or quiet hours).",
            now.format("%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use worksgood::agency::{TelegramBinding, TelegramBindingMap};

    /// One confirmed human, fronted by one configured bot.
    fn fixture() -> (TelegramConfig, TelegramBindingMap) {
        let bindings = TelegramBindingMap {
            bindings: vec![TelegramBinding::new(
                "8905220378",
                "human-one",
                "Household Member",
                Some("casa".to_string()),
                chrono::Utc::now(),
            )],
        };
        let mut bots = HashMap::new();
        bots.insert(
            "casa".to_string(),
            TelegramBotConfig {
                bot_token: "100:AAA".to_string(),
                chat_id: "-1001".to_string(),
                agent_id: Some("coordination-relay".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        (config, bindings)
    }

    #[test]
    fn a_bucket_keyed_by_raw_telegram_id_reaches_the_same_human() {
        // The live store carries TWO keys for one person — the binding name and the
        // raw telegram id — because writers key by whatever string they hold. A
        // name-only lookup makes the id-keyed bucket an undeliverable orphan whose
        // items are queued forever and read by nobody.
        let (config, bindings) = fixture();

        // POSITIVE CONTROL: the name key resolves. If this fails the fixture is
        // wrong, so the id assertion below cannot pass for the wrong reason.
        let by_name = resolve_dm_target(&config, &bindings, "Household Member", "")
            .expect("the binding name must resolve");
        let by_id = resolve_dm_target(&config, &bindings, "8905220378", "")
            .expect("an id-keyed bucket must resolve to the human it names");

        assert_eq!(
            (by_name.0, by_name.1),
            (by_id.0, by_id.1),
            "both keys name one person, so both must reach one chat via one bot",
        );
    }

    #[test]
    fn a_role_key_names_nobody_and_still_fails_closed() {
        // Widening the lookup must not turn `operator` into "whichever binding is
        // first". An unroutable key stays unroutable — the caller then leaves its
        // items queued and says so, rather than DMing a family member an alert
        // meant for whoever runs the house.
        let (config, bindings) = fixture();
        assert!(
            resolve_dm_target(&config, &bindings, "operator", "").is_none(),
            "a role key must never be delivered to an arbitrary human",
        );
    }
}
