//! Casa's one-shot Telegram commands: `owner`, `parity`, `capability`, and — added in slice 8
//! — `register-commands`, `route` and `command`. Each answers or performs exactly one thing and
//! exits; none of them exists upstream.
//!
//! Extracted from `commands/telegram.rs` as the fourth slice of the Casa/upstream split
//! (see docs/UPSTREAM-DIVERGENCE.md). None of the three exists upstream — not at our fork
//! point, not on `gwwg/main` today — and none of them is a `wg` concern: each answers one
//! family question and exits.
//!
//! CHOSEN BECAUSE IT IS CLEAN, not because it is big. These three drag no private helpers
//! and share none, so nothing had to be exposed in upstream's file: the marker count there
//! is unchanged. They also carry no tests in that file's test module, so nothing had to be
//! repointed. `run_web_inbound` was the obvious next candidate by size (512 lines) and was
//! deliberately skipped: it needs three new `pub(crate)` markers and ~69 test references
//! moved, and it gets cheaper once `run_listen` goes, taking `run_group_collective` and
//! `run_group_discussion` with it.
//!
//! `run_ask` sits next to these three in that file and is NOT a candidate: it is upstream's.
//!
//! This module borrows NOTHING from `commands::telegram` — the compiler said so, by warning
//! that the two helpers I imported on the strength of a regex match were both unused. So
//! this slice is fully self-contained: no import back into upstream's file, in either
//! direction.

use crate::commands::telegram::human_agent_id_set;
use crate::commands::telegram::load_telegram_config;
use crate::commands::telegram::project_root;
use anyhow::{Context, Result};
use std::path::Path;
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::family_plan;
use worksgood::notify::ownership;
use worksgood::notify::telegram::{TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_family_commands as family_commands;
use worksgood::notify::telegram_group::{
    NaturalRoute, parse_at_mention_tokens, route_natural_with_owner_map,
};

/// How to describe an owner lookup, keeping "nobody owns this" apart from "there was
/// nothing to look in".
///
/// Without `--root` there is no `household.toml` to read, so the map is empty BY
/// CONSTRUCTION — and this used to print the authoritative-sounding "no persona lists this
/// domain" for every ask, from a lookup that never happened. On the live house that reads
/// as a routing failure: `telegram owner "should we move Thursday's dinner?"` said
/// unresolved, while the same ask WITH `--root` resolves to `nora` off a roster that was
/// correct all along. It is the distinction the human-flow pre-flights already refuse to
/// blur between "could not sweep" and "clean": a lookup with no corpus is not a verdict
/// about the corpus.
fn owner_line(roster_loaded: bool, owner: Option<&str>) -> String {
    match (roster_loaded, owner) {
        (_, Some(o)) => o.to_string(),
        (true, None) => "(unresolved — no persona lists this domain)".to_string(),
        (false, None) => {
            "(no roster read — pass --root <project dir> to resolve owners)".to_string()
        }
    }
}

pub fn run_owner(
    ask: &str,
    persona: Option<&str>,
    root: Option<&Path>,
    _dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::ownership::{self, OwnerDecision, OwnerMap};

    let domain = ownership::classify_domain(ask);
    let (map, roster_loaded) = match root {
        Some(r) => (OwnerMap::load(r), true),
        None => (OwnerMap::default(), false),
    };
    let owner = map.owner_for_ask(ask).map(str::to_string);
    let decision = persona.map(|p| map.decide_owner(p, ask));

    if json {
        let (decision_slug, defer_to) = match &decision {
            Some(OwnerDecision::Owner) => ("owner", None),
            Some(OwnerDecision::Defer { owner }) => ("defer", Some(owner.clone())),
            None => ("n/a", None),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ask": ask,
                "domain": domain.slug(),
                "owner": owner,
                "persona": persona,
                "decision": decision_slug,
                "deferTo": defer_to,
            }))?
        );
        return Ok(());
    }

    println!("ask:     \"{ask}\"");
    println!("domain:  {}", domain.slug());
    println!("owner:   {}", owner_line(roster_loaded, owner.as_deref()));
    match (persona, &decision) {
        (Some(p), Some(OwnerDecision::Owner)) => {
            println!("verdict: {p} OWNS this ask → it creates the task");
        }
        (Some(p), Some(OwnerDecision::Defer { owner })) => {
            println!(
                "verdict: {p} is OFF-DOMAIN → defers to {owner} (re-routed, never its own copy)"
            );
        }
        _ => {}
    }
    Ok(())
}

/// Audit a composed reply for promise-action parity — the `wg telegram parity`
/// seam (see [`crate::cli::TelegramCommands::Parity`]).
///
/// Runs the exact pattern-based classifier the live conversational turn uses and
/// prints: the promise kind (`action` / `preference` / `none`), whether the
/// reply already carries a `TASK_CREATE:` tail, and whether there is a MISMATCH
/// a live turn would repair (a one-off action promised with no artifact). No
/// side effects — nothing is sent, no task is created.
pub fn run_parity(reply_text: &str, human: Option<&str>, _dry_run: bool, json: bool) -> Result<()> {
    use worksgood::notify::lifecycle;
    use worksgood::notify::parity::{self, PromiseKind};

    // The audit runs over the human-facing reply, with any machine tail stripped
    // — exactly as the live turn sees it. With the human's ask in hand it is the
    // INTENT-AWARE audit the live turn runs (task capability-answer-no-invented-work);
    // with no ask it stays the reply-only classifier.
    let directive = lifecycle::extract_task_directive(reply_text.trim());
    let audit = match human {
        Some(ask) => parity::audit_promise_in_turn(ask, &directive.reply),
        None => parity::audit_promise(&directive.reply),
    };
    // Did the human ask for work at all? A turn that did not can never receive a
    // correction tail, however the reply is worded.
    let asked_for_action = human.map(parity::turn_requests_action);
    let has_artifact = directive.title.is_some();
    // A mismatch is the parity gap: a one-off action promised, no artifact.
    let mismatch = audit.commits_action() && !has_artifact;
    // What the live turn would actually append to the reply.
    let correction = if mismatch && asked_for_action.unwrap_or(true) {
        Some(parity::correction_line())
    } else {
        None
    };
    let fallback_title = if mismatch {
        Some(parity::fallback_task_title(
            human.unwrap_or(""),
            &directive.reply,
        ))
    } else {
        None
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "promised": audit.kind.slug(),
                "commits": audit.commits(),
                "hasArtifact": has_artifact,
                "artifactTitle": directive.title,
                "mismatch": mismatch,
                "matched": audit.matched,
                "fallbackTitle": fallback_title,
                "requestedAction": asked_for_action,
                "correction": correction,
            }))?
        );
        return Ok(());
    }

    println!("promised: {}", audit.kind.slug());
    if let Some(m) = &audit.matched {
        println!("matched:  \"{m}\"");
    }
    if let Some(asked) = asked_for_action {
        println!(
            "asked:    the turn {} ask for work",
            if asked { "DID" } else { "did NOT" }
        );
    }
    match directive.title {
        Some(t) => println!("artifact: TASK_CREATE present → \"{t}\""),
        None => println!("artifact: none"),
    }
    match audit.kind {
        PromiseKind::Preference => {
            println!("verdict:  standing preference → would be written to the durable store");
        }
        PromiseKind::Action if mismatch => {
            println!("verdict:  MISMATCH → promised an action but no artifact");
            println!("          a live turn would retry once, then fall back to task:");
            println!("          \"{}\"", fallback_title.unwrap_or_default());
            match &correction {
                Some(c) => println!("correction: \"{c}\""),
                None => println!("correction: none — the turn requested no action"),
            }
        }
        PromiseKind::Action => {
            println!("verdict:  action promised AND artifact present → parity OK");
        }
        PromiseKind::None => {
            println!("verdict:  no commitment → nothing owed");
        }
    }
    Ok(())
}

/// The single-owner routing test seam — the `wg telegram owner` command (see
/// [`crate::cli::TelegramCommands::Owner`]).
///
/// Runs the exact pure classifier the live conversational turn uses and prints
/// the ask's household domain, the single persona that owns it, and — with
/// `--persona` — whether that voice would create the task or defer to the owner.
/// No side effects: nothing is sent, no task created.
/// The CAPABILITY act seam — the `wg telegram capability` command (see
/// [`crate::cli::TelegramCommands::Capability`]).
///
/// Prints whether `text` is a bare capability ask and, when it is, the answer the
/// configured household gives ([`worksgood::notify::capability`]). Also reports the
/// promise audit of that answer, because the live-cert C011 failure was not the
/// answer's content but the phantom promise appended to it. Side-effect-free.
pub fn run_capability(text: &str, root: Option<&Path>, _dry_run: bool, json: bool) -> Result<()> {
    use worksgood::notify::capability;
    use worksgood::notify::ownership::OwnerMap;
    use worksgood::notify::parity;

    let is_ask = capability::is_capability_ask(text);
    let (map, roster_loaded) = match root {
        Some(r) => (OwnerMap::load(r), true),
        None => (OwnerMap::default(), false),
    };
    let answer = if is_ask {
        capability::capability_answer(&map)
    } else {
        None
    };
    // The answer must never itself commit to work (that is the C011 defect).
    let promised = answer.as_deref().map(|a| {
        parity::audit_promise_in_turn(text, a)
            .kind
            .slug()
            .to_string()
    });

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "text": text,
                "capabilityAsk": is_ask,
                "answer": answer,
                "promised": promised,
                "requestedAction": parity::turn_requests_action(text),
            }))?
        );
        return Ok(());
    }

    println!("capability ask: {}", if is_ask { "yes" } else { "no" });
    match &answer {
        Some(a) => {
            println!("answer:  {a}");
            println!("promised: {}", promised.as_deref().unwrap_or("none"));
        }
        None => println!(
            "answer:  none — {}",
            capability_none_reason(roster_loaded, is_ask)
        ),
    }
    Ok(())
}

/// Why a capability ask produced no answer — same distinction as [`owner_line`]. The
/// "this household declares no domain ownership" wording is a claim ABOUT a household, so
/// it may only be used when a household was actually read. Without `--root` nothing was.
fn capability_none_reason(roster_loaded: bool, is_ask: bool) -> &'static str {
    match (is_ask, roster_loaded) {
        (false, _) => "not a capability ask, the normal path owns it",
        (true, true) => "this household declares no domain ownership",
        (true, false) => "no roster read — pass --root <project dir>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this pins: an empty-by-construction map reported as a verdict about the
    /// roster. `owner_line(false, None)` and `owner_line(true, None)` must not be the same
    /// sentence, or the caller cannot tell "nobody owns this" from "I read nothing".
    #[test]
    fn owner_line_keeps_no_roster_apart_from_no_owner() {
        assert_eq!(owner_line(true, Some("nora")), "nora");
        assert_eq!(owner_line(false, Some("nora")), "nora");
        let no_owner = owner_line(true, None);
        let no_roster = owner_line(false, None);
        assert!(
            no_owner.contains("no persona lists this domain"),
            "{no_owner}"
        );
        assert!(no_roster.contains("--root"), "{no_roster}");
        assert_ne!(
            no_owner, no_roster,
            "a lookup that loaded no roster must not read as a verdict about the roster"
        );
    }

    /// Same property on the capability side, where the misleading wording made a claim
    /// about the household ("declares no domain ownership") from an unread file.
    #[test]
    fn capability_reason_never_claims_a_household_it_did_not_read() {
        assert!(capability_none_reason(true, true).contains("this household"));
        let unread = capability_none_reason(false, true);
        assert!(!unread.contains("this household"), "{unread}");
        assert!(unread.contains("--root"), "{unread}");
        // Not-an-ask outranks both: there is nothing to resolve either way.
        assert_eq!(
            capability_none_reason(true, false),
            capability_none_reason(false, false)
        );
    }
}

// ── added in slice 8 ─────────────────────────────────────────────────────────────────
// Three more one-shot seams, moved for the same reason and on the same test: ours, no helper
// of upstream's needed, and no test in their file to repoint.

/// [`compose_family_reply`] with an explicit `today`/`now` (for deterministic
/// tests and the `--today` dry-run flag).
pub(crate) fn compose_family_reply_on(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    cmd: &family_commands::FamilyCommand,
    today: chrono::NaiveDate,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<String> {
    let root = project_root(workgraph_dir);
    let plans = family_plan::load_plans(&root);
    let graph = worksgood::parser::load_graph(crate::commands::graph_path(workgraph_dir)).ok();
    let humans = human_agent_id_set(workgraph_dir);
    let roster = if cmd.kind == family_commands::CommandKind::Roster {
        worksgood::notify::telegram_standup::load_project_roster(&root, config)?
    } else {
        Vec::new()
    };
    let ctx = family_commands::CommandContext {
        graph: graph.as_ref(),
        roster: &roster,
        plans: &plans,
        today,
        now,
        human_agents: &humans,
    };
    Ok(family_commands::compose(cmd, &ctx))
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
    workgraph_dir: &Path,
    message: &str,
    reply_to_bot: Option<&str>,
    chat_type: &str,
    chat_id: &str,
    json: bool,
) -> Result<()> {
    let config = load_telegram_config()?;

    // Approximate the listener's mention extraction: any @handle token.
    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));

    let route = route_natural_with_owner_map(
        Some(chat_type),
        Some(chat_id),
        message,
        &mention_usernames,
        reply_to_bot,
        &config,
        &owner_map,
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
            println!("/standup — intercepted; posts the configured household roster")
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

    let text = compose_family_reply_on(workgraph_dir, &config, cmd, today, now)?;
    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let owner = cmd.owner(&owner_map);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": cmd.keyword,
                "domain": cmd.domain.slug(),
                "owner": owner,
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
