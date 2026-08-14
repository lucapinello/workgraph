//! Casa's persona election — which helper takes a turn (task casa-election-classifier).
//!
//! Extracted from `commands/telegram.rs` as slice 5 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). Upstream has no personas to elect between.

use anyhow::Result;
use std::path::Path;
use worksgood::notify::ownership;
use worksgood::notify::telegram_group::{
    Election, elect_responders_with_owner_map, parse_at_mention_tokens,
};

use crate::commands::telegram::{human_agent_id_set, load_telegram_config, project_root};

/// `wg telegram elect` — show who would respond to a group message in
/// all-bots-privacy-off mode, without sending anything.
///
/// Runs the exact [`elect_responders_with_owner_map`] decision the listener uses on a deduped
/// message and prints the outcome: `mention` / `name` / `reply-chain` route to
/// one voice, `collective` fans out to the whole roster, `otto` coordinates a
/// team-directed ask, and `silence` means the bots stay out. Mentions are
/// approximated from any `@handle` tokens (the live listener reads Telegram
/// entities). See docs/09 §natural-group.
pub fn run_elect(
    workgraph_dir: &Path,
    message: &str,
    reply_to_bot: Option<&str>,
    chat_type: &str,
    chat_id: &str,
    human_count_override: Option<usize>,
    json: bool,
) -> Result<()> {
    let config = load_telegram_config()?;

    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);

    // Membership-aware silence: default to the real onboarded-human count so the
    // diagnostic mirrors the live listener, but let `--humans N` preview either
    // side of the boundary (a single-human group answers greetings; 2+ humans
    // keep the conservative silence).
    let human_count =
        human_count_override.unwrap_or_else(|| human_agent_id_set(workgraph_dir).len());

    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let election = elect_responders_with_owner_map(
        Some(chat_type),
        Some(chat_id),
        message,
        &mention_usernames,
        reply_to_bot,
        // The `wg telegram elect` diagnostic is always run by a human operator,
        // never a bot — the bot-loop guard is exercised by the unit tests.
        false,
        human_count,
        &config,
        &owner_map,
    );

    // (kind, who, addressed_by, body) — `who` is the elected agent for the
    // single-voice arms, the roster for `collective`, none for silence/private.
    let (kind, who, addressed_by, body): (&str, Option<String>, Option<String>, String) =
        match &election {
            Election::Private => ("private", None, None, message.to_string()),
            Election::Silence(reason) => (
                "silence",
                None,
                Some(reason.to_string()),
                message.to_string(),
            ),
            Election::All { body, .. } => {
                let roster = worksgood::notify::telegram_standup::load_project_roster(
                    &project_root(workgraph_dir),
                    &config,
                )?
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
            println!("/standup — intercepted; posts the configured household roster")
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
