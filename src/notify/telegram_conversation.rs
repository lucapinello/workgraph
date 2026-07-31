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

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// The bare/`default`/legacy channel maps to `default_bot` when configured. A
/// named channel maps only to that bot. Ambiguous or unknown routing fails
/// closed rather than selecting an arbitrary HashMap entry.
pub fn bot_id_for_channel_with_default(
    config: &TelegramConfig,
    channel_type: &str,
    default_bot: Option<&str>,
) -> Option<String> {
    let stripped = channel_type
        .strip_prefix("telegram:")
        .unwrap_or(channel_type);
    let bots = config.all_bots();
    if bots.is_empty() {
        return None;
    }
    if stripped.is_empty() || stripped == "telegram" || stripped == "default" {
        if let Some(want) = default_bot {
            return bots
                .iter()
                .find(|(id, bot)| {
                    id.eq_ignore_ascii_case(want)
                        || bot
                            .agent_id
                            .as_deref()
                            .is_some_and(|agent| agent.eq_ignore_ascii_case(want))
                })
                .map(|(id, _)| id.clone());
        }
        return (bots.len() == 1).then(|| bots[0].0.clone());
    }
    bots.iter()
        .find(|(id, _)| id == stripped)
        .map(|(id, _)| id.clone())
}

/// Resolve a channel with no project-local default owner available.
pub fn bot_id_for_channel(config: &TelegramConfig, channel_type: &str) -> Option<String> {
    bot_id_for_channel_with_default(config, channel_type, None)
}

/// The agency agent a bot fronts (its nonblank `agent_id`), falling back to the
/// bot id itself when no usable binding is configured. Exposed for the
/// listener's casa-feed mirror, which maps the replying `bot_id` back to its
/// persona id.
pub fn agent_for_bot(config: &TelegramConfig, bot_id: &str) -> String {
    config
        .all_bots()
        .iter()
        .find(|(id, _)| id == bot_id)
        .and_then(|(_, b)| {
            b.agent_id
                .as_ref()
                .filter(|agent_id| !agent_id.trim().is_empty())
                .cloned()
        })
        .unwrap_or_else(|| bot_id.to_string())
}

/// The agency agent addressed by an elected/receiving `channel_type` — the bot
/// it resolves to, then that bot's `agent_id`. Exposed for the `wg telegram
/// conversation` dry-run, which pre-binds a fixture session to this agent.
pub fn agent_for_channel(config: &TelegramConfig, channel_type: &str) -> Option<String> {
    let bot_id = bot_id_for_channel(config, channel_type)?;
    Some(agent_for_bot(config, &bot_id))
}

/// Resolve the addressed agent with a caller-supplied project-local default.
pub fn agent_for_channel_with_default(
    config: &TelegramConfig,
    channel_type: &str,
    default_bot: Option<&str>,
) -> Option<String> {
    let bot_id = bot_id_for_channel_with_default(config, channel_type, default_bot)?;
    Some(agent_for_bot(config, &bot_id))
}

/// Resolve a configured household persona reference to the one agent id whose
/// bound session may receive the turn.
///
/// Session aliases are the stable identity surface. Agent names are mutable
/// display metadata, so an exact case-sensitive alias is authoritative: it
/// resolves only when exactly one session owns it, that row carries a nonblank
/// agent id, and exactly one session is bound to that id. Invalid alias state
/// fails closed and never falls through to a coincidentally matching name.
///
/// With no exact alias, direct full agent ids and unique id prefixes are
/// accepted only when exactly one session is bound to the resolved full id. A
/// uniquely bound raw literal preserves legacy hermetic fixtures. Finally, a
/// unique case-insensitive Agent.name match is retained as a bounded migration
/// path, again only with exactly one bound session. Unknown, unbound, duplicate,
/// or ambiguous references return `None` so the caller plans sessionless.
pub fn canonical_agent_id(workgraph_dir: &Path, agent_ref: &str) -> Option<String> {
    if agent_ref.trim().is_empty() {
        return None;
    }

    let registry = chat_sessions::load(workgraph_dir).unwrap_or_default();
    let uniquely_bound = |candidate: &str| {
        if candidate.is_empty() || candidate != candidate.trim() {
            return None;
        }
        let count = registry
            .sessions
            .values()
            .filter(|meta| meta.agent_id.as_deref() == Some(candidate))
            .count();
        (count == 1).then(|| candidate.to_string())
    };

    let alias_matches: Vec<_> = registry
        .sessions
        .values()
        .filter(|meta| meta.aliases.iter().any(|alias| alias == agent_ref))
        .collect();
    if !alias_matches.is_empty() {
        if alias_matches.len() != 1 {
            return None;
        }
        return alias_matches[0]
            .agent_id
            .as_deref()
            .and_then(uniquely_bound);
    }

    let agents_dir = workgraph_dir.join("agency").join("cache/agents");
    let agents = crate::agency::load_all_agents_or_warn(&agents_dir);
    let exact_ids: Vec<_> = agents
        .iter()
        .filter(|agent| agent.id == agent_ref)
        .collect();
    if !exact_ids.is_empty() {
        return if exact_ids.len() == 1 {
            uniquely_bound(&exact_ids[0].id)
        } else {
            None
        };
    }

    let prefix_ids: Vec<_> = agents
        .iter()
        .filter(|agent| agent.id.starts_with(agent_ref))
        .collect();
    if !prefix_ids.is_empty() {
        return if prefix_ids.len() == 1 {
            uniquely_bound(&prefix_ids[0].id)
        } else {
            None
        };
    }

    let literal_bindings = registry
        .sessions
        .values()
        .filter(|meta| meta.agent_id.as_deref() == Some(agent_ref))
        .count();
    match literal_bindings {
        1 => return Some(agent_ref.to_string()),
        2.. => return None,
        0 => {}
    }

    let name_matches: Vec<_> = agents
        .iter()
        .filter(|agent| agent.name.eq_ignore_ascii_case(agent_ref))
        .collect();
    if name_matches.len() == 1 {
        return uniquely_bound(&name_matches[0].id);
    }
    None
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
    let root = project_root_of(workgraph_dir);
    let owner_map = ownership::OwnerMap::load(&root);
    let coordination_owner = owner_map.owner_for_domain(ownership::Domain::Coordination);
    let bot_id = bot_id_for_channel_with_default(config, route_channel, coordination_owner)
        .unwrap_or_else(|| {
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
    // Stable household aliases and bounded legacy references resolve to a full
    // id only when exactly one session binding exists. Any unsafe state plans
    // sessionless rather than guessing from mutable display metadata.
    let session_ref = canonical_agent_id(workgraph_dir, &agent_id)
        .and_then(|session_key| chat_sessions::session_for_agent(workgraph_dir, &session_key));
    let requester = requester_display_name(workgraph_dir, sender);
    let channel = match entry {
        Entry::Direct => crate::graph::OriginChannel::TelegramDirect,
        Entry::GroupElected => crate::graph::OriginChannel::TelegramGroup,
    };
    match session_ref {
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

    /// Send, saying WHICH PHASE OF THE TURN this line is.
    ///
    /// The phase is what decides whether a send consumes the turn's one final
    /// reservation. The latency ack, the watchdog and the failure notice are all
    /// physical sends, and none of them is the turn's answer: a reservation
    /// consumed by the ack means a crash between the ack and the final leaves an
    /// hourglass on the family's screen and a claim on disk saying the turn was
    /// already delivered, so the final is suppressed FOREVER.
    ///
    /// The default delegates to [`send`](ReplySink::send), so every existing
    /// sink keeps working; only the wrappers that care about phase override it.
    async fn send_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
        phase: crate::notify::relay_receipt::ReplyPhase,
    ) -> Result<Option<String>> {
        let _ = phase;
        self.send(bot_id, chat_id, text).await
    }

    /// Edit, saying WHICH PHASE OF THE TURN the new text is. Same rule as
    /// [`send_phase`](ReplySink::send_phase): only a `final` consumes the turn's
    /// one reservation.
    async fn edit_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        phase: crate::notify::relay_receipt::ReplyPhase,
    ) -> Result<()> {
        let _ = phase;
        self.edit(bot_id, chat_id, message_id, text).await
    }

    /// TAKE the id of the message a fallback send created because an edit was
    /// refused, if this sink made one.
    ///
    /// `edit` returns `Result<()>`, which is why the fallback's id used to be
    /// dropped on the floor: the final answer was delivered as a NEW message
    /// while the reservation, the row and any receipt all went on naming the old
    /// ack. The record then points at a message that never held the answer.
    fn take_fallback_message_id(&self) -> Option<String> {
        None
    }
}

/// A delivery whose outcome the transport did not PROVE either way.
///
/// The distinction this type carries is the whole of the audit's item 7. A
/// transport error is not one thing:
///
///   · Telegram answered and the answer was "no" — the send PROVABLY failed, and
///     the turn must be released so a retry can take it;
///   · the request timed out, the connection tore, the body did not parse, or
///     `ok:true` arrived with no readable positive message id — we do not know
///     whether the family has the message.
///
/// Collapsing the second into the first is how a timeout AFTER Telegram accepted
/// the message releases the reservation and posts the same answer twice. Wrapped
/// in an error chain, this marker keeps "unproven" legible all the way out to the
/// reservation, which then HOLDS instead of releasing.
#[derive(Debug)]
pub struct UnprovenDelivery {
    pub detail: String,
}

impl std::fmt::Display for UnprovenDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the delivery is UNPROVEN — {} (the reservation stays held: a duplicate \
             on the family's screen is worse than a gap an operator can see)",
            self.detail
        )
    }
}

impl std::error::Error for UnprovenDelivery {}

/// Build an error that says "we could not prove this either way".
pub fn unproven_delivery(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UnprovenDelivery {
        detail: detail.into(),
    })
}

/// Is this failure AMBIGUOUS rather than a proven failure? Checked through the
/// whole chain, so a marker wrapped in later context is still legible.
pub fn is_unproven(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<UnprovenDelivery>())
}

// ---------------------------------------------------------------------------
// Durable physical-turn delivery guard
// ---------------------------------------------------------------------------

/// Prefix stored on every durable Telegram digest.
///
/// The prefix is part of the persisted request-id / delivery-ledger contract:
/// a future encoding or hash change must use a new prefix instead of silently
/// reinterpreting existing claims.
pub const DURABLE_TELEGRAM_DIGEST_PREFIX: &str = "b3-v1";

/// Version-stable BLAKE3 digest for durable Telegram turn identities.
///
/// Encoding is explicit and platform-independent: a fixed v1 preamble, then
/// the domain and each UTF-8 field as an unsigned 64-bit big-endian byte length
/// followed by the bytes (with the field count encoded the same way). The full
/// 256-bit digest is hex-encoded and prefixed with [`DURABLE_TELEGRAM_DIGEST_PREFIX`].
///
/// Listener-local [`crate::notify::telegram_dedupe::DedupeKey`] hashing does not
/// use this helper because that set never survives a process restart.
pub fn durable_telegram_digest_v1(domain: &str, fields: &[&str]) -> String {
    fn update_sized(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"worksgood.telegram.durable-digest.v1\0");
    update_sized(&mut hasher, domain.as_bytes());
    hasher.update(&(fields.len() as u64).to_be_bytes());
    for field in fields {
        update_sized(&mut hasher, field.as_bytes());
    }
    format!(
        "{DURABLE_TELEGRAM_DIGEST_PREFIX}-{}",
        hasher.finalize().to_hex(),
    )
}

/// The CANONICAL turn id this reply belongs to, if there is one.
///
/// `web-turn-<uuid v4>` is the id the gateway accepted the turn under and the
/// id every row and receipt for that turn carries. It is resolved from the
/// caller's own delivery id when that IS the turn id, else from `WG_TURN_ID`.
/// Nothing else qualifies: a request id, a session ref or a digest are not the
/// turn, and reserving on one of those is what let a turn be delivered twice.
pub fn canonical_turn_id(delivery_id: &str) -> Option<String> {
    let from_caller = delivery_id.trim();
    if crate::notify::relay_receipt::is_valid_turn_id(from_caller) {
        return Some(from_caller.to_string());
    }
    let env = std::env::var("WG_TURN_ID").ok()?;
    let env = env.trim();
    crate::notify::relay_receipt::is_valid_turn_id(env).then(|| env.to_string())
}

fn delivery_claim_path(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
) -> Option<PathBuf> {
    // THE FINAL RESERVATION IS ON THE TURN, AND ON NOTHING ELSE.
    //
    // This used to fold bot and chat into the claim, so that "two configured
    // voices never suppress one another". That reasoning holds for two DIFFERENT
    // turns; it is exactly wrong for one. An accepted turn has ONE final answer.
    // Keyed on (delivery id, bot, chat), a late original and a fresh attempt
    // routed through a second bot are two different claims — so both send, the
    // family gets the answer twice, and the turn ends with two rows both calling
    // themselves final. Keyed on the canonical turn id alone, the second caller
    // finds the reservation already held and makes no API call at all.
    //
    // The old triple survives ONLY where there is no canonical turn id — a DM or
    // a legacy path that never had one. Those callers keep exactly the guarantee
    // they had; they simply cannot be part of a turn's final-answer race.
    let digest = match canonical_turn_id(delivery_id) {
        Some(turn) => durable_telegram_digest_v1("telegram-turn-final", &[&turn]),
        None => {
            if delivery_id.trim().is_empty() {
                return None;
            }
            // The filename itself starts with `b3-v1-`, making its encoding
            // version explicit on disk. Hash all routing fields with a
            // length-delimited canonical encoding so the ledger exposes no
            // household or bot ids.
            durable_telegram_digest_v1("telegram-delivery-claim", &[delivery_id, bot_id, chat_id])
        }
    };
    Some(
        workgraph_dir
            .join("telegram-deliveries")
            .join(format!("{digest}.sent")),
    )
}

/// Local state for one logical family-visible reply.
///
/// The filesystem claim is the cross-process authority. This state only keeps
/// one invocation from calling its inner transport twice (for example, if an
/// acknowledgement send returned no editable message id).
#[derive(Debug, Clone)]
enum TurnDeliveryState {
    Fresh,
    Owned(Option<String>),
    Duplicate(Option<String>),
}

/// A restart-stable, record-before-send guard around one logical reply.
///
/// Session outboxes already make successful composed turns idempotent, but
/// sessionless/onboarding replies, the legacy outbox-poll path, discussion
/// takes, and compose failures do not all leave an outbox reply. Those paths
/// therefore share this transport-level guard. The caller supplies an opaque
/// physical-turn-derived id; bot and chat routing are folded into the claim so
/// two configured voices never suppress one another.
struct TurnDeliverySink<'a> {
    inner: &'a dyn ReplySink,
    claim_path: Option<PathBuf>,
    retry_path: Option<PathBuf>,
    state: Mutex<TurnDeliveryState>,
    /// The message id of the latency ACK, which no longer lives in `state`:
    /// the ack does not claim the turn (only a `final` does), so the id it
    /// returned is remembered here instead. A turn that acked and then failed
    /// still has to be resumable BY EDITING THAT ACK, or the retry composes a
    /// second inbox turn and the family sees a stranded hourglass beside a new
    /// answer.
    ack_message_id: Mutex<Option<String>>,
}

impl<'a> TurnDeliverySink<'a> {
    fn new(
        workgraph_dir: &Path,
        delivery_id: &str,
        bot_id: &str,
        chat_id: &str,
        inner: &'a dyn ReplySink,
    ) -> Self {
        let claim_path = delivery_claim_path(workgraph_dir, delivery_id, bot_id, chat_id);
        let retry_path = claim_path.as_ref().map(|path| path.with_extension("retry"));
        Self {
            inner,
            claim_path,
            retry_path,
            state: Mutex::new(TurnDeliveryState::Fresh),
            ack_message_id: Mutex::new(None),
        }
    }

    /// A completed or in-flight claim means another invocation owns this
    /// physical reply. This early check keeps legacy polling/composition from
    /// running again; the atomic `create_new` in [`claim`] remains the race-safe
    /// authority when two processes reach this check together.
    fn already_claimed(&self) -> bool {
        self.claim_path.as_ref().is_some_and(|path| path.exists())
    }

    /// A prior transport call failed after the reply bytes were persisted to a
    /// session outbox. The next attempt should reuse those canonical bytes
    /// rather than compose and append a second same-request draft.
    fn has_failed_attempt(&self) -> bool {
        self.retry_path.as_ref().is_some_and(|path| path.exists())
    }

    fn failed_edit_message_id(&self) -> Option<String> {
        let path = self.retry_path.as_ref()?;
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .strip_prefix("edit:")
            .map(str::trim)
            .filter(|message_id| !message_id.is_empty())
            .map(str::to_string)
    }

    /// Atomically claim this logical reply. The claim is created and synced
    /// before transport, so a listener restart cannot resend an already-started
    /// physical turn. A confirmed Telegram message id replaces the empty
    /// pending marker after send, allowing a racing invocation to preserve the
    /// acknowledgement/edit shape without sending again.
    fn claim(&self) -> Result<TurnDeliveryState> {
        let Some(path) = self.claim_path.as_ref() else {
            return Ok(TurnDeliveryState::Owned(None));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Telegram delivery ledger {}",
                    parent.display()
                )
            })?;
        }
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(b"pending\n").and_then(|_| file.sync_all()) {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                    return Err(error).with_context(|| {
                        format!(
                            "Failed to persist Telegram delivery claim {}",
                            path.display()
                        )
                    });
                }
                if let Some(retry_path) = self.retry_path.as_ref() {
                    let _ = std::fs::remove_file(retry_path);
                }
                Ok(TurnDeliveryState::Owned(None))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let message_id = std::fs::read_to_string(path)
                    .ok()
                    .map(|body| body.trim().to_string())
                    .filter(|body| !body.is_empty() && body != "pending");
                Ok(TurnDeliveryState::Duplicate(message_id))
            }
            Err(error) => Err(error)
                .with_context(|| format!("Failed to claim Telegram delivery {}", path.display())),
        }
    }

    fn persist_message_id(&self, message_id: Option<&str>) {
        let Some(path) = self.claim_path.as_ref() else {
            return;
        };
        let body = message_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("sent");
        if let Err(error) = crate::atomic_file::write_atomic(path, format!("{body}\n").as_bytes()) {
            // The record-before-send claim still prevents a duplicate. Losing
            // only the transport-local id is safe because a replay exits before
            // composing; keep the confirmed delivery successful.
            eprintln!(
                "[{}] Telegram delivery ledger could not store message id: {error}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
    }

    fn rearm(&self, retry_state: &str) {
        if let Some(path) = self.claim_path.as_ref()
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "[{}] Telegram delivery ledger could not re-arm failed send: {error}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
        if let Some(retry_path) = self.retry_path.as_ref()
            && let Err(error) =
                crate::atomic_file::write_atomic(retry_path, format!("{retry_state}\n").as_bytes())
        {
            eprintln!(
                "[{}] Telegram delivery ledger could not mark failed delivery retryable: {error}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
        *self.state.lock().unwrap() = TurnDeliveryState::Fresh;
    }

    fn rearm_after_send_failure(&self) {
        self.rearm("send");
    }

    /// FAIL CLOSED. The transport answered without proving a delivery, so we do
    /// not know whether the family already has this message. The reservation
    /// stays HELD — no release, no retry marker — and the claim keeps its
    /// pending marker so any later caller sees the turn as taken. An operator
    /// clearing the claim is the deliberate way out; guessing is not.
    fn hold_ambiguous(&self) {
        *self.state.lock().unwrap() = TurnDeliveryState::Duplicate(None);
        eprintln!(
            "[{}] Telegram delivery UNPROVEN — the turn's reservation is held closed \
             rather than re-sent (a duplicate is worse than a gap the operator can see)",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }

    fn rearm_after_edit_failure(&self, message_id: &str) {
        self.rearm(&format!("edit:{message_id}"));
    }

    /// Write the retry marker WITHOUT touching the final reservation.
    ///
    /// A non-final phase never took the claim, so it has none to release — and
    /// deleting the claim path here would delete a reservation this invocation
    /// does not own. What it does still owe the next attempt is the fact that
    /// this one failed, and which ack it should edit.
    fn mark_retryable(&self, retry_state: &str) {
        if let Some(retry_path) = self.retry_path.as_ref()
            && let Err(error) =
                crate::atomic_file::write_atomic(retry_path, format!("{retry_state}\n").as_bytes())
        {
            eprintln!(
                "[{}] Telegram delivery ledger could not mark failed delivery retryable: {error}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
    }

    fn rearm_incomplete_ack(&self) {
        let state = self.state.lock().unwrap().clone();
        match state {
            TurnDeliveryState::Owned(Some(message_id)) => {
                self.rearm_after_edit_failure(&message_id);
            }
            TurnDeliveryState::Owned(None) => self.rearm_after_send_failure(),
            // FRESH now means "the ack sent but the final never claimed" — the
            // ordinary shape since the reservation became phase-aware. The turn
            // must still be resumable, and it must resume by EDITING the ack
            // rather than posting a second message beside it.
            TurnDeliveryState::Fresh => match self.ack_message_id.lock().unwrap().clone() {
                Some(message_id) => self.mark_retryable(&format!("edit:{message_id}")),
                None => self.mark_retryable("send"),
            },
            TurnDeliveryState::Duplicate(_) => {}
        }
    }

    fn duplicate_outcome(plan: &ConversationPlan) -> TurnOutcome {
        match plan {
            ConversationPlan::Onboard { .. } => TurnOutcome::Onboarded,
            ConversationPlan::Sessionless { .. } => TurnOutcome::Sessionless,
            ConversationPlan::Converse { .. } => TurnOutcome::Replied { acked: false },
        }
    }
}

#[async_trait]
impl ReplySink for TurnDeliverySink<'_> {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        let state = {
            let mut state = self.state.lock().unwrap();
            match &*state {
                TurnDeliveryState::Fresh => {
                    let claimed = self.claim()?;
                    *state = claimed.clone();
                    claimed
                }
                existing => existing.clone(),
            }
        };

        match state {
            TurnDeliveryState::Duplicate(Some(message_id))
            | TurnDeliveryState::Owned(Some(message_id)) => Ok(Some(message_id)),
            TurnDeliveryState::Duplicate(None) => Ok(None),
            // RESERVE-BEFORE-SEND, COMMIT-ON-PROVEN, RELEASE-ON-FAILURE, and
            // FAIL CLOSED ON AMBIGUOUS. The reservation above is already held.
            // What happens next depends on what the transport could PROVE:
            //
            //   · a positive message id → the send is proven; COMMIT it, so a
            //     racing caller reuses the id instead of sending again;
            //   · a transport error → the send provably failed; RELEASE, so the
            //     self-heal retry can take the turn;
            //   · anything else — accepted with no id, a torn answer, a body
            //     that did not parse → we do NOT know whether the family got the
            //     message. FAIL CLOSED: keep the reservation HELD and report the
            //     failure. Releasing here would send a second copy of a message
            //     that may already be on their screen, and a duplicate is the
            //     one failure mode the family actually experiences.
            TurnDeliveryState::Owned(None) => match self.inner.send(bot_id, chat_id, text).await {
                Ok(Some(message_id))
                    if !message_id.trim().is_empty() && message_id.trim() != "0" =>
                {
                    self.persist_message_id(Some(&message_id));
                    *self.state.lock().unwrap() =
                        TurnDeliveryState::Owned(Some(message_id.clone()));
                    Ok(Some(message_id))
                }
                Ok(_ambiguous) => {
                    self.hold_ambiguous();
                    Err(unproven_delivery(
                        "the transport accepted the reply but proved no message id",
                    ))
                }
                // A TYPED failure, not "any error". `Err` from the real stack is
                // two different facts wearing one type: a proven refusal from
                // Telegram, and an ambiguous timeout/torn body that may already
                // have been delivered. Only the PROVEN one may release the turn.
                Err(error) if is_unproven(&error) => {
                    self.hold_ambiguous();
                    Err(error)
                }
                Err(error) => {
                    self.rearm_after_send_failure();
                    Err(error)
                }
            },
            TurnDeliveryState::Fresh => unreachable!("fresh delivery must be claimed before send"),
        }
    }

    /// PHASE-AWARE. Only a physical `final` consumes the turn's one reservation.
    ///
    /// The ack, the watchdog line and the failure notice all go straight to the
    /// transport: they are real sends, but none of them is the turn's answer, and
    /// a reservation consumed by one of them means the answer that follows is
    /// suppressed as a duplicate of a message the family never received.
    async fn send_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
        phase: crate::notify::relay_receipt::ReplyPhase,
    ) -> Result<Option<String>> {
        use crate::notify::relay_receipt::ReplyPhase;
        match phase {
            ReplyPhase::Final => self.send(bot_id, chat_id, text).await,
            ReplyPhase::Ack | ReplyPhase::Watchdog | ReplyPhase::Failure => {
                match self.inner.send_phase(bot_id, chat_id, text, phase).await {
                    Ok(message_id) => {
                        if let Some(id) = message_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|id| !id.is_empty() && *id != "0")
                        {
                            *self.ack_message_id.lock().unwrap() = Some(id.to_string());
                        }
                        Ok(message_id)
                    }
                    Err(error) => {
                        // The final was never reserved, so there is nothing to
                        // release — but the next attempt still needs to know
                        // this one failed, or it composes a second inbox turn.
                        self.mark_retryable("send");
                        Err(error)
                    }
                }
            }
        }
    }

    /// The same rule for an edit: a failure notice that REPLACES the ack in
    /// place is still not the turn's answer, and must not consume finality.
    async fn edit_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        phase: crate::notify::relay_receipt::ReplyPhase,
    ) -> Result<()> {
        use crate::notify::relay_receipt::ReplyPhase;
        match phase {
            ReplyPhase::Final => self.edit(bot_id, chat_id, message_id, text).await,
            ReplyPhase::Ack | ReplyPhase::Watchdog | ReplyPhase::Failure => {
                self.inner
                    .edit_phase(bot_id, chat_id, message_id, text, phase)
                    .await
            }
        }
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let state = self.state.lock().unwrap().clone();
        match state {
            TurnDeliveryState::Duplicate(_) => Ok(()),
            TurnDeliveryState::Owned(_) => {
                match self.inner.edit(bot_id, chat_id, message_id, text).await {
                    Ok(()) => {
                        // THE ID THAT ACTUALLY CARRIES THE ANSWER. If the edit
                        // was refused and the sink fell back to a fresh send, the
                        // final answer is in THAT message; persisting the ack's
                        // id here is how the reservation came to name a message
                        // the family never read the answer in.
                        let delivered = self
                            .inner
                            .take_fallback_message_id()
                            .unwrap_or_else(|| message_id.to_string());
                        self.persist_message_id(Some(&delivered));
                        *self.state.lock().unwrap() = TurnDeliveryState::Owned(Some(delivered));
                        Ok(())
                    }
                    // Same typed split as `send`: an AMBIGUOUS edit may already
                    // have replaced the ack with the final answer, so releasing
                    // the turn here is how the family gets the answer twice.
                    Err(error) if is_unproven(&error) => {
                        self.hold_ambiguous();
                        Err(error)
                    }
                    Err(error) => {
                        self.rearm_after_edit_failure(message_id);
                        Err(error)
                    }
                }
            }
            TurnDeliveryState::Fresh => {
                // Defensive: current conversation paths always send before edit.
                // If a future caller edits directly, claim it with the same
                // record-before-act discipline.
                let claimed = self.claim()?;
                *self.state.lock().unwrap() = claimed.clone();
                match claimed {
                    TurnDeliveryState::Duplicate(_) => Ok(()),
                    TurnDeliveryState::Owned(_) => {
                        match self.inner.edit(bot_id, chat_id, message_id, text).await {
                            Ok(()) => {
                                self.persist_message_id(Some(message_id));
                                *self.state.lock().unwrap() =
                                    TurnDeliveryState::Owned(Some(message_id.to_string()));
                                Ok(())
                            }
                            Err(error) => {
                                self.rearm_after_edit_failure(message_id);
                                Err(error)
                            }
                        }
                    }
                    TurnDeliveryState::Fresh => unreachable!(),
                }
            }
        }
    }
}

/// Send one explicitly identified logical reply at most once across listener
/// restarts. A later household turn must pass a different `delivery_id`, even
/// when its words are identical.
pub async fn send_reply_once(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
    text: &str,
    sink: &dyn ReplySink,
) -> Result<Option<String>> {
    let guarded = TurnDeliverySink::new(workgraph_dir, delivery_id, bot_id, chat_id, sink);
    guarded.send(bot_id, chat_id, text).await
}

/// [`send_reply_once`], saying which PHASE of the turn these bytes are.
///
/// Only a `final` is sent at most once: the ack, the watchdog and the failure
/// notice are not the turn's answer and must not consume its one reservation.
pub async fn send_reply_once_phase(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
    text: &str,
    sink: &dyn ReplySink,
    phase: crate::notify::relay_receipt::ReplyPhase,
) -> Result<Option<String>> {
    let guarded = TurnDeliverySink::new(workgraph_dir, delivery_id, bot_id, chat_id, sink);
    guarded.send_phase(bot_id, chat_id, text, phase).await
}

/// Durable state for a family-visible reply whose exact guarded bytes are
/// persisted alongside the physical-turn delivery claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalDeliveryState {
    /// No canonical bytes and no transport state exist; composition may run.
    Missing,
    /// An empty canonical marker records a first-writer compose/timeout skip.
    /// It is never sent, but prevents a same-turn replay from resurrecting the
    /// skipped logical reply with newly composed words.
    Skipped,
    /// Canonical bytes exist but have not been confirmed by transport yet.
    Ready(String),
    /// Canonical bytes and a confirmed transport claim both exist.
    Confirmed(String),
    /// A process owns a pending record-before-transport claim. The bytes must
    /// not enter downstream discussion context until confirmation is durable.
    Pending,
    /// A transport claim/retry exists without canonical bytes (legacy or
    /// corrupt state). Fail closed: do not recompose or expose unknown words.
    Unavailable,
}

fn read_optional_canonical(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to read canonical Telegram delivery {}",
                path.display(),
            )
        }),
    }
}

/// Read canonical bytes and their transport state without claiming or sending.
///
/// Callers use this before composition: [`CanonicalDeliveryState::Confirmed`]
/// and [`CanonicalDeliveryState::Ready`] both carry the first writer's exact
/// guarded bytes, while only `Missing` permits a fresh draft.
pub fn canonical_delivery_state(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
) -> Result<CanonicalDeliveryState> {
    let Some(claim_path) = delivery_claim_path(workgraph_dir, delivery_id, bot_id, chat_id) else {
        return Ok(CanonicalDeliveryState::Missing);
    };
    let canonical_path = claim_path.with_extension("canonical");
    let retry_path = claim_path.with_extension("retry");
    let canonical = read_optional_canonical(&canonical_path)?;
    let claim = match std::fs::read_to_string(&claim_path) {
        Ok(body) => Some(!body.trim().is_empty() && body.trim() != "pending"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read Telegram delivery claim {}",
                    claim_path.display(),
                )
            });
        }
    };
    let retry_exists = retry_path.try_exists().with_context(|| {
        format!(
            "Failed to inspect Telegram delivery retry {}",
            retry_path.display(),
        )
    })?;

    Ok(match (canonical, claim, retry_exists) {
        (Some(text), None, false) if text.is_empty() => CanonicalDeliveryState::Skipped,
        (Some(text), Some(true), _) if !text.is_empty() => CanonicalDeliveryState::Confirmed(text),
        (Some(text), Some(false), _) if !text.is_empty() => CanonicalDeliveryState::Pending,
        (Some(text), None, _) if !text.is_empty() => CanonicalDeliveryState::Ready(text),
        (None, None, false) => CanonicalDeliveryState::Missing,
        _ => CanonicalDeliveryState::Unavailable,
    })
}

static NEXT_CANONICAL_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn create_canonical_temp_file(
    parent: &Path,
    file_name: &str,
    process_id: u32,
    next_id: &AtomicU64,
) -> std::io::Result<(PathBuf, std::fs::File)> {
    loop {
        let sequence = next_id.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".{file_name}.tmp.{process_id}.{sequence}",));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            // A process can crash after staging but before cleanup, and a later
            // process may eventually reuse its pid. Never delete or trust that
            // orphan; advance to a fresh no-clobber candidate.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Atomically publish the first canonical byte sequence for a delivery.
///
/// A fully written and synced same-directory temp file is hard-linked into the
/// canonical path. `hard_link` is the no-clobber commit point: concurrent
/// composers may race, but every caller reads and sends the same winning bytes,
/// and readers can never observe a partial file.
fn persist_canonical_reply_once(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
    text: &str,
) -> Result<String> {
    let Some(claim_path) = delivery_claim_path(workgraph_dir, delivery_id, bot_id, chat_id) else {
        return Ok(text.to_string());
    };
    let canonical_path = claim_path.with_extension("canonical");
    if let Some(existing) = read_optional_canonical(&canonical_path)? {
        return Ok(existing);
    }

    let parent = canonical_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("canonical delivery path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create Telegram delivery ledger {}",
            parent.display(),
        )
    })?;
    let file_name = canonical_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("canonical");
    let (temp_path, mut file) = create_canonical_temp_file(
        parent,
        file_name,
        std::process::id(),
        &NEXT_CANONICAL_TEMP_ID,
    )
    .with_context(|| {
        format!(
            "Failed to stage canonical Telegram delivery {}",
            canonical_path.display(),
        )
    })?;

    let write_result = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "Failed to stage canonical Telegram delivery {}",
                canonical_path.display(),
            )
        });
    }

    let published = match std::fs::hard_link(&temp_path, &canonical_path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!(
                    "Failed to publish canonical Telegram delivery {}",
                    canonical_path.display(),
                )
            });
        }
    };
    let _ = std::fs::remove_file(&temp_path);
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }

    if published {
        Ok(text.to_string())
    } else {
        read_optional_canonical(&canonical_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "canonical Telegram delivery disappeared after concurrent publish: {}",
                canonical_path.display(),
            )
        })
    }
}

/// Persist guarded reply bytes before transport and deliver the winning
/// canonical sequence at most once.
///
/// A returned transport error keeps the canonical file and re-arms the existing
/// retry marker, so the next invocation sends byte-for-byte the original draft.
/// `Pending` is conservative: another process may still be sending, and callers
/// must not use those words as delivered discussion context yet.
pub async fn send_canonical_reply_once(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
    text: &str,
    sink: &dyn ReplySink,
) -> Result<CanonicalDeliveryState> {
    if delivery_id.trim().is_empty() {
        if text.is_empty() {
            return Ok(CanonicalDeliveryState::Skipped);
        }
        send_reply_once(workgraph_dir, delivery_id, bot_id, chat_id, text, sink).await?;
        return Ok(CanonicalDeliveryState::Confirmed(text.to_string()));
    }

    // Inspect legacy/transport state before publishing anything. In particular,
    // a claim or retry without canonical bytes represents words this process
    // cannot know; attaching a newly composed draft would falsely bless those
    // bytes as already delivered.
    match canonical_delivery_state(workgraph_dir, delivery_id, bot_id, chat_id)? {
        CanonicalDeliveryState::Confirmed(text) => {
            return Ok(CanonicalDeliveryState::Confirmed(text));
        }
        CanonicalDeliveryState::Skipped => {
            return Ok(CanonicalDeliveryState::Skipped);
        }
        CanonicalDeliveryState::Pending => {
            return Ok(CanonicalDeliveryState::Pending);
        }
        CanonicalDeliveryState::Unavailable => {
            return Ok(CanonicalDeliveryState::Unavailable);
        }
        CanonicalDeliveryState::Missing => {
            persist_canonical_reply_once(workgraph_dir, delivery_id, bot_id, chat_id, text)?;
        }
        CanonicalDeliveryState::Ready(_) => {}
    }

    let canonical = match canonical_delivery_state(workgraph_dir, delivery_id, bot_id, chat_id)? {
        CanonicalDeliveryState::Confirmed(text) => {
            return Ok(CanonicalDeliveryState::Confirmed(text));
        }
        CanonicalDeliveryState::Skipped => {
            return Ok(CanonicalDeliveryState::Skipped);
        }
        CanonicalDeliveryState::Pending => {
            return Ok(CanonicalDeliveryState::Pending);
        }
        CanonicalDeliveryState::Unavailable | CanonicalDeliveryState::Missing => {
            return Ok(CanonicalDeliveryState::Unavailable);
        }
        CanonicalDeliveryState::Ready(text) => text,
    };

    send_reply_once(
        workgraph_dir,
        delivery_id,
        bot_id,
        chat_id,
        &canonical,
        sink,
    )
    .await?;
    Ok(
        match canonical_delivery_state(workgraph_dir, delivery_id, bot_id, chat_id)? {
            CanonicalDeliveryState::Confirmed(text) => CanonicalDeliveryState::Confirmed(text),
            CanonicalDeliveryState::Pending => CanonicalDeliveryState::Pending,
            CanonicalDeliveryState::Skipped
            | CanonicalDeliveryState::Missing
            | CanonicalDeliveryState::Ready(_)
            | CanonicalDeliveryState::Unavailable => CanonicalDeliveryState::Unavailable,
        },
    )
}

/// Production sink: resolves `bot_id` against the config and sends via that
/// bot's [`TelegramChannel`], falling back to the first configured bot so a
/// reply always goes out. The token lives only on the channel and is never
/// logged.
pub struct BotReplySink {
    config: TelegramConfig,
    /// The id of the message a FALLBACK send created after a refused edit. The
    /// final answer lives in that message, not in the ack the edit failed to
    /// change, so the id has to travel back out of `edit`'s `Result<()>`.
    fallback_message_id: Mutex<Option<String>>,
}

impl BotReplySink {
    pub fn new(config: TelegramConfig) -> Self {
        Self {
            config,
            fallback_message_id: Mutex::new(None),
        }
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

    fn take_fallback_message_id(&self) -> Option<String> {
        self.fallback_message_id.lock().unwrap().take()
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let bots = self.config.all_bots();
        let (id, bot) = resolve_reply_bot(&bots, bot_id)
            .ok_or_else(|| anyhow::anyhow!("no Telegram bots configured — cannot edit"))?;
        let channel = TelegramChannel::from_bot(id.clone(), bot.clone());
        // THE FALLBACK IS FOR A PROVEN EDIT FAILURE, AND ONLY THAT.
        //
        // This used to catch EVERY edit error and immediately post a fresh
        // message. An edit that timed out may already have replaced the ack with
        // the final answer, so the unconditional fallback is a second copy of an
        // answer the family already has — the exact duplicate the turn's
        // reservation exists to prevent, produced inside the sink the
        // reservation cannot see.
        //
        // So: Telegram said no (message too old, non-numeric id, `ok:false`) ⇒
        // the edit provably did not apply ⇒ send a fresh message so the human is
        // never stranded on an hourglass. Anything ambiguous ⇒ propagate the
        // UNPROVEN marker and let the reservation stay held.
        match channel.edit_text(chat_id, message_id, text).await {
            Ok(()) => Ok(()),
            Err(e) if is_unproven(&e) => Err(e),
            Err(e) => {
                // `{e:#}` prints the full error chain, which for a transport
                // failure embeds the request URL (and thus the bot token) —
                // redact before logging. See `telegram::redact_bot_token`.
                eprintln!(
                    "[convo] editMessageText was refused ({}) — sending fresh message instead",
                    super::telegram::redact_bot_token(&format!("{e:#}"))
                );
                // The fallback's OWN message id is what actually carries the
                // final answer. Losing it — as this path used to — leaves the
                // reservation and every receipt naming the OLD ack id, so the
                // record points at a message that never held the answer.
                let fallback = channel.send_text(chat_id, text).await?;
                self.fallback_message_id.lock().unwrap().replace(fallback.0);
                Ok(())
            }
        }
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

/// The context blocks the GATEWAY builds and forwards to this process over
/// forward-compatible env vars (a binary that hasn't learned one simply ignores
/// it — never a "no such flag" break). Grouped in one struct so the compose-prompt
/// builder can't have two `Option<&str>` swapped at a call site, and so adding a
/// fourth forwarded block is one field rather than another positional argument.
///
/// | field    | env var              | built by (gateway)                        |
/// |----------|----------------------|-------------------------------------------|
/// | `thread` | `WG_THREAD_CONTEXT`  | the originating pane's recent turns       |
/// | `week`   | `WG_WEEK_CONTEXT`    | `weekSource.buildWeekContext` (Dinners)   |
/// | `memory` | `WG_MEMORY_CONTEXT`  | `memoryInject.buildMemoryContext` (Tier 1)|
#[derive(Debug, Default, Clone, Copy)]
pub struct ForwardedContext<'a> {
    /// Recent turns of the originating conversation (task `nora-clarify-engine`).
    pub thread: Option<&'a str>,
    /// The parsed Dinners table + today/tomorrow markers (task
    /// `week-grounding-engine`).
    pub week: Option<&'a str>,
    /// The acting member's scoped, non-authoritative Tier-1 family memory (task
    /// `p1-engine-memory-reader`, docs/39 §6).
    pub memory: Option<&'a str>,
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
    // WEEK CONTEXT (task week-grounding-engine): the gateway forwards the parsed
    // Dinners table (day→dish + family-local today/tomorrow markers) via
    // `WG_WEEK_CONTEXT`, built by the SAME weekSource parser the Week view uses.
    // The LIVE Nora bug — "Nothing's locked in for Saturday yet" while the plan
    // table has "Saturday: Baked white fish" — was the composer answering from the
    // plan's prose skeleton instead of the table. Read at the production boundary
    // (unset on the Telegram-listener path → `None`, prompt unchanged) and threaded
    // into the pure `_at` builder so tests stay deterministic.
    let week = std::env::var("WG_WEEK_CONTEXT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // FAMILY MEMORY (task p1-engine-memory-reader): the gateway builds the
    // acting-member-scoped, NON-AUTHORITATIVE, token-budgeted Tier-1 memory block
    // (`memoryInject.buildMemoryContext`, docs/39 §6) and forwards it via
    // `WG_MEMORY_CONTEXT` — the third of the same family of forward-compatible env
    // vars. Until now THIS process never read it: the block was built, budgeted,
    // logged and dropped, so on a real deploy every remembered preference was
    // invisible to the model that answers the family (the hermetic human-flow stub
    // reflected it, which is exactly why the gap stayed green). Read at the
    // production boundary and threaded into the pure `_at` builder; unset (the
    // Telegram-listener path, or nothing remembered) → `None`, prompt unchanged.
    let memory = std::env::var("WG_MEMORY_CONTEXT")
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
        ForwardedContext {
            thread: thread.as_deref(),
            week: week.as_deref(),
            memory: memory.as_deref(),
        },
    )
}

/// `wg telegram compose-prompt` — the credential-free diagnostic view of the
/// assembled compose prompt (sibling of `wg telegram elect` / `discuss` /
/// `decide`). Runs the REAL production assembly, including the gateway-forwarded
/// `WG_THREAD_CONTEXT` / `WG_WEEK_CONTEXT` / `WG_MEMORY_CONTEXT` env blocks, and
/// returns the prompt WITHOUT spawning a model or sending anything — so a
/// scratch-project script can prove what the composer is really handed.
pub fn compose_prompt_preview(
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    human_message: &str,
) -> String {
    build_compose_prompt(workgraph_dir, session_ref, agent_id, human_message)
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
    forwarded: ForwardedContext<'_>,
) -> String {
    let ForwardedContext {
        thread: thread_context,
        week: week_context,
        memory: memory_context,
    } = forwarded;
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
        // RECENCY DISCIPLINE (task owner-pin-engine, spec item 3): the window
        // above can carry STALE unanswered asks from hours ago (e.g. an old
        // "how many calories in the pasta?") alongside the LIVE referent (the
        // duck-breast exchange seconds ago). The 17:5x repro answered pasta
        // first and buried the duck. So: the MOST RECENT exchange is THE
        // referent — lead with it and answer it first and primarily. An older
        // unanswered ask may be closed AFTERWARD, but only if explicitly marked
        // as such ("and to close the loop on the earlier pasta question: …") —
        // never led with, never blended so the family can't tell which dish a
        // number belongs to.
        prompt.push_str(
            "IMPORTANT — the LAST message in that list is the live topic. The new message \
             below refers to the MOST RECENT exchange, so answer THAT first and primarily. \
             If an OLDER, still-unanswered question is in the list, only address it AFTER \
             you've fully answered the recent one, and clearly label it as the older topic \
             (e.g. \"and to close the loop on the earlier <dish> question: …\"). Never lead \
             with the older topic, and never blend two dishes' numbers together so it's \
             unclear which is which.\n\n",
        );
    }
    prompt.push_str(
        "Reply to the message below in a natural, friendly way. Keep it short and \
         conversational. Talk like a person texting family — no jargon, no task ids, no \
         status dumps, no markdown headings. Just answer.\n\n",
    );
    // FIRST-PERSON, NO-DEFERRAL (task owner-pin-engine): the delivering voice IS
    // this persona — so answer as yourself and finish the thought. The 17:5x
    // repro had the answer land, then dangle "…let me get Nora's exact take"
    // (from Nora herself) — a third-person self-reference AND a fresh promise to
    // no one. This instruction closes it at generation time; the `enforce_no_
    // deferral` guard strips any tail that slips through.
    prompt.push_str(
        "You ARE this person — answer fully in the first person, as yourself. Never refer \
         to yourself in the third person or talk about yourself as if you were another \
         member of the team. You are the one delivering this answer, so give it and stop: \
         do NOT end with a fresh promise to \"get back to you\", \"get someone's exact \
         take\", \"check with\" anyone, or \"circle back\" — if you know the answer, say \
         it now; there is no one else to defer to.\n\n",
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

    // WEEK CONTEXT (task week-grounding-engine): the gateway's parsed Dinners
    // table, wrapped with the standing instruction to answer dinner/meal
    // questions FROM it and NEVER claim a day is empty when it has an entry. This
    // is the authoritative dinner source (parsed from the SAME weekSource the Week
    // view uses), so it lands AFTER the prose-based grounded block above — the
    // table wins for "what's for dinner <day>?". Absent (Telegram-listener path)
    // → omitted, prompt unchanged.
    if let Some(week) = week_context {
        if let Some(block) = grounding::week_context_block(week) {
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

    // FAMILY MEMORY (task p1-engine-memory-reader): the gateway's scoped, budgeted
    // Tier-1 block, wrapped with the "soft priors — everything above WINS"
    // instruction. It is injected LAST on purpose (docs/39 §6): the model reads the
    // live calendar line, the read-shaped week grounding, the forwarded Dinners
    // table and the family's corrections FIRST, so precedence — Tier 0 live >
    // config > Tier 1 durable, and corrections outrank distilled facts (§5.2) — is
    // legible in reading order, and a remembered pattern can never be presented as
    // this week's schedule. Absent (Telegram-listener path, or nothing remembered)
    // → omitted, prompt unchanged.
    if let Some(memory) = memory_context {
        if let Some(block) = grounding::memory_context_block(memory) {
            prompt.push_str(&block);
            prompt.push('\n');
        }
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
        let prompt = build_compose_prompt(workgraph_dir, session_ref, agent_id, human_message);
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
) -> Result<Option<chat::ChatMessage>> {
    let msgs = chat::read_outbox_since_ref(workgraph_dir, session_ref, baseline)?;
    if let Some(m) = msgs.iter().find(|m| m.request_id == request_id) {
        return Ok(Some(m.clone()));
    }
    Ok(msgs.into_iter().next())
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
    // Capture the gateway-forwarded week block and local civil date once at turn
    // entry. The prompt builder reads the same process-local env, while finalization
    // receives these immutable snapshots so delivery cannot reclassify the request
    // after a midnight crossing or observe later grounding bytes.
    let week_context = std::env::var("WG_WEEK_CONTEXT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let local_date = chrono::Local::now().date_naive();
    run_conversation_turn_with_week_context(
        workgraph_dir,
        plan,
        human_message,
        request_id,
        timing,
        composer,
        sink,
        week_context.as_deref(),
        local_date,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_conversation_turn_with_week_context(
    workgraph_dir: &Path,
    plan: &ConversationPlan,
    human_message: &str,
    request_id: &str,
    timing: AckTiming,
    composer: Option<&dyn ReplyComposer>,
    sink: &dyn ReplySink,
    week_context: Option<&str>,
    local_date: chrono::NaiveDate,
) -> Result<TurnOutcome> {
    let route = plan.route();
    let durable_sink = TurnDeliverySink::new(
        workgraph_dir,
        request_id,
        &route.bot_id,
        &route.chat_id,
        sink,
    );
    if durable_sink.already_claimed() {
        println!(
            "[{}] conversation delivery already claimed — skipping physical-turn replay",
            chrono::Utc::now().format("%H:%M:%S"),
        );
        return Ok(TurnDeliverySink::duplicate_outcome(plan));
    }
    let retry_persisted_reply = durable_sink.has_failed_attempt();
    let retry_ack_message_id = durable_sink.failed_edit_message_id();
    // Backward compatibility for projects upgraded from before the transport
    // ledger: composed replies used the matching session outbox row itself as
    // durable proof that the physical request had already been answered. Keep
    // honoring that proof so an upgrade replay cannot recompose or repeat task,
    // correction, inbox, or transport side effects. An explicit retry marker
    // wins: those rows were persisted before a failed delivery and still need
    // to be sent or edited by the retry path below.
    if !retry_persisted_reply
        && composer.is_some()
        && !request_id.trim().is_empty()
        && let ConversationPlan::Converse { session_ref, .. } = plan
        && chat::read_outbox_since_ref(workgraph_dir, session_ref, 0)
            .map(|outbox| {
                outbox
                    .iter()
                    .any(|message| message.request_id == request_id)
            })
            .unwrap_or(false)
    {
        println!(
            "[{}] conversation outbox already records this request — skipping upgrade replay",
            chrono::Utc::now().format("%H:%M:%S"),
        );
        return Ok(TurnDeliverySink::duplicate_outcome(plan));
    }
    // Capture the retry poll baseline before checking for an already-persisted
    // reply. If the original session answers between that check and the resumed
    // poll, its outbox row is still newer than this baseline and cannot be
    // missed. Only the legacy path needs this: composed replies are persisted
    // by this process before their transport attempt.
    let retry_legacy_baseline = if retry_persisted_reply && composer.is_none() {
        match plan {
            ConversationPlan::Converse { session_ref, .. } => {
                Some(outbox_baseline(workgraph_dir, session_ref))
            }
            ConversationPlan::Onboard { .. } | ConversationPlan::Sessionless { .. } => None,
        }
    } else {
        None
    };
    if retry_persisted_reply
        && !request_id.trim().is_empty()
        && let ConversationPlan::Converse {
            session_ref, route, ..
        } = plan
        && let Some(reply) = chat::read_outbox_since_ref(workgraph_dir, session_ref, 0)
            .ok()
            .and_then(|out| {
                out.into_iter()
                    .rev()
                    .find(|message| message.request_id == request_id)
            })
    {
        // Composer-owned rows contain their canonical, already-guarded bytes,
        // including any narrowly authorized owner handoff; do not guard those
        // a second time. A legacy session row is different: its best-effort
        // rewrite may have failed before transport did. Reapply the context-free
        // family guard so a still-dirty row can never bypass it on retry.
        let reply_text = if composer.is_none() {
            let family_roster =
                grounding::load_family_voice_roster(&project_root_of(workgraph_dir), workgraph_dir);
            guard_legacy_reply_and_sync_outbox(workgraph_dir, session_ref, &reply, &family_roster)
        } else {
            reply.content.clone()
        };
        deliver_persisted_reply(
            &durable_sink,
            route,
            retry_ack_message_id.as_deref(),
            &reply_text,
        )
        .await?;
        return Ok(TurnOutcome::Replied {
            acked: retry_ack_message_id.is_some(),
        });
    }

    match plan {
        ConversationPlan::Onboard { route, .. } => {
            let inviter = family_inviter_name(workgraph_dir);
            durable_sink
                .send(
                    &route.bot_id,
                    &route.chat_id,
                    &onboarding_line(inviter.as_deref()),
                )
                .await?;
            Ok(TurnOutcome::Onboarded)
        }
        ConversationPlan::Sessionless { route, .. } => {
            durable_sink
                .send(&route.bot_id, &route.chat_id, &sessionless_line())
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
                    &durable_sink,
                    composer,
                    &origin,
                    retry_ack_message_id.as_deref(),
                    retry_persisted_reply,
                    week_context,
                    local_date,
                )
                .await
            }
            // Legacy path (no composer injected): write the human turn to the
            // session inbox and poll the outbox for a reply a live session
            // produces. Retained for callers/tests that supply their own
            // outbox producer.
            None => {
                // Any returned transport failure belongs to the inbox turn
                // already appended by the first attempt. A failed ack send has
                // no message id, but must still resume rather than enqueue the
                // same physical turn twice. Only an edit retry carries an id.
                let resuming_failed_attempt = retry_persisted_reply;
                let baseline = retry_legacy_baseline
                    .unwrap_or_else(|| outbox_baseline(workgraph_dir, session_ref));
                if !resuming_failed_attempt {
                    chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id)?;
                }
                let outcome = await_session_reply(
                    workgraph_dir,
                    session_ref,
                    baseline,
                    request_id,
                    timing,
                    route,
                    &durable_sink,
                    retry_ack_message_id.as_deref(),
                )
                .await?;
                if matches!(outcome, TurnOutcome::TimedOut { acked: true }) {
                    // The ack landed, but the logical reply did not. Preserve
                    // its message id as retry state so a same-key attempt edits
                    // that ack when the session eventually answers.
                    durable_sink.rearm_incomplete_ack();
                }
                Ok(outcome)
            }
        },
    }
}

/// Deliver bytes that were already family-voice guarded before being persisted
/// to the session outbox. Do not guard them again: a composed reply may contain
/// a narrowly authorized owner handoff that a context-free second pass would
/// remove.
async fn deliver_persisted_reply(
    sink: &dyn ReplySink,
    route: &ReplyRoute,
    ack_mid: Option<&str>,
    text: &str,
) -> Result<()> {
    match ack_mid {
        Some(mid) if !mid.is_empty() => sink.edit(&route.bot_id, &route.chat_id, mid, text).await,
        _ => sink
            .send(&route.bot_id, &route.chat_id, text)
            .await
            .map(|_| ()),
    }
}

/// Deliver `text` to the human: edit the latency ack in place when one was sent
/// (turning the hourglass into the final answer), else send a fresh message.
async fn deliver_reply(
    sink: &dyn ReplySink,
    route: &ReplyRoute,
    ack_mid: Option<&str>,
    text: &str,
    roster: &grounding::FamilyVoiceRoster,
    authorized_handoff: Option<&str>,
    phase: crate::notify::relay_receipt::ReplyPhase,
) -> Result<()> {
    // Engine-originated replies never pass through the gateway finalizer:
    // The scoped family-reply sink mirrors the bytes sent here. Keep this as the single
    // dynamic-delivery choke point so composed replies, graph status, graceful
    // glitches, and legacy session replies all receive the same guard.
    let guarded = grounding::enforce_family_voice_with(
        text,
        roster,
        grounding::FamilyVoiceOptions { authorized_handoff },
    );
    if guarded != text {
        eprintln!(
            "[{}] family-voice guard: cleaned a dynamic reply before delivery",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }
    // THE PHASE TRAVELS WITH THE BYTES. Only a `final` consumes the turn's one
    // reservation; a graceful failure notice does not, so the real answer can
    // still be delivered later by a self-heal attempt.
    match ack_mid {
        Some(mid) if !mid.is_empty() => {
            sink.edit_phase(&route.bot_id, &route.chat_id, mid, &guarded, phase)
                .await
        }
        _ => sink
            .send_phase(&route.bot_id, &route.chat_id, &guarded, phase)
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

/// The log prefix an amendment writes, so the worker (and the audit trail) can
/// see the correction arrived after the task was minted.
pub const AMENDMENT_LOG_PREFIX: &str = "AMENDED (follow-up):";

/// Whether `task_id` is still open for amendment — i.e. present and not in a
/// terminal state. A finished/failed task is never amended: a follow-up after
/// the work landed is a genuinely new ask.
///
/// In-progress counts as open: the pizza burst's siblings arrived while the
/// first task was being claimed, which is exactly the case to coalesce.
fn task_is_open(workgraph_dir: &Path, task_id: &str) -> bool {
    let path = workgraph_dir.join("graph.jsonl");
    let Ok(graph) = crate::parser::load_graph(&path) else {
        return false;
    };
    graph
        .get_task(task_id)
        .is_some_and(|t| !t.status.is_terminal())
}

/// AMEND an open task with a rapid corrective follow-up: append the new wording
/// to its description and log it, rather than spawning a sibling task that the
/// dedupe will only abandon later (Luca's three pizza tasks).
///
/// The description append is what the worker actually reads when it claims the
/// task, so a correction that lands before the claim is honored by the same
/// single owner. The log line is the audit trail — and, because it bumps
/// `last_interaction_at`, the amendment also surfaces as live activity.
fn amend_origin_task(
    workgraph_dir: &Path,
    task_id: &str,
    human_message: &str,
    requester: &str,
) -> Result<()> {
    use crate::graph::LogEntry;
    let path = workgraph_dir.join("graph.jsonl");
    let mut graph =
        crate::parser::load_graph(&path).map_err(|e| anyhow::anyhow!("load graph: {e}"))?;
    let who = requester.trim();
    let who = if who.is_empty() { "the family" } else { who };
    let text = human_message.trim();
    if text.is_empty() {
        anyhow::bail!("empty follow-up, nothing to amend");
    }
    let now_iso = chrono::Utc::now().to_rfc3339();
    {
        let task = graph
            .get_task_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
        if task.status.is_terminal() {
            anyhow::bail!("task {task_id} is already {}", task.status);
        }
        let addition = format!("\n\nFollow-up from {who}: {text}");
        task.description = Some(match task.description.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{existing}{addition}"),
            _ => addition.trim_start().to_string(),
        });
        task.log.push(LogEntry {
            timestamp: now_iso.clone(),
            actor: None,
            user: Some(who.to_string()),
            message: format!("{AMENDMENT_LOG_PREFIX} {text}"),
        });
        task.last_interaction_at = Some(now_iso);
    }
    crate::parser::save_graph(&graph, &path).map_err(|e| anyhow::anyhow!("save graph: {e}"))?;
    Ok(())
}

/// Persist the guarded fallback before transport so a failed send/edit can
/// reuse the same canonical bytes on a same-key retry without invoking the
/// composer again.
async fn persist_and_deliver_glitch(
    workgraph_dir: &Path,
    session_ref: &str,
    request_id: &str,
    route: &ReplyRoute,
    sink: &TurnDeliverySink<'_>,
    ack_mid: Option<&str>,
    family_roster: &grounding::FamilyVoiceRoster,
) -> Result<()> {
    let reply = grounding::enforce_family_voice(&glitch_line(), family_roster);
    if let Err(error) = chat::append_outbox_ref(workgraph_dir, session_ref, &reply, request_id) {
        // If an acknowledgement already landed, release its in-flight claim so
        // a later same-key attempt can resume and replace it after storage
        // recovers. With no acknowledgement there is no transport claim yet.
        sink.rearm_incomplete_ack();
        return Err(error);
    }
    // A graceful "it glitched" line is a FAILURE notice, not the turn's answer.
    deliver_reply(
        sink,
        route,
        ack_mid,
        &reply,
        family_roster,
        None,
        crate::notify::relay_receipt::ReplyPhase::Failure,
    )
    .await
}

/// Persist and deliver a recognized historical-workout outcome without
/// exposing row data to generic conversational transforms. Both a grounded
/// answer and a fail-closed refusal are terminal: neither owes a promise audit,
/// action/task/preference side effect, repetition fallback, nor model-success
/// prerequisite.
#[allow(clippy::too_many_arguments)]
async fn persist_and_deliver_historical_workout(
    workgraph_dir: &Path,
    session_ref: &str,
    request_id: &str,
    agent_id: &str,
    route: &ReplyRoute,
    sink: &dyn ReplySink,
    ack_mid: Option<&str>,
    acked: bool,
    family_roster: &grounding::FamilyVoiceRoster,
    origin: &crate::graph::TaskOrigin,
    outcome: &grounding::HistoricalWorkoutReply,
) -> Result<TurnOutcome> {
    let reply_text = match outcome {
        grounding::HistoricalWorkoutReply::Grounded(reply) => {
            eprintln!(
                "[{}] historical-workout guard: replaced {agent_id}'s draft with terminal row-fed clauses",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply.as_str()
        }
        grounding::HistoricalWorkoutReply::InvalidContext => {
            eprintln!(
                "[{}] historical-workout guard: terminal refusal for invalid typed context for {agent_id}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            grounding::HISTORICAL_WORKOUT_REFUSAL
        }
        grounding::HistoricalWorkoutReply::NotApplicable => {
            anyhow::bail!("not-applicable workout outcome reached the terminal delivery path")
        }
    };
    println!(
        "[{}] parity: promised=none created=none agent={} chat={} lane=historical-workout-terminal",
        chrono::Utc::now().format("%H:%M:%S"),
        agent_id,
        origin.chat_id,
    );
    let guarded = grounding::enforce_family_voice(reply_text, family_roster);
    let _ = chat::append_outbox_ref(workgraph_dir, session_ref, &guarded, request_id);
    deliver_reply(
        sink,
        route,
        ack_mid,
        &guarded,
        family_roster,
        None,
        crate::notify::relay_receipt::ReplyPhase::Final,
    )
    .await?;
    Ok(TurnOutcome::Replied { acked })
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
    sink: &TurnDeliverySink<'_>,
    composer: &dyn ReplyComposer,
    origin: &crate::graph::TaskOrigin,
    retry_ack_message_id: Option<&str>,
    retrying_delivery: bool,
    week_context: Option<&str>,
    local_date: chrono::NaiveDate,
) -> Result<TurnOutcome> {
    // Load the authoritative project-local roster once for every dynamic send
    // this turn. The delivery choke point reuses it for graph answers, compose
    // failures/timeouts, and the finalized answer.
    let family_roster =
        grounding::load_family_voice_roster(&project_root_of(workgraph_dir), workgraph_dir);

    // "Are they done yet?" — a status question from someone with recent
    // origin-stamped tasks is answered from LIVE graph state, not a generic chat
    // turn. This is the honest report-back: what's in progress / done, in the
    // persona's voice, without spinning up the model.
    if !origin.requester.trim().is_empty() && lifecycle::is_status_question(human_message) {
        if let Some(answer) = answer_status_from_graph(workgraph_dir, &origin.requester) {
            let answer = grounding::enforce_family_voice(&answer, &family_roster);
            if !retrying_delivery {
                let _ =
                    chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id);
            }
            let _ = chat::append_outbox_ref(workgraph_dir, session_ref, &answer, request_id);
            deliver_reply(
                sink,
                route,
                None,
                &answer,
                &family_roster,
                None,
                crate::notify::relay_receipt::ReplyPhase::Final,
            )
            .await?;
            return Ok(TurnOutcome::Replied { acked: false });
        }
    }

    // CORRECTIONS STICK (rule 3): if the human is correcting a fact mid-chat
    // ("Nadin is not logged so ignore this"), persist it BEFORE we compose so
    // the very reply to this turn honours it (`build_compose_prompt` replays
    // every recorded correction), and so does every future turn. Best-effort.
    if !retrying_delivery && let Some(correction) = grounding::detect_correction(human_message) {
        let root = project_root_of(workgraph_dir);
        let stored = format!("{}{}", grounding::CORRECTION_PREFIX, correction);
        match parity::PreferenceStore::record(&root, &stored, &origin.requester, &origin.persona) {
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
    if !retrying_delivery {
        let _ = chat::append_inbox_ref(workgraph_dir, session_ref, human_message, request_id);
    }

    // Recognize the typed historical-workout lane before model execution. The
    // composer still runs on the normal path (preserving election/ack/model
    // lifecycle), but its success is no longer a prerequisite for a row-fed
    // answer or fail-closed refusal: an error/timeout cannot demote a recognized
    // read into the generic glitch path.
    let historical_workout = grounding::historical_week_workout_reply(
        human_message,
        week_context.unwrap_or_default(),
        local_date,
    );

    let compose = composer.compose(workgraph_dir, session_ref, agent_id, human_message);
    tokio::pin!(compose);

    let start = Instant::now();
    let mut acked = retry_ack_message_id.is_some();
    let mut ack_mid = retry_ack_message_id.map(str::to_string);

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
                        if !matches!(
                            &historical_workout,
                            grounding::HistoricalWorkoutReply::NotApplicable
                        ) {
                            return persist_and_deliver_historical_workout(
                                workgraph_dir,
                                session_ref,
                                request_id,
                                agent_id,
                                route,
                                sink,
                                ack_mid.as_deref(),
                                acked,
                                &family_roster,
                                origin,
                                &historical_workout,
                            )
                            .await;
                        }
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
                            &family_roster,
                            week_context,
                            local_date,
                        )
                        .await;
                    }
                    Err(e) => {
                        // Fail fast — surface the child's error (never a token)
                        // and give ordinary turns the graceful follow-up. A
                        // recognized historical workout has stronger typed
                        // evidence and terminates with that answer/refusal.
                        eprintln!(
                            "[{}] convo compose failed for {agent_id}: {e:#}",
                            chrono::Utc::now().format("%H:%M:%S"),
                        );
                        if !matches!(
                            &historical_workout,
                            grounding::HistoricalWorkoutReply::NotApplicable
                        ) {
                            return persist_and_deliver_historical_workout(
                                workgraph_dir,
                                session_ref,
                                request_id,
                                agent_id,
                                route,
                                sink,
                                ack_mid.as_deref(),
                                acked,
                                &family_roster,
                                origin,
                                &historical_workout,
                            )
                            .await;
                        }
                        persist_and_deliver_glitch(
                            workgraph_dir,
                            session_ref,
                            request_id,
                            route,
                            sink,
                            ack_mid.as_deref(),
                            &family_roster,
                        )
                        .await?;
                        return Ok(TurnOutcome::Glitched { acked });
                    }
                }
            }
            _ = tokio::time::sleep(sleep_for) => {
                let elapsed = start.elapsed();
                if !acked && elapsed >= timing.ack_after && elapsed < timing.reply_timeout {
                    // THE ACK IS NOT THE TURN'S ANSWER. It is stamped as the ack
                    // phase, so it does not consume the turn's one final
                    // reservation: a crash between this line and the final used
                    // to leave the family with an hourglass and the turn marked
                    // delivered, which suppressed the real answer forever.
                    ack_mid = sink
                        .send_phase(
                            &route.bot_id,
                            &route.chat_id,
                            &ack_line(),
                            crate::notify::relay_receipt::ReplyPhase::Ack,
                        )
                        .await?;
                    acked = true;
                }
                if start.elapsed() >= timing.reply_timeout {
                    eprintln!(
                        "[{}] convo compose timed out for {agent_id} after {:?}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        timing.reply_timeout,
                    );
                    if !matches!(
                        &historical_workout,
                        grounding::HistoricalWorkoutReply::NotApplicable
                    ) {
                        return persist_and_deliver_historical_workout(
                            workgraph_dir,
                            session_ref,
                            request_id,
                            agent_id,
                            route,
                            sink,
                            ack_mid.as_deref(),
                            acked,
                            &family_roster,
                            origin,
                            &historical_workout,
                        )
                        .await;
                    }
                    persist_and_deliver_glitch(
                        workgraph_dir,
                        session_ref,
                        request_id,
                        route,
                        sink,
                        ack_mid.as_deref(),
                        &family_roster,
                    )
                    .await?;
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
    family_roster: &grounding::FamilyVoiceRoster,
    week_context: Option<&str>,
    local_date: chrono::NaiveDate,
) -> Result<TurnOutcome> {
    // Whole historical dinner/training weeks remain real composed turns:
    // election, model execution, latency acknowledgement, edit-in-place
    // delivery, turn/attempt receipts, and the elected persona all stay
    // unchanged. Once the composer returns, however, the gateway's exact typed
    // rows are stronger evidence than a lossy model paraphrase. Format the final
    // from those rows BEFORE promise auditing, so discarded model prose cannot
    // create a phantom TASK_CREATE or promise side effect.
    //
    // RUN-3 C075: workout recognition and terminal delivery happen in
    // `run_composed_turn`, before this generic parity/action finalizer. Dinner
    // behavior remains unchanged: an invalid dinner block stays on its existing
    // ordinary compose path.
    let deterministic_historical = week_context.and_then(|context| {
        grounding::historical_week_dinner_reply(human_message, context, local_date)
    });
    let directive = if let Some(reply) = deterministic_historical.as_ref() {
        eprintln!(
            "[{}] historical-week guard: replaced {agent_id}'s draft with seven row-fed clauses",
            chrono::Utc::now().format("%H:%M:%S"),
        );
        lifecycle::TaskDirective {
            reply: reply.clone(),
            title: None,
        }
    } else {
        lifecycle::extract_task_directive(first_text.trim())
    };
    let mut reply_text = directive.reply.clone();
    // Audit the human-facing reply (with the machine tail already stripped) IN THE
    // CONTEXT OF THE TURN. Conditional capability copy on a turn that asked for
    // nothing ("if you need something done, just ask and I'll sort it") is an offer,
    // not a promise — live-cert C011 turned exactly that sentence into a phantom task
    // and a failure correction in the family group.
    let audit = parity::audit_promise_in_turn(human_message, &reply_text);
    // Was any work actually ASKED for on this turn? Nothing that follows may tell the
    // family a promise was missed when they never requested one.
    let asked_for_action = parity::turn_requests_action(human_message);
    let mut created: Option<String> = None;
    let mut authorized_handoff: Option<String> = None;

    // SINGLE-OWNER RULE. Before any creation, resolve who owns this ask's domain
    // from `household.toml`. Exactly one configured persona mints the task; every
    // other voice in a collective turn defers. With no valid project owner the
    // decision fails open so a real ask is not dropped.
    let root = project_root_of(workgraph_dir);
    let owner_map = ownership::OwnerMap::load(&root);
    let decision = owner_map.decide_owner(&origin.persona, human_message);

    // DEFER DISCIPLINE: the defer line must never appear on the owner's own
    // reply. `decide_owner` keys on
    // `origin.persona`, but a group-elected turn stamps that from the bot's
    // agent id — and a bot with no configured `agent_id` falls back to its bot id
    // (which need not textually equal the owner id). Correct a Defer back to
    // Owner whenever the speaking voice actually is the owner by persona or bot
    // id, so only a genuinely off-domain voice ever defers.
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
                let line = ownership::defer_line(&owner_map, &owner, domain);
                // This exact suffix is authored here, after composition, to
                // show where a re-routed ask landed. It is the only terminal
                // handoff the family-voice guard may preserve.
                authorized_handoff = Some(line.clone());
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
                    // CORRECTION COPY ONLY AFTER A GENUINE REQUESTED ACTION (live-cert
                    // C011). A correction says "I said I'd do that and it hasn't happened
                    // yet" — which is a lie, and reads as a malfunction, when the family
                    // asked for nothing at all. The fallback task above still exists so no
                    // real ask can be lost; only the apology is withheld.
                    if asked_for_action {
                        let correction = parity::correction_line();
                        if reply_text.is_empty() {
                            reply_text = correction;
                        } else {
                            reply_text.push_str("\n\n");
                            reply_text.push_str(&correction);
                        }
                    } else {
                        eprintln!(
                            "[{}] parity: suppressed a correction tail for {agent_id} — the turn requested no action",
                            chrono::Utc::now().format("%H:%M:%S"),
                        );
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
        .map(|out| {
            out.into_iter()
                // A transport-failed attempt is retried under the same request
                // id. Its persisted draft is this turn, not prior conversation;
                // excluding it preserves the intended bytes on retry.
                .filter(|m| m.request_id != request_id)
                .map(|m| m.content)
                .collect()
        })
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

    // NO DANGLING-PROMISE TAIL (rule 6, task owner-pin-engine): a DELIVERED
    // answer must not end on a fresh "let me get X's exact take / I'll get back
    // to you / let me check with <someone>" deferral. The 17:5x repro: the
    // pinned persona (Nora) acked "on it", the compose delivered the calorie
    // answer, then tacked on "…let me get Nora's exact take" — a new promise
    // that dangles forever. Runs on EVERY reply, like the anti-fabrication guard
    // (a deferral tail is volunteered, not tied to the ask shape), and strips
    // ONLY the trailing deferral clause, never emptying the reply. The persona
    // IS the delivering voice; there is no one to defer to.
    {
        let deferred = grounding::enforce_no_deferral(&reply_text, &family_roster);
        if deferred != reply_text {
            eprintln!(
                "[{}] deferral guard: stripped a dangling-promise tail from {agent_id}'s draft",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = deferred;
        }
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
        let sched = grounding::fetch_schedule_grounding(
            &project_root_of(workgraph_dir),
            now,
            human_message,
        );
        let unsourced = grounding::find_unsourced_schedule_claims(&reply_text, &sched);
        if !unsourced.is_empty() {
            eprintln!(
                "[{}] anti-fabrication guard: {agent_id}'s draft asserts unsourced schedule facts {unsourced:?} — rewriting to the honest fallback",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = grounding::grounding_fallback_line();
        }
    }

    // NEVER-CLAIM-EMPTY WEEK GUARD (task week-grounding-engine): the engine-side
    // twin of the anti-fabrication guard for the OTHER failure direction. Anti-
    // fabrication stops the composer INVENTING a schedule fact; this stops it
    // DENYING one that is right there in the plan — the LIVE Nora bug, "Nothing's
    // locked in for Saturday yet" while the Dinners table has "Saturday: Baked
    // white fish". The gateway forwards that table via `WG_WEEK_CONTEXT`; a reply
    // that asserts a planned day is empty (and doesn't already name the dish) is
    // rewritten to the honest answer. Runs on EVERY reply (like anti-fabrication)
    // and only when the env carries a table — unset → no-op. MUST live here in the
    // ENGINE process: engine-composed replies write through the scoped family-reply sink
    // here, so the gateway's own never-claim-empty guard never sees them.
    if let Some(raw) = week_context {
        let wc = grounding::parse_week_context(&raw);
        let false_empty = grounding::false_empty_week_claims(&reply_text, &wc);
        if !false_empty.is_empty() {
            eprintln!(
                "[{}] never-claim-empty guard: {agent_id}'s draft claims planned day(s) {:?} are empty — rewriting to the honest dish",
                chrono::Utc::now().format("%H:%M:%S"),
                false_empty
                    .iter()
                    .map(|(d, _)| d.as_str())
                    .collect::<Vec<_>>(),
            );
            reply_text = grounding::week_grounding_rewrite(&false_empty);
        }
        // WRONG-PLACEMENT GUARD (task meal-claim-slot, live-cert C004): the third
        // lie direction. Never-claim-empty catches DENYING a planned dish; anti-
        // fabrication catches INVENTING one; this catches MOVING one — Bruno's
        // "enjoy that frittata tomorrow … for lunch" while the table holds that
        // frittata on SUNDAY at DINNER. The dish is real and no day is called
        // empty, so neither existing guard fires, yet the family eats the wrong
        // meal on the wrong day. Runs after the never-claim-empty rewrite (whose
        // output names the real day, so it is never re-flagged) and splices only
        // the offending sentence — a casual "that frittata was great" claims no
        // placement and passes through untouched.
        let misplaced = grounding::misplaced_week_claims(&reply_text, &wc);
        if !misplaced.is_empty() {
            eprintln!(
                "[{}] wrong-placement guard: {agent_id}'s draft moves {:?} — rewriting to the plan's real placement",
                chrono::Utc::now().format("%H:%M:%S"),
                misplaced
                    .iter()
                    .map(|c| format!(
                        "{} → claimed {}{}, really {}'s dinner",
                        c.dish,
                        c.claimed_day.as_deref().unwrap_or("(no day)"),
                        c.claimed_slot
                            .as_deref()
                            .map(|s| format!(" {s}"))
                            .unwrap_or_default(),
                        c.true_day,
                    ))
                    .collect::<Vec<_>>(),
            );
            reply_text = grounding::week_placement_rewrite(&reply_text, &misplaced);
        }
    }

    // REPETITION GUARD (rule 2): never send the same summary a third time. If
    // this draft is substantially the same as the previous reply, answer
    // honestly instead — own that the answer already went out and offer to
    // actually go read the source. Delivered verbatim (style is not re-applied).
    // With turn-one grounding in place this is a backstop; the transcript shows
    // exactly why the backstop must exist.
    if deterministic_historical.is_none()
        && let Some(prev) = prior_replies.last()
    {
        if grounding::is_repetitive(&reply_text, prev) {
            eprintln!(
                "[{}] repetition guard: {agent_id}'s draft repeats its previous reply — answering honestly",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = grounding::repetition_fallback_line();
        }
    }

    // FAMILY-VISIBLE COPY GUARD. This is intentionally the LAST transform
    // before both persistence and delivery: engine replies flow from here into
    // the session outbox and the listener's scoped family-reply sink, so the gateway's
    // JavaScript finalizer never sees them. Load names only from this project's
    // household personas + live/fallback human roster, then remove self-attribution,
    // terminal persona handoffs, plumbing/process narration, machine jargon,
    // plain-text markdown, and strongly-shaped off-roster addressees.
    {
        let guarded = grounding::enforce_family_voice_with(
            &reply_text,
            family_roster,
            grounding::FamilyVoiceOptions {
                authorized_handoff: authorized_handoff.as_deref(),
            },
        );
        if guarded != reply_text {
            eprintln!(
                "[{}] family-voice guard: cleaned {agent_id}'s draft before delivery",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            reply_text = guarded;
        }
    }

    let _ = chat::append_outbox_ref(workgraph_dir, session_ref, &reply_text, request_id);
    deliver_reply(
        sink,
        route,
        ack_mid,
        &reply_text,
        family_roster,
        authorized_handoff.as_deref(),
        crate::notify::relay_receipt::ReplyPhase::Final,
    )
    .await?;
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
    // single-voice turn in the group, a parity retry, a fallback — and a
    // restart-replayed sibling of any of them.
    // finalize_composed_reply already routes the COLLECTIVE case, but its guard
    // keys on the election shape; a single-voice turn that reaches creation with
    // the answering voice as `origin.persona` could otherwise land a task on an
    // off-domain voice. So ownership is decided HERE, next to the intent dedupe,
    // independent of who called: whatever persona the caller stamped, re-route
    // ownership to the ask's configured domain owner. A voice that already owns
    // the domain, or an ask whose owner cannot be resolved from project config,
    // is left untouched (fail-open — a real ask is never dropped; the intent
    // ledger still dedupes).
    let owned_origin = match ownership::OwnerMap::load(&root)
        .decide_owner(&origin.persona, human_message)
    {
        ownership::OwnerDecision::Owner => None,
        ownership::OwnerDecision::Defer { owner } => {
            eprintln!(
                "[{}] creation choke-point off-domain guard: {} does not own a {} task — re-stamping ownership to {}",
                chrono::Utc::now().format("%H:%M:%S"),
                if origin.persona.is_empty() {
                    "an unnamed voice"
                } else {
                    origin.persona.as_str()
                },
                ownership::classify_domain(human_message).slug(),
                owner,
            );
            Some(origin_as_persona(origin, &owner))
        }
    };
    let origin = owned_origin.as_ref().unwrap_or(origin);

    let fp = ownership::fingerprint(human_message, &origin.chat_id);
    let now = chrono::Utc::now().timestamp();
    let domain = ownership::classify_domain(human_message);

    // THE THREE-TASK PIZZA SPAWN (Luca, 2026-07-24 14:27/14:28/14:29). Exact-ask
    // dedupe collapses IDENTICAL asks; it cannot collapse a human refining one
    // intent out loud ("pizza tomorrow night" → "just mozzarella" → "sorry just
    // margherita…"). Each refinement minted its own task and two were abandoned
    // as duplicates minutes later. So a rapid corrective follow-up — same chat,
    // same sender, same domain, inside the amendment window, with the first
    // task still open — AMENDS that task instead of spawning a sibling.
    match ownership::decide_creation(
        &root,
        human_message,
        &origin.chat_id,
        &origin.requester,
        now,
        |id| task_is_open(workgraph_dir, id),
    ) {
        ownership::CreateDecision::Duplicate { task_id } => {
            // The safety net fired: this exact ask already became a task inside
            // the window. Refuse the duplicate and reuse it — regardless of persona.
            println!(
                "[{}] duplicate intent, task {} already exists (persona {} chat {})",
                chrono::Utc::now().format("%H:%M:%S"),
                task_id,
                origin.persona,
                origin.chat_id,
            );
            return Some(task_id);
        }
        ownership::CreateDecision::Amend { task_id } => {
            match amend_origin_task(workgraph_dir, &task_id, human_message, &origin.requester) {
                Ok(()) => {
                    println!(
                        "[{}] rapid follow-up from {} amended task {} instead of spawning a sibling \
                         (domain {} chat {})",
                        chrono::Utc::now().format("%H:%M:%S"),
                        origin.requester,
                        task_id,
                        domain.slug(),
                        origin.chat_id,
                    );
                    // Record the new wording against the SAME task so a third
                    // refinement dedupes/amends against it too.
                    if let Err(e) = ownership::IntentLedger::record(
                        &root,
                        &fp,
                        &task_id,
                        &origin.persona,
                        &origin.chat_id,
                        &origin.requester,
                        domain,
                        now,
                    ) {
                        eprintln!(
                            "[{}] failed to record amended task intent: {e}",
                            chrono::Utc::now().format("%H:%M:%S"),
                        );
                    }
                    return Some(task_id);
                }
                Err(e) => {
                    // Fail OPEN: a real ask is never dropped. Fall through and
                    // create the task rather than lose the correction.
                    eprintln!(
                        "[{}] could not amend {task_id}, creating a fresh task instead: {e:#}",
                        chrono::Utc::now().format("%H:%M:%S"),
                    );
                }
            }
        }
        ownership::CreateDecision::Create => {}
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
            // restart-replayed message) dedupes against it — and so a rapid
            // corrective follow-up finds it to amend.
            if let Err(e) = ownership::IntentLedger::record(
                &root,
                &fp,
                &id,
                &origin.persona,
                &origin.chat_id,
                &origin.requester,
                domain,
                now,
            ) {
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
fn origin_as_persona(origin: &crate::graph::TaskOrigin, persona: &str) -> crate::graph::TaskOrigin {
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
        id == owner || id.starts_with(&format!("{owner}_")) || id.starts_with(&format!("{owner}-"))
    };
    matches_owner(&origin.persona) || origin.bot_id.as_deref().map(matches_owner).unwrap_or(false)
}

/// Persist a standing preference to the durable store under the project's
/// `.casa/`, best-effort (a write failure must never block the reply).
fn record_standing_preference(workgraph_dir: &Path, text: &str, origin: &crate::graph::TaskOrigin) {
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
    initial_ack_message_id: Option<&str>,
) -> Result<TurnOutcome> {
    let family_roster =
        grounding::load_family_voice_roster(&project_root_of(workgraph_dir), workgraph_dir);
    let start = Instant::now();
    let mut acked = initial_ack_message_id.is_some();
    let mut ack_mid = initial_ack_message_id.map(str::to_string);
    loop {
        if let Some(reply) = read_new_reply(workgraph_dir, session_ref, baseline, request_id)? {
            let guarded = guard_legacy_reply_and_sync_outbox(
                workgraph_dir,
                session_ref,
                &reply,
                &family_roster,
            );
            deliver_reply(
                sink,
                route,
                ack_mid.as_deref(),
                &guarded,
                &family_roster,
                None,
                crate::notify::relay_receipt::ReplyPhase::Final,
            )
            .await?;
            return Ok(TurnOutcome::Replied { acked });
        }
        let elapsed = start.elapsed();
        if !acked && elapsed >= timing.ack_after {
            // The turn is running long — break the silence immediately.
            ack_mid = sink
                .send_phase(
                    &route.bot_id,
                    &route.chat_id,
                    &ack_line(),
                    crate::notify::relay_receipt::ReplyPhase::Ack,
                )
                .await?;
            acked = true;
        }
        if elapsed >= timing.reply_timeout {
            return Ok(TurnOutcome::TimedOut { acked });
        }
        tokio::time::sleep(timing.poll).await;
    }
}

/// Guard a reply authored by a legacy session and keep its persisted summary in
/// sync when possible. Unlike composer-owned rows, legacy rows cannot carry an
/// authorized owner handoff, so retrying this context-free guard is safe and
/// required when a previous best-effort rewrite did not land.
fn guard_legacy_reply_and_sync_outbox(
    workgraph_dir: &Path,
    session_ref: &str,
    reply: &chat::ChatMessage,
    family_roster: &grounding::FamilyVoiceRoster,
) -> String {
    let guarded = grounding::enforce_family_voice(&reply.content, family_roster);
    if guarded != reply.content {
        // A legacy session produced the outbox entry before this bridge saw it.
        // Rewrite that exact entry so the persisted/TUI copy matches the
        // guarded scoped family-reply send.
        if let Err(error) =
            chat::edit_outbox_message_ref(workgraph_dir, session_ref, reply.id, &guarded)
        {
            eprintln!(
                "[{}] family-voice guard: could not update legacy outbox copy: {error:#}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
    }
    guarded
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

    #[test]
    fn durable_delivery_digest_has_versioned_stable_vector() {
        let digest = durable_telegram_digest_v1(
            "telegram-delivery-claim",
            &["physical-turn-42", "voice-7", "-100700"],
        );
        let expected = "b3-v1-4b104375ef8b7da0364f0eed5c0d3d89892964585c454af7859842b60ec24db9";
        assert_eq!(digest, expected);

        let dir = tempdir().unwrap();
        let inner = RecSink::default();
        let sink =
            TurnDeliverySink::new(dir.path(), "physical-turn-42", "voice-7", "-100700", &inner);
        assert_eq!(
            sink.claim_path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some(format!("{expected}.sent").as_str()),
            "the on-disk ledger filename must carry the digest version",
        );
        assert_eq!(
            sink.retry_path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some(format!("{expected}.retry").as_str()),
        );
    }

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
            self.sent.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                text.to_string(),
            ));
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

    // ── the turn's final reservation (task week-start-engine-2, set 3) ──────

    const CANON_TURN: &str = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3301";

    /// THE BARRIER GATE, in full: a LATE ORIGINAL arriving after a NEW ATTEMPT
    /// has been made, with the new attempt routed through a SECOND BOT.
    ///
    /// Keyed the old way — (delivery id, bot, chat) — these are two different
    /// claims: both send, the family gets the answer twice, and the turn ends
    /// with two rows both calling themselves the final. Keyed on the canonical
    /// turn id ALONE there is one reservation, so: ONE API call, ONE final,
    /// and (downstream) one receipt.
    #[tokio::test]
    async fn one_turn_reserves_once_across_attempts_bots_and_chats() {
        let dir = tempdir().unwrap();
        let transport = RecSink::default();

        // The new attempt goes out first, through the second bot.
        let attempt2 =
            TurnDeliverySink::new(dir.path(), CANON_TURN, "second-bot", "-100700", &transport);
        let first = attempt2
            .send("second-bot", "-100700", "the answer")
            .await
            .unwrap();
        assert!(first.is_some());

        // …and the LATE ORIGINAL turns up afterwards, on the first bot, even in
        // a different chat. It must make no API call at all.
        let late_original =
            TurnDeliverySink::new(dir.path(), CANON_TURN, "first-bot", "-100999", &transport);
        assert!(
            late_original.already_claimed(),
            "the late original did not see the turn's reservation",
        );
        let second = late_original
            .send("first-bot", "-100999", "the answer")
            .await
            .unwrap();

        assert_eq!(
            transport.calls().len(),
            1,
            "one accepted turn produced {} API calls — the family got the answer twice",
            transport.calls().len(),
        );
        assert_eq!(
            second, first,
            "the late original invented a second message id for one turn",
        );
    }

    /// The reservation is the TURN, not the words: two different turns with
    /// identical text both send.
    #[test]
    fn two_turns_are_two_reservations() {
        let dir = tempdir().unwrap();
        let a = delivery_claim_path(dir.path(), CANON_TURN, "b", "-1").unwrap();
        let b = delivery_claim_path(
            dir.path(),
            "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3302",
            "b",
            "-1",
        )
        .unwrap();
        assert_ne!(a, b, "two turns collapsed onto one reservation");
    }

    /// …and a caller with NO canonical turn id keeps exactly the guarantee it
    /// had: routing still separates it, because it cannot be part of a turn's
    /// final-answer race in the first place.
    #[test]
    fn a_turnless_caller_keeps_the_legacy_routing_key() {
        let dir = tempdir().unwrap();
        // Belt and braces: a stray WG_TURN_ID in the environment must not
        // silently re-key a legacy caller in this test.
        unsafe { std::env::remove_var("WG_TURN_ID") };
        let one = delivery_claim_path(dir.path(), "request-42", "bot-a", "-100").unwrap();
        let two = delivery_claim_path(dir.path(), "request-42", "bot-b", "-100").unwrap();
        assert_ne!(
            one, two,
            "two voices suppressed one another on a legacy path"
        );
        assert_eq!(
            delivery_claim_path(dir.path(), "   ", "bot-a", "-100"),
            None,
            "an empty delivery id must not claim anything",
        );
    }

    /// Only a canonical `web-turn-<uuid v4>` re-keys the reservation. A request
    /// id, a digest or a placeholder is not the turn, and treating one as the
    /// turn is how unrelated replies would suppress each other.
    #[test]
    fn only_a_canonical_turn_id_reserves_the_turn() {
        unsafe { std::env::remove_var("WG_TURN_ID") };
        assert_eq!(canonical_turn_id(CANON_TURN).as_deref(), Some(CANON_TURN));
        assert_eq!(canonical_turn_id("request-42"), None);
        assert_eq!(canonical_turn_id("web-turn-not-a-uuid"), None);
        assert_eq!(
            canonical_turn_id("web-turn---------------------------------"),
            None
        );
        assert_eq!(canonical_turn_id(""), None);
    }

    /// FAIL CLOSED ON AMBIGUOUS. A transport that accepts the reply but proves
    /// no message id leaves us unable to say whether the family has it. The
    /// reservation stays HELD and the call reports failure — a duplicate on the
    /// family's screen is worse than a gap an operator can see.
    #[tokio::test]
    async fn an_unproven_send_holds_the_reservation_closed() {
        #[derive(Default)]
        struct NoIdSink {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl ReplySink for NoIdSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                *self.calls.lock().unwrap() += 1;
                Ok(None) // accepted, nothing proven
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Ok(())
            }
        }
        let dir = tempdir().unwrap();
        let transport = NoIdSink::default();
        let sink = TurnDeliverySink::new(dir.path(), CANON_TURN, "bot", "-100", &transport);
        let out = sink.send("bot", "-100", "the answer").await;
        assert!(out.is_err(), "an unproven send was reported as delivered");
        assert!(
            out.unwrap_err().to_string().contains("UNPROVEN"),
            "the failure did not say the delivery was unproven",
        );

        // The reservation is still held: a retry through this sink, and a fresh
        // one for the same turn, both make NO further API call.
        let _ = sink.send("bot", "-100", "the answer").await;
        let fresh = TurnDeliverySink::new(dir.path(), CANON_TURN, "other-bot", "-100", &transport);
        assert!(
            fresh.already_claimed(),
            "the ambiguous send released the turn"
        );
        assert_eq!(
            *transport.calls.lock().unwrap(),
            1,
            "an unproven delivery was re-sent — the family may have it twice",
        );
    }

    /// RELEASE ON FAILURE, the other direction: a transport that PROVABLY failed
    /// must not leave the turn wedged. The self-heal retry has to be able to
    /// take it.
    #[tokio::test]
    async fn a_proven_failure_releases_the_reservation_for_the_retry() {
        #[derive(Default)]
        struct FailingSink {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl ReplySink for FailingSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                *self.calls.lock().unwrap() += 1;
                Err(anyhow::anyhow!("connection refused"))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Ok(())
            }
        }
        let dir = tempdir().unwrap();
        let transport = FailingSink::default();
        let sink = TurnDeliverySink::new(dir.path(), CANON_TURN, "bot", "-100", &transport);
        assert!(sink.send("bot", "-100", "the answer").await.is_err());

        let retry = TurnDeliverySink::new(dir.path(), CANON_TURN, "bot", "-100", &transport);
        assert!(
            !retry.already_claimed(),
            "a provably failed send wedged the turn — the self-heal retry can never take it",
        );
        assert!(
            retry.has_failed_attempt(),
            "the retry marker was not written"
        );
    }

    #[tokio::test]
    async fn legacy_claim_or_retry_without_canonical_never_publishes_new_bytes() {
        for (case, extension, body) in [
            ("confirmed", "sent", "message-1\n"),
            ("pending", "sent", "pending\n"),
            ("retry", "retry", "send\n"),
        ] {
            let dir = tempdir().unwrap();
            let delivery_id = format!("legacy-{case}");
            let claim_path =
                delivery_claim_path(dir.path(), &delivery_id, "voice-fixture", "-100-fixture")
                    .unwrap();
            std::fs::create_dir_all(claim_path.parent().unwrap()).unwrap();
            std::fs::write(claim_path.with_extension(extension), body).unwrap();
            let canonical_path = claim_path.with_extension("canonical");
            let sink = RecSink::default();

            let state = send_canonical_reply_once(
                dir.path(),
                &delivery_id,
                "voice-fixture",
                "-100-fixture",
                "A newly composed draft.",
                &sink,
            )
            .await
            .unwrap();

            assert_eq!(
                state,
                CanonicalDeliveryState::Unavailable,
                "{case} legacy state must fail closed",
            );
            assert!(
                !canonical_path.exists(),
                "{case} legacy state must never acquire newly composed canonical bytes",
            );
            assert!(
                sink.calls().is_empty(),
                "{case} legacy state must never reach transport",
            );
        }
    }

    #[test]
    fn canonical_temp_creation_skips_a_crash_orphan_candidate() {
        let dir = tempdir().unwrap();
        let sequence = AtomicU64::new(7);
        let process_id = 4242;
        let file_name = "fixture.canonical";
        let orphan = dir.path().join(format!(".{file_name}.tmp.{process_id}.7"));
        std::fs::write(&orphan, b"orphaned partial bytes").unwrap();

        let (path, file) =
            create_canonical_temp_file(dir.path(), file_name, process_id, &sequence).unwrap();
        drop(file);

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(format!(".{file_name}.tmp.{process_id}.8").as_str()),
        );
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            b"orphaned partial bytes",
            "the orphan is left intact for diagnosis; a fresh candidate is used",
        );
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

    fn write_owner_fixture(wg: &Path) {
        let root = project_root_of(wg);
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "nora"
domains = ["meals", "nutrition"]

[[agent]]
id = "bruno"
domains = ["meals", "cooking", "recipes"]

[[agent]]
id = "mira"
domains = ["workouts"]

[[agent]]
id = "otto"
domains = ["calendar", "coordination", "shopping"]
"#,
        )
        .unwrap();
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
        // Bare/legacy channel uses the caller's configured coordination owner.
        assert_eq!(
            bot_id_for_channel_with_default(&cfg, "telegram", Some("otto")).as_deref(),
            Some("otto")
        );
        assert_eq!(bot_id_for_channel(&cfg, "telegram"), None);
    }

    #[test]
    fn agent_for_bot_uses_only_a_nonblank_explicit_binding() {
        let cfg = cfg_with_bots(&[
            ("wire-a7", Some("relay-a7")),
            ("fallback-b4", None),
            ("fallback-c9", Some("   ")),
        ]);
        assert_eq!(agent_for_bot(&cfg, "wire-a7"), "relay-a7");
        assert_eq!(agent_for_bot(&cfg, "fallback-b4"), "fallback-b4");
        assert_eq!(agent_for_bot(&cfg, "fallback-c9"), "fallback-c9");
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

        // The resolver accepts a uniquely bound full id, name, and id prefix.
        // Unknown handles fail closed instead of inventing a session identity.
        assert_eq!(
            canonical_agent_id(wg, canonical).as_deref(),
            Some(canonical),
            "full id",
        );
        assert_eq!(canonical_agent_id(wg, "otto").as_deref(), Some(canonical),);
        assert_eq!(
            canonical_agent_id(wg, "OTTO").as_deref(),
            Some(canonical),
            "case-insensitive",
        );
        assert_eq!(
            canonical_agent_id(wg, "c10fe2fb").as_deref(),
            Some(canonical),
            "id prefix",
        );
        assert_eq!(
            canonical_agent_id(wg, "ghost"),
            None,
            "unknown fails closed",
        );

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
    fn opaque_household_alias_resolves_unrelated_agent_name() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let alias = "household-slot-a";
        let canonical = "a4f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500001";
        let tempting_name_id = "b5f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500002";
        write_agent(wg, canonical, "Unrelated Display Metadata");
        write_agent(wg, tempting_name_id, alias);

        let expected_session =
            create_session(wg, SessionKind::Interactive, &[alias.to_string()], None).unwrap();
        bind_agent(wg, canonical, &expected_session).unwrap();
        let tempting_session = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(wg, tempting_name_id, &tempting_session).unwrap();

        assert_eq!(
            canonical_agent_id(wg, alias).as_deref(),
            Some(canonical),
            "an exact session alias must beat a tempting mutable Agent.name",
        );

        let cfg = cfg_with_bots(&[("voice-router", Some(alias))]);
        confirm_human(wg, "member-fixture", "human-fixture", "voice-router");
        let first = plan_conversation(
            wg,
            &cfg,
            "telegram:voice-router",
            "chat-fixture",
            "member-fixture",
            Entry::Direct,
        );
        assert!(
            matches!(
                &first,
                ConversationPlan::Converse {
                    session_ref,
                    ..
                } if session_ref == &expected_session
            ),
            "alias routed to the wrong session: {first:?}",
        );

        write_agent(wg, canonical, "Renamed Display Metadata");
        assert_eq!(canonical_agent_id(wg, alias).as_deref(), Some(canonical),);
        let renamed = plan_conversation(
            wg,
            &cfg,
            "telegram:voice-router",
            "chat-fixture",
            "member-fixture",
            Entry::Direct,
        );
        assert!(
            matches!(
                &renamed,
                ConversationPlan::Converse {
                    session_ref,
                    ..
                } if session_ref == &expected_session
            ),
            "renaming display metadata changed alias routing: {renamed:?}",
        );
    }

    #[test]
    fn unbound_or_ambiguous_household_alias_fails_closed() {
        fn assert_sessionless(wg: &Path, alias: &str) {
            let cfg = cfg_with_bots(&[("voice-router", Some(alias))]);
            confirm_human(wg, "member-fixture", "human-fixture", "voice-router");
            let plan = plan_conversation(
                wg,
                &cfg,
                "telegram:voice-router",
                "chat-fixture",
                "member-fixture",
                Entry::Direct,
            );
            assert!(
                matches!(&plan, ConversationPlan::Sessionless { .. }),
                "unsafe alias state must not fall through to Agent.name: {plan:?}",
            );
        }

        // An exact but unbound alias is authoritative invalid state. A uniquely
        // bound Agent whose mutable name happens to equal it must not win.
        {
            let dir = tempdir().unwrap();
            let wg = dir.path();
            let alias = "household-slot-unbound";
            let tempting = "c6f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500003";
            write_agent(wg, tempting, alias);
            create_session(wg, SessionKind::Interactive, &[alias.to_string()], None).unwrap();
            let tempting_session = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(wg, tempting, &tempting_session).unwrap();
            assert_sessionless(wg, alias);
        }

        // A corrupt duplicate alias is ambiguous even when every row is bound.
        {
            let dir = tempdir().unwrap();
            let wg = dir.path();
            let alias = "household-slot-duplicate";
            let first_id = "d7f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500004";
            let tempting = "e8f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500005";
            write_agent(wg, first_id, "First Unrelated Display");
            write_agent(wg, tempting, alias);
            let first =
                create_session(wg, SessionKind::Interactive, &[alias.to_string()], None).unwrap();
            bind_agent(wg, first_id, &first).unwrap();
            let second = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(wg, tempting, &second).unwrap();
            let mut registry = crate::chat_sessions::load(wg).unwrap();
            registry
                .sessions
                .get_mut(&second)
                .unwrap()
                .aliases
                .push(alias.to_string());
            crate::chat_sessions::save(wg, &registry).unwrap();
            assert_sessionless(wg, alias);
        }

        // A unique alias whose agent id is corruptly bound to two sessions is
        // also ambiguous and must not migrate through an unrelated name.
        {
            let dir = tempdir().unwrap();
            let wg = dir.path();
            let alias = "household-slot-ambiguous";
            let target = "f9f74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500006";
            let tempting = "0af74b35c0e564f0a35886f59b55e6546aa53c77cfde4c9a34d9fcb987500007";
            write_agent(wg, target, "Second Unrelated Display");
            write_agent(wg, tempting, alias);
            let aliased =
                create_session(wg, SessionKind::Interactive, &[alias.to_string()], None).unwrap();
            bind_agent(wg, target, &aliased).unwrap();
            let duplicate_binding =
                create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            let tempting_session = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(wg, tempting, &tempting_session).unwrap();
            let mut registry = crate::chat_sessions::load(wg).unwrap();
            registry
                .sessions
                .get_mut(&duplicate_binding)
                .unwrap()
                .agent_id = Some(target.to_string());
            crate::chat_sessions::save(wg, &registry).unwrap();
            assert_sessionless(wg, alias);
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

        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:bruno",
            "-100777",
            "luca-1",
            Entry::GroupElected,
        );
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

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "bruno what's for dinner?",
            "req-2",
            fast_timing(),
            None,
            &sink,
        )
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

        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:bruno",
            "-100777",
            "luca-1",
            Entry::GroupElected,
        );
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
            assert_eq!(
                bot, "bruno",
                "composed group reply must send via the ELECTED bot"
            );
            assert_eq!(
                chat_id, "-100777",
                "composed group reply lands in the GROUP"
            );
        }
        // The final answer edits the ack in place — also via bruno.
        for (bot, chat_id, _mid, text) in sink.edits() {
            assert_eq!(
                bot, "bruno",
                "the final answer edit must also use the ELECTED bot"
            );
            assert_eq!(chat_id, "-100777");
            assert_eq!(text, "Dinner's at seven.");
        }
        // The elected bot's token is distinct from the concierge's, so a wrong-bot
        // send would have surfaced a different token — pin the mapping explicitly.
        let bruno_token = cfg
            .all_bots()
            .into_iter()
            .find(|(id, _)| id == "bruno")
            .unwrap()
            .1
            .bot_token;
        assert_eq!(bruno_token, "token-bruno");
        assert_ne!(
            bruno_token,
            cfg.all_bots()
                .into_iter()
                .find(|(id, _)| id == "otto")
                .unwrap()
                .1
                .bot_token,
            "bruno and otto must carry distinct tokens for this test to be meaningful"
        );
    }

    /// A re-fire of the SAME physical turn (same `request_id`) — a listener
    /// re-poll, a gateway retry, or a restart replay — must not compose or send
    /// a second time. A distinct occurrence id admits later identical words.
    #[tokio::test]
    async fn same_request_id_does_not_send_twice() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: uuid,
            agent_id: "persona-7".to_string(),
            route: ReplyRoute {
                bot_id: "voice-7".to_string(),
                chat_id: "-100700".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-4".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = RecSink::default();
        let composer = FakeComposer::ok("Dinner is at seven.");

        // First delivery of the turn.
        let out1 = run_conversation_turn(
            &wg,
            &plan,
            "what time is dinner?",
            "physical-turn-a",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(out1, TurnOutcome::Replied { .. }));

        // A fresh invocation simulates a listener restart. The same request id
        // finds the durable claim and produces no transport call.
        let out2 = run_conversation_turn(
            &wg,
            &plan,
            "what time is dinner?",
            "physical-turn-a",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(out2, TurnOutcome::Replied { .. }));

        // Identical words in a later physical occurrence remain answerable.
        let out3 = run_conversation_turn(
            &wg,
            &plan,
            "what time is dinner?",
            "physical-turn-b",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(out3, TurnOutcome::Replied { .. }));

        assert_eq!(
            sink.calls().len(),
            2,
            "one send per physical occurrence; refire is silent and later identical words send: {:?}",
            sink.calls()
        );
        assert!(sink.edits().is_empty());
        assert!(sink.calls().iter().all(|call| call.0 == "voice-7"));
    }

    /// Before the transport ledger existed, a matching composed outbox row was
    /// the durable proof that a request had already been answered. An upgrade
    /// replay must continue to honor that row when no explicit failed-delivery
    /// marker exists; otherwise it would compose, repeat lifecycle side effects,
    /// and send the old physical turn again.
    #[tokio::test]
    async fn preledger_composed_outbox_reply_remains_authoritative_on_upgrade() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        std::fs::write(
            wg.join("household.toml"),
            r#"
[[agent]]
id = "archive-voice"
name = "Archive Voice"
domains = ["calendar", "coordination"]
"#,
        )
        .unwrap();
        let session_ref = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        let request_id = "physical-turn-from-before-ledger";
        let human_message = "The red calendar is not correct; use the blue calendar and update it.";
        chat::append_inbox_ref(&wg, &session_ref, human_message, request_id).unwrap();
        chat::append_outbox_ref(
            &wg,
            &session_ref,
            "The calendar update was already delivered.",
            request_id,
        )
        .unwrap();
        assert!(
            !wg.join("telegram-deliveries").exists(),
            "the fixture must model a successful reply from before the ledger",
        );

        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "archive-voice".to_string(),
            route: ReplyRoute {
                bot_id: "archive-bot".to_string(),
                chat_id: "-1001500".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "archive-member".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = RecSink::default();
        let composer = SequenceComposer::new(&[
            "I will update it again.\nTASK_CREATE: update the blue calendar",
        ]);

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            human_message,
            request_id,
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Replied { acked: false });

        assert_eq!(
            composer.call_count(),
            0,
            "a pre-ledger successful outbox row must suppress recomposition",
        );
        assert!(sink.calls().is_empty(), "upgrade replay must stay silent");
        assert!(sink.edits().is_empty(), "upgrade replay must not edit");

        let inbox = chat::read_inbox_ref(&wg, &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.request_id == request_id)
                .count(),
            1,
            "upgrade replay must not append a second inbox turn",
        );
        let outbox = chat::read_outbox_since_ref(&wg, &session_ref, 0).unwrap();
        assert_eq!(
            outbox
                .iter()
                .filter(|message| message.request_id == request_id)
                .count(),
            1,
            "upgrade replay must not append a second outbox reply",
        );

        let task_count = crate::parser::load_graph(wg.join("graph.jsonl"))
            .map(|graph| graph.tasks().count())
            .unwrap_or(0);
        assert_eq!(task_count, 0, "upgrade replay must not create a task");
        let correction_count = parity::PreferenceStore::all(&wg)
            .into_iter()
            .filter(|entry| entry.text.starts_with(grounding::CORRECTION_PREFIX))
            .count();
        assert_eq!(
            correction_count, 0,
            "upgrade replay must not record the correction a second time",
        );
    }

    /// Reservation is not a tombstone for a failed transport. The exact same
    /// key retries after a confirmed send error, then becomes durable only
    /// after the retry succeeds.
    #[tokio::test]
    async fn failed_send_rearms_same_request_id_then_confirmed_send_deduplicates() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailOnceSink {
            attempts: AtomicUsize,
            delivered: AtomicUsize,
        }
        #[async_trait]
        impl ReplySink for FailOnceSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                _text: &str,
            ) -> Result<Option<String>> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub transport failure");
                }
                self.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(Some(format!("stub-{}", attempt + 1)))
            }
        }

        let dir = tempdir().unwrap();
        let session_ref = create_session(dir.path(), SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-9".to_string(),
            route: ReplyRoute {
                bot_id: "voice-9".to_string(),
                chat_id: "-100900".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-9".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailOnceSink::default();
        let composer = FakeComposer::ok("I heard you.");

        let first = run_conversation_turn(
            dir.path(),
            &plan,
            "hello household",
            "physical-turn-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await;
        assert!(first.is_err(), "the stub's first transport call must fail");

        run_conversation_turn(
            dir.path(),
            &plan,
            "hello household",
            "physical-turn-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            dir.path(),
            &plan,
            "hello household",
            "physical-turn-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
        let persisted = chat::read_outbox_since_ref(dir.path(), &session_ref, 0)
            .unwrap()
            .into_iter()
            .filter(|message| message.request_id == "physical-turn-retry")
            .collect::<Vec<_>>();
        assert_eq!(
            persisted.len(),
            1,
            "a failed send and retry share one canonical persisted reply: {persisted:?}",
        );
        assert_eq!(persisted[0].content, "I heard you.");
    }

    /// If the latency acknowledgement itself fails, the composer future is
    /// dropped before it can persist a final reply. A same-key retry may
    /// recompose, but must not repeat the already-recorded human turn or
    /// correction side effects.
    #[tokio::test]
    async fn failed_composed_ack_retry_keeps_one_inbox_turn_and_correction() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailFirstAckSink {
            sends: AtomicUsize,
            edits: AtomicUsize,
        }
        #[async_trait]
        impl ReplySink for FailFirstAckSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                _text: &str,
            ) -> Result<Option<String>> {
                let attempt = self.sends.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub acknowledgement failure");
                }
                Ok(Some("ack-retry-message".to_string()))
            }

            async fn edit(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                message_id: &str,
                _text: &str,
            ) -> Result<()> {
                assert_eq!(message_id, "ack-retry-message");
                self.edits.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tempdir().unwrap();
        let session_ref = create_session(dir.path(), SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-13".to_string(),
            route: ReplyRoute {
                bot_id: "voice-13".to_string(),
                chat_id: "-1001300".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-13".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailFirstAckSink::default();
        let composer =
            FakeComposer::ok_after("The blue calendar is current.", Duration::from_millis(150));
        let human_message = "The red calendar is not correct; use the blue calendar.";

        let first = run_conversation_turn(
            dir.path(),
            &plan,
            human_message,
            "physical-turn-ack-send-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await;
        assert!(first.is_err(), "the first acknowledgement must fail");

        run_conversation_turn(
            dir.path(),
            &plan,
            human_message,
            "physical-turn-ack-send-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            dir.path(),
            &plan,
            human_message,
            "physical-turn-ack-send-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.sends.load(Ordering::SeqCst), 2);
        assert_eq!(sink.edits.load(Ordering::SeqCst), 1);
        let inbox = chat::read_inbox_ref(dir.path(), &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-ack-send-retry")
                .count(),
            1,
            "the retry must reuse the already-persisted composed inbox turn",
        );
        let corrections = parity::PreferenceStore::all(dir.path())
            .into_iter()
            .filter(|entry| entry.text.starts_with(grounding::CORRECTION_PREFIX))
            .collect::<Vec<_>>();
        assert_eq!(
            corrections.len(),
            1,
            "the retry must not record the same correction twice",
        );
    }

    /// Composer failures use a stable guarded fallback. Persist it before the
    /// transport attempt so a same-key retry reuses those bytes without
    /// invoking the composer or appending another session row.
    #[tokio::test]
    async fn failed_glitch_send_reuses_canonical_reply_without_recomposing() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct CountingFailComposer {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl ReplyComposer for CountingFailComposer {
            async fn compose(
                &self,
                _workgraph_dir: &Path,
                _session_ref: &str,
                _agent_id: &str,
                _human_message: &str,
            ) -> Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("stub composer failure")
            }
        }

        #[derive(Default)]
        struct FailFirstGlitchSink {
            attempts: AtomicUsize,
            delivered: AtomicUsize,
            texts: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl ReplySink for FailFirstGlitchSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                self.texts.lock().unwrap().push(text.to_string());
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub fallback transport failure");
                }
                self.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(Some("glitch-retry-message".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let session_ref = create_session(dir.path(), SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-14".to_string(),
            route: ReplyRoute {
                bot_id: "voice-14".to_string(),
                chat_id: "-1001400".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-14".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailFirstGlitchSink::default();
        let composer = CountingFailComposer::default();

        let first = run_conversation_turn(
            dir.path(),
            &plan,
            "can you check this?",
            "physical-turn-glitch-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await;
        assert!(first.is_err(), "the first fallback send must fail");

        run_conversation_turn(
            dir.path(),
            &plan,
            "can you check this?",
            "physical-turn-glitch-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            dir.path(),
            &plan,
            "can you check this?",
            "physical-turn-glitch-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(
            composer.calls.load(Ordering::SeqCst),
            1,
            "the persisted fallback makes retry composition unnecessary",
        );
        assert_eq!(sink.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
        let texts = sink.texts.lock().unwrap().clone();
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], texts[1], "retry must reuse canonical bytes");

        let inbox = chat::read_inbox_ref(dir.path(), &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-glitch-retry")
                .count(),
            1,
        );
        let outbox = chat::read_outbox_since_ref(dir.path(), &session_ref, 0)
            .unwrap()
            .into_iter()
            .filter(|message| message.request_id == "physical-turn-glitch-retry")
            .collect::<Vec<_>>();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].content, texts[0]);
    }

    /// The legacy session-poll path persists its reply before transport too.
    /// After a send failure, the same-key retry reuses that reply immediately;
    /// it neither appends a duplicate inbox turn nor waits for the session to
    /// answer a second time.
    #[tokio::test]
    async fn failed_legacy_send_reuses_persisted_reply_then_deduplicates() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailOnceSink {
            attempts: AtomicUsize,
            delivered: AtomicUsize,
        }
        #[async_trait]
        impl ReplySink for FailOnceSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                _text: &str,
            ) -> Result<Option<String>> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub legacy transport failure");
                }
                self.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(Some("legacy-message-2".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let workgraph_dir = dir.path().to_path_buf();
        let session_ref =
            create_session(&workgraph_dir, SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-10".to_string(),
            route: ReplyRoute {
                bot_id: "voice-10".to_string(),
                chat_id: "-1001000".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-10".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailOnceSink::default();

        let responder_dir = workgraph_dir.clone();
        let responder_session = session_ref.clone();
        let responder = tokio::spawn(async move {
            for _ in 0..100 {
                let inbox =
                    chat::read_inbox_ref(&responder_dir, &responder_session).unwrap_or_default();
                if let Some(message) = inbox
                    .iter()
                    .find(|message| message.request_id == "physical-turn-legacy-retry")
                {
                    chat::append_outbox_ref(
                        &responder_dir,
                        &responder_session,
                        "The session already answered.",
                        &message.request_id,
                    )
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("legacy fixture did not receive its inbox turn");
        });

        let first = run_conversation_turn(
            &workgraph_dir,
            &plan,
            "is anyone there?",
            "physical-turn-legacy-retry",
            fast_timing(),
            None,
            &sink,
        )
        .await;
        responder.await.unwrap();
        assert!(first.is_err());

        run_conversation_turn(
            &workgraph_dir,
            &plan,
            "is anyone there?",
            "physical-turn-legacy-retry",
            fast_timing(),
            None,
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            &workgraph_dir,
            &plan,
            "is anyone there?",
            "physical-turn-legacy-retry",
            fast_timing(),
            None,
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
        let inbox = chat::read_inbox_ref(&workgraph_dir, &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-legacy-retry")
                .count(),
            1,
            "same-key retry must not append a second legacy inbox turn",
        );
        let outbox = chat::read_outbox_since_ref(&workgraph_dir, &session_ref, 0).unwrap();
        assert_eq!(
            outbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-legacy-retry")
                .count(),
            1,
        );
    }

    /// When the latency acknowledgement send itself fails, the failed-attempt
    /// marker still represents an already-enqueued legacy turn. A same-key
    /// retry must resume that turn without appending a second inbox row (and
    /// inviting the live session to produce a second, orphaned reply).
    #[tokio::test]
    async fn failed_legacy_ack_send_retry_keeps_one_inbox_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailFirstSendSink {
            attempts: AtomicUsize,
            delivered: AtomicUsize,
        }
        #[async_trait]
        impl ReplySink for FailFirstSendSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                _text: &str,
            ) -> Result<Option<String>> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub acknowledgement transport failure");
                }
                self.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(Some("legacy-final-message".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let workgraph_dir = dir.path().to_path_buf();
        let session_ref =
            create_session(&workgraph_dir, SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-ack-retry".to_string(),
            route: ReplyRoute {
                bot_id: "voice-ack-retry".to_string(),
                chat_id: "-1001400".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-ack-retry".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailFirstSendSink::default();
        let first_timing = AckTiming {
            ack_after: Duration::from_millis(15),
            reply_timeout: Duration::from_millis(250),
            poll: Duration::from_millis(5),
        };

        let first = run_conversation_turn(
            &workgraph_dir,
            &plan,
            "can you check?",
            "physical-turn-legacy-ack-send-retry",
            first_timing,
            None,
            &sink,
        )
        .await;
        assert!(
            first.is_err(),
            "the first latency acknowledgement must fail"
        );

        let responder_dir = workgraph_dir.clone();
        let responder_session = session_ref.clone();
        let responder = tokio::spawn(async move {
            // Give the retry enough time to append a duplicate inbox row if it
            // incorrectly treats a failed ack send as a brand-new turn.
            tokio::time::sleep(Duration::from_millis(40)).await;
            let inbox =
                chat::read_inbox_ref(&responder_dir, &responder_session).unwrap_or_default();
            let matching = inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-legacy-ack-send-retry")
                .collect::<Vec<_>>();
            for (index, message) in matching.iter().enumerate() {
                chat::append_outbox_ref(
                    &responder_dir,
                    &responder_session,
                    &format!("Session answer {}.", index + 1),
                    &message.request_id,
                )
                .unwrap();
            }
        });

        let retry_timing = AckTiming {
            ack_after: Duration::from_secs(1),
            reply_timeout: Duration::from_millis(500),
            poll: Duration::from_millis(5),
        };
        run_conversation_turn(
            &workgraph_dir,
            &plan,
            "can you check?",
            "physical-turn-legacy-ack-send-retry",
            retry_timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        responder.await.unwrap();

        assert_eq!(sink.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
        let inbox = chat::read_inbox_ref(&workgraph_dir, &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| { message.request_id == "physical-turn-legacy-ack-send-retry" })
                .count(),
            1,
            "the failed ack already belongs to the original inbox turn",
        );
        let outbox = chat::read_outbox_since_ref(&workgraph_dir, &session_ref, 0).unwrap();
        assert_eq!(
            outbox
                .iter()
                .filter(|message| { message.request_id == "physical-turn-legacy-ack-send-retry" })
                .count(),
            1,
            "one physical turn must not leave an orphaned second session reply",
        );
    }

    /// A latency acknowledgement can outlive the first polling window while the
    /// original session is still working. A same-key retry resumes that wait
    /// and edits the existing acknowledgement; it must not enqueue the same
    /// human turn a second time.
    #[tokio::test]
    async fn timed_out_legacy_retry_resumes_one_inbox_turn_and_edits_original_ack() {
        let dir = tempdir().unwrap();
        let workgraph_dir = dir.path().to_path_buf();
        let session_ref =
            create_session(&workgraph_dir, SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-12".to_string(),
            route: ReplyRoute {
                bot_id: "voice-12".to_string(),
                chat_id: "-1001200".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-12".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = RecSink::default();
        let first_timing = AckTiming {
            ack_after: Duration::from_millis(15),
            reply_timeout: Duration::from_millis(70),
            poll: Duration::from_millis(5),
        };

        let first = run_conversation_turn(
            &workgraph_dir,
            &plan,
            "can you check the plan?",
            "physical-turn-timeout-retry",
            first_timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        assert_eq!(first, TurnOutcome::TimedOut { acked: true });
        assert_eq!(sink.calls().len(), 1, "the first attempt sends one ack");
        assert!(sink.edits().is_empty());

        let responder_dir = workgraph_dir.clone();
        let responder_session = session_ref.clone();
        let responder = tokio::spawn(async move {
            // Land the answer after the retry has entered its resumed poll.
            tokio::time::sleep(Duration::from_millis(40)).await;
            let inbox =
                chat::read_inbox_ref(&responder_dir, &responder_session).unwrap_or_default();
            let matching = inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-timeout-retry")
                .collect::<Vec<_>>();
            assert_eq!(
                matching.len(),
                1,
                "the same physical turn must occupy one legacy inbox row",
            );
            chat::append_outbox_ref(
                &responder_dir,
                &responder_session,
                "The original session finished.",
                &matching[0].request_id,
            )
            .unwrap();
        });

        let retry_timing = AckTiming {
            ack_after: Duration::from_millis(15),
            reply_timeout: Duration::from_millis(300),
            poll: Duration::from_millis(5),
        };
        let second = run_conversation_turn(
            &workgraph_dir,
            &plan,
            "can you check the plan?",
            "physical-turn-timeout-retry",
            retry_timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        responder.await.unwrap();
        assert_eq!(second, TurnOutcome::Replied { acked: true });

        // A later refire sees the completed delivery record and stays silent.
        run_conversation_turn(
            &workgraph_dir,
            &plan,
            "can you check the plan?",
            "physical-turn-timeout-retry",
            retry_timing,
            None,
            &sink,
        )
        .await
        .unwrap();

        let inbox = chat::read_inbox_ref(&workgraph_dir, &session_ref).unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|message| message.request_id == "physical-turn-timeout-retry")
                .count(),
            1,
        );
        assert_eq!(
            sink.calls().len(),
            1,
            "retry edits the original acknowledgement; it never sends another",
        );
        let edits = sink.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].2, "1", "the retry edits the first ack message");
        assert_eq!(edits[0].3, "The original session finished.");
    }

    /// A delivered latency acknowledgement is not the completed logical reply.
    /// If replacing it with the final answer fails, the same-key retry edits the
    /// original acknowledgement instead of sending a second message or being
    /// suppressed by the pending claim.
    #[tokio::test]
    async fn failed_ack_edit_retries_same_message_then_confirmed_edit_deduplicates() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailFirstEditSink {
            sends: AtomicUsize,
            edit_attempts: AtomicUsize,
            edited: AtomicUsize,
        }
        #[async_trait]
        impl ReplySink for FailFirstEditSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                _text: &str,
            ) -> Result<Option<String>> {
                self.sends.fetch_add(1, Ordering::SeqCst);
                Ok(Some("ack-message-7".to_string()))
            }

            async fn edit(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                message_id: &str,
                _text: &str,
            ) -> Result<()> {
                assert_eq!(message_id, "ack-message-7");
                let attempt = self.edit_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub edit failure");
                }
                self.edited.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tempdir().unwrap();
        let session_ref = create_session(dir.path(), SessionKind::Interactive, &[], None).unwrap();
        let plan = ConversationPlan::Converse {
            session_ref: session_ref.clone(),
            agent_id: "persona-11".to_string(),
            route: ReplyRoute {
                bot_id: "voice-11".to_string(),
                chat_id: "-1001100".to_string(),
            },
            entry: Entry::GroupElected,
            requester: "member-11".to_string(),
            channel: crate::graph::OriginChannel::TelegramGroup,
        };
        let sink = FailFirstEditSink::default();
        let composer = FakeComposer::ok_after("The answer is ready.", Duration::from_millis(150));

        let first = run_conversation_turn(
            dir.path(),
            &plan,
            "can you check that?",
            "physical-turn-edit-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await;
        assert!(first.is_err(), "the first final-answer edit must fail");

        run_conversation_turn(
            dir.path(),
            &plan,
            "can you check that?",
            "physical-turn-edit-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            dir.path(),
            &plan,
            "can you check that?",
            "physical-turn-edit-retry",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(
            sink.sends.load(Ordering::SeqCst),
            1,
            "retry edits the original acknowledgement; it never posts another",
        );
        assert_eq!(sink.edit_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(sink.edited.load(Ordering::SeqCst), 1);
        let persisted = chat::read_outbox_since_ref(dir.path(), &session_ref, 0)
            .unwrap()
            .into_iter()
            .filter(|message| message.request_id == "physical-turn-edit-retry")
            .collect::<Vec<_>>();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].content, "The answer is ready.");
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
        let last = sink.edits().last().map(|e| e.3.clone()).unwrap_or(text);
        assert!(!last.contains("TASK_CREATE"), "directive leaked: {last}");
        assert!(
            !last.to_lowercase().contains("snag"),
            "no correction expected: {last}"
        );
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
        // The correction is honest and in plain family words — and carries none of
        // the operations vocabulary the live-cert C011 correction leaked
        // (task capability-answer-no-invented-work).
        let low = last.to_lowercase();
        assert!(
            low.contains("hasn't happened yet"),
            "expected correction: {last}"
        );
        for banned in ["coordinator", "flagged", "snag", "slip"] {
            assert!(
                !low.contains(banned),
                "correction leaked {banned:?}: {last}"
            );
        }
    }

    /// PARITY, INTENT-AWARE (task capability-answer-no-invented-work, live-cert run 2
    /// C011). The 2026-07-26 21:18 family-group failure end to end: Luca asked what the
    /// house can help with, the composer answered with conditional capability copy
    /// ("…just ask and I'll either sort it…") and emitted no directive. The old flat
    /// audit read that as a broken promise, so the turn minted a task
    /// (`follow-up-on-chat-request-2`) AND appended a correction about work that never
    /// existed. Now: no retry, no task, no correction — just the answer.
    #[tokio::test]
    async fn a_capability_answer_creates_no_task_and_gets_no_correction() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("nora", Some("nora"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "nora");

        let plan = plan_conversation(&wg, &cfg, "telegram:nora", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer = SequenceComposer::new(&[
            "Oh, lots of things! I keep an eye on the calendar and let you know what's \
             coming up. If you need something done or have a question about what's going \
             on, just ask and I'll either sort it or let you know what we need to do. 🙂",
        ]);

        run_conversation_turn(
            &wg,
            &plan,
            "What kinds of things can you help with?",
            "req-parity-capability",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(
            composer.call_count(),
            1,
            "a capability answer owes no artifact, so there is nothing to retry",
        );
        // No task at all — often not even a graph file, since nothing was written.
        if let Ok(graph) = crate::parser::load_graph(wg.join("graph.jsonl")) {
            assert!(
                graph.tasks().all(|t| t.origin.is_none()),
                "a capability ask must create NO task: {:?}",
                graph.tasks().map(|t| t.title.clone()).collect::<Vec<_>>(),
            );
        }
        let delivered = sink
            .edits()
            .last()
            .map(|e| e.3.clone())
            .or_else(|| sink.calls().last().map(|c| c.2.clone()))
            .unwrap();
        let low = delivered.to_lowercase();
        for banned in [
            "hasn't happened yet",
            "coordinator",
            "flagged",
            "snag",
            "slip",
        ] {
            assert!(
                !low.contains(banned),
                "the capability answer carried correction/ops copy ({banned:?}): {delivered}",
            );
        }
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
        assert_eq!(
            composer.call_count(),
            1,
            "no retry for a non-committal reply"
        );
        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).ok();
        let any_task = graph.map(|g| g.tasks().next().is_some()).unwrap_or(false);
        assert!(!any_task, "no task should be created for small talk");
    }

    /// Bind a persona's bot + session and (idempotently) confirm the human, so a
    /// `GroupElected` plan for that voice resolves to `Converse`. Returns the
    /// four-bot config the collective tests share.
    fn setup_collective(wg: &Path) -> TelegramConfig {
        write_owner_fixture(wg);
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

    /// A self-contained, household-independent roster for ownership regression
    /// tests. The returned ids are routing keys; assertions read the expected
    /// owner and family-visible label back from the authored configuration.
    fn setup_opaque_collective(wg: &Path) -> (TelegramConfig, Vec<String>) {
        std::fs::write(
            project_root_of(wg).join("household.toml"),
            r#"
[[agent]]
id = "meal-7"
name = "Copper Ladle"
domains = ["meals", "nutrition"]

[[agent]]
id = "recipe-4"
name = "Kitchen Lantern"
domains = ["cooking", "recipes"]

[[agent]]
id = "motion-2"
name = "Bright Steps"
domains = ["workouts"]

[[agent]]
id = "coord-9"
name = "Home Compass"
domains = ["calendar", "coordination", "shopping"]
"#,
        )
        .unwrap();
        let personas: Vec<String> = ["meal-7", "recipe-4", "motion-2", "coord-9"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let bot_specs: Vec<(&str, Option<&str>)> = personas
            .iter()
            .map(|id| (id.as_str(), Some(id.as_str())))
            .collect();
        let cfg = cfg_with_bots(&bot_specs);
        for persona in &personas {
            let uuid = create_session(wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(wg, persona, &uuid).unwrap();
        }
        add_binding_for_bot(wg, "member-1", "Household Member", true, "coord-9");
        (cfg, personas)
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

    async fn run_voice_as(
        wg: &Path,
        cfg: &TelegramConfig,
        persona: &str,
        chat: &str,
        requester: &str,
        ask: &str,
        reply_with_tail: &str,
        sink: &RecSink,
    ) {
        let plan = plan_conversation(
            wg,
            cfg,
            &format!("telegram:{persona}"),
            chat,
            requester,
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

    /// A single collective meal ask elects the whole configured roster; each
    /// voice composes a reply that would create its own task. With the
    /// single-owner rule + intent dedupe, exactly one task survives under the
    /// configured meal owner, and off-domain voices name only the roster-authored
    /// family label.
    #[tokio::test]
    async fn collective_meal_ask_uses_configured_owner() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let (cfg, personas) = setup_opaque_collective(&wg);
        let chat = "-100999";
        let ask = "swap Thursday dinner to grilled tofu";

        let owner_map = ownership::OwnerMap::load(&project_root_of(&wg));
        let expected_owner = owner_map
            .owner_for_ask(ask)
            .expect("the opaque roster configures a meal owner")
            .to_string();
        let expected_label = owner_map
            .display_names()
            .find(|(id, _)| id.eq_ignore_ascii_case(&expected_owner))
            .map(|(_, name)| name.to_string())
            .expect("the configured owner has an authored display name");

        // Run the configured owner first, then every off-domain voice with a
        // different title. This proves dedupe keys on the ask rather than title.
        let owner_sink = RecSink::default();
        run_voice_as(
            &wg,
            &cfg,
            &expected_owner,
            chat,
            "member-1",
            ask,
            "Grilled tofu Thursday it is 🥗\nTASK_CREATE: swap Thursday dinner to grilled tofu",
            &owner_sink,
        )
        .await;
        let mut first_off_domain_delivery = None;
        for (index, persona) in personas
            .iter()
            .filter(|persona| !persona.eq_ignore_ascii_case(&expected_owner))
            .enumerate()
        {
            let sink = RecSink::default();
            let reply = format!(
                "That sounds good.\nTASK_CREATE: alternate meal update {}",
                index + 1
            );
            run_voice_as(&wg, &cfg, persona, chat, "member-1", ask, &reply, &sink).await;
            if first_off_domain_delivery.is_none() {
                first_off_domain_delivery = sink.calls().last().map(|call| call.2.clone());
            }
        }

        // Exactly one task exists, owned by the configured meal owner.
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
        assert_eq!(
            owner.persona, expected_owner,
            "the configured meal-domain owner owns the task",
        );
        assert_eq!(stamped[0].title, "swap Thursday dinner to grilled tofu");

        // No off-domain voice can own a meal task.
        assert!(
            graph.tasks().all(|task| task
                .origin
                .as_ref()
                .is_none_or(|origin| origin.persona == expected_owner)),
            "an off-domain persona acquired the configured meal task",
        );

        // The handoff uses authored family presentation, never the routing id.
        let handoff = first_off_domain_delivery.expect("an off-domain voice replied");
        assert!(
            handoff.contains(&expected_label),
            "the handoff must use the configured owner label: {handoff:?}",
        );
        assert!(
            !handoff.contains(&expected_owner),
            "the family-visible handoff leaked the opaque routing id: {handoff:?}",
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
        write_owner_fixture(&wg);
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

    /// THE THREE-TASK PIZZA SPAWN (Luca, 2026-07-24 14:27/14:28/14:29). Three
    /// rapid messages refining ONE intent each minted their own task; the dedupe
    /// then abandoned two as duplicates, and each abandon spoke a false "I'll
    /// take another crack at it" to the family. Driven through the REAL creation
    /// choke point: the follow-ups must AMEND the first task, so exactly one task
    /// exists and nothing is ever there to be abandoned.
    #[test]
    fn rapid_pizza_follow_ups_amend_one_task_instead_of_spawning_three() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let origin = crate::graph::TaskOrigin::new(
            crate::graph::OriginChannel::TelegramGroup,
            "8905220378",
            "Luca",
            "nora",
            Some("nora".to_string()),
        );

        // 14:27 — the ask lands and mints one task.
        let first = try_create_origin_task(
            &wg,
            "can we do pizza tomorrow night",
            "Update Saturday dinner plan to pizza",
            &origin,
        )
        .expect("the first ask creates a task");

        // 14:28 and 14:29 — refinements. Same human, same chat, same subject.
        let second = try_create_origin_task(
            &wg,
            "just mozzarella",
            "Change Saturday dinner from Pasta to Mozzarella Pizza",
            &origin,
        );
        let third = try_create_origin_task(
            &wg,
            "sorry just margherita pizza for saturday",
            "prep margherita pizza for saturday dinner tomorrow",
            &origin,
        );
        assert_eq!(second.as_deref(), Some(first.as_str()), "follow-up amends");
        assert_eq!(third.as_deref(), Some(first.as_str()), "follow-up amends");

        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let stamped: Vec<_> = graph.tasks().filter(|t| t.origin.is_some()).collect();
        assert_eq!(
            stamped.len(),
            1,
            "one exchange, ONE task — got {:?}",
            stamped.iter().map(|t| &t.id).collect::<Vec<_>>()
        );

        // The corrections are not lost: they reach the worker via the description
        // and are recorded in the log.
        let task = stamped[0];
        let desc = task.description.clone().unwrap_or_default();
        assert!(desc.contains("just mozzarella"), "amendment kept: {desc}");
        assert!(desc.contains("margherita"), "amendment kept: {desc}");
        assert!(desc.contains("Follow-up from Luca"), "attributed: {desc}");
        assert_eq!(
            task.log
                .iter()
                .filter(|e| e.message.starts_with(AMENDMENT_LOG_PREFIX))
                .count(),
            2,
            "each correction is logged once"
        );
        assert!(!task.status.is_terminal(), "the survivor is still live");
    }

    /// A follow-up that arrives AFTER the task finished is a genuinely new ask —
    /// amendment must never resurrect closed work.
    #[test]
    fn a_follow_up_after_the_work_landed_creates_a_new_task() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let origin = crate::graph::TaskOrigin::new(
            crate::graph::OriginChannel::TelegramGroup,
            "8905220379",
            "Luca",
            "nora",
            Some("nora".to_string()),
        );
        let first = try_create_origin_task(
            &wg,
            "can we do pizza tomorrow night",
            "Update Saturday dinner plan to pizza",
            &origin,
        )
        .unwrap();

        // The work lands.
        let path = wg.join("graph.jsonl");
        let mut graph = crate::parser::load_graph(&path).unwrap();
        graph.get_task_mut(&first).unwrap().status = crate::graph::Status::Done;
        crate::parser::save_graph(&graph, &path).unwrap();

        let second = try_create_origin_task(
            &wg,
            "actually make it margherita",
            "Change Saturday pizza to margherita",
            &origin,
        )
        .unwrap();
        assert_ne!(second, first, "a closed task is never amended");
        let graph = crate::parser::load_graph(&path).unwrap();
        assert_eq!(graph.tasks().filter(|t| t.origin.is_some()).count(), 2);
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

    #[tokio::test]
    async fn missing_household_never_invents_an_owner_handoff() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("hearth", Some("hearth"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "hearth", &uuid).unwrap();
        confirm_human(&wg, "member-1", "human-member", "hearth");

        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:hearth",
            "-100404",
            "member-1",
            Entry::GroupElected,
        );
        let composer =
            FakeComposer::ok("Thursday soup is noted.\nTASK_CREATE: move Thursday dinner to soup");
        let sink = RecSink::default();
        run_conversation_turn(
            &wg,
            &plan,
            "swap Thursday dinner to soup",
            "req-no-household-owner",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        let delivered = sink
            .edits()
            .last()
            .map(|edit| edit.3.clone())
            .or_else(|| sink.calls().last().map(|call| call.2.clone()))
            .unwrap_or_default();
        assert_eq!(delivered, "Thursday soup is noted.");
        assert!(
            !delivered.contains("got this one"),
            "no project roster means no named owner handoff: {delivered:?}",
        );

        let graph = crate::parser::load_graph(wg.join("graph.jsonl")).unwrap();
        let created = graph
            .tasks()
            .find(|task| task.origin.is_some())
            .expect("the real ask still creates one task");
        assert_eq!(
            created.origin.as_ref().unwrap().persona,
            "hearth",
            "missing ownership config fails open to the speaking persona",
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
            &wg,
            &cfg,
            "otto",
            chat,
            ask,
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
            (
                "update Friday dinner to carbonara",
                "update Friday dinner",
                "nora",
            ),
            (
                "what's a good recipe for the tofu?",
                "share a tofu recipe",
                "bruno",
            ),
            (
                "can we move my gym session to Friday?",
                "reschedule gym to Friday",
                "mira",
            ),
            (
                "book a dentist appointment next week",
                "book the dentist",
                "otto",
            ),
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
                &wg,
                &cfg,
                "otto",
                &chat,
                ask,
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
            (
                "nora",
                "I'll flag it.\nTASK_CREATE: move the gym session to Friday",
            ),
            ("bruno", "Sure.\nTASK_CREATE: shift gym to Friday"),
            (
                "mira",
                "On it — Friday works 💪\nTASK_CREATE: reschedule gym session to Friday",
            ),
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
    async fn family_voice_guard_cleans_graph_status_delivery() {
        use crate::graph::{Node, OriginChannel, Status, Task, TaskOrigin, WorkGraph};
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("otto", Some("otto"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "otto");

        // Seed an in-progress task whose family-facing title contains markdown.
        // Status copy is assembled after the composer finalizer, so only the
        // dynamic-delivery choke point can clean it.
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "tweak-w29-meals".into(),
            title: "**weekly refresh**".into(),
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
        assert!(
            !text.contains("**"),
            "status markdown bypassed delivery: {text}"
        );
        assert!(
            text.contains("weekly refresh"),
            "status title was lost: {text}"
        );
        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(text.as_str()),
            "graph status must persist the same guarded bytes it sends"
        );
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
        assert!(
            text.contains(lifecycle::FOLLOW_ACK),
            "follow ack appended: {text}"
        );
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

        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "you there?",
            "req-3",
            fast_timing(),
            None,
            &sink,
        )
        .await
        .unwrap();
        responder.await.unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: true });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "the ack is the only fresh send: {calls:?}");
        assert_eq!(calls[0].0, "otto");
        assert_eq!(calls[0].1, "555");
        assert!(calls[0].2.contains("On it"));
        let edits = sink.edits();
        assert_eq!(
            edits.len(),
            1,
            "the final reply replaces the ack: {edits:?}"
        );
        assert_eq!(edits[0].0, "otto");
        assert_eq!(edits[0].1, "555");
        assert_eq!(edits[0].3, "here at last");
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
            assert_eq!(
                out.last().unwrap().content,
                "Yep — dinner's at seven, see you there!"
            );
        }
    }

    /// Induced failure: the composer errors (the production analogue is the
    /// `claude` child dying / non-zero exit / auth failure). The human gets the
    /// graceful "glitched" follow-up fast — never a permanent hourglass, never
    /// silence.
    #[tokio::test]
    async fn family_voice_guard_preserves_clean_glitch_delivery() {
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

        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must fail fast, not hang"
        );
        assert_eq!(outcome, TurnOutcome::Glitched { acked: false });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2,
            glitch_line(),
            "the clean glitch line survives the delivery guard byte-for-byte"
        );
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
        let composer =
            FakeComposer::ok_after("Here at last — all sorted!", Duration::from_millis(200));

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

    /// A historical whole-week read remains a real composed turn: the elected cook's
    /// composer still runs long enough to produce the normal acknowledgement/edit
    /// lifecycle, but the final bytes come deterministically from the exact seven
    /// forwarded plan rows rather than from the model's lossy paraphrase.
    #[tokio::test]
    async fn historical_week_dinner_final_is_row_fed_after_real_ack_lifecycle() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("bruno", Some("bruno"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "bruno", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "bruno");
        let plan = plan_conversation(&wg, &cfg, "telegram:bruno", "555", "luca-1", Entry::Direct);
        assert!(matches!(plan, ConversationPlan::Converse { .. }));

        let context = "Requested historical dinner plan (2026-07-20 through 2026-07-26),\n\
                       selected from the one exact indexed plan for that civil-date range. Answer the\n\
                       dated request FROM these seven rows, in the order shown. Do not substitute the\n\
                       current week, another week, or another day's row:\n\
                       - Monday (Jul 20): Mushroom risotto, finished with spinach & lemon\n\
                       - Tuesday (Jul 21): Pan-seared duck breast, roast potatoes & a quick salad\n\
                       - Wednesday (Jul 22): No cooking \u{2014} Luca's out this evening\n\
                       - Thursday (Jul 23): Pasta al pomodoro\n\
                       - Friday (Jul 24): Pan seared pork\n\
                       - Saturday (Jul 25): Pizza margherita, homemade dough\n\
                       - Sunday (Jul 26): Clear-the-fridge frittata, greens folded through";
        let expected = "Monday was Mushroom risotto, finished with spinach and lemon. \
                        Tuesday was Pan-seared duck breast, roast potatoes and a quick salad. \
                        Wednesday, July 22 was out, no cooking \u{2014} Luca was out that evening. \
                        Thursday was Pasta al pomodoro. \
                        Friday was Pan seared pork. \
                        Saturday was Pizza margherita, homemade dough. \
                        Sunday was Clear-the-fridge frittata, greens folded through.";
        let sink = RecSink::default();
        // Deliberately reproduce the lossy live draft and delay past ack_after. The
        // deterministic final must replace these model bytes without bypassing compose.
        // The bogus directive proves replacement happens before promise auditing and
        // task creation, not merely as a late presentation rewrite.
        let composer = FakeComposer::ok_after(
            "Monday through Sunday: Monday risotto - Tuesday duck - Wednesday out - \
             Thursday pasta - Friday pork - Saturday pizza - Sunday frittata.\n\
             TASK_CREATE: rewrite the whole dinner plan",
            Duration::from_millis(200),
        );

        let outcome = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Give me the July 20\u{2013}26 dinners in order.",
            "req-c034",
            fast_timing(),
            Some(&composer),
            &sink,
            Some(context),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 30).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: true });
        let calls = sink.calls();
        assert_eq!(
            calls.len(),
            1,
            "the only fresh send must be the ack: {calls:?}"
        );
        assert_eq!(calls[0].0, "bruno");
        assert!(calls[0].2.contains("On it"), "first send is not an ack");
        let edits = sink.edits();
        assert_eq!(
            edits.len(),
            1,
            "the final must edit the ack once: {edits:?}"
        );
        assert_eq!(edits[0].0, "bruno");
        assert_eq!(edits[0].2, "1");
        assert_eq!(edits[0].3, expected);

        if let ConversationPlan::Converse { session_ref, .. } = &plan {
            let out = chat::read_outbox_since_ref(&wg, session_ref, 0).unwrap();
            assert_eq!(
                out.last().map(|message| message.content.as_str()),
                Some(expected),
                "outbox and Telegram final diverged",
            );
        }
        let task_count = crate::parser::load_graph(wg.join("graph.jsonl"))
            .map(|graph| graph.tasks().count())
            .unwrap_or(0);
        assert_eq!(
            task_count, 0,
            "the discarded model directive created a phantom task",
        );
    }

    const C075_WORKOUT_CONTEXT: &str = "WG_HISTORICAL_WORKOUT_CONTEXT_V1\n\
                       week_key=2026-W30\n\
                       range_start=2026-07-20\n\
                       range_end=2026-07-26\n\
                       row=2026-07-20|monday|07:00|Lower (strength)\n\
                       row=2026-07-22|wednesday|07:00|Upper (push)\n\
                       row=2026-07-24|friday|07:00|Upper (pull)\n\
                       row=2026-07-26|sunday|10:00|Active recovery\n\
                       END_WG_HISTORICAL_WORKOUT_CONTEXT_V1";

    const C075_WORKOUT_REPLY: &str = "The July 20-26 training had three lifting sessions and one active recovery. \
         Monday lower at 7 a.m., Wednesday upper push at 7 a.m., \
         Friday upper pull at 7 a.m., and Sunday active recovery at 10 a.m.";

    fn assert_no_conversation_action_artifacts(wg: &Path) {
        let task_count = crate::parser::load_graph(wg.join("graph.jsonl"))
            .map(|graph| graph.tasks().count())
            .unwrap_or(0);
        assert_eq!(task_count, 0, "historical read reached the task graph");
        assert!(
            !wg.join(".casa/intents.jsonl").exists(),
            "historical read reached the intent ledger",
        );
        assert!(
            !wg.join(".casa/preferences.jsonl").exists(),
            "historical read reached the preference ledger",
        );
    }

    /// RUN-3 C075: the exact historical workout rows win after the real compose
    /// and engine ack lifecycle, before a model promise/TASK_CREATE can mutate
    /// the graph or intent ledger. Every delivered word below is derived by the
    /// grounding helper from the forwarded dates, titles, and times.
    #[tokio::test]
    async fn historical_week_workout_final_suppresses_model_task_creation() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("mira", Some("mira"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "mira", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "mira");
        let plan = plan_conversation(&wg, &cfg, "telegram:mira", "555", "luca-1", Entry::Direct);
        assert!(matches!(plan, ConversationPlan::Converse { .. }));

        let sink = RecSink::default();
        let composer = FakeComposer::ok_after(
            "Got it — pulling the full week's training right now and I'll get you that summary!\n\
             TASK_CREATE: Summarize Luca's July 20-26 training week",
            Duration::from_millis(200),
        );

        let outcome = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Summarize the July 20\u{2013}26 training.",
            "req-c075",
            fast_timing(),
            Some(&composer),
            &sink,
            Some(C075_WORKOUT_CONTEXT),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: true });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "only the engine ack is a fresh send");
        assert_eq!(calls[0].0, "mira");
        assert!(calls[0].2.contains("On it"));
        let edits = sink.edits();
        assert_eq!(edits.len(), 1, "the row-fed answer edits the ack");
        assert_eq!(edits[0].0, "mira");
        assert_eq!(edits[0].3, C075_WORKOUT_REPLY);

        // A second physical turn is allowed to receive the same truthful
        // answer. Generic repetition handling must not replace typed evidence
        // with a "go read the source" fallback.
        let repeated_composer = FakeComposer::ok("A model paraphrase that must be discarded.");
        let repeated = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Summarize the July 20–26 training.",
            "req-c075-repeat",
            fast_timing(),
            Some(&repeated_composer),
            &sink,
            Some(C075_WORKOUT_CONTEXT),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(repeated, TurnOutcome::Replied { acked: false });
        assert_eq!(sink.calls().last().unwrap().2, C075_WORKOUT_REPLY);

        assert_no_conversation_action_artifacts(&wg);
    }

    /// A row title is inert evidence even when one of its words is an action
    /// token to the generic parity classifier (`putting`). It remains visible,
    /// receives a neutral session count, and cannot trigger retry/task/intent
    /// side effects.
    #[tokio::test]
    async fn historical_workout_action_token_title_is_terminal_data() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("mira", Some("mira"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "mira", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "mira");
        let plan = plan_conversation(&wg, &cfg, "telegram:mira", "555", "luca-1", Entry::Direct);
        let context = C075_WORKOUT_CONTEXT.replacen("Upper (push)", "Putting practice", 1);
        let expected = "The July 20-26 training had four sessions. \
                        Monday lower at 7 a.m., Wednesday putting practice at 7 a.m., \
                        Friday upper pull at 7 a.m., and Sunday active recovery at 10 a.m.";
        let sink = RecSink::default();
        // If generic parity sees the row's word "putting", it retries and the
        // hostile second response would create a task. Terminal handling must
        // stop after the one ordinary compose call.
        let composer = SequenceComposer::new(&[
            "Here is the requested training summary.",
            "I'll put that together.\nTASK_CREATE: put together Luca's training summary",
        ]);

        let outcome = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Summarize the July 20-26 training.",
            "req-c075-putting",
            fast_timing(),
            Some(&composer),
            &sink,
            Some(&context),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: false });
        assert_eq!(sink.calls().len(), 1);
        assert_eq!(sink.calls()[0].2, expected);
        assert_eq!(composer.call_count(), 1, "promise parity retried the model");
        assert_no_conversation_action_artifacts(&wg);
    }

    /// Recognition, not successful parsing, is the no-task boundary. A typed
    /// C075 request with malformed context receives one neutral final and never
    /// falls back to the model directive that caused the live side effects.
    #[tokio::test]
    async fn malformed_historical_workout_context_refuses_without_task() {
        let dir = tempdir().unwrap();
        let wg = dir.path().to_path_buf();
        let cfg = cfg_with_bots(&[("mira", Some("mira"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "mira", &uuid).unwrap();
        confirm_human(&wg, "luca-1", "human-luca", "mira");
        let plan = plan_conversation(&wg, &cfg, "telegram:mira", "555", "luca-1", Entry::Direct);
        let sink = RecSink::default();
        let composer =
            FakeComposer::ok("I'll pull that together.\nTASK_CREATE: Summarize Luca's training");

        let outcome = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Summarize the July 20-26 training.",
            "req-c075-malformed",
            fast_timing(),
            Some(&composer),
            &sink,
            Some("WG_HISTORICAL_WORKOUT_CONTEXT_V1\nweek_key=2026-W30"),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: false });
        assert_eq!(sink.calls().len(), 1);
        assert_eq!(sink.calls()[0].2, grounding::HISTORICAL_WORKOUT_REFUSAL);

        let repeated_composer = FakeComposer::ok("Still cannot verify it.");
        let repeated = run_conversation_turn_with_week_context(
            &wg,
            &plan,
            "Summarize the July 20-26 training.",
            "req-c075-malformed-repeat",
            fast_timing(),
            Some(&repeated_composer),
            &sink,
            Some("WG_HISTORICAL_WORKOUT_CONTEXT_V1\nweek_key=2026-W30"),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(repeated, TurnOutcome::Replied { acked: false });
        assert_eq!(
            sink.calls().last().unwrap().2,
            grounding::HISTORICAL_WORKOUT_REFUSAL,
        );
        assert_no_conversation_action_artifacts(&wg);
    }

    /// Typed workout evidence remains authoritative when model execution
    /// fails. The valid lane answers from rows; malformed evidence refuses;
    /// neither degrades to the generic glitch or reaches parity.
    #[tokio::test]
    async fn historical_workout_terminal_survives_composer_error() {
        for (label, context, expected) in [
            ("grounded", C075_WORKOUT_CONTEXT, C075_WORKOUT_REPLY),
            (
                "invalid",
                "WG_HISTORICAL_WORKOUT_CONTEXT_V1\nweek_key=2026-W30",
                grounding::HISTORICAL_WORKOUT_REFUSAL,
            ),
        ] {
            let dir = tempdir().unwrap();
            let wg = dir.path().to_path_buf();
            let cfg = cfg_with_bots(&[("mira", Some("mira"))]);
            let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(&wg, "mira", &uuid).unwrap();
            confirm_human(&wg, "luca-1", "human-luca", "mira");
            let plan =
                plan_conversation(&wg, &cfg, "telegram:mira", "555", "luca-1", Entry::Direct);
            let sink = RecSink::default();
            let composer = FakeComposer::fail("model unavailable");

            let outcome = run_conversation_turn_with_week_context(
                &wg,
                &plan,
                "Summarize the July 20-26 training.",
                &format!("req-c075-error-{label}"),
                fast_timing(),
                Some(&composer),
                &sink,
                Some(context),
                chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
            )
            .await
            .unwrap();

            assert_eq!(outcome, TurnOutcome::Replied { acked: false }, "{label}");
            assert_eq!(sink.calls().len(), 1, "{label}");
            assert_eq!(sink.calls()[0].2, expected, "{label}");
            assert_no_conversation_action_artifacts(&wg);
        }
    }

    /// The timeout branch has the same terminal semantics as an immediate
    /// composer error, while retaining the ordinary ack/edit lifecycle.
    #[tokio::test]
    async fn historical_workout_terminal_survives_composer_timeout() {
        for (label, context, expected) in [
            ("grounded", C075_WORKOUT_CONTEXT, C075_WORKOUT_REPLY),
            (
                "invalid",
                "WG_HISTORICAL_WORKOUT_CONTEXT_V1\nweek_key=2026-W30",
                grounding::HISTORICAL_WORKOUT_REFUSAL,
            ),
        ] {
            let dir = tempdir().unwrap();
            let wg = dir.path().to_path_buf();
            let cfg = cfg_with_bots(&[("mira", Some("mira"))]);
            let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
            bind_agent(&wg, "mira", &uuid).unwrap();
            confirm_human(&wg, "luca-1", "human-luca", "mira");
            let plan =
                plan_conversation(&wg, &cfg, "telegram:mira", "555", "luca-1", Entry::Direct);
            let sink = RecSink::default();
            let composer = FakeComposer::ok_after(
                "late model prose that must never win",
                Duration::from_millis(120),
            );
            let timing = AckTiming {
                ack_after: Duration::from_millis(10),
                reply_timeout: Duration::from_millis(40),
                poll: Duration::from_millis(5),
            };

            let outcome = run_conversation_turn_with_week_context(
                &wg,
                &plan,
                "Summarize the July 20-26 training.",
                &format!("req-c075-timeout-{label}"),
                timing,
                Some(&composer),
                &sink,
                Some(context),
                chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
            )
            .await
            .unwrap();

            assert_eq!(outcome, TurnOutcome::Replied { acked: true }, "{label}");
            assert_eq!(sink.calls().len(), 1, "{label}: ack missing");
            assert!(sink.calls()[0].2.contains("On it"), "{label}");
            assert_eq!(sink.edits().len(), 1, "{label}: final did not edit ack");
            assert_eq!(sink.edits()[0].3, expected, "{label}");
            assert_no_conversation_action_artifacts(&wg);
        }
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
        assert!(
            edits[0].3.contains("glitched"),
            "ack edited into glitch: {:?}",
            edits[0].3
        );
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
        assert!(speaker_is_owner(
            &origin("bruno_casapinello_bot", None),
            "bruno"
        ));
        assert!(speaker_is_owner(&origin("bruno-bot", None), "bruno"));
        // Owner recognised via the bot_id channel even when persona is a bot id.
        assert!(speaker_is_owner(
            &origin("bruno_casapinello_bot", Some("bruno_casapinello_bot")),
            "bruno"
        ));
        // A different voice is NOT the owner — it still defers.
        assert!(!speaker_is_owner(
            &origin("mira", Some("mira_casapinello_bot")),
            "bruno"
        ));
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

## 1. Dinners (planner → cook)

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
        let grounded = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "Plans for tomorrow?",
            now,
            ForwardedContext::default(),
        );
        assert!(
            grounded.contains("Dentist"),
            "tomorrow's appointment missing from prompt:\n{grounded}"
        );
        assert!(grounded.contains("Thursday"));
        // The answer-first instruction header is present.
        assert!(
            grounded
                .to_lowercase()
                .contains("answer the question directly")
        );
        // Other days must NOT bleed into a scoped "tomorrow" ask.
        assert!(
            !grounded.contains("Baked salmon"),
            "Tue meal leaked:\n{grounded}"
        );
        assert!(
            !grounded.contains("Chickpea"),
            "Mon meal leaked:\n{grounded}"
        );

        // A whole-week ask still surfaces the full week's meals.
        let week = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "how's the week?",
            now,
            ForwardedContext::default(),
        );
        assert!(week.contains("Baked salmon"));
        assert!(week.contains("Luca PT check-in"));
        assert!(week.to_lowercase().contains("do not stall"));

        // Small talk carries no read-shaped WEEK block (no meal dump)...
        let plain = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "morning!",
            now,
            ForwardedContext::default(),
        );
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
        let greet = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "how's your day?",
            tue_noon,
            ForwardedContext::default(),
        );
        assert!(
            greet.contains("PT check-in"),
            "real event missing from greeting prompt:\n{greet}"
        );
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
            ForwardedContext {
                thread: Some(thread),
                ..Default::default()
            },
        );
        assert!(
            followup.contains("pasta pomodoro"),
            "thread context (the referent) missing from the compose prompt:\n{followup}"
        );
        assert!(
            followup.to_lowercase().contains("follow-up")
                && followup
                    .to_lowercase()
                    .contains("do not ask what they mean"),
            "compose-not-clarify instruction missing from the prompt:\n{followup}"
        );

        // WITHOUT thread context: the same ambiguous ask carries no referent and
        // no follow-up instruction — this is exactly the state that made the engine
        // clarify instead of answer.
        let bare = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "tell me the calories",
            now,
            ForwardedContext::default(),
        );
        assert!(
            !bare.contains("pasta pomodoro"),
            "referent leaked without a thread:\n{bare}"
        );
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
            ForwardedContext {
                thread: Some("   "),
                ..Default::default()
            },
        );
        assert!(
            !blank.to_lowercase().contains("do not ask what they mean"),
            "{blank}"
        );
    }

    /// WEEK CONTEXT (task week-grounding-engine): the gateway forwards the parsed
    /// Dinners table via `WG_WEEK_CONTEXT`; the composer must inject it into the
    /// prompt with the standing NEVER-claim-empty instruction so a "what's for
    /// dinner tomorrow?" turn is answered FROM the table. Without the context the
    /// block is absent (the Telegram-listener path); with it, the table AND the
    /// instruction are in the prompt.
    #[test]
    fn week_context_is_injected_with_never_claim_empty_instruction() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 25)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();

        // The gateway's parsed Dinners table (weekSource.buildWeekContext shape).
        let week = "This week's dinners, parsed from the family plan's Dinners table:\n\
                    - Friday (July 24): Chicken tray bake\n\
                    - Saturday (July 25): Baked white fish with tomato, olives & capers\n\
                    Today is Friday — dinner: Chicken tray bake.\n\
                    Tomorrow is Saturday — dinner: Baked white fish with tomato, olives & capers.";

        // WITH week context: the table AND the NEVER-claim-empty instruction land
        // in the prompt.
        let grounded = build_compose_prompt_at(
            &wg,
            &uuid,
            "nora",
            "what's for dinner tomorrow?",
            now,
            ForwardedContext {
                week: Some(week),
                ..Default::default()
            },
        );
        assert!(
            grounded.contains("Baked white fish with tomato, olives & capers"),
            "the Dinners table dish is missing from the compose prompt:\n{grounded}"
        );
        let lower = grounded.to_lowercase();
        assert!(
            lower.contains("this week's dinners") && lower.contains("never say it is empty"),
            "the NEVER-claim-empty week instruction is missing from the prompt:\n{grounded}"
        );

        // WITHOUT week context: no stray week block (the Telegram-listener path).
        let bare = build_compose_prompt_at(
            &wg,
            &uuid,
            "nora",
            "what's for dinner tomorrow?",
            now,
            ForwardedContext::default(),
        );
        assert!(
            !bare.to_lowercase().contains("never say it is empty"),
            "the week instruction leaked without a forwarded table:\n{bare}"
        );
    }

    /// The gateway's real Tier-1 memory block shape
    /// (`claw3d-bridge/src/memoryInject.mjs` `buildMemoryContext`): banner +
    /// preamble + one line per scoped fact, the acting member's own facts tagged
    /// "(you)". Carries a pattern that DISAGREES with the live week on purpose —
    /// remembered "fish on Friday" vs the plan's Friday chicken tray bake.
    fn sample_memory_block() -> &'static str {
        "Family memory — remembered preferences & patterns, NOT the current schedule.\n\
         These are things the family has said or settled over time.\n\
         \n\
         - Nina is allergic to peanuts\n\
         - Friday is usually a fish night (you)\n\
         - Gym is usually Wednesday evening (you)"
    }

    /// FAMILY MEMORY (task p1-engine-memory-reader): the gateway forwards its
    /// scoped Tier-1 block via `WG_MEMORY_CONTEXT`; the engine composer must
    /// INJECT it — and inject it ranked BELOW live state (docs/39 §5.3/§6). Before
    /// this the production Rust path read only `WG_THREAD_CONTEXT` /
    /// `WG_WEEK_CONTEXT`, so the block the gateway built, scoped and budgeted was
    /// silently dropped on every real deploy.
    #[test]
    fn memory_context_is_injected_and_ranked_below_live_state() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();
        // A live plan on disk, so the read-shaped grounding + calendar-truth line
        // are really in the prompt to be ranked against.
        let plans = dir.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("2026-W29-family-plan.md"), W29_FIXTURE_PLAN).unwrap();
        // A family correction, which outranks distilled memory (docs/39 §5.2).
        parity::PreferenceStore::record(
            dir.path(),
            &format!(
                "{}Nadin is not logged so ignore this",
                grounding::CORRECTION_PREFIX
            ),
            "luca",
            "nora",
        )
        .unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let week = "This week's dinners, parsed from the family plan's Dinners table:\n\
                    - Friday (July 24): Chicken tray bake\n\
                    Today is Wednesday — dinner: Chickpea curry.";

        let prompt = build_compose_prompt_at(
            &wg,
            &uuid,
            "nora",
            "what's the plan for the week?",
            now,
            ForwardedContext {
                week: Some(week),
                memory: Some(sample_memory_block()),
                ..Default::default()
            },
        );

        // 1 · the remembered facts are actually THERE (the reader exists at all).
        assert!(
            prompt.contains("Nina is allergic to peanuts")
                && prompt.contains("Friday is usually a fish night (you)"),
            "the forwarded memory block is missing from the compose prompt:\n{prompt}"
        );
        // 2 · it is labelled non-authoritative — live wins, patterns are OFFERED.
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("not the current schedule")
                && lower.contains("live truth and it wins")
                && lower.contains("never present a remembered pattern as this week's plan"),
            "the live-wins labelling is missing from the memory block:\n{prompt}"
        );
        // 3 · ORDER: memory lands AFTER every live source and after the family's
        // corrections, so precedence is legible in reading order (docs/39 §6).
        let mem_pos = prompt.find("FAMILY MEMORY").expect("memory block present");
        let calendar_pos = lower
            .find("calendar")
            .expect("the always-on calendar-truth line is present");
        // "THIS WEEK'S MEALS" since task meal-read-lane — the forwarded block carries
        // every slot the plan knows (dinners, lunches, no-cook nights), not the Dinners
        // table alone.
        let week_pos = prompt
            .find("THIS WEEK'S MEALS")
            .expect("forwarded week block present");
        let plan_pos = prompt
            .find("Chickpea")
            .expect("read-shaped week grounding present");
        let corr_pos = prompt
            .find("Nadin is not logged")
            .expect("corrections block present");
        let msg_pos = prompt
            .find("Message: ")
            .expect("the human message tail is present");
        for (label, pos) in [
            ("the calendar-truth line", calendar_pos),
            ("the read-shaped week grounding", plan_pos),
            ("the forwarded Dinners table", week_pos),
            ("the family's corrections", corr_pos),
        ] {
            assert!(
                mem_pos > pos,
                "memory was injected BEFORE {label} — live state must be read first:\n{prompt}"
            );
        }
        assert!(
            mem_pos < msg_pos,
            "memory landed after the human message — it would not ground the reply:\n{prompt}"
        );
    }

    /// Nothing remembered (or the Telegram-listener path, where the gateway never
    /// set the env var) leaves the compose prompt BYTE-FOR-BYTE unchanged: an
    /// absent, empty or whitespace-only block adds nothing at all.
    #[test]
    fn memory_context_absent_leaves_the_prompt_byte_identical() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        let none = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "what's for dinner?",
            now,
            ForwardedContext::default(),
        );
        let blank = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "what's for dinner?",
            now,
            ForwardedContext {
                memory: Some("  \n \n"),
                ..Default::default()
            },
        );
        assert_eq!(
            none, blank,
            "a blank forwarded memory block changed the prompt"
        );
        assert!(
            !none.contains("FAMILY MEMORY"),
            "a memory block appeared with nothing forwarded:\n{none}"
        );

        // And with a real block it DOES change — otherwise the equality above
        // would pass trivially if the injection were deleted.
        let with = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "what's for dinner?",
            now,
            ForwardedContext {
                memory: Some(sample_memory_block()),
                ..Default::default()
            },
        );
        assert_ne!(with, none, "the memory block is not being injected at all");
        assert!(with.contains("FAMILY MEMORY"), "{with}");
    }

    /// BUDGET (docs/39 §6, no-silent-caps): a runaway forwarded block — a
    /// distiller bug upstream — cannot balloon the engine's compose prompt. The
    /// engine is a separate process reading an env var it does not own, so it
    /// re-guards: the block is trimmed at line boundaries and SAYS it is partial.
    #[test]
    fn memory_context_is_budget_guarded_in_the_compose_prompt() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "otto", &uuid).unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        let mut huge = String::from("- Nina is allergic to peanuts\n");
        for i in 0..800 {
            huge.push_str(&format!("- remembered filler fact number {i}\n"));
        }
        let prompt = build_compose_prompt_at(
            &wg,
            &uuid,
            "otto",
            "what's for dinner?",
            now,
            ForwardedContext {
                memory: Some(&huge),
                ..Default::default()
            },
        );
        assert!(
            prompt.contains("Nina is allergic to peanuts"),
            "the highest-priority remembered line was dropped by the budget:\n{prompt}"
        );
        assert!(
            prompt.contains("left out to keep this small"),
            "the budget trim was SILENT — no-silent-caps posture broken:\n{prompt}"
        );
        assert!(
            !prompt.contains("filler fact number 799"),
            "the runaway block was injected whole — no budget guard:\n{prompt}"
        );
    }

    /// RECENCY DISCIPLINE (task owner-pin-engine, spec item 3): when the thread
    /// window carries a STALE unanswered ask ("how many calories in the pasta?")
    /// alongside a FRESH referent (a duck-breast exchange seconds ago), the
    /// compose prompt must instruct the model to lead with the MOST RECENT topic
    /// and only close the older loop afterward, explicitly labelled. The 17:5x
    /// repro ("I asked duck but I got pasta calories?") buried the live duck
    /// under the stale pasta ask.
    #[test]
    fn thread_context_enforces_recency_leads_with_fresh_referent() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "nora", &uuid).unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 23)
            .unwrap()
            .and_hms_opt(17, 55, 0)
            .unwrap();

        // The window: a STALE pasta-calories ask from hours ago, then a FRESH
        // duck-breast exchange seconds before the new "tell me the calories".
        let thread = "Human: how many calories in the pasta pomodoro?\n\
                      You: Tonight is duck breast with roast potatoes.\n\
                      Human: sounds great";

        let followup = build_compose_prompt_at(
            &wg,
            &uuid,
            "nora",
            "tell me the calories",
            now,
            ForwardedContext {
                thread: Some(thread),
                ..Default::default()
            },
        );

        // The recency instruction is present: lead with the most recent, close
        // older loops only afterward and explicitly labelled.
        let lower = followup.to_lowercase();
        assert!(
            lower.contains("most recent") && lower.contains("answer that first"),
            "recency (lead-with-fresh) instruction missing from the prompt:\n{followup}"
        );
        assert!(
            lower.contains("close the loop on the earlier"),
            "explicit older-loop labelling instruction missing:\n{followup}"
        );
        // The instruction must land AFTER the raw thread block (the model reads
        // the discipline after seeing the turns it applies to).
        let thread_pos = followup.find("duck breast").expect("thread block present");
        let recency_pos = followup
            .find("the LAST message in that list")
            .expect("recency instr present");
        assert!(
            recency_pos > thread_pos,
            "recency instruction placed before the thread block:\n{followup}"
        );
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
            prefs
                .iter()
                .any(|p| p.text.starts_with(grounding::CORRECTION_PREFIX)
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
            &wg,
            &plan,
            "Plans for tomorrow?",
            "req-1",
            fast_timing(),
            Some(&c1),
            &sink1,
        )
        .await
        .unwrap();
        assert!(
            sink1
                .calls()
                .last()
                .unwrap()
                .2
                .contains("waiting on confirmations")
        );

        // Turn 2: the SAME stall is drafted again → guard answers honestly.
        let sink2 = RecSink::default();
        let c2 = FakeComposer::ok(stall);
        run_conversation_turn(
            &wg,
            &plan,
            "walk me through it",
            "req-2",
            fast_timing(),
            Some(&c2),
            &sink2,
        )
        .await
        .unwrap();
        let last = sink2.calls().last().unwrap().2.clone();
        assert!(
            !last.contains("waiting on confirmations"),
            "stall repeated: {last}"
        );
        assert!(
            last.to_lowercase().contains("read"),
            "not the honest fallback: {last}"
        );
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
            &wg,
            &plan,
            "how's your day?",
            "req-fab",
            fast_timing(),
            Some(&c),
            &sink,
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

    /// ENGINE DELIVERY-SEAM REGRESSION: a composed reply is cleaned on the
    /// actual `run_conversation_turn → finalize → outbox + ReplySink` path.
    /// The household uses fixture-only names to prove every name decision comes
    /// from `household.toml`, not a roster compiled into the engine.
    #[tokio::test]
    async fn family_voice_guard_cleans_the_real_delivery_path() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[household]
members = ["Household Member"]

[[agent]]
id = "hearth"
name = "The Hearth"
domains = ["coordination"]

[[agent]]
id = "wayfinder"
name = "The Wayfinder"
domains = ["calendar"]
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[("hearth", Some("hearth"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "hearth", &uuid).unwrap();
        add_binding_for_bot(
            &wg,
            "member-1",
            "Household Member",
            true,
            "coordination-lantern",
        );
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:hearth",
            "555",
            "member-1",
            Entry::Direct,
        );

        let raw = "**The Hearth** 💬 **Dinner is ready.** Check with **Zephyra** before serving. \
                   That lives over in the pipeline. **Service:** dispatcher healthy — 2 agents. \
                   🧭 The Wayfinder's got this one.";
        let sink = RecSink::default();
        let composer = FakeComposer::ok(raw);
        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "req-family-voice",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, TurnOutcome::Replied { .. }));
        let delivered = sink.calls().last().unwrap().2.clone();
        assert_eq!(delivered, "Dinner is ready. Check before serving.");

        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|m| m.content.as_str()),
            Some(delivered.as_str()),
            "the guarded copy is persisted before the scoped reply sink can mirror it"
        );
    }

    #[tokio::test]
    async fn family_voice_guard_replaces_unauthorized_handoff_only_composed_reply() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[[agent]]
id = "hearth"
name = "The Hearth"
domains = ["coordination"]

[[agent]]
id = "wayfinder"
name = "The Wayfinder"
domains = ["calendar"]
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[("hearth", Some("hearth"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "hearth", &uuid).unwrap();
        add_binding(&wg, "member-1", "Household Member", true);
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:hearth",
            "555",
            "member-1",
            Entry::Direct,
        );

        let sink = RecSink::default();
        let composer = FakeComposer::ok("The Wayfinder's got this one.");
        run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "req-handoff-only",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(
            sink.calls().last().map(|call| call.2.as_str()),
            Some(grounding::family_voice_fallback_line().as_str()),
            "a composer-authored handoff-only reply must become neutral copy"
        );
        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(grounding::family_voice_fallback_line().as_str())
        );
    }

    /// The no-composer compatibility path reads a reply that another session
    /// already wrote to the outbox. It must enter the same delivery choke point,
    /// and its persisted summary must be rewritten to the exact guarded send.
    #[tokio::test]
    async fn family_voice_guard_cleans_legacy_session_reply_and_outbox() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[household]
members = ["Quillon Vale"]

[[agent]]
id = "hearth"
name = "The Hearth"

[[agent]]
id = "wayfinder"
name = "The Wayfinder"
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[("hearth", Some("hearth"))]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "hearth", &uuid).unwrap();
        add_binding(&wg, "member-1", "Household Member", true);
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:hearth",
            "555",
            "member-1",
            Entry::Direct,
        );

        let raw = "**The Hearth** 💬 **Dinner is ready.** We're waiting on you and \
                   **Quillon Vale** to confirm. 🧭 The Wayfinder's got this one.";
        let responder_wg = wg.clone();
        let responder_session = uuid.clone();
        let responder = tokio::spawn(async move {
            for _ in 0..100 {
                let inbox =
                    chat::read_inbox_ref(&responder_wg, &responder_session).unwrap_or_default();
                if let Some(message) = inbox.iter().find(|message| message.role == "user") {
                    chat::append_outbox_ref(
                        &responder_wg,
                        &responder_session,
                        raw,
                        &message.request_id,
                    )
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("legacy fixture never received the inbox turn");
        });

        let sink = RecSink::default();
        let outcome = run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "req-legacy-family-voice",
            fast_timing(),
            None,
            &sink,
        )
        .await
        .unwrap();
        responder.await.unwrap();

        assert_eq!(outcome, TurnOutcome::Replied { acked: false });
        let calls = sink.calls();
        assert_eq!(calls.len(), 1, "the legacy bridge emits one guarded send");
        assert_eq!(calls[0].2, "Dinner is ready.");
        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(calls[0].2.as_str()),
            "the legacy outbox summary must match the guarded send byte-for-byte"
        );
    }

    /// A legacy session row is not canonical until the engine guard has run.
    /// If rewriting the guarded bytes to the outbox fails and transport then
    /// fails too, a same-key retry must guard the still-dirty row again rather
    /// than relaying it through the persisted-reply fast path.
    #[tokio::test]
    async fn legacy_retry_guards_dirty_outbox_when_rewrite_and_transport_fail() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailFirstRecordingSink {
            attempts: AtomicUsize,
            texts: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl ReplySink for FailFirstRecordingSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                self.texts.lock().unwrap().push(text.to_string());
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub guarded transport failure");
                }
                Ok(Some("guarded-legacy-message".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[household]
members = ["Fixture Member"]

[[agent]]
id = "fixture-voice"
name = "Fixture Voice"
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[("fixture-voice", Some("fixture-voice"))]);
        let session_ref = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "fixture-voice", &session_ref).unwrap();
        add_binding(&wg, "fixture-member", "Household Member", true);
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:fixture-voice",
            "555",
            "fixture-member",
            Entry::Direct,
        );

        // `edit_outbox_message_ref` writes this sibling path before renaming it.
        // A directory at that exact path deterministically forces the rewrite
        // to fail while leaving the original outbox readable for retry.
        let rewrite_blocker = chat::outbox_path_ref(&wg, &session_ref).with_extension("jsonl.tmp");
        std::fs::create_dir_all(&rewrite_blocker).unwrap();

        let raw = "**Fixture Voice** says dinner is ready for **Fixture Member**.";
        let family_roster = grounding::load_family_voice_roster(&project_root_of(&wg), &wg);
        let expected = grounding::enforce_family_voice(raw, &family_roster);
        assert_ne!(
            expected, raw,
            "the fixture must contain bytes that the family guard changes",
        );
        let responder_wg = wg.clone();
        let responder_session = session_ref.clone();
        let responder = tokio::spawn(async move {
            for _ in 0..100 {
                let inbox =
                    chat::read_inbox_ref(&responder_wg, &responder_session).unwrap_or_default();
                if let Some(message) = inbox
                    .iter()
                    .find(|message| message.request_id == "legacy-dirty-retry")
                {
                    chat::append_outbox_ref(
                        &responder_wg,
                        &responder_session,
                        raw,
                        &message.request_id,
                    )
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("legacy fixture never received the inbox turn");
        });

        let sink = FailFirstRecordingSink::default();
        let timing = AckTiming {
            ack_after: Duration::from_secs(1),
            reply_timeout: Duration::from_millis(500),
            poll: Duration::from_millis(5),
        };
        let first = run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "legacy-dirty-retry",
            timing,
            None,
            &sink,
        )
        .await;
        responder.await.unwrap();
        assert!(first.is_err(), "the first guarded transport must fail");

        run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "legacy-dirty-retry",
            timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            &wg,
            &plan,
            "quick update",
            "legacy-dirty-retry",
            timing,
            None,
            &sink,
        )
        .await
        .unwrap();

        let texts = sink.texts.lock().unwrap().clone();
        assert_eq!(texts.len(), 2);
        assert_eq!(
            texts,
            vec![expected.clone(), expected],
            "both attempts must use guarded bytes even while the persisted row stays dirty",
        );
        let outbox = chat::read_outbox_since_ref(&wg, &session_ref, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(raw),
            "the blocker must prove the retry read an unguarded persisted row",
        );
    }

    /// The single-owner path appends one trusted authored-name handoff after
    /// composition. That exact engine-authored suffix survives; a composer
    /// cannot grant itself the same exception. A transport retry must relay the
    /// already-guarded outbox bytes verbatim rather than stripping that suffix
    /// in a second context-free guard pass.
    #[tokio::test]
    async fn family_voice_guard_preserves_owner_handoff_bytes_on_transport_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct FailFirstHandoffSink {
            attempts: AtomicUsize,
            texts: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl ReplySink for FailFirstHandoffSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                self.texts.lock().unwrap().push(text.to_string());
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub handoff transport failure");
                }
                Ok(Some("handoff-retry-message".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[[agent]]
id = "coordination-lantern"
name = "Evening Lantern"
domains = ["coordination"]

[[agent]]
id = "meal-cairn"
name = "Cedar Table"
domains = ["meals"]
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[
            ("coordination-lantern", Some("coordination-lantern")),
            ("meal-cairn", Some("meal-cairn")),
        ]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "coordination-lantern", &uuid).unwrap();
        add_binding(&wg, "member-1", "Household Member", true);
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:coordination-lantern",
            "555",
            "member-1",
            Entry::Direct,
        );

        let sink = FailFirstHandoffSink::default();
        let composer =
            FakeComposer::ok("Thursday soup is noted.\nTASK_CREATE: move Thursday dinner to soup");
        let first = run_conversation_turn(
            &wg,
            &plan,
            "swap Thursday dinner to soup",
            "req-owner-handoff",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await;
        assert!(first.is_err(), "the first transport attempt must fail");
        run_conversation_turn(
            &wg,
            &plan,
            "swap Thursday dinner to soup",
            "req-owner-handoff",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();
        run_conversation_turn(
            &wg,
            &plan,
            "swap Thursday dinner to soup",
            "req-owner-handoff",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.attempts.load(Ordering::SeqCst), 2);
        let attempted = sink.texts.lock().unwrap().clone();
        assert_eq!(attempted.len(), 2);
        assert_eq!(
            attempted[0], attempted[1],
            "the persisted retry must preserve the exact authorized bytes",
        );
        let delivered = attempted[1].clone();
        let owner_map = ownership::OwnerMap::load(dir.path());
        let trusted =
            ownership::defer_line(&owner_map, "meal-cairn", ownership::Domain::MealPlanning);
        assert_eq!(
            delivered,
            format!("Thursday soup is noted.\n\n{trusted}"),
            "the exact ownership notice must survive after the guarded body"
        );
        assert!(
            delivered.contains("Cedar Table"),
            "the family sees the authored multiword display name: {delivered}",
        );
        assert!(
            !delivered.contains("meal-cairn"),
            "the opaque routing id must not become family-visible copy: {delivered}",
        );
        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(delivered.as_str()),
            "the ownership exception must produce identical outbox/send bytes"
        );
    }

    #[tokio::test]
    async fn owner_handoff_without_a_safe_display_name_is_name_free() {
        let dir = tempdir().unwrap();
        let wg = dir.path().join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            dir.path().join("household.toml"),
            r#"
[[agent]]
id = "coordination-lantern"
name = "Evening Lantern"
domains = ["coordination"]

[[agent]]
id = "meal-cairn"
domains = ["meals"]
"#,
        )
        .unwrap();

        let cfg = cfg_with_bots(&[
            ("coordination-lantern", Some("coordination-lantern")),
            ("meal-cairn", Some("meal-cairn")),
        ]);
        let uuid = create_session(&wg, SessionKind::Interactive, &[], None).unwrap();
        bind_agent(&wg, "coordination-lantern", &uuid).unwrap();
        add_binding_for_bot(
            &wg,
            "member-2",
            "Household Member",
            true,
            "coordination-lantern",
        );
        let plan = plan_conversation(
            &wg,
            &cfg,
            "telegram:coordination-lantern",
            "556",
            "member-2",
            Entry::Direct,
        );

        let sink = RecSink::default();
        let composer =
            FakeComposer::ok("Saturday stew is noted.\nTASK_CREATE: move Saturday dinner to stew");
        run_conversation_turn(
            &wg,
            &plan,
            "swap Saturday dinner to stew",
            "req-name-free-owner-handoff",
            fast_timing(),
            Some(&composer),
            &sink,
        )
        .await
        .unwrap();

        let delivered = sink.calls().last().unwrap().2.clone();
        assert_eq!(
            delivered, "Saturday stew is noted.\n\nThis one's for the right person 🥗",
            "a missing authored name gets grounded name-free copy",
        );
        assert!(
            !delivered.contains("meal-cairn"),
            "the routing id must stay private even when no display name exists: {delivered}",
        );
        let outbox = chat::read_outbox_since_ref(&wg, &uuid, 0).unwrap();
        assert_eq!(
            outbox.last().map(|message| message.content.as_str()),
            Some(delivered.as_str()),
            "the name-free ownership notice must match persisted and sent bytes",
        );
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

    fn add_binding_for_bot(wg: &Path, sender: &str, name: &str, confirmed: bool, bot_id: &str) {
        let agency_dir = wg.join("agency");
        let mut map = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
        let mut b = crate::agency::TelegramBinding::new(
            sender,
            &format!("agent-{sender}"),
            name,
            Some(bot_id.to_string()),
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
