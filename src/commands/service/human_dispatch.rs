//! Human-as-agent dispatch tail (R10 / R11 / R13).
//!
//! Humans are first-class [`Agent`]s (`Agent::is_human()`), excluded from AI
//! assignment (`assignment_eligibility`) but still legitimate assignees for a
//! task. The upstream series wired half of the human dispatch path:
//! `WaitCondition::HumanInput` is satisfied by `has_non_agent_message_since(...)`
//! and `wg wait --condition human-input` can set it. What was missing — and
//! explicitly deferred at `src/notify/telegram.rs:42` ("the `awaiting-human`
//! task router — see follow-up PR") — is the *tail* that closes the loop:
//!
//! 1. **Park (R10).** When a ready task is assigned to a human agent, the
//!    coordinator must not spawn an AI worker for it. Instead it transitions
//!    the task to `Waiting` on `WaitCondition::HumanInput`
//!    ([`park_ready_human_tasks`]).
//! 2. **Render (R11).** The task title + description are pushed to the human
//!    through their notification channel — their Telegram bot binding when
//!    configured, honoring the multi-bot config ([`notify_parked_human`]).
//! 3. **Route the reply back (R13).** When the human replies, the inbound
//!    message satisfies the wait condition (already handled by the coordinator)
//!    AND is recorded on the task: it is already a message, and where the task
//!    declares a deliverable the reply is written as a reply-to-artifact. The
//!    task then completes rather than resuming to `Open`
//!    ([`try_complete_human_task_on_reply`]) — resuming would re-park it in an
//!    endless loop since there is no AI agent to spawn.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;

use worksgood::agency::{self, Agent, TelegramBindingMap};
use worksgood::graph::{
    LogEntry, Status, Task, TaskChoice, WaitCondition, WaitSpec, WorkGraph, is_system_task,
};
use worksgood::messages;
use worksgood::notify::{Action, ActionStyle, NotificationChannel};
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::telegram::{TelegramChannel, TelegramConfig};
use worksgood::query::ready_tasks_with_peers_cycle_aware;

/// Separator between the task id and the button key in a generic inline-button
/// callback token (`<task_id>#<key>`). Replaces the legacy hard-coded
/// `<verb>:<task>` scheme so any task can route its own buttons back to itself
/// without the listener knowing the verbs in advance (R18).
pub const BUTTON_TOKEN_SEP: char = '#';

/// Build the generic callback token for a task's choice: `<task_id>#<key>`.
pub fn button_token(task_id: &str, choice_key: &str) -> String {
    format!("{task_id}{BUTTON_TOKEN_SEP}{choice_key}")
}

/// Parse a generic inline-button callback token into `(task_id, button_key)`.
///
/// Splits on the FIRST `#` only, so a task id may itself contain no `#` (task
/// ids are slugs) while the key is whatever follows. Returns `None` for tokens
/// that carry no separator (e.g. the legacy `<verb>:<task>` form), letting the
/// caller fall back to legacy handling.
pub fn parse_button_token(token: &str) -> Option<(&str, &str)> {
    token
        .split_once(BUTTON_TOKEN_SEP)
        .filter(|(task, key)| !task.is_empty() && !key.is_empty())
}

/// Text embedded in the park log entry. `evaluate_waiting_tasks` derives a
/// task's `wait_started` timestamp from the most recent log line containing
/// "Agent parked", so reusing that phrase makes the human-input clock start
/// at the moment we park — only replies newer than this count.
const PARK_LOG_MARKER: &str = "Agent parked: awaiting human input";

/// A task that was newly parked on `HumanInput` this tick, carried out of the
/// graph lock so its notification (network I/O) can be sent without holding it.
#[derive(Debug, Clone)]
pub struct ParkedHumanTask {
    pub task_id: String,
    pub agent_id: String,
    pub title: String,
    pub description: String,
    /// Declared choices (R18). When non-empty the notification is sent with one
    /// inline button per choice instead of a plain "reply to complete" prompt.
    pub choices: Vec<TaskChoice>,
}

/// True when `agent_id` resolves to a human operator agent (`Agent::is_human()`).
///
/// Used by the auto-assigner to refuse to override an explicit human
/// assignment (R10): a task pinned to a human via `wg assign` must be parked
/// for that human, never handed to the LLM assigner which would replace the
/// human with an AI agent. Unknown / unresolvable ids are treated as non-human
/// (fail open — an id we can't resolve is not a human we must protect).
pub fn agent_id_is_human(dir: &Path, agent_id: &str) -> bool {
    let agents_dir = dir.join("agency").join("cache/agents");
    agency::find_agent_by_prefix(&agents_dir, agent_id)
        .map(|a| a.is_human())
        .unwrap_or(false)
}

/// Load the set of agent ids that are human operators (matrix / email / shell
/// executors).
fn human_agent_ids(dir: &Path) -> HashSet<String> {
    let agents_dir = dir.join("agency").join("cache/agents");
    agency::load_all_agents_or_warn(&agents_dir)
        .into_iter()
        .filter(|a| a.is_human())
        .map(|a| a.id)
        .collect()
}

/// Park every ready, human-assigned task on `WaitCondition::HumanInput` (R10).
///
/// A ready task assigned to a human must not be handed to the AI spawn path;
/// this transitions it to `Waiting` so `spawn_agents_for_ready_tasks` skips it
/// and the human's reply (an inbound non-agent message) is what unblocks it.
///
/// Returns the tasks newly parked this pass so the caller can notify them
/// outside the graph lock via [`notify_parked_human`].
pub fn park_ready_human_tasks(graph: &mut WorkGraph, dir: &Path) -> Vec<ParkedHumanTask> {
    let humans = human_agent_ids(dir);
    if humans.is_empty() {
        return Vec::new();
    }

    // Collect ids first (immutable borrow) before mutating.
    let cycle_analysis = graph.compute_cycle_analysis();
    let target_ids: Vec<String> = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis)
        .iter()
        .filter(|t| t.wait_condition.is_none())
        .filter(|t| !is_system_task(&t.id))
        .filter(|t| {
            t.agent
                .as_deref()
                .map(|a| humans.contains(a))
                .unwrap_or(false)
        })
        .map(|t| t.id.clone())
        .collect();
    drop(cycle_analysis);

    let mut parked = Vec::new();
    for task_id in target_ids {
        if let Some(t) = graph.get_task_mut(&task_id) {
            let agent_id = t.agent.clone().unwrap_or_default();
            t.status = Status::Waiting;
            t.wait_condition = Some(WaitSpec::All(vec![WaitCondition::HumanInput]));
            t.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("coordinator".to_string()),
                user: Some(worksgood::current_user()),
                message: format!(
                    "{} (assigned to human agent '{}')",
                    PARK_LOG_MARKER, agent_id
                ),
            });
            parked.push(ParkedHumanTask {
                task_id: t.id.clone(),
                agent_id,
                title: t.title.clone(),
                description: t.description.clone().unwrap_or_default(),
                choices: t.choices.clone(),
            });
        }
    }
    parked
}

/// Best-effort: render a parked human task through the human's notification
/// channel (R11). Never fails the tick — logs on error.
pub fn notify_parked_human(dir: &Path, parked: &ParkedHumanTask) {
    match try_notify_parked_human(dir, parked) {
        Ok(Some(bot)) => eprintln!(
            "[dispatcher] Notified human agent '{}' of task '{}' via {}",
            parked.agent_id, parked.task_id, bot
        ),
        Ok(None) => {
            // No channel configured for this human — the task still waits; a
            // human can reply via any surface that records a message on it.
        }
        Err(e) => eprintln!(
            "[dispatcher] Failed to notify human for task '{}': {}",
            parked.task_id, e
        ),
    }
}

/// Send the task title/description to the human's Telegram bot binding.
///
/// Bot selection (multi-bot aware): prefer a bot whose `agent_id` matches the
/// assigned human (by workgraph id OR by name); otherwise fall back to a shared
/// bot with no agent binding. Returns the bot's channel type on success, or
/// `Ok(None)` when telegram is not configured / no bot resolves.
fn try_notify_parked_human(dir: &Path, parked: &ParkedHumanTask) -> Result<Option<String>> {
    let agents_dir = dir.join("agency").join("cache/agents");
    let agent_name = agency::find_agent_by_prefix(&agents_dir, &parked.agent_id)
        .ok()
        .map(|a| a.name);

    let notify_config = match load_notify_config(dir)? {
        Some(c) => c,
        None => return Ok(None),
    };

    let channels = TelegramChannel::all_from_notify_config(&notify_config)
        .context("building telegram channels")?;
    if channels.is_empty() {
        return Ok(None);
    }

    let matches_agent = |bot_agent: &str| -> bool {
        bot_agent == parked.agent_id
            || agent_name
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(bot_agent))
                .unwrap_or(false)
    };
    let chosen = channels
        .iter()
        .find(|c| c.agent_id().map(matches_agent).unwrap_or(false))
        .or_else(|| channels.iter().find(|c| c.agent_id().is_none()));
    let channel = match chosen {
        Some(c) => c,
        None => return Ok(None),
    };

    let text = format_human_task_message(parked);
    let target = channel.chat_id().to_string();
    let bot_label = channel.channel_type().to_string();

    // R18: when the task declares choices, render them as inline buttons whose
    // callback data is the generic `<task_id>#<key>` routing token. A tap comes
    // back through the listener and `route_button_callback` records the chosen
    // label as the human's reply. With no choices, keep the free-text prompt.
    let actions = button_actions(&parked.task_id, &parked.choices);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building telegram runtime")?;
    rt.block_on(async {
        if actions.is_empty() {
            channel.send_text(&target, &text).await
        } else {
            channel.send_with_actions(&target, &text, &actions).await
        }
    })
    .context("telegram send")?;
    Ok(Some(bot_label))
}

/// Load notify config, preferring the project-local `notify.toml` next to the
/// graph, then the standard `.wg/notify.toml` / global lookup.
fn load_notify_config(dir: &Path) -> Result<Option<NotifyConfig>> {
    let local = dir.join("notify.toml");
    if local.exists() {
        return Ok(Some(NotifyConfig::load_from(&local)?));
    }
    NotifyConfig::load(dir.parent())
}

/// Human-readable rendering of a task handed to a person.
fn format_human_task_message(parked: &ParkedHumanTask) -> String {
    let mut s = format!("📋 Task for you: {}\n{}", parked.task_id, parked.title);
    let desc = parked.description.trim();
    if !desc.is_empty() {
        s.push_str("\n\n");
        s.push_str(desc);
    }
    // With declared choices the buttons carry the call-to-action; a free-text
    // prompt would be misleading (the reply comes from a tap, not typing).
    if parked.choices.is_empty() {
        s.push_str("\n\nReply to this message to complete the task.");
    } else {
        s.push_str("\n\nTap a button below to answer.");
    }
    s
}

/// Build the inline-button [`Action`]s for a parked task's declared choices.
///
/// Each button's id is the generic `<task_id>#<key>` routing token; the first
/// choice is styled `Primary` (the affirmative default), the rest `Secondary`.
/// Returns an empty vec when the task declares no choices — callers fall back to
/// a plain text message in that case.
fn button_actions(task_id: &str, choices: &[TaskChoice]) -> Vec<Action> {
    choices
        .iter()
        .enumerate()
        .map(|(i, c)| Action {
            id: button_token(task_id, &c.key),
            label: c.label.clone(),
            style: if i == 0 {
                ActionStyle::Primary
            } else {
                ActionStyle::Secondary
            },
        })
        .collect()
}

/// Close the human loop when a parked task's wait condition is satisfied (R13).
///
/// Called from the coordinator's satisfied-wait branch BEFORE the generic
/// resume-to-`Open` transition. If the task's assigned agent is a human, the
/// newest non-agent message since `wait_started` is their reply: it is written
/// as a reply-to-artifact for every declared deliverable, recorded in the log,
/// and the task is marked `Done`. Returns `true` when it handled the task (the
/// caller must then skip the generic resume path); `false` for non-human tasks,
/// leaving them to the normal resume.
pub fn try_complete_human_task_on_reply(
    graph: &mut WorkGraph,
    dir: &Path,
    task_id: &str,
    wait_started: Option<&str>,
) -> bool {
    let agent_id = match graph.get_task(task_id).and_then(|t| t.agent.clone()) {
        Some(a) => a,
        None => return false,
    };

    let agents_dir = dir.join("agency").join("cache/agents");
    let is_human = agency::find_agent_by_prefix(&agents_dir, &agent_id)
        .map(|a| a.is_human())
        .unwrap_or(false);
    if !is_human {
        return false;
    }

    let reply = latest_human_reply(dir, task_id, wait_started);

    let deliverables = graph
        .get_task(task_id)
        .map(|t| t.deliverables.clone())
        .unwrap_or_default();
    let mut written_artifacts = Vec::new();
    if let Some(ref body) = reply {
        for deliverable in &deliverables {
            match write_reply_artifact(dir, deliverable, body) {
                Ok(path) => written_artifacts.push(path),
                Err(e) => eprintln!(
                    "[dispatcher] Failed to write reply artifact '{}' for task '{}': {}",
                    deliverable, task_id, e
                ),
            }
        }
    }

    if let Some(t) = graph.get_task_mut(task_id) {
        t.status = Status::Done;
        t.wait_condition = None;
        t.completed_at = Some(Utc::now().to_rfc3339());
        for a in &written_artifacts {
            if !t.artifacts.contains(a) {
                t.artifacts.push(a.clone());
            }
        }
        let summary = match &reply {
            Some(body) => {
                let preview: String = body.chars().take(80).collect();
                format!("Human reply received; task complete. Reply: {}", preview)
            }
            None => "Human input received; task complete.".to_string(),
        };
        t.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: Some("coordinator".to_string()),
            user: Some(worksgood::current_user()),
            message: summary,
        });
    }
    true
}

/// The newest non-agent message on `task_id` recorded after `wait_started`
/// (the human's reply). Matches the sender predicate the coordinator's
/// `has_non_agent_message_since` uses for `WaitCondition::HumanInput`.
fn latest_human_reply(dir: &Path, task_id: &str, wait_started: Option<&str>) -> Option<String> {
    let msgs = messages::list_messages(dir, task_id).ok()?;
    let wait_time = wait_started.and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
    msgs.into_iter()
        .filter(|m| !m.sender.starts_with("agent-"))
        .filter(|m| match wait_time {
            Some(wt) => m
                .timestamp
                .parse::<chrono::DateTime<chrono::Utc>>()
                .map(|t| t > wt)
                .unwrap_or(false),
            None => true,
        })
        .last()
        .map(|m| m.body)
}

/// The result of routing an inbound human reply through sender authorization.
///
/// The distinction matters for the listener: a [`Rejected`](Self::Rejected)
/// reply is a *security* event (an unproven sender tried to answer for a human)
/// and is logged with its reason, whereas [`NoWaitingTask`](Self::NoWaitingTask)
/// is the benign "your reply arrived but nothing was waiting" case. The reason
/// string is log-only — never surfaced verbatim to the sender — so it does not
/// leak which humans exist or which tasks are parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundReplyOutcome {
    /// The reply was authorized and recorded on this task id.
    Recorded(String),
    /// The sender proved their confirmed binding, but the human they are bound
    /// to had no parked task awaiting a reply — nothing to record.
    NoWaitingTask,
    /// The sender is a confirmed human, but the receiving bot fronts an AI
    /// persona (not the human this sender is bound to). This is the ordinary
    /// Casa case — a confirmed human simply chatting in a group an AI persona's
    /// bot also listens on — so it is NOT a parked-task reply and is expected to
    /// fall through to the conversation composer. It is benign, distinct from a
    /// [`Rejected`](Self::Rejected) security event, so the listener can log it
    /// neutrally instead of crying wolf on every normal message. Carries the
    /// persona's DISPLAY NAME (never the raw agent id/hash).
    NotParkedReply { persona: String },
    /// The reply was rejected before recording. The string is a log-only
    /// reason (unrecognized/unconfirmed sender, or a sender answering for a
    /// human they are not bound to).
    Rejected(String),
}

/// Route an inbound human reply (delivered by a notification listener) onto the
/// awaiting-human task it answers, recording it as a message (R13). This is the
/// "awaiting-human task router" that `src/notify/telegram.rs` deferred.
///
/// # Sender authorization (PR #51 hardening)
///
/// Recording a message on a parked task is exactly what satisfies its
/// `WaitCondition::HumanInput` and completes it — so *who* is allowed to record
/// that message is a security boundary. The earlier tail authorized on the
/// receiving bot's binding plus "freshest waiting task", which let **any**
/// non-command sender visible to a (shared) bot be recorded as the assigned
/// human's reply. This function instead proves the sender against the
/// **confirmed** Telegram binding (`TelegramBindingMap`, R21/R22) for the human
/// the task is assigned to:
///
/// 1. The inbound `sender` must have a binding, and it must be `confirmed`
///    (the `YES` handshake completed). An unknown or unconfirmed sender is
///    [`Rejected`](InboundReplyOutcome::Rejected).
/// 2. The reply is only eligible for tasks assigned to *that binding's* human
///    agent — not any human, and not the freshest ask across all humans. A
///    sender confirmed for human A therefore cannot answer human B's task.
/// 3. Defense in depth: if the receiving bot fronts a specific agent, it must
///    be the same human the sender is bound to (a confirmed sender for A may
///    not answer through B's dedicated bot).
///
/// Among the authorized human's parked tasks the freshest (newest park time)
/// wins, matching the coordinator's `HumanInput` completion path.
pub fn route_inbound_reply(
    dir: &Path,
    channel_type: &str,
    sender: &str,
    sender_username: Option<&str>,
    body: &str,
) -> InboundReplyOutcome {
    let graph = match worksgood::parser::load_graph(&crate::commands::graph_path(dir)) {
        Ok(g) => g,
        Err(e) => return InboundReplyOutcome::Rejected(format!("failed to load graph: {e}")),
    };
    let agency_dir = dir.join("agency");
    let agents_dir = agency_dir.join("cache/agents");
    let agents = agency::load_all_agents_or_warn(&agents_dir);

    let human_ids: HashSet<&str> = agents
        .iter()
        .filter(|a| a.is_human())
        .map(|a| a.id.as_str())
        .collect();
    if human_ids.is_empty() {
        return InboundReplyOutcome::NoWaitingTask;
    }

    // Authorization: the sender must prove a CONFIRMED binding. The bound
    // agent id — not "any human" or the freshest ask — is the only human this
    // reply may answer for. Resolution uses the SAME `matches_sender(id,
    // username)` identity contract as `apply_confirmation` (via
    // `find_by_sender`): a numeric binding matches the stable `from.id`, a
    // `@handle` binding matches `from.username`. A live inbound reply always
    // arrives as a numeric `sender` (`from.id`) plus an optional
    // `sender_username`, so a handle binding that was confirmed on `YES` must
    // also route the subsequent numeric-sender replies — equality-only keying
    // on `sender` would confirm the handshake and then reject every real reply.
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let authorized_agent = match bindings.find_by_sender(sender, sender_username) {
        None => {
            return InboundReplyOutcome::Rejected(format!(
                "unrecognized sender '{sender}': no confirmed Telegram binding"
            ));
        }
        Some(b) if !b.confirmed => {
            return InboundReplyOutcome::Rejected(format!(
                "sender '{sender}' is bound to '{}' but has not confirmed (YES handshake pending)",
                b.agent_id
            ));
        }
        Some(b) => b.agent_id.clone(),
    };

    // The bound agent must actually be a live human agent — a stale binding to
    // a removed/AI agent must not authorize anything.
    if !human_ids.contains(authorized_agent.as_str()) {
        return InboundReplyOutcome::Rejected(format!(
            "sender '{sender}' is bound to '{authorized_agent}', which is not a known human agent"
        ));
    }

    // Defense in depth: a per-human bot must front the same human the sender is
    // bound to. (Shared/default bots front no specific agent and skip this.)
    if let Some(bound) = bound_agent_for_channel(dir, channel_type, &agents) {
        if bound != authorized_agent {
            // The receiving bot fronts an agent OTHER than the human this sender
            // is bound to. Two very different situations land here:
            //
            //  - It fronts a DIFFERENT HUMAN: a genuine mis-route / spoof — a
            //    confirmed sender for human A trying to answer through human B's
            //    dedicated bot. That stays a clearly-logged security event.
            //
            //  - It fronts an AI PERSONA (the common Casa case): a confirmed
            //    human simply chatting in a group an AI persona's bot also
            //    listens on. This is NOT a parked-task reply and is expected to
            //    fall through to the conversation composer. Reporting it as
            //    `Rejected` cried wolf on every ordinary message, so it gets a
            //    benign outcome carrying the persona's display name.
            if human_ids.contains(bound.as_str()) {
                return InboundReplyOutcome::Rejected(format!(
                    "sender '{sender}' (bound to '{authorized_agent}') arrived on a bot fronting human '{bound}'"
                ));
            }
            // `bound_agent_for_channel` only returns an id that matched a loaded
            // agent, so the name lookup below resolves; fall back to the id only
            // defensively (a concurrently-removed persona).
            let persona = agents
                .iter()
                .find(|a| a.id == bound)
                .map(|a| a.name.clone())
                .unwrap_or(bound);
            return InboundReplyOutcome::NotParkedReply { persona };
        }
    }

    // Only the authorized human's own parked tasks are eligible; freshest wins.
    let target = graph
        .tasks()
        .filter(|t| t.status == Status::Waiting)
        .filter(|t| waits_on_human_input(t))
        .filter(|t| t.agent.as_deref() == Some(authorized_agent.as_str()))
        .max_by_key(|t| park_time(t));

    let task_id = match target {
        Some(t) => t.id.clone(),
        None => return InboundReplyOutcome::NoWaitingTask,
    };

    match messages::send_message(dir, &task_id, body, sender, "normal") {
        Ok(_) => InboundReplyOutcome::Recorded(task_id),
        Err(e) => {
            InboundReplyOutcome::Rejected(format!("failed to record reply on '{task_id}': {e}"))
        }
    }
}

/// Route a generic inline-button callback (`<task_id>#<key>`) back to its
/// originating task (R18), recording the chosen option as the human's reply.
///
/// This is the button analogue of [`route_inbound_reply`]: where a typed reply
/// lands on the *freshest* awaiting-human task, a button tap carries its target
/// task id in the callback token, so it routes to that exact task. The button
/// `key` is resolved to the task's declared [`TaskChoice`] and that choice's
/// `label` is recorded as an inbound message — identical to what a typed reply
/// would do, so the coordinator's next tick completes the task and writes the
/// label as a reply-to-artifact for every declared deliverable.
///
/// Returns `(task_id, chosen_label)` on success. Returns `None` when the token
/// is not a `#`-form button token (the caller then falls back to legacy
/// `<verb>:<task>` handling), the task is unknown, or `key` matches no declared
/// choice (a stale button whose choice was removed).
pub fn route_button_callback(dir: &Path, token: &str, sender: &str) -> Option<(String, String)> {
    let (task_id, key) = parse_button_token(token)?;
    let graph = worksgood::parser::load_graph(&crate::commands::graph_path(dir)).ok()?;
    let task = graph.get_task(task_id)?;
    let label = task
        .choices
        .iter()
        .find(|c| c.key == key)
        .map(|c| c.label.clone())?;
    messages::send_message(dir, task_id, &label, sender, "normal").ok()?;
    Some((task_id.to_string(), label))
}

/// True if a task's wait spec includes `WaitCondition::HumanInput`.
fn waits_on_human_input(task: &Task) -> bool {
    match &task.wait_condition {
        Some(WaitSpec::All(c) | WaitSpec::Any(c)) => c
            .iter()
            .any(|cond| matches!(cond, WaitCondition::HumanInput)),
        None => false,
    }
}

/// The park timestamp for ordering candidate tasks — the most recent park log
/// entry, falling back to `created_at`, then the empty string.
fn park_time(task: &Task) -> String {
    task.log
        .iter()
        .rev()
        .find(|l| l.message.contains(PARK_LOG_MARKER))
        .map(|l| l.timestamp.clone())
        .or_else(|| task.created_at.clone())
        .unwrap_or_default()
}

/// Resolve which human agent id (if any) a receiving bot fronts, from the
/// telegram multi-bot config. `channel_type` is "telegram" (the legacy/default
/// bot) or "telegram:<bot_id>". The bot's `agent_id` binding is matched against
/// each human agent's workgraph id OR name.
fn bound_agent_for_channel(dir: &Path, channel_type: &str, agents: &[Agent]) -> Option<String> {
    let notify_config = load_notify_config(dir).ok().flatten()?;
    let tg = TelegramConfig::from_notify_config(&notify_config).ok()?;

    let want_bot_id = channel_type.strip_prefix("telegram:").unwrap_or("default");
    let binding = tg
        .all_bots()
        .into_iter()
        .find(|(id, _)| id == want_bot_id)
        .and_then(|(_, cfg)| cfg.agent_id)?;

    agents
        .iter()
        .find(|a| a.id == binding || a.name.eq_ignore_ascii_case(&binding))
        .map(|a| a.id.clone())
}

/// Write a human reply to a declared deliverable path (reply-to-artifact).
///
/// Deliverables are repo-relative; the workgraph data dir's parent is the repo
/// root. Returns the deliverable string to record in `task.artifacts`.
fn write_reply_artifact(dir: &Path, deliverable: &str, body: &str) -> std::io::Result<String> {
    let path = Path::new(deliverable);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.parent().unwrap_or(dir).join(path)
    };
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&abs, body)?;
    Ok(deliverable.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use worksgood::agency::PerformanceRecord;
    use worksgood::graph::Node;

    fn write_human_agent(dir: &Path, id: &str, name: &str) {
        let agents_dir = dir.join("agency").join("cache/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let agent = Agent {
            id: id.to_string(),
            role_id: "human".to_string(),
            tradeoff_id: "default".to_string(),
            name: name.to_string(),
            performance: PerformanceRecord::default(),
            lineage: Default::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            // matrix / email / shell mark a human operator (is_human_executor).
            executor: "shell".to_string(),
            preferred_model: None,
            preferred_provider: None,
            deployment_history: vec![],
            attractor_weight: 0.5,
            staleness_flags: vec![],
        };
        agency::save_agent(&agent, &agents_dir).unwrap();
    }

    fn ready_task(id: &str, agent: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status: Status::Open,
            agent: agent.map(String::from),
            ..Default::default()
        }
    }

    /// Write a Telegram binding into the agency store for the router's auth
    /// check. `confirmed` toggles whether the `YES` handshake completed.
    fn write_binding(dir: &Path, telegram_user: &str, agent_id: &str, name: &str, confirmed: bool) {
        use worksgood::agency::TelegramBinding;
        let agency_dir = dir.join("agency");
        let mut map = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
        let mut b = TelegramBinding::new(
            telegram_user,
            agent_id,
            name,
            None,
            "2026-07-10T12:00:00Z".parse().unwrap(),
        );
        if confirmed {
            b.confirmed = true;
            b.confirmed_at = Some("2026-07-10T12:03:00Z".parse().unwrap());
        }
        map.add(b).unwrap();
        map.save(&agency_dir).unwrap();
    }

    /// Park a human task and persist the graph so `route_inbound_reply` (which
    /// loads from disk) can see it.
    fn park_and_persist(graph: &mut WorkGraph, dir: &Path) {
        park_ready_human_tasks(graph, dir);
        worksgood::parser::save_graph(graph, crate::commands::graph_path(dir)).unwrap();
    }

    #[test]
    fn park_transitions_ready_human_task_to_waiting_human_input() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));

        let parked = park_ready_human_tasks(&mut graph, dir);

        assert_eq!(parked.len(), 1, "one human task should be parked");
        assert_eq!(parked[0].task_id, "groceries");
        let t = graph.get_task("groceries").unwrap();
        assert_eq!(t.status, Status::Waiting);
        assert_eq!(
            t.wait_condition,
            Some(WaitSpec::All(vec![WaitCondition::HumanInput]))
        );
        assert!(
            t.log.iter().any(|l| l.message.contains("Agent parked")),
            "park log marker present so wait_started resolves"
        );
    }

    #[test]
    fn park_ignores_ai_assigned_and_unassigned_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("ai-task", Some("agent-abc"))));
        graph.add_node(Node::Task(ready_task("free-task", None)));

        let parked = park_ready_human_tasks(&mut graph, dir);

        assert!(parked.is_empty(), "no human tasks to park");
        assert_eq!(graph.get_task("ai-task").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("free-task").unwrap().status, Status::Open);
    }

    #[test]
    fn human_reply_completes_task_and_writes_reply_to_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");

        let mut graph = WorkGraph::new();
        let mut task = ready_task("groceries", Some("human-nadin"));
        task.deliverables = vec!["shopping-list.txt".to_string()];
        graph.add_node(Node::Task(task));

        // Park it, then capture the wait_started timestamp from the park log.
        let parked = park_ready_human_tasks(&mut graph, dir);
        assert_eq!(parked.len(), 1);
        let wait_started = graph
            .get_task("groceries")
            .unwrap()
            .log
            .iter()
            .rev()
            .find(|l| l.message.contains("Agent parked"))
            .map(|l| l.timestamp.clone());

        // Human replies via a non-agent message.
        messages::send_message(dir, "groceries", "eggs, milk, bread", "nadin", "normal").unwrap();

        let handled =
            try_complete_human_task_on_reply(&mut graph, dir, "groceries", wait_started.as_deref());

        assert!(
            handled,
            "human task reply should be handled here, not by generic resume"
        );
        let t = graph.get_task("groceries").unwrap();
        assert_eq!(t.status, Status::Done);
        assert!(t.wait_condition.is_none());
        assert!(
            t.artifacts.contains(&"shopping-list.txt".to_string()),
            "declared deliverable recorded as artifact"
        );
        // reply-to-artifact write landed at repo root (dir's parent).
        let written = std::fs::read_to_string(dir.parent().unwrap().join("shopping-list.txt"))
            .expect("artifact file written");
        assert_eq!(written, "eggs, milk, bread");
        assert!(
            t.log
                .iter()
                .any(|l| l.message.contains("Human reply received")),
            "completion log records the reply"
        );
    }

    #[test]
    fn route_inbound_reply_records_message_on_parked_human_task() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        // Nadin was onboarded by @handle (#49): the confirmed binding is keyed
        // on the handle "nadin", NOT a numeric id. DISTINCT id vs handle values
        // below so a value-equality lookup cannot masquerade as correct.
        write_binding(dir, "nadin", "human-nadin", "Nadin", true);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // Live-shaped inbound: a shared bot delivers the reply as the canonical
        // numeric `from.id` "78901234" (which never equals the stored handle)
        // plus `from.username` "nadin". Equality-only `find_by_user(sender)`
        // would reject this; `matches_sender` routes it on the username.
        let routed = route_inbound_reply(
            dir,
            "telegram",
            "78901234",
            Some("nadin"),
            "eggs, milk, bread",
        );

        assert_eq!(
            routed,
            InboundReplyOutcome::Recorded("groceries".to_string())
        );
        let msgs = messages::list_messages(dir, "groceries").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "eggs, milk, bread");
        assert_eq!(msgs[0].sender, "78901234");
        // The recorded message is a non-agent message, so it satisfies
        // WaitCondition::HumanInput on the next coordinator tick.
        assert!(!msgs[0].sender.starts_with("agent-"));
    }

    #[test]
    fn route_inbound_reply_rejects_spoofed_sender() {
        // A sender with NO binding must not be recorded as the human's reply,
        // even though a matching parked task exists (the shared-bot spoof Erik
        // flagged: any sender visible to the bot could answer for the human).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        write_binding(dir, "nadin", "human-nadin", "Nadin", true);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // Live-shaped spoof: a different person arrives with numeric id "424242"
        // and username "mallory" — neither matches Nadin's handle binding.
        let routed = route_inbound_reply(
            dir,
            "telegram",
            "424242",
            Some("mallory"),
            "eggs, milk, bread",
        );

        match routed {
            InboundReplyOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("424242"),
                    "reason names the sender: {reason}"
                );
            }
            other => panic!("spoofed sender must be Rejected, got {other:?}"),
        }
        // Crucially, nothing was recorded on the task.
        let msgs = messages::list_messages(dir, "groceries").unwrap_or_default();
        assert!(
            msgs.is_empty(),
            "no message may be recorded for a spoofed sender"
        );
    }

    #[test]
    fn route_inbound_reply_rejects_wrong_task_for_confirmed_sender() {
        // Erik's mismatch case: a sender confirmed for human A must not answer
        // human B's parked task, even on a shared bot picking the freshest ask.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        write_human_agent(dir, "human-erik", "Erik");
        // Only Erik is confirmed here.
        write_binding(dir, "erik", "human-erik", "Erik", true);

        let mut graph = WorkGraph::new();
        // The only parked task is Nadin's — Erik has none.
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // Erik (confirmed via his handle) replies from his numeric id "700100",
        // but nothing is his to answer.
        let routed =
            route_inbound_reply(dir, "telegram", "700100", Some("erik"), "eggs, milk, bread");

        assert_eq!(
            routed,
            InboundReplyOutcome::NoWaitingTask,
            "confirmed sender with no parked task of their own must not land on another human's task"
        );
        let msgs = messages::list_messages(dir, "groceries").unwrap_or_default();
        assert!(
            msgs.is_empty(),
            "Nadin's task must not receive Erik's reply"
        );
    }

    #[test]
    fn route_inbound_reply_confirmed_sender_lands_only_on_own_task() {
        // Two humans, each with a parked task; a shared bot. The confirmed
        // sender's reply must land on THEIR task, not the freshest across all.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        write_human_agent(dir, "human-erik", "Erik");
        write_binding(dir, "nadin", "human-nadin", "Nadin", true);
        write_binding(dir, "erik", "human-erik", "Erik", true);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task(
            "nadin-groceries",
            Some("human-nadin"),
        )));
        graph.add_node(Node::Task(ready_task("erik-repairs", Some("human-erik"))));
        park_and_persist(&mut graph, dir);

        // Erik's numeric id "700100" + username "erik" resolves to his handle
        // binding; the reply must land on his task, not the freshest across all.
        let routed = route_inbound_reply(dir, "telegram", "700100", Some("erik"), "fixed the sink");

        assert_eq!(
            routed,
            InboundReplyOutcome::Recorded("erik-repairs".to_string())
        );
        // Erik's reply landed on Erik's task only.
        assert_eq!(
            messages::list_messages(dir, "erik-repairs").unwrap().len(),
            1
        );
        assert!(
            messages::list_messages(dir, "nadin-groceries")
                .unwrap_or_default()
                .is_empty(),
            "Nadin's task is untouched by Erik's reply"
        );
    }

    #[test]
    fn route_inbound_reply_rejects_unconfirmed_binding() {
        // A bound but UNCONFIRMED sender (never completed the YES handshake)
        // must be rejected — a pending onboarding is not authorization.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        write_binding(dir, "nadin", "human-nadin", "Nadin", false);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // Numeric id "78901234" + username "nadin" resolves to Nadin's handle
        // binding, but it never completed the YES handshake.
        let routed = route_inbound_reply(
            dir,
            "telegram",
            "78901234",
            Some("nadin"),
            "eggs, milk, bread",
        );

        match routed {
            InboundReplyOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("confirm"),
                    "reason cites the missing confirmation: {reason}"
                );
            }
            other => panic!("unconfirmed sender must be Rejected, got {other:?}"),
        }
        assert!(
            messages::list_messages(dir, "groceries")
                .unwrap_or_default()
                .is_empty()
        );
    }

    /// Write an AI persona agent (native executor ⇒ `is_human()` is false) that
    /// a per-agent Telegram bot fronts (e.g. "otto"). `id` may be a hash-shaped
    /// agent id; `name` is the persona display name.
    fn write_persona_agent(dir: &Path, id: &str, name: &str) {
        let agents_dir = dir.join("agency").join("cache/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let agent = Agent {
            id: id.to_string(),
            role_id: "concierge".to_string(),
            tradeoff_id: "default".to_string(),
            name: name.to_string(),
            performance: PerformanceRecord::default(),
            lineage: Default::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: Default::default(),
            contact: None,
            executor: "native".to_string(),
            preferred_model: None,
            preferred_provider: None,
            deployment_history: vec![],
            attractor_weight: 0.5,
            staleness_flags: vec![],
        };
        agency::save_agent(&agent, &agents_dir).unwrap();
    }

    /// Write a `notify.toml` next to the graph with a single per-agent bot that
    /// fronts `agent_id`, so `bound_agent_for_channel(dir, "telegram:<bot>", …)`
    /// resolves to that agent.
    fn write_persona_bot(dir: &Path, bot: &str, agent_id: &str) {
        std::fs::write(
            dir.join("notify.toml"),
            format!(
                "[telegram.bots.{bot}]\n\
                 bot_token = \"111:AAA\"\n\
                 chat_id = \"-100777\"\n\
                 agent_id = \"{agent_id}\"\n"
            ),
        )
        .unwrap();
    }

    /// The misleading-noise fix. A confirmed human's ordinary group message that
    /// arrives on a bot fronting an AI PERSONA (not a parked-task reply) must
    /// resolve to the benign `NotParkedReply` carrying the persona DISPLAY NAME
    /// — never a security `Rejected`, and never the raw agent id/hash. This is
    /// the fall-through the listener logs neutrally instead of "Rejected reply".
    #[test]
    fn route_inbound_reply_persona_bot_is_benign_not_parked_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");
        // A hash-shaped persona id proves the outcome carries the NAME, not id.
        write_persona_agent(dir, "agent-a1b2c3d4", "Otto");
        write_binding(dir, "luca-1", "human-luca", "Luca", true);
        write_persona_bot(dir, "otto", "agent-a1b2c3d4");

        // No parked task; persist an empty graph the router loads from disk.
        worksgood::parser::save_graph(&WorkGraph::new(), crate::commands::graph_path(dir)).unwrap();

        let routed =
            route_inbound_reply(dir, "telegram:otto", "luca-1", None, "morning everyone!");

        match routed {
            InboundReplyOutcome::NotParkedReply { persona } => {
                assert_eq!(persona, "Otto", "carries the persona display name");
                assert!(
                    !persona.contains("agent-"),
                    "must never surface the raw agent hash: {persona}"
                );
            }
            other => panic!("persona-bot message must be benign NotParkedReply, got {other:?}"),
        }
    }

    /// Do NOT weaken the genuine mis-route path: a confirmed sender for human A
    /// arriving on a bot dedicated to a DIFFERENT HUMAN B is still a security
    /// `Rejected`, logged clearly with both names.
    #[test]
    fn route_inbound_reply_wrong_human_bot_still_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");
        write_human_agent(dir, "human-nadin", "Nadin");
        write_binding(dir, "luca-1", "human-luca", "Luca", true);
        // A per-human bot dedicated to Nadin, not Luca.
        write_persona_bot(dir, "nadin", "human-nadin");

        worksgood::parser::save_graph(&WorkGraph::new(), crate::commands::graph_path(dir)).unwrap();

        let routed =
            route_inbound_reply(dir, "telegram:nadin", "luca-1", None, "answer for nadin");

        match routed {
            InboundReplyOutcome::Rejected(reason) => {
                assert!(
                    reason.contains("human-luca") && reason.contains("human-nadin"),
                    "genuine mis-route names both humans clearly: {reason}"
                );
            }
            other => panic!("a bot fronting a DIFFERENT human must stay Rejected, got {other:?}"),
        }
    }

    #[test]
    fn route_inbound_reply_handle_binding_rejects_wrong_username() {
        // Live-shaped wrong-human via the handle path (#49 parity): a confirmed
        // HANDLE binding must reject an inbound whose username is someone else,
        // even though the numeric id is equally unknown. Guards against a router
        // that mistakenly matched on the numeric id or ignored the username.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        // Nadin onboarded by handle "nadin"; confirmed.
        write_binding(dir, "nadin", "human-nadin", "Nadin", true);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // A different person: numeric id "555000" + username "mallory".
        let routed = route_inbound_reply(
            dir,
            "telegram",
            "555000",
            Some("mallory"),
            "eggs, milk, bread",
        );

        match routed {
            InboundReplyOutcome::Rejected(_) => {}
            other => panic!("wrong username on a handle binding must be Rejected, got {other:?}"),
        }
        assert!(
            messages::list_messages(dir, "groceries")
                .unwrap_or_default()
                .is_empty(),
            "Nadin's task must not receive a reply from the wrong username"
        );
    }

    #[test]
    fn route_inbound_reply_numeric_binding_matches_on_id_ignoring_username() {
        // The other half of the `matches_sender` contract: a binding keyed on a
        // NUMERIC id authorizes on the stable `from.id` and is indifferent to
        // the mutable `from.username`. DISTINCT id vs username values so the
        // test cannot pass by accidental string equality.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");
        // Onboarded by stable numeric id, confirmed.
        write_binding(dir, "78901234", "human-nadin", "Nadin", true);

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        park_and_persist(&mut graph, dir);

        // Inbound carries the matching numeric id but a since-changed username;
        // the numeric binding must still authorize on the id alone.
        let routed = route_inbound_reply(
            dir,
            "telegram",
            "78901234",
            Some("nadin_new_handle"),
            "eggs, milk, bread",
        );

        assert_eq!(
            routed,
            InboundReplyOutcome::Recorded("groceries".to_string()),
            "a numeric binding must match the stable id regardless of username"
        );
    }

    #[test]
    fn non_human_task_is_left_for_generic_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-nadin", "Nadin");

        let mut graph = WorkGraph::new();
        let mut task = ready_task("build", Some("agent-xyz"));
        task.status = Status::Waiting;
        task.wait_condition = Some(WaitSpec::All(vec![WaitCondition::HumanInput]));
        graph.add_node(Node::Task(task));

        let handled = try_complete_human_task_on_reply(&mut graph, dir, "build", None);

        assert!(
            !handled,
            "AI-assigned task must fall through to generic resume"
        );
        assert_eq!(graph.get_task("build").unwrap().status, Status::Waiting);
    }

    // ----- R18: generic inline-button -> task routing -----------------------

    fn confirm_task(id: &str, agent: Option<&str>) -> Task {
        let mut t = ready_task(id, agent);
        t.choices = TaskChoice::confirmation_pair();
        t
    }

    #[test]
    fn button_token_round_trips() {
        let tok = button_token("2026-w29-plan", "looks_good");
        assert_eq!(tok, "2026-w29-plan#looks_good");
        assert_eq!(
            parse_button_token(&tok),
            Some(("2026-w29-plan", "looks_good"))
        );
    }

    #[test]
    fn parse_button_token_rejects_legacy_and_malformed() {
        // Legacy `<verb>:<task>` carries no `#`, so the caller falls back.
        assert_eq!(parse_button_token("approve:my-task"), None);
        assert_eq!(parse_button_token("no-separator"), None);
        // Empty task or empty key are both rejected.
        assert_eq!(parse_button_token("#key"), None);
        assert_eq!(parse_button_token("task#"), None);
        // Only the FIRST `#` splits, so a key may itself contain `#`.
        assert_eq!(parse_button_token("t#a#b"), Some(("t", "a#b")));
    }

    #[test]
    fn slugify_choice_key_is_short_and_stable() {
        assert_eq!(worksgood::graph::slugify_choice_key("Looks good"), "looks_good");
        assert_eq!(worksgood::graph::slugify_choice_key("Change something!"), "change_something");
        assert_eq!(worksgood::graph::slugify_choice_key("  Yes / No  "), "yes_no");
        // TaskChoice::new derives the key when one isn't supplied.
        assert_eq!(TaskChoice::new("", "Change something").key, "change_something");
    }

    #[test]
    fn button_actions_use_generic_tokens_and_style() {
        let choices = TaskChoice::confirmation_pair();
        let actions = button_actions("plan-review", &choices);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].id, "plan-review#looks_good");
        assert_eq!(actions[0].label, "Looks good");
        assert_eq!(actions[0].style, ActionStyle::Primary);
        assert_eq!(actions[1].id, "plan-review#change_something");
        assert_eq!(actions[1].style, ActionStyle::Secondary);
        // No choices -> no buttons -> caller sends a plain text prompt.
        assert!(button_actions("plan-review", &[]).is_empty());
    }

    #[test]
    fn park_carries_choices_and_message_prompts_for_a_tap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(confirm_task("plan-review", Some("human-luca"))));

        let parked = park_ready_human_tasks(&mut graph, dir);
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].choices, TaskChoice::confirmation_pair());

        let msg = format_human_task_message(&parked[0]);
        assert!(
            msg.contains("Tap a button"),
            "choice tasks prompt for a tap, not a free-text reply: {msg}"
        );
        assert!(!msg.contains("Reply to this message"));
    }

    #[test]
    fn button_callback_routes_to_originating_task_and_records_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(confirm_task("plan-review", Some("human-luca"))));
        park_ready_human_tasks(&mut graph, dir);
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        // Luca taps [Looks good]; the callback token routes to THIS task.
        let routed = route_button_callback(dir, "plan-review#looks_good", "lucapinello");
        assert_eq!(
            routed,
            Some(("plan-review".to_string(), "Looks good".to_string()))
        );

        // The chosen option's LABEL (not the key) is recorded as the reply, as a
        // non-agent message that satisfies WaitCondition::HumanInput.
        let msgs = messages::list_messages(dir, "plan-review").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "Looks good");
        assert_eq!(msgs[0].sender, "lucapinello");
        assert!(!msgs[0].sender.starts_with("agent-"));
    }

    #[test]
    fn button_callback_rejects_unknown_task_and_stale_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(confirm_task("plan-review", Some("human-luca"))));
        park_ready_human_tasks(&mut graph, dir);
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        // Unknown task id -> None, nothing recorded.
        assert_eq!(route_button_callback(dir, "ghost#looks_good", "lucapinello"), None);
        // Known task but a key it never declared (stale button) -> None.
        assert_eq!(
            route_button_callback(dir, "plan-review#delete_everything", "lucapinello"),
            None
        );
        // Legacy `<verb>:<task>` (no `#`) is not a button token here.
        assert_eq!(route_button_callback(dir, "approve:plan-review", "lucapinello"), None);
        assert!(messages::list_messages(dir, "plan-review").unwrap().is_empty());
    }

    #[test]
    fn button_tap_completes_task_and_writes_choice_as_reply_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");

        let mut graph = WorkGraph::new();
        let mut task = confirm_task("plan-review", Some("human-luca"));
        task.deliverables = vec!["plan-decision.txt".to_string()];
        graph.add_node(Node::Task(task));

        park_ready_human_tasks(&mut graph, dir);
        let wait_started = graph
            .get_task("plan-review")
            .unwrap()
            .log
            .iter()
            .rev()
            .find(|l| l.message.contains("Agent parked"))
            .map(|l| l.timestamp.clone());
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        // Tap records the chosen label as the reply...
        let routed = route_button_callback(dir, "plan-review#change_something", "lucapinello");
        assert_eq!(
            routed,
            Some(("plan-review".to_string(), "Change something".to_string()))
        );

        // ...then the coordinator's completion pass writes it as reply-artifact.
        let handled = try_complete_human_task_on_reply(
            &mut graph,
            dir,
            "plan-review",
            wait_started.as_deref(),
        );
        assert!(handled);
        let t = graph.get_task("plan-review").unwrap();
        assert_eq!(t.status, Status::Done);
        assert!(t.artifacts.contains(&"plan-decision.txt".to_string()));
        let written = std::fs::read_to_string(dir.parent().unwrap().join("plan-decision.txt"))
            .expect("reply-artifact written");
        assert_eq!(
            written, "Change something",
            "the tapped choice's label is recorded as the reply-to-artifact"
        );
    }

    #[test]
    fn choices_survive_graph_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(confirm_task("plan-review", Some("human-luca"))));
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        let reloaded =
            worksgood::parser::load_graph(&crate::commands::graph_path(dir)).unwrap();
        assert_eq!(
            reloaded.get_task("plan-review").unwrap().choices,
            TaskChoice::confirmation_pair(),
            "declared choices persist across serialize/deserialize"
        );
    }
}
