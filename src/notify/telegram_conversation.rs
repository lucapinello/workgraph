//! Conversational reply composer — the ONE path for a plain message that
//! routes to a single agent, shared by BOTH entry points:
//!
//! 1. a **1:1** message the human sent directly to a bot, and
//! 2. a **name/mention/reply-elected group** message.
//!
//! ## The bug this closes
//!
//! Before this module, only *collective* addresses (`Election::All` → the
//! roster composer) and `/commands` (`telegram_family_commands`) had reply
//! composers. A plain message that resolved to ONE agent — whether from a 1:1
//! (`Election::Private`) or a group election (`Election::One`) — fell through
//! the command/button checks into `classify_inbound_message`, and when it was
//! neither an onboarding `YES` nor a reply to a parked task it landed in
//! `InboundOutcome::Unmatched`, which only *logged* and stayed silent. Luca hit
//! this dead-end twice live: a 1:1 to Otto, then "otto…" in the group. Both
//! entry points share the same dead-end, so they share the same fix here.
//!
//! ## The fix
//!
//! A plain message from a **confirmed** human that routes to a single agent
//! becomes a **chat turn against that agent's persistent session** (the R2
//! sessions-as-identity binding — `chat_sessions::session_for_agent`): the
//! human's text is written to the session inbox and the session's reply is read
//! back from the outbox and sent to the chat the human wrote in, via the bot
//! they addressed. This reuses the same inbox/outbox surface the office chat
//! uses (`executor::native::chat_surface`) rather than duplicating it.
//!
//! Guarantees:
//! - **Reply where asked** — 1:1 replies in the 1:1 via that bot; a
//!   group-elected message replies in the group via the elected bot
//!   ([`ReplyRoute`]).
//! - **Latency ack** — if the session turn takes longer than a configurable
//!   threshold, a lightweight ack ("On it — one sec…") is sent immediately in
//!   the same chat so silence never happens ([`AckTiming`]).
//! - **Precedence preserved** — this path is only ever reached from the
//!   `Unmatched` arm, *after* onboarding-confirmation and awaiting-human-task
//!   routing have had their say, so task replies still win over conversation.
//! - **Unknown senders** — a sender with no confirmed binding gets a single
//!   polite onboarding one-liner, nothing more ([`ConversationPlan::Onboard`]).
//! - **No tokens or secrets** are ever logged; the bot token lives only on the
//!   send channel.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::agency::TelegramBindingMap;
use crate::chat;
use crate::chat_sessions;

use super::NotificationChannel;
use super::telegram::{TelegramChannel, TelegramConfig};
use super::telegram_group::CONCIERGE_BOT;

/// Which entry point produced a conversational message. Carried for logging so
/// every handled inbound records *how* it was addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// A 1:1 message the human DMed a bot (`Election::Private`).
    Direct,
    /// A name/mention/reply-elected group message (`Election::One`).
    GroupElected,
}

impl Entry {
    pub fn label(self) -> &'static str {
        match self {
            Entry::Direct => "1:1",
            Entry::GroupElected => "group-elected",
        }
    }
}

/// The bot that answers and the chat it answers in. "Reply where asked" is
/// entirely captured here: for a 1:1 it is the messaged bot + the 1:1 chat; for
/// a group election it is the elected bot + the group chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRoute {
    pub bot_id: String,
    pub chat_id: String,
}

/// What to do with a plain message that routed to a single agent and matched
/// neither an onboarding confirmation nor an awaiting-human task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationPlan {
    /// Confirmed human addressing an agent that has a bound persistent session:
    /// run a chat turn against `session_ref`, reply via `route`.
    Converse {
        session_ref: String,
        agent_id: String,
        route: ReplyRoute,
        entry: Entry,
    },
    /// Confirmed human, but the elected agent has no bound session yet. We still
    /// answer — never silence — with a lightweight in-voice line.
    Sessionless {
        agent_id: String,
        route: ReplyRoute,
        entry: Entry,
    },
    /// Unknown sender (no confirmed binding): one polite onboarding line.
    Onboard { route: ReplyRoute, entry: Entry },
}

impl ConversationPlan {
    pub fn route(&self) -> &ReplyRoute {
        match self {
            ConversationPlan::Converse { route, .. }
            | ConversationPlan::Sessionless { route, .. }
            | ConversationPlan::Onboard { route, .. } => route,
        }
    }

    pub fn entry(&self) -> Entry {
        match self {
            ConversationPlan::Converse { entry, .. }
            | ConversationPlan::Sessionless { entry, .. }
            | ConversationPlan::Onboard { entry, .. } => *entry,
        }
    }

    /// Short kind label for the route-decision log line.
    pub fn kind_label(&self) -> &'static str {
        match self {
            ConversationPlan::Converse { .. } => "converse",
            ConversationPlan::Sessionless { .. } => "sessionless",
            ConversationPlan::Onboard { .. } => "onboard",
        }
    }
}

/// The result of handling one conversational turn — logged (never a token) so
/// every handled inbound records its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The session replied and we relayed it. `acked` is true when we also sent
    /// the latency ack because the turn ran long.
    Replied { acked: bool },
    /// The session never replied within the timeout. `acked` records whether the
    /// human at least got the "on it" ack (so it was not pure silence).
    TimedOut { acked: bool },
    /// An unknown sender got the onboarding one-liner.
    Onboarded,
    /// A confirmed human addressed an agent with no bound session; sent the
    /// graceful fallback line.
    Sessionless,
}

impl TurnOutcome {
    pub fn label(&self) -> String {
        match self {
            TurnOutcome::Replied { acked: false } => "replied".to_string(),
            TurnOutcome::Replied { acked: true } => "replied (after ack)".to_string(),
            TurnOutcome::TimedOut { acked } => {
                format!("timed-out (acked={acked})")
            }
            TurnOutcome::Onboarded => "onboarded".to_string(),
            TurnOutcome::Sessionless => "sessionless-fallback".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Family-voice copy (no jargon, no task ids — humans read these)
// ---------------------------------------------------------------------------

/// The immediate ack sent when a session turn runs longer than the threshold,
/// so the human never stares at silence.
pub fn ack_line() -> String {
    "On it — one sec… \u{23f3}".to_string()
}

/// One polite line for a sender we don't recognise. Deliberately warm and
/// jargon-free — no command syntax, no ids.
pub fn onboarding_line() -> String {
    "Hi there! \u{1f44b} I don't recognise you yet, so I can't chat just now. Ask Luca to add \
     you to the family and I'll be right with you."
        .to_string()
}

/// Graceful fallback when the addressed agent has no bound session yet.
pub fn sessionless_line() -> String {
    "I'm here! \u{1f642} Give me a little while to get settled and I'll be able to help properly."
        .to_string()
}

// ---------------------------------------------------------------------------
// Pure planning
// ---------------------------------------------------------------------------

/// Resolve the bot id that should answer, from an elected/receiving
/// `channel_type` ("telegram" or "telegram:<bot_id>").
///
/// The bare/`default`/legacy channel maps to the concierge ([`CONCIERGE_BOT`])
/// when configured, else the first bot. A named channel maps to that bot,
/// falling back to the first configured bot so a reply always has a sender.
pub fn bot_id_for_channel(config: &TelegramConfig, channel_type: &str) -> Option<String> {
    let stripped = channel_type
        .strip_prefix("telegram:")
        .unwrap_or(channel_type);
    let bots = config.all_bots();
    if bots.is_empty() {
        return None;
    }
    if stripped.is_empty() || stripped == "telegram" || stripped == "default" {
        return bots
            .iter()
            .find(|(id, _)| id == CONCIERGE_BOT)
            .or_else(|| bots.first())
            .map(|(id, _)| id.clone());
    }
    Some(
        bots.iter()
            .find(|(id, _)| id == stripped)
            .or_else(|| bots.first())
            .map(|(id, _)| id.clone())
            .unwrap(),
    )
}

/// The agency agent a bot fronts (its `agent_id`), falling back to the bot id
/// itself when no explicit binding is configured.
fn agent_for_bot(config: &TelegramConfig, bot_id: &str) -> String {
    config
        .all_bots()
        .iter()
        .find(|(id, _)| id == bot_id)
        .and_then(|(_, b)| b.agent_id.clone())
        .unwrap_or_else(|| bot_id.to_string())
}

/// The agency agent addressed by an elected/receiving `channel_type` — the bot
/// it resolves to, then that bot's `agent_id`. Exposed for the `wg telegram
/// conversation` dry-run, which pre-binds a fixture session to this agent.
pub fn agent_for_channel(config: &TelegramConfig, channel_type: &str) -> Option<String> {
    let bot_id = bot_id_for_channel(config, channel_type)?;
    Some(agent_for_bot(config, &bot_id))
}

/// Is this Telegram sender a *confirmed* human? Unknown or unconfirmed senders
/// are not eligible for conversation — they get the onboarding line. A missing
/// binding file (first-ever onboard) reads as "not confirmed".
fn sender_is_confirmed(workgraph_dir: &Path, sender: &str) -> bool {
    let agency_dir = workgraph_dir.join("agency");
    match TelegramBindingMap::load(&agency_dir) {
        Ok(map) => map
            .find_by_user(sender)
            .map(|b| b.confirmed)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Decide what to do with a plain message that routed to a single agent.
///
/// Pure and filesystem-only (binding map + session registry) — no network — so
/// it is unit-testable without a live bot.
///
/// * `route_channel` — the elected/receiving bot's `channel_type`.
/// * `reply_chat` — the chat to answer in (the group for a group election, the
///   1:1 for a direct message).
/// * `sender` — the Telegram user id of the human.
pub fn plan_conversation(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    route_channel: &str,
    reply_chat: &str,
    sender: &str,
    entry: Entry,
) -> ConversationPlan {
    let bot_id = bot_id_for_channel(config, route_channel).unwrap_or_else(|| {
        route_channel
            .strip_prefix("telegram:")
            .unwrap_or(route_channel)
            .to_string()
    });
    let route = ReplyRoute {
        bot_id: bot_id.clone(),
        chat_id: reply_chat.to_string(),
    };

    if !sender_is_confirmed(workgraph_dir, sender) {
        return ConversationPlan::Onboard { route, entry };
    }

    let agent_id = agent_for_bot(config, &bot_id);
    match chat_sessions::session_for_agent(workgraph_dir, &agent_id) {
        Some(session_ref) => ConversationPlan::Converse {
            session_ref,
            agent_id,
            route,
            entry,
        },
        None => ConversationPlan::Sessionless {
            agent_id,
            route,
            entry,
        },
    }
}

// ---------------------------------------------------------------------------
// Latency-ack timing
// ---------------------------------------------------------------------------

/// Timing for the session round-trip: when to ack, when to give up, and how
/// often to poll the outbox.
#[derive(Debug, Clone, Copy)]
pub struct AckTiming {
    /// Send the lightweight ack if no reply has arrived within this long.
    pub ack_after: Duration,
    /// Stop waiting for the session reply after this long.
    pub reply_timeout: Duration,
    /// How often to re-scan the outbox.
    pub poll: Duration,
}

impl Default for AckTiming {
    fn default() -> Self {
        Self {
            ack_after: Duration::from_secs(4),
            reply_timeout: Duration::from_secs(120),
            poll: Duration::from_millis(400),
        }
    }
}

impl AckTiming {
    /// Load timing from the environment, falling back to [`Default`]. The ack
    /// threshold and reply timeout are configurable so operators can tune them
    /// without a rebuild.
    pub fn from_env() -> Self {
        let d = Self::default();
        let secs = |key: &str, fallback: Duration| -> Duration {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(fallback)
        };
        Self {
            ack_after: secs("WG_TELEGRAM_ACK_AFTER_SECS", d.ack_after),
            reply_timeout: secs("WG_TELEGRAM_REPLY_TIMEOUT_SECS", d.reply_timeout),
            poll: d.poll,
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound sink (network seam — mockable in tests)
// ---------------------------------------------------------------------------

/// Where conversational replies go out. The real implementation sends via the
/// addressed bot's Telegram channel; tests substitute a recorder so the
/// round-trip can be asserted without a live bot.
#[async_trait]
pub trait ReplySink: Send + Sync {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<()>;
}

/// Production sink: resolves `bot_id` against the config and sends via that
/// bot's [`TelegramChannel`], falling back to the first configured bot so a
/// reply always goes out. The token lives only on the channel and is never
/// logged.
pub struct BotReplySink {
    config: TelegramConfig,
}

impl BotReplySink {
    pub fn new(config: TelegramConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ReplySink for BotReplySink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<()> {
        let bots = self.config.all_bots();
        let (id, bot) = bots
            .iter()
            .find(|(id, _)| id == bot_id)
            .or_else(|| bots.first())
            .ok_or_else(|| anyhow::anyhow!("no Telegram bots configured — cannot reply"))?;
        let channel = TelegramChannel::from_bot(id.clone(), bot.clone());
        channel.send_text(chat_id, text).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Turn orchestration
// ---------------------------------------------------------------------------

/// The max outbox message id currently present for a session — the baseline so
/// that only replies produced *after* our turn are relayed.
fn outbox_baseline(workgraph_dir: &Path, session_ref: &str) -> u64 {
    chat::read_outbox_since_ref(workgraph_dir, session_ref, 0)
        .map(|msgs| msgs.iter().map(|m| m.id).max().unwrap_or(0))
        .unwrap_or(0)
}

/// Read the session's reply to our turn, if one has landed past `baseline`.
///
/// Prefers a message whose `request_id` matches our turn; otherwise the first
/// new outbox message (a session is single-threaded, so any new outbox entry
/// past the baseline is a response to our turn).
fn read_new_reply(
    workgraph_dir: &Path,
    session_ref: &str,
    baseline: u64,
    request_id: &str,
) -> Result<Option<String>> {
    let msgs = chat::read_outbox_since_ref(workgraph_dir, session_ref, baseline)?;
    if let Some(m) = msgs.iter().find(|m| m.request_id == request_id) {
        return Ok(Some(m.content.clone()));
    }
    Ok(msgs.into_iter().next().map(|m| m.content))
}

/// Run a single conversational turn per `plan`, sending via `sink`.
///
/// For [`ConversationPlan::Converse`] this writes the human's message to the
/// bound session inbox and polls the outbox for the reply, emitting the latency
/// ack if the turn runs past `timing.ack_after`. For the other variants it
/// sends the corresponding one-liner. Returns the [`TurnOutcome`] for logging.
pub async fn run_conversation_turn(
    workgraph_dir: &Path,
    plan: &ConversationPlan,
    human_message: &str,
    request_id: &str,
    timing: AckTiming,
    sink: &dyn ReplySink,
) -> Result<TurnOutcome> {
    match plan {
        ConversationPlan::Onboard { route, .. } => {
            sink.send(&route.bot_id, &route.chat_id, &onboarding_line())
                .await?;
            Ok(TurnOutcome::Onboarded)
        }
        ConversationPlan::Sessionless { route, .. } => {
            sink.send(&route.bot_id, &route.chat_id, &sessionless_line())
                .await?;
            Ok(TurnOutcome::Sessionless)
        }
        ConversationPlan::Converse {
            session_ref, route, ..
        } => {
            let baseline = outbox_baseline(workgraph_dir, session_ref);
            // Deliver the human's turn to the agent's persistent session.
            chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id)?;
            await_session_reply(
                workgraph_dir,
                session_ref,
                baseline,
                request_id,
                timing,
                route,
                sink,
            )
            .await
        }
    }
}

/// Poll the session outbox for the reply, sending the latency ack once if the
/// turn runs past `timing.ack_after`. Returns when the reply lands or the
/// timeout elapses.
#[allow(clippy::too_many_arguments)]
async fn await_session_reply(
    workgraph_dir: &Path,
    session_ref: &str,
    baseline: u64,
    request_id: &str,
    timing: AckTiming,
    route: &ReplyRoute,
    sink: &dyn ReplySink,
) -> Result<TurnOutcome> {
    let start = Instant::now();
    let mut acked = false;
    loop {
        if let Some(text) = read_new_reply(workgraph_dir, session_ref, baseline, request_id)? {
            sink.send(&route.bot_id, &route.chat_id, &text).await?;
            return Ok(TurnOutcome::Replied { acked });
        }
        let elapsed = start.elapsed();
        if !acked && elapsed >= timing.ack_after {
            // The turn is running long — break the silence immediately.
            sink.send(&route.bot_id, &route.chat_id, &ack_line()).await?;
            acked = true;
        }
        if elapsed >= timing.reply_timeout {
            return Ok(TurnOutcome::TimedOut { acked });
        }
        tokio::time::sleep(timing.poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_sessions::{SessionKind, bind_agent, create_session};
    use crate::notify::telegram::{TelegramBotConfig, TelegramConfig};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Recording sink: captures every (bot_id, chat_id, text) send so tests can
    /// assert *which bot* replied *in which chat* with *what text*.
    #[derive(Default)]
    struct RecSink {
        sent: Mutex<Vec<(String, String, String)>>,
    }
    #[async_trait]
    impl ReplySink for RecSink {
        async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((bot_id.to_string(), chat_id.to_string(), text.to_string()));
            Ok(())
        }
    }
    impl RecSink {
        fn calls(&self) -> Vec<(String, String, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    fn cfg_with_bots(bots: &[(&str, Option<&str>)]) -> TelegramConfig {
        let mut map = HashMap::new();
        for (id, agent) in bots {
            map.insert(
                id.to_string(),
                TelegramBotConfig {
                    bot_token: format!("token-{id}"),
                    chat_id: "100".to_string(),
                    agent_id: agent.map(|a| a.to_string()),
                    username: Some(format!("{id}_bot")),
                },
            );
        }
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: map,
        }
    }

    fn confirm_human(wg: &Path, sender: &str, agent_id: &str, bot_id: &str) {
        let agency_dir = wg.join("agency");
        let mut map = TelegramBindingMap::load(&agency_dir).unwrap();
        let mut b = crate::agency::TelegramBinding::new(
            sender,
            agent_id,
            "Luca",
            Some(bot_id.to_string()),
            Utc::now(),
        );
        b.confirmed = true;
        b.confirmed_at = Some(Utc::now());
        map.add(b).unwrap();
        map.save(&agency_dir).unwrap();
    }

    fn fast_timing() -> AckTiming {
        AckTiming {
            ack_after: Duration::from_millis(80),
            reply_timeout: Duration::from_millis(2000),
            poll: Duration::from_millis(20),
        }
    }

    #[test]
    fn bot_id_for_channel_resolves_named_and_default() {
        let cfg = cfg_with_bots(&[("otto", Some("otto")), ("bruno", Some("bruno"))]);
        assert_eq!(
            bot_id_for_channel(&cfg, "telegram:bruno").as_deref(),
            Some("bruno")
        );
        // Bare/legacy channel prefers the concierge.
        assert_eq!(
            bot_id_for_channel(&cfg, "telegram").as_deref(),
            Some("otto")
        );
    }

    #[test]
    fn unknown_sender_plans_onboard_nothing_more() {
        let dir = tempdir().unwrap();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let plan = plan_conversation(
            dir.path(),
            &cfg,
            "telegram:otto",
            "999",
            "stranger-42",
            Entry::Direct,
        );
        assert!(matches!(plan, ConversationPlan::Onboard { .. }));
        assert_eq!(plan.route().bot_id, "otto");
        assert_eq!(plan.route().chat_id, "999");
    }

    #[test]
    fn confirmed_human_direct_plans_converse_via_messaged_bot() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        // Bind a session to the otto agent and confirm the human.
        let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(wg, "otto", &uuid).unwrap();
        confirm_human(wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(
            wg,
            &cfg,
            "telegram:otto",
            "555", // the 1:1 chat
            "luca-1",
            Entry::Direct,
        );
        match plan {
            ConversationPlan::Converse {
                session_ref,
                route,
                entry,
                ..
            } => {
                assert_eq!(session_ref, uuid);
                assert_eq!(route.bot_id, "otto");
                assert_eq!(route.chat_id, "555");
                assert_eq!(entry, Entry::Direct);
            }
            other => panic!("expected Converse, got {other:?}"),
        }
    }

    #[test]
    fn confirmed_human_group_elected_plans_converse_via_elected_bot_in_group() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let cfg = cfg_with_bots(&[("otto", Some("otto")), ("bruno", Some("bruno"))]);
        let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(wg, "bruno", &uuid).unwrap();
        confirm_human(wg, "luca-1", "human-luca", "otto");

        // Elected bot is bruno; reply chat is the GROUP.
        let plan = plan_conversation(
            wg,
            &cfg,
            "telegram:bruno",
            "-1002000", // group chat id
            "luca-1",
            Entry::GroupElected,
        );
        match plan {
            ConversationPlan::Converse {
                session_ref,
                route,
                entry,
                ..
            } => {
                assert_eq!(session_ref, uuid);
                assert_eq!(route.bot_id, "bruno");
                assert_eq!(route.chat_id, "-1002000");
                assert_eq!(entry, Entry::GroupElected);
            }
            other => panic!("expected Converse, got {other:?}"),
        }
    }

    #[test]
    fn confirmed_human_no_session_plans_sessionless() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        confirm_human(wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        assert!(matches!(plan, ConversationPlan::Sessionless { .. }));
    }

    #[tokio::test]
    async fn direct_round_trip_relays_session_reply_to_the_1to1_via_messaged_bot() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();

        // The "persistent session" fixture: a task that reads the human's inbox
        // turn and writes an assistant reply to the outbox, just like a live nex
        // session would.
        let wg2 = wg.clone();
        let uuid2 = uuid.clone();
        let responder = tokio::spawn(async move {
            for _ in 0..100 {
                let inbox = chat::read_inbox_ref(&wg2, &uuid2).unwrap_or_default();
                if let Some(m) = inbox.iter().find(|m| m.role == "user") {
                    chat::append_outbox_ref(
                        &wg2,
                        &uuid2,
                        &format!("Got your message: {}", m.content),
                        &m.request_id,
                    )
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "are we still on for dinner?",
            "req-1",
            fast_timing(),
            &sink,
        )
        .await
        .unwrap();
        responder.await.unwrap();

        assert!(matches!(outcome, TurnOutcome::Replied { .. }));
        let calls = sink.calls();
        // The reply (last call) went to the 1:1 chat via otto with the session text.
        let (bot, chat_id, text) = calls.last().unwrap();
        assert_eq!(bot, "otto");
        assert_eq!(chat_id, "555");
        assert!(text.contains("Got your message: are we still on for dinner?"));
    }

    #[tokio::test]
    async fn group_round_trip_replies_in_group_via_elected_bot() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto")), ("bruno", Some("bruno"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "bruno", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan =
            plan_conversation(&wg, &cfg, "telegram:bruno", "-100777", "luca-1", Entry::GroupElected);
        let sink = RecSink::default();

        let wg2 = wg.clone();
        let uuid2 = uuid.clone();
        let responder = tokio::spawn(async move {
            for _ in 0..100 {
                let inbox = chat::read_inbox_ref(&wg2, &uuid2).unwrap_or_default();
                if let Some(m) = inbox.iter().find(|m| m.role == "user") {
                    chat::append_outbox_ref(&wg2, &uuid2, "Dinner's at seven.", &m.request_id)
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let outcome =
            run_conversation_turn(&wg, &plan, "bruno what's for dinner?", "req-2", fast_timing(), &sink)
                .await
                .unwrap();
        responder.await.unwrap();

        assert!(matches!(outcome, TurnOutcome::Replied { .. }));
        let (bot, chat_id, text) = sink.calls().last().unwrap().clone();
        assert_eq!(bot, "bruno", "group election replies via the ELECTED bot");
        assert_eq!(chat_id, "-100777", "group reply lands in the GROUP");
        assert_eq!(text, "Dinner's at seven.");
    }

    #[tokio::test]
    async fn slow_turn_sends_ack_before_the_reply() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();

        // Responder deliberately delays past ack_after (80ms) before replying.
        let wg2 = wg.clone();
        let uuid2 = uuid.clone();
        let responder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let inbox = chat::read_inbox_ref(&wg2, &uuid2).unwrap_or_default();
            let m = inbox.iter().find(|m| m.role == "user").unwrap();
            chat::append_outbox_ref(&wg2, &uuid2, "here at last", &m.request_id).unwrap();
        });

        let outcome =
            run_conversation_turn(&wg, &plan, "you there?", "req-3", fast_timing(), &sink)
                .await
                .unwrap();
        responder.await.unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: true });
        let calls = sink.calls();
        assert!(calls.len() >= 2, "expected ack + reply, got {calls:?}");
        // First send is the ack, in the same chat via the same bot.
        assert_eq!(calls[0].0, "otto");
        assert_eq!(calls[0].1, "555");
        assert!(calls[0].2.contains("On it"));
        // Last send is the actual reply.
        assert_eq!(calls.last().unwrap().2, "here at last");
    }

    #[tokio::test]
    async fn no_session_reply_times_out_but_acked_so_never_silent() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        // No responder — the session never replies.
        let timing = AckTiming {
            ack_after: Duration::from_millis(30),
            reply_timeout: Duration::from_millis(150),
            poll: Duration::from_millis(15),
        };
        let outcome = run_conversation_turn(&wg, &plan, "hello?", "req-4", timing, &sink)
            .await
            .unwrap();
        assert_eq!(outcome, TurnOutcome::TimedOut { acked: true });
        // The human at least got the ack — not pure silence.
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.contains("On it"));
    }

    #[tokio::test]
    async fn onboard_sends_one_polite_line_and_nothing_more() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        // No confirmed binding for this sender.
        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "stranger", Entry::Direct);
        let sink = RecSink::default();
        let outcome = run_conversation_turn(&wg, &plan, "hi", "req-5", fast_timing(), &sink)
            .await
            .unwrap();
        assert_eq!(outcome, TurnOutcome::Onboarded);
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "onboarding is one line, nothing more");
        assert_eq!(calls[0].0, "otto");
        assert_eq!(calls[0].1, "555");
    }
}
