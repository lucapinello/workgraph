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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::agency::TelegramBindingMap;
use crate::chat;
use crate::chat_sessions;
use crate::config::Config;
use crate::notify::grounding;
use crate::notify::lifecycle;
use crate::notify::ownership;
use crate::notify::parity;

use super::NotificationChannel;
use super::telegram::{TelegramBotConfig, TelegramChannel, TelegramConfig};
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
        /// Display name of the human who addressed the persona (from the binding
        /// map), so a task this turn creates can be stamped with its origin and a
        /// "are they done yet?" can be answered for the right person. Empty when
        /// the sender resolves to no known display name.
        requester: String,
        /// Which surface the ask arrived on — stamped onto any task created here.
        channel: crate::graph::OriginChannel,
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

    /// The bound session ref this plan converses against, when it is a
    /// [`ConversationPlan::Converse`]. Used by the photo-to-shopping vision turn
    /// to ground the reply in the elected persona's voice.
    pub fn session_ref(&self) -> Option<&str> {
        match self {
            ConversationPlan::Converse { session_ref, .. } => Some(session_ref.as_str()),
            _ => None,
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
    /// The reply composer failed fast (child errored / non-zero exit) or the
    /// compose deadline elapsed, so the human got the graceful "glitched"
    /// follow-up (editing the ack in place when one was sent) rather than a
    /// permanent hourglass. `acked` records whether an ack preceded it.
    Glitched { acked: bool },
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
            TurnOutcome::Glitched { acked } => {
                format!("glitched-fallback (acked={acked})")
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

/// The name of a family member a stranger can ask to be added — derived from
/// the household's own roster (the confirmed Telegram bindings), NOT hardcoded.
///
/// A prebuilt binary must never leak the DEVELOPER's name to a *different*
/// family: the onboarding line greets any stranger pre-sign-in, so a baked-in
/// "Ask Luca…" is shown to households that have never heard of Luca. This is the
/// engine-side twin of the app's no-hardcoded-names guard. We return the first
/// CONFIRMED member's name (author order in the binding map) so the line names a
/// real person who can actually add them; `None` when the roster is empty or
/// unreadable, in which case [`onboarding_line`] uses a neutral fallback.
pub fn family_inviter_name(workgraph_dir: &Path) -> Option<String> {
    let agency_dir = workgraph_dir.join("agency");
    let map = TelegramBindingMap::load(&agency_dir).ok()?;
    map.bindings
        .iter()
        .find(|b| b.confirmed && !b.name.trim().is_empty())
        .map(|b| b.name.trim().to_string())
}

/// One polite line for a sender we don't recognise. Deliberately warm and
/// jargon-free — no command syntax, no ids.
///
/// `inviter` is a family member the stranger can ask to be added, resolved from
/// the household roster by [`family_inviter_name`]. When the roster names no one
/// (empty/unreadable), we fall back to the NEUTRAL "a family member" — never a
/// hardcoded personal name, which in a prebuilt binary would leak the
/// developer's name to an unrelated household.
pub fn onboarding_line(inviter: Option<&str>) -> String {
    let who = inviter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("a family member");
    format!(
        "Hi there! \u{1f44b} I don't recognise you yet, so I can't chat just now. Ask {who} to add \
         you to the family and I'll be right with you."
    )
}

/// Graceful fallback when the addressed agent has no bound session yet.
pub fn sessionless_line() -> String {
    "I'm here! \u{1f642} Give me a little while to get settled and I'll be able to help properly."
        .to_string()
}

/// Human-facing line sent when the reply composition fails or times out — the
/// turn never silently strands the human on a stuck hourglass. Warm, no jargon,
/// invites a retry. Replaces the ack in place when one was sent.
pub fn glitch_line() -> String {
    "Sorry, I glitched for a second there \u{1f605} — mind trying me again?".to_string()
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
/// itself when no explicit binding is configured. Exposed for the listener's
/// casa-feed mirror, which maps the replying `bot_id` back to its persona id.
pub fn agent_for_bot(config: &TelegramConfig, bot_id: &str) -> String {
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

/// Resolve a roster agent handle (the notify.toml `agent_id`, which for the
/// Casa family bots is a human-friendly NAME like `"otto"`) to the **canonical
/// agency agent id** that `wg agent session` uses as the session-binding key.
///
/// This is the fix for the `dedupe-key-fix` converse-hang: `wg agent session
/// <persona>` binds a session under the agent's full 64-hex id (e.g.
/// `c10fe2fb…`), but the election/roster surface addresses the persona by name
/// (`agent_for_bot` returns `"otto"`). Looking a session up by the bare name
/// therefore missed every real binding — nora/bruno/mira resolved to `None`
/// (→ generic Sessionless reply, no memory) and otto matched only a stray
/// name-keyed session with no live agent (→ a full `reply_timeout` hang). By
/// canonicalising the handle first, the lookup lands on the session `wg agent
/// session` actually bound.
///
/// Resolution order: an exact/prefix match on an agency agent **id** wins (so a
/// config that already uses the canonical id is untouched), then a
/// case-insensitive match on the agent **name**. Falls back to the input
/// unchanged when nothing matches — a bot fronting no agency agent, or a test
/// fixture that binds by the literal handle, both keep working.
pub fn canonical_agent_id(workgraph_dir: &Path, agent_ref: &str) -> String {
    let agents_dir = workgraph_dir.join("agency").join("cache/agents");
    let agents = crate::agency::load_all_agents_or_warn(&agents_dir);
    if let Some(a) = agents
        .iter()
        .find(|a| a.id == agent_ref || a.id.starts_with(agent_ref))
    {
        return a.id.clone();
    }
    if let Some(a) = agents.iter().find(|a| a.name.eq_ignore_ascii_case(agent_ref)) {
        return a.id.clone();
    }
    agent_ref.to_string()
}

/// Is this Telegram sender a *confirmed* human? Unknown or unconfirmed senders
/// are not eligible for conversation — they get the onboarding line. A missing
/// binding file (first-ever onboard) reads as "not confirmed".
pub fn sender_is_confirmed(workgraph_dir: &Path, sender: &str) -> bool {
    let agency_dir = workgraph_dir.join("agency");
    match TelegramBindingMap::load(&agency_dir) {
        Ok(map) => map
            .find_by_user(sender)
            .map(|b| b.confirmed)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// The human's display name for this Telegram `sender`, from the binding map
/// (e.g. `"Luca"`), or empty when the sender resolves to no known name. Used to
/// stamp a conversationally-created task's origin and to answer that person's
/// "are they done yet?".
fn requester_display_name(workgraph_dir: &Path, sender: &str) -> String {
    let agency_dir = workgraph_dir.join("agency");
    match TelegramBindingMap::load(&agency_dir) {
        Ok(map) => map
            .find_by_user(sender)
            .map(|b| b.name.clone())
            .unwrap_or_default(),
        Err(_) => String::new(),
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
    // The roster addresses the persona by name ("otto"), but `wg agent session`
    // binds under the canonical agency id — canonicalise before the lookup so we
    // find the session that was actually bound (see `canonical_agent_id`).
    let session_key = canonical_agent_id(workgraph_dir, &agent_id);
    let requester = requester_display_name(workgraph_dir, sender);
    let channel = match entry {
        Entry::Direct => crate::graph::OriginChannel::TelegramDirect,
        Entry::GroupElected => crate::graph::OriginChannel::TelegramGroup,
    };
    match chat_sessions::session_for_agent(workgraph_dir, &session_key) {
        Some(session_ref) => ConversationPlan::Converse {
            session_ref,
            agent_id,
            route,
            entry,
            requester,
            channel,
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
    /// Send a message and return the platform message id when known, so the
    /// caller can later [`edit`](ReplySink::edit) it in place — e.g. replace the
    /// latency ack with the final answer. `Ok(None)` means the id is
    /// unavailable; the caller then falls back to sending a fresh message.
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>>;

    /// Edit a previously-sent message in place (Telegram `editMessageText`) so
    /// the "On it — one sec…" ack becomes the final answer rather than a stale
    /// hourglass followed by a second message. The default falls back to sending
    /// a fresh message, so a sink that cannot edit still never strands the human
    /// on the ack.
    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let _ = message_id;
        self.send(bot_id, chat_id, text).await.map(|_| ())
    }
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

/// Resolve the `(id, config)` a reply must send with, keyed by the ELECTED
/// `bot_id`. When the elected bot is present the reply goes out as that persona
/// (the whole point — see BUG 3: a group-elected reply must render as the elected
/// bot, not the default/concierge one). Only when the elected id is genuinely
/// absent from the config do we fall back to the first bot so a reply still goes
/// out — and we log that loudly, because a silent fallback is exactly what made
/// bruno's group reply render as Otto. Never logs a token.
fn resolve_reply_bot<'a>(
    bots: &'a [(String, TelegramBotConfig)],
    bot_id: &str,
) -> Option<&'a (String, TelegramBotConfig)> {
    if let Some(hit) = bots.iter().find(|(id, _)| id == bot_id) {
        return Some(hit);
    }
    // Elected bot missing from the config — send *something* rather than drop the
    // reply, but make the wrong-persona render diagnosable instead of silent.
    if let Some(first) = bots.first() {
        eprintln!(
            "[convo] elected bot {bot_id:?} not in Telegram config — falling back to {:?}; \
             the reply will render as the wrong persona until the bot is configured",
            first.0,
        );
        return Some(first);
    }
    None
}

#[async_trait]
impl ReplySink for BotReplySink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        let bots = self.config.all_bots();
        let (id, bot) = resolve_reply_bot(&bots, bot_id)
            .ok_or_else(|| anyhow::anyhow!("no Telegram bots configured — cannot reply"))?;
        let channel = TelegramChannel::from_bot(id.clone(), bot.clone());
        let mid = channel.send_text(chat_id, text).await?;
        Ok(Some(mid.0))
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let bots = self.config.all_bots();
        let (id, bot) = resolve_reply_bot(&bots, bot_id)
            .ok_or_else(|| anyhow::anyhow!("no Telegram bots configured — cannot edit"))?;
        let channel = TelegramChannel::from_bot(id.clone(), bot.clone());
        // If the edit fails (e.g. message too old, or a non-numeric id), fall
        // back to a fresh send so the human still gets the answer — never a
        // stranded hourglass.
        if let Err(e) = channel.edit_text(chat_id, message_id, text).await {
            // `{e:#}` prints the full error chain, which for a transport
            // failure embeds the request URL (and thus the bot token) — redact
            // before logging. See `telegram::redact_bot_token`.
            eprintln!(
                "[convo] editMessageText failed ({}) — sending fresh message instead",
                super::telegram::redact_bot_token(&format!("{e:#}"))
            );
            channel.send_text(chat_id, text).await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reply composition (the think-spawn seam — mockable in tests)
// ---------------------------------------------------------------------------

/// Composes the agent's actual reply to a human turn.
///
/// This is the seam the `fix-converse-hang` bug lived behind: the previous
/// design wrote the human turn to the bound-session inbox and *polled the
/// outbox forever* for a reply that only a live `wg nex` daemon could produce.
/// In the real deployment no such daemon runs, so every converse turn acked at
/// `ack_after` and then TIMED OUT at `reply_timeout` — the real answer never
/// sent. The composer replaces that open-loop wait with a bounded, directly
/// driven turn: the production impl spawns a one-shot `claude` CLI call
/// ([`OneshotComposer`]); tests substitute a synchronous fake so the round-trip
/// and the failure/timeout paths are provable without a live model.
#[async_trait]
pub trait ReplyComposer: Send + Sync {
    /// Produce the reply text for `human_message`, grounded in the persona's
    /// bound session (`session_ref`). Returns `Err` fast on any failure so the
    /// caller can send the graceful "glitched" follow-up instead of hanging.
    async fn compose(
        &self,
        workgraph_dir: &Path,
        session_ref: &str,
        agent_id: &str,
        human_message: &str,
    ) -> Result<String>;
}

/// Production composer: a **one-shot** LLM turn via
/// [`crate::service::llm::run_model_oneshot`] (which shells out to the `claude`
/// CLI in `--print --output-format json` non-interactive mode, prompt on stdin
/// with EOF, wrapped in the platform timeout, Claude-Code env stripped, `[auth]`
/// OAuth injected — see `call_claude_cli`). This is the exact spawn the task
/// asked to drive: non-interactive, self-authenticating, and time-bounded, so
/// it either returns the answer or fails fast with the child's stderr.
pub struct OneshotComposer {
    config: Config,
    model_spec: String,
    timeout_secs: u64,
}

impl OneshotComposer {
    /// Build from the merged workgraph config. The model spec and per-call
    /// timeout are env-tunable without a rebuild:
    /// - `WG_TELEGRAM_COMPOSE_MODEL` (default `claude:haiku` — fast, cheap,
    ///   self-authenticating CLI, right-sized for a short family-chat reply)
    /// - `WG_TELEGRAM_COMPOSE_TIMEOUT_SECS` (default 90)
    pub fn from_config(config: Config) -> Self {
        let model_spec = std::env::var("WG_TELEGRAM_COMPOSE_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "claude:haiku".to_string());
        let timeout_secs = std::env::var("WG_TELEGRAM_COMPOSE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(90);
        Self {
            config,
            model_spec,
            timeout_secs,
        }
    }
}

/// Assemble the composer prompt: the persona's session-summary (voice + role),
/// a short slice of recent conversation for continuity, and the human's new
/// message — with explicit family-voice guidance (warm, no jargon, no task ids;
/// see the `family-voice-no-jargon` project rule). Pure/filesystem-only.
fn build_compose_prompt(
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    human_message: &str,
) -> String {
    // THREAD CONTEXT (task nora-clarify-engine, fix 2): the gateway forwards the
    // ORIGINATING pane's recent turns via `WG_THREAD_CONTEXT` so a topic
    // follow-up ("tell me the calories" right after a pasta-pomodoro nutrition
    // line) resolves against that thread and is ANSWERED, instead of the composer
    // treating it as an ambiguous fresh ask and clarifying. Read from env at the
    // production boundary and threaded into the pure `_at` builder so tests stay
    // deterministic. Unset (the Telegram listener path, which reads the session
    // inbox) → `None`, prompt unchanged.
    let thread = std::env::var("WG_THREAD_CONTEXT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Household local time is the existing `chrono::Local` seam (same as the
    // fast lane and task-stamping). Threaded through the `_at` variant so the
    // scope/clock rules are testable with a fixed clock.
    build_compose_prompt_at(
        workgraph_dir,
        session_ref,
        agent_id,
        human_message,
        chrono::Local::now().naive_local(),
        thread.as_deref(),
    )
}

/// [`build_compose_prompt`] with the household's local time injected, so the
/// scope (rule 3) and clock-aware (rule 4) grounding are fully deterministic
/// under test. Production passes `chrono::Local::now()`.
fn build_compose_prompt_at(
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    human_message: &str,
    now: chrono::NaiveDateTime,
    thread_context: Option<&str>,
) -> String {
    let summary = read_session_summary(workgraph_dir, session_ref);

    // A few recent turns for continuity (best-effort; empty on a fresh session).
    let mut history: Vec<String> = Vec::new();
    if let Ok(inbox) = chat::read_inbox_ref(workgraph_dir, session_ref) {
        for m in inbox.iter().rev().take(4).rev() {
            if m.role == "user" {
                history.push(format!("Human: {}", m.content.trim()));
            }
        }
    }
    if let Ok(outbox) = chat::read_outbox_since_ref(workgraph_dir, session_ref, 0) {
        for m in outbox.iter().rev().take(4).rev() {
            history.push(format!("You: {}", m.content.trim()));
        }
    }

    let mut prompt = String::new();
    match &summary {
        Some(s) => {
            prompt.push_str("You are answering as this person, in their voice:\n\n");
            prompt.push_str(s);
            prompt.push_str("\n\n");
        }
        None => {
            prompt.push_str(&format!(
                "You are '{agent_id}', a warm, helpful member of the family team.\n\n"
            ));
        }
    }
    if !history.is_empty() {
        prompt.push_str("Recent conversation:\n");
        prompt.push_str(&history.join("\n"));
        prompt.push_str("\n\n");
    }
    // THREAD CONTEXT (task nora-clarify-engine, fix 2): the recent turns of the
    // ORIGINATING conversation, forwarded by the gateway. A topic follow-up
    // ("tell me the calories" right after a pasta-pomodoro nutrition line) is
    // ambiguous in isolation, so without this the composer asks a clarifying
    // question; WITH it, the topic is resolvable and the persona answers directly.
    // The explicit instruction tells the model to treat the new message as a
    // continuation and NOT clarify when the thread already carries the referent.
    if let Some(thread) = thread_context.map(str::trim).filter(|t| !t.is_empty()) {
        prompt.push_str(
            "Recent messages in this conversation (the message below is very likely a \
             follow-up to these — resolve any pronoun, \"it\", or omitted topic from here \
             and answer DIRECTLY; do NOT ask what they mean when the topic is already \
             clear from these turns):\n",
        );
        prompt.push_str(thread);
        prompt.push_str("\n\n");
    }
    prompt.push_str(
        "Reply to the message below in a natural, friendly way. Keep it short and \
         conversational. Talk like a person texting family — no jargon, no task ids, no \
         status dumps, no markdown headings. Just answer.\n\n",
    );
    prompt.push_str(
        "If the message is asking the family to actually DO something (change the week, \
         plan a meal, run an errand, book something), reply warmly that you're on it, then \
         on a FINAL separate line emit exactly one machine directive of the form \
         `TASK_CREATE: <short imperative describing the work>`. It is stripped before the \
         human sees your reply, so never mention it. Do NOT emit it for small talk, \
         questions, or things you can answer directly.\n\n",
    );

    // DATE ANCHOR (rule 1b): ALWAYS ground the persona in today's local date and
    // resolve any relative day term in the message ("tomorrow", "tonight",
    // "tomorrow night", a named weekday) to a concrete date. Unlike the scoped
    // week block below, this runs for write/plan-change asks too — the branzino
    // transcript (Luca, 2026-07-17) had the persona schedule "tomorrow night"
    // for a PAST Thursday because a plan-change ask carried no date anchor at
    // all. Pure/clock-seam only, so it is deterministic under `build_*_at`.
    prompt.push_str(&grounding::date_anchor(human_message, now));

    // ANTI-FABRICATION context (rule 5, docs/20 §6.7): ALWAYS put the real,
    // clock-scoped calendar in front of the composer BEFORE it drafts — today's
    // actual events, or an explicit "the calendar is clear". The transcript's
    // fabrication ("birthday + back-to-back meetings, packed day" on an EMPTY
    // calendar) was VOLUNTEERED in ordinary chatter, so it never hit the
    // read-shaped grounding below; the calendar was simply not in the context.
    // This line closes that hole for every message shape. Best-effort read.
    // (root is bound once here and reused by the read-shaped grounding +
    // corrections blocks below — #26 date-anchor and #28 anti-fabrication
    // both landed, one binding.)
    let root = project_root_of(workgraph_dir);
    prompt.push_str(&grounding::fetch_schedule_context_line(&root, now));
    prompt.push('\n');

    // GROUNDING (rule 1): a question/read-shaped ask about plans/calendar/meals
    // /schedule gets the REAL week model injected, so the answer is grounded on
    // turn one instead of a stall. This is the read-side twin of the fast lane's
    // edit-shaped classifier. Best-effort — a missing plan just omits the block.
    if grounding::is_read_shaped(human_message) {
        if let Some(block) = grounding::fetch_scoped(&root, now, human_message) {
            prompt.push_str(&block);
            prompt.push('\n');
        }
    }

    // CORRECTIONS (rule 3): replay every correction the family has made so the
    // persona honours it for the rest of the window and every future turn, and
    // never repeats a claim they already corrected.
    let corrections: Vec<String> = parity::PreferenceStore::all(&root)
        .into_iter()
        .filter_map(|r| {
            r.text
                .strip_prefix(grounding::CORRECTION_PREFIX)
                .map(|c| c.trim().to_string())
        })
        .filter(|c| !c.is_empty())
        .collect();
    if let Some(block) = grounding::corrections_block(&corrections) {
        prompt.push_str(&block);
        prompt.push('\n');
    }

    prompt.push_str(&format!("Message: {}\n\nYour reply:", human_message.trim()));
    prompt
}

/// Read a bound persona's `session-summary.md` (its voice + role), trimmed;
/// `None` when absent or empty. Public so the photo-to-shopping vision turn can
/// ground its reply in the same persona summary the text composer uses.
pub fn read_session_summary(workgraph_dir: &Path, session_ref: &str) -> Option<String> {
    let chat_dir = chat::chat_dir_for_ref(workgraph_dir, session_ref);
    std::fs::read_to_string(chat_dir.join("session-summary.md"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[async_trait]
impl ReplyComposer for OneshotComposer {
    async fn compose(
        &self,
        workgraph_dir: &Path,
        session_ref: &str,
        agent_id: &str,
        human_message: &str,
    ) -> Result<String> {
        let prompt =
            build_compose_prompt(workgraph_dir, session_ref, agent_id, human_message);
        let config = self.config.clone();
        let model = self.model_spec.clone();
        let timeout = self.timeout_secs;
        // `run_model_oneshot` is synchronous and spawns+waits a child CLI, so it
        // must run on a blocking thread rather than stalling the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            crate::service::llm::run_model_oneshot(&config, &model, &prompt, timeout)
        })
        .await
        .context("compose task panicked")??;
        let text = result.text.trim().to_string();
        if text.is_empty() {
            anyhow::bail!("compose model returned an empty reply");
        }
        Ok(text)
    }
}

/// The same one-shot spawn, but with attached images — the `photo-to-shopping`
/// vision turn. Reuses this composer's model/timeout (so the vision model is
/// tunable via `WG_TELEGRAM_COMPOSE_MODEL`, which must resolve to a Claude CLI
/// model for images to attach). The prompt is built by the caller
/// ([`super::telegram_photo::build_vision_prompt`]); we only spawn the model.
#[async_trait]
impl super::telegram_photo::VisionComposer for OneshotComposer {
    async fn compose_vision(
        &self,
        prompt: &str,
        image_paths: &[std::path::PathBuf],
    ) -> Result<String> {
        let config = self.config.clone();
        let model = self.model_spec.clone();
        let timeout = self.timeout_secs;
        let prompt = prompt.to_string();
        let images = image_paths.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            crate::service::llm::run_model_oneshot_with_images(
                &config, &model, &prompt, &images, timeout,
            )
        })
        .await
        .context("vision compose task panicked")??;
        let text = result.text.trim().to_string();
        if text.is_empty() {
            anyhow::bail!("vision compose model returned an empty reply");
        }
        Ok(text)
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
#[allow(clippy::too_many_arguments)]
pub async fn run_conversation_turn(
    workgraph_dir: &Path,
    plan: &ConversationPlan,
    human_message: &str,
    request_id: &str,
    timing: AckTiming,
    composer: Option<&dyn ReplyComposer>,
    sink: &dyn ReplySink,
) -> Result<TurnOutcome> {
    match plan {
        ConversationPlan::Onboard { route, .. } => {
            let inviter = family_inviter_name(workgraph_dir);
            sink.send(
                &route.bot_id,
                &route.chat_id,
                &onboarding_line(inviter.as_deref()),
            )
            .await?;
            Ok(TurnOutcome::Onboarded)
        }
        ConversationPlan::Sessionless { route, .. } => {
            sink.send(&route.bot_id, &route.chat_id, &sessionless_line())
                .await?;
            Ok(TurnOutcome::Sessionless)
        }
        ConversationPlan::Converse {
            session_ref,
            route,
            agent_id,
            requester,
            channel,
            ..
        } => match composer {
            // The real path: directly drive a bounded compose turn (a one-shot
            // `claude` spawn in production). Completes with the real answer or
            // fails fast into the graceful "glitched" follow-up — never the
            // open-loop 120s hang that this task fixes.
            Some(composer) => {
                // Everything the lifecycle loop needs to stamp a task this turn
                // creates and to report back here later, in this persona's voice.
                let origin = crate::graph::TaskOrigin::new(
                    *channel,
                    route.chat_id.clone(),
                    requester.clone(),
                    agent_id.clone(),
                    Some(route.bot_id.clone()),
                );
                run_composed_turn(
                    workgraph_dir,
                    session_ref,
                    agent_id,
                    human_message,
                    request_id,
                    timing,
                    route,
                    sink,
                    composer,
                    &origin,
                )
                .await
            }
            // Legacy path (no composer injected): write the human turn to the
            // session inbox and poll the outbox for a reply a live session
            // produces. Retained for callers/tests that supply their own
            // outbox producer.
            None => {
                let baseline = outbox_baseline(workgraph_dir, session_ref);
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
        },
    }
}

/// Deliver `text` to the human: edit the latency ack in place when one was sent
/// (turning the hourglass into the final answer), else send a fresh message.
async fn deliver_reply(
    sink: &dyn ReplySink,
    route: &ReplyRoute,
    ack_mid: Option<&str>,
    text: &str,
) -> Result<()> {
    match ack_mid {
        Some(mid) if !mid.is_empty() => {
            sink.edit(&route.bot_id, &route.chat_id, mid, text).await
        }
        _ => sink
            .send(&route.bot_id, &route.chat_id, text)
            .await
            .map(|_| ()),
    }
}

/// Answer a requester's "are they done yet?" from their origin-stamped tasks'
/// live state. Loads the graph, keeps the tasks stamped as theirs, and renders
/// a family-voice status line ([`lifecycle::answer_status`]). `None` when they
/// have no such tasks — the caller then falls back to an ordinary chat turn.
fn answer_status_from_graph(workgraph_dir: &Path, requester: &str) -> Option<String> {
    let graph = crate::parser::load_graph(workgraph_dir.join("graph.jsonl")).ok()?;
    let states: Vec<lifecycle::TaskState> = graph
        .tasks()
        .filter(|t| {
            t.origin
                .as_ref()
                .map(|o| o.requester.eq_ignore_ascii_case(requester))
                .unwrap_or(false)
        })
        .map(lifecycle::TaskState::from_task)
        .collect();
    lifecycle::answer_status(&states)
}

/// Create an origin-stamped task from a conversational `TASK_CREATE:` title and
/// persist it to the graph, returning its id. The task lands `Open` (the
/// coordinator dispatches it from there) and carries `origin`, so the lifecycle
/// loop can report "on it" / "done" back to the chat the ask arrived in.
fn create_origin_task(
    workgraph_dir: &Path,
    title: &str,
    origin: &crate::graph::TaskOrigin,
) -> Result<String> {
    use crate::graph::{Node, Status, Task, WorkGraph};
    let path = workgraph_dir.join("graph.jsonl");
    // A missing/empty graph (first-ever task) is not an error — start fresh.
    let mut graph = if path.exists() {
        crate::parser::load_graph(&path).map_err(|e| anyhow::anyhow!("load graph: {e}"))?
    } else {
        WorkGraph::new()
    };
    let id = lifecycle::derive_task_id(title, |cand| graph.get_node(cand).is_some());
    let now = chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let who = if origin.requester.is_empty() {
        "a family member"
    } else {
        &origin.requester
    };
    let task = Task {
        id: id.clone(),
        title: title.to_string(),
        description: Some(format!(
            "Created from a {} chat request by {who}.",
            origin.channel.label(),
        )),
        status: Status::Open,
        created_at: Some(now.clone()),
        last_interaction_at: Some(now),
        origin: Some(origin.clone()),
        ..Default::default()
    };
    graph.add_node(Node::Task(task));
    crate::parser::save_graph(&graph, &path).map_err(|e| anyhow::anyhow!("save graph: {e}"))?;
    Ok(id)
}

/// Drive a bounded compose turn: race the composer against the ack/timeout
/// clock. Emits the latency ack once past `ack_after`; on success relays the
/// answer (editing the ack in place); on failure OR at `reply_timeout` sends the
/// graceful "glitched" follow-up (also editing the ack) so the human never sees
/// a permanent hourglass. The composed reply is also written to the session
/// outbox so the TUI / casa feed stay consistent with what was sent.
#[allow(clippy::too_many_arguments)]
async fn run_composed_turn(
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    human_message: &str,
    request_id: &str,
    timing: AckTiming,
    route: &ReplyRoute,
    sink: &dyn ReplySink,
    composer: &dyn ReplyComposer,
    origin: &crate::graph::TaskOrigin,
) -> Result<TurnOutcome> {
    // ONE REPLY PER TURN (idempotency). A single turn is keyed by `request_id`,
    // and every reply we send is also appended to the outbox under that id. If an
    // outbox reply for this exact request already exists, this turn has already
    // been answered — a re-fire (a listener re-poll, a gateway retry, a
    // restart-replay) must NOT post a second message. This is the guard against
    // the back-to-back double-post Luca saw from Otto: one ask, two messages. We
    // return without composing or sending again. Best-effort read — a missing
    // outbox simply means "not answered yet".
    if !request_id.trim().is_empty() {
        let already_answered = chat::read_outbox_since_ref(workgraph_dir, session_ref, 0)
            .map(|out| out.iter().any(|m| m.request_id == request_id))
            .unwrap_or(false);
        if already_answered {
            println!(
                "[{}] convo idempotency: request {request_id} already answered for {agent_id} — \
                 skipping duplicate reply",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            return Ok(TurnOutcome::Replied { acked: false });
        }
    }

    // "Are they done yet?" — a status question from someone with recent
    // origin-stamped tasks is answered from LIVE graph state, not a generic chat
    // turn. This is the honest report-back: what's in progress / done, in the
    // persona's voice, without spinning up the model.
    if !origin.requester.trim().is_empty() && lifecycle::is_status_question(human_message) {
        if let Some(answer) = answer_status_from_graph(workgraph_dir, &origin.requester) {
            let _ = chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id);
            let _ = chat::append_outbox_ref(workgraph_dir, session_ref, &answer, request_id);
            deliver_reply(sink, route, None, &answer).await?;
            return Ok(TurnOutcome::Replied { acked: false });
        }
    }

    // CORRECTIONS STICK (rule 3): if the human is correcting a fact mid-chat
    // ("Nadin is not logged so ignore this"), persist it BEFORE we compose so
    // the very reply to this turn honours it (`build_compose_prompt` replays
    // every recorded correction), and so does every future turn. Best-effort.
    if let Some(correction) = grounding::detect_correction(human_message) {
        let root = project_root_of(workgraph_dir);
        let stored = format!("{}{}", grounding::CORRECTION_PREFIX, correction);
        match parity::PreferenceStore::record(
            &root,
            &stored,
            &origin.requester,
            &origin.persona,
        ) {
            Ok(_) => println!(
                "[{}] conversation recorded correction (chat {})",
                chrono::Utc::now().format("%H:%M:%S"),
                origin.chat_id,
            ),
            Err(e) => eprintln!(
                "[{}] failed to record correction: {e}",
                chrono::Utc::now().format("%H:%M:%S"),
            ),
        }
    }

    // Persist the human turn so a live nex session and the TUI stay consistent
    // with the answer we compose here (best-effort — a write failure must not
    // block the reply).
    let _ = chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id);

    let compose = composer.compose(workgraph_dir, session_ref, agent_id, human_message);
    tokio::pin!(compose);

    let start = Instant::now();
    let mut acked = false;
    let mut ack_mid: Option<String> = None;

    loop {
        let elapsed = start.elapsed();
        // Wake at the next deadline we still care about: the ack point (if not
        // yet acked) or the hard reply timeout.
        let deadline = if !acked && elapsed < timing.ack_after {
            timing.ack_after
        } else {
            timing.reply_timeout
        };
        let sleep_for = deadline.saturating_sub(elapsed);

        tokio::select! {
            res = &mut compose => {
                match res {
                    Ok(text) => {
                        // Post-turn parity audit + delivery: create the task the
                        // reply promised (via the composer's directive, a forced
                        // retry, or a fallback), record a standing preference, or
                        // just relay a non-committal reply — always leaving the
                        // promised artifact or an honest correction.
                        return finalize_composed_reply(
                            workgraph_dir,
                            session_ref,
                            agent_id,
                            human_message,
                            request_id,
                            route,
                            sink,
                            composer,
                            origin,
                            ack_mid.as_deref(),
                            acked,
                            text,
                        )
                        .await;
                    }
                    Err(e) => {
                        // Fail fast — surface the child's error (never a token)
                        // and give the human the graceful follow-up.
                        eprintln!(
                            "[{}] convo compose failed for {agent_id}: {e:#}",
                            chrono::Utc::now().format("%H:%M:%S"),
                        );
                        deliver_reply(sink, route, ack_mid.as_deref(), &glitch_line()).await?;
                        return Ok(TurnOutcome::Glitched { acked });
                    }
                }
            }
            _ = tokio::time::sleep(sleep_for) => {
                let elapsed = start.elapsed();
                if !acked && elapsed >= timing.ack_after && elapsed < timing.reply_timeout {
                    ack_mid = sink.send(&route.bot_id, &route.chat_id, &ack_line()).await?;
                    acked = true;
                }
                if start.elapsed() >= timing.reply_timeout {
                    eprintln!(
                        "[{}] convo compose timed out for {agent_id} after {:?}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        timing.reply_timeout,
                    );
                    deliver_reply(sink, route, ack_mid.as_deref(), &glitch_line()).await?;
                    return Ok(TurnOutcome::Glitched { acked });
                }
            }
        }
    }
}

/// Turn a composed reply into a delivered message with **promise-action
/// parity** enforced: when the reply commits to doing something, the system
/// proves it happened.
///
/// The decision tree over one composed `first_text`:
/// * The composer emitted a `TASK_CREATE:` tail → create that task (the happy
///   path that already worked for the carbonara ask).
/// * The reply is a standing preference ("no weekday lunches") → record it in
///   the durable [`parity::PreferenceStore`], not a one-off task.
/// * The reply commits to a one-off action but produced NO tail (the salad
///   regression) → retry the compose ONCE, forcing the tail; if it still
///   produces nothing, create a fallback task from the promise text AND append
///   an honest correction to the reply so the ask is never silently dropped.
/// * A non-committal reply → relay as-is.
///
/// Every turn logs `promised=<kind> created=<task-id|none>` so the parity of
/// promises-vs-artifacts is observable.
#[allow(clippy::too_many_arguments)]
async fn finalize_composed_reply(
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    human_message: &str,
    request_id: &str,
    route: &ReplyRoute,
    sink: &dyn ReplySink,
    composer: &dyn ReplyComposer,
    origin: &crate::graph::TaskOrigin,
    ack_mid: Option<&str>,
    acked: bool,
    first_text: String,
) -> Result<TurnOutcome> {
    let directive = lifecycle::extract_task_directive(first_text.trim());
    let mut reply_text = directive.reply.clone();
    // Audit the human-facing reply (with the machine tail already stripped).
    let audit = parity::audit_promise(&reply_text);
    let mut created: Option<String> = None;

    // SINGLE-OWNER RULE. Before any creation, resolve who OWNS this ask's domain
    // (from `household.toml`, else the Casa default). Exactly one persona — the
    // owner — mints the task; every other voice in a collective turn defers. This
    // is the fix for Luca's tofu bug: one group ask electing the whole roster no
    // longer mints one task per persona (with Coach Mira taking on a cooking task).
    let decision = {
        let root = project_root_of(workgraph_dir);
        ownership::OwnerMap::load(&root).decide_owner(&origin.persona, human_message)
    };

    // DEFER DISCIPLINE (morning-taco-bugs): the defer line ("Bruno's got this
    // one 🍳") must NEVER appear on the OWNER's own reply. `decide_owner` keys on
    // `origin.persona`, but a group-elected turn stamps that from the bot's
    // agent id — and a bot with no configured `agent_id` falls back to its bot id
    // ("bruno_casapinello_bot"), which does not textually equal the owner id
    // ("bruno"). That mismatch made Bruno defer to *himself* out loud. Correct a
    // Defer back to Owner whenever the speaking voice actually IS the owner (by
    // persona or bot id), so only a genuinely off-domain voice ever defers.
    let decision = match decision {
        ownership::OwnerDecision::Defer { owner } if speaker_is_owner(origin, &owner) => {
            ownership::OwnerDecision::Owner
        }
        other => other,
    };

    match decision {
        ownership::OwnerDecision::Defer { owner } => {
            // OFF-DOMAIN GUARD. This voice does not own the ask, so it must NOT
            // create its own copy. If the turn nonetheless promised an action (a
            // directive tail or an action-committing reply), log a loud warning
            // and RE-ROUTE the ask to the owner: create it ONCE, stamped as the
            // owner, guarded by the intent ledger so a collective never multiplies
            // it. Then defer out loud so the ask visibly lands with its owner.
            let wants_task = directive.title.is_some() || audit.commits_action();
            if wants_task {
                let domain = ownership::classify_domain(human_message);
                eprintln!(
                    "[{}] off-domain guard: {} would create a {} task it does not own — re-routing to {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    origin.persona,
                    domain.slug(),
                    owner,
                );
                let title = directive
                    .title
                    .clone()
                    .unwrap_or_else(|| parity::fallback_task_title(human_message, &reply_text));
                let owner_origin = origin_as_persona(origin, &owner);
                created =
                    try_create_origin_task(workgraph_dir, human_message, &title, &owner_origin);
                let line = ownership::defer_line(&owner, domain);
                if reply_text.is_empty() {
                    reply_text = line;
                } else {
                    reply_text.push_str("\n\n");
                    reply_text.push_str(&line);
                }
            }
        }
        ownership::OwnerDecision::Owner => {
            if let Some(title) = directive.title.as_deref() {
                // The composer emitted the artifact directive — stamp it with this
                // ask's origin so the lifecycle loop can report back here later.
                created = try_create_origin_task(workgraph_dir, human_message, title, origin);
            } else if audit.kind == parity::PromiseKind::Preference {
                // A standing rule: write it where every future weekly draft can read it.
                record_standing_preference(workgraph_dir, &reply_text, origin);
            } else if audit.commits_action() {
                // MISMATCH: the reply promised a one-off action but left no artifact.
                // Retry the turn ONCE, explicitly instructing the persona to emit the
                // directive this time.
                let retry_msg = parity::retry_message(human_message, &reply_text);
                match composer
                    .compose(workgraph_dir, session_ref, agent_id, &retry_msg)
                    .await
                {
                    Ok(retry_raw) => {
                        let retry_dir = lifecycle::extract_task_directive(retry_raw.trim());
                        if let Some(title) = retry_dir.title.as_deref() {
                            created =
                                try_create_origin_task(workgraph_dir, human_message, title, origin);
                            // Prefer the retry's fresh confirmation when it created the task.
                            if created.is_some() && !retry_dir.reply.is_empty() {
                                reply_text = retry_dir.reply;
                            }
                        }
                    }
                    Err(e) => eprintln!(
                        "[{}] parity retry compose failed for {agent_id}: {e:#}",
                        chrono::Utc::now().format("%H:%M:%S"),
                    ),
                }
                if created.is_none() {
                    // The persona still would not create it — never lose the ask. Build a
                    // fallback task from the promise text and correct the record honestly.
                    let title = parity::fallback_task_title(human_message, &reply_text);
                    created = try_create_origin_task(workgraph_dir, human_message, &title, origin);
                    let correction = parity::correction_line();
                    if reply_text.is_empty() {
                        reply_text = correction;
                    } else {
                        reply_text.push_str("\n\n");
                        reply_text.push_str(&correction);
                    }
                }
            }
        }
    }

    // Metrics: every turn records promised-vs-created (task id), so a promise
    // that leaves no artifact is loud, not silent.
    println!(
        "[{}] parity: promised={} created={} agent={} chat={}",
        chrono::Utc::now().format("%H:%M:%S"),
        audit.kind.slug(),
        created.as_deref().unwrap_or("none"),
        agent_id,
        origin.chat_id,
    );

    // Honor an explicit "let me know when…": acknowledge it out loud (the payoff
    // itself arrives later via the Done notification).
    if lifecycle::is_follow_request(human_message) {
        if reply_text.is_empty() {
            reply_text = lifecycle::FOLLOW_ACK.to_string();
        } else {
            reply_text.push_str("\n\n");
            reply_text.push_str(lifecycle::FOLLOW_ACK);
        }
    }
    if reply_text.is_empty() {
        // The whole reply was a bare directive — never send an empty message.
        reply_text = "On it 👍".to_string();
    }

    // Read this persona's prior replies (this turn's outbox is not appended
    // yet) for the repetition and style guards.
    let prior_replies: Vec<String> = chat::read_outbox_since_ref(workgraph_dir, session_ref, 0)
        .map(|out| out.into_iter().map(|m| m.content).collect())
        .unwrap_or_default();

    // STYLE (rule 4): at most one formulaic "Anything specific…?" tail per
    // conversation. If a prior reply already spent the allowance, drop this
    // one's trailing filler question (never emptying the reply). Applied before
    // the repetition guard so the honest fallback below is never itself trimmed.
    let already_used = grounding::count_formulaic(&prior_replies) > 0;
    reply_text = grounding::enforce_style(&reply_text, already_used);

    // ANSWER-FIRST HARD RULE (rule 2 promoted): a plain read-ask must be
    // answered, not bounced back with a question. If the human asked a
    // read-shaped question and did NOT invite us to deliberate/plan, strip any
    // trailing question so the reply ends on a statement. The deliberation
    // escape hatch ("let's think about the day") keeps its question. Applied
    // before the repetition guard so the honest fallback's own question — an
    // offer to go read the source — is never trimmed.
    if grounding::is_read_shaped(human_message)
        && !grounding::is_deliberation_request(human_message)
    {
        reply_text = grounding::enforce_answer_shape(&reply_text, false);
    }

    // ANTI-FABRICATION GUARD (rule 5, docs/20 §6.7): a composed reply must NEVER
    // assert a schedule/calendar fact — a meeting, a birthday, "back-to-back",
    // "packed" — with NO support in the REAL calendar. This runs on EVERY reply
    // (greeting chatter, 1:1, group), not just read-shaped asks, because the
    // transcript's fabrication was volunteered, not requested. It runs BEFORE the
    // repetition guard (mirroring the JS twin's §6.7-before-§6.3 order): an
    // invented schedule fact must not survive even if it is a fresh, non-repeated
    // line. Grounding is scoped to what the model was shown; an empty/absent
    // calendar is strict — any schedule claim is then a fabrication.
    {
        let now = chrono::Local::now().naive_local();
        let sched = grounding::fetch_schedule_grounding(&project_root_of(workgraph_dir), now, human_message);
        let unsourced = grounding::find_unsourced_schedule_claims(&reply_text, &sched);
        if !unsourced.is_empty() {
            eprintln!(
                "[{}] anti-fabrication guard: {agent_id}'s draft asserts unsourced schedule facts {unsourced:?} — rewriting to the honest fallback",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = grounding::grounding_fallback_line();
        }
    }

    // REPETITION GUARD (rule 2): never send the same summary a third time. If
    // this draft is substantially the same as the previous reply, answer
    // honestly instead — own that the answer already went out and offer to
    // actually go read the source. Delivered verbatim (style is not re-applied).
    // With turn-one grounding in place this is a backstop; the transcript shows
    // exactly why the backstop must exist.
    if let Some(prev) = prior_replies.last() {
        if grounding::is_repetitive(&reply_text, prev) {
            eprintln!(
                "[{}] repetition guard: {agent_id}'s draft repeats its previous reply — answering honestly",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = grounding::repetition_fallback_line();
        }
    }

    let _ = chat::append_outbox_ref(workgraph_dir, session_ref, &reply_text, request_id);
    deliver_reply(sink, route, ack_mid, &reply_text).await?;
    Ok(TurnOutcome::Replied { acked })
}

/// Create an origin-stamped task, logging success/failure; returns the id the
/// ask now lives under. Thin wrapper so the parity flow reads cleanly, with the
/// **intent-dedupe safety net** in front of every creation path: a second
/// creation matching the ask's fingerprint (normalized `human_message` + origin
/// chat, within the dedupe window) is REFUSED — logged as `duplicate intent,
/// task X already exists` — and reuses the existing task, no matter which
/// persona tries. This backstops the single-owner rule for races and for
/// households where the owner cannot be resolved.
fn try_create_origin_task(
    workgraph_dir: &Path,
    human_message: &str,
    title: &str,
    origin: &crate::graph::TaskOrigin,
) -> Option<String> {
    let root = project_root_of(workgraph_dir);

    // AUTHORITATIVE OFF-DOMAIN GUARD — the round-2 fix. EVERY conversationally
    // created task funnels through this choke point: a collective round, a
    // single-voice/concierge turn (Otto answering 1:1-style in the group), a
    // parity retry, a fallback — and a restart-replayed sibling of any of them.
    // finalize_composed_reply already routes the COLLECTIVE case, but its guard
    // keys on the election shape; a single-voice turn that reaches creation with
    // the answering voice as `origin.persona` would otherwise land a meals task on
    // Otto (Luca, 2026-07-14: "why is otto dealing with dishes"). So ownership is
    // decided HERE, next to the intent dedupe, independent of who called: whatever
    // persona the caller stamped, re-route ownership to the ask's DOMAIN OWNER
    // from household.toml (Casa default as fallback). A voice that already owns the
    // domain, or an ask whose owner cannot be resolved, is left untouched
    // (fail-open — a real ask is never dropped; the intent ledger still dedupes).
    let owned_origin = match ownership::OwnerMap::load(&root)
        .decide_owner(&origin.persona, human_message)
    {
        ownership::OwnerDecision::Owner => None,
        ownership::OwnerDecision::Defer { owner } => {
            eprintln!(
                "[{}] creation choke-point off-domain guard: {} does not own a {} task — re-stamping ownership to {}",
                chrono::Utc::now().format("%H:%M:%S"),
                if origin.persona.is_empty() { "an unnamed voice" } else { origin.persona.as_str() },
                ownership::classify_domain(human_message).slug(),
                owner,
            );
            Some(origin_as_persona(origin, &owner))
        }
    };
    let origin = owned_origin.as_ref().unwrap_or(origin);

    let fp = ownership::fingerprint(human_message, &origin.chat_id);
    let now = chrono::Utc::now().timestamp();
    let window = ownership::IntentLedger::window_secs();
    if let Some(existing) = ownership::IntentLedger::find_recent(&root, &fp, now, window) {
        // The safety net fired: this exact ask already became a task inside the
        // window. Refuse the duplicate and reuse it — regardless of persona.
        println!(
            "[{}] duplicate intent, task {} already exists (persona {} chat {})",
            chrono::Utc::now().format("%H:%M:%S"),
            existing,
            origin.persona,
            origin.chat_id,
        );
        return Some(existing);
    }
    match create_origin_task(workgraph_dir, title, origin) {
        Ok(id) => {
            println!(
                "[{}] conversation created task {} (origin {} chat {})",
                chrono::Utc::now().format("%H:%M:%S"),
                id,
                origin.channel.label(),
                origin.chat_id,
            );
            // Record the intent so any sibling turn (a later collective voice, a
            // restart-replayed message) dedupes against it.
            if let Err(e) =
                ownership::IntentLedger::record(&root, &fp, &id, &origin.persona, now)
            {
                eprintln!(
                    "[{}] failed to record task intent for dedupe: {e}",
                    chrono::Utc::now().format("%H:%M:%S"),
                );
            }
            Some(id)
        }
        Err(e) => {
            eprintln!(
                "[{}] failed to create conversational task: {e:#}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            None
        }
    }
}

/// A copy of `origin` re-stamped to a DIFFERENT persona — used by the off-domain
/// guard to create a re-routed task under the domain owner while keeping the
/// chat/requester the ask arrived with, so the lifecycle loop still reports back
/// to the right conversation.
fn origin_as_persona(
    origin: &crate::graph::TaskOrigin,
    persona: &str,
) -> crate::graph::TaskOrigin {
    let mut owned = origin.clone();
    owned.persona = persona.trim().to_string();
    owned
}

/// True when the speaking voice in `origin` IS the domain `owner` — so it must
/// never defer to itself (morning-taco-bugs). Matches on the persona id, and,
/// because a bot with no configured `agent_id` stamps its bot id as the persona,
/// also on the bot id — treating a `"bruno"` owner as the speaker behind
/// `"bruno"`, `"bruno_casapinello_bot"`, or `"bruno-bot"`.
fn speaker_is_owner(origin: &crate::graph::TaskOrigin, owner: &str) -> bool {
    let owner = owner.trim().to_ascii_lowercase();
    if owner.is_empty() {
        return false;
    }
    let matches_owner = |id: &str| {
        let id = id.trim().to_ascii_lowercase();
        id == owner
            || id.starts_with(&format!("{owner}_"))
            || id.starts_with(&format!("{owner}-"))
    };
    matches_owner(&origin.persona)
        || origin.bot_id.as_deref().map(matches_owner).unwrap_or(false)
}

/// Persist a standing preference to the durable store under the project's
/// `.casa/`, best-effort (a write failure must never block the reply).
fn record_standing_preference(
    workgraph_dir: &Path,
    text: &str,
    origin: &crate::graph::TaskOrigin,
) {
    let root = project_root_of(workgraph_dir);
    match parity::PreferenceStore::record(&root, text, &origin.requester, &origin.persona) {
        Ok(_) => println!(
            "[{}] conversation recorded standing preference (chat {})",
            chrono::Utc::now().format("%H:%M:%S"),
            origin.chat_id,
        ),
        Err(e) => eprintln!(
            "[{}] failed to record standing preference: {e}",
            chrono::Utc::now().format("%H:%M:%S"),
        ),
    }
}

/// The project root that owns `.casa/`: the parent of a `.wg`/`.workgraph`
/// state dir, else the dir itself. Mirrors `commands::telegram::project_root`.
fn project_root_of(workgraph_dir: &Path) -> PathBuf {
    match workgraph_dir.file_name().and_then(|n| n.to_str()) {
        Some(".wg") | Some(".workgraph") => workgraph_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| workgraph_dir.to_path_buf()),
        _ => workgraph_dir.to_path_buf(),
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
    /// assert *which bot* replied *in which chat* with *what text*. Sends return
    /// a monotonic fake message id so the ack-edit path is exercisable, and
    /// edits are recorded separately so tests can assert the ack was turned INTO
    /// the final answer rather than left as a second message.
    #[derive(Default)]
    struct RecSink {
        sent: Mutex<Vec<(String, String, String)>>,
        edited: Mutex<Vec<(String, String, String, String)>>,
        next_id: Mutex<u64>,
    }
    #[async_trait]
    impl ReplySink for RecSink {
        async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
            self.sent
                .lock()
                .unwrap()
                .push((bot_id.to_string(), chat_id.to_string(), text.to_string()));
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            Ok(Some(n.to_string()))
        }
        async fn edit(
            &self,
            bot_id: &str,
            chat_id: &str,
            message_id: &str,
            text: &str,
        ) -> Result<()> {
            self.edited.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                message_id.to_string(),
                text.to_string(),
            ));
            Ok(())
        }
    }
    impl RecSink {
        fn calls(&self) -> Vec<(String, String, String)> {
            self.sent.lock().unwrap().clone()
        }
        fn edits(&self) -> Vec<(String, String, String, String)> {
            self.edited.lock().unwrap().clone()
        }
    }

    /// Fake composer for the round-trip / failure / slow tests — no live model.
    struct FakeComposer {
        reply: Result<String, String>,
        delay: Duration,
    }
    impl FakeComposer {
        fn ok(text: &str) -> Self {
            Self {
                reply: Ok(text.to_string()),
                delay: Duration::ZERO,
            }
        }
        fn ok_after(text: &str, delay: Duration) -> Self {
            Self {
                reply: Ok(text.to_string()),
                delay,
            }
        }
        fn fail(msg: &str) -> Self {
            Self {
                reply: Err(msg.to_string()),
                delay: Duration::ZERO,
            }
        }
    }
    #[async_trait]
    impl ReplyComposer for FakeComposer {
        async fn compose(
            &self,
            _workgraph_dir: &Path,
            _session_ref: &str,
            _agent_id: &str,
            _human_message: &str,
        ) -> Result<String> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            match &self.reply {
                Ok(t) => Ok(t.clone()),
                Err(e) => anyhow::bail!("{e}"),
            }
        }
    }

    /// A composer that returns a different reply on each successive call, so the
    /// parity retry path (first turn promises, second turn is forced to create
    /// the task — or stubbornly refuses again) is provable. The last reply is
    /// reused if called more times than it has entries.
    struct SequenceComposer {
        replies: Vec<String>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl SequenceComposer {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: replies.iter().map(|s| s.to_string()).collect(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ReplyComposer for SequenceComposer {
        async fn compose(
            &self,
            _workgraph_dir: &Path,
            _session_ref: &str,
            _agent_id: &str,
            _human_message: &str,
        ) -> Result<String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = n.min(self.replies.len().saturating_sub(1));
            Ok(self.replies[idx].clone())
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

    /// Write a minimal agency agent yaml (id + name) so `canonical_agent_id`
    /// can resolve a roster NAME to the agent's canonical id — mirrors the real
    /// `agency/cache/agents/<id>.yaml` shape.
    fn write_agent(wg: &Path, id: &str, name: &str) {
        let dir = wg.join("agency").join("cache/agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.yaml")),
            format!(
                "id: {id}\nrole_id: role-x\ntradeoff_id: mot-x\nname: {name}\n\
                 performance:\n  task_count: 0\n  avg_score: 0.0\n"
            ),
        )
        .unwrap();
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

    /// THE dedupe-key-fix converse-hang regression. The Casa reality: the otto
    /// persona's canonical agency id is a 64-hex hash, its human-friendly name
    /// is "otto", and `wg agent session` bound its session under the CANONICAL
    /// id. The roster (`agent_for_bot`) addresses the persona by NAME, so the
    /// pre-fix `session_for_agent("otto")` missed the id-keyed binding entirely
    /// (→ Sessionless / a stray-session hang). Canonicalising the handle first
    /// lands the lookup on the real bound session.
    #[test]
    fn roster_name_resolves_to_canonical_bound_session() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let canonical = "c10fe2fb2e60fe5208d8a10c39c1582151eab62c64c934431146e6a054e3d2a4";
        write_agent(wg, canonical, "otto");
        let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
        // `wg agent session <canonical_id>` binds under the full id, NOT "otto".
        bind_agent(wg, canonical, &uuid).unwrap();

        // The resolver: name → canonical id, id-prefix → canonical id, and an
        // unknown handle falls through unchanged (bot with no agency agent).
        assert_eq!(canonical_agent_id(wg, "otto"), canonical);
        assert_eq!(canonical_agent_id(wg, "OTTO"), canonical, "case-insensitive");
        assert_eq!(canonical_agent_id(wg, "c10fe2fb"), canonical, "id prefix");
        assert_eq!(canonical_agent_id(wg, "ghost"), "ghost", "unknown falls through");

        // The plan lands on the canonical-bound session — Converse, not the
        // pre-fix Sessionless miss.
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        confirm_human(wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        match plan {
            ConversationPlan::Converse { session_ref, .. } => assert_eq!(session_ref, uuid),
            other => panic!("expected Converse via the canonical-bound session, got {other:?}"),
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
            None,
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
            run_conversation_turn(&wg, &plan, "bruno what's for dinner?", "req-2", fast_timing(), None, &sink)
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
    async fn composed_group_turn_replies_via_elected_bot_not_the_concierge() {
        // BUG 3 (2026-07-12): a group-elected conversational reply rendered as
        // Otto in the Telegram bubble even though the election target was bruno.
        // The LIVE path is the COMPOSER (a one-shot claude spawn), not the legacy
        // outbox poll — and it had no group-elected coverage. This proves every
        // send the composed turn makes (ack + final answer) goes out with the
        // ELECTED persona's bot id, never the default/concierge bot, exactly like
        // the 1:1 path sends via the messaged bot.
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        // otto is first (the concierge / default reply bot); bruno is elected.
        let cfg = cfg_with_bots(&[("otto", Some("otto")), ("bruno", Some("bruno"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "bruno", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan =
            plan_conversation(&wg, &cfg, "telegram:bruno", "-100777", "luca-1", Entry::GroupElected);
        // Sanity: the plan itself routed to bruno.
        assert_eq!(plan.route().bot_id, "bruno");

        let sink = RecSink::default();
        // A slow compose so the latency ack fires too — assert IT is bruno as well.
        let composer = FakeComposer::ok_after("Dinner's at seven.", Duration::from_millis(150));
        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "can you all weigh in on dinner?",
            "req-grp",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, TurnOutcome::Replied { acked: true }));
        // EVERY send (the ack) went out via bruno, in the group — never otto.
        for (bot, chat_id, _text) in sink.calls() {
            assert_eq!(bot, "bruno", "composed group reply must send via the ELECTED bot");
            assert_eq!(chat_id, "-100777", "composed group reply lands in the GROUP");
        }
        // The final answer edits the ack in place — also via bruno.
        for (bot, chat_id, _mid, text) in sink.edits() {
            assert_eq!(bot, "bruno", "the final answer edit must also use the ELECTED bot");
            assert_eq!(chat_id, "-100777");
            assert_eq!(text, "Dinner's at seven.");
        }
        // The elected bot's token is distinct from the concierge's, so a wrong-bot
        // send would have surfaced a different token — pin the mapping explicitly.
        let bruno_token = cfg.all_bots().into_iter().find(|(id, _)| id == "bruno").unwrap().1.bot_token;
        assert_eq!(bruno_token, "token-bruno");
        assert_ne!(
            bruno_token,
            cfg.all_bots().into_iter().find(|(id, _)| id == "otto").unwrap().1.bot_token,
            "bruno and otto must carry distinct tokens for this test to be meaningful"
        );
    }

    /// ONE REPLY PER TURN (Luca, 2026-07-17): Otto posted two messages
    /// back-to-back for a single ask. A re-fire of the SAME turn (same
    /// `request_id`) — a listener re-poll, a gateway retry, a restart-replay —
    /// must NOT compose or send a second time. The first turn answers; the
    /// second is a no-op, so the human sees exactly one message.
    #[tokio::test]
    async fn same_request_id_never_double_posts() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = FakeComposer::ok("Yeah, today's Friday the 17th.");

        // First delivery of the turn.
        let out1 = run_conversation_turn(
            &wg, &plan, "what day is it?", "req-dup", fast_timing(), Some(&composer), &sink,
        )
        .await
        .unwrap();
        assert!(matches!(out1, TurnOutcome::Replied { .. }));

        // Same request id fires again (the double-post trigger).
        let out2 = run_conversation_turn(
            &wg, &plan, "what day is it?", "req-dup", fast_timing(), Some(&composer), &sink,
        )
        .await
        .unwrap();
        assert!(matches!(out2, TurnOutcome::Replied { .. }));

        // Exactly ONE human-visible message across BOTH invocations: the fast
        // compose sends no ack, so the total send count is one and there are no
        // edits. The second turn produced nothing.
        assert_eq!(
            sink.calls().len(),
            1,
            "one turn must post one message; a re-fired request must not double-post: {:?}",
            sink.calls()
        );
        assert!(
            sink.edits().is_empty(),
            "no ack/edit expected on a fast turn: {:?}",
            sink.edits()
        );
        assert_eq!(sink.calls()[0].0, "otto");
    }

    /// A 1:1 ask that the persona turns into work stamps the created task with
    /// its ORIGIN (channel, chat, requester, persona) and never leaks the
    /// `TASK_CREATE:` directive into the reply the human sees. This is the birth
    /// of the loop: without the stamp there is no way to report back.
    #[tokio::test]
    async fn lifecycle_task_create_stamps_origin_and_strips_the_tail() {
        use crate::graph::OriginChannel;
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        // A meals ask is owned by Nora (meals & nutrition); the origin-stamp
        // machinery is identical on the owner path, so drive it as her.
        let cfg = cfg_with_bots(&[("nora", Some("nora"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "nora");

        let plan = plan_conversation(&wg, &cfg, "telegram:nora", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = FakeComposer::ok(
            "On it — I'll get the week tweaked.\nTASK_CREATE: tweak this week's meals",
        );

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "carbonara Wednesday, eggs Tuesday, fish Saturday lunch please",
            "req-tc",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, TurnOutcome::Replied { .. }));

        // The human sees the warm reply, never the machine directive.
        let (_bot, _chat, text) = sink.calls().last().unwrap().clone();
        assert_eq!(text, "On it — I'll get the week tweaked.");
        assert!(!text.contains("TASK_CREATE"));

        // A real, origin-stamped task now exists in the graph.
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let created = graph
            .tasks()
            .find(|t| t.origin.is_some())
            .expect("a stamped task was created");
        assert_eq!(created.title, "tweak this week's meals");
        let o = created.origin.as_ref().unwrap();
        assert_eq!(o.channel, OriginChannel::TelegramDirect);
        assert_eq!(o.chat_id, "555");
        assert_eq!(o.requester, "Luca");
        assert_eq!(o.persona, "nora");
        assert_eq!(o.bot_id.as_deref(), Some("nora"));
    }

    /// PARITY, retry path: the salad regression. The composer's FIRST reply
    /// promises action ("I'll add the salad…") but emits NO `TASK_CREATE:` tail,
    /// so nothing would be created. The post-turn audit catches the mismatch,
    /// retries ONCE, and the second reply carries the directive → a real task
    /// now exists and the human sees the confirming reply (never the directive).
    #[tokio::test]
    async fn promise_without_artifact_retries_then_creates_task() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        // Meals → Nora owns it; the parity retry runs on the owner path.
        let cfg = cfg_with_bots(&[("nora", Some("nora"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "nora");

        let plan = plan_conversation(&wg, &cfg, "telegram:nora", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        // 1st: a bare promise. 2nd (forced retry): the same promise WITH the tail.
        let composer = SequenceComposer::new(&[
            "Sure! I'll add the salad to the list I'm sending Nora and Bruno.",
            "On it — adding it now.\nTASK_CREATE: add a green salad to Monday dinner",
        ]);

        run_conversation_turn(
            &wg,
            &plan,
            "add a green salad to Monday dinner and pass it to Nora and Bruno",
            "req-parity-1",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        // The composer was retried exactly once (2 calls total).
        assert_eq!(composer.call_count(), 2, "expected one retry");

        // A real, origin-stamped task now exists — parity restored.
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let created = graph
            .tasks()
            .find(|t| t.origin.is_some())
            .expect("the retry created a stamped task");
        assert_eq!(created.title, "add a green salad to Monday dinner");

        // The human sees the confirming reply, never the machine directive.
        let (_bot, _chat, text) = sink.calls().last().cloned().unwrap_or_default();
        // Delivery may edit the ack in place; check both channels for the reply.
        let last = sink
            .edits()
            .last()
            .map(|e| e.3.clone())
            .unwrap_or(text);
        assert!(!last.contains("TASK_CREATE"), "directive leaked: {last}");
        assert!(!last.to_lowercase().contains("snag"), "no correction expected: {last}");
    }

    /// PARITY, fallback path: a stubborn composer promises action on BOTH the
    /// first turn and the forced retry, never emitting the directive. The ask
    /// must still not be lost: the system creates a fallback task from the
    /// promise text AND appends an honest correction to the SAME chat.
    #[tokio::test]
    async fn promise_survives_stubborn_composer_via_fallback_task_and_correction() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        // Meals → Nora owns it; the fallback path runs on the owner path.
        let cfg = cfg_with_bots(&[("nora", Some("nora"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "nora");

        let plan = plan_conversation(&wg, &cfg, "telegram:nora", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        // Both replies promise but NEVER emit the tail.
        let composer = SequenceComposer::new(&[
            "Absolutely, I'll add the salad to the list.",
            "Yep, adding it right now!",
        ]);

        run_conversation_turn(
            &wg,
            &plan,
            "add a green salad to Monday dinner",
            "req-parity-2",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(composer.call_count(), 2, "expected exactly one retry");

        // A fallback task carrying the ask now exists — the ask was not lost.
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let created = graph
            .tasks()
            .find(|t| t.origin.is_some())
            .expect("a fallback task was created");
        assert!(
            created.title.contains("add a green salad to Monday dinner"),
            "fallback title should carry the ask: {}",
            created.title
        );

        // The human got an honest correction in the same chat.
        let last = sink
            .edits()
            .last()
            .map(|e| e.3.clone())
            .or_else(|| sink.calls().last().map(|c| c.2.clone()))
            .unwrap();
        assert!(last.to_lowercase().contains("snag"), "expected correction: {last}");
    }

    /// PARITY, no false positive: a purely non-committal reply (no promise)
    /// creates NO task — the audit must not manufacture work from small talk.
    #[tokio::test]
    async fn non_committal_reply_creates_no_task() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = SequenceComposer::new(&["Dinner's at seven, see you there!"]);

        run_conversation_turn(
            &wg,
            &plan,
            "what time is dinner?",
            "req-parity-3",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        // No retry, no task.
        assert_eq!(composer.call_count(), 1, "no retry for a non-committal reply");
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).ok();
        let any_task = graph.map(|g| g.tasks().next().is_some()).unwrap_or(false);
        assert!(!any_task, "no task should be created for small talk");
    }

    /// Bind a persona's bot + session and (idempotently) confirm the human, so a
    /// `GroupElected` plan for that voice resolves to `Converse`. Returns the
    /// four-bot config the collective tests share.
    fn setup_collective(wg: &Path) -> TelegramConfig {
        let cfg = cfg_with_bots(&[
            ("nora", Some("nora")),
            ("bruno", Some("bruno")),
            ("mira", Some("mira")),
            ("otto", Some("otto")),
        ]);
        for persona in ["nora", "bruno", "mira", "otto"] {
            let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(wg, persona, &uuid).unwrap();
        }
        confirm_human(wg, "luca-1", "human-luca", "otto");
        cfg
    }

    /// Run one collective voice's turn: it emits a reply with a `TASK_CREATE`
    /// tail (each voice, as in the live bug, *would* create its own copy).
    async fn run_voice(
        wg: &Path,
        cfg: &TelegramConfig,
        persona: &str,
        chat: &str,
        ask: &str,
        reply_with_tail: &str,
        sink: &RecSink,
    ) {
        let plan = plan_conversation(
            wg,
            cfg,
            &format!("telegram:{persona}"),
            chat,
            "luca-1",
            Entry::GroupElected,
        );
        let composer = FakeComposer::ok(reply_with_tail);
        run_conversation_turn(
            wg,
            &plan,
            ask,
            &format!("req-collective-{persona}"),
            fast_timing(),
            Some(&composer),
            sink,
        )
        .await
        .unwrap();
    }

    /// THE REGRESSION FIXTURE (Luca, 2026-07-13). A single collective ask ("swap
    /// Thursday dinner to grilled tofu") elects the WHOLE roster; each voice
    /// composes a reply that would create its own task — exactly the path that
    /// minted FOUR duplicates, one per persona, including Coach Mira (workouts)
    /// taking on a cooking task. With the single-owner rule + intent dedupe,
    /// exactly ONE task survives, owned by Nora (the dietitian — her domain), and
    /// the off-domain voices defer out loud.
    #[tokio::test]
    async fn collective_tofu_ask_creates_exactly_one_task_owned_by_nora() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = setup_collective(&wg);
        let chat = "-100999";
        let ask = "swap Thursday dinner to grilled tofu";

        // Roster order: nora (owner) runs first, then three off-domain voices —
        // each with a DIFFERENT title to prove dedupe keys on the ASK, not the
        // title. Mira's would have been the impossible "add grilled tofu" card.
        let nora_sink = RecSink::default();
        run_voice(&wg, &cfg, "nora", chat, ask,
            "Grilled tofu Thursday it is 🥗\nTASK_CREATE: swap Thursday dinner to grilled tofu",
            &nora_sink).await;
        let bruno_sink = RecSink::default();
        run_voice(&wg, &cfg, "bruno", chat, ask,
            "Sounds tasty!\nTASK_CREATE: prep grilled tofu for Thursday", &bruno_sink).await;
        let mira_sink = RecSink::default();
        run_voice(&wg, &cfg, "mira", chat, ask,
            "Nice protein swap.\nTASK_CREATE: add grilled tofu", &mira_sink).await;
        let otto_sink = RecSink::default();
        run_voice(&wg, &cfg, "otto", chat, ask,
            "Noted!\nTASK_CREATE: put tofu on the Thursday plan", &otto_sink).await;

        // Exactly ONE task exists, and it is Nora's.
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(
            stamped.len(),
            1,
            "one ask, one task — got {}: {:?}",
            stamped.len(),
            stamped.iter().map(|t| &t.title).collect::<Vec<_>>()
        );
        let owner = stamped[0].origin.as_ref().unwrap();
        assert_eq!(owner.persona, "nora", "the meal-plan owner (dietitian) owns it");
        assert_eq!(stamped[0].title, "swap Thursday dinner to grilled tofu");

        // Coach Mira never owns a cooking task — the impossible card is impossible.
        assert!(
            !graph.tasks().any(|t| t.origin.as_ref().map(|o| o.persona.as_str()) == Some("mira")),
            "Coach Mira must never own a meals/cooking task"
        );

        // The off-domain voices defer out loud so the ask visibly lands with Nora.
        let mira_last = mira_sink.calls().last().map(|c| c.2.clone()).unwrap_or_default();
        assert!(
            mira_last.contains("Nora"),
            "an off-domain voice should defer to the owner by name, got: {mira_last:?}"
        );
    }

    /// THE ROUND-2 CHOKE-POINT INVARIANT (fails before the fix). Every
    /// conversationally created task funnels through [`try_create_origin_task`];
    /// this proves that layer is AUTHORITATIVE for ownership regardless of which
    /// persona the caller stamped or which election shape produced the turn. Otto
    /// (off-domain for meals) tries to create a carbonara task directly — the exact
    /// single-voice/concierge shape Luca hit ("why is otto dealing with dishes").
    /// Before the fix the choke point trusted the caller's persona and Otto owned
    /// the dish; after it, the task lands owned by Nora.
    #[test]
    fn choke_point_restamps_off_domain_meal_task_to_owner_nora() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let origin = crate::graph::TaskOrigin::new(
            crate::graph::OriginChannel::TelegramGroup,
            "-100555",
            "Luca",
            "otto",
            Some("otto".to_string()),
        );
        let id = try_create_origin_task(
            &wg,
            "update Friday dinner to carbonara instead",
            "update Friday dinner to carbonara",
            &origin,
        );
        assert!(id.is_some(), "the ask must not be dropped");
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(stamped.len(), 1, "one ask, one task");
        assert_eq!(
            stamped[0].origin.as_ref().unwrap().persona,
            "nora",
            "the creation choke point re-stamps an off-domain meal task to its owner (Nora)"
        );
    }

    /// The choke point leaves an ON-domain creation untouched: Otto creating a
    /// coordination task (his domain) stays owned by Otto — the guard re-routes
    /// only OFF-domain asks, never hijacks a legitimate one.
    #[test]
    fn choke_point_leaves_on_domain_owner_untouched() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let origin = crate::graph::TaskOrigin::new(
            crate::graph::OriginChannel::TelegramGroup,
            "-100556",
            "Luca",
            "otto",
            Some("otto".to_string()),
        );
        let id = try_create_origin_task(
            &wg,
            "who is picking up the kids on Friday?",
            "arrange Friday kid pickup",
            &origin,
        );
        assert!(id.is_some());
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(stamped.len(), 1);
        assert_eq!(
            stamped[0].origin.as_ref().unwrap().persona,
            "otto",
            "an on-domain (coordination) task stays with its owner"
        );
    }

    /// THE ROUND-2 REGRESSION FIXTURE (Luca, 2026-07-14): Otto answers a meal ask
    /// 1:1-style in the group — a single [`Election::One`] turn, NOT a collective
    /// round — and the composer emits a `TASK_CREATE` tail. The created task must
    /// be owned by Nora (the meal owner), not the answering voice (Otto). This
    /// drives the full single-voice production path (plan → compose → finalize →
    /// choke point), the human-flow analog of the constellation bug.
    #[tokio::test]
    async fn single_voice_otto_meal_ask_owner_is_nora() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = setup_collective(&wg);
        let chat = "-100777";
        let ask = "update Friday dinner to carbonara instead";
        let sink = RecSink::default();
        run_voice(
            &wg, &cfg, "otto", chat, ask,
            "On it!\nTASK_CREATE: update Friday dinner to carbonara",
            &sink,
        )
        .await;
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(stamped.len(), 1, "one ask, one task");
        assert_eq!(
            stamped[0].origin.as_ref().unwrap().persona,
            "nora",
            "single-voice meal ask must be owned by Nora, not Otto"
        );
    }

    /// The single-voice OWNER matrix: one concierge (Otto) turn per domain, each
    /// creating a task, asserting ownership lands on the DOMAIN owner — meals →
    /// Nora, recipes → Bruno, workouts → Mira, calendar/shopping/coordination →
    /// Otto. This is the single-voice sibling of the collective stampede matrix:
    /// the guard is election-shape agnostic.
    #[tokio::test]
    async fn single_voice_owner_matrix_routes_each_domain() {
        // (ask, task title, expected owner). One Otto turn per row, separate chats
        // so the intent ledger never cross-dedupes distinct asks.
        let cases: &[(&str, &str, &str)] = &[
            ("update Friday dinner to carbonara", "update Friday dinner", "nora"),
            ("what's a good recipe for the tofu?", "share a tofu recipe", "bruno"),
            ("can we move my gym session to Friday?", "reschedule gym to Friday", "mira"),
            ("book a dentist appointment next week", "book the dentist", "otto"),
            ("add oat milk to the shopping list", "add oat milk", "otto"),
            ("who is picking up the kids?", "arrange kid pickup", "otto"),
        ];
        for (i, (ask, title, expected)) in cases.iter().enumerate() {
            let dir = tempdir().unwrap();
            let wg = dir.path().to_path_buf();
            let cfg = setup_collective(&wg);
            let chat = format!("-1006{i:02}");
            let sink = RecSink::default();
            run_voice(
                &wg, &cfg, "otto", &chat, ask,
                &format!("On it!\nTASK_CREATE: {title}"),
                &sink,
            )
            .await;
            let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
            let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
            assert_eq!(stamped.len(), 1, "ask {ask:?}: exactly one task");
            assert_eq!(
                stamped[0].origin.as_ref().unwrap().persona,
                *expected,
                "single-voice ask {ask:?} must be owned by {expected}",
            );
        }
    }

    /// The off-domain re-route creates the task under the OWNER even when the
    /// owner is not first in roster order: a collective workout ask ("can we all
    /// move my gym session to Friday?") has owner Mira, who runs third. Nora
    /// (first, off-domain) re-routes and creates it stamped as Mira; the later
    /// voices — Mira included — dedupe against it. Net: one task, owned by Mira.
    #[tokio::test]
    async fn collective_off_domain_reroutes_to_owner_run_last() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = setup_collective(&wg);
        let chat = "-100888";
        let ask = "can we all move my gym session to Friday?";

        for (persona, reply) in [
            ("nora", "I'll flag it.\nTASK_CREATE: move the gym session to Friday"),
            ("bruno", "Sure.\nTASK_CREATE: shift gym to Friday"),
            ("mira", "On it — Friday works 💪\nTASK_CREATE: reschedule gym session to Friday"),
            ("otto", "Noted.\nTASK_CREATE: gym Friday"),
        ] {
            let sink = RecSink::default();
            run_voice(&wg, &cfg, persona, chat, ask, reply, &sink).await;
        }

        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(stamped.len(), 1, "one workout ask, one task");
        assert_eq!(
            stamped[0].origin.as_ref().unwrap().persona,
            "mira",
            "the workout owner owns it, even created by an earlier off-domain voice"
        );
    }

    /// PARITY, standing preference: "remember we work Mon–Fri" writes to the
    /// durable preference store (not a one-off task) so the weekly draft can
    /// read it every week.
    #[tokio::test]
    async fn standing_preference_is_recorded_durably() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        // A meal-planning standing rule is Nora's domain; it is recorded once, by
        // the owner, rather than duplicated across a collective.
        let cfg = cfg_with_bots(&[("nora", Some("nora"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "nora");

        let plan = plan_conversation(&wg, &cfg, "telegram:nora", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer =
            SequenceComposer::new(&["Got it — from now on, no weekday lunches. We work Mon-Fri."]);

        run_conversation_turn(
            &wg,
            &plan,
            "remember we work Monday to Friday — no weekday lunches",
            "req-parity-4",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        // The preference is durably recorded under the project's .casa/.
        let root = dir.path();
        let prefs = parity::PreferenceStore::all(root);
        assert_eq!(prefs.len(), 1, "one preference recorded");
        assert!(prefs[0].text.to_lowercase().contains("no weekday lunches"));
        assert_eq!(prefs[0].requester, "Luca");
        assert_eq!(prefs[0].persona, "nora");

        // A preference is NOT a one-off task.
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).ok();
        let any_task = graph.map(|g| g.tasks().next().is_some()).unwrap_or(false);
        assert!(!any_task, "a standing preference must not create a task");
    }

    /// "Are they done yet?" is answered from LIVE graph state — the in-progress
    /// task's status — not by spinning up the model. The FakeComposer here would
    /// return a wrong answer if it were called, so a correct status line proves
    /// the short-circuit.
    #[tokio::test]
    async fn lifecycle_status_question_answers_from_graph_not_the_model() {
        use crate::graph::{Node, OriginChannel, Status, Task, TaskOrigin, WorkGraph};
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        // Seed an in-progress task Luca asked for.
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "tweak-w29-meals".into(),
            title: "tweak this week's meals".into(),
            status: Status::InProgress,
            origin: Some(TaskOrigin::new(
                OriginChannel::TelegramDirect,
                "555",
                "Luca",
                "otto",
                Some("otto".into()),
            )),
            ..Default::default()
        }));
        crate::parser::save_graph(&graph, wg.join("graph.jsonl")).unwrap();

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = FakeComposer::ok("WRONG — the model should not be consulted here.");

        run_conversation_turn(
            &wg,
            &plan,
            "hey are they done yet?",
            "req-st",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        let (_bot, _chat, text) = sink.calls().last().unwrap().clone();
        assert!(text.contains("on it now"), "status answer, got: {text}");
        assert!(!text.contains("WRONG"), "must not use the model: {text}");
    }

    /// An explicit "let me know when…" is acknowledged out loud, appended to the
    /// composed reply (the payoff itself arrives later via the Done notification).
    #[tokio::test]
    async fn lifecycle_follow_request_appends_the_ack() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = FakeComposer::ok("Sure — I'll get it sorted.");

        run_conversation_turn(
            &wg,
            &plan,
            "tweak the week, and let me know when they are done",
            "req-fl",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        let (_bot, _chat, text) = sink.calls().last().unwrap().clone();
        assert!(text.starts_with("Sure — I'll get it sorted."), "{text}");
        assert!(text.contains(lifecycle::FOLLOW_ACK), "follow ack appended: {text}");
    }

    #[test]
    fn resolve_mentioned_bot_needs_a_real_handle_no_false_positive_on_prose() {
        // BUG 3 side-investigation: msg=70 logged rule=mention target=bruno for a
        // message the screenshot shows had no @mention. The mention rule fires only
        // from parsed @mention entities; resolve_mentioned_bot itself matches a
        // bot by @username / bot_id / agent_id / compound handle segment — NOT by a
        // bare persona word buried in prose. So plain sentences that merely say
        // "bruno" as a word do not resolve here (the addressed-NAME rule handles
        // vocatives separately); only an actual handle does.
        use crate::notify::telegram_group::resolve_mentioned_bot;
        let cfg = cfg_with_bots(&[("otto", Some("otto")), ("bruno", Some("bruno"))]);
        // A real handle resolves.
        assert_eq!(
            resolve_mentioned_bot("bruno_bot", &cfg).map(|b| b.bot_id),
            Some("bruno".to_string()),
        );
        assert_eq!(
            resolve_mentioned_bot("@bruno", &cfg).map(|b| b.bot_id),
            Some("bruno".to_string()),
        );
        // Prose words that are NOT a configured handle do not resolve.
        for prose in ["dinner", "meeting", "brunobrunch", "the", "everyone"] {
            assert!(
                resolve_mentioned_bot(prose, &cfg).is_none(),
                "plain word {prose:?} must not resolve to a bot"
            );
        }
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
            run_conversation_turn(&wg, &plan, "you there?", "req-3", fast_timing(), None, &sink)
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
        let outcome = run_conversation_turn(&wg, &plan, "hello?", "req-4", timing, None, &sink)
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
        let outcome = run_conversation_turn(&wg, &plan, "hi", "req-5", fast_timing(), None, &sink)
            .await
            .unwrap();
        assert_eq!(outcome, TurnOutcome::Onboarded);
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "onboarding is one line, nothing more");
        assert_eq!(calls[0].0, "otto");
        assert_eq!(calls[0].1, "555");
    }

    /// Build a `Converse` plan bound to a fresh session for the composer tests.
    fn converse_fixture(wg: &Path) -> (TelegramConfig, ConversationPlan) {
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(wg, "otto", &uuid).unwrap();
        confirm_human(wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        assert!(matches!(plan, ConversationPlan::Converse { .. }));
        (cfg, plan)
    }

    /// The core fix: with a composer injected, a converse turn COMPLETES with the
    /// composer's real answer within the timeout — no open-loop outbox poll, no
    /// 120s hang. This is what the old inbox/outbox-only path could never do in
    /// production (no daemon produced the outbox reply).
    #[tokio::test]
    async fn converse_composes_and_relays_reply_within_timeout() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let (_cfg, plan) = converse_fixture(&wg);
        let sink = RecSink::default();
        let composer = FakeComposer::ok("Yep — dinner's at seven, see you there!");

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "are we still on for dinner?",
            "req-c1",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: false });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "fast compose sends exactly the answer");
        assert_eq!(calls[0].0, "otto");
        assert_eq!(calls[0].1, "555");
        assert_eq!(calls[0].2, "Yep — dinner's at seven, see you there!");
        // The composed reply is also persisted to the outbox for TUI/feed parity.
        if let ConversationPlan::Converse { session_ref, .. } = &plan {
            let out = chat::read_outbox_since_ref(&wg, session_ref, 0).unwrap();
            assert_eq!(out.last().unwrap().content, "Yep — dinner's at seven, see you there!");
        }
    }

    /// Induced failure: the composer errors (the production analogue is the
    /// `claude` child dying / non-zero exit / auth failure). The human gets the
    /// graceful "glitched" follow-up fast — never a permanent hourglass, never
    /// silence.
    #[tokio::test]
    async fn compose_failure_sends_glitch_follow_up_fast() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let (_cfg, plan) = converse_fixture(&wg);
        let sink = RecSink::default();
        let composer = FakeComposer::fail("claude CLI exited 1: Invalid API key");

        let start = Instant::now();
        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "you there?",
            "req-c2",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert!(start.elapsed() < Duration::from_secs(1), "must fail fast, not hang");
        assert_eq!(outcome, TurnOutcome::Glitched { acked: false });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.contains("glitched"), "got: {:?}", calls[0].2);
    }

    /// A slow compose (past `ack_after`) sends the ack, then EDITS it in place
    /// into the final answer — one clean message, no stale hourglass + second
    /// message.
    #[tokio::test]
    async fn slow_compose_acks_then_edits_ack_into_answer() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let (_cfg, plan) = converse_fixture(&wg);
        let sink = RecSink::default();
        // fast_timing ack_after is 80ms; delay 200ms so the ack fires first.
        let composer = FakeComposer::ok_after("Here at last — all sorted!", Duration::from_millis(200));

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "any update?",
            "req-c3",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: true });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "only the ack is a fresh send");
        assert!(calls[0].2.contains("On it"), "first send is the ack");
        let ack_mid = "1".to_string(); // RecSink's first id
        let edits = sink.edits();
        assert_eq!(edits.len(), 1, "the answer edits the ack in place");
        assert_eq!(edits[0].2, ack_mid, "edit targets the ack's message id");
        assert_eq!(edits[0].3, "Here at last — all sorted!");
    }

    /// A slow compose that then FAILS: the ack is edited into the glitch line
    /// (not left hanging, not duplicated).
    #[tokio::test]
    async fn slow_compose_failure_edits_ack_into_glitch() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let (_cfg, plan) = converse_fixture(&wg);
        let sink = RecSink::default();
        let composer = FakeComposer {
            reply: Err("boom".to_string()),
            delay: Duration::from_millis(200),
        };

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "hello?",
            "req-c4",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Glitched { acked: true });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "only the ack was a fresh send");
        assert!(calls[0].2.contains("On it"));
        let edits = sink.edits();
        assert_eq!(edits.len(), 1);
        assert!(edits[0].3.contains("glitched"), "ack edited into glitch: {:?}", edits[0].3);
    }

    /// DEFER DISCIPLINE (morning-taco-bugs): the owner never defers to itself. The
    /// self-owner guard recognises the owner whether the turn stamped the persona
    /// as the roster id ("bruno") or fell back to the bot id
    /// ("bruno_casapinello_bot" / "bruno-bot"); a genuinely different voice
    /// (Coach Mira) is NOT mistaken for the owner and still defers.
    #[test]
    fn speaker_is_owner_recognises_persona_and_bot_id_forms() {
        use crate::graph::{OriginChannel, TaskOrigin};
        let origin = |persona: &str, bot: Option<&str>| {
            TaskOrigin::new(
                OriginChannel::TelegramGroup,
                "-100999",
                "Luca",
                persona,
                bot.map(str::to_string),
            )
        };
        // Persona stamped as the roster id.
        assert!(speaker_is_owner(&origin("bruno", None), "bruno"));
        assert!(speaker_is_owner(&origin("Bruno", None), "bruno"));
        // Persona fell back to the bot id (no configured agent_id).
        assert!(speaker_is_owner(&origin("bruno_casapinello_bot", None), "bruno"));
        assert!(speaker_is_owner(&origin("bruno-bot", None), "bruno"));
        // Owner recognised via the bot_id channel even when persona is a bot id.
        assert!(speaker_is_owner(
            &origin("bruno_casapinello_bot", Some("bruno_casapinello_bot")),
            "bruno"
        ));
        // A different voice is NOT the owner — it still defers.
        assert!(!speaker_is_owner(&origin("mira", Some("mira_casapinello_bot")), "bruno"));
        assert!(!speaker_is_owner(&origin("otto", None), "bruno"));
        // No owner resolved → never a self-owner.
        assert!(!speaker_is_owner(&origin("bruno", None), ""));
    }

    // -----------------------------------------------------------------------
    // otto-answers-like: grounded, non-repetitive, corrigible conversation.
    //
    // The regression fixture is Luca's 2026-07-15 transcript: he asked "Plans
    // for tomorrow?" and Otto stalled four times with a near-identical "meals
    // set, waiting on confirmations from you and Nadin, want the rundown?" —
    // never reading the plan — even after a correction ("Nadin is not logged so
    // ignore this"), until commanded "You need to read the calendar". These
    // tests prove the composer now grounds turn one, refuses to repeat itself,
    // and honours corrections.
    // -----------------------------------------------------------------------

    const W29_FIXTURE_PLAN: &str = "\
# 2026-W29 Family Plan

**Week of Monday 2026-07-13 to Sunday 2026-07-19**
**Status:** DRAFT

## 1. Meals

| Day | Slot | Dinner | Prep |
|-----|------|--------|------|
| Mon 07-13 | Vegetarian | Chickpea & spinach curry, brown rice | ~35 min |
| Tue 07-14 | Fish | Baked salmon, roasted potatoes, green beans | ~30 min |
| Wed 07-15 | Flex | Leftovers | ~10 min |

## 3. Calendar

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Tue 07-14 | 19:30 | Luca PT check-in | Otto |
| Thu 07-16 | 09:00 | Dentist — Nadin | Otto |
";

    /// RULE 1 (grounding): a read-shaped ask about the plan pulls the REAL week
    /// model into the compose prompt on turn one — the fix for four ungrounded
    /// stalls. Chit-chat is left un-bloated.
    #[test]
    fn ground_compose_prompt_injects_the_week_model_for_read_shaped_asks() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), W29_FIXTURE_PLAN).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();

        // Asked on Wed 07-15 at noon (fixed clock). "Tomorrow" is Thu 07-16 —
        // the plan data must be present turn one, SCOPED to Thursday: the
        // Dentist appointment is in, and other days do NOT leak (rule 3).
        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let grounded =
            build_compose_prompt_at(&wg, &uuid, "otto", "Plans for tomorrow?", now, None);
        assert!(
            grounded.contains("Dentist"),
            "tomorrow's appointment missing from prompt:\n{grounded}"
        );
        assert!(grounded.contains("Thursday"));
        // The answer-first instruction header is present.
        assert!(grounded.to_lowercase().contains("answer the question directly"));
        // Other days must NOT bleed into a scoped "tomorrow" ask.
        assert!(!grounded.contains("Baked salmon"), "Tue meal leaked:\n{grounded}");
        assert!(!grounded.contains("Chickpea"), "Mon meal leaked:\n{grounded}");

        // A whole-week ask still surfaces the full week's meals.
        let week = build_compose_prompt_at(&wg, &uuid, "otto", "how's the week?", now, None);
        assert!(week.contains("Baked salmon"));
        assert!(week.contains("Luca PT check-in"));
        assert!(week.to_lowercase().contains("do not stall"));

        // Small talk carries no read-shaped WEEK block (no meal dump)...
        let plain = build_compose_prompt_at(&wg, &uuid, "otto", "morning!", now, None);
        assert!(!plain.contains("Baked salmon"));
        // ...but it DOES now carry the always-on anti-fabrication calendar-truth
        // line (rule 5, §6.7). Wed 07-15 has no calendar events → the model is
        // told the day is clear and forbidden from inventing one. This is the
        // root-cause fix: the calendar is in the context for EVERY message shape.
        assert!(
            plain.to_lowercase().contains("nothing on the calendar")
                && plain.to_lowercase().contains("do not invent"),
            "small talk missing the anti-fabrication calendar-truth line:\n{plain}"
        );

        // A greeting on a day that DOES have a real upcoming event names ONLY
        // that event (Tue 07-14 15:00 → the 19:30 PT check-in is still ahead).
        let tue_noon = chrono::NaiveDate::from_ymd_opt(2026, 7, 14)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap();
        let greet = build_compose_prompt_at(&wg, &uuid, "otto", "how's your day?", tue_noon, None);
        assert!(greet.contains("PT check-in"), "real event missing from greeting prompt:\n{greet}");
        assert!(greet.to_lowercase().contains("do not invent"), "{greet}");
    }

    /// THREAD CONTEXT (task nora-clarify-engine, fix 2): the gateway forwards the
    /// originating pane's recent turns via `WG_THREAD_CONTEXT`; the composer must
    /// inject them so a topic follow-up ("tell me the calories" right after a
    /// pasta-pomodoro nutrition line) is ANSWERED, not clarified. Without the
    /// context the ambiguous ask stands alone; with it, the referent is in the
    /// prompt AND the model is explicitly told not to ask what they mean.
    #[test]
    fn thread_context_is_injected_so_topic_followups_compose_not_clarify() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        let thread = "You: Tonight is pasta pomodoro — light, about 480 calories a plate.\n\
                      Human: nice";

        // WITH thread context: the referent ("pasta pomodoro", its calories) is in
        // the prompt, and the composer is told to resolve the follow-up and NOT
        // clarify when the topic is already clear.
        let followup = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "tell me the calories",
            now,
            Some(thread),
        );
        assert!(
            followup.contains("pasta pomodoro"),
            "thread context (the referent) missing from the compose prompt:\n{followup}"
        );
        assert!(
            followup.to_lowercase().contains("follow-up")
                && followup.to_lowercase().contains("do not ask what they mean"),
            "compose-not-clarify instruction missing from the prompt:\n{followup}"
        );

        // WITHOUT thread context: the same ambiguous ask carries no referent and
        // no follow-up instruction — this is exactly the state that made the engine
        // clarify instead of answer.
        let bare = build_compose_prompt_at(&wg, &uuid, "otto", "tell me the calories", now, None);
        assert!(!bare.contains("pasta pomodoro"), "referent leaked without a thread:\n{bare}");
        assert!(
            !bare.to_lowercase().contains("do not ask what they mean"),
            "follow-up instruction present without a thread:\n{bare}"
        );

        // An empty/whitespace thread is treated as absent (no stray block).
        let blank = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "tell me the calories",
            now,
            Some("   "),
        );
        assert!(!blank.to_lowercase().contains("do not ask what they mean"), "{blank}");
    }

    /// RULE 3 (corrections stick): "Nadin is not logged so ignore this" is
    /// persisted durably AND replayed into every subsequent compose prompt so
    /// the corrected claim is never repeated.
    #[tokio::test]
    async fn ground_correction_is_persisted_and_replayed() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = FakeComposer::ok("Got it — I won't count Nadin as logged.");

        run_conversation_turn(
            &wg,
            &plan,
            "Nadin is not logged so ignore this",
            "req-corr",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        // Durably recorded under .casa, tagged as a correction.
        let root = dir.path();
        let prefs = parity::PreferenceStore::all(root);
        assert!(
            prefs.iter().any(|p| p
                .text
                .starts_with(grounding::CORRECTION_PREFIX)
                && p.text.contains("Nadin")),
            "correction not persisted: {prefs:?}"
        );

        // Replayed into the next turn's prompt so the claim is honoured.
        let next = build_compose_prompt(&wg, &uuid, "otto", "what's for dinner?");
        assert!(next.to_lowercase().contains("correction"), "prompt: {next}");
        assert!(next.contains("Nadin is not logged"));
    }

    /// RULE 2 (repetition guard): the same summary is never sent twice. When the
    /// composer drafts a reply substantially identical to its previous one, the
    /// guard swaps in an honest offer to actually read the source.
    #[tokio::test]
    async fn ground_repetition_guard_replaces_the_repeated_summary() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);

        let stall = "Meals are set, just waiting on confirmations from you and Nadin.";

        // Turn 1: the stall is a fresh reply — it goes out as-is.
        let sink1 = RecSink::default();
        let c1 = FakeComposer::ok(stall);
        run_conversation_turn(
            &wg, &plan, "Plans for tomorrow?", "req-1", fast_timing(), Some(&c1), &sink1,
        )
        .await
        .unwrap();
        assert!(sink1.calls().last().unwrap().2.contains("waiting on confirmations"));

        // Turn 2: the SAME stall is drafted again → guard answers honestly.
        let sink2 = RecSink::default();
        let c2 = FakeComposer::ok(stall);
        run_conversation_turn(
            &wg, &plan, "walk me through it", "req-2", fast_timing(), Some(&c2), &sink2,
        )
        .await
        .unwrap();
        let last = sink2.calls().last().unwrap().2.clone();
        assert!(!last.contains("waiting on confirmations"), "stall repeated: {last}");
        assert!(last.to_lowercase().contains("read"), "not the honest fallback: {last}");
    }

    /// RULE 5 (§6.7 anti-fabrication): a composed reply that INVENTS a schedule
    /// fact — a birthday, back-to-back meetings, a packed day — with nothing on
    /// the calendar is rewritten to the honest fallback on the REAL delivery
    /// path (run_conversation_turn → run_composed_turn's finalize), not merely in
    /// a unit test of the guard. This is Luca's exact transcript bug, end to end:
    /// the fabrication was volunteered on an ordinary greeting, so it never hit
    /// the read-shaped grounding — the guard has to catch it regardless.
    #[tokio::test]
    async fn ground_anti_fabrication_rewrites_invented_schedule_on_the_real_path() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        // NO plan file → the calendar is EMPTY → strict grounding: any schedule
        // claim in the drafted reply is a fabrication.
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");
        let plan = plan_conversation(&wg, &cfg, "telegram:otto", "555", "luca-1", Entry::Direct);

        // The exact fabrication from the transcript, volunteered on a greeting.
        let fabricated = "Morning! You've got a birthday today and back-to-back meetings — \
                          a pretty packed day ahead.";
        let sink = RecSink::default();
        let c = FakeComposer::ok(fabricated);
        run_conversation_turn(
            &wg, &plan, "how's your day?", "req-fab", fast_timing(), Some(&c), &sink,
        )
        .await
        .unwrap();

        let last = sink.calls().last().unwrap().2.clone();
        let lc = last.to_lowercase();
        // NONE of the invented specifics survive to the family.
        assert!(!lc.contains("birthday"), "birthday survived: {last}");
        assert!(!lc.contains("meeting"), "meeting survived: {last}");
        assert!(!lc.contains("packed"), "packed survived: {last}");
        assert!(
            !lc.contains("back to back") && !lc.contains("back-to-back"),
            "load claim survived: {last}"
        );
        // The honest, calendar-referencing fallback went out instead.
        assert!(lc.contains("calendar"), "not the honest fallback: {last}");
    }

    /// Seed a confirmed OR unconfirmed binding under `name` so the inviter
    /// resolver has a roster to read.
    fn add_binding(wg: &Path, sender: &str, name: &str, confirmed: bool) {
        let agency_dir = wg.join("agency");
        let mut map = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
        let mut b = crate::agency::TelegramBinding::new(
            sender,
            &format!("agent-{sender}"),
            name,
            Some("otto".to_string()),
            Utc::now(),
        );
        b.confirmed = confirmed;
        b.confirmed_at = confirmed.then(Utc::now);
        map.add(b).unwrap();
        map.save(&agency_dir).unwrap();
    }

    /// D17 — the prebuilt binary must NOT bake a developer's name into the
    /// stranger-onboarding line. With no roster the line names "a family member"
    /// (neutral), never "Luca"; given a real member it names THAT person.
    #[test]
    fn onboarding_line_never_hardcodes_a_developer_name() {
        // Neutral fallback: no inviter known.
        let neutral = onboarding_line(None);
        assert!(
            !neutral.to_lowercase().contains("luca"),
            "onboarding line leaked a hardcoded name: {neutral}"
        );
        assert!(
            neutral.contains("a family member"),
            "neutral fallback missing: {neutral}"
        );
        assert!(neutral.contains("add"), "line lost its meaning: {neutral}");

        // Derived from the roster: names the real member.
        let derived = onboarding_line(Some("Robin"));
        assert!(
            derived.contains("Ask Robin to add"),
            "did not name the roster member: {derived}"
        );
        assert!(
            !derived.to_lowercase().contains("luca"),
            "derived line leaked a hardcoded name: {derived}"
        );

        // Empty / whitespace inviter degrades to the neutral fallback, not a
        // dangling "Ask  to add".
        assert_eq!(onboarding_line(Some("   ")), neutral);
    }

    /// D17 — `family_inviter_name` reads the household's OWN roster: the first
    /// confirmed member, ignoring unconfirmed bindings; `None` on an empty roster
    /// (which drives the neutral fallback above).
    #[test]
    fn family_inviter_name_derives_from_confirmed_roster() {
        let dir = tempdir().unwrap();
        // Empty roster → nobody to name.
        assert_eq!(family_inviter_name(dir.path()), None);

        // An UNconfirmed binding must not be offered as an inviter.
        add_binding(dir.path(), "pending-1", "Zoe", false);
        assert_eq!(
            family_inviter_name(dir.path()),
            None,
            "unconfirmed member must not be named as inviter"
        );

        // A confirmed member is named.
        add_binding(dir.path(), "user-1", "Robin", true);
        assert_eq!(
            family_inviter_name(dir.path()).as_deref(),
            Some("Robin"),
            "confirmed member should be the inviter"
        );
    }
}
