//! Telegram commands for WG CLI
//!
//! Provides commands for interacting with Telegram:
//! - `wg telegram listen` - Start the Telegram bot listener
//! - `wg telegram send` - Send a message to the configured chat
//! - `wg telegram status` - Show Telegram configuration status

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use worksgood::notify::NotificationChannel;
use worksgood::notify::casa_feed;
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::family_plan;
use worksgood::notify::fast_lane;
use worksgood::notify::telegram::{TelegramBotConfig, TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_family_commands as family_commands;
use worksgood::notify::telegram_voice;
use worksgood::notify::telegram_dedupe::{DedupeKey, DedupeSet};
use worksgood::notify::ownership;
use worksgood::notify::telegram_group::{
    CONCIERGE_BOT, Election, NaturalRoute, elect_group_inbound, elect_responders,
    election_decision_summary, is_discussion_ask, parse_at_mention_tokens, resolve_mentioned_bot,
    route_natural,
};

/// Whether an inbound listener message may fire a FAMILY command and/or the
/// OPERATOR command reference.
///
/// This is the single gate that closed `fix-command-leaks`: a bare `?` in the
/// group was parsed as an operator HELP command and dumped the raw WG
/// claim/done reference into the family chat, racing the mention election. The
/// three rules it encodes:
///
/// 1. A message is a command **only** when it opens with a genuine Telegram
///    slash command (`has_bot_command` — a `bot_command` entity at offset 0).
///    Punctuation, a bare `?`, or ordinary chatter is conversation, never a
///    command — so it can never race the election.
/// 2. Because the gate keys off the slash entity (not the text), an addressed
///    conversational turn like `@nora ?` carries no command entity → the
///    election owns it and the agent converses.
/// 3. The OPERATOR reference (claim/done/status/help) is coordinator content
///    that must NEVER surface in a family group — it runs only in a 1:1
///    operator DM, and only for a real slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandGate {
    /// A family-voice command (`/dinner`, `/help`, …) may run in this chat.
    pub family: bool,
    /// The operator WG command reference may run in this chat.
    pub operator: bool,
}

/// Decide the [`CommandGate`] for an inbound message. Pure and unit-testable
/// against real `decode_update` output.
pub fn command_gate(msg: &worksgood::notify::IncomingMessage) -> CommandGate {
    let is_group = matches!(msg.chat_type.as_deref(), Some("group") | Some("supergroup"));
    let is_command = msg.has_bot_command;
    CommandGate {
        family: is_command,
        operator: is_command && !is_group,
    }
}

/// Loopback gateway endpoint the web-identity `/start login_<nonce>` gate POSTs
/// the verified telegram id to. One-directional, token-free, loopback-only — the
/// gateway enforces the loopback guard (`403` for a non-loopback caller) and
/// resolves the telegram id against the live binding roster itself. Overridable
/// via `CASA_AUTH_CONFIRM_URL` ONLY so the live/scripted test can point the
/// listener at a stub gateway; production always uses the loopback default. See
/// docs/16-web-identity.md §The listener side.
const AUTH_CONFIRM_URL_DEFAULT: &str = "http://127.0.0.1:7788/auth/confirm";

fn auth_confirm_url() -> String {
    std::env::var("CASA_AUTH_CONFIRM_URL").unwrap_or_else(|_| AUTH_CONFIRM_URL_DEFAULT.to_string())
}

/// The request header carrying the listener→gateway SHARED SECRET (task
/// urgent-auth-phantom). The old "loopback-only" guard on the gateway's WRITE path
/// (`/auth/confirm`, `/auth/found`, `/invite/redeem`) is meaningless on the kiosk
/// deployment — every kitchen-tablet browser IS loopback, so an `/auth` page open
/// on an already-signed-in browser could self-confirm each auto-refreshed nonce
/// and mint a PHANTOM device (found live 2026-07-12). The real trust boundary is a
/// secret only the LISTENER can read: the gateway mints it into a mode-600
/// gitignored file at boot (`.casa/auth-confirm.secret`) and the listener attaches
/// it here on every write; the gateway rejects (`403`) any write without it. A
/// browser can never read that file, so a page can never forge the header.
const CONFIRM_SECRET_HEADER: &str = "x-casa-auth-secret";

/// Default location of the gateway-minted confirm secret, relative to the casa
/// project root (the listener's CWD) — mirrors the gateway's
/// `resolve(project.root, ".casa/auth-confirm.secret")`.
const CONFIRM_SECRET_FILE_DEFAULT: &str = ".casa/auth-confirm.secret";

/// Read the listener→gateway confirm secret. Prefers `CASA_AUTH_CONFIRM_SECRET`
/// (a literal, for the live/scripted test stub) then the mode-600 secret file
/// (`CASA_AUTH_CONFIRM_SECRET_FILE` or the `.casa/auth-confirm.secret` default).
/// Read fresh on each write so a gateway that re-mints the secret on restart is
/// picked up without a listener restart. Returns `None` when neither is present —
/// the listener then posts WITHOUT the header, exactly as before, so an OLD
/// gateway that predates the secret gate still works. The value is a bearer
/// secret and is NEVER logged.
fn confirm_secret() -> Option<String> {
    if let Ok(v) = std::env::var("CASA_AUTH_CONFIRM_SECRET") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let path = std::env::var("CASA_AUTH_CONFIRM_SECRET_FILE")
        .unwrap_or_else(|_| CONFIRM_SECRET_FILE_DEFAULT.to_string());
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        Err(_) => None,
    }
}

/// Attach the confirm secret header to a listener→gateway WRITE request when a
/// secret is configured (see [`confirm_secret`]). A no-op when none is present, so
/// the listener stays compatible with a gateway that predates the secret gate.
fn with_confirm_secret(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match confirm_secret() {
        Some(secret) => req.header(CONFIRM_SECRET_HEADER, secret),
        None => req,
    }
}

/// Loopback gateway endpoints for the onboarding-bootstrap flows (task
/// onboarding-bootstrap-first). `/auth/found` founds the household from the very
/// first scan of an EMPTY roster (after the owner answers YES); `/invite/redeem`
/// completes a `join_<nonce>` invite. Both are loopback-only and derived from the
/// same base as `/auth/confirm` so a single override points every path at a stub
/// gateway in the live/scripted test. See docs/16-web-identity.md §The listener side.
fn auth_found_url() -> String {
    auth_confirm_url().replace("/auth/confirm", "/auth/found")
}
fn invite_redeem_url() -> String {
    auth_confirm_url().replace("/auth/confirm", "/invite/redeem")
}

/// The gateway's reply to `POST /auth/confirm`. `ok:true` → the telegram id
/// resolved to a household human and the browser session is now bound;
/// `ok:false` with `reason:"unknown-user"` → the id is not in the roster;
/// `reason:"empty-roster"` → a FRESH deployment, so offer to found the household.
#[derive(Debug, serde::Deserialize)]
struct ConfirmResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// The gateway's reply to `POST /invite/redeem` (and `/auth/found`). `ok:true`
/// carries the joined/founded person's display name so Otto can welcome them by
/// name; `ok:false` carries a `reason` (`unknown-invite`/`expired`/`used`).
#[derive(Debug, serde::Deserialize)]
struct RedeemResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Outcome of a `/start login_<nonce>` confirm against the gateway. The founding
/// window (item 1) needs to distinguish an EMPTY roster (offer ownership) from a
/// genuinely unknown user (ask Otto to add you), so the handler branches on this
/// rather than only receiving a pre-baked reply string.
#[derive(Debug, PartialEq)]
enum WebLoginOutcome {
    /// The telegram id resolved to a household human; the browser is signed in.
    SignedIn,
    /// The roster is EMPTY (fresh deployment) — the handler runs the "are you
    /// the owner?" founding handshake instead of rejecting.
    EmptyRoster,
    /// The id is not in a (non-empty) roster — ask Otto to add you.
    UnknownUser,
    /// The sign-in link EXPIRED / was unknown (a slow new-device round-trip blew past the pending window). Recovery: reopen the page + tap the fresh link.
    LinkExpired,
    /// Bad/expired nonce or the gateway was unreachable — tell them to retry.
    NoSession,
}

impl WebLoginOutcome {
    /// The family-voice reply for the outcomes that DON'T need the founding
    /// handshake (EmptyRoster is handled specially by the caller).
    fn reply(&self) -> String {
        match self {
            WebLoginOutcome::SignedIn => "You're signed in on the kitchen tablet ✋".to_string(),
            WebLoginOutcome::UnknownUser => {
                "I don't recognise you yet — ask Otto to add you to the household.".to_string()
            }
            WebLoginOutcome::EmptyRoster | WebLoginOutcome::LinkExpired | WebLoginOutcome::NoSession => {
                "That sign-in link expired — reopen the Casa page and tap the fresh link.".to_string()
            }
        }
    }
}

/// A pending founding handshake: the empty-roster scanner has been asked "are
/// you the owner?" and we are holding the login nonce + their profile name until
/// they answer YES/NO. Keyed by the sender's numeric telegram id in the listener
/// loop; expires with the login nonce so a stale YES never founds a home.
#[derive(Debug, Clone)]
struct PendingFounding {
    nonce: String,
    name: String,
    created: i64,
}

/// The founding handshake expires with the login nonce (15 minutes) — a YES that
/// arrives after the window is ignored and the scanner just taps the tablet again.
const FOUNDING_TTL_SECS: i64 = 15 * 60;

/// Is an inbound body an explicit decline (`no`/`n`)? The mirror of
/// [`worksgood::agency::human_binding::is_affirmative`] for the founding
/// handshake's NO path. Anything that is neither yes nor no re-prompts.
fn is_negative(body: &str) -> bool {
    let n = body.trim().to_ascii_lowercase();
    n == "no" || n == "n"
}

/// Derive an editable display name for a founding member from their Telegram
/// display label (`@username` when present, else the numeric id). Strips a
/// leading `@`; falls back to "Owner" when only a numeric id is available, so the
/// household's first member never gets a bare number for a name (they can rename
/// later — see docs/16). The listener never has the profile first-name, only the
/// display label the transport surfaced.
fn founding_display_name(sender: &str) -> String {
    let s = sender.trim().trim_start_matches('@').trim();
    if s.is_empty() || s.chars().all(|c| c.is_ascii_digit()) {
        "Owner".to_string()
    } else {
        s.to_string()
    }
}

/// Extract the login nonce from a `/start` deep-link message body, or `None`
/// when this is not a `login_` deep link. Accepts `/start login_<nonce>` and
/// the `@bot`-qualified `/start@otto_bot login_<nonce>` form. The command token
/// must terminate cleanly (whitespace, `@bot`, or end-of-string) so `/started …`
/// is never mistaken for `/start`. The returned slice is the raw nonce — callers
/// MUST NOT log it (see the redaction note in the handler).
fn parse_login_nonce(body: &str) -> Option<&str> {
    let rest = body.trim_start().strip_prefix("/start")?;
    let payload = match rest.chars().next() {
        // "/start" with no payload.
        None => return None,
        // "/start login_…" — payload follows the whitespace.
        Some(c) if c.is_whitespace() => rest.trim_start(),
        // "/start@botname login_…" — drop the "@botname" token first.
        Some('@') => match rest[1..].split_once(char::is_whitespace) {
            Some((_bot, tail)) => tail.trim_start(),
            None => return None,
        },
        // "/started…" — not the start command.
        Some(_) => return None,
    };
    // The nonce is the first whitespace-delimited token after `login_`; drop any
    // trailing text so a stray trailing space never corrupts the secret.
    let nonce = payload
        .strip_prefix("login_")?
        .split_whitespace()
        .next()
        .unwrap_or("");
    if nonce.is_empty() {
        None
    } else {
        Some(nonce)
    }
}

/// Handle a 1:1 `/start login_<nonce>` web-identity deep link: POST the sender's
/// REAL telegram id to the loopback gateway `/auth/confirm` and return the
/// family-voice reply to send back. One-directional and token-free; the gateway
/// resolves the id against the household roster and binds the browser session.
///
/// NEVER logs the nonce or the confirm body — the confirm is token-free but the
/// nonce is still a single-use secret (see docs/16-web-identity.md).
async fn confirm_web_login_outcome(
    client: &reqwest::Client,
    nonce: &str,
    telegram_id: &str,
) -> WebLoginOutcome {
    let body = serde_json::json!({ "nonce": nonce, "telegram_id": telegram_id });
    let resp = with_confirm_secret(client.post(auth_confirm_url()).json(&body))
        .send()
        .await;
    let confirm = match resp {
        Ok(r) => r.json::<ConfirmResp>().await.ok(),
        Err(_) => None,
    };
    match confirm {
        Some(c) if c.ok => WebLoginOutcome::SignedIn,
        Some(c) if c.reason.as_deref() == Some("empty-roster") => WebLoginOutcome::EmptyRoster,
        Some(c) if c.reason.as_deref() == Some("unknown-user") => WebLoginOutcome::UnknownUser,
        Some(c) if matches!(c.reason.as_deref(), Some("unknown-nonce") | Some("expired") | Some("used")) => WebLoginOutcome::LinkExpired,
        _ => WebLoginOutcome::NoSession,
    }
}

/// Backward-compatible thin wrapper returning the family-voice reply string for
/// the non-founding outcomes (used by the existing unit tests + the plain
/// signed-in / unknown-user / no-session paths).
async fn confirm_web_login(
    client: &reqwest::Client,
    nonce: &str,
    telegram_id: &str,
) -> String {
    confirm_web_login_outcome(client, nonce, telegram_id)
        .await
        .reply()
}

/// Extract the invite nonce from a `/start join_<nonce>` deep-link body, or
/// `None` when this is not a `join_` deep link. The mirror of
/// [`parse_login_nonce`] for the INVITE family (onboarding-bootstrap item 2):
/// the operator's Manage-household card mints `t.me/<bot>?start=join_<nonce>`,
/// the invitee taps it, and Telegram delivers `/start join_<nonce>`. Same
/// `@bot`-qualified handling and clean-termination rules as the login parser.
fn parse_join_nonce(body: &str) -> Option<&str> {
    let rest = body.trim_start().strip_prefix("/start")?;
    let payload = match rest.chars().next() {
        None => return None,
        Some(c) if c.is_whitespace() => rest.trim_start(),
        Some('@') => match rest[1..].split_once(char::is_whitespace) {
            Some((_bot, tail)) => tail.trim_start(),
            None => return None,
        },
        Some(_) => return None,
    };
    let nonce = payload
        .strip_prefix("join_")?
        .split_whitespace()
        .next()
        .unwrap_or("");
    if nonce.is_empty() { None } else { Some(nonce) }
}

/// Redeem a `/start join_<nonce>` invite (onboarding-bootstrap item 2): POST the
/// tapper's REAL telegram id + the nonce to the loopback gateway `/invite/redeem`,
/// which resolves the nonce to the pending person's name, creates their binding
/// under that name + this id, and confirms it (the tap IS the handshake). Returns
/// the family-voice welcome (or a friendly error). NEVER logs the nonce.
async fn redeem_invite(client: &reqwest::Client, nonce: &str, telegram_id: &str) -> String {
    let body = serde_json::json!({ "nonce": nonce, "telegram_id": telegram_id });
    let resp = with_confirm_secret(client.post(invite_redeem_url()).json(&body))
        .send()
        .await;
    let redeemed = match resp {
        Ok(r) => r.json::<RedeemResp>().await.ok(),
        Err(_) => None,
    };
    match redeemed {
        Some(r) if r.ok => {
            let who = r.name.as_deref().filter(|s| !s.is_empty()).unwrap_or("friend");
            format!("Welcome to the household, {who}! You're all set — sign in on any device. \u{1f3e0}")
        }
        Some(r) if r.reason.as_deref() == Some("used") => {
            "That invite was already used — ask for a fresh one from Manage household.".to_string()
        }
        Some(r) if r.reason.as_deref() == Some("expired") => {
            "That invite has expired — ask for a fresh one from Manage household.".to_string()
        }
        _ => "I couldn't find that invite — ask whoever invited you for a new link.".to_string(),
    }
}

/// Found the household from an empty-roster scan (onboarding-bootstrap item 1)
/// once the owner has answered YES: POST the login nonce + their telegram id +
/// profile name to the loopback gateway `/auth/found`, which creates them as the
/// first member (operator) and binds the browser session. Returns the family-voice
/// welcome (or a friendly error). NEVER logs the nonce.
async fn found_household(
    client: &reqwest::Client,
    nonce: &str,
    telegram_id: &str,
    name: &str,
) -> String {
    let body = serde_json::json!({ "nonce": nonce, "telegram_id": telegram_id, "name": name });
    let resp = with_confirm_secret(client.post(auth_found_url()).json(&body))
        .send()
        .await;
    let founded = match resp {
        Ok(r) => r.json::<RedeemResp>().await.ok(),
        Err(_) => None,
    };
    match founded {
        Some(r) if r.ok => {
            let who = r.name.as_deref().filter(|s| !s.is_empty()).unwrap_or(name);
            format!(
                "This home is yours now, {who} — you're the first member. \u{2705} \
                 You're signed in on the tablet; invite the rest of the family from Manage household."
            )
        }
        _ => "I couldn't finish setting up — tap the tablet for a fresh link and try again.".to_string(),
    }
}

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

    // Periodic conversational report-back tick. The listener is the process that
    // OWNS the Telegram sockets, and its stdout is `.casa/telegram.log`, so
    // report-backs are delivered — and logged where all other family-facing
    // Telegram traffic lands — from HERE, not from the coordinator (round 1 sent
    // them from a detached thread in the wg-service process, logging only to the
    // daemon log; the second live test's pesto "done" reply was invisible there
    // and, worse, swallowed by the pacing cap). Every LIFECYCLE_TICK_SECS this
    // observes the live graph: for any origin-stamped task whose start/done/fail
    // transition is not yet in the FiredLog it delivers the family-voice report
    // exactly once (see `run_lifecycle`). A cheap `pending_fires` gate keeps an
    // idle house silent — no per-tick chatter in the log. It runs on a plain OS
    // thread, NOT a tokio task: `run_lifecycle` builds its own runtime to send,
    // which would panic if nested inside this listener's runtime. Read-only
    // against the graph; it never touches the message-routing pipeline below.
    const LIFECYCLE_TICK_SECS: u64 = 15;
    {
        let lifecycle_dir = dir.to_path_buf();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(LIFECYCLE_TICK_SECS));
            let graph_path = crate::commands::graph_path(&lifecycle_dir);
            let graph = match worksgood::parser::load_graph(&graph_path) {
                Ok(g) => g,
                Err(_) => continue, // no graph yet — nothing to report
            };
            let root = project_root(&lifecycle_dir);
            let fired = worksgood::notify::reminder::FiredLog::load(
                &worksgood::notify::reminder::FiredLog::path(&root),
            );
            let pending = worksgood::notify::lifecycle::pending_fires(
                graph.tasks(),
                |id| fired.contains(id),
            );
            if pending.is_empty() {
                continue; // no unreported transition — stay quiet
            }
            // Something transitioned: deliver every pending report-back (same code
            // path as `wg telegram lifecycle`, real send). Exactly-once + pacing
            // are enforced inside via the persisted FiredLog.
            if let Err(e) = run_lifecycle(&lifecycle_dir, None, false, None, false, false) {
                eprintln!(
                    "[{}] lifecycle report-back tick failed: {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                );
            }
        });
    }

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
        // arrives four times. Keyed by a CONTENT FINGERPRINT
        // (chat_id, from.id, date, hash(text)) — NOT (chat_id, message_id),
        // because the wire shows each bot assigns the message its OWN
        // message_id (observed 58/36/45/39 for one Luca message), so a
        // message_id key never collides across bots and never dedupes. The
        // fingerprint IS identical across all four deliveries, so the first
        // copy wins and the other three are dropped silently. See
        // `notify::telegram_dedupe`.
        let dedupe = DedupeSet::new();

        let workgraph_dir = dir.to_path_buf();

        // The wg config drives the conversational reply COMPOSER — the one-shot
        // `claude` spawn that actually produces an agent's reply to a plain
        // message. Loaded once here; each converse turn builds an
        // `OneshotComposer` from a clone. Before this, a converse turn wrote the
        // human message to the bound-session inbox and polled the outbox for a
        // reply that only a live `wg nex` daemon could produce — none runs in the
        // deployment, so every turn acked then TIMED OUT at 120s. Loading it may
        // fail (no config); we log and fall back to the legacy poll path only
        // then. See `notify::telegram_conversation::OneshotComposer`.
        let wg_config = match worksgood::config::Config::load_merged(&workgraph_dir) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!(
                    "[telegram] could not load wg config for the reply composer ({e:#}) — \
                     converse turns will use the legacy session-outbox path"
                );
                None
            }
        };

        // The constellation split view's conversation pane reads this feed (the
        // casa gateway tails it and serves `GET /conversation`). We are the only
        // process holding the Telegram sockets, so we mirror every inbound GROUP
        // message and every agent reply we relay into the group here. The feed
        // lives at `<project-root>/.casa/group-feed.jsonl` and carries ONLY the
        // six display-safe fields — never a token, chat id, or user id. See
        // `notify::casa_feed` and docs/15 §chat-split.
        let feed_path = casa_feed::feed_path_for(&project_root(&workgraph_dir));

        // Fix #1 (startup stale-backlog) + Fix #2 (burst coalescing) state. The
        // start timestamp anchors the staleness test; `backlog_notified` ensures
        // the "skipped older messages" line is posted at most once; the coalescer
        // collapses a rapid burst of collective/concierge elections to one reply.
        use worksgood::notify::telegram_pacing;
        let listener_start = chrono::Utc::now().timestamp();
        let mut backlog_notified = false;
        // Shared so the OFF-LOOP conversation turn (spawned below) can mark its
        // reply *sent* when it completes — that ends the pending turn so a genuine
        // follow-up arriving after the answer is a NEW turn, never coalesced away
        // (BUG 2, the 2026-07-12 swallowed "why they don't reply?"). Locked only
        // for the trivial admit/mark calls, never across an `.await`.
        let coalescer =
            std::sync::Arc::new(std::sync::Mutex::new(telegram_pacing::BurstCoalescer::default()));

        // Dedicated short-timeout client for the loopback web-identity confirm
        // POST — separate from the long-poll clients so a slow/absent gateway
        // never stalls a poll. See `confirm_web_login`.
        let auth_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // Founding handshakes in flight (onboarding-bootstrap item 1), keyed by
        // the scanner's numeric telegram id: the empty-roster `/start login_`
        // scan asked "are you the owner?" and we hold their login nonce + profile
        // name here until they answer YES/NO. Stale entries expire with the login
        // nonce (FOUNDING_TTL_SECS) so a late YES never founds a household.
        let mut pending_founding: std::collections::HashMap<String, PendingFounding> =
            std::collections::HashMap::new();

        // Album coalescing for photo → shopping (task photo-to-shopping): the
        // FIRST frame of a Telegram album (all frames share a `media_group_id`)
        // fires ONE vision turn; every later frame of the same group is dropped
        // here so six fridge photos never fire six turns/replies. A lone photo
        // carries no group id and is never suppressed.
        let mut answered_media_groups: HashSet<String> = HashSet::new();

        while let Some(mut msg) = rx.recv().await {
            // Fix #0 — the bot-loop guard, FIRST (before dedupe, feed mirror,
            // commands, and election). The family bots run as group admins, so
            // each bot's poller RECEIVES the replies the OTHER bots send. A
            // message whose sender is a bot must never be mirrored, commanded,
            // elected, routed, or composed — otherwise one human message fans
            // out into a self-amplifying storm (roster reply → other pollers see
            // it → re-election → 12 replies). One compact line, then drop.
            if msg.sender_is_bot {
                println!(
                    "[{}] ignored bot-sent msg {}",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.message_id.as_deref().unwrap_or("none"),
                );
                continue;
            }

            // De-duplicate first: a text message whose CONTENT FINGERPRINT
            // (chat_id, from.id, date, hash(text)) we have already processed on
            // another bot's queue is a duplicate delivery — drop it before it
            // can trigger a second election. We key on content, not message_id,
            // because each bot stamps the same physical message with a
            // different message_id (see `telegram_dedupe` docs). The fingerprint
            // needs a chat id and a send timestamp; when the sender's numeric
            // `from.id` is absent we fall back to the display sender (still
            // identical across the four deliveries). A message missing chat_id
            // or a timestamp skips dedupe and is always processed — matching the
            // prior "None → process" contract. Button presses (action_id) carry
            // no message text to route and only ever reach the one bot whose
            // message held the button, so they are exempt.
            if msg.action_id.is_none() {
                if let (Some(cid), Some(date)) = (msg.chat_id.as_deref(), msg.sent_at) {
                    let sender = msg.sender_id.as_deref().unwrap_or(msg.sender.as_str());
                    let key = DedupeKey::from_content(cid, sender, date, &msg.body);
                    if !dedupe.first_delivery(key) {
                        println!(
                            "[{}] Duplicate group message (chat {}, from {}, date {}, msg {}) dropped — already handled",
                            chrono::Utc::now().format("%H:%M:%S"),
                            cid,
                            sender,
                            date,
                            msg.message_id.as_deref().unwrap_or("none"),
                        );
                        continue;
                    }
                }
            }

            // Mirror the inbound GROUP message to the conversation pane's feed.
            // Only group/supergroup text messages (not 1:1 DMs, not button
            // presses) — the pane is "our end of the family group chat". This
            // runs post-dedupe so a message the fan-out delivered on four bots'
            // queues is written exactly once. `msg.sender` is a Telegram
            // @username (a display handle, never a numeric user id), and only
            // the six allow-listed fields are written — no token or chat id ever
            // touches the file. See `notify::casa_feed`.
            if msg.action_id.is_none()
                && matches!(msg.chat_type.as_deref(), Some("group") | Some("supergroup"))
                && !msg.body.trim().is_empty()
            {
                let entry = casa_feed::group_entry(&msg.sender, &msg.body, casa_feed::now_ms());
                if let Err(e) = casa_feed::append_entry(&feed_path, &entry) {
                    eprintln!(
                        "[{}] casa feed: failed to mirror inbound group message: {e}",
                        chrono::Utc::now().format("%H:%M:%S"),
                    );
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

            // Fix #1 — startup stale-backlog policy. A text message sent well
            // before the listener came up is queued backlog, not a live turn: we
            // do NOT answer it (the household has moved on), and we post ONE
            // compact concierge line the first time we skip any, so the skip is
            // visible rather than silent. Button presses are exempt (they carry
            // no timestamp and are inherently interactive). Messages without a
            // timestamp are never treated as stale.
            if msg.action_id.is_none() {
                if let Some(sent_at) = msg.sent_at {
                    if telegram_pacing::is_stale_backlog(
                        sent_at,
                        listener_start,
                        telegram_pacing::DEFAULT_STALE_SECS,
                    ) {
                        println!(
                            "[{}] stale backlog: not answering msg {} (sent {}s before start)",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.message_id.as_deref().unwrap_or("none"),
                            listener_start.saturating_sub(sent_at),
                        );
                        if !backlog_notified {
                            backlog_notified = true;
                            if let Err(e) = channel
                                .send_text(&reply_target, &telegram_pacing::backlog_skipped_line())
                                .await
                            {
                                eprintln!("Failed to send backlog-skipped line: {e}");
                            }
                        }
                        continue;
                    }
                }
            }

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

            // VOICE NOTE / AUDIO / VIDEO_NOTE → transcript → normal message
            // (task telegram-voice-notes). A recording carries no text — its
            // words are SPOKEN. Transcribe it with the SAME whisper engine the
            // kiosk mic uses (the gateway's /conversation/transcribe), then
            // INJECT the transcript as the message body so it routes EXACTLY
            // like a typed line: election, single-owner routing, fast lane,
            // composer, ledger and dedupe all run UNCHANGED below. The bot-loop,
            // dedupe and stale-backlog guards above already applied — a recording
            // has an empty body, so the content-fingerprint dedupe still collapses
            // the four bot deliveries to one, and the earlier text-only feed mirror
            // was skipped (empty body). We mirror the SPOKEN line here instead,
            // with an honest 🎙️ marker. Never silent: every failure replies
            // in-persona via the receiving bot.
            if let Some(voice_file_id) = msg.voice_file_id.clone() {
                // The `file_id` is only valid for the bot that RECEIVED the
                // recording, so we download — and reply — via THAT bot's channel.
                let Some(receiving) =
                    channels.iter().find(|c| c.channel_type() == msg.channel)
                else {
                    eprintln!(
                        "[{}] voice note from {}: no channel matches receiving bot {:?} — dropped",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                        msg.channel,
                    );
                    continue;
                };

                // The mime is only a hint (ffmpeg sniffs the container); the raw
                // kind isn't threaded through `IncomingMessage`, so default it to
                // Voice — the post-download size guard runs regardless.
                let meta = telegram_voice::VoiceMeta {
                    file_id: voice_file_id.clone(),
                    mime_type: msg.voice_mime.clone(),
                    kind: telegram_voice::VoiceKind::Voice,
                    file_size: None,
                };
                let gateway = telegram_voice::HttpTranscribeGateway::from_env();
                let lang = telegram_voice::voice_lang();

                println!(
                    "[{}] voice note from {} (file {}) — transcribing via gateway",
                    chrono::Utc::now().format("%H:%M:%S"),
                    msg.sender,
                    voice_file_id,
                );

                match telegram_voice::transcribe_voice_note(
                    receiving,
                    &gateway,
                    &meta,
                    &telegram_voice::VoiceLimits::default(),
                    &lang,
                )
                .await
                {
                    Ok(telegram_voice::TranscribeResult::Transcript(text)) => {
                        println!(
                            "[{}] voice note from {} transcribed ({} chars) — injecting as message",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            text.chars().count(),
                        );
                        // Mirror the SPOKEN line to the group feed with the 🎙️
                        // marker so everyone sees it was spoken. Group/supergroup
                        // only — a 1:1 DM is private and never lands in the feed.
                        if matches!(
                            msg.chat_type.as_deref(),
                            Some("group") | Some("supergroup")
                        ) {
                            let entry = casa_feed::group_entry(
                                &msg.sender,
                                &telegram_voice::spoken_feed_body(&text),
                                casa_feed::now_ms(),
                            );
                            if let Err(e) = casa_feed::append_entry(&feed_path, &entry) {
                                eprintln!(
                                    "[{}] casa feed: failed to mirror spoken message: {e}",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                );
                            }
                        }
                        // Inject the transcript as the body and clear the voice
                        // handle so the rest of the loop treats this as an
                        // ordinary typed message. NO `continue` — fall through to
                        // the UNCHANGED pipeline below (command gate, election,
                        // routing, fast lane, composer), which all read `msg.body`.
                        msg.body = text;
                        msg.voice_file_id = None;
                    }
                    Ok(telegram_voice::TranscribeResult::Failed(failure)) => {
                        println!(
                            "[{}] voice note from {} not transcribed ({:?}) — replying in-persona",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            failure,
                        );
                        if let Err(e) =
                            receiving.send_text(&reply_target, failure.message()).await
                        {
                            eprintln!(
                                "Failed to send voice failure reply: {}",
                                worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                            );
                        }
                        continue;
                    }
                    Err(e) => {
                        // A download / transport error — token already scrubbed by
                        // the downloader, scrubbed again here for defence in depth.
                        eprintln!(
                            "[{}] voice note from {} failed to transcribe: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.sender,
                            worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                        );
                        // Never silent — send the honest "couldn't make it out" line.
                        if let Err(e2) = receiving
                            .send_text(
                                &reply_target,
                                telegram_voice::TranscribeFailure::Unclear.message(),
                            )
                            .await
                        {
                            eprintln!(
                                "Failed to send voice error reply: {}",
                                worksgood::notify::telegram::redact_bot_token(&format!("{e2:#}")),
                            );
                        }
                        continue;
                    }
                }
            }

            // Command gate: a message is a command ONLY when it opens with a
            // genuine Telegram slash command (a `bot_command` entity at offset
            // 0). A bare `?`, punctuation, or ordinary chatter carries no such
            // entity and is conversation — it flows to the election below and is
            // never parsed as a command. See `fix-command-leaks`.
            let gate = command_gate(&msg);

            // Web-identity sign-in: a 1:1 `/start login_<nonce>` deep link. The
            // household member scans the kitchen-tablet QR, which opens the bot
            // with `?start=login_<nonce>`; Telegram delivers `/start login_<nonce>`
            // as a genuine slash command (bot_command entity at offset 0, so
            // `gate.family` is set). We take the sender's REAL numeric telegram id
            // (`sender_id`, never a forwarded/spoofable field) and POST it with the
            // nonce to the loopback gateway, which resolves it against the binding
            // roster and binds the browser session. One-directional, token-free.
            // NEVER log the nonce — it is a single-use secret. Only fires in a
            // private chat; a group `/start` is not a sign-in. See
            // docs/16-web-identity.md §The listener side.
            let is_private = matches!(msg.chat_type.as_deref(), Some("private") | None);
            if gate.operator && is_private {
                if let Some(nonce) = parse_login_nonce(&msg.body) {
                    match msg.sender_id.as_deref() {
                        Some(telegram_id) => {
                            let outcome =
                                confirm_web_login_outcome(&auth_client, nonce, telegram_id).await;
                            if outcome == WebLoginOutcome::EmptyRoster {
                                // FOUNDING WINDOW (item 1): a fresh deployment — do
                                // NOT reject the very first scan. Ask the owner to
                                // confirm, then hold the login nonce + their profile
                                // name until they reply YES/NO. A stray scan never
                                // silently founds a household.
                                let name = founding_display_name(&msg.sender);
                                pending_founding.insert(
                                    telegram_id.to_string(),
                                    PendingFounding {
                                        nonce: nonce.to_string(),
                                        name: name.clone(),
                                        created: chrono::Utc::now().timestamp(),
                                    },
                                );
                                println!(
                                    "[{}] empty-roster founding offered to {} (awaiting YES/NO)",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    msg.sender,
                                );
                                let q = format!(
                                    "You're setting up this home, {name} — are you the owner? \
                                     Reply YES to make this your household, or NO to cancel."
                                );
                                if let Err(e) = channel.send_text(&reply_target, &q).await {
                                    eprintln!("Failed to send founding prompt: {e}");
                                }
                            } else {
                                let reply = outcome.reply();
                                // Redacted breadcrumb — the outcome, never the nonce.
                                println!(
                                    "[{}] web sign-in confirm from {} -> {:?}",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    msg.sender,
                                    outcome,
                                );
                                if let Err(e) = channel.send_text(&reply_target, &reply).await {
                                    eprintln!("Failed to send web sign-in reply: {e}");
                                }
                            }
                        }
                        // No numeric id to verify — cannot bind a session.
                        None => {
                            let reply = "That sign-in link expired — reopen the Casa page and tap the fresh link.";
                            if let Err(e) = channel.send_text(&reply_target, reply).await {
                                eprintln!("Failed to send web sign-in reply: {e}");
                            }
                        }
                    }
                    continue;
                }
                // INVITE (item 2): a 1:1 `/start join_<nonce>` deep link. The
                // invitee taps the QR/link the operator generated in Manage
                // household; we POST their REAL telegram id + the nonce to the
                // loopback gateway `/invite/redeem`, which creates their binding
                // under the invite's name and confirms it (the tap is the
                // handshake). One-directional, token-free. NEVER log the nonce.
                if let Some(nonce) = parse_join_nonce(&msg.body) {
                    let reply = match msg.sender_id.as_deref() {
                        Some(telegram_id) => redeem_invite(&auth_client, nonce, telegram_id).await,
                        None => {
                            "That invite link needs your Telegram profile — open it from your own chat with the bot.".to_string()
                        }
                    };
                    // Redacted breadcrumb — outcome only, never the nonce.
                    println!(
                        "[{}] invite redeem from {} -> {}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        msg.sender,
                        if reply.starts_with("Welcome") { "joined" } else { "rejected" },
                    );
                    if let Err(e) = channel.send_text(&reply_target, &reply).await {
                        eprintln!("Failed to send invite reply: {e}");
                    }
                    continue;
                }
            }

            // FOUNDING handshake reply (item 1): the empty-roster scanner answers
            // YES/NO to Otto's "are you the owner?" prompt. This is a PLAIN message
            // (no slash command), so we intercept it here — before family commands,
            // confirmation routing, and the conversation composer — whenever the
            // sender has a founding handshake in flight. On YES we found the
            // household; on NO we cancel; anything else re-prompts. The window
            // expires with the login nonce so a late reply is treated as a normal
            // message. See docs/16-web-identity.md §The founding member.
            if is_private {
                if let Some(tid) = msg.sender_id.clone() {
                    if let Some(pending) = pending_founding.get(&tid).cloned() {
                        let now = chrono::Utc::now().timestamp();
                        if now - pending.created > FOUNDING_TTL_SECS {
                            // Expired — drop it and let the message fall through.
                            pending_founding.remove(&tid);
                        } else if worksgood::agency::human_binding::is_affirmative(&msg.body) {
                            pending_founding.remove(&tid);
                            let reply =
                                found_household(&auth_client, &pending.nonce, &tid, &pending.name)
                                    .await;
                            println!(
                                "[{}] FOUNDED household — first member {} ({})",
                                chrono::Utc::now().format("%H:%M:%S"),
                                pending.name,
                                msg.sender,
                            );
                            if let Err(e) = channel.send_text(&reply_target, &reply).await {
                                eprintln!("Failed to send founding welcome: {e}");
                            }
                            continue;
                        } else if is_negative(&msg.body) {
                            pending_founding.remove(&tid);
                            println!(
                                "[{}] founding declined by {}",
                                chrono::Utc::now().format("%H:%M:%S"),
                                msg.sender,
                            );
                            let reply = "No problem — nothing was set up. Tap the tablet again whenever you're ready.";
                            if let Err(e) = channel.send_text(&reply_target, reply).await {
                                eprintln!("Failed to send founding cancel: {e}");
                            }
                            continue;
                        } else {
                            // Ambiguous — re-prompt without consuming the window.
                            let reply = "Just reply YES to set up this home as yours, or NO to cancel.";
                            if let Err(e) = channel.send_text(&reply_target, reply).await {
                                eprintln!("Failed to send founding re-prompt: {e}");
                            }
                            continue;
                        }
                    }
                }
            }

            // Family command set (/dinner /shopping /week /reminders /standup
            // /help). Commands ride ABOVE the election table: the surviving
            // (deduped) copy is exactly-once, and a bare slash command must
            // never be silenced as small-talk — so we resolve it here, before
            // election. In a group the command's OWNER answers (Bruno for
            // /dinner) regardless of which bot's queue delivered the surviving
            // copy; in a 1:1 the bot you messaged answers. `/standup` fans out
            // to the whole roster. See `notify::telegram_family_commands`.
            if gate.family {
                if let Some(cmd) =
                    worksgood::notify::telegram_family_commands::match_command(&msg.body)
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
            // Fix #5 — resolve the sender ONCE against the binding map at the
            // boundary. `auth_sender` is the binding's stored key when the sender
            // (by numeric id or @username) is recognized, so the verbatim
            // `find_by_user` lookups downstream (classify + the 1:1 AND collective
            // conversation composers) match a confirmed human even with no public
            // @username. Falls back to the raw display label for unbound senders.
            let auth_sender = resolve_auth_sender(&workgraph_dir, &msg);

            // Membership-aware silence: count the onboarded humans so a
            // single-human group (Casa Pinello: one human, four bots) answers
            // greetings instead of protecting non-existent human-to-human
            // chatter. Recomputed per message so a newly-onboarded human flips
            // the group back to the conservative silence rule without a restart.
            let human_count = human_agent_id_set(&workgraph_dir).len();

            let election = elect_responders(
                msg.chat_type.as_deref(),
                msg.chat_id.as_deref(),
                &msg.body,
                &msg.mention_usernames,
                msg.reply_to_bot.as_deref(),
                // Defense in depth behind the boundary guard above — a bot-sent
                // message never reaches here, but the election refuses it too.
                msg.sender_is_bot,
                human_count,
                &route_config,
            );

            // Observability: exactly ONE decision line per consumed message —
            // which election rule fired and where it landed — emitted for EVERY
            // case, silence included (small-talk silence previously logged
            // nothing, so "no reply" was indistinguishable from a dropped
            // message). PII-safe: the message id, not its text, and no tokens.
            // See `telegram_group::election_decision_summary`.
            println!(
                "[{}] election {}",
                chrono::Utc::now().format("%H:%M:%S"),
                election_decision_summary(
                    msg.message_id.as_deref(),
                    msg.chat_type.as_deref(),
                    &election,
                ),
            );

            // If the concierge (auto-routed otto) case admits below, this holds
            // the agent id so the spawned turn can mark its reply sent (BUG 2).
            let mut coalesced_named_agent: Option<String> = None;

            let (route_channel, route_body) = match election {
                Election::Silence(_) => {
                    // Small-talk / no-chat-id / no-voice — bots stay quiet. The
                    // decision line above already recorded the reason.
                    continue;
                }
                Election::All {
                    ref reply_chat,
                    ref body,
                } => {
                    // Fix #2 — burst coalescing. Multiple collective elections
                    // that arrive WHILE a roster reply is still composing collapse
                    // to ONE reply; a follow-up after the reply was sent is a new
                    // turn (BUG 2, pending-only coalescing).
                    if !coalescer
                        .lock()
                        .unwrap()
                        .admit_collective(chrono::Utc::now().timestamp())
                    {
                        println!(
                            "[{}] collective coalesced (burst) — msg {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            msg.message_id.as_deref().unwrap_or("none"),
                        );
                        continue;
                    }
                    // A collective election splits two ways. An opinion /
                    // discussion ask ("can you guys discuss this and find
                    // consensus", "what do you all think?") runs a DISCUSSION
                    // ROUND — sequenced, reacting in-voice takes plus an Otto
                    // wrap-up (`run_group_discussion`). Everything else (plain
                    // collective greetings, "hey guys are you around?") keeps
                    // today's behavior: four brief independent replies, in roster
                    // order (`run_group_collective`). The burst coalescer above
                    // already guarantees a second discussion ask arriving WHILE a
                    // round is composing does not start a second round.
                    let run = if is_discussion_ask(body) {
                        run_group_discussion(
                            &workgraph_dir,
                            &route_config,
                            reply_chat,
                            &feed_path,
                            body,
                            &auth_sender,
                        )
                        .await
                    } else {
                        // Collective address — the whole roster answers, briefly
                        // and in-voice, in roster order. The single listener
                        // orchestrates the sequential sends so no bot double-posts.
                        // Fix #4b: each voice answers the MESSAGE CONTENT through
                        // the SAME persistent-session composer the 1:1 path uses
                        // (grounded reply), falling back to a task-grounded in-voice
                        // line only when that voice has no bound session. The
                        // composed turn logs its own compose-start + per-voice sent
                        // message_id.
                        run_group_collective(
                            &workgraph_dir,
                            &route_config,
                            reply_chat,
                            &feed_path,
                            body,
                            &auth_sender,
                        )
                        .await
                    };
                    if let Err(e) = run {
                        eprintln!("Failed to run collective reply: {e}");
                    }
                    // The roster reply has gone out — end the pending turn so the
                    // next collective message is a fresh turn, not coalesced away
                    // (BUG 2). The collective send is awaited inline here, so it is
                    // provably sent by this point.
                    coalescer.lock().unwrap().mark_collective_sent();
                    continue;
                }
                Election::One {
                    ref bot,
                    ref body,
                    ref reply_chat,
                    addressed_by,
                } => {
                    debug_assert_eq!(reply_chat, &reply_target);
                    // Fix #2 — burst coalescing for the AUTO-routed concierge
                    // case (an unaddressed team ask that lands on otto). Repeated
                    // concierge elections that arrive WHILE the turn is composing
                    // collapse to one reply; a follow-up after the reply was sent
                    // is a NEW turn (BUG 2 — the swallowed "why they don't reply?").
                    // Explicit @mentions / addressed names / reply-chains are
                    // deliberate and are ALWAYS answered — never coalesced. A
                    // DOMAIN-elected auto-route (an unaddressed ask sent to its
                    // domain owner's voice) is the same kind of auto-routed team
                    // ask as the concierge case, so it coalesces the same way.
                    if matches!(
                        addressed_by,
                        worksgood::notify::telegram_group::AddressedBy::Concierge
                            | worksgood::notify::telegram_group::AddressedBy::Domain(_)
                    ) {
                        let agent = bot.agent_id.as_deref().unwrap_or(&bot.bot_id);
                        if !coalescer
                            .lock()
                            .unwrap()
                            .admit_named(agent, chrono::Utc::now().timestamp())
                        {
                            println!(
                                "[{}] concierge coalesced (burst) — msg {}",
                                chrono::Utc::now().format("%H:%M:%S"),
                                msg.message_id.as_deref().unwrap_or("none"),
                            );
                            continue;
                        }
                        // Remember the admitted agent so the spawned turn below can
                        // mark its reply sent when it finishes composing (BUG 2).
                        coalesced_named_agent = Some(agent.to_string());
                    }
                    (bot.channel_type.clone(), body.clone())
                }
                Election::Private => (msg.channel.clone(), msg.body.clone()),
            };

            // Family command with a leading @mention (e.g. "@otto /shopping").
            // Reached only when election stripped a leading mention off the
            // front — and by Fix (2) an @mention/name election OWNS the message:
            // it addresses an agent, so the agent converses rather than a command
            // racing the election. `gate.family` is keyed off the offset-0 slash
            // entity of the ORIGINAL message, so a mention-prefixed body never
            // qualifies here; a bare `/shopping` was already handled pre-election.
            if gate.family {
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
            }

            // Operator WG command reference (claim/done/fail/status/ready/help).
            // This is COORDINATOR content — backticks and wg vocabulary — and it
            // must NEVER surface in a family group (`fix-command-leaks`: a bare
            // `?` fired the operator HELP and dumped the claim/done reference into
            // the family chat). `gate.operator` runs it ONLY in a 1:1 operator DM
            // and ONLY for a genuine slash command. In a group the election above
            // owns the message and the conversational composer answers.
            let operator_cmd = if gate.operator {
                worksgood::telegram_commands::parse(&route_body)
            } else {
                None
            };
            if let Some(cmd) = operator_cmd {
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
                // `auth_sender` (resolved once above) is what classify + the
                // conversation composer authorize against, so a confirmed human
                // with no public @username still resolves. See Fix #5.
                match classify_inbound_message(
                    &workgraph_dir,
                    &route_channel,
                    &auth_sender,
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
                        // Not a command, not a button, not an onboarding YES, and
                        // not a reply to a parked awaiting-human task — so it is a
                        // plain conversational turn that routed to ONE agent. This
                        // is the shared dead-end both entry points used to hit
                        // (1:1 `Election::Private` and group `Election::One`); the
                        // conversational composer answers it here. Awaiting-human
                        // task routing above still wins — this only runs when it
                        // returned Unmatched, so precedence is preserved.
                        use worksgood::notify::telegram_conversation as convo;

                        // Reminder-intent short-circuit: "Otto remind me Thursday
                        // to defrost the trout" registers a scheduled nudge and
                        // confirms in one line — no session round-trip. Only a
                        // confirmed human triggers it (else onboarding runs first).
                        if let Some(confirmation) = try_register_reminder(
                            &workgraph_dir,
                            &auth_sender,
                            &msg.sender,
                            &route_body,
                            chrono::Local::now().naive_local(),
                        ) {
                            let bot_id = convo::bot_id_for_channel(&route_config, &route_channel)
                                .unwrap_or_else(|| {
                                    route_channel
                                        .strip_prefix("telegram:")
                                        .unwrap_or(&route_channel)
                                        .to_string()
                                });
                            if let Some((_, bot)) = route_config
                                .all_bots()
                                .into_iter()
                                .find(|(id, _)| id == &bot_id)
                            {
                                let channel = TelegramChannel::from_bot(bot_id.clone(), bot);
                                if let Err(e) =
                                    channel.send_text(&reply_target, &confirmation).await
                                {
                                    eprintln!("Failed to send reminder confirmation: {e}");
                                }
                            }
                            println!(
                                "[{}] Registered reminder from {} -> confirmed via {}",
                                chrono::Utc::now().format("%H:%M:%S"),
                                msg.sender,
                                bot_id,
                            );
                            continue;
                        }

                        let entry = if matches!(
                            msg.chat_type.as_deref(),
                            Some("group") | Some("supergroup")
                        ) {
                            convo::Entry::GroupElected
                        } else {
                            convo::Entry::Direct
                        };
                        let plan = convo::plan_conversation(
                            &workgraph_dir,
                            &route_config,
                            &route_channel,
                            &reply_target,
                            &auth_sender,
                            entry,
                        );
                        // Route-decision log line (no tokens): every handled
                        // inbound is now visible, so a silent success can never
                        // again make diagnosis hard.
                        println!(
                            "[{}] Conversation ({}) from {} -> {} via {} [{}]",
                            chrono::Utc::now().format("%H:%M:%S"),
                            entry.label(),
                            msg.sender,
                            plan.route().chat_id,
                            plan.route().bot_id,
                            plan.kind_label(),
                        );

                        // PHOTO → SHOPPING (task photo-to-shopping). An inbound
                        // PHOTO that has elected to a persona is a VISION turn, not
                        // a text compose: Bruno (or whoever was named) looks at the
                        // fridge and adjusts the pickup list. Two hard limits:
                        //  · images only from CONFIRMED humans — a stranger's photo
                        //    is never downloaded or fed to the model;
                        //  · one turn per album — the first frame of a media group
                        //    fires; later frames are dropped (no loops on albums).
                        if msg.photo_file_id.is_some() {
                            if !sender_is_confirmed_human(&workgraph_dir, &msg) {
                                println!(
                                    "[{}] photo from unconfirmed sender {} — ignored",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    msg.sender,
                                );
                                continue;
                            }
                            if let Some(gid) = msg.media_group_id.as_deref() {
                                if !answered_media_groups.insert(gid.to_string()) {
                                    println!(
                                        "[{}] album frame (group {}) — already answering, dropped",
                                        chrono::Utc::now().format("%H:%M:%S"),
                                        gid,
                                    );
                                    continue;
                                }
                            }
                            match handle_photo_shopping_turn(
                                &workgraph_dir,
                                &msg,
                                &plan,
                                &channels,
                                &route_config,
                                wg_config.as_ref(),
                            )
                            .await
                            {
                                Ok(()) => println!(
                                    "[{}] photo-to-shopping turn from {} handled",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    msg.sender,
                                ),
                                Err(e) => eprintln!(
                                    "[{}] photo-to-shopping turn from {} failed: {}",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    msg.sender,
                                    worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                                ),
                            }
                            continue;
                        }

                        // Run the turn off the poll loop so waiting on one agent's
                        // session reply never blocks the next inbound message.
                        let dir_owned = workgraph_dir.clone();
                        let cfg_owned = route_config.clone();
                        let human_message = route_body.clone();
                        let sender = msg.sender.clone();
                        let request_id = format!(
                            "tg-{}-{}-{}",
                            reply_target,
                            msg.message_id.as_deref().unwrap_or("na"),
                            sender,
                        );
                        let timing = convo::AckTiming::from_env();
                        // Mirror the persona's reply into the conversation pane's
                        // feed ONLY for a group-elected turn — a 1:1 DM is private
                        // and must never land in the shared group feed.
                        let mirror_group = matches!(entry, convo::Entry::GroupElected);
                        let feed_path_owned = feed_path.clone();
                        let wg_config_owned = wg_config.clone();
                        // Hand the coalescer + admitted agent into the spawn so it
                        // marks the reply *sent* when it finishes — ending the
                        // pending turn so the next follow-up is a new turn (BUG 2).
                        let coalescer_spawn = coalescer.clone();
                        let coalesced_agent_spawn = coalesced_named_agent.clone();
                        tokio::spawn(async move {
                            let base = convo::BotReplySink::new(cfg_owned.clone());
                            let sink: Box<dyn convo::ReplySink> = if mirror_group {
                                Box::new(FeedMirrorSink::new(base, feed_path_owned, cfg_owned))
                            } else {
                                Box::new(base)
                            };
                            // The composer is the fix: it drives a bounded one-shot
                            // `claude` turn so the reply COMPLETES (or fails fast into
                            // the "glitched" follow-up) instead of the open-loop hang.
                            let composer = wg_config_owned
                                .map(convo::OneshotComposer::from_config);
                            let composer_ref = composer
                                .as_ref()
                                .map(|c| c as &dyn convo::ReplyComposer);
                            match convo::run_conversation_turn(
                                &dir_owned,
                                &plan,
                                &human_message,
                                &request_id,
                                timing,
                                composer_ref,
                                sink.as_ref(),
                            )
                            .await
                            {
                                Ok(outcome) => println!(
                                    "[{}] Conversation from {} via {} — {}",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    sender,
                                    plan.route().bot_id,
                                    outcome.label(),
                                ),
                                Err(e) => eprintln!(
                                    "[{}] Conversation turn from {} failed: {e}",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    sender,
                                ),
                            }
                            // The turn is done (reply sent, or failed fast into the
                            // glitched follow-up) — end its pending window so a
                            // genuine follow-up to the concierge is a NEW turn, not
                            // coalesced away (BUG 2). Only set for the concierge
                            // case; None for 1:1 / named turns that never coalesced.
                            if let Some(agent) = coalesced_agent_spawn {
                                coalescer_spawn.lock().unwrap().mark_named_sent(&agent);
                            }
                        });
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
    // `sender` is the already-resolved binding key (numeric id or @handle).
    // Passing it as both id and username satisfies Erik's `matches_sender`
    // contract for either key kind (numeric matches on id, handle on username).
    let name = apply_confirmation(&mut bindings, sender, Some(sender), body, chrono::Utc::now())?;
    if let Err(e) = bindings.save(&agency_dir) {
        eprintln!("Failed to persist Telegram binding confirmation: {e}");
        return None;
    }
    Some(name)
}

/// Detect a "remind me …" request in a plain conversational turn, register it as
/// an ad-hoc reminder, and return the one-line family-voice confirmation.
///
/// Returns `None` — leaving the message to the normal composer — when the text is
/// not a reminder request, when it carries no time expression, or when the sender
/// is not a confirmed human (an unconfirmed sender must still onboard first). The
/// registered reminder lands in `<root>/.casa/reminders-adhoc.json`, which the
/// scheduler tick (`wg telegram remind`) later fires exactly once.
fn try_register_reminder(
    workgraph_dir: &Path,
    sender: &str,
    sender_display: &str,
    body: &str,
    now: chrono::NaiveDateTime,
) -> Option<String> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::reminder::{self, AdHocStore};

    let intent = reminder::parse_reminder_intent(body, now)?;

    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    // The sender must be a confirmed human; resolve their display name + bot.
    let binding = bindings.find_by_identity(Some(sender), Some(sender_display));
    let (recipient, bot) = match binding {
        Some(b) if b.confirmed => (
            b.name.clone(),
            b.bot_id.clone().unwrap_or_else(|| "otto".to_string()),
        ),
        _ => return None,
    };

    let rem = reminder::intent_to_reminder(&intent, &recipient, &bot);
    let root = project_root(workgraph_dir);
    let path = AdHocStore::path(&root);
    let mut store = AdHocStore::load(&path);
    if store.add(rem) {
        if let Err(e) = store.save(&path) {
            eprintln!("Failed to persist ad-hoc reminder: {e}");
            return None;
        }
    }
    Some(intent.confirmation)
}

/// Resolve an inbound message's sender to the identity downstream auth uses
/// (Fix #5 — the lead bug).
///
/// The listener now carries both the numeric Telegram user id (`sender_id`) and
/// the display label (`sender`). Binding lookups downstream
/// (`classify_inbound_message`, `plan_conversation`) match `telegram_user`
/// **verbatim**, so a human bound by their numeric id but arriving with only a
/// @username (or no username at all) never resolved — the "unrecognized sender
/// 'unknown'" live failure. Here we resolve ONCE at the boundary against the
/// binding map, trying the numeric id first then the username, and return the
/// binding's stored key so the verbatim downstream lookups match. When no
/// binding claims the sender we fall back to the raw display label (an unbound
/// human is handled exactly as before).
fn resolve_auth_sender(workgraph_dir: &Path, msg: &worksgood::notify::IncomingMessage) -> String {
    use worksgood::agency::TelegramBindingMap;
    let agency_dir = workgraph_dir.join("agency");
    match TelegramBindingMap::load(&agency_dir) {
        Ok(map) => map
            .find_by_identity(msg.sender_id.as_deref(), Some(&msg.sender))
            .map(|b| b.telegram_user.clone())
            .unwrap_or_else(|| msg.sender.clone()),
        Err(_) => msg.sender.clone(),
    }
}

/// Whether `msg`'s sender is a **confirmed** onboarded human (a binding that
/// completed the YES handshake). The photo-to-shopping vision turn only ever
/// runs for confirmed humans — a stranger's image is never downloaded or fed to
/// the model. Matches on the numeric id first (Fix #5), then the @username.
fn sender_is_confirmed_human(
    workgraph_dir: &Path,
    msg: &worksgood::notify::IncomingMessage,
) -> bool {
    use worksgood::agency::TelegramBindingMap;
    let agency_dir = workgraph_dir.join("agency");
    match TelegramBindingMap::load(&agency_dir) {
        Ok(map) => map
            .find_by_identity(msg.sender_id.as_deref(), Some(&msg.sender))
            .map(|b| b.confirmed)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Run one photo → shopping-list vision turn end-to-end for an inbound photo
/// that has ELECTED to a persona (task `photo-to-shopping`). Downloads the image
/// with the RECEIVING bot's token (the `file_id` is bot-specific), reads the
/// current shopping list from the gateway, runs the vision compose turn grounded
/// in the elected persona's voice, applies the implied list changes through the
/// SAME gateway endpoints the kiosk taps, and replies in-persona. Awaited inline
/// (photos are infrequent; correctness over throughput) — it fails fast into the
/// gentle note on any error so the family never sees a hang.
async fn handle_photo_shopping_turn(
    workgraph_dir: &Path,
    msg: &worksgood::notify::IncomingMessage,
    plan: &worksgood::notify::telegram_conversation::ConversationPlan,
    channels: &[TelegramChannel],
    route_config: &TelegramConfig,
    wg_config: Option<&worksgood::config::Config>,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_conversation::ReplySink as _;
    use worksgood::notify::telegram_photo as photo;

    // The photo `file_id` is only valid for the bot that received it, so we must
    // download via THAT bot's channel.
    let receiving = channels
        .iter()
        .find(|c| c.channel_type() == msg.channel)
        .with_context(|| format!("no channel matches receiving bot {:?}", msg.channel))?;

    let turn = match photo::coalesce_album(std::slice::from_ref(msg)).into_iter().next() {
        Some(t) => t,
        None => return Ok(()), // not a photo (shouldn't happen — caller gated)
    };

    let route = plan.route();
    let sink = convo::BotReplySink::new(route_config.clone());

    // No model config → we can't run vision. Acknowledge gracefully rather than
    // going silent, and invite the human to say what they need in words.
    let Some(cfg) = wg_config else {
        sink.send(
            &route.bot_id,
            &route.chat_id,
            "Got your photo! I can't read pictures right now — tell me what you need and I'll update the list.",
        )
        .await?;
        return Ok(());
    };

    let composer = convo::OneshotComposer::from_config(cfg.clone());
    let persona_summary = plan
        .session_ref()
        .and_then(|s| convo::read_session_summary(workgraph_dir, s));
    let gateway = photo::HttpShoppingGateway::from_env();

    // A per-message scratch dir for the temp image(s); removed after the turn.
    let scratch = std::env::temp_dir().join(format!(
        "casa-photo-{}-{}",
        msg.message_id.as_deref().unwrap_or("na"),
        msg.sender_id.as_deref().unwrap_or("na"),
    ));
    std::fs::create_dir_all(&scratch).ok();

    let outcome = photo::run_photo_shopping_turn(
        &turn,
        persona_summary.as_deref(),
        &photo::PhotoLimits::default(),
        receiving,
        &composer,
        &gateway,
        &scratch,
    )
    .await;
    let _ = std::fs::remove_dir_all(&scratch);

    let result = outcome?;
    sink.send(&route.bot_id, &route.chat_id, &result.reply_text)
        .await?;
    Ok(())
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
/// The neutral listener log line for a benign fall-through: a confirmed human's
/// ordinary message reached an AI persona's bot, so it is not a parked-task
/// reply and continues on to the conversation composer. Uses the persona's
/// display name (never the raw agent hash) and reads as "continuing", not a
/// refusal — so tailing the listener log does not cry wolf on every normal group
/// message (the misleading "Rejected reply from …" noise this replaces).
fn fallthrough_log_line(persona: &str) -> String {
    format!("not a parked-task reply (bot fronts {persona}) — continuing to conversation")
}

fn classify_inbound_message(
    workgraph_dir: &Path,
    channel_type: &str,
    sender: &str,
    body: &str,
) -> InboundOutcome {
    use crate::commands::service::human_dispatch::InboundReplyOutcome;

    // 1. Confirmation check first — this is the ordering fix.
    let confirmed_name = try_confirm_binding(workgraph_dir, sender, body);
    // 2. Then awaiting-human task routing. The router now authorizes the sender
    //    against their CONFIRMED Telegram binding before recording anything
    //    (PR #51 hardening): only a proven sender lands on the human's own task.
    //    A `Rejected` outcome is a security event (unproven/mismatched sender) —
    //    we log it server-side and treat it as unmatched for routing purposes.
    let routed_task = match crate::commands::service::human_dispatch::route_inbound_reply(
        workgraph_dir,
        channel_type,
        sender,
        // `sender` is the already-resolved binding key (numeric id or @handle).
        // Passing it as both id and username reproduces the old exact-key
        // `find_by_user` match under Erik's `find_by_sender(id, username)`:
        // a numeric key matches on id, a handle key matches on username.
        Some(sender),
        body,
    ) {
        InboundReplyOutcome::Recorded(task_id) => Some(task_id),
        InboundReplyOutcome::NoWaitingTask => None,
        InboundReplyOutcome::NotParkedReply { persona } => {
            // Benign, NOT a failure: a confirmed human's ordinary message reached
            // an AI persona's bot, so it is not a parked-task answer and falls
            // through to the conversation composer below. Log a neutral one-liner
            // (persona NAME, never the raw agent hash) — the old "Rejected reply"
            // wording read as a refusal and tripped the operator problems monitor
            // on every normal group message.
            eprintln!(
                "[{}] {}",
                chrono::Utc::now().format("%H:%M:%S"),
                fallthrough_log_line(&persona)
            );
            None
        }
        InboundReplyOutcome::Rejected(reason) => {
            eprintln!(
                "[{}] Rejected reply from {}: {}",
                chrono::Utc::now().format("%H:%M:%S"),
                sender,
                reason
            );
            None
        }
    };
    match (confirmed_name, routed_task) {
        (Some(name), routed_task) => InboundOutcome::Confirmed { name, routed_task },
        (None, Some(task_id)) => InboundOutcome::Routed { task_id },
        (None, None) => InboundOutcome::Unmatched,
    }
}

/// Send a message to the configured Telegram chat.
///
/// `persona` names the composing voice (e.g. `otto` for the Sunday review
/// digest). When set, the message is bound to that persona's bot and a
/// misconfigured persona is a hard error — see [`resolve_send_bot`]. When
/// `None`, the plain default-bot resolution applies.
pub fn run_send(
    chat_id: Option<&str>,
    message: &str,
    dry_run: bool,
    persona: Option<&str>,
) -> Result<()> {
    let config = load_telegram_config()?;
    let (bot_id, bot, effective_chat_id) = resolve_send_bot(&config, chat_id, persona)?;

    if dry_run {
        // Resolution-only path: prove which bot + chat + URL a real send would
        // use, with the token redacted. The URL host segment MUST be
        // `bot<digits>:...` — an empty token (the old bots-map-only 404 bug)
        // would render `bot/sendMessage`.
        if let Some(p) = persona {
            println!("[dry-run] composing persona: {}", p);
        }
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
/// When `persona` is `Some`, the caller is naming the COMPOSING voice (e.g. the
/// Sunday review digest is signed "— Otto", so it must leave via Otto's bot).
/// The message is then bound to the bot whose id OR `agent_id` matches that
/// persona, and if none is configured this HARD ERRORS rather than silently
/// falling back to another bot. Silent wrong-identity delivery — "Otto's words
/// via Bruno's face" (task `review-digest-sent`) — is worse than a failed send,
/// so a persona-named send never resolves to a different persona's bot.
///
/// When `persona` is `None` (the plain `wg telegram send` default), this
/// prefers the legacy top-level `[telegram]` bot, falling back to the
/// lexicographically-first `[telegram.bots.*]` entry. Previously `run_send`
/// always built the channel from the top-level `bot_token`, which is EMPTY in a
/// bots-map-only config — producing the URL `https://api.telegram.org/bot/sendMessage`
/// and a bare 404 (task `listener-reconnect`). `all_bots()` lists the legacy
/// bot first when present, so it picks the correct default either way. The `bots`
/// map is a HashMap, so the fallback pins the lexicographically-first id rather
/// than a random one, keeping the default send stable. The effective chat id
/// defaults to the resolved bot's own chat when the caller passes none.
fn resolve_send_bot(
    config: &TelegramConfig,
    chat_id: Option<&str>,
    persona: Option<&str>,
) -> Result<(String, TelegramBotConfig, String)> {
    let mut bots = config.all_bots();
    if bots.is_empty() {
        anyhow::bail!(
            "No Telegram bots configured — set [telegram] bot_token/chat_id or a [telegram.bots.*] entry",
        );
    }

    let (bot_id, bot) = match persona {
        Some(p) => {
            // Bind to the named voice's bot (by bot id OR agent_id binding).
            // Refuse to fall back — the caller asked for THIS persona.
            match bots
                .iter()
                .find(|(id, bot)| id == p || bot.agent_id.as_deref() == Some(p))
            {
                Some((id, bot)) => (id.clone(), bot.clone()),
                None => anyhow::bail!(
                    "Telegram send requested as persona '{p}', but no bot is configured for it \
                     (neither a [telegram.bots.{p}] id nor an agent_id binding). Refusing to \
                     fall back to another persona's bot — a review/reminder/announcement signed \
                     as '{p}' must leave via {p}'s bot, not deliver under the wrong identity. \
                     Configured bots: {}",
                    bots.iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            }
        }
        None => {
            // The legacy top-level bot (always id "default") is listed first by
            // `all_bots` and wins when present. Otherwise pick the
            // lexicographically-first named bot for a stable default.
            if bots.first().map(|(id, _)| id == "default").unwrap_or(false) {
                bots.remove(0)
            } else {
                bots.into_iter()
                    .min_by(|a, b| a.0.cmp(&b.0))
                    .expect("bots is non-empty (checked above)")
            }
        }
    };
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| bot.chat_id.clone());
    Ok((bot_id, bot, effective_chat_id))
}

/// Resolve the family group chat id for an inbound/reply target, in priority
/// order: explicit `--chat-id` override → legacy top-level `[telegram] chat_id`
/// → the first non-empty `[telegram.bots.*]` chat id (all family bots share the
/// one group chat). Returns `None` only when nothing configures a chat id.
///
/// This mirrors what `resolve_send_bot`/`run_listen` already do: the multi-bot
/// map is now the norm and the legacy top-level `chat_id` is often empty, so a
/// caller that reads only `config.chat_id` bails on a perfectly-valid bots-map
/// config (task `urgent-web-inbound` — the kiosk web-inbound regression).
fn resolve_group_chat_id(config: &TelegramConfig, chat_id_override: Option<&str>) -> Option<String> {
    if let Some(o) = chat_id_override {
        let o = o.trim();
        if !o.is_empty() {
            return Some(o.to_string());
        }
    }
    if !config.chat_id.trim().is_empty() {
        return Some(config.chat_id.clone());
    }
    config
        .all_bots()
        .into_iter()
        .map(|(_, bot)| bot.chat_id)
        .find(|c| !c.trim().is_empty())
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
    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);

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

/// `wg telegram resolve-sender` — the Fix #5 diagnostic. Resolve the sender of
/// a raw Telegram update against the binding map through the exact boundary path
/// the listener uses, and print the result.
///
/// `update` is the raw `getUpdates` element as JSON — inline, or `@path` to read
/// it from a file. Bindings are loaded from `<workgraph_dir>/agency`. Nothing is
/// sent; this only reads. Proves a human bound by their numeric id resolves even
/// with no @username (the "unrecognized sender 'unknown'" live failure).
pub fn run_resolve_sender(workgraph_dir: &Path, update: &str, json: bool) -> Result<()> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::telegram_sender;

    let raw = if let Some(path) = update.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read update fixture {path}"))?
    } else {
        update.to_string()
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("update is not valid JSON")?;

    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let resolved = telegram_sender::resolve_inbound(&value, &bindings);

    if json {
        let out = serde_json::json!({
            "sender_id": resolved.identity.user_id,
            "username": resolved.identity.username,
            "is_bot": resolved.identity.is_bot,
            "agent_id": resolved.agent_id,
            "name": resolved.name,
            "confirmed": resolved.confirmed,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{}", telegram_sender::resolve_inbound_summary(&value, &bindings));
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

    let election = elect_responders(
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

/// `wg telegram discuss --dry-run` — show whether a group message would run a
/// DISCUSSION ROUND, and the planned round, without sending anything.
///
/// Runs the exact [`elect_responders`] decision the listener uses, then applies
/// the same [`is_discussion_ask`] gate the live `Election::All` handler uses to
/// split a collective election into a discussion round vs today's four
/// independent hellos. Prints the category and, for a round, the voices in
/// contribution order plus the synthesizer (Otto). This is the scripted-test
/// seam (sibling of `wg telegram elect`): a discussion ask → `discussion-round`;
/// a plain collective greeting → `collective-greeting`; a named/concierge ask →
/// `single-voice`; small talk → `silence`. Nothing is sent.
pub fn run_discuss(workgraph_dir: &Path, message: &str, json: bool) -> Result<()> {
    use worksgood::notify::telegram_discussion as discussion;
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;
    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
    let human_count = human_agent_id_set(workgraph_dir).len();

    let election = elect_responders(
        Some("supergroup"),
        Some("-1000000000001"),
        message,
        &mention_usernames,
        None,
        // The diagnostic is always run by a human operator, never a bot.
        false,
        human_count,
        &config,
    );

    let is_discussion = is_discussion_ask(message);
    let roster_ids: Vec<String> = standup::plan_roster(&config, standup::DEFAULT_ROSTER)
        .into_iter()
        .map(|m| m.bot_id)
        .collect();

    // (category, plan) — plan is Some only for a discussion round.
    let (category, plan): (&str, Option<discussion::DiscussionPlan>) = match &election {
        Election::Private => ("private", None),
        Election::Silence(_) => ("silence", None),
        Election::One { .. } => ("single-voice", None),
        Election::All { .. } => {
            if is_discussion {
                ("discussion-round", Some(discussion::plan_round(&roster_ids)))
            } else {
                ("collective-greeting", None)
            }
        }
    };

    if json {
        let out = serde_json::json!({
            "category": category,
            "is_discussion_ask": is_discussion,
            "voices": plan.as_ref().map(|p| p.take_voices.clone()),
            "synthesizer": plan.as_ref().and_then(|p| p.synthesizer.clone()),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match category {
        "discussion-round" => {
            let plan = plan.unwrap();
            println!(
                "discussion round — each voice gives a short take, in order: {}",
                if plan.take_voices.is_empty() {
                    "(none configured)".to_string()
                } else {
                    plan.take_voices.join(" → ")
                },
            );
            match plan.synthesizer {
                Some(s) => println!(
                    "then {s} closes with a synthesis (only if ≥2 other voices weigh in)"
                ),
                None => println!("no synthesizer configured — no closing wrap-up"),
            }
        }
        "collective-greeting" => println!(
            "collective greeting — the whole roster answers with brief independent hellos \
             (no discussion round)"
        ),
        "single-voice" => {
            println!("single voice — one bot answers (named/mention/reply/concierge); no round")
        }
        "silence" => println!("silence — no one responds; no round"),
        _ => println!("private chat — 1:1 passthrough; no round"),
    }
    Ok(())
}

/// `wg telegram decide` — run the listener's command-vs-election decision on a
/// raw Telegram update, without sending anything.
///
/// Feeds the raw `getUpdates` element through the SAME boundary the live
/// listener uses: [`decode_update`] (which reads the Telegram entities so a
/// bare `?` is distinguished from a real `/help`), then [`command_gate`] and
/// [`elect_responders`]. Prints the decision — is it a command, and if not, who
/// the election routes it to. This is the `fix-command-leaks` proof: a bare `?`
/// or `@mention ?` must decide `conversation` with ZERO commands and never
/// touch the operator claim/done path.
pub fn run_decide(workgraph_dir: &Path, update: &str, json: bool) -> Result<()> {
    let raw = if let Some(path) = update.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read update fixture {path}"))?
    } else {
        update.to_string()
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("update is not valid JSON")?;

    let msg = match worksgood::notify::telegram::decode_update(&value, "telegram") {
        Some(m) => m,
        None => {
            if json {
                println!("{}", serde_json::json!({ "decision": "ignored" }));
            } else {
                println!("ignored — not a text message or button press");
            }
            return Ok(());
        }
    };

    let gate = command_gate(&msg);

    // If the message is a genuine slash command, that's the decision — report
    // which command path (family vs operator) and, for a family command, which
    // one it matches. Otherwise fall through to the election.
    if gate.family || gate.operator {
        let family = family_commands::match_command(&msg.body);
        let (kind, name) = if let Some(cmd) = family {
            ("family_command", Some(cmd.keyword.to_string()))
        } else if gate.operator {
            match worksgood::telegram_commands::parse(&msg.body) {
                Some(cmd) => ("operator_command", Some(cmd.description())),
                None => ("conversation", None),
            }
        } else {
            ("conversation", None)
        };
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "decision": kind,
                    "command": name,
                    "has_bot_command": msg.has_bot_command,
                })
            );
        } else {
            println!(
                "{} ({}) — has_bot_command={}",
                kind,
                name.as_deref().unwrap_or("-"),
                msg.has_bot_command,
            );
        }
        return Ok(());
    }

    // Not a command — this is conversation. Run the exact election the listener
    // would, so the diagnostic proves an addressed `@mention ?` routes to the
    // agent (converses) rather than firing a command.
    let config = load_telegram_config()?;
    // Membership-aware silence: mirror the listener by counting onboarded humans
    // so the diagnostic's election matches the live decision.
    let human_count = human_agent_id_set(workgraph_dir).len();
    let election = elect_responders(
        msg.chat_type.as_deref(),
        msg.chat_id.as_deref(),
        &msg.body,
        &msg.mention_usernames,
        msg.reply_to_bot.as_deref(),
        msg.sender_is_bot,
        human_count,
        &config,
    );
    let (elected, addressed_by): (Option<String>, Option<String>) = match &election {
        Election::One { bot, addressed_by, .. } => (
            bot.agent_id.clone().or_else(|| Some(bot.bot_id.clone())),
            Some(addressed_by.to_string()),
        ),
        Election::All { .. } => (Some("roster".to_string()), None),
        Election::Private => (None, None),
        Election::Silence(reason) => (None, Some(reason.to_string())),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "decision": "conversation",
                "command": serde_json::Value::Null,
                "has_bot_command": msg.has_bot_command,
                "elected": elected,
                "addressed_by": addressed_by,
            })
        );
    } else {
        println!(
            "conversation — no command; elected={} (by {}), has_bot_command={}",
            elected.as_deref().unwrap_or("(silence/1:1)"),
            addressed_by.as_deref().unwrap_or("-"),
            msg.has_bot_command,
        );
    }
    Ok(())
}

/// `wg telegram photo-plan` — diagnose the photo → shopping-list vision
/// pipeline WITHOUT a network (task `photo-to-shopping`).
///
/// Decodes the raw update(s) through the SAME `decode_update` boundary the
/// listener uses (photo `file_id`, caption, media group), coalesces album
/// frames into per-turn units, runs the real `elect_responders` decision on the
/// first turn's caption (who a captioned photo routes to), and — when a fixture
/// `--reply` + `--list` are given — parses the model's `SHOPPING_UPDATE:` tail
/// and prints the exact mutations that WOULD be applied through the gateway
/// endpoints. Nothing is downloaded and nothing is sent.
pub fn run_photo_plan(
    workgraph_dir: &Path,
    update: &str,
    reply: Option<&str>,
    list: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram as tg;
    use worksgood::notify::telegram_photo as photo;

    // --- 1. Decode update(s) → messages. Accept a single object or an array. ---
    let read_arg = |arg: &str| -> Result<String> {
        if let Some(path) = arg.strip_prefix('@') {
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read fixture {path}"))
        } else {
            Ok(arg.to_string())
        }
    };
    let raw = read_arg(update)?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("update is not valid JSON")?;
    let elements: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    let messages: Vec<worksgood::notify::IncomingMessage> = elements
        .iter()
        .filter_map(|u| tg::decode_update(u, "telegram"))
        .collect();

    // --- 2. Coalesce albums → per-turn units. ---
    let turns = photo::coalesce_album(&messages);

    // --- 3. Election on the first turn's caption (who answers a captioned photo). ---
    let config = load_telegram_config()?;
    let first = turns.first();
    let (elected, is_photo): (Option<String>, bool) = match first {
        Some(turn) => {
            let mention_usernames = parse_at_mention_tokens(&turn.caption);
            let human_count = human_agent_id_set(workgraph_dir).len();
            let election = elect_responders(
                turn.chat_type.as_deref(),
                turn.chat_id.as_deref(),
                &turn.caption,
                &mention_usernames,
                None,
                false,
                human_count,
                &config,
            );
            let who = match &election {
                Election::One { bot, .. } => {
                    bot.agent_id.clone().or_else(|| Some(bot.bot_id.clone()))
                }
                Election::All { .. } => Some("roster".to_string()),
                Election::Private => Some("(1:1 passthrough)".to_string()),
                Election::Silence(_) => None,
            };
            (who, true)
        }
        None => (None, false),
    };

    // --- 4. Optional: parse the reply tail + plan the mutations. ---
    let (verdict, actions): (Option<photo::VisionVerdict>, Vec<photo::ShoppingAction>) =
        if let Some(reply) = reply {
            let items = match list {
                Some(l) => {
                    let raw = read_arg(l)?;
                    let json: serde_json::Value =
                        serde_json::from_str(&raw).context("list is not valid JSON")?;
                    photo::parse_shopping_json(&json)
                }
                None => Vec::new(),
            };
            let v = photo::parse_vision_verdict(reply);
            let a = photo::plan_shopping_actions(&v, &items);
            (Some(v), a)
        } else {
            (None, Vec::new())
        };

    // --- 5. Report. ---
    if json {
        let actions_json: Vec<serde_json::Value> = actions
            .iter()
            .map(|a| match a {
                photo::ShoppingAction::CrossOff { key, text } => {
                    serde_json::json!({ "op": "cross_off", "key": key, "text": text })
                }
                photo::ShoppingAction::Restore { key, text } => {
                    serde_json::json!({ "op": "restore", "key": key, "text": text })
                }
                photo::ShoppingAction::Add { text } => {
                    serde_json::json!({ "op": "add", "text": text })
                }
            })
            .collect();
        let out = serde_json::json!({
            "is_photo": is_photo,
            "turns": turns.len(),
            "file_ids": turns.iter().flat_map(|t| t.file_ids.clone()).collect::<Vec<_>>(),
            "caption": first.map(|t| t.caption.clone()),
            "media_group_id": first.and_then(|t| t.media_group_id.clone()),
            "elected": elected,
            "have": verdict.as_ref().map(|v| v.have.clone()),
            "need": verdict.as_ref().map(|v| v.need.clone()),
            "actions": actions_json,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if !is_photo {
        println!("not a photo — no photo turn");
        return Ok(());
    }
    let turn = first.unwrap();
    println!(
        "photo — {} turn(s), {} image(s){}",
        turns.len(),
        turn.file_ids.len(),
        turn.media_group_id
            .as_deref()
            .map(|g| format!(", album group {g}"))
            .unwrap_or_default(),
    );
    println!("  caption: {}", if turn.caption.is_empty() { "(none)" } else { &turn.caption });
    println!(
        "  routes to: {}",
        elected.as_deref().unwrap_or("(silence — no one answers)")
    );
    if let Some(v) = &verdict {
        println!("  reply: {}", v.reply_text);
        println!("  have: {:?}", v.have);
        println!("  need: {:?}", v.need);
        if actions.is_empty() {
            println!("  actions: (none — list already matches)");
        }
        for a in &actions {
            match a {
                photo::ShoppingAction::CrossOff { text, .. } => {
                    println!("  action: cross off '{text}' (POST /shopping/toggle checked=true)")
                }
                photo::ShoppingAction::Restore { text, .. } => {
                    println!("  action: restore '{text}' (POST /shopping/toggle checked=false)")
                }
                photo::ShoppingAction::Add { text } => {
                    println!("  action: add '{text}' (POST /shopping/add)")
                }
            }
        }
    }
    Ok(())
}

/// `wg telegram classify` — show how the listener would CLASSIFY an inbound
/// (non-command, non-button) message once it has survived dedupe + election,
/// without sending anything.
///
/// This drives the exact [`classify_inbound_message`] the live `wg telegram
/// listen` loop invokes — the pure, filesystem-only core of the inbound branch
/// — against the real `.wg` (bindings, graph, `notify.toml`). It is the
/// diagnostic that would have made the pr51-auth swallow obvious: a CONFIRMED
/// human's plain chat turn that the hardened awaiting-task router rejects must
/// classify as `unmatched` (⇒ the conversational composer answers), never be
/// silently consumed. `--json` prints `{ kind, name?, task? }` where `kind` is
/// one of `confirmed` | `routed` | `unmatched`.
pub fn run_classify(
    workgraph_dir: &Path,
    channel: &str,
    sender: &str,
    message: &str,
    json: bool,
) -> Result<()> {
    let outcome = classify_inbound_message(workgraph_dir, channel, sender, message);

    // (kind, name, task) — flattened for a stable, scriptable shape.
    let (kind, name, task): (&str, Option<String>, Option<String>) = match &outcome {
        InboundOutcome::Confirmed { name, routed_task } => {
            ("confirmed", Some(name.clone()), routed_task.clone())
        }
        InboundOutcome::Routed { task_id } => ("routed", None, Some(task_id.clone())),
        InboundOutcome::Unmatched => ("unmatched", None, None),
    };

    if json {
        let out = serde_json::json!({
            "kind": kind,
            "name": name,
            "task": task,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match kind {
        "confirmed" => println!(
            "confirmed — onboarding binding for {} confirmed{}",
            name.as_deref().unwrap_or("?"),
            task.as_deref()
                .map(|t| format!(" (and reply recorded on task '{t}')"))
                .unwrap_or_default(),
        ),
        "routed" => println!(
            "routed — reply recorded on awaiting-human task '{}'",
            task.as_deref().unwrap_or("?"),
        ),
        // `unmatched` is the arm the live listener hands to the conversational
        // composer — a confirmed human's chat turn (incl. one the hardened
        // awaiting-task auth rejected) lands here, never swallowed.
        _ => println!("unmatched — falls through to the conversational composer"),
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
pub async fn run_group_collective(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
    feed_path: &Path,
    human_message: &str,
    sender: &str,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::plan_roster(config, standup::DEFAULT_ROSTER);
    if roster.is_empty() {
        eprintln!("No named bots configured — collective reply has no voices to post.");
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
            let sink = FeedMirrorSink::new(
                convo::BotReplySink::new(config.clone()),
                feed_path.to_path_buf(),
                config.clone(),
            );
            let request_id = format!("tg-collective-{}-{}", target, member.bot_id);
            let composer = wg_config
                .clone()
                .map(convo::OneshotComposer::from_config);
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

        let channel = TelegramChannel::from_bot(member.bot_id.clone(), member.bot.clone());
        match channel.send_text(target, &post.text).await {
            Ok(sent) => {
                println!(
                    "[{}] collective: {} replied (sent message_id {})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    post.bot_id,
                    sent.0,
                );
                // Mirror this voice's reply into the conversation pane's feed as
                // an `agent` line (the persona's answer relayed into the group).
                let entry =
                    casa_feed::agent_entry(member.agent_id(), &post.text, casa_feed::now_ms());
                if let Err(e) = casa_feed::append_entry(feed_path, &entry) {
                    eprintln!("collective: failed to mirror {} reply to feed: {e}", post.bot_id);
                }
            }
            Err(e) => eprintln!("collective: {} failed to reply: {e}", post.bot_id),
        }
    }
    Ok(())
}

/// Orchestrate a **discussion round** (collective election + an opinion /
/// discussion ask). Instead of four independent replies the family talks it
/// through: each bound-session persona contributes one short in-voice take, in
/// roster order and *reacting* to the takes so far, then Otto closes with a
/// synthesis when at least two other voices weighed in.
///
/// This is the deliberative sibling of [`run_group_collective`]: same sole
/// orchestrator (the single listener), same per-voice bot, same
/// persistent-session composer ([`convo::OneshotComposer`]) and casa-feed mirror
/// ([`FeedMirrorSink`]). It differs in that the takes are *sequenced with
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
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_discussion as discussion;
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::plan_roster(config, standup::DEFAULT_ROSTER);
    if roster.is_empty() {
        eprintln!("No named bots configured — discussion round has no voices.");
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
        )
        .await;
    }

    let timing = discussion::DiscussionTiming::from_env();
    println!(
        "[{}] discussion round -> {} ({} voice(s), synthesizer {})",
        chrono::Utc::now().format("%H:%M:%S"),
        target,
        voices.len(),
        CONCIERGE_BOT,
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
            )
            .await;
        }
    };
    let sink = FeedMirrorSink::new(
        convo::BotReplySink::new(config.clone()),
        feed_path.to_path_buf(),
        config.clone(),
    );

    let outcome = discussion::run_discussion_round(
        workgraph_dir,
        human_message,
        &voices,
        CONCIERGE_BOT,
        composer_ref,
        &sink,
        target,
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

// ---------------------------------------------------------------------------
// Casa conversation-pane feed mirror (group-elected agent replies)
// ---------------------------------------------------------------------------

/// A [`ReplySink`](worksgood::notify::telegram_conversation::ReplySink) decorator
/// that mirrors every reply it relays into the group to the conversation pane's
/// feed as an `agent` line, then delegates the real Telegram send to the wrapped
/// [`BotReplySink`].
///
/// Only ever wraps a **group-elected** turn (a 1:1 DM uses the bare sink) so a
/// private reply never leaks into the shared group feed. The replying `bot_id`
/// is mapped back to its persona id via [`agent_for_bot`]; the feed line carries
/// only the six display-safe fields — no token or chat id. A feed-write failure
/// is logged and swallowed so a full disk can never break the Telegram reply.
struct FeedMirrorSink {
    inner: worksgood::notify::telegram_conversation::BotReplySink,
    feed_path: PathBuf,
    config: TelegramConfig,
}

impl FeedMirrorSink {
    fn new(
        inner: worksgood::notify::telegram_conversation::BotReplySink,
        feed_path: PathBuf,
        config: TelegramConfig,
    ) -> Self {
        Self {
            inner,
            feed_path,
            config,
        }
    }
}

impl FeedMirrorSink {
    /// Mirror `text` into the casa feed as this bot's persona reply — but never
    /// the transient latency ack (it's edited away into the real answer, so the
    /// feed should carry only the answer/glitch line).
    fn mirror(&self, bot_id: &str, text: &str) {
        use worksgood::notify::telegram_conversation as convo;
        if text == convo::ack_line() {
            return;
        }
        let agent_id = convo::agent_for_bot(&self.config, bot_id);
        let entry = casa_feed::agent_entry(&agent_id, text, casa_feed::now_ms());
        if let Err(e) = casa_feed::append_entry(&self.feed_path, &entry) {
            eprintln!(
                "[{}] casa feed: failed to mirror agent reply: {e}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
    }
}

#[async_trait]
impl worksgood::notify::telegram_conversation::ReplySink for FeedMirrorSink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        // Send for real first; only mirror what actually went out to the group.
        let mid = self.inner.send(bot_id, chat_id, text).await?;
        self.mirror(bot_id, text);
        Ok(mid)
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        // The ack is being turned into the final answer (or glitch line) —
        // mirror that final text into the feed.
        self.inner.edit(bot_id, chat_id, message_id, text).await?;
        self.mirror(bot_id, text);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Web-origin inbound (kiosk conversation pane → first-class group turn)
// ---------------------------------------------------------------------------

/// Resolve a WEB session sender to a confirmed human's binding key.
///
/// A message typed in the kiosk pane carries a web identity — a `humanId` from
/// `GET /auth/me` (e.g. `luca`) or, pre-`web-identity-sign`, the household
/// default display name (`Luca`). The conversation composer downstream
/// (`sender_is_confirmed` → `find_by_user`) matches the binding's stored
/// `telegram_user` **verbatim** (a numeric Telegram id for Casa Pinello), so a
/// bare `luca`/`Luca` would never resolve as a confirmed human and every voice
/// would answer with the onboarding line instead of a grounded reply.
///
/// So we resolve here, mirroring `resolve_auth_sender`'s boundary role for the
/// Telegram path: first the direct identity match (numeric id or `@handle`),
/// then a web-friendly match on the binding's display `name` or its agency
/// `agent_id` (`human-luca`). We return the binding's `telegram_user` so the
/// verbatim downstream lookup recognizes the confirmed human. An unresolved
/// sender falls back to its raw form (handled exactly as an unbound human —
/// the onboarding line, never a crash).
fn resolve_web_sender(workgraph_dir: &Path, sender: &str) -> String {
    use worksgood::agency::TelegramBindingMap;
    let agency_dir = workgraph_dir.join("agency");
    let map = match TelegramBindingMap::load(&agency_dir) {
        Ok(m) => m,
        Err(_) => return sender.to_string(),
    };
    // 1. Direct identity match — numeric Telegram id or @username.
    if let Some(b) = map.find_by_identity(Some(sender), Some(sender)) {
        return b.telegram_user.clone();
    }
    // 2. Web identity is a display name / humanId — match the binding's `name`
    //    or its `human-`-prefixed agent id, then hand back the stored key.
    let want = sender.trim().trim_start_matches("human-").to_ascii_lowercase();
    if let Some(b) = map.bindings.iter().find(|b| {
        b.name.eq_ignore_ascii_case(sender)
            || b.agent_id
                .trim_start_matches("human-")
                .eq_ignore_ascii_case(&want)
    }) {
        return b.telegram_user.clone();
    }
    sender.to_string()
}

/// `wg telegram web-inbound --sender <humanId> --message <text>` — make a
/// web-origin (kiosk conversation-pane) message a **first-class group turn**.
///
/// The live gap this closes: the kiosk send box only RELAYED a line into the
/// family Telegram group via a bot, and Telegram bots never see other bots'
/// messages — so the listener's election/conversation pipeline NEVER ran on a
/// kiosk-typed message. It was posted (`💬 Luca (kiosk): …`) and never answered,
/// while the same words typed on a phone got four replies.
///
/// This command runs the SAME pipeline the listener runs on a group message,
/// without a live socket: it elects responder(s) with the exact
/// [`elect_responders`] table (@mention / addressed name / collective / concierge
/// / silence), then dispatches through the SAME senders + composer the listener
/// uses — [`run_group_discussion`] / [`run_group_collective`] for a collective
/// address, or the single-voice [`plan_conversation`] +
/// [`run_conversation_turn`] path for a named/concierge ask. Every reply goes
/// out to the group via the elected persona's OWN bot AND is mirrored into
/// `.casa/group-feed.jsonl` (via [`FeedMirrorSink`]) so the kiosk pane shows it.
///
/// The gateway shells out to this after it mirrors the kiosk line into the group.
///
/// # Double-reply / dedupe
/// The gateway ALSO mirrors the same human line into the Telegram group via the
/// relay bot. That mirrored copy is **bot-authored**, so the listener's
/// unconditional bot-loop guard ([`SilenceReason::BotSender`], Fix #0) drops it
/// before election — it can never be re-answered. Every reply THIS command posts
/// likewise goes out via a persona bot, so the listener drops those too. No
/// cross-process dedupe set is required: **bot authorship is the fingerprint that
/// covers the mirror**, exactly as it already does for every reply the listener
/// itself sends into the group.
///
/// [`SilenceReason::BotSender`]: worksgood::notify::telegram_group::SilenceReason
/// Pick the `(bot_id, chat)` a fast-lane confirmation should go out as: the
/// elected single voice when the ask elected one (so a food edit confirms in the
/// chef's voice, a workout edit in the coach's), else the first configured bot in
/// the family group. A fast-lane hit always has *something* to answer with.
fn fast_lane_reply_target(
    election: &Election,
    config: &TelegramConfig,
    target: &str,
) -> (String, String) {
    if let Election::One { bot, reply_chat, .. } = election {
        return (bot.bot_id.clone(), reply_chat.clone());
    }
    let bot_id = config
        .all_bots()
        .first()
        .map(|(id, _)| id.clone())
        .unwrap_or_default();
    (bot_id, target.to_string())
}

pub fn run_web_inbound(
    workgraph_dir: &Path,
    sender: &str,
    message: &str,
    chat_id_override: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;

    // Reply target: an explicit override wins, else the configured family group.
    // A recent engine change made the multi-bot `[telegram.bots.*]` map the norm
    // and left the legacy top-level `chat_id` empty, so a bots-map-only config made
    // this command bail with "no chat id" and Luca's kiosk asks died silently. Fall
    // back to the bots map the SAME way every send path does (`resolve_send_bot`,
    // `run_listen`): all family bots point at the one group chat, so the first
    // non-empty bot chat id is that group. See task `urgent-web-inbound`.
    let target = match resolve_group_chat_id(&config, chat_id_override) {
        Some(t) => t,
        None => anyhow::bail!(
            "no chat id — pass --chat-id, or configure telegram.chat_id / a \
             [telegram.bots.*] chat_id in notify.toml"
        ),
    };

    // Resolve the web identity to a confirmed human's binding key so the composer
    // treats them as a known human and answers grounded (see `resolve_web_sender`).
    let auth_sender = resolve_web_sender(workgraph_dir, sender);

    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
    let human_count = human_agent_id_set(workgraph_dir).len();

    // A web-origin message is first-class GROUP inbound — run the exact election
    // the listener runs, via the shared `elect_group_inbound` seam (supergroup,
    // no reply-chain, never bot-sent). The path-parity test locks this to the
    // listener's decision.
    let mut election =
        elect_group_inbound(&target, message, &mention_usernames, human_count, &config);

    // ── CLARIFICATION CONTINUATION ────────────────────────────────────────
    // A bare "yes"/"ok"/"si" from the same human within the clarify window is not
    // a fresh ask — it CONTINUES the exchange a persona just opened by asking a
    // clarifying question. Route it back to THAT voice carrying the ORIGINAL ask,
    // with NO re-election (the fennel bug re-elected a bare "yes" from scratch and
    // a different voice answered) and reusing the original fingerprint so the
    // downstream turn dedupes against the first task instead of minting a second.
    let clarify_root = project_root(workgraph_dir);
    let clarify_now = chrono::Utc::now().timestamp();
    let clarify_window = ownership::ClarifyLedger::window_secs();
    let mut clarify_continued_body: Option<String> = None;
    if let Some(ex) = ownership::clarify_continuation(
        &clarify_root,
        &target,
        &auth_sender,
        message,
        clarify_now,
        clarify_window,
    ) {
        if let Some(bot) = resolve_mentioned_bot(&ex.voice, &config) {
            println!(
                "[{}] web-inbound clarify-continuation from {} -> {} (reusing original ask)",
                chrono::Utc::now().format("%H:%M:%S"),
                sender,
                ex.voice,
            );
            election = Election::One {
                bot,
                reply_chat: target.clone(),
                body: ex.original_ask.clone(),
                addressed_by: worksgood::notify::telegram_group::AddressedBy::ReplyChain,
            };
            clarify_continued_body = Some(ex.original_ask.clone());
        }
    }

    // Observability: one decision line, PII-safe (no tokens, no chat id text).
    println!(
        "[{}] web-inbound election from {} -> {}",
        chrono::Utc::now().format("%H:%M:%S"),
        sender,
        election_decision_summary(None, Some("supergroup"), &election),
    );

    let feed_path = casa_feed::feed_path_for(&project_root(workgraph_dir));

    let category = match &election {
        Election::Silence(_) => "silence",
        Election::Private => "private",
        Election::All { body, .. } => {
            if is_discussion_ask(body) {
                "discussion-round"
            } else {
                "collective"
            }
        }
        Election::One { .. } => "single-voice",
    };

    // Dry-run seam (credential-free, like `wg telegram elect`/`discuss`): run the
    // real election + planning and report WHO would answer, but send nothing.
    // This is the scripted-test entry point — the smoke scenario asserts the
    // decision without a live bot or token.
    if dry_run {
        let who: Option<String> = match &election {
            Election::All { .. } => Some(
                standup::plan_roster(&config, standup::DEFAULT_ROSTER)
                    .into_iter()
                    .map(|m| m.bot_id)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Election::One { bot, .. } => {
                Some(bot.agent_id.clone().unwrap_or_else(|| bot.bot_id.clone()))
            }
            _ => None,
        };
        if json {
            let out = serde_json::json!({
                "dry_run": true,
                "category": category,
                "sender": sender,
                "auth_sender": auth_sender,
                "target": target,
                "who": who,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!(
                "web-inbound [{category}] from {sender} (dry-run) -> {}",
                who.as_deref().unwrap_or("(no reply)"),
            );
        }
        return Ok(());
    }

    // ── FAST LANE ─────────────────────────────────────────────────────────
    // A closed set of simple plan edits — a single meal swap/add/remove, a
    // shopping add, a reminder — applies DIRECTLY to the week's plan file right
    // here, in seconds, instead of spawning a full agent (worktree, edit, tests,
    // eval) that takes twenty minutes for "swap Friday to tacos". The edit is
    // round-tripped through the real plan parser before we confirm; anything
    // outside the closed set (or a compound ask like "…and rebalance the week")
    // returns Fallback and drops through to the full election pipeline below,
    // exactly as today. Only a confirmed human may trigger a direct write.
    if convo::sender_is_confirmed(workgraph_dir, &auth_sender) {
        let today = chrono::Local::now().date_naive();
        if let fast_lane::FastLaneResult::Applied { report, op, .. } =
            fast_lane::run_fast_lane(&project_root(workgraph_dir), message, today)
        {
            let (bot_id, chat) = fast_lane_reply_target(&election, &config, &target);
            let persona = convo::agent_for_bot(&config, &bot_id);
            println!(
                "[{}] fast-lane {} applied for {} -> {} ({})",
                chrono::Utc::now().format("%H:%M:%S"),
                op.kind_label(),
                sender,
                bot_id,
                report,
            );
            let sink = FeedMirrorSink::new(
                convo::BotReplySink::new(config.clone()),
                feed_path.clone(),
                config.clone(),
            );
            let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
            rt.block_on(async {
                use convo::ReplySink as _;
                let _ = sink.send(&bot_id, &chat, &report).await;
            });
            // Origin-stamp + brief graph visibility (a queued→done light) so the
            // fast-lane edit still shows in the constellation/timeline.
            let origin = worksgood::graph::TaskOrigin::new(
                worksgood::graph::OriginChannel::Web,
                chat.clone(),
                auth_sender.clone(),
                persona,
                Some(bot_id.clone()),
            );
            fast_lane::stamp_graph_node(workgraph_dir, &origin, &op, &report);

            if json {
                let out = serde_json::json!({
                    "category": "fast-lane",
                    "fast_lane_op": op.kind_label(),
                    "sender": sender,
                    "auth_sender": auth_sender,
                    "target": chat,
                    "outcome": report,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("web-inbound [fast-lane {}] from {sender}: {report}", op.kind_label());
            }
            return Ok(());
        }
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let outcome_label: String = rt.block_on(async {
        match &election {
            Election::Silence(reason) => {
                // Small talk / no voice — the bots deliberately stay quiet, exactly
                // as they would on the same words typed into Telegram.
                Ok::<String, anyhow::Error>(format!("silence ({reason}) — no reply sent"))
            }
            Election::Private => {
                // A supergroup election never resolves to Private; guard defensively
                // rather than leak a 1:1 into the shared group feed.
                Ok("private — no group reply".to_string())
            }
            Election::All { reply_chat, body } => {
                if is_discussion_ask(body) {
                    run_group_discussion(
                        workgraph_dir,
                        &config,
                        reply_chat,
                        &feed_path,
                        body,
                        &auth_sender,
                    )
                    .await?;
                    Ok("discussion round posted".to_string())
                } else {
                    run_group_collective(
                        workgraph_dir,
                        &config,
                        reply_chat,
                        &feed_path,
                        body,
                        &auth_sender,
                    )
                    .await?;
                    Ok("collective reply posted".to_string())
                }
            }
            Election::One {
                bot,
                reply_chat,
                body,
                ..
            } => {
                // Single voice — the SAME path the listener's `Unmatched` branch
                // runs for a named/@mention/concierge ask: plan the converse turn
                // for the elected bot and compose+send it, mirroring the reply into
                // the casa feed (this is a group-elected turn).
                let plan = convo::plan_conversation(
                    workgraph_dir,
                    &config,
                    &bot.channel_type,
                    reply_chat,
                    &auth_sender,
                    convo::Entry::GroupElected,
                );
                let sink = FeedMirrorSink::new(
                    convo::BotReplySink::new(config.clone()),
                    feed_path.clone(),
                    config.clone(),
                );
                let request_id = format!("web-{}-{}", reply_chat, bot.bot_id);
                let timing = convo::AckTiming::from_env();
                let wg_config = worksgood::config::Config::load_merged(workgraph_dir).ok();
                let composer = wg_config.map(convo::OneshotComposer::from_config);
                let composer_ref = composer.as_ref().map(|c| c as &dyn convo::ReplyComposer);
                let out = convo::run_conversation_turn(
                    workgraph_dir,
                    &plan,
                    body,
                    &request_id,
                    timing,
                    composer_ref,
                    &sink,
                )
                .await?;
                // Open a clarification window: if this human sends a bare
                // "yes"/"ok"/"si" in the next few minutes, it continues WITH THIS
                // VOICE carrying THIS ask (the clarify-continuation block above),
                // instead of re-electing from scratch. Skipped when this turn is
                // itself a continuation so a confirmed exchange can't loop. Best
                // effort — a ledger write failure never blocks the reply.
                if clarify_continued_body.is_none() {
                    let voice = bot
                        .agent_id
                        .clone()
                        .unwrap_or_else(|| bot.bot_id.clone());
                    if let Err(e) = ownership::ClarifyLedger::open(
                        &clarify_root,
                        &target,
                        &auth_sender,
                        &voice,
                        body,
                        clarify_now,
                    ) {
                        eprintln!("web-inbound clarify-ledger open failed (non-fatal): {e}");
                    }
                }
                Ok(format!(
                    "single voice ({}) answered [{}]",
                    bot.bot_id,
                    out.label()
                ))
            }
        }
    })?;

    if json {
        let out = serde_json::json!({
            "category": category,
            "sender": sender,
            "auth_sender": auth_sender,
            "target": target,
            "outcome": outcome_label,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("web-inbound [{category}] from {sender}: {outcome_label}");
    }
    Ok(())
}

/// Mirror one synthetic line to the casa conversation-pane feed (diagnostic).
///
/// Drives the EXACT `casa_feed` writer the listener uses, so a smoke test can
/// prove the feed writer end-to-end against the real binary without a live
/// group: `--kind group` writes an inbound human line (needs `--sender`),
/// `--kind agent` writes a relayed persona reply (needs `--agent-id`). Only the
/// six display-safe fields are written — never a token or chat id.
pub fn run_feed_write(
    root: &Path,
    kind: &str,
    sender: Option<&str>,
    agent_id: Option<&str>,
    text: &str,
) -> Result<()> {
    let entry = match kind {
        "group" => {
            let sender = sender.context("--kind group requires --sender")?;
            casa_feed::group_entry(sender, text, casa_feed::now_ms())
        }
        "agent" => {
            let agent_id = agent_id.context("--kind agent requires --agent-id")?;
            casa_feed::agent_entry(agent_id, text, casa_feed::now_ms())
        }
        other => anyhow::bail!("--kind must be 'group' or 'agent', got '{other}'"),
    };
    let feed_path = casa_feed::feed_path_for(root);
    casa_feed::append_entry(&feed_path, &entry)
        .with_context(|| format!("failed to append to feed {}", feed_path.display()))?;
    // The written line is itself display-safe (the six-field allowlist), so
    // echoing it back cannot leak a secret — handy for the smoke assertion.
    println!("{}", entry.to_json_line());
    Ok(())
}

// ---------------------------------------------------------------------------
// Family command set (/dinner /shopping /week /reminders /standup /help)
// ---------------------------------------------------------------------------

/// The project root that holds `plans/`. The graph dir is `<root>/.wg`, so when
/// `workgraph_dir` is a `.wg`/`.workgraph` subdir we step up to its parent;
/// otherwise we treat it as the root itself.
pub(crate) fn project_root(workgraph_dir: &Path) -> PathBuf {
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

    // Family-voice gate: a command reply bound for a family chat must never
    // carry coordinator content (markdown backticks or the WG claim/done
    // vocabulary). The compose fns are already family-voice; this is the
    // defensive brace that makes a regression loud instead of silent. See
    // `fix-command-leaks`.
    if !family_commands::is_family_voice(&text) {
        eprintln!(
            "[{}] REFUSING to send non-family-voice {} reply (contains operator vocabulary/backticks)",
            chrono::Utc::now().format("%H:%M:%S"),
            cmd.keyword,
        );
        debug_assert!(
            family_commands::is_family_voice(&text),
            "{} composed a non-family-voice reply: {text:?}",
            cmd.keyword,
        );
        return Ok(());
    }

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

/// `wg telegram remind` — the reminder engine's CLI seam.
///
/// Gathers the current plan's reminder rows plus the ad-hoc store, then either
/// lists them (`--list`), shows what would fire at `--now` (`--dry-run`),
/// registers a new ad-hoc reminder (`--add`), or — with no flag — fires the due
/// ones for real, DMing each recipient via their bound bot and recording each in
/// the persistent fired-log first so it fires exactly once.
#[allow(clippy::too_many_arguments)]
pub fn run_remind(
    workgraph_dir: &Path,
    list: bool,
    dry_run: bool,
    add: Option<&str>,
    recipient: Option<&str>,
    now_override: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::reminder::{
        self, AdHocStore, FiredLog, FirePolicy, Reminder,
    };

    let root = project_root(workgraph_dir);
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Known family members (recipients we can name/DM), from the agency bindings.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let members: Vec<String> = bindings
        .bindings
        .iter()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    // --add: register an ad-hoc reminder and print the confirmation.
    if let Some(request) = add {
        let intent = match reminder::parse_reminder_intent(request, now) {
            Some(i) => i,
            None => {
                let msg = "That didn't look like a reminder — try \"remind me Thursday to …\".";
                if json {
                    println!("{}", serde_json::json!({ "registered": false, "reason": msg }));
                } else {
                    println!("{msg}");
                }
                return Ok(());
            }
        };
        // Recipient: explicit, else the first known member, else "you".
        let who = recipient
            .map(|s| s.to_string())
            .or_else(|| members.first().cloned())
            .unwrap_or_else(|| "you".to_string());
        let bot = bindings
            .find_by_name_ci(&who)
            .and_then(|b| b.bot_id.clone())
            .unwrap_or_else(|| "otto".to_string());
        let rem = reminder::intent_to_reminder(&intent, &who, &bot);

        let path = AdHocStore::path(&root);
        let mut store = AdHocStore::load(&path);
        let added = store.add(rem.clone());
        store.save(&path).with_context(|| {
            format!("failed to persist ad-hoc reminder to {}", path.display())
        })?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "registered": added,
                    "confirmation": intent.confirmation,
                    "recipient": who,
                    "due": rem.due.format("%Y-%m-%dT%H:%M").to_string(),
                    "text": rem.text,
                })
            );
        } else {
            println!("{}", intent.confirmation);
        }
        return Ok(());
    }

    // Gather the reminder set: current-week plan rows + ad-hoc store.
    let plans = family_plan::load_plans(&root);
    let current = family_plan::current_plan(&plans, now.date());
    let mut reminders: Vec<Reminder> = current
        .map(|p| reminder::reminders_from_plan(p, &members))
        .unwrap_or_default();
    let store = AdHocStore::load(&AdHocStore::path(&root));
    reminders.extend(store.reminders.iter().cloned());
    reminders.sort_by_key(|r| r.due);

    let log_path = FiredLog::path(&root);
    let log = FiredLog::load(&log_path);
    let policy = FirePolicy::default();

    // --list: every reminder with its state, no firing.
    if list {
        if json {
            let rows: Vec<_> = reminders
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "due": r.due.format("%Y-%m-%dT%H:%M").to_string(),
                        "recipient": r.recipient,
                        "bot": r.bot,
                        "text": r.text,
                        "source": format!("{:?}", r.source),
                        "state": log.outcome(&r.id).map(|o| format!("{o:?}")).unwrap_or_else(|| "pending".to_string()),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if reminders.is_empty() {
            println!("No reminders in the current plan or ad-hoc store.");
        } else {
            println!("Reminders ({} total):", reminders.len());
            for r in &reminders {
                let state = match log.outcome(&r.id) {
                    Some(o) => format!("{o:?}").to_lowercase(),
                    None => "pending".to_string(),
                };
                let who = if r.recipient.is_empty() {
                    "(group)".to_string()
                } else {
                    r.recipient.clone()
                };
                println!(
                    "  [{}] {} → {}  ⏰ {}  ({})",
                    state,
                    r.due.format("%a %m-%d %H:%M"),
                    who,
                    r.text,
                    r.bot,
                );
            }
        }
        return Ok(());
    }

    // Decide firings at `now`. We always tick over a clone; `--dry-run` simply
    // never persists it (nor sends), while the real path saves it BEFORE sending.
    let mut work_log = log.clone();
    let result = reminder::tick(&reminders, &mut work_log, now, &policy);

    if dry_run {
        if json {
            let fired: Vec<_> = result
                .fired
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "recipient": f.reminder.recipient,
                        "bot": f.reminder.bot,
                        "late": f.late,
                        "message": f.message(),
                    })
                })
                .collect();
            let dropped: Vec<_> = result
                .dropped
                .iter()
                .map(|r| serde_json::json!({ "id": r.id, "text": r.text }))
                .collect();
            println!(
                "{}",
                serde_json::json!({ "would_fire": fired, "would_drop": dropped })
            );
        } else if result.fired.is_empty() && result.dropped.is_empty() {
            println!("Nothing due at {}.", now.format("%Y-%m-%d %H:%M"));
        } else {
            for f in &result.fired {
                let who = if f.reminder.recipient.is_empty() {
                    "(group)".to_string()
                } else {
                    f.reminder.recipient.clone()
                };
                println!("WOULD SEND to {} via {}: {}", who, f.reminder.bot, f.message());
            }
            for r in &result.dropped {
                println!("WOULD DROP (too late): ⏰ {}", r.text);
            }
        }
        // Show errands that WOULD nudge too (renders from live shopping state).
        if !json {
            let config = load_telegram_config().unwrap_or_default();
            if let Err(e) = fire_errands(&root, now, current, &members, &bindings, &config, true) {
                eprintln!("errand dry-run skipped: {e:#}");
            }
        }
        return Ok(());
    }

    // Real firing: persist state FIRST (restart-safe exactly-once), then DM.
    work_log
        .save(&log_path)
        .with_context(|| format!("failed to persist reminder state to {}", log_path.display()))?;
    for r in &result.dropped {
        eprintln!(
            "[{}] reminder dropped as stale (>2h late): {}",
            chrono::Utc::now().format("%H:%M:%S"),
            r.text,
        );
    }
    let config = load_telegram_config().unwrap_or_default();
    let mut sent = 0usize;
    if !result.fired.is_empty() {
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        rt.block_on(async {
            for f in &result.fired {
                let (target, bot_id, bot) =
                    match resolve_reminder_target(&config, &bindings, &f.reminder) {
                        Some(t) => t,
                        None => {
                            eprintln!(
                                "[{}] no bound bot/chat for reminder recipient '{}' — skipping DM",
                                chrono::Utc::now().format("%H:%M:%S"),
                                f.reminder.recipient,
                            );
                            continue;
                        }
                    };
                let channel = TelegramChannel::from_bot(bot_id.clone(), bot);
                match channel.send_text(&target, &f.message()).await {
                    Ok(_) => {
                        sent += 1;
                        println!(
                            "[{}] reminded {} via {}: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            f.reminder.recipient,
                            bot_id,
                            f.message(),
                        );
                    }
                    Err(e) => eprintln!(
                        "[{}] failed to DM reminder to {}: {e:#}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        f.reminder.recipient,
                    ),
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;
    }

    // Errands ride the same tick: rendered from live shopping state, paced through
    // the daily-digest layer, and DM'd standalone via the owning bot (e.g. Otto).
    let errand_sent = fire_errands(&root, now, current, &members, &bindings, &config, false)?;

    if result.fired.is_empty() && errand_sent == 0 && !json {
        println!("Nothing due at {}.", now.format("%Y-%m-%d %H:%M"));
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "fired": result.fired.len(), "sent": sent, "errands_sent": errand_sent })
        );
    }
    Ok(())
}

/// Resolve which bot fronts a reminder's recipient and the chat to DM: prefer the
/// recipient's own bound bot + chat, else a bot whose `agent_id` matches the
/// reminder's owning voice, else any configured bot to the recipient's chat.
fn resolve_reminder_target(
    config: &TelegramConfig,
    bindings: &worksgood::agency::TelegramBindingMap,
    rem: &worksgood::notify::reminder::Reminder,
) -> Option<(String, String, TelegramBotConfig)> {
    resolve_dm_target(config, bindings, &rem.recipient, &rem.bot)
}

/// Audit a composed reply for promise-action parity — the `wg telegram parity`
/// seam (see [`crate::cli::TelegramCommands::Parity`]).
///
/// Runs the exact pattern-based classifier the live conversational turn uses and
/// prints: the promise kind (`action` / `preference` / `none`), whether the
/// reply already carries a `TASK_CREATE:` tail, and whether there is a MISMATCH
/// a live turn would repair (a one-off action promised with no artifact). No
/// side effects — nothing is sent, no task is created.
pub fn run_parity(
    reply_text: &str,
    human: Option<&str>,
    _dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::lifecycle;
    use worksgood::notify::parity::{self, PromiseKind};

    // The audit runs over the human-facing reply, with any machine tail stripped
    // — exactly as the live turn sees it.
    let directive = lifecycle::extract_task_directive(reply_text.trim());
    let audit = parity::audit_promise(&directive.reply);
    let has_artifact = directive.title.is_some();
    // A mismatch is the parity gap: a one-off action promised, no artifact.
    let mismatch = audit.commits_action() && !has_artifact;
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
            }))?
        );
        return Ok(());
    }

    println!("promised: {}", audit.kind.slug());
    if let Some(m) = &audit.matched {
        println!("matched:  \"{m}\"");
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
pub fn run_owner(
    ask: &str,
    persona: Option<&str>,
    root: Option<&Path>,
    _dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::ownership::{self, OwnerDecision, OwnerMap};

    let domain = ownership::classify_domain(ask);
    let map = match root {
        Some(r) => OwnerMap::load(r),
        None => OwnerMap::casa_default(),
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
    match &owner {
        Some(o) => println!("owner:   {o}"),
        None => println!("owner:   (unresolved — no persona lists this domain)"),
    }
    match (persona, &decision) {
        (Some(p), Some(OwnerDecision::Owner)) => {
            println!("verdict: {p} OWNS this ask → it creates the task");
        }
        (Some(p), Some(OwnerDecision::Defer { owner })) => {
            println!("verdict: {p} is OFF-DOMAIN → defers to {owner} (re-routed, never its own copy)");
        }
        _ => {}
    }
    Ok(())
}

/// A network-free [`ReplySink`](worksgood::notify::telegram_conversation::ReplySink)
/// that records each send instead of hitting Telegram — the credential-free seam
/// behind `wg telegram lifecycle --mock-send`. It lets the cross-surface smoke
/// drive the REAL lifecycle tick and the REAL casa-feed mirror end-to-end (only
/// the transport is stubbed): every send is recorded and returns a synthetic
/// message id, so `deliver_lifecycle_fire` treats it as a confirmed delivery and
/// mirrors a group report-back into the pane feed exactly as a live send would.
#[derive(Default)]
struct RecordingSink {
    sends: std::sync::Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl worksgood::notify::telegram_conversation::ReplySink for RecordingSink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        let mut sends = self.sends.lock().unwrap();
        let n = sends.len() + 1;
        sends.push((bot_id.to_string(), chat_id.to_string(), text.to_string()));
        Ok(Some(format!("mock-{n}")))
    }
}

/// Deliver ONE lifecycle report-back through the same one-path writer the
/// conversation replies use — the fix for docs/20 ("every origin writes to the
/// ledger; Telegram is a mirror") and the "sends must verify delivery" rule.
///
/// 1. **Send + verify** — `sink.send` resolves the origin persona's bot, calls
///    the Telegram API, and returns `Ok(message_id)` ONLY when the API confirmed
///    `ok:true` (see [`TelegramChannel::api_call`]); any other outcome is `Err`.
///    On a transport/API failure it retries **once** before giving up.
/// 2. **Ledger mirror** — on a confirmed send of a GROUP report-back, it appends
///    an `agent` line to the canonical `.casa/group-feed.jsonl` the constellation
///    pane reads, via the SAME [`casa_feed`] writer the conversation replies use.
///    Gated to group origins (the pane is "our end of the family group chat"); a
///    1:1 DM report-back never leaks into the shared pane. A feed-write failure is
///    logged and swallowed so a full disk can't lose the Telegram delivery.
///
/// Exactly-once is the caller's FiredLog (`notification_id` = the source id,
/// recorded before the send): a delivered `(task, event)` is never re-sent, so
/// the line lands in the pane and in Telegram exactly once. Returns `Ok(())` when
/// delivered, `Err` when BOTH attempts failed — the caller re-arms the FiredLog so
/// a later tick retries rather than the human silently never hearing back.
async fn deliver_lifecycle_fire(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    config: &TelegramConfig,
    feed_path: &Path,
    fire: &worksgood::notify::lifecycle::LifecycleFire,
) -> Result<()> {
    use worksgood::graph::OriginChannel;
    use worksgood::notify::telegram_conversation as convo;

    // Send AS the origin persona's bot (bot_id when known, else the persona id):
    // the reply leaves via the same voice the human addressed, never a wrong face.
    let bot_id = fire
        .origin
        .bot_id
        .clone()
        .unwrap_or_else(|| fire.origin.persona.clone());

    // DELIVERY VERIFICATION with a single retry. `send` bails on a non-`ok`
    // Telegram response, so `Ok` here means the API accepted the message.
    let mut result = sink.send(&bot_id, &fire.origin.chat_id, &fire.text).await;
    if let Err(first) = &result {
        eprintln!(
            "[{}] lifecycle {} for {} send failed (attempt 1/2), retrying: {}",
            chrono::Utc::now().format("%H:%M:%S"),
            fire.event.slug(),
            fire.task_id,
            worksgood::notify::telegram::redact_bot_token(&format!("{first:#}")),
        );
        result = sink.send(&bot_id, &fire.origin.chat_id, &fire.text).await;
    }
    let message_id = result?.unwrap_or_default();

    println!(
        "[{}] lifecycle {} for {} → chat {} via {} (message_id {}): {}",
        chrono::Utc::now().format("%H:%M:%S"),
        fire.event.slug(),
        fire.task_id,
        fire.origin.chat_id,
        bot_id,
        message_id,
        fire.text,
    );

    // LEDGER MIRROR — a group report-back is part of the family group
    // conversation, so it lands in the canonical feed the pane reads, via the
    // exact same `casa_feed` writer the conversation replies use.
    if matches!(fire.origin.channel, OriginChannel::TelegramGroup) {
        let agent_id = convo::agent_for_bot(config, &bot_id);
        let entry = casa_feed::agent_entry(&agent_id, &fire.text, casa_feed::now_ms());
        if let Err(e) = casa_feed::append_entry(feed_path, &entry) {
            eprintln!(
                "[{}] casa feed: failed to mirror lifecycle {} for {}: {e}",
                chrono::Utc::now().format("%H:%M:%S"),
                fire.event.slug(),
                fire.task_id,
            );
        }
    }
    Ok(())
}

/// Report conversational tasks' progress back to the chats they came from — the
/// `wg telegram lifecycle` seam (see [`crate::cli::TelegramCommands::Lifecycle`]).
///
/// Scans origin-stamped tasks (or the single `task_id`), derives each one's
/// start/done/fail event from live status, renders the family-voice line in the
/// composing persona's voice, and fires it exactly once — paced through the
/// daily-digest choke point (time-critical but capped) and delivered to the
/// origin chat via the origin persona's bot. `--dry-run` prints what would be
/// sent where and touches no state; the real path persists the FiredLog +
/// pacing store FIRST (restart-safe), then sends.
pub fn run_lifecycle(
    workgraph_dir: &Path,
    task_id: Option<&str>,
    dry_run: bool,
    now_override: Option<&str>,
    json: bool,
    mock_send: bool,
) -> Result<()> {
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore};
    use worksgood::notify::lifecycle::{self, LifecycleInput};
    use worksgood::notify::reminder::FiredLog;
    use worksgood::notify::telegram_conversation::{BotReplySink, ReplySink};

    let root = project_root(workgraph_dir);
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Live graph: gather origin-stamped tasks that owe a notification. A missing
    // graph is not an error — there is simply nothing to report yet.
    let graph_path = crate::commands::graph_path(workgraph_dir);
    let graph = if graph_path.exists() {
        worksgood::parser::load_graph(&graph_path).context("failed to load the task graph")?
    } else {
        worksgood::WorkGraph::new()
    };
    let mut inputs: Vec<LifecycleInput> = Vec::new();
    for task in graph.tasks() {
        if let Some(want) = task_id {
            if task.id != want {
                continue;
            }
        }
        // Workers doing the work, for the "Nora and Bruno are on it" line: the
        // assignee display name when it reads as a name, else the origin persona.
        let workers = lifecycle_workers(task);
        if let Some(input) = LifecycleInput::from_task(task, workers) {
            inputs.push(input);
        }
    }

    let log_path = FiredLog::path(&root);
    let store_path = DigestStore::path(&root);
    let mut log = FiredLog::load(&log_path);
    let mut store = DigestStore::load(&store_path);
    let policy = DigestPolicy::default();

    if dry_run {
        // Compute against throwaway copies so a dry run records nothing.
        let mut dry_log = log.clone();
        let mut dry_store = store.clone();
        let result =
            lifecycle::lifecycle_tick(&inputs, &mut dry_log, &mut dry_store, now, &policy);
        if json {
            let rows: Vec<_> = result
                .fired
                .iter()
                .chain(result.capped.iter())
                .map(|f| {
                    serde_json::json!({
                        "task": f.task_id,
                        "event": f.event.slug(),
                        "chat": f.origin.chat_id,
                        "persona": f.origin.persona,
                        "bot": f.origin.bot_id,
                        "text": f.text,
                        "capped": result.capped.iter().any(|c| c.task_id == f.task_id && c.event == f.event),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if result.fired.is_empty() && result.capped.is_empty() {
            println!("Nothing to report at {} (family-local; the telegram.log delivery lines are UTC).", now.format("%Y-%m-%d %H:%M"));
        } else {
            for f in &result.fired {
                println!("{}", lifecycle::dry_run_line(f));
            }
            for f in &result.capped {
                println!("[dry-run] (capped → folds into digest) {}", lifecycle::dry_run_line(f));
            }
        }
        return Ok(());
    }

    // Real firing: persist exactly-once + pacing state FIRST, then deliver.
    let result = lifecycle::lifecycle_tick(&inputs, &mut log, &mut store, now, &policy);
    log.save(&log_path)
        .with_context(|| format!("failed to persist lifecycle state to {}", log_path.display()))?;
    store
        .save(&store_path)
        .with_context(|| format!("failed to persist pacing state to {}", store_path.display()))?;

    let config = load_telegram_config().unwrap_or_default();
    let feed_path = casa_feed::feed_path_for(&root);
    // The ONE-PATH writer: lifecycle report-backs leave through the same
    // `ReplySink` the conversation replies use, so a group report-back both
    // reaches Telegram AND lands in the canonical `.casa/group-feed.jsonl` the
    // constellation pane reads — no more sends that bypass the ledger (docs/20).
    // `--mock-send` swaps in a network-free recorder so the cross-surface smoke
    // exercises the real tick + real feed mirror without a live bot.
    let sink: Box<dyn ReplySink> = if mock_send {
        Box::new(RecordingSink::default())
    } else {
        Box::new(BotReplySink::new(config.clone()))
    };
    let mut sent = 0usize;
    let mut undelivered = 0usize;
    if !result.fired.is_empty() {
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        rt.block_on(async {
            for f in &result.fired {
                match deliver_lifecycle_fire(sink.as_ref(), &config, &feed_path, f).await {
                    Ok(()) => sent += 1,
                    Err(e) => {
                        // Both attempts failed — surface it LOUDLY (matching the
                        // web-inbound "make failure visible" rule) so a dropped
                        // report-back can never masquerade as delivered in the log.
                        undelivered += 1;
                        eprintln!(
                            "[{}] UNDELIVERED lifecycle {} for {} after 2 attempts: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            f.event.slug(),
                            f.task_id,
                            worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                        );
                    }
                }
            }
        });
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "fired": result.fired.len(),
                "sent": sent,
                "undelivered": undelivered,
                "capped": result.capped.len(),
            })
        );
    } else if result.fired.is_empty() && result.capped.is_empty() {
        println!("Nothing to report at {} (family-local; the telegram.log delivery lines are UTC).", now.format("%Y-%m-%d %H:%M"));
    }
    Ok(())
}

/// `wg telegram digest` — flush each family member's ONE calm morning digest.
///
/// This is the missing production caller the daily 12:00 UTC `daily-digest` cron
/// runs (task `re-arm-the`). The digest ENGINE (`DigestStore`, one-calm-daily)
/// already accumulates every bundled + overflow proactive item per person, but
/// nothing ever EMITTED the bundled morning message: `emit_digest` had no
/// caller, so even when the cron fired it sent nothing (and the cron itself was
/// registered with an empty description, so a cleanup sweep read it as junk and
/// abandoned it — the friendly fire this task fixes).
///
/// For each known member whose digest is due at `now` — past the digest hour,
/// out of quiet hours, pending non-empty, not already sent today (see
/// [`DigestStore::digest_due`]) — compose the calm `Today: …` line, deliver it
/// through the SAME one-path writer the lifecycle report-backs use
/// ([`deliver_digest_fire`]: send + verify + retry once, then mirror to the
/// canonical `.casa/group-feed.jsonl` ledger the pane reads), and — ONLY on a
/// confirmed delivery — mark the digest sent + clear the queue. A failed send
/// leaves the queue intact so the next tick retries; at most one per person/day.
///
/// `--dry-run` prints what would go to whom and touches no state. `--mock-send`
/// runs the REAL tick + REAL ledger mirror against a network-free recorder so a
/// smoke/test proves the full path (engine → Telegram → ledger) without a bot.
pub fn run_digest(
    workgraph_dir: &Path,
    dry_run: bool,
    now_override: Option<&str>,
    json: bool,
    mock_send: bool,
) -> Result<()> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore, compose_digest};
    use worksgood::notify::telegram_conversation::{BotReplySink, ReplySink};

    let root = project_root(workgraph_dir);
    let now = match now_override {
        Some(s) => parse_naive_now(s)
            .with_context(|| format!("invalid --now '{s}', expected YYYY-MM-DDTHH:MM"))?,
        None => chrono::Local::now().naive_local(),
    };

    // Known family members (recipients we can name/DM), from the agency bindings.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let members: Vec<String> = bindings
        .bindings
        .iter()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    let config = load_telegram_config().unwrap_or_default();
    let policy = DigestPolicy::default();
    let store_path = DigestStore::path(&root);
    let mut store = DigestStore::load(&store_path);

    // Peek (without mutating) each member's due digest so a --dry-run and the
    // real send agree on exactly what would go out.
    let due: Vec<(String, String)> = members
        .iter()
        .filter(|m| store.digest_due(m, now, &policy))
        .filter_map(|m| {
            store
                .state(m)
                .map(|st| (m.clone(), compose_digest(st.pending())))
        })
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();

    if dry_run {
        if json {
            let rows: Vec<_> = due
                .iter()
                .map(|(m, text)| serde_json::json!({ "recipient": m, "text": text }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if due.is_empty() {
            println!(
                "No digest due at {} (nothing pending, already sent, or quiet hours).",
                now.format("%Y-%m-%d %H:%M")
            );
        } else {
            for (m, text) in &due {
                println!("WOULD DIGEST to {}: {}", m, text.replace('\n', " · "));
            }
        }
        return Ok(());
    }

    let feed_path = casa_feed::feed_path_for(&root);
    // `--mock-send` swaps in a network-free recorder so the cross-surface smoke
    // exercises the real tick + real feed mirror without a live bot.
    let sink: Box<dyn ReplySink> = if mock_send {
        Box::new(RecordingSink::default())
    } else {
        Box::new(BotReplySink::new(config.clone()))
    };

    let mut sent = 0usize;
    let mut undelivered = 0usize;
    if !due.is_empty() {
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        rt.block_on(async {
            for (member, text) in &due {
                // Resolve the recipient's DM target; the digest is the household
                // concierge's calm summary, so it fronts as the recipient's own
                // bot when bound, else Otto (see `resolve_dm_target`).
                let (target, bot_id, _bot) =
                    match resolve_dm_target(&config, &bindings, member, "otto") {
                        Some(t) => t,
                        None => {
                            eprintln!(
                                "[{}] no bound bot/chat for digest recipient '{}' — skipping",
                                chrono::Utc::now().format("%H:%M:%S"),
                                member,
                            );
                            continue;
                        }
                    };
                match deliver_digest_fire(
                    sink.as_ref(),
                    &config,
                    &feed_path,
                    &bot_id,
                    &target,
                    text,
                )
                .await
                {
                    Ok(()) => {
                        // Confirmed delivery: NOW mark today's digest sent and
                        // clear the queue (restart-safe — a failed send above
                        // leaves the queue intact for the next tick to retry).
                        store.emit_digest(member, now, &policy);
                        sent += 1;
                        println!(
                            "[{}] digest → {} via {}: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            member,
                            bot_id,
                            text.replace('\n', " · "),
                        );
                    }
                    Err(e) => {
                        undelivered += 1;
                        eprintln!(
                            "[{}] UNDELIVERED digest for {} after 2 attempts: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            member,
                            worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                        );
                    }
                }
            }
        });
    }

    // Persist the pacing store after the tick (digest_sent flags + drained queues
    // for confirmed deliveries; untouched for failed ones).
    store
        .save(&store_path)
        .with_context(|| format!("failed to persist digest state to {}", store_path.display()))?;

    if json {
        println!(
            "{}",
            serde_json::json!({ "due": due.len(), "sent": sent, "undelivered": undelivered })
        );
    } else if due.is_empty() {
        println!(
            "No digest due at {} (nothing pending, already sent, or quiet hours).",
            now.format("%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}

/// Deliver ONE morning digest through the same one-path writer the lifecycle
/// report-backs use (see [`deliver_lifecycle_fire`]): send + verify with a single
/// retry, then mirror the delivered text into the canonical `.casa/group-feed.jsonl`
/// ledger the constellation pane reads, via the SAME [`casa_feed`] writer.
///
/// The digest is the household's calm morning summary of family-group activity
/// (bundled report-backs, reminders, errands), so — unlike a private 1:1 reply —
/// it belongs in the shared pane ledger: "arrives in Telegram AND the ledger"
/// (task `re-arm-the`, sequenced with `lifecycle-messages-obey`). A feed-write
/// failure is logged and swallowed so a full disk can't lose the Telegram send.
/// Returns `Ok(())` on confirmed delivery, `Err` when BOTH send attempts failed
/// (the caller then leaves the pending queue intact for the next tick).
async fn deliver_digest_fire(
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
    config: &TelegramConfig,
    feed_path: &Path,
    bot_id: &str,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;

    // DELIVERY VERIFICATION with a single retry (matches the lifecycle path).
    let mut result = sink.send(bot_id, chat_id, text).await;
    if let Err(first) = &result {
        eprintln!(
            "[{}] digest send to {} failed (attempt 1/2), retrying: {}",
            chrono::Utc::now().format("%H:%M:%S"),
            chat_id,
            worksgood::notify::telegram::redact_bot_token(&format!("{first:#}")),
        );
        result = sink.send(bot_id, chat_id, text).await;
    }
    let _message_id = result?.unwrap_or_default();

    // LEDGER MIRROR — the morning digest lands in the canonical feed the pane
    // reads, via the exact same `casa_feed` writer the conversation replies use.
    let agent_id = convo::agent_for_bot(config, bot_id);
    let entry = casa_feed::agent_entry(&agent_id, text, casa_feed::now_ms());
    if let Err(e) = casa_feed::append_entry(feed_path, &entry) {
        eprintln!(
            "[{}] casa feed: failed to mirror digest to ledger: {e}",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }
    Ok(())
}

/// The persona name(s) doing a task's work, for the "on it" line: the task's
/// assignee display name when it reads like a plain roster name (not an agent
/// content-hash), else the origin persona so the line still names a voice.
fn lifecycle_workers(task: &worksgood::graph::Task) -> Vec<String> {
    // The SAME family-voice gate the composer enforces (morning-taco-bugs): a
    // raw worker id ("agent-2972"), task id, or content hash is not a speakable
    // name, so the "on it" line falls back to the owning persona instead of
    // leaking "Agent-2972 is on it 🍳" into the family group.
    if let Some(a) = task
        .assigned
        .as_deref()
        .filter(|a| worksgood::notify::lifecycle::is_family_safe_name(a))
    {
        return vec![a.to_string()];
    }
    Vec::new()
}

/// Resolve the DM target (chat + bot) for a proactive nudge to `recipient`, sent
/// in the voice `bot`: prefer the recipient's own bound bot + chat, else a bot
/// whose `agent_id` matches the owning voice, else any configured bot. Shared by
/// the reminder tick and the errand tick so both DMs leave via the same rule.
fn resolve_dm_target(
    config: &TelegramConfig,
    bindings: &worksgood::agency::TelegramBindingMap,
    recipient: &str,
    bot: &str,
) -> Option<(String, String, TelegramBotConfig)> {
    let binding = bindings.find_by_name_ci(recipient)?;
    let target = binding.telegram_user.clone();
    let bots = config.all_bots();
    // 1) the recipient's configured bot.
    if let Some(bid) = &binding.bot_id {
        if let Some((id, b)) = bots.iter().find(|(id, _)| id == bid) {
            return Some((target, id.clone(), b.clone()));
        }
    }
    // 2) a bot fronting the nudge's owning voice.
    if let Some((id, b)) = bots
        .iter()
        .find(|(id, b)| id == &bot || b.agent_id.as_deref() == Some(bot))
    {
        return Some((target, id.clone(), b.clone()));
    }
    // 3) any bot.
    bots.first().map(|(id, b)| (target, id.clone(), b.clone()))
}

/// Fire due errand nudges as part of the `wg telegram remind` tick.
///
/// Builds errands from the current-week plan, ticks them (exactly-once + drop-if-
/// stale via a dedicated `.casa/errand-fired.json` log), renders each from **live**
/// shopping state fetched *now* (`GET /shopping.json`), routes every firing through
/// the daily-digest pacing layer — time-critical, so under the daily standalone cap
/// it DMs the runner standalone (via the errand's owning bot, e.g. Otto), and over
/// the cap it folds into the morning digest — and returns how many were DM'd.
///
/// A live-shopping fetch failure **skips the whole tick without recording anything**
/// (`Ok(0)`), so the one nudge is never burned on a stale or empty render.
#[allow(clippy::too_many_arguments)]
fn fire_errands(
    root: &Path,
    now: chrono::NaiveDateTime,
    current: Option<&worksgood::notify::family_plan::PlanDoc>,
    members: &[String],
    bindings: &worksgood::agency::TelegramBindingMap,
    config: &TelegramConfig,
    dry_run: bool,
) -> Result<usize> {
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore, Offer};
    use worksgood::notify::errand::{self, ShoppingModel};
    use worksgood::notify::reminder::{FiredLog, FirePolicy};

    let plan = match current {
        Some(p) => p,
        None => return Ok(0),
    };
    let errands = errand::errands_from_plan(plan, members, errand::resolve_lead());
    if errands.is_empty() {
        return Ok(0);
    }

    // Which errands are due now? Own FiredLog namespace so it never collides with
    // the reminder log (ids are `errand:…` vs `⏰` reminder ids anyway).
    let log_path = root.join(".casa").join("errand-fired.json");
    let mut work_log = FiredLog::load(&log_path);
    let firings = errand::errand_tick(&errands, &mut work_log, now, &FirePolicy::default());
    if firings.is_empty() {
        return Ok(0);
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;

    // Live shopping state, fetched AT FIRE TIME. A failure skips the tick (no nudge
    // burned) — the body must never render from stale/empty state.
    let base = worksgood::notify::telegram_photo::gateway_base_url();
    let url = format!("{base}/shopping.json?back=0");
    let shopping: ShoppingModel = match rt.block_on(async {
        let body = reqwest::get(&url).await?.text().await?;
        Ok::<_, anyhow::Error>(ShoppingModel::from_json(&body))
    }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "[{}] errand tick: live shopping fetch failed ({e:#}) — skipping, no nudge burned",
                chrono::Utc::now().format("%H:%M:%S"),
            );
            return Ok(0);
        }
    };

    if dry_run {
        for f in &firings {
            let body = errand::route_errand_nudge(f, &shopping, &mut DigestStore::default(), now, &DigestPolicy::new()).0;
            let who = if f.errand.recipient.is_empty() {
                "(group)".to_string()
            } else {
                f.errand.recipient.clone()
            };
            println!(
                "WOULD ERRAND-NUDGE {} via {}: {}",
                who,
                f.errand.bot,
                body.replace('\n', " · "),
            );
        }
        return Ok(0);
    }

    // Real firing: persist the fired-log FIRST (restart-safe exactly-once), then pace + send.
    work_log
        .save(&log_path)
        .with_context(|| format!("failed to persist errand state to {}", log_path.display()))?;

    let digest_path = DigestStore::path(root);
    let mut digest = DigestStore::load(&digest_path);
    let policy = DigestPolicy::new();

    let mut sent = 0usize;
    rt.block_on(async {
        for f in &firings {
            let (_, offer) = errand::route_errand_nudge(f, &shopping, &mut digest, now, &policy);
            match offer {
                Offer::SendNow(text) => {
                    let (target, bot_id, bot) =
                        match resolve_dm_target(config, bindings, &f.errand.recipient, &f.errand.bot) {
                            Some(t) => t,
                            None => {
                                eprintln!(
                                    "[{}] no bound bot/chat for errand recipient '{}' — skipping DM",
                                    chrono::Utc::now().format("%H:%M:%S"),
                                    f.errand.recipient,
                                );
                                continue;
                            }
                        };
                    let channel = TelegramChannel::from_bot(bot_id.clone(), bot);
                    match channel.send_text(&target, &text).await {
                        Ok(_) => {
                            sent += 1;
                            println!(
                                "[{}] errand-nudged {} via {} ({} still needed)",
                                chrono::Utc::now().format("%H:%M:%S"),
                                f.errand.recipient,
                                bot_id,
                                shopping.remaining_count(),
                            );
                        }
                        Err(e) => eprintln!(
                            "[{}] failed to DM errand to {}: {e:#}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            f.errand.recipient,
                        ),
                    }
                }
                Offer::Queued { overflow } => println!(
                    "[{}] errand for {} folded into the morning digest (overflow={overflow})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    f.errand.recipient,
                ),
                Offer::Pending | Offer::Duplicate => {}
            }
        }
    });

    // Persist the pacing state (standalone counter + any queued overflow) after the tick.
    if let Err(e) = digest.save(&digest_path) {
        eprintln!(
            "[{}] warning: failed to persist digest pacing state: {e:#}",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }
    Ok(sent)
}

/// Parse a `YYYY-MM-DDTHH:MM` (or space-separated) local wall-clock instant.
fn parse_naive_now(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

/// `wg telegram conversation` — dry-run the conversational composer for a plain
/// message (the 1:1 and group-name-addressed path), without a live bot.
///
/// Prints the route decision (which bot answers, in which chat, how it was
/// addressed, and the plan kind) and the outbound replies the listener WOULD
/// send — captured by a recording sink, never sent, so it is credential-free.
///
/// With `--session-reply <text>` it exercises the legacy persistent-session
/// round-trip: an ephemeral session is created and bound to the addressed
/// agent (making the plan `converse`), the human's message is written to that
/// session's inbox, a fixture responder writes `<text>` to the outbox, and the
/// relayed reply is captured.
///
/// With `--compose` it exercises the REAL fix end-to-end: the converse turn is
/// driven by the production [`OneshotComposer`] (a live one-shot `claude`
/// spawn), so the captured reply is an actual session-generated answer — no
/// fixture, no mock. This is the credential-bearing "real turn" validation.
///
/// With `--compose-error` a deliberately-failing composer is injected so the
/// fail-fast + graceful "glitched" follow-up path is provable through the built
/// binary without a live model (the induced-failure test).
#[allow(clippy::too_many_arguments)]
/// Drive the voice-note path end-to-end from a recording FILE: detect →
/// transcribe → inject (task `telegram-voice-notes`). The credential-free
/// scripted-test seam behind `wg telegram voice --file`.
///
/// `detect`: read the file and build the same [`telegram_voice::VoiceMeta`] the
/// listener parses from a real update. `transcribe`: POST the bytes to a
/// gateway — a STUB (when `stub_ok`/`stub_reason` is set) so the whole path runs
/// with NO live whisper, else the real `/conversation/transcribe`. `inject`: on
/// a transcript, print it (the body that would be injected) AND how the SAME
/// fast-lane classifier a typed line hits would route it — proving a spoken line
/// == a typed line. On failure, print the honest in-persona line the listener
/// would send.
pub fn run_voice_dryrun(
    file: &Path,
    mime: &str,
    lang: &str,
    gateway: Option<&str>,
    stub_ok: Option<&str>,
    stub_reason: Option<&str>,
    json: bool,
) -> Result<()> {
    use async_trait::async_trait;
    use worksgood::notify::fast_lane;
    use worksgood::notify::telegram_voice as tv;

    // ── detect ────────────────────────────────────────────────────────────
    let bytes = std::fs::read(file)
        .with_context(|| format!("failed to read recording file {}", file.display()))?;
    let meta = tv::VoiceMeta {
        file_id: format!("local:{}", file.display()),
        mime_type: Some(mime.to_string()),
        kind: tv::VoiceKind::Voice,
        file_size: Some(bytes.len() as u64),
    };

    // A downloader that just yields the already-read local bytes — the file IS
    // the "download". The real listener path uses the Telegram getFile impl.
    struct LocalBytes(Vec<u8>);
    #[async_trait]
    impl tv::VoiceDownloader for LocalBytes {
        async fn download_bytes(&self, _file_id: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    // A stub gateway returning a canned response, so the full detect→transcribe
    // →inject path is provable with no live whisper engine.
    struct StubGateway(serde_json::Value);
    #[async_trait]
    impl tv::TranscribeGateway for StubGateway {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _mime_type: &str,
            _lang: &str,
        ) -> Result<serde_json::Value> {
            Ok(self.0.clone())
        }
    }

    let downloader = LocalBytes(bytes.clone());
    let limits = tv::VoiceLimits::default();

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let result: tv::TranscribeResult = rt.block_on(async {
        if let Some(text) = stub_ok {
            let gw = StubGateway(serde_json::json!({ "ok": true, "text": text }));
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        } else if let Some(reason) = stub_reason {
            let gw = StubGateway(serde_json::json!({ "ok": false, "reason": reason }));
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        } else {
            let base = gateway
                .map(|g| g.to_string())
                .unwrap_or_else(tv::gateway_base_url);
            let gw = tv::HttpTranscribeGateway::new(base);
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        }
    })?;

    // ── inject ────────────────────────────────────────────────────────────
    match result {
        tv::TranscribeResult::Transcript(text) => {
            // Route the transcript through the SAME classifier a typed line hits.
            let today = chrono::Local::now().date_naive();
            let classification = fast_lane::classify(&text, today);
            let route = match &classification {
                fast_lane::Classification::FastLane(op) => {
                    format!("fast-lane:{}", op.kind_label())
                }
                fast_lane::Classification::Fallback(_) => "composer".to_string(),
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "outcome": "transcript",
                        "bytes": bytes.len(),
                        "mime": mime,
                        "transcript": text,
                        "injected_body": text,
                        "route": route,
                    })
                );
            } else {
                println!("detect: {} bytes, mime {}", bytes.len(), mime);
                println!("transcribe: ok");
                println!("inject: message body = {text:?}");
                println!("route (same path as typed): {route}");
            }
        }
        tv::TranscribeResult::Failed(failure) => {
            let reason = match failure {
                tv::TranscribeFailure::Unconfigured => "unconfigured",
                tv::TranscribeFailure::Silence => "silence",
                tv::TranscribeFailure::Unclear => "unclear",
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "outcome": "failed",
                        "reason": reason,
                        "reply": failure.message(),
                    })
                );
            } else {
                println!("detect: {} bytes, mime {}", bytes.len(), mime);
                println!("transcribe: failed ({reason})");
                println!("reply (in-persona): {}", failure.message());
            }
        }
    }
    Ok(())
}

pub fn run_conversation_dryrun(
    workgraph_dir: &Path,
    channel: &str,
    chat: &str,
    sender: &str,
    message: &str,
    group: bool,
    session_reply: Option<&str>,
    compose: bool,
    compose_error: bool,
    json: bool,
) -> Result<()> {
    use std::sync::{Arc, Mutex};
    use worksgood::notify::telegram_conversation as convo;

    let config = load_telegram_config().unwrap_or_default();
    let entry = if group {
        convo::Entry::GroupElected
    } else {
        convo::Entry::Direct
    };

    // Bind an ephemeral session to the addressed agent so the plan resolves to
    // `converse` and the turn has somewhere to land — needed for the fixture
    // round-trip AND both compose modes.
    if session_reply.is_some() || compose || compose_error {
        if let Some(agent_id) = convo::agent_for_channel(&config, channel) {
            let uuid = worksgood::chat_sessions::create_session(
                workgraph_dir,
                worksgood::chat_sessions::SessionKind::Interactive,
                &[],
                None,
            )?;
            worksgood::chat_sessions::bind_agent(workgraph_dir, &agent_id, &uuid)?;
        }
    }

    let plan = convo::plan_conversation(workgraph_dir, &config, channel, chat, sender, entry);

    // Recording sink: capture every send AND edit instead of hitting the
    // network. Sends return a monotonic fake message id so the ack-edit path
    // works; edits are recorded so the printed output shows the final text.
    #[derive(Clone, Default)]
    struct DryRunSink {
        sent: Arc<Mutex<Vec<(String, String, String)>>>,
        edited: Arc<Mutex<Vec<(String, String, String, String)>>>,
        next_id: Arc<Mutex<u64>>,
    }
    #[async_trait::async_trait]
    impl convo::ReplySink for DryRunSink {
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
    let sink = DryRunSink::default();

    // Injected failing composer for `--compose-error`.
    struct FailingComposer;
    #[async_trait::async_trait]
    impl convo::ReplyComposer for FailingComposer {
        async fn compose(
            &self,
            _wg: &Path,
            _s: &str,
            _a: &str,
            _m: &str,
        ) -> Result<String> {
            anyhow::bail!("induced compose failure (--compose-error)")
        }
    }

    // Build the composer for whichever mode is active.
    let real_composer = if compose {
        Some(convo::OneshotComposer::from_config(
            worksgood::config::Config::load_merged(workgraph_dir)
                .context("--compose needs a loadable wg config")?,
        ))
    } else {
        None
    };
    let failing_composer = FailingComposer;
    let composer_ref: Option<&dyn convo::ReplyComposer> = if compose_error {
        Some(&failing_composer)
    } else {
        real_composer.as_ref().map(|c| c as &dyn convo::ReplyComposer)
    };

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let outcome = rt.block_on(async {
        // Legacy fixture responder: only when NOT using a composer — echo the
        // canned reply to the outbox as a live session would.
        if composer_ref.is_none() {
            if let (Some(reply), convo::ConversationPlan::Converse { session_ref, .. }) =
                (session_reply, &plan)
            {
                let dir = workgraph_dir.to_path_buf();
                let session_ref = session_ref.clone();
                let reply = reply.to_string();
                tokio::spawn(async move {
                    for _ in 0..200 {
                        let inbox = worksgood::chat::read_inbox_ref(&dir, &session_ref)
                            .unwrap_or_default();
                        if let Some(m) = inbox.iter().find(|m| m.role == "user") {
                            let _ = worksgood::chat::append_outbox_ref(
                                &dir,
                                &session_ref,
                                &reply,
                                &m.request_id,
                            );
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                });
            }
        }
        // Timing: in compose modes keep the ack point far out so a normal reply
        // (or a fast induced failure) lands as a single direct send that the
        // test can capture; the reply timeout bounds a genuinely hung child. In
        // the legacy fixture mode, fast timing so the dry-run doesn't stall.
        let timing = if composer_ref.is_some() {
            convo::AckTiming {
                ack_after: std::time::Duration::from_secs(60),
                reply_timeout: std::time::Duration::from_secs(120),
                poll: std::time::Duration::from_millis(50),
            }
        } else {
            convo::AckTiming {
                ack_after: std::time::Duration::from_millis(50),
                reply_timeout: std::time::Duration::from_secs(5),
                poll: std::time::Duration::from_millis(15),
            }
        };
        convo::run_conversation_turn(
            workgraph_dir,
            &plan,
            message,
            &format!("dryrun-{sender}"),
            timing,
            composer_ref,
            &sink,
        )
        .await
    })?;

    // Fold edits into the send list for output so the final text (when the ack
    // was edited in place) is always visible.
    let mut sends = sink.sent.lock().unwrap().clone();
    for (bot, chat, _mid, text) in sink.edited.lock().unwrap().iter() {
        sends.push((bot.clone(), chat.clone(), text.clone()));
    }
    if json {
        let sends_json: Vec<_> = sends
            .iter()
            .map(|(bot, chat, text)| {
                serde_json::json!({ "bot": bot, "chat": chat, "text": text })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "entry": entry.label(),
                "kind": plan.kind_label(),
                "route": { "bot": plan.route().bot_id, "chat": plan.route().chat_id },
                "outcome": outcome.label(),
                "sends": sends_json,
            }))?
        );
    } else {
        println!(
            "route: {} via {} in {} [{}] — {}",
            entry.label(),
            plan.route().bot_id,
            plan.route().chat_id,
            plan.kind_label(),
            outcome.label(),
        );
        for (bot, chat, text) in &sends {
            println!("  send[{bot} -> {chat}]: {text}");
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
                let identity = worksgood::notify::telegram_sender::extract_sender(update);
                let sender = identity.display();

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
                    sender,
                    sender_id: identity.user_id,
                    sender_is_bot: identity.is_bot,
                    sent_at: cb
                        .get("message")
                        .and_then(|m| m.get("date"))
                        .and_then(|d| d.as_i64()),
                    body: action_id.clone(),
                    action_id: Some(action_id),
                    reply_to,
                    message_id: None,
                    chat_id,
                    chat_type: None,
                    mention_usernames: Vec::new(),
                    reply_to_bot: None,
                    has_bot_command: false,
                    photo_file_id: None,
                    media_group_id: None,
                    voice_file_id: None,
                    voice_mime: None,
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
                let identity =
                    worksgood::notify::telegram_sender::identity_from_message(message);
                let sender = identity.display();

                let body = message
                    .get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| message.get("caption").and_then(|c| c.as_str()))
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
                let has_bot_command = worksgood::notify::telegram_group::has_leading_bot_command(
                    message.get("entities").unwrap_or(&serde_json::Value::Null),
                );

                let msg = worksgood::notify::IncomingMessage {
                    channel: "telegram".to_string(),
                    sender,
                    sender_id: identity.user_id,
                    sender_is_bot: identity.is_bot,
                    sent_at: message.get("date").and_then(|d| d.as_i64()),
                    body,
                    action_id: None,
                    reply_to,
                    message_id,
                    chat_id: chat_id.clone(),
                    chat_type,
                    mention_usernames,
                    reply_to_bot,
                    has_bot_command,
                    photo_file_id: worksgood::notify::telegram_photo::largest_photo_file_id(
                        message,
                    ),
                    media_group_id: message
                        .get("media_group_id")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string()),
                    voice_file_id: worksgood::notify::telegram_voice::voice_meta(message)
                        .as_ref()
                        .map(|v| v.file_id.clone()),
                    voice_mime: worksgood::notify::telegram_voice::voice_meta(message)
                        .and_then(|v| v.mime_type),
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

    // --- command_gate (fix-command-leaks) ---------------------------------

    fn gate_msg(chat_type: &str, has_bot_command: bool) -> worksgood::notify::IncomingMessage {
        worksgood::notify::IncomingMessage {
            channel: "telegram".to_string(),
            sender: "luca".to_string(),
            sender_id: Some("8905220378".to_string()),
            sender_is_bot: false,
            sent_at: None,
            body: "?".to_string(),
            action_id: None,
            reply_to: None,
            message_id: Some("1".to_string()),
            chat_id: Some("-100".to_string()),
            chat_type: Some(chat_type.to_string()),
            mention_usernames: Vec::new(),
            reply_to_bot: None,
            has_bot_command,
            photo_file_id: None,
            media_group_id: None,
            voice_file_id: None,
            voice_mime: None,
        }
    }

    #[test]
    fn command_gate_blocks_non_slash_everywhere() {
        // Punctuation / chatter (no bot_command) is never a command — the leak.
        let g = command_gate(&gate_msg("supergroup", false));
        assert!(!g.family && !g.operator, "no slash entity → no command");
        let g = command_gate(&gate_msg("private", false));
        assert!(!g.family && !g.operator, "no slash entity → no command in DM either");
    }

    #[test]
    fn command_gate_operator_reference_never_in_a_group() {
        // Even a genuine slash command in a family GROUP must NOT open the
        // operator claim/done path — that content is coordinator-only.
        let g = command_gate(&gate_msg("supergroup", true));
        assert!(g.family, "a real /command still runs the family set in a group");
        assert!(!g.operator, "the operator WG reference must never surface in a group");
    }

    #[test]
    fn command_gate_operator_only_in_private_slash() {
        // A 1:1 operator DM with a real slash command is the only place the
        // operator reference may run.
        let g = command_gate(&gate_msg("private", true));
        assert!(g.operator, "operator reference is allowed in a 1:1 slash command");
    }

    #[test]
    fn parse_login_nonce_extracts_deep_link_payload() {
        // Plain deep link.
        assert_eq!(parse_login_nonce("/start login_abc123"), Some("abc123"));
        // @bot-qualified deep link (Telegram sends this in some clients).
        assert_eq!(
            parse_login_nonce("/start@otto_casapinello_bot login_deadbeef"),
            Some("deadbeef")
        );
        // Leading/trailing whitespace tolerated.
        assert_eq!(parse_login_nonce("  /start   login_xyz  "), Some("xyz"));
    }

    #[test]
    fn parse_login_nonce_rejects_non_login_start() {
        // Bare /start (onboarding) is not a sign-in.
        assert_eq!(parse_login_nonce("/start"), None);
        // A different deep-link payload.
        assert_eq!(parse_login_nonce("/start invite_abc"), None);
        // Empty nonce.
        assert_eq!(parse_login_nonce("/start login_"), None);
        // "/started" must not be mistaken for "/start".
        assert_eq!(parse_login_nonce("/started login_abc"), None);
        // "/start@bot" with no payload.
        assert_eq!(parse_login_nonce("/start@otto_casapinello_bot"), None);
        // Not the start command at all.
        assert_eq!(parse_login_nonce("/dinner login_abc"), None);
        assert_eq!(parse_login_nonce("hello there"), None);
    }

    #[test]
    fn confirm_resp_deserializes_gateway_shapes() {
        let ok: ConfirmResp = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(ok.ok);
        let unknown: ConfirmResp =
            serde_json::from_str(r#"{"ok":false,"reason":"unknown-user"}"#).unwrap();
        assert!(!unknown.ok);
        assert_eq!(unknown.reason.as_deref(), Some("unknown-user"));
        // Tolerates missing fields (defaults to ok:false, reason:None).
        let empty: ConfirmResp = serde_json::from_str("{}").unwrap();
        assert!(!empty.ok);
        assert!(empty.reason.is_none());
    }

    #[test]
    fn auth_confirm_url_defaults_to_loopback() {
        // Default (no override) is the loopback gateway. We do not mutate the
        // process env here (tests run concurrently); just assert the constant.
        assert_eq!(AUTH_CONFIRM_URL_DEFAULT, "http://127.0.0.1:7788/auth/confirm");
    }

    /// Spin a one-shot loopback HTTP stub standing in for the gateway
    /// `POST /auth/confirm`. It captures the request body (so the test can assert
    /// exactly `{nonce, telegram_id}` crossed the wire) and answers with
    /// `response_json`. Drives the REAL `confirm_web_login` POST path end to end.
    fn spawn_confirm_stub(response_json: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{}/auth/confirm", port);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 512];
                // Read until the header terminator, then drain the declared body.
                let header_end = loop {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) => break buf.len(),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break buf.len(),
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body = buf[header_end..].to_vec();
                while body.len() < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&body).to_string());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, rx)
    }

    /// Like [`spawn_confirm_stub`] but sends the FULL raw request (headers + body)
    /// over the channel, so a test can assert on the request HEADERS — used to
    /// prove the listener attaches (or omits) the `x-casa-auth-secret` header
    /// (task urgent-auth-phantom).
    fn spawn_confirm_stub_raw(response_json: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{}/auth/confirm", port);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 512];
                let header_end = loop {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) => break buf.len(),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break buf.len(),
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body = buf[header_end..].to_vec();
                while body.len() < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                let mut raw = headers;
                raw.push_str(&String::from_utf8_lossy(&body));
                let _ = tx.send(raw);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        (url, rx)
    }

    /// SECURITY (task urgent-auth-phantom): when a confirm secret is configured,
    /// the listener MUST attach it as the `x-casa-auth-secret` header so the
    /// gateway's shared-secret gate accepts the write. Without this, a loopback
    /// browser page could self-confirm nonces and mint phantom devices.
    #[test]
    #[serial_test::serial]
    fn confirm_write_attaches_secret_header_when_configured() {
        let (url, rx) = spawn_confirm_stub_raw(r#"{"ok":true}"#);
        unsafe {
            std::env::set_var("CASA_AUTH_CONFIRM_URL", &url);
            std::env::set_var("CASA_AUTH_CONFIRM_SECRET", "top-secret-token-abc");
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let _ = rt.block_on(confirm_web_login(&client, "nonce", "123456789"));
        unsafe {
            std::env::remove_var("CASA_AUTH_CONFIRM_URL");
            std::env::remove_var("CASA_AUTH_CONFIRM_SECRET");
        }

        let raw = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let lower = raw.to_ascii_lowercase();
        assert!(
            lower.contains("x-casa-auth-secret: top-secret-token-abc"),
            "confirm request must carry the secret header; got:\n{raw}"
        );
    }

    /// BACKWARD COMPAT: with NO secret configured (no env, no file) the listener
    /// omits the header entirely, so a gateway that predates the secret gate — and
    /// the existing loopback-only path — keeps working unchanged.
    #[test]
    #[serial_test::serial]
    fn confirm_write_omits_secret_header_when_unconfigured() {
        let (url, rx) = spawn_confirm_stub_raw(r#"{"ok":true}"#);
        unsafe {
            std::env::set_var("CASA_AUTH_CONFIRM_URL", &url);
            std::env::remove_var("CASA_AUTH_CONFIRM_SECRET");
            // Point the file lookup at a path that cannot exist so the default
            // `.casa/auth-confirm.secret` (which may exist in a live CWD) is skipped.
            std::env::set_var("CASA_AUTH_CONFIRM_SECRET_FILE", "/nonexistent/casa/auth-confirm.secret");
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let _ = rt.block_on(confirm_web_login(&client, "nonce", "123456789"));
        unsafe {
            std::env::remove_var("CASA_AUTH_CONFIRM_URL");
            std::env::remove_var("CASA_AUTH_CONFIRM_SECRET_FILE");
        }

        let raw = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(
            !raw.to_ascii_lowercase().contains("x-casa-auth-secret"),
            "no secret configured → no header; got:\n{raw}"
        );
    }

    /// A bound household member's `/start login_<nonce>` POSTs exactly
    /// `{nonce, telegram_id}` to the gateway and gets the signed-in family reply.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_posts_nonce_and_signs_in_bound_user() {
        let (url, rx) = spawn_confirm_stub(r#"{"ok":true}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "s3cr3t-nonce", "123456789"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        let body = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["nonce"], "s3cr3t-nonce");
        assert_eq!(parsed["telegram_id"], "123456789");
        assert!(reply.starts_with("You're signed in"), "reply: {reply}");
    }

    /// An unknown telegram id → the friendly "ask Otto" reply, no session.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_unknown_user_gets_ask_otto_reply() {
        let (url, _rx) = spawn_confirm_stub(r#"{"ok":false,"reason":"unknown-user"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "999"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert!(reply.contains("ask Otto"), "reply: {reply}");
        assert!(!reply.starts_with("You're signed in"));
    }

    /// An UNKNOWN/expired nonce (slow new-device round-trip blew past the pending
    /// window) → the actionable "reopen the Casa page" reply, matching the gateway
    /// contract (`SIGN_IN_LINK_EXPIRED_REPLY` in auth.mjs). Never tells a
    /// personal-device signer to "tap the tablet".
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_unknown_nonce_gets_reopen_reply() {
        let (url, _rx) = spawn_confirm_stub(r#"{"ok":false,"reason":"unknown-nonce"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "999"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert!(reply.contains("reopen the Casa page"), "reply: {reply}");
        assert!(!reply.contains("tap the tablet"), "reply: {reply}");
    }

    /// Gateway unreachable → the expired/try-again reply, never a false sign-in.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_gateway_down_expired_reply() {
        // Port 1 has no listener → connection refused.
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", "http://127.0.0.1:1/auth/confirm") };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "1"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert!(reply.contains("expired"), "reply: {reply}");
        assert!(!reply.starts_with("You're signed in"));
    }

    // ── onboarding-bootstrap: invite (join_) + founding gate ─────────────────

    #[test]
    fn parse_join_nonce_extracts_and_rejects() {
        // Plain + @bot-qualified + whitespace-tolerant, mirroring parse_login_nonce.
        assert_eq!(parse_join_nonce("/start join_abc123"), Some("abc123"));
        assert_eq!(
            parse_join_nonce("/start@otto_casapinello_bot join_deadbeef"),
            Some("deadbeef")
        );
        assert_eq!(parse_join_nonce("  /start   join_xyz  "), Some("xyz"));
        // A LOGIN payload is not a JOIN payload (the two families never collide).
        assert_eq!(parse_join_nonce("/start login_abc"), None);
        assert_eq!(parse_join_nonce("/start"), None);
        assert_eq!(parse_join_nonce("/start join_"), None);
        assert_eq!(parse_join_nonce("/started join_abc"), None);
        assert_eq!(parse_join_nonce("hello"), None);
        // And parse_login_nonce rejects a join payload (symmetry).
        assert_eq!(parse_login_nonce("/start join_abc"), None);
    }

    #[test]
    fn is_negative_matches_only_no_variants() {
        assert!(is_negative("no"));
        assert!(is_negative("NO"));
        assert!(is_negative("  No  "));
        assert!(is_negative("n"));
        assert!(!is_negative("yes"));
        assert!(!is_negative("nope, not me"));
        assert!(!is_negative(""));
    }

    #[test]
    fn founding_display_name_strips_handle_and_falls_back_for_numeric() {
        assert_eq!(founding_display_name("@luca_pinello"), "luca_pinello");
        assert_eq!(founding_display_name("Nadin"), "Nadin");
        // A bare numeric id (no public @username) → the editable "Owner" default.
        assert_eq!(founding_display_name("8905220378"), "Owner");
        assert_eq!(founding_display_name(""), "Owner");
        assert_eq!(founding_display_name("  @erik "), "erik");
    }

    #[test]
    fn redeem_resp_deserializes_gateway_shapes() {
        let ok: RedeemResp = serde_json::from_str(r#"{"ok":true,"name":"Erik"}"#).unwrap();
        assert!(ok.ok);
        assert_eq!(ok.name.as_deref(), Some("Erik"));
        let used: RedeemResp = serde_json::from_str(r#"{"ok":false,"reason":"used"}"#).unwrap();
        assert!(!used.ok);
        assert_eq!(used.reason.as_deref(), Some("used"));
    }

    /// EMPTY roster: the confirm outcome is `EmptyRoster` so the handler offers
    /// founding instead of rejecting the very first scan.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_outcome_detects_empty_roster() {
        let (url, rx) = spawn_confirm_stub(r#"{"ok":false,"reason":"empty-roster"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let outcome = rt.block_on(confirm_web_login_outcome(&client, "nonce", "55501234"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        let body = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["telegram_id"], "55501234");
        assert_eq!(outcome, WebLoginOutcome::EmptyRoster);
    }

    /// A tapped invite (`/start join_<nonce>`) POSTs `{nonce, telegram_id}` and,
    /// on success, welcomes the joined person by the invite's name.
    #[test]
    #[serial_test::serial]
    fn redeem_invite_posts_and_welcomes_by_name() {
        let (url, rx) = spawn_confirm_stub(r#"{"ok":true,"name":"Erik"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(redeem_invite(&client, "inv-nonce", "77712345"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        let body = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["nonce"], "inv-nonce");
        assert_eq!(parsed["telegram_id"], "77712345");
        assert!(reply.starts_with("Welcome"), "reply: {reply}");
        assert!(reply.contains("Erik"), "reply: {reply}");
    }

    /// A replayed / dead invite gets the friendly "ask for a fresh one" reply.
    #[test]
    #[serial_test::serial]
    fn redeem_invite_used_link_is_rejected_kindly() {
        let (url, _rx) = spawn_confirm_stub(r#"{"ok":false,"reason":"used"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(redeem_invite(&client, "inv", "1"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert!(reply.contains("already used"), "reply: {reply}");
        assert!(!reply.starts_with("Welcome"));
    }

    /// Founding (`/auth/found`) POSTs `{nonce, telegram_id, name}` and welcomes
    /// the first member as the household owner.
    #[test]
    #[serial_test::serial]
    fn found_household_posts_name_and_welcomes_owner() {
        let (url, rx) = spawn_confirm_stub(r#"{"ok":true,"name":"Luca"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(found_household(&client, "login-nonce", "55501234", "Luca"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        let body = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["nonce"], "login-nonce");
        assert_eq!(parsed["telegram_id"], "55501234");
        assert_eq!(parsed["name"], "Luca");
        assert!(reply.contains("first member"), "reply: {reply}");
        assert!(reply.contains("Luca"), "reply: {reply}");
    }

    #[test]
    fn onboarding_urls_derive_from_confirm_base() {
        // The two write paths share the confirm base so one override retargets all.
        assert_eq!(auth_found_url(), "http://127.0.0.1:7788/auth/found");
        assert_eq!(invite_redeem_url(), "http://127.0.0.1:7788/invite/redeem");
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
    fn web_inbound_chat_id_resolves_from_bots_map_when_top_level_empty() {
        // Regression (task urgent-web-inbound): a bots-map-only config leaves the
        // legacy top-level `chat_id` empty, so `run_web_inbound` used to bail with
        // "no chat id" and the kiosk ask died silently. The group chat id must fall
        // back to the bots map, exactly as every send path does.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        // No override, empty top-level: resolves the group from the bots map.
        assert_eq!(
            resolve_group_chat_id(&config, None).as_deref(),
            Some("-100777"),
        );
        // An explicit override still wins.
        assert_eq!(
            resolve_group_chat_id(&config, Some("-100999")).as_deref(),
            Some("-100999"),
        );
        // A blank override is ignored (falls through to the bots map).
        assert_eq!(
            resolve_group_chat_id(&config, Some("  ")).as_deref(),
            Some("-100777"),
        );
    }

    #[test]
    fn web_inbound_chat_id_prefers_legacy_top_level_then_none_when_unconfigured() {
        // Legacy top-level chat_id wins over the (absent) bots map.
        let legacy = TelegramConfig {
            bot_token: "123:ABC".to_string(),
            chat_id: "-100555".to_string(),
            bots: HashMap::new(),
        };
        assert_eq!(
            resolve_group_chat_id(&legacy, None).as_deref(),
            Some("-100555"),
        );

        // Nothing configured anywhere → None (caller bails with a helpful error).
        let empty = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: HashMap::new(),
        };
        assert_eq!(resolve_group_chat_id(&empty, None), None);
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

        let (bot_id, bot, chat) = resolve_send_bot(&config, None, None).unwrap();
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
            let (bot_id, bot, _chat) = resolve_send_bot(&config, None, None).unwrap();
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

        let (bot_id, bot, chat) = resolve_send_bot(&config, Some("777"), None).unwrap();
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
        let err = resolve_send_bot(&config, None, None).unwrap_err().to_string();
        assert!(err.contains("No Telegram bots configured"), "got: {err}");
    }

    /// The bug that motivated `review-digest-sent`: the Sunday review digest is
    /// composed and signed "— Otto", but the plain default send resolved to the
    /// lexicographically-first bot (bruno < otto), delivering Otto's words under
    /// Bruno's face. A persona-named send must resolve OTTO's token, never the
    /// alphabetical default.
    #[test]
    fn review_digest_send_as_otto_resolves_ottos_token_not_bruno() {
        let mut bots = HashMap::new();
        bots.insert(
            "bruno".to_string(),
            TelegramBotConfig {
                bot_token: "1000000:BRUNO".to_string(),
                chat_id: "10".to_string(),
                agent_id: Some("bruno".to_string()),
                username: None,
            },
        );
        bots.insert(
            "otto".to_string(),
            TelegramBotConfig {
                bot_token: "2000000:OTTO".to_string(),
                chat_id: "20".to_string(),
                agent_id: Some("otto".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        // Without a persona the default fallback would pick "bruno" (< "otto")
        // — that is exactly the wrong-identity trap.
        let (default_id, _, _) = resolve_send_bot(&config, None, None).unwrap();
        assert_eq!(
            default_id, "bruno",
            "sanity: the silent default lands on bruno — the very trap we are fixing"
        );

        // Naming the composing persona resolves Otto's own bot + token + chat.
        let (bot_id, bot, chat) = resolve_send_bot(&config, None, Some("otto")).unwrap();
        assert_eq!(bot_id, "otto");
        assert_eq!(bot.bot_token, "2000000:OTTO", "must send with Otto's token");
        assert_eq!(chat, "20", "defaults to Otto's own chat");
    }

    /// A persona can be named by the bot's `agent_id` binding even when the bot
    /// id (the `[telegram.bots.<id>]` key) differs — the composing voice is the
    /// agent, not the arbitrary map key.
    #[test]
    fn send_as_persona_matches_agent_id_binding() {
        let mut bots = HashMap::new();
        bots.insert(
            "otto_concierge_bot".to_string(),
            TelegramBotConfig {
                bot_token: "3000000:OTTO".to_string(),
                chat_id: "30".to_string(),
                agent_id: Some("otto".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let (bot_id, bot, _chat) = resolve_send_bot(&config, None, Some("otto")).unwrap();
        assert_eq!(bot_id, "otto_concierge_bot");
        assert_eq!(bot.bot_token, "3000000:OTTO");
    }

    /// The core safety property: a persona-named send NEVER falls back silently.
    /// If the named voice has no configured bot, resolution is a hard error —
    /// delivering under another persona's identity is worse than a failed send.
    #[test]
    fn send_as_persona_never_falls_back_silently() {
        // Only bruno is configured; a review signed as Otto must NOT go out via
        // bruno's bot.
        let mut bots = HashMap::new();
        bots.insert(
            "bruno".to_string(),
            TelegramBotConfig {
                bot_token: "1000000:BRUNO".to_string(),
                chat_id: "10".to_string(),
                agent_id: Some("bruno".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let result = resolve_send_bot(&config, None, Some("otto"));
        assert!(
            result.is_err(),
            "a persona-named send with no matching bot must hard-fail, not fall back to bruno"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("otto"), "error must name the missing persona: {err}");
        assert!(
            !err.contains("BRUNO"),
            "error must never leak/return bruno's token as a fallback: {err}"
        );
    }

    /// Even the legacy top-level `[telegram]` bot (id "default") does not satisfy
    /// a persona-named send — the default bot fronts no specific voice, so a
    /// request "as otto" against a default-only config still hard-fails rather
    /// than sending anonymously as the group bot.
    #[test]
    fn send_as_persona_does_not_match_legacy_default_bot() {
        let config = TelegramConfig {
            bot_token: "999:LEGACY".to_string(),
            chat_id: "500".to_string(),
            bots: HashMap::new(),
        };
        let err = resolve_send_bot(&config, None, Some("otto"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("otto"), "got: {err}");
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

    // ----- Precedence: awaiting-human task routing wins over conversation ----

    /// Seed a confirmed human binding (so the sender is conversation-eligible).
    fn seed_confirmed_binding(workgraph_dir: &Path, user: &str, agent: &str, name: &str) {
        let agency_dir = workgraph_dir.join("agency");
        let mut map = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
        let mut b = TelegramBinding::new(user, agent, name, None, ts());
        b.confirmed = true;
        b.confirmed_at = Some(ts());
        map.add(b).unwrap();
        map.save(&agency_dir).unwrap();
    }

    /// Write a human operator agent so a task assigned to it parks on HumanInput.
    fn write_human_agent(workgraph_dir: &Path, id: &str, name: &str) {
        use worksgood::agency::{Agent, PerformanceRecord, save_agent};
        let agents_dir = workgraph_dir.join("agency").join("cache/agents");
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
            executor: "shell".to_string(),
            preferred_model: None,
            preferred_provider: None,
            deployment_history: vec![],
            attractor_weight: 0.5,
            staleness_flags: vec![],
        };
        save_agent(&agent, &agents_dir).unwrap();
    }

    /// Write an AI persona agent (native executor ⇒ `is_human()` is false) —
    /// the family voice a per-agent Telegram bot fronts (e.g. "otto"). Used to
    /// exercise pr51's defense-in-depth check that a per-agent bot must front
    /// the same human the inbound sender is bound to.
    fn write_persona_agent(workgraph_dir: &Path, id: &str, name: &str) {
        use worksgood::agency::{Agent, PerformanceRecord, save_agent};
        let agents_dir = workgraph_dir.join("agency").join("cache/agents");
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
        save_agent(&agent, &agents_dir).unwrap();
    }

    /// A parked awaiting-human task exists AND the sender is a confirmed human.
    /// The classifier MUST route the plain reply to the task (task wins), NOT
    /// fall through to `Unmatched` where the conversational composer runs. This
    /// is the precedence the composer must never override.
    #[test]
    fn awaiting_human_task_reply_wins_over_conversation() {
        use worksgood::graph::{Node, Status, Task, WorkGraph};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");
        seed_confirmed_binding(dir, "luca-1", "human-luca", "Luca");

        // A ready task assigned to the human, parked on HumanInput and persisted.
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "groceries".to_string(),
            title: "groceries".to_string(),
            status: Status::Open,
            agent: Some("human-luca".to_string()),
            ..Default::default()
        }));
        crate::commands::service::human_dispatch::park_ready_human_tasks(&mut graph, dir);
        worksgood::parser::save_graph(&graph, crate::commands::graph_path(dir)).unwrap();

        // A plain (non-YES) message from the confirmed human.
        let outcome = classify_inbound_message(dir, "telegram", "luca-1", "eggs, milk, bread");
        match outcome {
            InboundOutcome::Routed { task_id } => assert_eq!(task_id, "groceries"),
            other => panic!("awaiting-human task must win over conversation, got {other:?}"),
        }
    }

    /// With NO awaiting-human task, the same confirmed human's plain message is
    /// `Unmatched` — the exact arm that dispatches the conversational composer.
    /// This is the complement of the precedence test: conversation only runs
    /// when task routing found nothing.
    #[test]
    fn plain_message_without_awaiting_task_is_unmatched_for_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");
        seed_confirmed_binding(dir, "luca-1", "human-luca", "Luca");
        // No parked task, empty graph on disk.
        worksgood::parser::save_graph(
            &worksgood::graph::WorkGraph::new(),
            crate::commands::graph_path(dir),
        )
        .unwrap();

        let outcome = classify_inbound_message(dir, "telegram", "luca-1", "hey otto, you around?");
        assert_eq!(
            outcome,
            InboundOutcome::Unmatched,
            "no awaiting task ⇒ Unmatched ⇒ conversational composer handles it"
        );
    }

    /// Regression — the pr51-auth swallow. A CONFIRMED human's plain chat turn
    /// that the awaiting-task router HARDENS to `Rejected` must STILL fall
    /// through to `Unmatched` (⇒ the conversational composer), never be silently
    /// consumed. This is the exact failure the live report described: after the
    /// pr51-auth deploy, a confirmed human's message in the group produced no
    /// reply and no log line. The auth hardening (Erik's fix) guards recording a
    /// reply onto an awaiting-human TASK; it must never gate ordinary
    /// conversation.
    ///
    /// The decline is produced the realistic Casa way: the human speaks
    /// through an AI-persona bot ("otto"), so pr51's defense-in-depth check
    /// ("a per-agent bot must front the same human the sender is bound to")
    /// declines to record it — bound to `human-luca`, arriving on a bot fronting
    /// the persona `otto`. That decline is the benign `NotParkedReply` outcome
    /// (NOT a security `Rejected`), and the listener must converse, not swallow.
    #[test]
    fn hardened_auth_rejection_falls_through_to_conversation() {
        use crate::commands::service::human_dispatch::{InboundReplyOutcome, route_inbound_reply};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_human_agent(dir, "human-luca", "Luca");
        write_persona_agent(dir, "otto", "Otto");
        seed_confirmed_binding(dir, "luca-1", "human-luca", "Luca");

        // A per-agent bot "otto" that fronts the AI persona (NOT human-luca).
        std::fs::write(
            dir.join("notify.toml"),
            "[telegram.bots.otto]\n\
             bot_token = \"111:AAA\"\n\
             chat_id = \"-100777\"\n\
             agent_id = \"otto\"\n\
             username = \"otto_casapinello_bot\"\n",
        )
        .unwrap();

        // No parked task; persist an empty graph the router loads from disk.
        worksgood::parser::save_graph(
            &worksgood::graph::WorkGraph::new(),
            crate::commands::graph_path(dir),
        )
        .unwrap();

        // Precondition: the hardened router declines to record this confirmed
        // human's turn (bound to human-luca but arriving on otto's bot). It is a
        // benign NotParkedReply — NOT a security Rejected — carrying the persona
        // DISPLAY NAME ("Otto"), never the raw agent id/hash. That name is what
        // the listener logs, so tailing the log never reads as a refusal.
        match route_inbound_reply(dir, "telegram:otto", "luca-1", Some("luca-1"), "otto, are you there?") {
            InboundReplyOutcome::NotParkedReply { persona } => {
                assert_eq!(persona, "Otto", "logs the persona display name, not a hash");
                let line = fallthrough_log_line(&persona);
                assert!(
                    !line.to_lowercase().contains("reject"),
                    "fall-through log must not read as a rejection: {line}"
                );
                assert!(
                    line.contains("Otto") && line.contains("continuing to conversation"),
                    "neutral one-liner names the persona and says it continues: {line}"
                );
            }
            other => panic!("expected benign NotParkedReply precondition, got {other:?}"),
        }

        // The listener MUST fall through to conversation, not swallow.
        let outcome =
            classify_inbound_message(dir, "telegram:otto", "luca-1", "otto, are you there?");
        assert_eq!(
            outcome,
            InboundOutcome::Unmatched,
            "a hardened-auth REJECTION of a confirmed human's chat turn must fall through to \
             conversation, never be silently swallowed (pr51-auth regression)"
        );
    }

    // --- lifecycle cross-surface delivery (lifecycle-messages-obey) --------
    //
    // The one-path writer: a lifecycle report-back must reach BOTH surfaces the
    // family sees — Telegram AND the constellation pane's `.casa/group-feed.jsonl`
    // ledger — for a GROUP origin, and must VERIFY delivery (retry once, then
    // surface an error so the caller re-arms). Luca's screenshots showed the pane
    // missing 'is on it'/'Done!' because the send bypassed the ledger; these pin
    // that it no longer can.

    use worksgood::graph::{OriginChannel, TaskOrigin};
    use worksgood::notify::lifecycle::{LifecycleEvent, LifecycleFire};
    use worksgood::notify::telegram_conversation::ReplySink;

    fn lc_fire(channel: OriginChannel, event: LifecycleEvent, text: &str) -> LifecycleFire {
        LifecycleFire {
            task_id: "tweak-the-week".to_string(),
            event,
            origin: TaskOrigin::new(channel, "-100999", "Luca", "nora", Some("nora".to_string())),
            text: text.to_string(),
        }
    }

    /// A [`ReplySink`] that fails its first `fail_first` attempts, then succeeds —
    /// records every attempt so a test can count sends and prove the retry.
    struct FlakySink {
        fail_first: std::sync::Mutex<u32>,
        attempts: std::sync::Mutex<Vec<String>>,
    }
    impl FlakySink {
        fn new(fail_first: u32) -> Self {
            Self {
                fail_first: std::sync::Mutex::new(fail_first),
                attempts: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl ReplySink for FlakySink {
        async fn send(&self, _bot: &str, _chat: &str, text: &str) -> Result<Option<String>> {
            self.attempts.lock().unwrap().push(text.to_string());
            let mut left = self.fail_first.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                anyhow::bail!("induced send failure");
            }
            Ok(Some("mid-1".to_string()))
        }
    }

    fn feed_lines(feed: &Path) -> Vec<String> {
        std::fs::read_to_string(feed)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn lifecycle_group_report_back_lands_in_feed_and_telegram_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = RecordingSink::default();
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Nora is on it 🍳",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(deliver_lifecycle_fire(&sink, &config, &feed, &fire))
            .unwrap();

        // Telegram: exactly one send.
        assert_eq!(sink.sends.lock().unwrap().len(), 1, "exactly one telegram send");
        // Pane feed: exactly one `agent` line carrying the report-back.
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1, "exactly one feed line, got {lines:?}");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["kind"], "agent", "{v}");
        assert_eq!(v["agentId"], "nora", "{v}");
        assert!(
            v["text"].as_str().unwrap().contains("is on it"),
            "the ledger carries the 'is on it' report-back: {v}"
        );
    }

    #[test]
    fn lifecycle_direct_report_back_never_leaks_into_the_shared_feed() {
        // A 1:1 DM report-back is private — it reaches Telegram but must NEVER be
        // written into the shared group-feed the pane renders (docs/15 privacy).
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = RecordingSink::default();
        let fire = lc_fire(
            OriginChannel::TelegramDirect,
            LifecycleEvent::Done,
            "Done! that's sorted ✅",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(deliver_lifecycle_fire(&sink, &config, &feed, &fire))
            .unwrap();

        assert_eq!(sink.sends.lock().unwrap().len(), 1, "the 1:1 DM is still sent");
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "a 1:1 DM report-back must not touch the shared group feed"
        );
    }

    #[test]
    fn lifecycle_send_retries_once_then_succeeds_and_still_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = FlakySink::new(1); // first attempt fails, retry succeeds
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Nora is on it 🍳",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(deliver_lifecycle_fire(&sink, &config, &feed, &fire))
            .unwrap();

        assert_eq!(
            sink.attempts.lock().unwrap().len(),
            2,
            "a transient failure is retried exactly once"
        );
        assert_eq!(
            feed_lines(&feed).len(),
            1,
            "a retried-then-delivered report-back still mirrors to the ledger exactly once"
        );
    }

    #[test]
    fn lifecycle_send_that_fails_twice_errors_and_does_not_mirror() {
        // Both attempts fail → Err (so run_lifecycle re-arms the FiredLog) and the
        // undelivered line must NOT appear in the pane (no phantom "Done!").
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = FlakySink::new(2);
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Nora is on it 🍳",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(deliver_lifecycle_fire(&sink, &config, &feed, &fire));

        assert!(res.is_err(), "two failures surface an error for the caller to re-arm");
        assert_eq!(
            sink.attempts.lock().unwrap().len(),
            2,
            "exactly two attempts: the send plus one retry"
        );
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "an undelivered report-back must not appear in the pane"
        );
    }

    // ── Daily-digest flush delivery (task re-arm-the) ──────────────────────
    //
    // The morning digest must reach Telegram AND land in the canonical ledger
    // the pane reads — the same one-path contract lifecycle report-backs obey
    // (lifecycle-messages-obey). These exercise `deliver_digest_fire` directly,
    // mirroring the lifecycle delivery tests above.

    #[test]
    fn digest_delivers_to_telegram_and_mirrors_to_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = RecordingSink::default();
        let text = "Today: PT check-in at 19:30 · how was last night's salmon?";

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(deliver_digest_fire(
            &sink, &config, &feed, "otto", "-100777", text,
        ))
        .unwrap();

        // Telegram: exactly one send, to the resolved chat.
        let sends = sink.sends.lock().unwrap();
        assert_eq!(sends.len(), 1, "exactly one digest telegram send");
        assert_eq!(sends[0].1, "-100777", "sent to the resolved chat");
        assert_eq!(sends[0].2, text, "the composed digest is what goes out");
        drop(sends);

        // Ledger: exactly one `agent` line carrying the digest text.
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1, "exactly one feed line, got {lines:?}");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["kind"], "agent", "{v}");
        assert!(
            v["text"].as_str().unwrap().contains("PT check-in"),
            "the ledger carries the morning digest: {v}"
        );
    }

    #[test]
    fn digest_send_that_fails_twice_errors_and_does_not_mirror() {
        // Both attempts fail → Err so `run_digest` leaves the pending queue
        // intact for the next tick, and NOTHING is mirrored (no phantom digest
        // in the pane for a message that never reached the human).
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let config = TelegramConfig::default();
        let sink = FlakySink::new(2);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(deliver_digest_fire(
            &sink, &config, &feed, "otto", "-100777", "Today: something",
        ));

        assert!(res.is_err(), "two failures surface an error so the queue is kept");
        assert_eq!(
            sink.attempts.lock().unwrap().len(),
            2,
            "exactly two attempts: the send plus one retry"
        );
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "an undelivered digest must not appear in the ledger"
        );
    }
}
