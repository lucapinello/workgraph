//! Casa's photo → shopping-list pipeline (task photo-to-shopping-vision-pipeline).
//!
//! Extracted from `commands/telegram.rs` as the first slice of the Casa/upstream split
//! (see docs/UPSTREAM-DIVERGENCE.md). That file is upstream's: 869 lines at our fork
//! point, of which we deleted 677 and added 15,349. Every Casa item living there is a
//! merge conflict waiting for the next sync, and nothing in this pipeline is a `wg`
//! concern — it reads a photo a family member sent, turns it into shopping rows, and
//! answers on the surface the photo arrived on.
//!
//! Borrows three items from `commands::telegram` that it does not own:
//! `load_telegram_config` (upstream's), plus `project_root` and `human_agent_id_set`,
//! shared Casa helpers still awaiting a home of their own.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use worksgood::notify::NotificationChannel;
use worksgood::notify::ownership;
use worksgood::notify::telegram::{TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_conversation::durable_telegram_digest_v1;
use worksgood::notify::telegram_group::{
    Election, elect_responders_with_owner_map, parse_at_mention_tokens,
};

use crate::casa::reply_delivery::{FamilyReplyDelivery, GuardPolicy, ReplyScope};
use crate::commands::telegram::{human_agent_id_set, load_telegram_config, project_root};
/// Run one photo → shopping-list vision turn end-to-end for an inbound photo
/// that has ELECTED to a persona (task `photo-to-shopping`). Downloads the image
/// with the RECEIVING bot's token (the `file_id` is bot-specific), reads the
/// current shopping list from the gateway, runs the vision compose turn grounded
/// in the elected persona's voice, applies the implied list changes through the
/// SAME gateway endpoints the kiosk taps, and replies in-persona. Awaited inline
/// (photos are infrequent; correctness over throughput) — it fails fast into the
/// gentle note on any error so the family never sees a hang.
pub(crate) async fn handle_photo_shopping_turn(
    workgraph_dir: &Path,
    msg: &worksgood::notify::IncomingMessage,
    plan: &worksgood::notify::telegram_conversation::ConversationPlan,
    channels: &[TelegramChannel],
    route_config: &TelegramConfig,
    wg_config: Option<&worksgood::config::Config>,
) -> Result<()> {
    use worksgood::notify::grounding;
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_photo as photo;

    let turn = match photo::coalesce_album(std::slice::from_ref(msg))
        .into_iter()
        .next()
    {
        Some(t) => t,
        None => return Ok(()), // not a photo (shouldn't happen — caller gated)
    };

    let route = plan.route();
    let family_roster =
        grounding::load_family_voice_roster(&project_root(workgraph_dir), workgraph_dir);
    let delivery = FamilyReplyDelivery::load(workgraph_dir, route_config);
    // The occurrence helper applies the family-voice guard before it journals
    // the canonical reply. Preserve those exact bytes through transport and
    // feed mirroring; a replay must never re-finalize into different copy.
    let sink = delivery.wrap(
        convo::BotReplySink::new(route_config.clone()),
        ReplyScope::from_chat_type(msg.chat_type.as_deref()),
        GuardPolicy::AlreadyGuarded,
    );
    let physical_turn_key = telegram_photo_turn_key(msg);

    // No model config → we can't run vision. Acknowledge gracefully rather than
    // going silent, and invite the human to say what they need in words. This
    // family-visible fallback is journaled too, so a dispatcher refire cannot
    // post it twice.
    let Some(cfg) = wg_config else {
        run_photo_shopping_occurrence(
            workgraph_dir,
            &physical_turn_key,
            &route.bot_id,
            &route.chat_id,
            &family_roster,
            &sink,
            || async {
                Ok(
                    "Got your photo! I can't read pictures right now — tell me what you need and I'll update the list."
                        .to_string(),
                )
            },
        )
        .await?;
        return Ok(());
    };

    let composer = convo::OneshotComposer::from_config(cfg.clone());
    let persona_summary = plan
        .session_ref()
        .and_then(|s| convo::read_session_summary(workgraph_dir, s));
    let gateway = photo::HttpShoppingGateway::from_env();

    run_photo_shopping_occurrence(
        workgraph_dir,
        &physical_turn_key,
        &route.bot_id,
        &route.chat_id,
        &family_roster,
        &sink,
        || async {
            // The photo `file_id` is only valid for the bot that received it,
            // so a NEW occurrence downloads via THAT bot's channel. Keep this
            // lookup and scratch creation inside the mutation closure: an
            // `applied` delivery retry must not need image inputs or create
            // fresh temporary state.
            let receiving = channels
                .iter()
                .find(|channel| channel.channel_type() == msg.channel)
                .with_context(|| format!("no channel matches receiving bot {:?}", msg.channel))?;
            let scratch = std::env::temp_dir().join(format!(
                "casa-photo-{}-{}",
                msg.message_id.as_deref().unwrap_or("na"),
                msg.sender_id.as_deref().unwrap_or("na"),
            ));
            std::fs::create_dir_all(&scratch).ok();
            let outcome = photo::run_photo_shopping_turn(
                &turn,
                persona_summary.as_deref(),
                &photo::PhotoLimits::default(),
                receiving,
                &composer,
                &gateway,
                &family_roster,
                &scratch,
            )
            .await;
            let _ = std::fs::remove_dir_all(&scratch);
            Ok(outcome?.reply_text)
        },
    )
    .await?;
    Ok(())
}

const PHOTO_SHOPPING_OCCURRENCE_DOMAIN: &str = "photo-shopping";

/// Exact restart payload for one accepted photo-shopping turn.
///
/// Only family-visible canonical bytes and their original opaque configured
/// route are retained. Image handles, captions, chat ids, helper ids, and
/// household labels remain absent from journal filenames because the shared
/// occurrence journal hashes the caller-supplied physical key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PhotoShoppingOutcome {
    reply_text: String,
    bot_id: String,
    chat_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhotoShoppingDispatch {
    outcome: PhotoShoppingOutcome,
    resumed_delivery: bool,
    already_delivered: bool,
}

fn photo_shopping_delivery_id(physical_turn_key: &str) -> String {
    format!(
        "photo-shopping-{}",
        durable_telegram_digest_v1("photo-shopping-delivery", &[physical_turn_key]),
    )
}

/// Apply and deliver one image-derived shopping occurrence with restart-safe
/// ordering.
///
/// The mutation closure is invoked only for a newly reserved physical turn.
/// Its exact guarded reply and original route are persisted before transport.
/// An `applied` retry therefore sends only those stored bytes; a `delivered`
/// retry is a no-op. A leftover `reserved` record fails closed because the
/// process cannot know whether a list mutation reached the gateway.
async fn run_photo_shopping_occurrence<F, Fut>(
    workgraph_dir: &Path,
    physical_turn_key: &str,
    bot_id: &str,
    chat_id: &str,
    family_roster: &worksgood::notify::grounding::FamilyVoiceRoster,
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    apply: F,
) -> Result<PhotoShoppingDispatch>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_occurrence::{OccurrenceJournal, OccurrenceState};

    let (journal, state) = OccurrenceJournal::<PhotoShoppingOutcome>::claim(
        workgraph_dir,
        PHOTO_SHOPPING_OCCURRENCE_DOMAIN,
        physical_turn_key,
    )?;

    let (outcome, resumed_delivery) = match state {
        OccurrenceState::New => {
            let draft = apply().await?;
            let reply_text =
                worksgood::notify::grounding::enforce_family_voice(&draft, family_roster);
            if reply_text != draft {
                eprintln!(
                    "[{}] family-voice guard: cleaned a photo reply before journaling",
                    chrono::Utc::now().format("%H:%M:%S"),
                );
            }
            let outcome = PhotoShoppingOutcome {
                reply_text,
                bot_id: bot_id.to_string(),
                chat_id: chat_id.to_string(),
            };
            journal.mark_applied(&outcome)?;
            (outcome, false)
        }
        OccurrenceState::Incomplete => {
            anyhow::bail!(
                "photo-shopping occurrence is incomplete; refusing to reapply uncertain list changes"
            );
        }
        OccurrenceState::Applied(outcome) => (outcome, true),
        OccurrenceState::Delivered(outcome) => {
            return Ok(PhotoShoppingDispatch {
                outcome,
                resumed_delivery: false,
                already_delivered: true,
            });
        }
        OccurrenceState::PassedThrough => {
            anyhow::bail!(
                "photo-shopping occurrence was recorded as pass-through; refusing to apply it"
            );
        }
    };

    let delivery_id = photo_shopping_delivery_id(physical_turn_key);
    convo::send_reply_once(
        workgraph_dir,
        &delivery_id,
        &outcome.bot_id,
        &outcome.chat_id,
        &outcome.reply_text,
        sink,
    )
    .await
    .context("photo-shopping reply delivery failed; the stored outcome remains retryable")?;
    journal.mark_delivered(&outcome)?;

    Ok(PhotoShoppingDispatch {
        outcome,
        resumed_delivery,
        already_delivered: false,
    })
}

/// `wg telegram photo-plan` — diagnose the photo → shopping-list vision
/// pipeline WITHOUT a network (task `photo-to-shopping`).
///
/// Decodes the raw update(s) through the SAME `decode_update` boundary the
/// listener uses (photo `file_id`, caption, media group), coalesces album
/// frames into per-turn units, runs the real `elect_responders_with_owner_map` decision on the
/// first turn's caption (who a captioned photo routes to), and — when a fixture
/// `--reply` + `--list` are given — parses the model's `SHOPPING_UPDATE:` tail
/// and prints the exact mutations that WOULD be applied through the gateway
/// endpoints. Nothing is downloaded and nothing is sent.
pub fn run_photo_plan(
    workgraph_dir: &Path,
    update: &str,
    reply: Option<&str>,
    list: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram as tg;
    use worksgood::notify::telegram_photo as photo;

    // --- 1. Decode update(s) → messages. Accept a single object or an array. ---
    let read_arg = |arg: &str| -> Result<String> {
        if let Some(path) = arg.strip_prefix('@') {
            std::fs::read_to_string(path).with_context(|| format!("failed to read fixture {path}"))
        } else {
            Ok(arg.to_string())
        }
    };
    let raw = read_arg(update)?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("update is not valid JSON")?;
    let elements: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    let messages: Vec<worksgood::notify::IncomingMessage> = elements
        .iter()
        .filter_map(|u| tg::decode_update(u, "telegram"))
        .collect();

    // --- 2. Coalesce albums → per-turn units. ---
    let turns = photo::coalesce_album(&messages);

    // --- 3. Election on the first turn's caption (who answers a captioned photo). ---
    let config = load_telegram_config()?;
    let first = turns.first();
    let (elected, is_photo): (Option<String>, bool) = match first {
        Some(turn) => {
            let mention_usernames = parse_at_mention_tokens(&turn.caption);
            let human_count = human_agent_id_set(workgraph_dir).len();
            let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
            let election = elect_responders_with_owner_map(
                turn.chat_type.as_deref(),
                turn.chat_id.as_deref(),
                &turn.caption,
                &mention_usernames,
                None,
                false,
                human_count,
                &config,
                &owner_map,
            );
            let who = match &election {
                Election::One { bot, .. } => {
                    bot.agent_id.clone().or_else(|| Some(bot.bot_id.clone()))
                }
                Election::All { .. } => Some("roster".to_string()),
                Election::Private => Some("(1:1 passthrough)".to_string()),
                Election::Silence(_) => None,
            };
            (who, true)
        }
        None => (None, false),
    };

    // --- 4. Optional: parse the reply tail + plan the mutations. ---
    let (verdict, actions): (Option<photo::VisionVerdict>, Vec<photo::ShoppingAction>) =
        if let Some(reply) = reply {
            let items = match list {
                Some(l) => {
                    let raw = read_arg(l)?;
                    let json: serde_json::Value =
                        serde_json::from_str(&raw).context("list is not valid JSON")?;
                    photo::parse_shopping_json(&json)
                }
                None => Vec::new(),
            };
            let v = photo::parse_vision_verdict(reply);
            let a = photo::plan_shopping_actions(&v, &items);
            (Some(v), a)
        } else {
            (None, Vec::new())
        };

    // --- 5. Report. ---
    if json {
        let actions_json: Vec<serde_json::Value> = actions
            .iter()
            .map(|a| match a {
                photo::ShoppingAction::CrossOff { key, text } => {
                    serde_json::json!({ "op": "cross_off", "key": key, "text": text })
                }
                photo::ShoppingAction::Restore { key, text } => {
                    serde_json::json!({ "op": "restore", "key": key, "text": text })
                }
                photo::ShoppingAction::Add { text } => {
                    serde_json::json!({ "op": "add", "text": text })
                }
            })
            .collect();
        let out = serde_json::json!({
            "is_photo": is_photo,
            "turns": turns.len(),
            "file_ids": turns.iter().flat_map(|t| t.file_ids.clone()).collect::<Vec<_>>(),
            "caption": first.map(|t| t.caption.clone()),
            "media_group_id": first.and_then(|t| t.media_group_id.clone()),
            "elected": elected,
            "have": verdict.as_ref().map(|v| v.have.clone()),
            "need": verdict.as_ref().map(|v| v.need.clone()),
            "actions": actions_json,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if !is_photo {
        println!("not a photo — no photo turn");
        return Ok(());
    }
    let turn = first.unwrap();
    println!(
        "photo — {} turn(s), {} image(s){}",
        turns.len(),
        turn.file_ids.len(),
        turn.media_group_id
            .as_deref()
            .map(|g| format!(", album group {g}"))
            .unwrap_or_default(),
    );
    println!(
        "  caption: {}",
        if turn.caption.is_empty() {
            "(none)"
        } else {
            &turn.caption
        }
    );
    println!(
        "  routes to: {}",
        elected.as_deref().unwrap_or("(silence — no one answers)")
    );
    if let Some(v) = &verdict {
        println!("  reply: {}", v.reply_text);
        println!("  have: {:?}", v.have);
        println!("  need: {:?}", v.need);
        if actions.is_empty() {
            println!("  actions: (none — list already matches)");
        }
        for a in &actions {
            match a {
                photo::ShoppingAction::CrossOff { text, .. } => {
                    println!("  action: cross off '{text}' (POST /shopping/toggle checked=true)")
                }
                photo::ShoppingAction::Restore { text, .. } => {
                    println!("  action: restore '{text}' (POST /shopping/toggle checked=false)")
                }
                photo::ShoppingAction::Add { text } => {
                    println!("  action: add '{text}' (POST /shopping/add)")
                }
            }
        }
    }
    Ok(())
}

struct PhotoReplayDownloader {
    image_path: PathBuf,
}

#[async_trait]
impl worksgood::notify::telegram_photo::PhotoDownloader for PhotoReplayDownloader {
    async fn download(&self, _file_id: &str, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating photo replay download dir {}", parent.display())
            })?;
        }
        std::fs::copy(&self.image_path, dest).with_context(|| {
            format!(
                "copying photo replay fixture {} to {}",
                self.image_path.display(),
                dest.display(),
            )
        })?;
        Ok(())
    }
}

struct PhotoReplayComposer {
    reply: String,
}

#[async_trait]
impl worksgood::notify::telegram_photo::VisionComposer for PhotoReplayComposer {
    async fn compose_vision(&self, prompt: &str, image_paths: &[PathBuf]) -> Result<String> {
        if prompt.trim().is_empty() || image_paths.is_empty() {
            anyhow::bail!("photo replay composer did not receive its image prompt");
        }
        Ok(self.reply.clone())
    }
}

struct PhotoReplayGateway {
    items: Vec<worksgood::notify::telegram_photo::ShoppingItem>,
    log_path: PathBuf,
}

#[async_trait]
impl worksgood::notify::telegram_photo::ShoppingGateway for PhotoReplayGateway {
    async fn list(&self) -> Result<Vec<worksgood::notify::telegram_photo::ShoppingItem>> {
        Ok(self.items.clone())
    }

    async fn toggle(&self, key: &str, checked: bool) -> Result<()> {
        append_photo_replay_row(
            &self.log_path,
            &serde_json::json!({
                "op": "toggle",
                "key": key,
                "checked": checked,
            }),
        )
    }

    async fn add(&self, text: &str) -> Result<()> {
        append_photo_replay_row(
            &self.log_path,
            &serde_json::json!({
                "op": "add",
                "text": text,
            }),
        )
    }
}

struct PhotoReplaySink {
    log_path: PathBuf,
    fail_send: bool,
}

#[async_trait]
impl worksgood::notify::telegram_conversation::ReplySink for PhotoReplaySink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        if self.fail_send {
            anyhow::bail!("stubbed photo reply transport failure");
        }
        append_photo_replay_row(
            &self.log_path,
            &serde_json::json!({
                "bot_id": bot_id,
                "chat_id": chat_id,
                "text": text,
            }),
        )?;
        Ok(Some("stubbed-photo-message".to_string()))
    }
}

fn append_photo_replay_row(path: &Path, row: &serde_json::Value) -> Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating photo replay fixture dir {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening photo replay fixture log {}", path.display()))?;
    serde_json::to_writer(&mut file, row)
        .with_context(|| format!("writing photo replay fixture log {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("finishing photo replay fixture log {}", path.display()))
}

/// Scratch-only binary seam for restart testing the exact occurrence helper
/// used by [`handle_photo_shopping_turn`].
///
/// The raw update, election, complete photo-turn orchestrator, canonical reply
/// guard, occurrence journal, and delivery ledger are production code. Only
/// the three I/O seams are replaced: a fixture image is copied instead of
/// downloaded, the supplied vision reply is returned instead of spawning a
/// model, and gateway mutations / successful Telegram sends append JSONL rows.
/// No credential or network is used.
#[allow(clippy::too_many_arguments)]
pub fn run_photo_replay(
    workgraph_dir: &Path,
    update: &str,
    reply: &str,
    list: &str,
    mock_image: &Path,
    mock_mutation_log: &Path,
    mock_send_log: &Path,
    fail_send: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram as tg;
    use worksgood::notify::telegram_photo as photo;

    let read_arg = |arg: &str| -> Result<String> {
        if let Some(path) = arg.strip_prefix('@') {
            std::fs::read_to_string(path).with_context(|| format!("failed to read fixture {path}"))
        } else {
            Ok(arg.to_string())
        }
    };

    let update_value: serde_json::Value =
        serde_json::from_str(&read_arg(update)?).context("update is not valid JSON")?;
    let elements = match update_value {
        serde_json::Value::Array(values) => values,
        value => vec![value],
    };
    let messages: Vec<worksgood::notify::IncomingMessage> = elements
        .iter()
        .filter_map(|value| tg::decode_update(value, "telegram"))
        .collect();
    let photo_message = messages
        .iter()
        .find(|message| message.photo_file_id.is_some())
        .context("update did not contain a photo message")?;
    let turn = photo::coalesce_album(&messages)
        .into_iter()
        .next()
        .context("update did not produce a photo turn")?;

    let config = load_telegram_config()?;
    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let election = elect_responders_with_owner_map(
        turn.chat_type.as_deref(),
        turn.chat_id.as_deref(),
        &turn.caption,
        &parse_at_mention_tokens(&turn.caption),
        None,
        false,
        human_agent_id_set(workgraph_dir).len(),
        &config,
        &owner_map,
    );
    let (bot_id, chat_id) = match election {
        Election::One {
            bot, reply_chat, ..
        } => (bot.bot_id, reply_chat),
        Election::All { .. } => {
            anyhow::bail!("photo replay fixture expected one configured voice, got the full roster")
        }
        Election::Private => anyhow::bail!(
            "photo replay fixture expected one configured voice, got a private passthrough"
        ),
        Election::Silence(reason) => anyhow::bail!(
            "photo replay fixture expected one configured voice, got {}",
            reason,
        ),
    };

    let list_value: serde_json::Value =
        serde_json::from_str(&read_arg(list)?).context("list is not valid JSON")?;
    let items = photo::parse_shopping_json(&list_value);

    let physical_turn_key = telegram_photo_turn_key(photo_message);
    let family_roster = worksgood::notify::grounding::load_family_voice_roster(
        &project_root(workgraph_dir),
        workgraph_dir,
    );
    let orchestration_roster = family_roster.clone();
    let downloader = PhotoReplayDownloader {
        image_path: mock_image.to_path_buf(),
    };
    let composer = PhotoReplayComposer {
        reply: reply.to_string(),
    };
    let gateway = PhotoReplayGateway {
        items,
        log_path: mock_mutation_log.to_path_buf(),
    };
    let scratch = workgraph_dir
        .join("photo-replay-scratch")
        .join(&physical_turn_key);
    let sink = PhotoReplaySink {
        log_path: mock_send_log.to_path_buf(),
        fail_send,
    };
    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let dispatch = rt.block_on(run_photo_shopping_occurrence(
        workgraph_dir,
        &physical_turn_key,
        &bot_id,
        &chat_id,
        &family_roster,
        &sink,
        move || async move {
            std::fs::create_dir_all(&scratch).with_context(|| {
                format!("creating photo replay scratch dir {}", scratch.display())
            })?;
            let result = photo::run_photo_shopping_turn(
                &turn,
                None,
                &photo::PhotoLimits::default(),
                &downloader,
                &composer,
                &gateway,
                &orchestration_roster,
                &scratch,
            )
            .await;
            let _ = std::fs::remove_dir_all(&scratch);
            Ok(result?.reply_text)
        },
    ))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "bot_id": dispatch.outcome.bot_id,
                "chat_id": dispatch.outcome.chat_id,
                "reply": dispatch.outcome.reply_text,
                "resumed_delivery": dispatch.resumed_delivery,
                "already_delivered": dispatch.already_delivered,
            }))?
        );
    } else {
        let phase = if dispatch.already_delivered {
            "already delivered"
        } else if dispatch.resumed_delivery {
            "stored delivery resumed"
        } else {
            "applied"
        };
        println!("photo replay fixture: {phase}");
    }
    Ok(())
}

/// Stable physical-turn key for one photo-shopping occurrence.
///
/// Every frame in a Telegram album carries the same opaque
/// `media_group_id`, while its caption and timestamp may differ by frame. Key
/// albums by that group id so a listener restart on a later frame cannot run a
/// second vision mutation. A lone photo uses the cross-bot-stable Telegram
/// fields (chat, stable sender, sent-at second, and caption); bot-scoped
/// `file_id` and transport-local `message_id` are deliberately excluded.
fn telegram_photo_turn_key(message: &worksgood::notify::IncomingMessage) -> String {
    let sender = message
        .sender_id
        .as_deref()
        .unwrap_or(message.sender.as_str());
    let chat_id = message.chat_id.as_deref().unwrap_or("");
    let digest = match message
        .media_group_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(group_id) => durable_telegram_digest_v1(
            "telegram-photo-physical-turn",
            &[chat_id, sender, "album", group_id],
        ),
        None => {
            let sent_at = message.sent_at.unwrap_or(i64::MIN).to_string();
            durable_telegram_digest_v1(
                "telegram-photo-physical-turn",
                &[chat_id, sender, "single", &sent_at, &message.body],
            )
        }
    };
    format!("telegram-photo-turn-{digest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal inbound Telegram message. A copy of the identical fixture in
    // `commands::telegram`'s test module, deliberately: that file is upstream's and
    // the point of this extraction is to stop editing it. Duplicating ~18 lines of
    // test scaffolding is cheaper than exporting a private test helper across
    // modules, and cheaper still than a merge conflict.
    fn gate_msg(chat_type: &str, has_bot_command: bool) -> worksgood::notify::IncomingMessage {
        worksgood::notify::IncomingMessage {
            channel: "telegram".to_string(),
            sender: "luca".to_string(),
            sender_id: Some("8905220378".to_string()),
            sender_is_bot: false,
            sent_at: None,
            body: "?".to_string(),
            action_id: None,
            reply_to: None,
            message_id: Some("1".to_string()),
            chat_id: Some("-100".to_string()),
            chat_type: Some(chat_type.to_string()),
            mention_usernames: Vec::new(),
            reply_to_bot: None,
            has_bot_command,
            photo_file_id: None,
            media_group_id: None,
            voice_file_id: None,
            voice_mime: None,
        }
    }

    /// Permanent behavior gate for the photo mutation boundary. A fresh
    /// invocation after transport failure may deliver the stored canonical
    /// reply, but it must never rerun image-derived list changes.
    #[tokio::test]
    async fn photo_shopping_same_physical_turn_mutates_and_sends_once() {
        #[derive(Default)]
        struct ReplaySink {
            fail_next: std::sync::atomic::AtomicBool,
            attempts: std::sync::Mutex<Vec<(String, String, String)>>,
            delivered: std::sync::Mutex<Vec<(String, String, String)>>,
        }

        #[async_trait]
        impl worksgood::notify::telegram_conversation::ReplySink for ReplaySink {
            async fn send(
                &self,
                bot_id: &str,
                chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                let row = (bot_id.to_string(), chat_id.to_string(), text.to_string());
                self.attempts.lock().unwrap().push(row.clone());
                if self
                    .fail_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    anyhow::bail!("fixture transport failure");
                }
                let mut delivered = self.delivered.lock().unwrap();
                delivered.push(row);
                Ok(Some(format!("fixture-{}", delivered.len())))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let workgraph_dir = dir.path().join(".wg");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let roster = worksgood::notify::grounding::FamilyVoiceRoster::from_names(
            ["Copper Comet"],
            ["Household Member"],
        );
        let sink = ReplaySink::default();
        sink.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mutations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut first = gate_msg("supergroup", false);
        first.channel = "telegram:wire-axis-7".to_string();
        first.sender = "member-display".to_string();
        first.sender_id = Some("member-anchor-41".to_string());
        first.sent_at = Some(1_721_900_000);
        first.body = "Copper Comet, what do we still need?".to_string();
        first.message_id = Some("transport-local-58".to_string());
        first.chat_id = Some("-100700".to_string());
        first.photo_file_id = Some("bot-scoped-photo-a".to_string());
        let first_key = telegram_photo_turn_key(&first);

        // Bot-scoped transport fields may drift on a true refire; the physical
        // key must stay stable.
        let mut cross_bot_refire = first.clone();
        cross_bot_refire.channel = "telegram:wire-axis-9".to_string();
        cross_bot_refire.message_id = Some("transport-local-907".to_string());
        cross_bot_refire.photo_file_id = Some("other-bot-photo-handle".to_string());
        assert_eq!(
            first_key,
            telegram_photo_turn_key(&cross_bot_refire),
            "bot-scoped photo handles must not split one physical turn",
        );

        // Every frame of one album is one mutation occurrence even when only a
        // later frame is seen after restart.
        let mut album_first = first.clone();
        album_first.media_group_id = Some("album-occurrence-31".to_string());
        let mut album_later_frame = album_first.clone();
        album_later_frame.body.clear();
        album_later_frame.sent_at = Some(1_721_900_001);
        album_later_frame.photo_file_id = Some("album-frame-two".to_string());
        assert_eq!(
            telegram_photo_turn_key(&album_first),
            telegram_photo_turn_key(&album_later_frame),
        );

        let first_mutations = mutations.clone();
        let first_attempt = run_photo_shopping_occurrence(
            &workgraph_dir,
            &first_key,
            "voice-anchor-731",
            "-100700",
            &roster,
            &sink,
            move || async move {
                first_mutations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok("I found chickpeas and added oat milk to the list.".to_string())
            },
        )
        .await;
        assert!(first_attempt.is_err());
        assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst), 1,);

        // A fresh invocation carries drifted route and draft inputs. The
        // persisted original route and exact reply win, and apply is untouched.
        let retry_mutations = mutations.clone();
        let retry = run_photo_shopping_occurrence(
            &workgraph_dir,
            &first_key,
            "decoy-voice-992",
            "-100999",
            &roster,
            &sink,
            move || async move {
                retry_mutations.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                Ok("This newly composed reply must never be used.".to_string())
            },
        )
        .await
        .unwrap();
        assert!(retry.resumed_delivery);
        assert!(!retry.already_delivered);
        assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst), 1,);
        let delivered = sink.delivered.lock().unwrap().clone();
        assert_eq!(
            delivered,
            vec![(
                "voice-anchor-731".to_string(),
                "-100700".to_string(),
                "I found chickpeas and added oat milk to the list.".to_string(),
            )],
        );

        let completed_mutations = mutations.clone();
        let completed = run_photo_shopping_occurrence(
            &workgraph_dir,
            &first_key,
            "voice-anchor-731",
            "-100700",
            &roster,
            &sink,
            move || async move {
                completed_mutations.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
                Ok("This completed replay must stay silent.".to_string())
            },
        )
        .await
        .unwrap();
        assert!(completed.already_delivered);
        assert_eq!(sink.attempts.lock().unwrap().len(), 2);
        assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst), 1,);

        // Identical words one second later are a distinct photo occurrence.
        let mut later = first.clone();
        later.sent_at = Some(1_721_900_001);
        later.message_id = Some("transport-local-59".to_string());
        later.photo_file_id = Some("bot-scoped-photo-b".to_string());
        let later_key = telegram_photo_turn_key(&later);
        assert_ne!(first_key, later_key);
        let later_mutations = mutations.clone();
        run_photo_shopping_occurrence(
            &workgraph_dir,
            &later_key,
            "voice-anchor-731",
            "-100700",
            &roster,
            &sink,
            move || async move {
                later_mutations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok("I found chickpeas and added oat milk to the list.".to_string())
            },
        )
        .await
        .unwrap();
        assert_eq!(mutations.load(std::sync::atomic::Ordering::SeqCst), 2,);
        assert_eq!(sink.delivered.lock().unwrap().len(), 2);
    }
}
