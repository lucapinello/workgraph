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

use worksgood::agency::{self, Agent};
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

/// Route an inbound human reply (delivered by a notification listener) onto the
/// awaiting-human task it answers, recording it as a message (R13). This is the
/// "awaiting-human task router" that `src/notify/telegram.rs` deferred: the
/// listener tags each inbound message with the receiving bot's channel type, and
/// this maps that back to the human agent — and thus the parked task — the reply
/// belongs to. Returns the task id the reply was recorded on, if one was found.
///
/// Recording the message is exactly what satisfies the task's
/// `WaitCondition::HumanInput`, so the coordinator's next tick completes the
/// task via [`try_complete_human_task_on_reply`].
pub fn route_inbound_reply(
    dir: &Path,
    channel_type: &str,
    sender: &str,
    body: &str,
) -> Option<String> {
    let graph = worksgood::parser::load_graph(&crate::commands::graph_path(dir)).ok()?;
    let agents_dir = dir.join("agency").join("cache/agents");
    let agents = agency::load_all_agents_or_warn(&agents_dir);

    let human_ids: HashSet<&str> = agents
        .iter()
        .filter(|a| a.is_human())
        .map(|a| a.id.as_str())
        .collect();
    if human_ids.is_empty() {
        return None;
    }

    // If this bot fronts a specific agent, the reply is theirs; otherwise a
    // shared bot's reply may answer any human's parked task.
    let bound_agent = bound_agent_for_channel(dir, channel_type, &agents);

    let mut candidates: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.status == Status::Waiting)
        .filter(|t| waits_on_human_input(t))
        .filter(|t| {
            t.agent
                .as_deref()
                .map(|a| human_ids.contains(a))
                .unwrap_or(false)
        })
        .collect();

    if let Some(ref agent_id) = bound_agent {
        candidates.retain(|t| t.agent.as_deref() == Some(agent_id.as_str()));
    }

    // Land the reply on the freshest open ask (newest park time wins).
    let target = candidates.into_iter().max_by_key(|t| park_time(t))?;
    let task_id = target.id.clone();

    messages::send_message(dir, &task_id, body, sender, "normal").ok()?;
    Some(task_id)
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

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(ready_task("groceries", Some("human-nadin"))));
        // Park it so it is Waiting on HumanInput, then persist for the router
        // (which loads the graph from disk).
        park_ready_human_tasks(&mut graph, dir);
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        // A shared bot (no notify config → no agent binding) delivers a reply.
        let routed = route_inbound_reply(dir, "telegram", "nadin", "eggs, milk, bread");

        assert_eq!(routed.as_deref(), Some("groceries"));
        let msgs = messages::list_messages(dir, "groceries").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "eggs, milk, bread");
        assert_eq!(msgs[0].sender, "nadin");
        // The recorded message is a non-agent message, so it satisfies
        // WaitCondition::HumanInput on the next coordinator tick.
        assert!(!msgs[0].sender.starts_with("agent-"));
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
