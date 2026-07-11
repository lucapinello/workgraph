//! Telegram commands for WG CLI
//!
//! Provides commands for interacting with Telegram:
//! - `wg telegram listen` - Start the Telegram bot listener
//! - `wg telegram send` - Send a message to the configured chat
//! - `wg telegram status` - Show Telegram configuration status

use anyhow::{Context, Result};
use std::path::Path;

use worksgood::notify::NotificationChannel;
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::telegram::{TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_group::{NaturalRoute, route_natural};

/// Run the Telegram listener.
///
/// Starts a long-running process that polls for incoming messages via the
/// Telegram Bot API and dispatches WG commands.
pub fn run_listen(dir: &Path, chat_id: Option<&str>) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Starting Telegram listener...");
    println!("{}", bot_banner(&config));
    println!("Chat ID: {}", effective_chat_id);
    println!("Press Ctrl+C to stop\n");

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    // Keep a copy of the full multi-bot config for group @mention resolution;
    // `config` itself is moved into the channel below.
    let route_config = config.clone();

    rt.block_on(async {
        let channel = TelegramChannel::new(config);
        let mut rx = channel
            .listen()
            .await
            .context("Failed to start Telegram listener")?;

        let workgraph_dir = dir.to_path_buf();
        while let Some(msg) = rx.recv().await {
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

            // Natural group routing (layered on R17's privacy-aware core). In a
            // private chat this is a passthrough. In a group/supergroup the
            // message is routed to a family voice by, in order: an explicit
            // @mention, the first family name in the text, the bot a reply is
            // threaded onto, and finally the concierge (otto) when no one is
            // named. The reply always goes back to the group, and the target
            // bot's channel type is what the downstream 1:1 router lands on.
            // This assumes the listening (concierge) bot runs with Telegram
            // privacy mode OFF so plain chatter reaches it — see docs/09
            // §natural-group.
            let route = route_natural(
                msg.chat_type.as_deref(),
                msg.chat_id.as_deref(),
                &msg.body,
                &msg.mention_usernames,
                msg.reply_to_bot.as_deref(),
                &route_config,
            );
            let (route_channel, route_body) = match route {
                NaturalRoute::Drop => {
                    println!(
                        "[{}] Group message from {} dropped (no chat id / no voice to route to)",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                    );
                    continue;
                }
                NaturalRoute::ToBot {
                    ref bot,
                    ref body,
                    ref reply_chat,
                    addressed_by,
                } => {
                    println!(
                        "[{}] Group message from {} routed by {} -> {} (agent {})",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                        addressed_by,
                        bot.bot_id,
                        bot.agent_id.as_deref().unwrap_or("(unbound)"),
                    );
                    debug_assert_eq!(reply_chat, &reply_target);
                    (bot.channel_type.clone(), body.clone())
                }
                NaturalRoute::Private => (msg.channel.clone(), msg.body.clone()),
            };

            // `/standup` — the whole-team check-in. It is NOT an ordinary
            // single-response command: it must post ONE message per named voice
            // in roster order, each AS that bot. The single listener is the sole
            // orchestrator, so order is guaranteed and no bot double-posts. We
            // intercept it before the generic command parse (which does not know
            // `/standup`) and before the human-reply classifier (which would
            // otherwise treat the slash text as a reply). See
            // `notify::telegram_standup` for the design rationale.
            if worksgood::notify::telegram_standup::is_standup_command(&route_body) {
                println!(
                    "[{}] /standup from {} — posting roster check-in to {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    reply_target,
                );
                if let Err(e) =
                    run_group_standup(&workgraph_dir, &route_config, &reply_target).await
                {
                    eprintln!("Failed to run standup: {e}");
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
pub fn run_send(chat_id: Option<&str>, message: &str) -> Result<()> {
    let config = load_telegram_config()?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let channel = TelegramChannel::new(config);
        channel
            .send_text(&effective_chat_id, message)
            .await
            .context("Failed to send message")?;
        println!("Message sent to chat {}", effective_chat_id);
        Ok(())
    })
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
