//! Telegram commands for WG CLI
//!
//! Provides commands for interacting with Telegram:
//! - `wg telegram listen` - Start the Telegram bot listener
//! - `wg telegram send` - Send a message to the configured chat
//! - `wg telegram status` - Show Telegram configuration status

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use worksgood::notify::NotificationChannel;
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::family_plan;
use worksgood::notify::telegram::{TelegramBotConfig, TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_family_commands as family_commands;
use worksgood::notify::telegram_dedupe::{DedupeKey, DedupeSet};
use worksgood::notify::telegram_group::{
    CONCIERGE_BOT, Election, NaturalRoute, SilenceReason, elect_responders, route_natural,
};

/// Run the Telegram listener.
///
/// Starts a long-running process that polls for incoming messages via the
/// Telegram Bot API and dispatches WG commands.
pub fn run_listen(dir: &Path, chat_id: Option<&str>) -> Result<()> {
    // Load the raw notify config once: `TelegramConfig` drives the banner and
    // the group @mention router, while `all_from_notify_config` builds one
    // channel per configured bot for the poll loop below.
    let notify_config = NotifyConfig::load(Some(Path::new(".")))
        .context("Failed to load notification config")?
        .context("No notify.toml found. Create one at ~/.config/workgraph/notify.toml")?;
    let config = TelegramConfig::from_notify_config(&notify_config)?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Starting Telegram listener...");
    println!("{}", bot_banner(&config));
    println!("Chat ID: {}", effective_chat_id);

    // Build one channel per configured bot. Live evidence for this whole task:
    // Luca tags a bot in the group and the @mention lands ONLY in that bot's
    // getUpdates queue — so a listener that polls a single bot never sees
    // mentions of the others. We long-poll EVERY bot concurrently (one tokio
    // task per bot, each persisting its own offset) and funnel them all into
    // one shared receiver, which the single routing pipeline below drains.
    let channels = TelegramChannel::all_from_notify_config(&notify_config)
        .context("Failed to build Telegram channels")?;
    if channels.is_empty() {
        anyhow::bail!("No Telegram bots configured — nothing to poll");
    }

    // Replies go out via one bot: the concierge (otto) when present, else the
    // first configured bot. This matches the pre-existing single-channel reply
    // behaviour — the poll fan-out below is the only change in scope here.
    let reply_idx = channels
        .iter()
        .position(|c| c.bot_id() == CONCIERGE_BOT)
        .unwrap_or(0);

    println!("Press Ctrl+C to stop\n");

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    // Config for group @mention resolution in the routing pipeline.
    let route_config = config.clone();

    rt.block_on(async {
        // One shared receiver fed by every bot's poll task. Each task tags its
        // messages with the bot's channel_type, so the router still knows which
        // bot received a given update.
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        for ch in &channels {
            println!("polling {}", ch.bot_id());
            ch.spawn_poll(tx.clone(), Some(bot_offset_state_path(ch.bot_id())?));
        }
        // Drop our own sender handle so the receiver closes if every poll task
        // exits (all senders dropped) rather than hanging forever.
        drop(tx);

        let channel = &channels[reply_idx];

        // Cross-bot de-duplication for all-bots-privacy-off mode. With privacy
        // off, Telegram delivers every plain group message to ALL four bots'
        // queues; the fan-out above polls all four, so the SAME physical message
        // arrives four times. Keyed by (chat_id, message_id) — stable across
        // every bot that received it — the first copy wins and the other three
        // are dropped silently. See `notify::telegram_dedupe`.
        let dedupe = DedupeSet::new();

        let workgraph_dir = dir.to_path_buf();
        while let Some(msg) = rx.recv().await {
            // De-duplicate first: a text message with a (chat_id, message_id)
            // that we have already processed on another bot's queue is a
            // duplicate delivery — drop it before it can trigger a second
            // election. Button presses (action_id) carry no message text to
            // route and only ever reach the one bot whose message held the
            // button, so they are exempt.
            if msg.action_id.is_none() {
                if let (Some(cid), Some(mid)) = (msg.chat_id.as_deref(), msg.message_id.as_deref())
                {
                    if !dedupe.first_delivery(DedupeKey::new(cid, mid)) {
                        println!(
                            "[{}] Duplicate group message (chat {}, msg {}) dropped — already handled",
                            chrono::Utc::now().format("%H:%M:%S"),
                            cid,
                            mid,
                        );
                        continue;
                    }
                }
            }

            // Reply target: the chat the message came from (in a group, the
            // group itself — never the bot's default DM). Falls back to the
            // configured chat when the transport didn't surface a chat id.
            let reply_target = msg
                .chat_id
                .clone()
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| effective_chat_id.clone());

            // Button presses are handled first — they carry an action id, not
            // text to route by @mention.
            if let Some(ref action_id) = msg.action_id {
                println!(
                    "[{}] Button press from {}: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    action_id
                );

                // Action IDs follow the pattern "action:task_id" (e.g. "approve:my-task")
                let response = handle_action(&workgraph_dir, action_id, &msg.sender);

                if let Err(e) = channel.send_text(&reply_target, &response).await {
                    eprintln!("Failed to send response: {e}");
                }
                continue;
            }

            // Family command set (/dinner /shopping /week /reminders /standup
            // /help). Commands ride ABOVE the election table: the surviving
            // (deduped) copy is exactly-once, and a bare slash command must
            // never be silenced as small-talk — so we resolve it here, before
            // election. In a group the command's OWNER answers (Bruno for
            // /dinner) regardless of which bot's queue delivered the surviving
            // copy; in a 1:1 the bot you messaged answers. `/standup` fans out
            // to the whole roster. See `notify::telegram_family_commands`.
            if let Some(cmd) = worksgood::notify::telegram_family_commands::match_command(&msg.body)
            {
                let is_group =
                    matches!(msg.chat_type.as_deref(), Some("group") | Some("supergroup"));
                println!(
                    "[{}] Command {} from {} -> {} ({})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    cmd.keyword,
                    msg.sender,
                    reply_target,
                    if is_group { "group" } else { "direct" },
                );
                if let Err(e) = run_family_command(
                    &workgraph_dir,
                    &route_config,
                    cmd,
                    &reply_target,
                    is_group,
                    &msg.channel,
                )
                .await
                {
                    eprintln!("Failed to run command {}: {e}", cmd.keyword);
                }
                continue;
            }

            // All-bots-privacy-off responder election (layered on R17's
            // privacy-aware core + natural routing). In a private chat this is a
            // passthrough. In a group/supergroup the deduped message is resolved
            // to responder(s) by Luca's ordered table: @mention, addressed name,
            // reply-chain, collective-address (ALL four answer), team-directed
            // unaddressed ask (otto coordinates), else silence. Replies always go
            // back to the group. This assumes every bot runs with Telegram
            // privacy mode OFF so plain chatter reaches the listener — see
            // docs/09 §natural-group.
            let election = elect_responders(
                msg.chat_type.as_deref(),
                msg.chat_id.as_deref(),
                &msg.body,
                &msg.mention_usernames,
                msg.reply_to_bot.as_deref(),
                &route_config,
            );
            let (route_channel, route_body) = match election {
                Election::Silence(reason) => {
                    // Small-talk / no-chat-id / no-voice — bots stay quiet.
                    if !matches!(reason, SilenceReason::SmallTalk) {
                        println!(
                            "[{}] Group message from {} not answered ({})",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            reason,
                        );
                    }
                    continue;
                }
                Election::All {
                    ref reply_chat, ..
                } => {
                    // Collective address — the whole roster answers, briefly and
                    // in-voice, in roster order. The single listener orchestrates
                    // the sequential sends so no bot double-posts.
                    println!(
                        "[{}] Collective address from {} — roster reply to {}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                        reply_chat,
                    );
                    if let Err(e) =
                        run_group_collective(&workgraph_dir, &route_config, reply_chat).await
                    {
                        eprintln!("Failed to run collective reply: {e}");
                    }
                    continue;
                }
                Election::One {
                    ref bot,
                    ref body,
                    ref reply_chat,
                    addressed_by,
                } => {
                    println!(
                        "[{}] Group message from {} elected by {} -> {} (agent {})",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                        addressed_by,
                        bot.bot_id,
                        bot.agent_id.as_deref().unwrap_or("(unbound)"),
                    );
                    debug_assert_eq!(reply_chat, &reply_target);
                    (bot.channel_type.clone(), body.clone())
                }
                Election::Private => (msg.channel.clone(), msg.body.clone()),
            };

            // Family command with a leading @mention (e.g. "@otto /shopping").
            // The bare and `@bot`-suffixed forms were already handled before
            // election; this catches the case where election stripped a leading
            // mention off the front. `/standup` is one such command (whole-team
            // check-in — one post per voice in roster order); it and the rest of
            // the family set now flow through the SAME table. See
            // `notify::telegram_family_commands`. Only groups reach here (a
            // private command was resolved pre-election), so the owner answers.
            if let Some(cmd) =
                worksgood::notify::telegram_family_commands::match_command(&route_body)
            {
                println!(
                    "[{}] Command {} from {} (post-mention) -> {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    cmd.keyword,
                    msg.sender,
                    reply_target,
                );
                if let Err(e) = run_family_command(
                    &workgraph_dir,
                    &route_config,
                    cmd,
                    &reply_target,
                    true,
                    &msg.channel,
                )
                .await
                {
                    eprintln!("Failed to run command {}: {e}", cmd.keyword);
                }
                continue;
            }

            // Try to parse as a command
            if let Some(cmd) = worksgood::telegram_commands::parse(&route_body) {
                println!(
                    "[{}] Command from {}: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    cmd.description()
                );

                let response =
                    worksgood::telegram_commands::execute(&workgraph_dir, &cmd, &msg.sender);

                // Send response back
                if let Err(e) = channel.send_text(&reply_target, &response).await {
                    eprintln!("Failed to send response: {e}");
                }
            } else {
                // Not a command and not a button press. The merged R21×R10
                // inbound path (bug fix): classify the message by checking for a
                // pending onboarding confirmation FIRST, then falling through to
                // awaiting-human task routing. Doing routing first (as the naive
                // merge did) let the router swallow a "YES" handshake reply so
                // the binding never confirmed. See `classify_inbound_message`.
                // In a group route, `route_channel` is the addressed bot's
                // channel type so the reply lands on the right agent's task.
                match classify_inbound_message(
                    &workgraph_dir,
                    &route_channel,
                    &msg.sender,
                    &route_body,
                ) {
                    InboundOutcome::Confirmed { name, routed_task } => {
                        // A bound-but-unconfirmed human replied YES — the
                        // inbound half of the `wg agency human add` handshake.
                        println!(
                            "[{}] {} ({}) confirmed — joined via YES handshake",
                            chrono::Utc::now().format("%H:%M:%S"),
                            name,
                            msg.sender
                        );
                        let welcome = format!("Welcome aboard, {}! You're all set. \u{2705}", name);
                        if let Err(e) = channel.send_text(&reply_target, &welcome).await {
                            eprintln!("Failed to send welcome: {e}");
                        }
                        // A single message can be both a confirmation AND a
                        // reply to a task the human was handed; ack the task too.
                        if let Some(task_id) = routed_task {
                            println!(
                                "[{}] Reply from {} also recorded on awaiting-human task '{}'",
                                chrono::Utc::now().format("%H:%M:%S"),
                                msg.sender,
                                task_id
                            );
                            if let Err(e) = channel
                                .send_text(
                                    &reply_target,
                                    &format!("✓ Recorded your reply on task '{}'.", task_id),
                                )
                                .await
                            {
                                eprintln!("Failed to send ack: {e}");
                            }
                        }
                    }
                    InboundOutcome::Routed { task_id } => {
                        // A human's reply to a task they were handed. Recording
                        // it as a message satisfies the task's HumanInput wait so
                        // the coordinator completes it. (The "awaiting-human task
                        // router" formerly deferred at src/notify/telegram.rs:42.)
                        println!(
                            "[{}] Reply from {} recorded on awaiting-human task '{}'",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            task_id
                        );
                        if let Err(e) = channel
                            .send_text(
                                &reply_target,
                                &format!("✓ Recorded your reply on task '{}'.", task_id),
                            )
                            .await
                        {
                            eprintln!("Failed to send ack: {e}");
                        }
                    }
                    InboundOutcome::Unmatched => {
                        println!(
                            "[{}] Message from {} (no awaiting-human task matched): {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            msg.body
                        );
                    }
                }
            }
        }

        Ok(())
    })
}

/// Try to confirm a human-onboarding binding from an inbound message.
///
/// If `sender` has an unconfirmed Telegram binding (see
/// `wg agency human add`) and `body` is an affirmative `YES`, mark the binding
/// confirmed, persist it, and return the human's name. Otherwise return
/// `None`. Persistence failures are logged and swallowed so the listener keeps
/// running.
fn try_confirm_binding(workgraph_dir: &Path, sender: &str, body: &str) -> Option<String> {
    use worksgood::agency::{TelegramBindingMap, apply_confirmation};

    let agency_dir = workgraph_dir.join("agency");
    let mut bindings = match TelegramBindingMap::load(&agency_dir) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to load Telegram binding map: {e}");
            return None;
        }
    };
    let name = apply_confirmation(&mut bindings, sender, body, chrono::Utc::now())?;
    if let Err(e) = bindings.save(&agency_dir) {
        eprintln!("Failed to persist Telegram binding confirmation: {e}");
        return None;
    }
    Some(name)
}

/// Classification of an inbound (non-command, non-button) Telegram message.
#[derive(Debug, PartialEq)]
enum InboundOutcome {
    /// A bound-but-unconfirmed human replied YES: their binding is now
    /// confirmed. `routed_task` is `Some` when the same message ALSO matched an
    /// awaiting-human task (confirm + answer in one message); `None` when it was
    /// a pure confirmation (confirmed silently, no task to answer).
    Confirmed {
        name: String,
        routed_task: Option<String>,
    },
    /// Not a confirmation, but recorded on an awaiting-human task.
    Routed { task_id: String },
    /// Neither confirmed a pending binding nor matched an awaiting task.
    Unmatched,
}

/// Classify an inbound message from the Telegram listener.
///
/// The merged R21 (onboarding handshake) × R10 (human-dispatch tail) path had a
/// bug: routing ran first and the awaiting-human-task router swallowed a plain
/// "YES" handshake reply, so the onboarding binding never confirmed. The fix is
/// ordering — check for a pending unconfirmed binding for `sender` FIRST and
/// apply the confirmation, THEN fall through to awaiting-human task routing. A
/// single message can be both (a confirmation that also answers a parked task);
/// a confirmation with no matching task confirms silently.
///
/// This is the pure, filesystem-only core of the listener's inbound branch (no
/// network), so it is unit-testable without a live bot.
fn classify_inbound_message(
    workgraph_dir: &Path,
    channel_type: &str,
    sender: &str,
    body: &str,
) -> InboundOutcome {
    // 1. Confirmation check first — this is the ordering fix.
    let confirmed_name = try_confirm_binding(workgraph_dir, sender, body);
    // 2. Then awaiting-human task routing (records the reply as a message).
    let routed = crate::commands::service::human_dispatch::route_inbound_reply(
        workgraph_dir,
        channel_type,
        sender,
        body,
    );
    match (confirmed_name, routed) {
        (Some(name), routed_task) => InboundOutcome::Confirmed { name, routed_task },
        (None, Some(task_id)) => InboundOutcome::Routed { task_id },
        (None, None) => InboundOutcome::Unmatched,
    }
}

/// Send a message to the configured Telegram chat.
pub fn run_send(chat_id: Option<&str>, message: &str, dry_run: bool) -> Result<()> {
    let config = load_telegram_config()?;
    let (bot_id, bot, effective_chat_id) = resolve_send_bot(&config, chat_id)?;

    if dry_run {
        // Resolution-only path: prove which bot + chat + URL a real send would
        // use, with the token redacted. The URL host segment MUST be
        // `bot<digits>:...` — an empty token (the old bots-map-only 404 bug)
        // would render `bot/sendMessage`.
        println!("[dry-run] would send via bot '{}'", bot_id);
        println!("[dry-run] target chat: {}", effective_chat_id);
        println!("[dry-run] api url: {}", redacted_send_url(&bot.bot_token));
        println!("[dry-run] message: {}", message);
        return Ok(());
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::from_bot(bot_id, bot);
        channel
            .send_text(&effective_chat_id, message)
            .await
            .context("Failed to send message")?;
        println!("Message sent to chat {}", effective_chat_id);
        Ok(())
    })
}

/// The `sendMessage` API URL a send would hit, with the token body redacted but
/// the numeric bot id kept (so a real token renders `bot123456:REDACTED` and an
/// unresolved/empty token renders the tell-tale `bot:REDACTED` → the 404 shape).
fn redacted_send_url(bot_token: &str) -> String {
    let id = bot_token.split(':').next().unwrap_or("");
    format!("https://api.telegram.org/bot{}:REDACTED/sendMessage", id)
}

/// Resolve which bot + chat `wg telegram send` should use.
///
/// Prefers the legacy top-level `[telegram]` bot, falling back to the first
/// `[telegram.bots.*]` entry. Previously `run_send` always built the channel
/// from the top-level `bot_token`, which is EMPTY in a bots-map-only config —
/// producing the URL `https://api.telegram.org/bot/sendMessage` and a bare 404
/// (task `listener-reconnect`). `all_bots()` lists the legacy bot first when
/// present, so `.next()` picks the correct default either way. The effective
/// chat id defaults to the resolved bot's own chat when the caller passes none.
fn resolve_send_bot(
    config: &TelegramConfig,
    chat_id: Option<&str>,
) -> Result<(String, TelegramBotConfig, String)> {
    let mut bots = config.all_bots();
    // The legacy top-level bot (always id "default") is listed first by
    // `all_bots` and wins when present. Otherwise pick the lexicographically-
    // first named bot: the `bots` map is a HashMap, so a bare `.next()` would
    // target a RANDOM bot each run — this keeps `wg telegram send` stable.
    let (bot_id, bot) = if bots.first().map(|(id, _)| id == "default").unwrap_or(false) {
        bots.remove(0)
    } else {
        bots.into_iter().min_by(|a, b| a.0.cmp(&b.0)).context(
            "No Telegram bots configured — set [telegram] bot_token/chat_id or a [telegram.bots.*] entry",
        )?
    };
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| bot.chat_id.clone());
    Ok((bot_id, bot, effective_chat_id))
}

/// `wg telegram route` — show how a group message would be routed to a family
/// voice, without sending anything.
///
/// Runs the exact [`route_natural`] decision the listener uses, so it verifies
/// natural-group routing (docs/09 §natural-group) end-to-end against the real
/// `notify.toml` bots. Mentions are approximated from any `@handle` tokens in
/// the text (the live listener reads them from Telegram entities). Prints the
/// resolved voice and *how* it was addressed (@mention / name / reply-chain /
/// concierge), and flags a `/standup` that the listener would intercept for the
/// whole roster.
pub fn run_route(
    message: &str,
    reply_to_bot: Option<&str>,
    chat_type: &str,
    chat_id: &str,
    json: bool,
) -> Result<()> {
    let config = load_telegram_config()?;

    // Approximate the listener's mention extraction: any @handle token.
    let mention_usernames: Vec<String> = message
        .split_whitespace()
        .filter(|t| t.starts_with('@'))
        .map(|t| t.trim_start_matches('@').to_ascii_lowercase())
        .collect();

    let route = route_natural(
        Some(chat_type),
        Some(chat_id),
        message,
        &mention_usernames,
        reply_to_bot,
        &config,
    );

    // The listener intercepts `/standup` (for the whole roster) on the routed
    // body before the per-agent handler, so report that specially.
    let (kind, agent, addressed_by, routed_body) = match &route {
        NaturalRoute::Private => ("private", None, None, message.to_string()),
        NaturalRoute::Drop => ("drop", None, None, message.to_string()),
        NaturalRoute::ToBot {
            bot,
            body,
            addressed_by,
            ..
        } => {
            let is_standup = worksgood::notify::telegram_standup::is_standup_command(body);
            let kind = if is_standup { "standup" } else { "agent" };
            (
                kind,
                bot.agent_id.clone().or_else(|| Some(bot.bot_id.clone())),
                Some(addressed_by.to_string()),
                body.clone(),
            )
        }
    };

    if json {
        let out = serde_json::json!({
            "kind": kind,
            "agent": agent,
            "addressed_by": addressed_by,
            "routed_body": routed_body,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match kind {
        "private" => println!("private chat — 1:1 passthrough (not group-routed)"),
        "drop" => println!("dropped — no chat id, or no voice to route to"),
        "standup" => {
            println!("/standup — intercepted; posts the whole roster (nora, bruno, mira, otto)")
        }
        _ => println!(
            "routed to {} (by {}): {}",
            agent.as_deref().unwrap_or("(unbound)"),
            addressed_by.as_deref().unwrap_or("?"),
            routed_body,
        ),
    }
    Ok(())
}

/// `wg telegram elect` — show who would respond to a group message in
/// all-bots-privacy-off mode, without sending anything.
///
/// Runs the exact [`elect_responders`] decision the listener uses on a deduped
/// message and prints the outcome: `mention` / `name` / `reply-chain` route to
/// one voice, `collective` fans out to the whole roster, `otto` coordinates a
/// team-directed ask, and `silence` means the bots stay out. Mentions are
/// approximated from any `@handle` tokens (the live listener reads Telegram
/// entities). See docs/09 §natural-group.
pub fn run_elect(
    message: &str,
    reply_to_bot: Option<&str>,
    chat_type: &str,
    chat_id: &str,
    json: bool,
) -> Result<()> {
    let config = load_telegram_config()?;

    let mention_usernames: Vec<String> = message
        .split_whitespace()
        .filter(|t| t.starts_with('@'))
        .map(|t| t.trim_start_matches('@').to_ascii_lowercase())
        .collect();

    let election = elect_responders(
        Some(chat_type),
        Some(chat_id),
        message,
        &mention_usernames,
        reply_to_bot,
        &config,
    );

    // (kind, who, addressed_by, body) — `who` is the elected agent for the
    // single-voice arms, the roster for `collective`, none for silence/private.
    let (kind, who, addressed_by, body): (&str, Option<String>, Option<String>, String) =
        match &election {
            Election::Private => ("private", None, None, message.to_string()),
            Election::Silence(reason) => ("silence", None, Some(reason.to_string()), message.to_string()),
            Election::All { body, .. } => {
                let roster = worksgood::notify::telegram_standup::plan_roster(
                    &config,
                    worksgood::notify::telegram_standup::DEFAULT_ROSTER,
                )
                .into_iter()
                .map(|m| m.bot_id)
                .collect::<Vec<_>>()
                .join(", ");
                ("collective", Some(roster), None, body.clone())
            }
            Election::One {
                bot,
                body,
                addressed_by,
                ..
            } => {
                let is_standup = worksgood::notify::telegram_standup::is_standup_command(body);
                let kind = if is_standup { "standup" } else { "agent" };
                (
                    kind,
                    bot.agent_id.clone().or_else(|| Some(bot.bot_id.clone())),
                    Some(addressed_by.to_string()),
                    body.clone(),
                )
            }
        };

    if json {
        let out = serde_json::json!({
            "kind": kind,
            "who": who,
            "addressed_by": addressed_by,
            "body": body,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match kind {
        "private" => println!("private chat — 1:1 passthrough (not group-routed)"),
        "silence" => println!(
            "silence ({}) — no one responds",
            addressed_by.as_deref().unwrap_or("?")
        ),
        "collective" => println!(
            "collective address — the whole roster answers in order: {}",
            who.as_deref().unwrap_or("(none configured)")
        ),
        "standup" => {
            println!("/standup — intercepted; posts the whole roster (nora, bruno, mira, otto)")
        }
        _ => println!(
            "answered by {} (by {}): {}",
            who.as_deref().unwrap_or("(unbound)"),
            addressed_by.as_deref().unwrap_or("?"),
            body,
        ),
    }
    Ok(())
}

/// Orchestrate a `/standup` in a group: post one family-voice message per named
/// voice, in roster order, each AS that bot.
///
/// This is the sole orchestrator (the single listener process), so the four
/// posts are strictly sequential and no bot double-posts. Each post is grounded
/// in that persona's live graph state (its open / in-progress tasks). `target`
/// is the group chat id every post is sent to. Tokens come from `config` and
/// are used only to construct each bot's channel — never logged.
pub async fn run_group_standup(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
) -> Result<()> {
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::plan_roster(config, standup::DEFAULT_ROSTER);
    if roster.is_empty() {
        eprintln!("No named bots configured — /standup has no voices to post.");
        return Ok(());
    }

    // Load the graph once; ground every persona's report against it. A missing
    // or unreadable graph is not fatal — the standup still runs with each voice
    // reporting an empty plate (honest "all caught up").
    let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();

    for member in &roster {
        let (in_progress, open) = match &graph {
            Some(g) => standup::agent_task_lines(g, member.agent_id()),
            None => (Vec::new(), Vec::new()),
        };
        let post = standup::render_report(member, &in_progress, &open);

        let channel = TelegramChannel::from_bot(member.bot_id.clone(), member.bot.clone());
        match channel.send_text(target, &post.text).await {
            Ok(_) => println!(
                "[{}] standup: {} posted",
                chrono::Utc::now().format("%H:%M:%S"),
                post.bot_id,
            ),
            Err(e) => eprintln!("standup: {} failed to post: {e}", post.bot_id),
        }
    }
    Ok(())
}

/// Orchestrate a **collective-address reply** (election rule d): post one brief
/// in-voice hello per named voice, in roster order, each AS that bot and each
/// grounded in that persona's live graph state.
///
/// This is the conversational sibling of [`run_group_standup`] — same sole
/// orchestrator (the single listener), same strict roster order, same
/// no-double-post guarantee — but the bodies answer a greeting rather than file
/// a status report (see `telegram_standup::render_conversational`). `target` is
/// the group chat id every reply is sent to.
pub async fn run_group_collective(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
) -> Result<()> {
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::plan_roster(config, standup::DEFAULT_ROSTER);
    if roster.is_empty() {
        eprintln!("No named bots configured — collective reply has no voices to post.");
        return Ok(());
    }

    // Ground every voice against the live graph (missing graph → empty plate,
    // an honest "all quiet" line rather than a crash).
    let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();

    for member in &roster {
        let (in_progress, open) = match &graph {
            Some(g) => standup::agent_task_lines(g, member.agent_id()),
            None => (Vec::new(), Vec::new()),
        };
        let post = standup::render_conversational(member, &in_progress, &open);

        let channel = TelegramChannel::from_bot(member.bot_id.clone(), member.bot.clone());
        match channel.send_text(target, &post.text).await {
            Ok(_) => println!(
                "[{}] collective: {} replied",
                chrono::Utc::now().format("%H:%M:%S"),
                post.bot_id,
            ),
            Err(e) => eprintln!("collective: {} failed to reply: {e}", post.bot_id),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Family command set (/dinner /shopping /week /reminders /standup /help)
// ---------------------------------------------------------------------------

/// The project root that holds `plans/`. The graph dir is `<root>/.wg`, so when
/// `workgraph_dir` is a `.wg`/`.workgraph` subdir we step up to its parent;
/// otherwise we treat it as the root itself.
fn project_root(workgraph_dir: &Path) -> PathBuf {
    match workgraph_dir.file_name().and_then(|n| n.to_str()) {
        Some(".wg") | Some(".workgraph") => workgraph_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| workgraph_dir.to_path_buf()),
        _ => workgraph_dir.to_path_buf(),
    }
}

/// The set of agent ids that are human operators — the grounding for
/// `/reminders` and `/week` pending confirmations. Mirrors the private helper
/// in `service::human_dispatch` (kept here so the family-command path stays
/// self-contained).
fn human_agent_id_set(workgraph_dir: &Path) -> HashSet<String> {
    use worksgood::agency;
    let agents_dir = workgraph_dir.join("agency").join("cache/agents");
    agency::load_all_agents_or_warn(&agents_dir)
        .into_iter()
        .filter(|a| a.is_human())
        .map(|a| a.id)
        .collect()
}

/// Orchestrate a single family command (`/dinner`, `/shopping`, `/week`,
/// `/reminders`, `/help`) or the whole-roster `/standup`.
///
/// * **Group** — a single-reply command is composed once and sent AS the
///   command's owner bot (Bruno for `/dinner`), regardless of which bot's queue
///   delivered the surviving (deduped) copy. `/standup` fans out to the whole
///   roster via [`run_group_standup`].
/// * **1:1** — the bot the user messaged (identified by `receiving_channel`,
///   its `channel_type`) answers directly, with the same composed content.
///
/// Grounding — the live graph, the roster config, and the parsed `plans/` — is
/// loaded once and handed to the pure composer. Tokens live only on the send
/// channel and are never logged.
pub async fn run_family_command(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    cmd: &family_commands::FamilyCommand,
    target: &str,
    is_group: bool,
    receiving_channel: &str,
) -> Result<()> {
    // `/standup` posts one message per voice — reuse the standup orchestrator so
    // the group gets the four-voice check-in in roster order, no double-posts.
    if cmd.kind == family_commands::CommandKind::Roster {
        return run_group_standup(workgraph_dir, config, target).await;
    }

    let text = compose_family_reply(workgraph_dir, config, cmd);

    // Which bot sends: in a group, the command's owner; in a 1:1, the bot the
    // user actually messaged (mapped from its channel_type). Fall back to the
    // owner, then to any configured bot, so a reply always goes out.
    let bots = config.all_bots();
    let want_id = if is_group {
        cmd.owner.to_string()
    } else {
        receiving_channel
            .strip_prefix("telegram:")
            .unwrap_or(receiving_channel)
            .to_string()
    };
    let chosen = bots
        .iter()
        .find(|(id, _)| id == &want_id)
        .or_else(|| bots.iter().find(|(id, _)| id == cmd.owner))
        .or_else(|| bots.first());
    let (bot_id, bot) = match chosen {
        Some((id, bot)) => (id.clone(), bot.clone()),
        None => {
            eprintln!("No Telegram bots configured — cannot answer {}.", cmd.keyword);
            return Ok(());
        }
    };

    let channel = TelegramChannel::from_bot(bot_id.clone(), bot);
    channel
        .send_text(target, &text)
        .await
        .with_context(|| format!("{} failed to send {}", bot_id, cmd.keyword))?;
    println!(
        "[{}] {} answered by {}",
        chrono::Utc::now().format("%H:%M:%S"),
        cmd.keyword,
        bot_id,
    );
    Ok(())
}

/// Load grounding (plans + graph + human agents) and compose a single family
/// command's reply. Shared by the live listener and the `wg telegram command`
/// dry-run so both render identical text. `today`/`now` come from the local
/// clock (overridable in the dry-run via [`compose_family_reply_on`]).
fn compose_family_reply(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    cmd: &family_commands::FamilyCommand,
) -> String {
    compose_family_reply_on(
        workgraph_dir,
        config,
        cmd,
        chrono::Local::now().date_naive(),
        chrono::Utc::now(),
    )
}

/// [`compose_family_reply`] with an explicit `today`/`now` (for deterministic
/// tests and the `--today` dry-run flag).
fn compose_family_reply_on(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    cmd: &family_commands::FamilyCommand,
    today: chrono::NaiveDate,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let plans = family_plan::load_plans(&project_root(workgraph_dir));
    let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();
    let humans = human_agent_id_set(workgraph_dir);
    let ctx = family_commands::CommandContext {
        graph: graph.as_ref(),
        config,
        plans: &plans,
        today,
        now,
        human_agents: &humans,
    };
    family_commands::compose(cmd, &ctx)
}

/// `wg telegram register-commands` — register the shared family command set with
/// Telegram (via `setMyCommands`) for EVERY configured bot, so the commands
/// autocomplete when a user types `/` in the group or a 1:1. Each bot registers
/// the full set (any bot can receive a `/command`; the listener's election
/// decides who answers). After each `setMyCommands` we read the menu back with
/// `getMyCommands` and report the count — verification, no tokens logged.
pub fn run_register_commands(json: bool) -> Result<()> {
    let notify = NotifyConfig::load(Some(Path::new(".")))
        .context("Failed to load notification config")?
        .context("No notify.toml found. Create one at ~/.config/workgraph/notify.toml")?;
    let channels = TelegramChannel::all_from_notify_config(&notify)
        .context("Failed to build Telegram channels")?;
    if channels.is_empty() {
        anyhow::bail!("No Telegram bots configured — nothing to register");
    }

    let cmds: Vec<(String, String)> = family_commands::FAMILY_COMMANDS
        .iter()
        .map(|c| (c.name().to_string(), c.description.to_string()))
        .collect();

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    rt.block_on(async {
        let mut summaries = Vec::new();
        for ch in &channels {
            ch.set_my_commands(&cmds)
                .await
                .with_context(|| format!("setMyCommands failed for bot {}", ch.bot_id()))?;
            let got = ch
                .get_my_commands()
                .await
                .with_context(|| format!("getMyCommands failed for bot {}", ch.bot_id()))?;
            let registered = got
                .get("result")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if !json {
                println!(
                    "✓ {} — {} command(s) registered and verified",
                    ch.bot_id(),
                    registered
                );
            }
            summaries.push(serde_json::json!({
                "bot_id": ch.bot_id(),
                "registered": registered,
                "commands": got.get("result").cloned().unwrap_or(serde_json::Value::Null),
            }));
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "bots": summaries,
                    "command_set": cmds.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                }))?
            );
        } else {
            println!(
                "\nRegistered {} command(s) across {} bot(s): {}",
                cmds.len(),
                channels.len(),
                cmds.iter()
                    .map(|(n, _)| format!("/{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

/// `wg telegram command <name>` — compose a family command's reply against live
/// data and print it, WITHOUT sending anything. This is the scripted-test and
/// dry-run entry point: it proves each command returns grounded content (from
/// the real `plans/` + graph) in the owner's voice. `--today` pins the date so
/// the "current week" / "tonight's dinner" selection is deterministic.
pub fn run_command(
    workgraph_dir: &Path,
    name: &str,
    today: Option<&str>,
    json: bool,
) -> Result<()> {
    let cmd = family_commands::match_command(name)
        .or_else(|| family_commands::match_command(&format!("/{name}")))
        .with_context(|| {
            format!(
                "unknown command '{name}' — known: {}",
                family_commands::FAMILY_COMMANDS
                    .iter()
                    .map(|c| c.keyword)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })?;

    // Config is only needed for /standup's roster; tolerate its absence so the
    // plan-grounded commands compose even without a [telegram] section.
    let config = load_telegram_config().unwrap_or_default();

    let today = match today {
        Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .with_context(|| format!("invalid --today '{s}', expected YYYY-MM-DD"))?,
        None => chrono::Local::now().date_naive(),
    };
    let now = today
        .and_hms_opt(9, 0, 0)
        .map(|dt| dt.and_utc())
        .unwrap_or_else(chrono::Utc::now);

    let text = compose_family_reply_on(workgraph_dir, &config, cmd, today, now);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": cmd.keyword,
                "owner": cmd.owner,
                "kind": format!("{:?}", cmd.kind),
                "data_source": cmd.data_source,
                "text": text,
            }))?
        );
    } else {
        println!("{text}");
    }
    Ok(())
}

/// `wg telegram standup` — run a standup on demand (for the live demo and the
/// scripted end-to-end test). With `--dry-run` the posts are printed to stdout
/// in roster order instead of being sent, so the flow is verifiable without a
/// live group or real tokens.
pub fn run_standup(workgraph_dir: &Path, chat_id: Option<&str>, dry_run: bool) -> Result<()> {
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;
    let roster = standup::plan_roster(&config, standup::DEFAULT_ROSTER);
    if roster.is_empty() {
        anyhow::bail!("No named bots configured under [telegram.bots.*] — nothing to post.");
    }

    // Target: explicit --chat-id, else the first named bot's configured chat id
    // (in Casa Pinello every bot shares the group chat id).
    let target = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| roster[0].bot.chat_id.clone());

    if dry_run {
        let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();
        let posts: Vec<standup::StandupPost> = roster
            .iter()
            .map(|member| {
                let (in_progress, open) = match &graph {
                    Some(g) => standup::agent_task_lines(g, member.agent_id()),
                    None => (Vec::new(), Vec::new()),
                };
                standup::render_report(member, &in_progress, &open)
            })
            .collect();
        println!("STANDUP DRY-RUN — {} posts (roster order):", posts.len());
        for (i, post) in posts.iter().enumerate() {
            println!(
                "--- [{}] {} ({}) ---",
                i + 1,
                post.bot_id,
                post.channel_type
            );
            println!("{}", post.text);
        }
        return Ok(());
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    rt.block_on(async { run_group_standup(workgraph_dir, &config, &target).await })
}

/// List all configured Telegram bots — the legacy single-bot from `[telegram]`
/// (if any) plus every entry under `[telegram.bots.<id>]`. Used to verify a
/// multi-bot setup before `wg telegram listen` spawns one long-poll task per
/// bot.
pub fn run_list_bots(json: bool) -> Result<()> {
    // Match the project-local-then-global lookup the other telegram subcommands
    // use (see `load_telegram_config`): try `.workgraph/notify.toml` from CWD
    // first, then fall back to `~/.config/workgraph/notify.toml`.
    let notify = match NotifyConfig::load(Some(Path::new(".")))? {
        Some(c) => c,
        None => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"bots": []}))?
                );
            } else {
                println!("Telegram: not configured (no notify.toml found)");
            }
            return Ok(());
        }
    };

    let channels = TelegramChannel::all_from_notify_config(&notify)?;

    if json {
        let bots: Vec<serde_json::Value> = channels
            .iter()
            .map(|ch| {
                serde_json::json!({
                    "bot_id": ch.bot_id(),
                    "channel_type": ch.channel_type(),
                    "agent_id": ch.agent_id(),
                    "chat_id": ch.chat_id(),
                    "bot_token_preview": ch.bot_token_preview(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"bots": bots}))?
        );
    } else if channels.is_empty() {
        println!("Telegram: no bots configured");
        println!(
            "\nAdd a [telegram] block to ~/.config/workgraph/notify.toml or .workgraph/notify.toml."
        );
        println!(
            "Single-bot (legacy):\n  [telegram]\n  bot_token = \"...\"\n  chat_id = \"...\"\n"
        );
        println!(
            "Multi-bot (one per persistent agent):\n  [telegram.bots.nora]\n  bot_token = \"...\"\n  chat_id = \"...\"\n  agent_id = \"nora\"\n"
        );
    } else {
        println!("{} bot(s) configured:\n", channels.len());
        for ch in &channels {
            let agent = ch.agent_id().unwrap_or("(shared / no agent binding)");
            println!("  bot id:        {}", ch.bot_id());
            println!("  channel type:  {}", ch.channel_type());
            println!("  agent id:      {}", agent);
            println!("  chat id:       {}", ch.chat_id());
            println!("  token preview: {}", ch.bot_token_preview());
            println!();
        }
    }
    Ok(())
}

/// Show Telegram configuration status.
pub fn run_status(json: bool) -> Result<()> {
    match load_telegram_config() {
        Ok(config) => {
            if json {
                let status = serde_json::json!({
                    "configured": true,
                    "chat_id": config.chat_id,
                    "bot_token_prefix": &config.bot_token[..config.bot_token.len().min(6)],
                });
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Telegram: configured");
                println!(
                    "  Bot token: {}...",
                    &config.bot_token[..config.bot_token.len().min(6)]
                );
                println!("  Chat ID: {}", config.chat_id);
            }
        }
        Err(_) => {
            if json {
                let status = serde_json::json!({ "configured": false });
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Telegram: not configured");
                println!("\nAdd a [telegram] section to your notify.toml:");
                println!("  ~/.config/workgraph/notify.toml");
                println!("  or .wg/notify.toml");
                println!();
                println!("  [telegram]");
                println!("  bot_token = \"123456:ABC-DEF...\"");
                println!("  chat_id = \"12345678\"");
            }
        }
    }
    Ok(())
}

/// Handle an action button callback.
///
/// R18: the generic scheme is `<task_id>#<button_key>` — the button routes back
/// to its originating task and records the chosen option's label as the human's
/// reply (completing the task via the human-dispatch tail). We try that FIRST so
/// any task can declare its own buttons without the listener knowing the verbs.
/// The legacy `<verb>:<task>` action ids (approve/claim/done/fail) remain
/// supported as a fallback for older DMs still in a chat.
fn handle_action(workgraph_dir: &Path, action_id: &str, sender: &str) -> String {
    use crate::commands::service::human_dispatch::route_button_callback;

    // Generic `<task_id>#<key>` routing (the R18 replacement).
    if action_id.contains('#') {
        return match route_button_callback(workgraph_dir, action_id, sender) {
            Some((task_id, label)) => {
                format!("✓ Recorded your choice \u{201c}{label}\u{201d} on task '{task_id}'.")
            }
            None => format!("Unknown or expired button: {action_id}"),
        };
    }

    // Legacy `<verb>:<task>` action ids.
    let parts: Vec<&str> = action_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return format!("Unknown action: {action_id}");
    }

    let (action, task_id) = (parts[0], parts[1]);
    match action {
        "approve" | "claim" => {
            worksgood::matrix_commands::execute_claim(workgraph_dir, task_id, Some(sender))
        }
        "reject" | "fail" => worksgood::matrix_commands::execute_fail(
            workgraph_dir,
            task_id,
            Some("rejected via Telegram"),
        ),
        "done" => worksgood::matrix_commands::execute_done(workgraph_dir, task_id),
        _ => format!("Unknown action: {action}"),
    }
}

/// Poll for replies from the configured Telegram chat.
///
/// Calls the Telegram Bot API getUpdates endpoint and waits for replies
/// from the configured chat_id within the timeout period.
pub fn run_poll(chat_id: Option<&str>, timeout_seconds: u64) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Polling for messages from chat {}...", effective_chat_id);
    println!("Timeout: {} seconds", timeout_seconds);

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);

        // Load last seen update_id
        let offset = load_last_update_id().unwrap_or(0);

        let start_time = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_seconds);

        loop {
            if start_time.elapsed() >= timeout_duration {
                println!("Timeout reached - no new messages");
                return Ok(());
            }

            match poll_once(&channel, offset, &effective_chat_id, 10).await {
                Ok(Some((message, new_offset))) => {
                    // Save the new offset
                    if let Err(e) = save_last_update_id(new_offset) {
                        eprintln!("Warning: failed to save update_id: {}", e);
                    }

                    println!("Message from {}: {}", message.sender, message.body);
                    return Ok(());
                }
                Ok(None) => {
                    // No new messages, wait a bit before trying again
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    eprintln!("Poll error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Send a message and wait for reply.
///
/// Sends the message and polls for reply at intervals. Times out after
/// configurable max wait. Includes task ID context if provided.
pub fn run_ask(
    message: &str,
    chat_id: Option<&str>,
    timeout_seconds: u64,
    interval_seconds: u64,
    task_id: Option<&str>,
) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    // Format message with task context if provided
    let formatted_message = if let Some(task) = task_id {
        format!("[{}] Agent question: {}", task, message)
    } else {
        format!("Agent question: {}", message)
    };

    println!("Sending message and waiting for reply...");
    println!("Message: {}", formatted_message);
    println!(
        "Timeout: {} seconds, polling every {} seconds",
        timeout_seconds, interval_seconds
    );

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);

        // Send the message first
        match channel
            .send_text(&effective_chat_id, &formatted_message)
            .await
        {
            Ok(msg_id) => {
                println!("Message sent (ID: {})", msg_id.0);
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to send message: {}", e));
            }
        }

        // Load last seen update_id
        let offset = load_last_update_id().unwrap_or(0);

        let start_time = std::time::Instant::now();
        let timeout_duration = std::time::Duration::from_secs(timeout_seconds);
        let interval_duration = std::time::Duration::from_secs(interval_seconds);

        loop {
            if start_time.elapsed() >= timeout_duration {
                println!("Timeout reached - no reply received");
                return Ok(());
            }

            match poll_once(&channel, offset, &effective_chat_id, 10).await {
                Ok(Some((message, new_offset))) => {
                    // Save the new offset
                    if let Err(e) = save_last_update_id(new_offset) {
                        eprintln!("Warning: failed to save update_id: {}", e);
                    }

                    println!("Reply from {}: {}", message.sender, message.body);
                    return Ok(());
                }
                Ok(None) => {
                    // No new messages, wait for the next polling interval
                    tokio::time::sleep(interval_duration).await;
                }
                Err(e) => {
                    eprintln!("Poll error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Poll Telegram once for new messages from a specific chat.
/// Returns the first message and the new offset, or None if no messages.
async fn poll_once(
    channel: &TelegramChannel,
    offset: i64,
    target_chat_id: &str,
    timeout: u32,
) -> Result<Option<(worksgood::notify::IncomingMessage, i64)>> {
    let body = serde_json::json!({
        "offset": offset,
        "timeout": timeout,
        "allowed_updates": ["message", "callback_query"],
    });

    let resp = channel.api_call("getUpdates", &body).await?;

    let updates = resp
        .get("result")
        .and_then(|r| r.as_array())
        .context("Invalid response format")?;

    let mut new_offset = offset;

    for update in updates {
        if let Some(uid) = update.get("update_id").and_then(|u| u.as_i64()) {
            new_offset = uid + 1;
        }

        // Handle callback queries (button presses)
        if let Some(cb) = update.get("callback_query") {
            let chat_id = cb
                .get("message")
                .and_then(|m| m.get("chat"))
                .and_then(|c| c.get("id"))
                .and_then(|id| id.as_i64())
                .map(|id| id.to_string());

            if chat_id.as_deref() == Some(target_chat_id) {
                let sender = cb
                    .get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .or_else(|| {
                        cb.get("from")
                            .and_then(|f| f.get("id"))
                            .and_then(|i| i.as_i64())
                            .map(|_| "unknown")
                    })
                    .unwrap_or("unknown");

                let action_id = cb
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();

                let reply_to = cb
                    .get("message")
                    .and_then(|m| m.get("message_id"))
                    .and_then(|m| m.as_i64())
                    .map(|mid| worksgood::notify::MessageId(mid.to_string()));

                let msg = worksgood::notify::IncomingMessage {
                    channel: "telegram".to_string(),
                    sender: sender.to_string(),
                    body: action_id.clone(),
                    action_id: Some(action_id),
                    reply_to,
                    message_id: None,
                    chat_id,
                    chat_type: None,
                    mention_usernames: Vec::new(),
                    reply_to_bot: None,
                };

                return Ok(Some((msg, new_offset)));
            }
        }

        // Handle regular messages
        if let Some(message) = update.get("message") {
            let chat_id = message
                .get("chat")
                .and_then(|c| c.get("id"))
                .and_then(|id| id.as_i64())
                .map(|id| id.to_string());

            if chat_id.as_deref() == Some(target_chat_id) {
                let sender = message
                    .get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("unknown");

                let body = message
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();

                let reply_to = message
                    .get("reply_to_message")
                    .and_then(|r| r.get("message_id"))
                    .and_then(|m| m.as_i64())
                    .map(|mid| worksgood::notify::MessageId(mid.to_string()));

                let message_id = message
                    .get("message_id")
                    .and_then(|m| m.as_i64())
                    .map(|m| m.to_string());

                let chat_type = message
                    .get("chat")
                    .and_then(|c| c.get("type"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let mention_usernames = worksgood::notify::telegram_group::parse_mention_usernames(
                    message.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                    message.get("entities").unwrap_or(&serde_json::Value::Null),
                );
                let reply_to_bot =
                    worksgood::notify::telegram_group::reply_to_bot_username(message);

                let msg = worksgood::notify::IncomingMessage {
                    channel: "telegram".to_string(),
                    sender: sender.to_string(),
                    body,
                    action_id: None,
                    reply_to,
                    message_id,
                    chat_id: chat_id.clone(),
                    chat_type,
                    mention_usernames,
                    reply_to_bot,
                };

                return Ok(Some((msg, new_offset)));
            }
        }
    }

    Ok(None)
}

/// Load the last seen update_id from state file.
fn load_last_update_id() -> Result<i64> {
    let state_file = get_state_file_path()?;
    let content = std::fs::read_to_string(state_file)?;
    let id: i64 = content
        .trim()
        .parse()
        .context("Invalid update_id format in state file")?;
    Ok(id)
}

/// Save the last seen update_id to state file.
fn save_last_update_id(update_id: i64) -> Result<()> {
    let state_file = get_state_file_path()?;

    // Ensure parent directory exists
    if let Some(parent) = state_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(state_file, update_id.to_string())?;
    Ok(())
}

/// Get the path to the update_id state file.
fn get_state_file_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home
        .join(".config")
        .join("workgraph")
        .join("telegram_update_id"))
}

/// Per-bot `getUpdates` offset checkpoint file.
///
/// The multi-bot listener polls every bot concurrently, so each needs its own
/// cursor file — a single shared `telegram_update_id` (as the legacy
/// single-bot wait-reply path uses) would let one bot's offset clobber
/// another's and replay/skip updates. Keyed by `bot_id` alongside the legacy
/// file, e.g. `~/.config/workgraph/telegram_update_id_bruno`. The `bot_id`
/// comes from the config key; any path separators are neutralised defensively.
fn bot_offset_state_path(bot_id: &str) -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let safe: String = bot_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Ok(home
        .join(".config")
        .join("workgraph")
        .join(format!("telegram_update_id_{safe}")))
}

/// Render the startup banner line describing which bot(s) are configured.
///
/// Prefers the legacy single `bot_token` when present (masking it to a
/// preview). When the config uses only the multi-bot `bots` map — the
/// legacy `bot_token` is empty — falls back to summarizing all configured
/// bots by id instead of slicing the empty token, which used to panic
/// (out-of-bounds string slice).
fn bot_banner(config: &TelegramConfig) -> String {
    if config.bot_token.is_empty() {
        let bots = config.all_bots();
        if bots.is_empty() {
            "Bots configured: none".to_string()
        } else {
            let names = bots
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} bots configured: {}", bots.len(), names)
        }
    } else {
        let prefix = config.bot_token.get(..6).unwrap_or(&config.bot_token);
        let suffix_start = config.bot_token.len().saturating_sub(4);
        let suffix = config
            .bot_token
            .get(suffix_start..)
            .unwrap_or(&config.bot_token);
        format!("Bot token: {}...{}", prefix, suffix)
    }
}

/// Load Telegram config from notify.toml.
fn load_telegram_config() -> Result<TelegramConfig> {
    let notify_config = NotifyConfig::load(Some(Path::new(".")))
        .context("Failed to load notification config")?
        .context("No notify.toml found. Create one at ~/.config/workgraph/notify.toml")?;
    TelegramConfig::from_notify_config(&notify_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use worksgood::agency::{TelegramBinding, TelegramBindingMap};
    use worksgood::notify::telegram::TelegramBotConfig;

    fn ts() -> chrono::DateTime<chrono::Utc> {
        "2026-07-10T12:00:00Z".parse().unwrap()
    }

    /// Record an unconfirmed binding exactly as `wg agency human add` would,
    /// under `<workgraph_dir>/agency`.
    fn seed_unconfirmed_binding(workgraph_dir: &Path, user: &str, agent: &str, name: &str) {
        let agency_dir = workgraph_dir.join("agency");
        let mut map = TelegramBindingMap::default();
        map.add(TelegramBinding::new(user, agent, name, None, ts()))
            .unwrap();
        map.save(&agency_dir).unwrap();
    }

    /// Bug 1 — the merged R21×R10 inbound path. An unconfirmed binding exists
    /// and the human replies "yes" (in any case/whitespace). The classifier
    /// MUST confirm the binding first (not let awaiting-task routing swallow
    /// it), and — with no awaiting task present — confirm silently.
    #[test]
    fn inbound_yes_confirms_pending_binding_case_insensitive() {
        for body in ["yes", "YES", "  Yes  ", "y", "Y"] {
            let tmp = tempfile::TempDir::new().unwrap();
            let dir = tmp.path();
            seed_unconfirmed_binding(dir, "55501234", "human-luca", "Luca");

            let outcome = classify_inbound_message(dir, "telegram", "55501234", body);
            match outcome {
                InboundOutcome::Confirmed { name, routed_task } => {
                    assert_eq!(name, "Luca", "body {body:?}");
                    assert_eq!(
                        routed_task, None,
                        "no awaiting task ⇒ confirm silently (body {body:?})"
                    );
                }
                other => panic!("expected Confirmed for body {body:?}, got {other:?}"),
            }

            // The confirmation was persisted to disk.
            let reloaded = TelegramBindingMap::load(&dir.join("agency")).unwrap();
            assert!(
                reloaded.find_by_user("55501234").unwrap().confirmed,
                "binding must be persisted as confirmed (body {body:?})"
            );
        }
    }

    /// A non-affirmative message from a bound-but-unconfirmed human with no
    /// awaiting task is Unmatched (and does NOT confirm the binding).
    #[test]
    fn inbound_non_affirmative_does_not_confirm_and_is_unmatched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        seed_unconfirmed_binding(dir, "55501234", "human-luca", "Luca");

        let outcome = classify_inbound_message(dir, "telegram", "55501234", "who is this?");
        assert_eq!(outcome, InboundOutcome::Unmatched);

        let reloaded = TelegramBindingMap::load(&dir.join("agency")).unwrap();
        assert!(!reloaded.find_by_user("55501234").unwrap().confirmed);
    }

    /// An already-confirmed binding replying "yes" again with no awaiting task
    /// is Unmatched — confirmation is idempotent and does not re-fire.
    #[test]
    fn inbound_yes_from_confirmed_binding_is_unmatched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let agency_dir = dir.join("agency");
        let mut map = TelegramBindingMap::default();
        let mut b = TelegramBinding::new("55501234", "human-luca", "Luca", None, ts());
        b.confirmed = true;
        map.bindings.push(b);
        map.save(&agency_dir).unwrap();

        let outcome = classify_inbound_message(dir, "telegram", "55501234", "yes");
        assert_eq!(outcome, InboundOutcome::Unmatched);
    }

    #[test]
    fn bot_banner_bots_map_only_does_not_panic() {
        // Config with ONLY the multi-bot map, no legacy [telegram] bot_token —
        // this used to panic on `&config.bot_token[..6]`.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "1".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        bots.insert(
            "bruno".to_string(),
            TelegramBotConfig {
                bot_token: "222:BBB".to_string(),
                chat_id: "2".to_string(),
                agent_id: Some("bruno".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let banner = bot_banner(&config);
        assert!(banner.contains("2 bots configured"));
        assert!(banner.contains("nora"));
        assert!(banner.contains("bruno"));
    }

    #[test]
    fn bot_banner_legacy_token_masks_preview() {
        let config = TelegramConfig {
            bot_token: "123456:ABCDEF".to_string(),
            chat_id: "1".to_string(),
            bots: HashMap::new(),
        };

        let banner = bot_banner(&config);
        assert_eq!(banner, "Bot token: 123456...CDEF");
    }

    #[test]
    fn bot_banner_empty_config_does_not_panic() {
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: HashMap::new(),
        };

        assert_eq!(bot_banner(&config), "Bots configured: none");
    }

    #[test]
    fn send_with_bots_map_only_resolves_a_real_token_not_404() {
        // Regression: with a bots-map-only config the legacy top-level
        // `bot_token` is empty, so `run_send` used to build the URL
        // `https://api.telegram.org/bot/sendMessage` → 404. `resolve_send_bot`
        // must fall back to the first configured bot's real token + chat.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "1001".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let (bot_id, bot, chat) = resolve_send_bot(&config, None).unwrap();
        assert_eq!(bot_id, "nora");
        assert_eq!(bot.bot_token, "111:AAA", "must not slice an empty token");
        assert!(!bot.bot_token.is_empty(), "empty token would yield a 404 URL");
        assert_eq!(chat, "1001", "defaults to the resolved bot's own chat");
    }

    #[test]
    fn send_with_bots_map_only_is_deterministic_lexicographically_first() {
        // The `bots` map is a HashMap; without a deterministic pick `send`
        // would target a random bot each run. Resolution must be stable on the
        // lexicographically-first id ("bruno" < "nora") regardless of map order.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "7654321:NORA".to_string(),
                chat_id: "1".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        bots.insert(
            "bruno".to_string(),
            TelegramBotConfig {
                bot_token: "1234567:BRUNO".to_string(),
                chat_id: "2".to_string(),
                agent_id: Some("bruno".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        // Resolve several times — the pick must never change.
        for _ in 0..8 {
            let (bot_id, bot, _chat) = resolve_send_bot(&config, None).unwrap();
            assert_eq!(bot_id, "bruno", "must pick the lexicographically-first bot");
            assert_eq!(bot.bot_token, "1234567:BRUNO");
        }
    }

    #[test]
    fn send_prefers_legacy_bot_and_honors_explicit_chat() {
        // When the legacy [telegram] bot IS present it wins (all_bots lists it
        // first), and an explicit --chat-id overrides the bot's default chat.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "222:BBB".to_string(),
                chat_id: "2".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: "999:LEGACY".to_string(),
            chat_id: "500".to_string(),
            bots,
        };

        let (bot_id, bot, chat) = resolve_send_bot(&config, Some("777")).unwrap();
        assert_eq!(bot_id, "default");
        assert_eq!(bot.bot_token, "999:LEGACY");
        assert_eq!(chat, "777", "explicit chat id overrides the default");
    }

    #[test]
    fn send_with_no_bots_errors_clearly() {
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: HashMap::new(),
        };
        let err = resolve_send_bot(&config, None).unwrap_err().to_string();
        assert!(err.contains("No Telegram bots configured"), "got: {err}");
    }

    #[test]
    fn handle_action_approve() {
        // We can't easily test without a graph, but we can verify parsing.
        let result = handle_action(Path::new("/nonexistent"), "approve:my-task", "testuser");
        assert!(result.contains("Error") || result.contains("Claimed"));
    }

    #[test]
    fn handle_action_unknown() {
        let result = handle_action(Path::new("/nonexistent"), "foobar:task", "testuser");
        assert!(result.contains("Unknown action"));
    }

    #[test]
    fn handle_action_malformed() {
        let result = handle_action(Path::new("/nonexistent"), "no-colon", "testuser");
        assert!(result.contains("Unknown action"));
    }

    /// R18: a generic `<task>#<key>` button routes back to the originating task
    /// and records the tapped choice's label as the human's reply — the full
    /// listener callback path, not just the parser.
    #[test]
    fn handle_action_generic_button_records_choice() {
        use worksgood::graph::{Node, Status, Task, TaskChoice, WorkGraph};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "plan-review".to_string(),
            title: "Review the plan".to_string(),
            status: Status::Waiting,
            choices: TaskChoice::confirmation_pair(),
            ..Default::default()
        }));
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        let result = handle_action(dir, "plan-review#change_something", "lucapinello");
        assert!(
            result.contains("Change something") && result.contains("plan-review"),
            "ack names the chosen label and task: {result}"
        );

        let msgs = worksgood::messages::list_messages(dir, "plan-review").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "Change something");
    }

    /// A `#`-form token for an unknown/stale button is reported, not silently
    /// swallowed, and records nothing.
    #[test]
    fn handle_action_generic_button_unknown_is_reported() {
        let result = handle_action(Path::new("/nonexistent"), "ghost#looks_good", "lucapinello");
        assert!(result.contains("Unknown or expired button"), "{result}");
    }
}
