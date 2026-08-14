//! Casa's group-turn handlers: the collective round and the discussion round.
//!
//! Extracted from `commands/telegram.rs` as slice 7a of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). All three items here are OURS — none exists upstream at the
//! fork point or on `gwwg/main` today — even though two of them are called from inside
//! upstream's `run_listen`. That call site is our own added line, so moving the functions out
//! and importing them back is strictly better than the alternative the first analysis reached
//! for: exposing them as `pub(crate)` in their file. Exposure keeps our code there AND adds a
//! marker; this removes 303 lines and adds none.
//!
//! `collective_request_id` came along for the same reason. It is shared with `run_listen`,
//! which stays, but it is ours, so their file imports it rather than hosting it.

use crate::casa::reply_delivery::{FamilyReplyDelivery, GuardPolicy, ReplyScope};
use crate::commands::telegram::project_root;
use anyhow::Result;
use std::path::Path;
use worksgood::notify::ownership;
use worksgood::notify::telegram::TelegramConfig;
use worksgood::notify::telegram_conversation::durable_telegram_digest_v1;
use worksgood::notify::telegram_group::resolve_mentioned_bot;

/// Request id stored for one persona's reply to a Telegram group turn.
///
/// Both collective and elected-single-voice paths use this shape. Voice, chat,
/// and physical turn all participate: the same physical redelivery is stable, a
/// later turn is distinct, and two configured voices never suppress each other
/// even if they share a session implementation.
pub fn collective_request_id(reply_chat: &str, bot_id: &str, physical_turn_key: &str) -> String {
    format!(
        "tg-collective-{}",
        durable_telegram_digest_v1(
            "telegram-collective-request",
            &[reply_chat, bot_id, physical_turn_key],
        ),
    )
}

/// Orchestrate a **collective-address reply** (election rule d): one reply per
/// named voice, in roster order, each AS that bot.
///
/// This is the conversational sibling of [`run_group_standup`] — same sole
/// orchestrator (the single listener), same strict roster order, same
/// no-double-post guarantee. `target` is the group chat id every reply is sent
/// to; `human_message` is the text the family sent; `sender` is the resolved
/// (binding-key) identity of who sent it.
///
/// **Fix #4b — content grounding.** Every voice answers the MESSAGE CONTENT
/// through the *same persistent-session composer the 1:1 path uses*
/// ([`telegram_conversation::run_conversation_turn`]): the human's text is the
/// turn, so a concrete question (`"what's for dinner?"`) gets each voice's real
/// answer, not a canned greeting-shaped status line. Only when a voice has **no
/// bound session** (or the sender isn't a confirmed human) do we fall back to
/// the task-grounded in-voice line ([`telegram_standup::render_conversational`])
/// — never silence, never a status dump.
///
/// `physical_turn_key` identifies the inbound household turn that elected this
/// roster. Every voice gets its own request id derived from that shared key:
/// redelivery of one physical turn remains idempotent, while a later turn in
/// the same chat cannot collide with an earlier voice's outbox entry.
pub async fn run_group_collective(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
    feed_path: &Path,
    human_message: &str,
    sender: &str,
    physical_turn_key: &str,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::load_project_roster(&project_root(workgraph_dir), config)?;
    let family_delivery =
        FamilyReplyDelivery::load_at(workgraph_dir, config, feed_path.to_path_buf());
    if roster.is_empty() {
        eprintln!("No household roster entries have matching Telegram bots.");
        return Ok(());
    }

    // Ground the fallback line against the live graph (missing graph → empty
    // plate, an honest "all quiet" line rather than a crash). The session path
    // grounds itself against each persona's own session context.
    let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();

    // Compose-start line for this composed turn — pairs with the per-voice sent
    // message_id lines below so the log shows the full compose→send arc.
    println!(
        "[{}] compose collective -> {} ({} voice(s) in roster order)",
        chrono::Utc::now().format("%H:%M:%S"),
        target,
        roster.len(),
    );

    let timing = convo::AckTiming::from_env();

    // The reply composer (one-shot `claude` spawn) — same fix as the 1:1 path:
    // each voice COMPOSES a real answer rather than polling an outbox no daemon
    // fills. Best-effort load; on failure the turns use the legacy poll path.
    let wg_config = worksgood::config::Config::load_merged(workgraph_dir).ok();

    for member in &roster {
        // The SAME composer the 1:1 path uses: does this voice have a bound
        // session and is the sender a confirmed human? If so, answer the actual
        // message content grounded in that voice's session.
        let plan = convo::plan_conversation(
            workgraph_dir,
            config,
            &member.channel_type(),
            target,
            sender,
            convo::Entry::GroupElected,
        );

        if matches!(plan, convo::ConversationPlan::Converse { .. }) {
            // Grounded per-voice answer to the human's message. The FeedMirror
            // sink relays the reply into the conversation pane's feed too.
            let sink = family_delivery.wrap(
                convo::BotReplySink::new(config.clone()),
                ReplyScope::Group,
                GuardPolicy::AlreadyGuarded,
            );
            let request_id = collective_request_id(target, &member.bot_id, physical_turn_key);
            let composer = wg_config.clone().map(convo::OneshotComposer::from_config);
            let composer_ref = composer.as_ref().map(|c| c as &dyn convo::ReplyComposer);
            match convo::run_conversation_turn(
                workgraph_dir,
                &plan,
                human_message,
                &request_id,
                timing,
                composer_ref,
                &sink,
            )
            .await
            {
                Ok(outcome) => println!(
                    "[{}] collective: {} answered content [{}]",
                    chrono::Utc::now().format("%H:%M:%S"),
                    member.bot_id,
                    outcome.label(),
                ),
                Err(e) => eprintln!("collective: {} content turn failed: {e}", member.bot_id),
            }
            continue;
        }

        // Fallback: no bound session (or unconfirmed sender) — an honest,
        // task-grounded in-voice line rather than silence.
        let (in_progress, open) = match &graph {
            Some(g) => standup::agent_task_lines(g, member.agent_id()),
            None => (Vec::new(), Vec::new()),
        };
        let post = standup::render_conversational(member, &in_progress, &open);
        let request_id = collective_request_id(target, &member.bot_id, physical_turn_key);
        let sink = family_delivery.wrap(
            convo::BotReplySink::new(config.clone()),
            ReplyScope::Group,
            GuardPolicy::Enforce,
        );

        match convo::send_reply_once(
            workgraph_dir,
            &request_id,
            &member.bot_id,
            target,
            &post.text,
            &sink,
        )
        .await
        {
            Ok(sent) => {
                println!(
                    "[{}] collective: {} replied (sent message_id {})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    post.bot_id,
                    sent.as_deref().unwrap_or("unknown"),
                );
            }
            Err(e) => eprintln!("collective: {} failed to reply: {e}", post.bot_id),
        }
    }
    Ok(())
}

/// Orchestrate a **discussion round** (collective election + an opinion /
/// discussion ask). Instead of four independent replies the family talks it
/// through: each bound-session persona contributes one short in-voice take, in
/// roster order and *reacting* to the takes so far, then the configured
/// coordination owner closes with a synthesis when at least two other voices
/// weighed in.
///
/// This is the deliberative sibling of [`run_group_collective`]: same sole
/// orchestrator (the single listener), same per-voice bot, same
/// persistent-session composer ([`convo::OneshotComposer`]) and casa-feed mirror
/// ([`FamilyReplyDelivery`]). It differs in that the takes are *sequenced with
/// context* and a voice whose session errors or does not answer in time is
/// skipped SILENTLY — no glitch line, no jargon in the family group. The
/// round-runner itself lives in [`telegram_discussion`] and is unit-tested there;
/// this function only resolves the live roster/composer/sink and hands off.
///
/// [`telegram_discussion`]: worksgood::notify::telegram_discussion
pub async fn run_group_discussion(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
    feed_path: &Path,
    human_message: &str,
    sender: &str,
    physical_turn_key: &str,
) -> Result<()> {
    use worksgood::notify::grounding;
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_discussion as discussion;
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::load_project_roster(&project_root(workgraph_dir), config)?;
    let family_delivery =
        FamilyReplyDelivery::load_at(workgraph_dir, config, feed_path.to_path_buf());
    if roster.is_empty() {
        eprintln!("No household roster entries have matching Telegram bots.");
        return Ok(());
    }

    // Only bound-session voices can contribute a grounded, in-character take. A
    // voice with no bound session (or an unconfirmed sender) is left out of the
    // round rather than posting a canned status line into a discussion.
    let mut voices: Vec<discussion::DiscussionVoice> = Vec::new();
    for member in &roster {
        let plan = convo::plan_conversation(
            workgraph_dir,
            config,
            &member.channel_type(),
            target,
            sender,
            convo::Entry::GroupElected,
        );
        if let convo::ConversationPlan::Converse {
            session_ref,
            agent_id,
            ..
        } = plan
        {
            voices.push(discussion::DiscussionVoice {
                bot_id: member.bot_id.clone(),
                display_name: member.display_name.clone(),
                agent_id,
                session_ref,
            });
        }
    }

    if voices.is_empty() {
        // Nobody can speak in character (no bound sessions / unconfirmed sender) —
        // fall back to today's collective reply so the family still hears back.
        return run_group_collective(
            workgraph_dir,
            config,
            target,
            feed_path,
            human_message,
            sender,
            physical_turn_key,
        )
        .await;
    }

    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let synthesizer_bot = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .and_then(|owner| resolve_mentioned_bot(owner, config))
        .map(|bot| bot.bot_id);
    let timing = discussion::DiscussionTiming::from_env();
    println!(
        "[{}] discussion round -> {} ({} voice(s), synthesizer {})",
        chrono::Utc::now().format("%H:%M:%S"),
        target,
        voices.len(),
        synthesizer_bot.as_deref().unwrap_or("none"),
    );

    // Same composer + feed-mirroring sink as the collective path: each take is
    // composed by that persona's bound session and mirrored into the casa
    // conversation pane as an `agent` line.
    let wg_config = worksgood::config::Config::load_merged(workgraph_dir).ok();
    let composer = wg_config.map(convo::OneshotComposer::from_config);
    let composer_ref = match composer.as_ref() {
        Some(c) => c as &dyn convo::ReplyComposer,
        None => {
            eprintln!("No composer available — discussion round falls back to collective reply.");
            return run_group_collective(
                workgraph_dir,
                config,
                target,
                feed_path,
                human_message,
                sender,
                physical_turn_key,
            )
            .await;
        }
    };
    let sink = family_delivery.wrap(
        convo::BotReplySink::new(config.clone()),
        ReplyScope::Group,
        GuardPolicy::AlreadyGuarded,
    );
    let family_roster =
        grounding::load_family_voice_roster(&project_root(workgraph_dir), workgraph_dir);

    let outcome = discussion::run_discussion_round(
        workgraph_dir,
        human_message,
        &voices,
        synthesizer_bot.as_deref().unwrap_or(""),
        composer_ref,
        &family_roster,
        &sink,
        target,
        physical_turn_key,
        timing,
    )
    .await?;

    println!(
        "[{}] discussion round done: {} take(s), synthesis={}, skipped=[{}]",
        chrono::Utc::now().format("%H:%M:%S"),
        outcome.takes.len(),
        outcome.synthesis.is_some(),
        outcome.skipped.join(","),
    );
    Ok(())
}
