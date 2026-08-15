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

use crate::casa::reply_delivery::{
    BorrowedReplySink, FamilyReplyDelivery, GuardPolicy, MirrorOutcome, RecordingSink, ReplyScope,
    write_engine_receipt_at,
};
use crate::casa::telegram_photo::handle_photo_shopping_turn;
use worksgood::notify::NotificationChannel;
use worksgood::notify::casa_audience;
use worksgood::notify::casa_feed;
use worksgood::notify::config::NotifyConfig;
use worksgood::notify::family_plan;
use worksgood::notify::fast_lane;
use worksgood::notify::ownership;
use worksgood::notify::relay_receipt;
use worksgood::notify::telegram::{TelegramBotConfig, TelegramChannel, TelegramConfig};
use worksgood::notify::telegram_conversation::durable_telegram_digest_v1;
use worksgood::notify::telegram_dedupe::{DedupeKey, DedupeSet};
use worksgood::notify::telegram_family_commands as family_commands;
use worksgood::notify::telegram_group::{
    Election, NaturalRoute, ResolvedBot, elect_group_inbound_with_owner_map,
    elect_responders_with_owner_map, election_decision_summary, is_discussion_ask,
    parse_at_mention_tokens, resolve_mentioned_bot, route_natural_with_owner_map,
};
use worksgood::notify::telegram_voice;

// Moved to Casa's layer in slice 3 of the upstream split; two of our lanes here still
// resolve a `--now` override with it. See casa/remind.rs.
use crate::casa::remind::parse_naive_now;

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
    /// A plain, family-voice label for the device that signed in — "your iPhone",
    /// "a Mac", … — derived by the gateway from the browser's User-Agent at
    /// /auth/start (task sign-in-confirmation). NEVER a raw UA string. Absent on an
    /// older gateway, in which case the listener falls back to "a new device".
    #[serde(default)]
    device: Option<String>,
    /// True only when the signing-in browser is the MARKED family tablet, so the
    /// confirmation may legitimately say "the kitchen tablet" — never as a default.
    #[serde(default)]
    tablet: bool,
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
    /// Same family-voice device label as [`ConfirmResp::device`] — the founding
    /// welcome names the REAL device the owner signed in on (task
    /// sign-in-confirmation). Absent on an older gateway → "a new device".
    #[serde(default)]
    device: Option<String>,
    /// True only when the founding scan came from the MARKED family tablet.
    #[serde(default)]
    tablet: bool,
}

/// Outcome of a `/start login_<nonce>` confirm against the gateway. The founding
/// window (item 1) needs to distinguish an EMPTY roster (offer ownership) from a
/// genuinely unknown user (ask the household founder to add you), so the handler branches on this
/// rather than only receiving a pre-baked reply string.
/// The family-voice fallback label when the gateway did not carry a device
/// descriptor (an older gateway, or a client with no User-Agent). NEVER a raw UA.
const DEVICE_LABEL_FALLBACK: &str = "a new device";

/// The family-voice label for the MARKED family tablet — the ONE case where the
/// confirmation may say "the kitchen tablet" (task sign-in-confirmation).
const TABLET_DEVICE_LABEL: &str = "the kitchen tablet";

#[derive(Debug, PartialEq)]
enum WebLoginOutcome {
    /// The telegram id resolved to a household human; the browser is signed in.
    /// Carries the family-voice device descriptor the gateway derived from the
    /// signing-in browser (task sign-in-confirmation): `device` is a plain label
    /// ("your iPhone", "a Mac", …) and `tablet` is true ONLY when the browser is
    /// the marked family tablet, which is the one case the reply may name "the
    /// kitchen tablet".
    SignedIn { device: String, tablet: bool },
    /// The roster is EMPTY (fresh deployment) — the handler runs the "are you
    /// the owner?" founding handshake instead of rejecting.
    EmptyRoster,
    /// The id is not in a (non-empty) roster — ask the household founder to add you.
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
            // Name the REAL device (task sign-in-confirmation). Only the marked
            // family tablet says "the kitchen tablet"; every other device uses the
            // plain label the gateway mapped from its User-Agent, never a default
            // tablet string and never a raw UA.
            WebLoginOutcome::SignedIn { device, tablet } => {
                let label = if *tablet {
                    TABLET_DEVICE_LABEL
                } else if device.trim().is_empty() {
                    DEVICE_LABEL_FALLBACK
                } else {
                    device.as_str()
                };
                format!("You're signed in on {label} ✋")
            }
            WebLoginOutcome::UnknownUser => {
                "I don't recognise you yet — ask someone already in the household to add you."
                    .to_string()
            }
            WebLoginOutcome::EmptyRoster
            | WebLoginOutcome::LinkExpired
            | WebLoginOutcome::NoSession => {
                "That sign-in link expired — reopen the Casa page and tap the fresh link."
                    .to_string()
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
    if nonce.is_empty() { None } else { Some(nonce) }
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
        Some(c) if c.ok => WebLoginOutcome::SignedIn {
            device: c.device.unwrap_or_default(),
            tablet: c.tablet,
        },
        Some(c) if c.reason.as_deref() == Some("empty-roster") => WebLoginOutcome::EmptyRoster,
        Some(c) if c.reason.as_deref() == Some("unknown-user") => WebLoginOutcome::UnknownUser,
        Some(c)
            if matches!(
                c.reason.as_deref(),
                Some("unknown-nonce") | Some("expired") | Some("used")
            ) =>
        {
            WebLoginOutcome::LinkExpired
        }
        _ => WebLoginOutcome::NoSession,
    }
}

/// Backward-compatible thin wrapper returning the family-voice reply string for
/// the non-founding outcomes (used by the existing unit tests + the plain
/// signed-in / unknown-user / no-session paths).
async fn confirm_web_login(client: &reqwest::Client, nonce: &str, telegram_id: &str) -> String {
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
            let who = r
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("friend");
            format!(
                "Welcome to the household, {who}! You're all set — sign in on any device. \u{1f3e0}"
            )
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
            // Name the REAL device the owner founded from (task
            // sign-in-confirmation): the marked family tablet only when flagged,
            // otherwise the gateway's plain device label (fallback "a new device").
            let device = r.device.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let label = if r.tablet {
                TABLET_DEVICE_LABEL
            } else {
                device.unwrap_or(DEVICE_LABEL_FALLBACK)
            };
            format!(
                "This home is yours now, {who} — you're the first member. \u{2705} \
                 You're signed in on {label}; invite the rest of the family from Manage household."
            )
        }
        _ => "I couldn't finish setting up — tap the sign-in link for a fresh one and try again."
            .to_string(),
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
        .with_context(|| {
            format!(
                "No notify.toml found. Create one at .wg/notify.toml in this project (that is \
                 what is checked first, and what `casa` and the /setup wizard write), or \
                 globally at {}",
                worksgood::notify::config::default_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<config dir>/worksgood/notify.toml".to_string()),
            )
        })?;
    let config = TelegramConfig::from_notify_config(&notify_config)?;
    let effective_chat_id = chat_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.chat_id.clone());

    println!("Starting Telegram listener...");
    println!("{}", bot_banner(&config));
    // The listener's startup banner is written to a log file that outlives the
    // run; the chat it is bound to is the household's, and an opaque handle is
    // all an operator needs to tell one run's binding from another's.
    println!(
        "Chat ID: {}",
        worksgood::notify::telegram::redact_chat_id(&effective_chat_id)
    );

    // Build one channel per configured bot. Live evidence for this whole task:
    // Luca tags a bot in the group and the @mention lands ONLY in that bot's
    // getUpdates queue — so a listener that polls a single bot never sees
    // mentions of the others. We long-poll EVERY bot concurrently (one tokio
    // task per bot, each persisting its own offset) and funnel them all into
    // one shared receiver, which the single routing pipeline below drains.
    // Each poll task publishes its health under `<dir>/service/listener_health/`
    // so a DEAF listener (process alive, every poll failing — the 2026-07-24
    // blocked-egress incident) is visible to `wg service status` and to the casa
    // supervisor instead of existing only as log noise nobody reads.
    let channels: Vec<TelegramChannel> = TelegramChannel::all_from_notify_config(&notify_config)
        .context("Failed to build Telegram channels")?
        .into_iter()
        .map(|ch| ch.publishing_health_to(dir))
        .collect();
    if channels.is_empty() {
        anyhow::bail!("No Telegram bots configured — nothing to poll");
    }
    // Start this run from a clean slate so a previous run's failure streak
    // cannot make a freshly started listener look deaf.
    let bot_ids: Vec<String> = channels.iter().map(|c| c.bot_id().to_string()).collect();
    if let Err(e) =
        worksgood::notify::listener_health::reset_for_new_run(dir, &bot_ids, chrono::Utc::now())
    {
        eprintln!("warning: could not initialize listener health state: {e:#}");
    }

    // D20 — validate every bot's chat_id LOUDLY at listener start. A POSITIVE
    // chat_id is a 1:1 DM, not the negative family GROUP the relay expects; left
    // unflagged it makes a kiosk→group relay silently DM one person with
    // relayError:null. Warn on boot rather than misroute in silence (docs/05
    // §5.4). Non-fatal: an operator may deliberately target a DM, but they see
    // the warning either way.
    let all_bots = config.all_bots();
    worksgood::notify::telegram::warn_on_dm_chat_ids(&all_bots);

    // Generic replies go out through the project-local coordination owner.
    // A single legacy bot is unambiguous; a multi-bot household with no
    // configured coordination owner fails loudly rather than speaking as an
    // arbitrary first HashMap entry.
    let owner_map = ownership::OwnerMap::load(&project_root(dir));
    let coordination_bot = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .and_then(|owner| resolve_mentioned_bot(owner, &config));
    let reply_idx = match coordination_bot
        .as_ref()
        .and_then(|bot| channels.iter().position(|c| c.bot_id() == bot.bot_id))
    {
        Some(idx) => idx,
        None if channels.len() == 1 => 0,
        None => anyhow::bail!(
            "household.toml must assign the coordination domain to one configured Telegram bot"
        ),
    };

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
    // exactly once (see `crate::casa::lifecycle::run_lifecycle`). A cheap `pending_fires` gate keeps an
    // idle house silent — no per-tick chatter in the log. It runs on a plain OS
    // thread, NOT a tokio task: `crate::casa::lifecycle::run_lifecycle` builds its own runtime to send,
    // which would panic if nested inside this listener's runtime. Read-only
    // against the graph; it never touches the message-routing pipeline below.
    const LIFECYCLE_TICK_SECS: u64 = 15;
    {
        let lifecycle_dir = dir.to_path_buf();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(LIFECYCLE_TICK_SECS));
                let graph_path = crate::commands::graph_path(&lifecycle_dir);
                let graph = match worksgood::parser::load_graph(&graph_path) {
                    Ok(g) => g,
                    Err(_) => continue, // no graph yet — nothing to report
                };
                let root = project_root(&lifecycle_dir);
                let log_path = worksgood::notify::reminder::FiredLog::path(&root);
                let fired = worksgood::notify::reminder::FiredLog::load(&log_path);
                let pending = worksgood::notify::lifecycle::pending_fires(graph.tasks(), |id| {
                    fired.contains(id)
                });
                if pending.is_empty() && !lifecycle_reconciliation_needs_tick(&log_path) {
                    continue; // no unreported transition — stay quiet
                }
                // Something transitioned: deliver every pending report-back (same code
                // path as `wg telegram lifecycle`, real send). Exactly-once + pacing
                // are enforced inside via the persisted FiredLog.
                if let Err(e) = crate::casa::lifecycle::run_lifecycle(
                    &lifecycle_dir,
                    None,
                    false,
                    None,
                    false,
                    false,
                ) {
                    eprintln!(
                        "[{}] lifecycle report-back tick failed: {}",
                        chrono::Utc::now().format("%H:%M:%S"),
                        worksgood::notify::telegram::redact_bot_token(&format!("{e:#}")),
                    );
                }
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
        let family_delivery = FamilyReplyDelivery::load(&workgraph_dir, &route_config);

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
                            "[{}] {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            duplicate_drop_line(
                                cid,
                                sender,
                                date,
                                msg.message_id.as_deref().unwrap_or(""),
                                &msg.body,
                            ),
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
                // Durable dedupe id (docs/20 §2): the SAME content fingerprint the
                // cross-bot dedupe above keys on — stable across the four bot
                // deliveries AND across a listener restart that re-delivers this
                // update, so the gateway's `dedupeBySrcId` collapses a re-delivery
                // to one pane line. Hashed, so no chat/user id reaches the feed.
                // `None` when the transport didn't surface a chat id or send time
                // (a null srcId is unique-by-construction on the read side).
                let src_id = match (msg.chat_id.as_deref(), msg.sent_at) {
                    (Some(cid), Some(date)) => {
                        let sender = msg.sender_id.as_deref().unwrap_or(msg.sender.as_str());
                        Some(casa_feed::source_id(cid, sender, date, &msg.body))
                    }
                    _ => None,
                };
                // Resolve the sender to a bound human NAME (never the raw numeric
                // Telegram id a username-less person decodes to — task
                // mirrored-telegram-sender). Unbound bare id → the neutral label.
                let feed_sender = resolve_feed_sender(&workgraph_dir, &msg);
                // WRITER-STAMPED as an inbound: this row was never relayed
                // anywhere, so no delivery receipt could ever prove it. Saying so
                // in machine-readable form is what keeps a human's own message
                // from reading as an unbound row that nothing certifies.
                //
                // `declaring_non_relay` (not `with_non_relay_type`) because the
                // stamp is also what makes this append TYPECHECK: the
                // receipt-free form takes a `casa_feed::NonRelayRow`, so a
                // future outbound reply copied from this block cannot land here
                // unproven — it would not compile.
                let entry = casa_feed::group_entry(
                    &family_delivery.personas,
                    &feed_sender,
                    &msg.body,
                    casa_feed::now_ms(),
                    src_id,
                )
                .declaring_non_relay(casa_feed::NON_RELAY_TELEGRAM_INBOUND);
                if let Err(e) = casa_feed::append_entry_allocating(&feed_path, &entry) {
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
            let reply_scope = ReplyScope::from_chat_type(msg.chat_type.as_deref());

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
                            if let Err(e) = family_delivery
                                .send(
                                    reply_scope,
                                    channel.bot_id(),
                                    &reply_target,
                                    &telegram_pacing::backlog_skipped_line(),
                                )
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

                if let Err(e) = family_delivery
                    .send(
                        reply_scope,
                        channel.bot_id(),
                        &reply_target,
                        &response,
                    )
                    .await
                {
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
                            // Stamp the spoken mirror with the SAME durable dedupe
                            // id as the inbound text path (#23, docs/20 §2): a
                            // content fingerprint over (chat, sender, send-time,
                            // body) so a listener re-delivery collapses to one pane
                            // line; None when the transport surfaced no chat id /
                            // send time (a null srcId is unique-by-construction on
                            // the read side).
                            let spoken = telegram_voice::spoken_feed_body(&text);
                            let src_id = match (msg.chat_id.as_deref(), msg.sent_at) {
                                (Some(cid), Some(date)) => {
                                    let sender =
                                        msg.sender_id.as_deref().unwrap_or(msg.sender.as_str());
                                    Some(casa_feed::source_id(cid, sender, date, &spoken))
                                }
                                _ => None,
                            };
                            // Same sender resolution as the text mirror above: a
                            // spoken line must also show the human's NAME, never the
                            // raw numeric id (task mirrored-telegram-sender).
                            let feed_sender = resolve_feed_sender(&workgraph_dir, &msg);
                            // Same inbound stamp as the text mirror: a spoken
                            // message is still a human speaking into the group,
                            // and it was still never relayed anywhere — and the
                            // same `declaring_non_relay` witness, without which
                            // the receipt-free append below does not typecheck.
                            let entry = casa_feed::group_entry(
                                &family_delivery.personas,
                                &feed_sender,
                                &spoken,
                                casa_feed::now_ms(),
                                src_id,
                            )
                            .declaring_non_relay(casa_feed::NON_RELAY_TELEGRAM_INBOUND);
                            if let Err(e) = casa_feed::append_entry_allocating(&feed_path, &entry) {
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
                        if let Err(e) = family_delivery
                            .send(
                                reply_scope,
                                receiving.bot_id(),
                                &reply_target,
                                failure.message(),
                            )
                            .await
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
                        if let Err(e2) = family_delivery
                            .send(
                                reply_scope,
                                receiving.bot_id(),
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
            let gate = crate::casa::command_gate::command_gate(&msg);

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
                                if let Err(e) = family_delivery
                                    .send(reply_scope, channel.bot_id(), &reply_target, &q)
                                    .await
                                {
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
                                if let Err(e) = family_delivery
                                    .send(reply_scope, channel.bot_id(), &reply_target, &reply)
                                    .await
                                {
                                    eprintln!("Failed to send web sign-in reply: {e}");
                                }
                            }
                        }
                        // No numeric id to verify — cannot bind a session.
                        None => {
                            let reply = "That sign-in link expired — reopen the Casa page and tap the fresh link.";
                            if let Err(e) = family_delivery
                                .send(reply_scope, channel.bot_id(), &reply_target, reply)
                                .await
                            {
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
                    if let Err(e) = family_delivery
                        .send(reply_scope, channel.bot_id(), &reply_target, &reply)
                        .await
                    {
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
                            if let Err(e) = family_delivery
                                .send(reply_scope, channel.bot_id(), &reply_target, &reply)
                                .await
                            {
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
                            if let Err(e) = family_delivery
                                .send(reply_scope, channel.bot_id(), &reply_target, reply)
                                .await
                            {
                                eprintln!("Failed to send founding cancel: {e}");
                            }
                            continue;
                        } else {
                            // Ambiguous — re-prompt without consuming the window.
                            let reply = "Just reply YES to set up this home as yours, or NO to cancel.";
                            if let Err(e) = family_delivery
                                .send(reply_scope, channel.bot_id(), &reply_target, reply)
                                .await
                            {
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

            let owner_map = ownership::OwnerMap::load(&project_root(&workgraph_dir));
            let election = elect_responders_with_owner_map(
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
                &owner_map,
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
                        crate::casa::group::run_group_discussion(
                            &workgraph_dir,
                            &route_config,
                            reply_chat,
                            &feed_path,
                            body,
                            &auth_sender,
                            &telegram_physical_turn_key(&msg),
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
                        crate::casa::group::run_group_collective(
                            &workgraph_dir,
                            &route_config,
                            reply_chat,
                            &feed_path,
                            body,
                            &auth_sender,
                            &telegram_physical_turn_key(&msg),
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
                if let Err(e) = family_delivery
                    .send(
                        reply_scope,
                        channel.bot_id(),
                        &reply_target,
                        &response,
                    )
                    .await
                {
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
                        if let Err(e) = family_delivery
                            .send(
                                reply_scope,
                                channel.bot_id(),
                                &reply_target,
                                &welcome,
                            )
                            .await
                        {
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
                            if let Err(e) = family_delivery
                                .send(
                                    reply_scope,
                                    channel.bot_id(),
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
                        if let Err(e) = family_delivery
                            .send(
                                reply_scope,
                                channel.bot_id(),
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
                        //
                        // CANCELLATION IS CHECKED FIRST: "cancel the reminder about
                        // the dentist" carries the reminder verb and would otherwise
                        // be filed as a SECOND reminder (date-reminder-fail (c)).
                        let reminder_now = chrono::Local::now().naive_local();
                        // READ-BACK IS CHECKED FIRST (task reminder-readback-lane):
                        // "what date is the reminder to call the dentist set for?"
                        // is a question about what is already on file. Answered
                        // deterministically from disk, scoped to the asker, writing
                        // nothing — and ahead of the cancel lane, because "did you
                        // cancel my dentist reminder?" carries the cancel verb and
                        // would otherwise be answered by actually cancelling it.
                        let short_circuit = try_reminder_readback(
                            &workgraph_dir,
                            &auth_sender,
                            &msg.sender,
                            &route_body,
                            reminder_now,
                        )
                        .map(|reply| ("reminder-read", reply))
                        .or_else(|| {
                            try_cancel_reminder(
                                &workgraph_dir,
                                &auth_sender,
                                &msg.sender,
                                &route_body,
                                reminder_now,
                            )
                            .map(|reply| ("reminder-cancel", reply))
                        })
                        .or_else(|| {
                            try_register_reminder(
                                &workgraph_dir,
                                &auth_sender,
                                &msg.sender,
                                &route_body,
                                reminder_now,
                            )
                            .map(|reply| ("reminder", reply))
                        })
                        // SHOPPING-LANGUAGE short-circuit (task
                        // engine-shopping-language). A list mutation typed into the
                        // family GROUP used to reach only the composer here — so a
                        // removal never removed, a nonsense item was confirmed as
                        // understood, and "don't add it yet" could still be written.
                        // The closed shopping lane now owns those turns on this path
                        // too, exactly as it already did for web-inbound and voice
                        // notes: it writes, or it ASKS, and it never fabricates.
                        .or_else(|| {
                            try_shopping_language(
                                &workgraph_dir,
                                &auth_sender,
                                &msg.sender,
                                &route_body,
                                reminder_now.date(),
                            )
                        })
                        // CAPABILITY short-circuit (task
                        // capability-answer-no-invented-work, live-cert C011). "What
                        // kinds of things can you help with?" is a social act with a
                        // deterministic answer, and routing it through the composer is
                        // what produced a 22.6-second reply carrying a phantom promise
                        // correction plus a task for work nobody asked for. Answered
                        // from the configured household instead: instant, one voice,
                        // nothing created. Checked LAST so a lane-specific ask, a
                        // reminder or a list mutation still owns its turn.
                        .or_else(|| try_capability_answer(&workgraph_dir, &route_body));
                        if let Some((short_circuit_lane, confirmation)) = short_circuit {
                            let owner_map =
                                ownership::OwnerMap::load(&project_root(&workgraph_dir));
                            let coordination_owner = owner_map
                                .owner_for_domain(ownership::Domain::Coordination);
                            let bot_id = convo::bot_id_for_channel_with_default(
                                &route_config,
                                &route_channel,
                                coordination_owner,
                            )
                            .unwrap_or_else(|| {
                                    route_channel
                                        .strip_prefix("telegram:")
                                        .unwrap_or(&route_channel)
                                        .to_string()
                            });
                            if route_config
                                .all_bots()
                                .into_iter()
                                .any(|(id, _)| id == bot_id)
                            {
                                if let Err(e) = family_delivery
                                    .send(
                                        reply_scope,
                                        &bot_id,
                                        &reply_target,
                                        &confirmation,
                                    )
                                    .await
                                {
                                    eprintln!(
                                        "Failed to send {short_circuit_lane} confirmation: {e}"
                                    );
                                }
                            }
                            println!(
                                "[{}] Handled {} request from {} -> answered via {}",
                                chrono::Utc::now().format("%H:%M:%S"),
                                short_circuit_lane,
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
                        let request_id = if matches!(entry, convo::Entry::GroupElected) {
                            // A privacy-off group turn reaches every bot with a
                            // transport-local message id. Key the elected reply
                            // by the shared physical fingerprint, exactly like
                            // collective voices, so a cross-bot/restart replay
                            // cannot post twice.
                            crate::casa::group::collective_request_id(
                                &reply_target,
                                &plan.route().bot_id,
                                &telegram_physical_turn_key(&msg),
                            )
                        } else {
                            // A direct message reaches one bot, so its Telegram
                            // message id remains a stable occurrence key.
                            format!(
                                "tg-{}-{}-{}",
                                reply_target,
                                msg.message_id.as_deref().unwrap_or("na"),
                                sender,
                            )
                        };
                        let timing = convo::AckTiming::from_env();
                        // Every reply uses the same scoped final delivery seam:
                        // group-elected answers mirror once; 1:1 replies remain
                        // private.
                        let delivery_scope = if matches!(entry, convo::Entry::GroupElected) {
                            ReplyScope::Group
                        } else {
                            ReplyScope::Private
                        };
                        let family_delivery_owned = family_delivery.clone();
                        let wg_config_owned = wg_config.clone();
                        // Hand the coalescer + admitted agent into the spawn so it
                        // marks the reply *sent* when it finishes — ending the
                        // pending turn so the next follow-up is a new turn (BUG 2).
                        let coalescer_spawn = coalescer.clone();
                        let coalesced_agent_spawn = coalesced_named_agent.clone();
                        tokio::spawn(async move {
                            let base = convo::BotReplySink::new(cfg_owned.clone());
                            let sink = family_delivery_owned.wrap(
                                base,
                                delivery_scope,
                                GuardPolicy::AlreadyGuarded,
                            );
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
                                &sink,
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
    let name = apply_confirmation(
        &mut bindings,
        sender,
        Some(sender),
        body,
        chrono::Utc::now(),
    )?;
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
    let root = project_root(workgraph_dir);
    let owner_map = ownership::OwnerMap::load(&root);
    let coordination_owner = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .unwrap_or_default()
        .to_string();
    // The sender must be a confirmed human; resolve their display name + bot.
    let binding = bindings.find_by_identity(Some(sender), Some(sender_display));
    let (recipient, bot) = match binding {
        Some(b) if b.confirmed => (
            b.name.clone(),
            b.bot_id
                .clone()
                .unwrap_or_else(|| coordination_owner.clone()),
        ),
        _ => return None,
    };

    let rem = reminder::intent_to_reminder(&intent, &recipient, &bot);
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

/// Handle a SHOPPING-LIST turn typed into the family group before it reaches the
/// composer, returning `(lane, reply)`: either the confirmation for a real list write or
/// the honest question a safety lane answers with.
///
/// THE GAP THIS CLOSES (task engine-shopping-language). The closed shopping lane already
/// ran on the kiosk/web path (`run_web_fast_lane_occurrence`) and on voice notes, but the
/// live Telegram TEXT listener never called it — every list mutation typed in the family
/// group was elected and COMPOSED. Measured before this existed: "Remove AA batteries
/// again." removed nothing, "Add glorptwax to shopping." was answered by the model, and
/// only the gateway half of the live-cert P1 fix was in force. Now this path runs the
/// SAME classifier, over the SAME vocabulary
/// ([`worksgood::notify::shopping_language`]), so both halves cannot drift.
///
/// Returns `None` — leaving the turn to the composer, exactly as before — when the turn
/// is not a shopping mutation, when the sender is not a confirmed human (onboarding runs
/// first), or when there is no plan to edit. Meal edits and reminders are untouched: only
/// `shopping-*` operations and the shopping ASK lanes are short-circuited here.
fn try_shopping_language(
    workgraph_dir: &Path,
    sender: &str,
    sender_display: &str,
    body: &str,
    today: chrono::NaiveDate,
) -> Option<(&'static str, String)> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::fast_lane::{self, Classification, FastLaneResult};

    // Is this a shopping turn at all? Classify FIRST (pure, no I/O) so a normal
    // conversational message costs nothing here.
    match fast_lane::classify(body, today) {
        Classification::FastLane(op) if op.kind_label().starts_with("shopping-") => {}
        Classification::Ask { .. } => {}
        _ => return None,
    }

    // Same gate as the reminder short-circuits: only a confirmed human may change the
    // family's list directly.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    match bindings.find_by_identity(Some(sender), Some(sender_display)) {
        Some(b) if b.confirmed => {}
        _ => return None,
    }

    let root = project_root(workgraph_dir);
    match fast_lane::run_fast_lane(&root, body, today) {
        FastLaneResult::Applied { report, op, .. } => match op.kind_label() {
            "shopping-add" => Some(("shopping-add", report)),
            "shopping-remove" => Some(("shopping-remove", report)),
            // A non-shopping op cannot arrive here (the classify gate above), but if it
            // ever did, the composer — not this lane — owns it.
            _ => None,
        },
        FastLaneResult::Answered { reply, .. } => Some(("shopping-ask", reply)),
        FastLaneResult::Fallback { .. } => None,
    }
}

/// Answer a bare CAPABILITY ask ("What kinds of things can you help with?")
/// instantly from the configured household, returning `(lane, reply)`.
///
/// THE GAP THIS CLOSES (task capability-answer-no-invented-work, live-cert run 2
/// C011). Asked in the family group at 21:18 on 2026-07-26, that question took
/// **22.6 seconds** through the full compose pipeline, came back with a phantom
/// promise correction ("I said I'd set that up but hit a snag … flagged it for the
/// coordinator"), and MINTED A TASK for a question that needs no work at all — a
/// human then answered it by hand. A capability ask has a deterministic answer, so
/// it is answered here: instantly, model-free, creating nothing.
///
/// Returns `None` — leaving the turn exactly where it was — when the message is not
/// a bare capability ask (a lane-specific "what can you do about dinner?" keeps its
/// owner) or when the household declares no domain ownership to describe. Unlike
/// the reminder/shopping short-circuits this needs NO confirmed-human gate: it
/// changes nothing and reveals nothing a family member could not already see.
fn try_capability_answer(workgraph_dir: &Path, body: &str) -> Option<(&'static str, String)> {
    use worksgood::notify::capability;
    use worksgood::notify::grounding;
    use worksgood::notify::ownership::OwnerMap;

    if !capability::is_capability_ask(body) {
        return None;
    }
    let root = project_root(workgraph_dir);
    let answer = capability::capability_answer(&OwnerMap::load(&root))?;
    // Same guard every deterministic family-facing reply passes through.
    let roster = grounding::load_family_voice_roster(&root, workgraph_dir);
    Some((
        "capability",
        grounding::enforce_family_voice(&answer, &roster),
    ))
}

/// Answer a reminder READ-BACK ("what date and time is the reminder to call the
/// dentist set for?") from what is persisted, scoped to the person asking.
///
/// THE GAP THIS CLOSES (task reminder-readback-lane, live-cert C052). A reminder
/// could be filed by three paths and read back by none: the grounded block reads
/// the weekly plan model only — never the ad-hoc reminders this listener writes —
/// and the fast lane hands every reminder read to the composer. So the answer was
/// drafted with no reminder data in front of it and simply agreed with whatever
/// date the QUESTION carried. Here the answer comes off disk instead: the same
/// plan-rows + ad-hoc merge `wg telegram remind --list` shows, filtered to the
/// asker's own reminders, rendered deterministically. Zero writes, one reply.
///
/// Returns `None` — leaving the turn exactly as it was — when the text is not a
/// reminder read, or when the sender is not a confirmed human. The unconfirmed
/// case matters for privacy as much as onboarding: without a resolved identity
/// there is no one to scope the answer to, and an unscoped answer would disclose
/// another member's reminder.
///
/// Checked BEFORE [`try_cancel_reminder`] and [`try_register_reminder`] on
/// purpose: "did you cancel my dentist reminder?" is a QUESTION carrying a cancel
/// verb, and the cancel lane would otherwise answer it by actually cancelling.
fn try_reminder_readback(
    workgraph_dir: &Path,
    sender: &str,
    sender_display: &str,
    body: &str,
    now: chrono::NaiveDateTime,
) -> Option<String> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::{grounding, reminder_readback};

    // Cheap and pure first: a non-reminder turn costs nothing here.
    reminder_readback::parse_readback(body)?;

    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    let requester = match bindings.find_by_identity(Some(sender), Some(sender_display)) {
        Some(b) if b.confirmed && !b.name.trim().is_empty() => b.name.clone(),
        _ => return None,
    };
    let members: Vec<String> = bindings
        .bindings
        .iter()
        .map(|b| b.name.clone())
        .filter(|n| !n.is_empty())
        .collect();

    let root = project_root(workgraph_dir);
    let owner_map = ownership::OwnerMap::load(&root);
    let answer = reminder_readback::answer_for(&root, body, &requester, &members, &owner_map, now)?;
    // Same guard every deterministic family-facing reply passes through.
    let roster = grounding::load_family_voice_roster(&root, workgraph_dir);
    Some(grounding::enforce_family_voice(&answer, &roster))
}

/// Detect a "cancel the reminder about …" request and drop the ONE pending
/// ad-hoc reminder it names, returning the one-line confirmation.
///
/// Returns `None` — leaving the turn to the composer, which can ask — when the
/// text is not a cancellation, when it matches no pending reminder, or when it
/// matches more than one. It never creates anything: this runs BEFORE
/// [`try_register_reminder`] precisely so a cancel phrase cannot be filed as a
/// brand-new reminder (date-reminder-fail (c)).
///
/// AMBIGUITY IS DECIDED ACROSS BOTH SURFACES, NOT JUST THIS ONE (task
/// cross-surface-reminder). The same words can name a `⏰ Reminder` row in the week
/// plan as easily as an ad-hoc one, and this lane sees only the ad-hoc store — so
/// "the one match" it used to act on could be one of two live reminders, and the
/// family got "Cancelled — …" for a reminder they still had. It now surveys the
/// plan too and stands down unless the ad-hoc row is the ONLY candidate anywhere.
fn try_cancel_reminder(
    workgraph_dir: &Path,
    sender: &str,
    sender_display: &str,
    body: &str,
    now: chrono::NaiveDateTime,
) -> Option<String> {
    use worksgood::agency::TelegramBindingMap;
    use worksgood::notify::reminder::{self, AdHocStore};

    let req = reminder::parse_reminder_cancel(body)?;

    // Same gate as registration: only a confirmed human can change the schedule.
    let agency_dir = workgraph_dir.join("agency");
    let bindings = TelegramBindingMap::load(&agency_dir).unwrap_or_default();
    match bindings.find_by_identity(Some(sender), Some(sender_display)) {
        Some(b) if b.confirmed => {}
        _ => return None,
    }

    let root = project_root(workgraph_dir);

    // ONE unambiguous target across both surfaces, or nothing goes. A plan row in
    // the count means this lane is not the one to act: the fast lane owns the plan
    // surface and will either remove that row or ask, and either way it must not
    // find the ad-hoc reminder already deleted underneath it.
    let survey = worksgood::notify::fast_lane::cancel_candidates(&root, &req.target, req.day, now);
    if survey.total() != 1 || survey.adhoc != 1 {
        return None;
    }

    let path = AdHocStore::path(&root);
    let mut store = AdHocStore::load(&path);
    let cancelled = match store.cancel(&req, now) {
        Ok(Some(r)) => r,
        // Nothing pending matches, or several do — the composer asks rather than
        // dropping the wrong one.
        Ok(None) | Err(_) => return None,
    };
    if let Err(e) = store.save(&path) {
        eprintln!("Failed to persist the reminder cancellation: {e}");
        return None;
    }
    Some(format!("Cancelled — {} \u{2713}", cancelled.text))
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

/// The neutral label shown for an inbound sender we could not resolve to a bound
/// human but whose display handle is a bare numeric Telegram user id. A number is
/// not a person, and the family conversation pane must never render one.
pub const NEUTRAL_SENDER_LABEL: &str = "family member";

/// Resolve the display NAME to write as the sender of a mirrored inbound GROUP
/// message in the casa conversation pane (task `mirrored-telegram-sender`).
///
/// The live bug: a confirmed human with no public @username (Luca, bound by his
/// numeric id `8905220378`) decodes at the listener boundary to that numeric id as
/// `msg.sender` (see `telegram_sender::SenderIdentity::display` — username → id →
/// "unknown"). The mirror wrote that raw number as the feed sender, so the pane
/// showed `8905220378` where it must show `Luca`.
///
/// We resolve ONCE here, against the SAME agency bindings `resolve_auth_sender`
/// and `HumansSource` read, trying the numeric id first then the @username: a bound
/// human shows their stored `name`. When no binding claims the sender AND the raw
/// handle is a bare numeric id, we return the neutral [`NEUTRAL_SENDER_LABEL`] —
/// never the digits. Any other unbound label (a real @username) passes through
/// unchanged. This mirrors `casa_feed`'s privacy rule: a chat/user id never reaches
/// the feed.
fn resolve_feed_sender(workgraph_dir: &Path, msg: &worksgood::notify::IncomingMessage) -> String {
    use worksgood::agency::TelegramBindingMap;
    let agency_dir = workgraph_dir.join("agency");
    if let Ok(map) = TelegramBindingMap::load(&agency_dir) {
        if let Some(b) = map.find_by_identity(msg.sender_id.as_deref(), Some(&msg.sender)) {
            let name = b.name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    neutralize_raw_sender(&msg.sender)
}

/// Scrub a bare numeric Telegram user id to the neutral label, leaving a real
/// display handle (@username / already-resolved name) untouched. The pure core of
/// [`resolve_feed_sender`]'s fallback, so the unit test can drive it directly.
fn neutralize_raw_sender(sender: &str) -> String {
    let s = sender.trim();
    if worksgood::agency::human_binding::is_numeric_id(s) {
        NEUTRAL_SENDER_LABEL.to_string()
    } else {
        s.to_string()
    }
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
fn resolve_group_chat_id(
    config: &TelegramConfig,
    chat_id_override: Option<&str>,
) -> Option<String> {
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

/// The chat a clarify window must reopen against: the ORIGINATING conversation
/// (the family-group `target` the gateway forwarded / a web pane maps to), and
/// NEVER a bare 1:1 DM id.
///
/// The live regression (task `nora-clarify-engine`): the engine wrote a clarify
/// to `.casa/clarify.jsonl` keyed by Luca's positive DM id (8905220378), so a
/// bare "yes" would only continue in his private chat — the family group that
/// asked never saw the follow-up, and only the gateway watchdog rescued it from
/// silence. Commit 313c6a6f already WARNS on a DM chat_id (D20); this makes the
/// behaviour safe: honour the passed group `target`, but if it is a positive DM
/// id fall back to the first configured family-group chat id (negative), and
/// only when nothing configures a group at all do we keep the target best-effort.
fn clarify_target(target: &str, config: &TelegramConfig) -> String {
    use worksgood::notify::telegram::is_dm_chat_id;
    if !is_dm_chat_id(target) {
        // Group / supergroup / channel / @username — the expected clarify target.
        return target.to_string();
    }
    // `target` is a 1:1 DM id — reopen the clarify in the family group instead.
    let group = config
        .all_bots()
        .into_iter()
        .map(|(_, b)| b.chat_id)
        .find(|c| !c.trim().is_empty() && !is_dm_chat_id(c))
        .or_else(|| {
            let c = config.chat_id.trim().to_string();
            (!c.is_empty() && !is_dm_chat_id(&c)).then_some(c)
        });
    match group {
        Some(g) => {
            eprintln!(
                "⚠️  clarify target {target} is a 1:1 DM (D20) — reopening the clarify \
                 window in the family group {g} instead of the human's private chat"
            );
            g
        }
        None => target.to_string(),
    }
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
        println!(
            "{}",
            telegram_sender::resolve_inbound_summary(&value, &bindings)
        );
    }
    Ok(())
}

/// `wg telegram decide` — run the listener's command-vs-election decision on a
/// raw Telegram update, without sending anything.
///
/// Feeds the raw `getUpdates` element through the SAME boundary the live
/// listener uses: [`decode_update`] (which reads the Telegram entities so a
/// bare `?` is distinguished from a real `/help`), then [`crate::casa::command_gate::command_gate`] and
/// [`elect_responders_with_owner_map`]. Prints the decision — is it a command, and if not, who
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

    let gate = crate::casa::command_gate::command_gate(&msg);

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
    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let election = elect_responders_with_owner_map(
        msg.chat_type.as_deref(),
        msg.chat_id.as_deref(),
        &msg.body,
        &msg.mention_usernames,
        msg.reply_to_bot.as_deref(),
        msg.sender_is_bot,
        human_count,
        &config,
        &owner_map,
    );
    let (elected, addressed_by): (Option<String>, Option<String>) = match &election {
        Election::One {
            bot, addressed_by, ..
        } => (
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
    run_standup_for_scope(workgraph_dir, config, target, ReplyScope::Group).await
}

async fn run_standup_for_scope(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    target: &str,
    scope: ReplyScope,
) -> Result<()> {
    use worksgood::notify::telegram_standup as standup;

    let roster = standup::load_project_roster(&project_root(workgraph_dir), config)?;
    if roster.is_empty() {
        eprintln!("No household roster entries have matching Telegram bots.");
        return Ok(());
    }
    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, config);

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

        match family_delivery
            .send(scope, &member.bot_id, target, &post.text)
            .await
        {
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

// ---------------------------------------------------------------------------
// Engine family-reply delivery seam (Telegram + scoped conversation feed)
// ---------------------------------------------------------------------------

/// ITEM 6 — A DELIVERY THAT LEFT NO ROW IS NOT A SUCCESS.
///
/// The mirror used to log its failure and return, so the caller reported a
/// delivered reply while the family's conversation held no record of it: the
/// pane shows nothing, the audit counts nothing, and the join finds nothing —
/// the "delivered, but absent everywhere" shape.
///
/// It is reported as UNPROVEN rather than as a proven failure, and that choice
/// is load-bearing: the message really was accepted by Telegram, so the turn's
/// reservation must stay HELD. Releasing it would answer the family twice to fix
/// a bookkeeping problem.
pub(crate) fn surface_mirror_failure(outcome: MirrorOutcome) -> Result<()> {
    match outcome {
        MirrorOutcome::Skipped | MirrorOutcome::Recorded { .. } => Ok(()),
        MirrorOutcome::Failed(detail) => Err(
            worksgood::notify::telegram_conversation::unproven_delivery(format!(
                "the reply was accepted by Telegram but left NO row in the family's conversation: {detail}"
            )),
        ),
    }
}

/// Load the committable household presentation used by the shared feed.
///
/// Delivery remains available when the presentation file is absent or invalid,
/// but the fallback catalog is intentionally empty: feed entries use a neutral
/// id-derived label and no emoji instead of a compiled or guessed identity.
pub(crate) fn load_feed_persona_catalog(project_root: &Path) -> casa_feed::PersonaCatalog {
    match casa_feed::PersonaCatalog::load(project_root) {
        Ok(catalog) => catalog,
        Err(e) => {
            eprintln!(
                "[telegram] could not load household presentation for the shared feed ({e:#})"
            );
            casa_feed::PersonaCatalog::default()
        }
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
    let want = sender
        .trim()
        .trim_start_matches("human-")
        .to_ascii_lowercase();
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

/// `wg telegram web-inbound --default-owner <opaque-id> --sender <humanId>
/// --message <text>` — make a web-origin (kiosk conversation-pane) message a
/// **first-class group turn**. A caller with no designated contact passes
/// `--no-default-owner` instead.
///
/// The live gap this closes: the kiosk send box only RELAYED a line into the
/// family Telegram group via a bot, and Telegram bots never see other bots'
/// messages — so the listener's election/conversation pipeline NEVER ran on a
/// kiosk-typed message. It was posted (`💬 Luca (kiosk): …`) and never answered,
/// while the same words typed on a phone got four replies.
///
/// This command runs the SAME pipeline the listener runs on a group message,
/// without a live socket: it elects responder(s) with the exact
/// [`elect_responders_with_owner_map`] table (@mention / addressed name / collective / concierge
/// / silence), then dispatches through the SAME senders + composer the listener
/// uses — [`run_group_discussion`] / [`run_group_collective`] for a collective
/// address, or the single-voice [`plan_conversation`] +
/// [`run_conversation_turn`] path for a named/concierge ask. Every reply goes
/// out to the group via the elected persona's OWN bot AND is mirrored into
/// `.casa/group-feed.jsonl` (via [`FamilyReplyDelivery`]) so the kiosk pane shows it.
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
///
/// The idempotency key for a web-inbound single-voice turn — UNIQUE per
/// physical turn, STABLE on a genuine re-fire.
///
/// `run_composed_turn`'s "one reply per turn" guard keys "already answered" on the
/// `request_id` (an outbox entry under that id ⇒ skip compose+send). A request_id that
/// is CONSTANT per `(chat, bot)` — as the old `web-{chat}-{bot}` was — makes the SECOND
/// named ask to this voice in this chat match the FIRST turn's outbox entry and
/// short-circuit to `Replied` while composing, sending, and mirroring NOTHING. That is
/// the exact 17:03 lie (task live-compose-reliability): the engine logged
/// "single voice (nora) answered [replied]" yet the pane got silence, because the guard
/// treated a brand-new question as a duplicate of an earlier one.
///
/// The gateway supplies the physical-turn key once per accepted occurrence.
/// Hashing that opaque key preserves a real dispatcher refire while allowing a
/// household member to repeat identical words later and receive another answer.
fn web_inbound_request_id(reply_chat: &str, bot_id: &str, physical_turn_key: &str) -> String {
    format!(
        "web-request-{}",
        durable_telegram_digest_v1(
            "web-inbound-request",
            &[reply_chat, bot_id, physical_turn_key],
        ),
    )
}

/// Stable physical-turn key for a Telegram collective election.
///
/// Telegram `message_id` is intentionally excluded: each privacy-off bot sees
/// the same physical group message with a different id. Use the exact
/// cross-bot-stable fields used by [`DedupeKey`] instead — chat, stable sender,
/// sent-at second, and the complete body. The durable digest hashes that
/// canonical material directly; it must not embed `DedupeKey::text_hash`,
/// whose listener-local `DefaultHasher` algorithm is not a persistence
/// contract.
fn telegram_physical_turn_key(message: &worksgood::notify::IncomingMessage) -> String {
    let sender = message
        .sender_id
        .as_deref()
        .unwrap_or(message.sender.as_str());
    let sent_at = message.sent_at.unwrap_or(i64::MIN).to_string();
    format!(
        "telegram-turn-{}",
        durable_telegram_digest_v1(
            "telegram-physical-turn",
            &[
                message.chat_id.as_deref().unwrap_or(""),
                sender,
                &sent_at,
                &message.body,
            ],
        ),
    )
}

/// Pick the `(bot_id, chat)` a fast-lane confirmation should go out as: the
/// elected single voice when the ask elected one (so a food edit confirms in the
/// chef's voice, a workout edit in the coach's), else the first configured bot in
/// the family group. A fast-lane hit always has *something* to answer with.
fn fast_lane_reply_target(
    election: &Election,
    config: &TelegramConfig,
    target: &str,
) -> (String, String) {
    if let Election::One {
        bot, reply_chat, ..
    } = election
    {
        return (bot.bot_id.clone(), reply_chat.clone());
    }
    let bot_id = config
        .all_bots()
        .first()
        .map(|(id, _)| id.clone())
        .unwrap_or_default();
    (bot_id, target.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WebFastLaneDispatch {
    PassedThrough,
    Handled {
        outcome: crate::casa::plan_edits::WebFastLaneOutcome,
        resumed_delivery: bool,
        already_delivered: bool,
    },
}

fn web_fast_lane_delivery_id(physical_turn_key: &str) -> String {
    format!(
        "web-fast-lane-{}",
        durable_telegram_digest_v1("web-fast-lane-delivery", &[physical_turn_key]),
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_web_fast_lane_occurrence(
    workgraph_dir: &Path,
    root: &Path,
    message: &str,
    today: chrono::NaiveDate,
    calendar_owner: Option<&str>,
    physical_turn_key: &str,
    bot_id: &str,
    chat_id: &str,
    auth_sender: &str,
    persona: &str,
    family_roster: &worksgood::notify::grounding::FamilyVoiceRoster,
    sink: &dyn worksgood::notify::telegram_conversation::ReplySink,
) -> Result<WebFastLaneDispatch> {
    use worksgood::notify::fast_lane::{Classification, FastLaneResult};
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_occurrence::{OccurrenceJournal, OccurrenceState};

    // Do not create occurrence records for the ordinary conversation pipeline,
    // but always reopen an existing record first. Mutable dispatcher inputs may
    // drift on retry; the durable accepted occurrence still wins.
    // A safety-lane ASK (an implausible item, a held ask) is OWNED by this path too —
    // it must be journaled and delivered exactly once, like an applied edit, or the
    // question would be dropped and the composer would answer in its place with a
    // confident fabrication (task engine-shopping-language).
    // REMINDER READ-BACK (task reminder-readback-lane). "What date and time is
    // the reminder to call the dentist set for?" is a question about what is
    // already on file — answered here, from disk, scoped to the person asking,
    // writing nothing. It is journaled exactly like a safety-lane ASK so the one
    // deterministic reply cannot double-post, and it never mints a graph node.
    let readback = try_reminder_readback(
        workgraph_dir,
        auth_sender,
        auth_sender,
        message,
        crate::casa::plan_edits::web_fast_lane_now(today),
    );
    let classified_fast_lane = readback.is_some()
        || matches!(
            fast_lane::classify_at(message, crate::casa::plan_edits::web_fast_lane_now(today)),
            Classification::FastLane(_) | Classification::Ask { .. }
        );
    let opened = if classified_fast_lane {
        Some(OccurrenceJournal::<
            crate::casa::plan_edits::WebFastLaneOutcome,
        >::claim(
            workgraph_dir,
            crate::casa::plan_edits::WEB_FAST_LANE_OCCURRENCE_DOMAIN,
            physical_turn_key,
        )?)
    } else {
        OccurrenceJournal::<crate::casa::plan_edits::WebFastLaneOutcome>::reopen(
            workgraph_dir,
            crate::casa::plan_edits::WEB_FAST_LANE_OCCURRENCE_DOMAIN,
            physical_turn_key,
        )?
    };
    let Some((journal, state)) = opened else {
        return Ok(WebFastLaneDispatch::PassedThrough);
    };

    let (outcome, resumed_delivery) = match state {
        OccurrenceState::New => {
            // The read-back answer is already family-voice guarded and derived
            // wholly from persisted state; nothing was written to produce it.
            if let Some(reply) = readback {
                let outcome = crate::casa::plan_edits::WebFastLaneOutcome {
                    op_kind: "reminder-read".to_string(),
                    report: reply,
                    bot_id: bot_id.to_string(),
                    chat_id: chat_id.to_string(),
                };
                journal.mark_applied(&outcome)?;
                (outcome, false)
            } else {
                // The CLOCK, not just the date: a bare same-day reminder whose
                // time has already gone rolls forward instead of being filed in
                // the past (task next-weekday-strict). `calendar_owner` still
                // rides along — the owner decides whether a reminder op may
                // touch the plan at all.
                match fast_lane::run_fast_lane_at(
                    root,
                    message,
                    crate::casa::plan_edits::web_fast_lane_now(today),
                    calendar_owner,
                ) {
                    FastLaneResult::Fallback { .. } => {
                        // This closed-set classifier did not ultimately own the turn
                        // (for example, no current plan could be edited). Persist that
                        // decision so a dispatcher refire takes the same normal path.
                        journal.mark_passed_through()?;
                        return Ok(WebFastLaneDispatch::PassedThrough);
                    }
                    // A safety lane owned the turn and wrote NOTHING: the reply is a
                    // question ("I don't know what glorptwax is…") or an honest "that isn't
                    // on the list". Journaled and delivered exactly like an applied edit so
                    // it cannot double-post, but with NO graph node — nothing mutated.
                    FastLaneResult::Answered { reply, lane } => {
                        let guarded = worksgood::notify::grounding::enforce_family_voice(
                            &reply,
                            family_roster,
                        );
                        let outcome = crate::casa::plan_edits::WebFastLaneOutcome {
                            op_kind: format!("ask-{lane}"),
                            report: guarded,
                            bot_id: bot_id.to_string(),
                            chat_id: chat_id.to_string(),
                        };
                        journal.mark_applied(&outcome)?;
                        (outcome, false)
                    }
                    FastLaneResult::Applied { report, op, .. } => {
                        let guarded = worksgood::notify::grounding::enforce_family_voice(
                            &report,
                            family_roster,
                        );
                        if guarded != report {
                            eprintln!(
                                "[{}] family-voice guard: cleaned a web fast-lane reply before journaling",
                                chrono::Utc::now().format("%H:%M:%S"),
                            );
                        }

                        // Graph visibility belongs to the mutation stage. It is
                        // intentionally never repeated from an `applied` replay.
                        let origin = worksgood::graph::TaskOrigin::new(
                            worksgood::graph::OriginChannel::Web,
                            chat_id.to_string(),
                            auth_sender.to_string(),
                            persona.to_string(),
                            Some(bot_id.to_string()),
                        );
                        fast_lane::stamp_graph_node(workgraph_dir, &origin, &op, &guarded);

                        let outcome = crate::casa::plan_edits::WebFastLaneOutcome {
                            op_kind: op.kind_label().to_string(),
                            report: guarded,
                            bot_id: bot_id.to_string(),
                            chat_id: chat_id.to_string(),
                        };
                        // Persist canonical bytes and routing before the first send.
                        journal.mark_applied(&outcome)?;
                        (outcome, false)
                    }
                }
            }
        }
        OccurrenceState::Incomplete => {
            anyhow::bail!(
                "web fast-lane occurrence is incomplete; refusing to reapply an uncertain plan edit"
            );
        }
        OccurrenceState::Applied(outcome) => (outcome, true),
        OccurrenceState::Delivered(outcome) => {
            return Ok(WebFastLaneDispatch::Handled {
                outcome,
                resumed_delivery: false,
                already_delivered: true,
            });
        }
        OccurrenceState::PassedThrough => return Ok(WebFastLaneDispatch::PassedThrough),
    };

    let delivery_id = web_fast_lane_delivery_id(physical_turn_key);
    convo::send_reply_once(
        workgraph_dir,
        &delivery_id,
        &outcome.bot_id,
        &outcome.chat_id,
        &outcome.report,
        sink,
    )
    .await
    .context("web fast-lane reply delivery failed; the stored outcome remains retryable")?;
    journal.mark_delivered(&outcome)?;

    Ok(WebFastLaneDispatch::Handled {
        outcome,
        resumed_delivery,
        already_delivered: false,
    })
}

/// Honor a pinned domain owner for a heavy web-inbound turn (task
/// `owner-pin-engine`).
///
/// THE LAST ROUTING SEAM. When the gateway resolves a domain owner for an
/// open-ended heavy ask, it drops
/// that persona's crisp "on it" ack AND forwards the pin (`--owner` /
/// `WG_OWNER_PIN`). Before this, production's shell-out IGNORED the hint — the
/// fork's election re-resolved ownership and often re-elected the concierge
/// so the async reply came back in a DIFFERENT voice than the one that acked.
/// The pin must be BINDING for voice selection: the composing/delivering bot IS
/// the pinned persona, not a re-election winner.
///
/// Given the current election and an already-resolved pin, return it with its
/// single voice REBOUND to the pinned persona's bot. An absent pin leaves the
/// election untouched. The body is preserved from a One/All election, else the
/// raw `message` — a pinned turn the engine would have stayed silent on is
/// still forced to answer in the pinned voice, because the gateway already
/// decided it is a domain-owned heavy ask.
fn apply_owner_pin(
    election: Election,
    pin: Option<&ResolvedBot>,
    target: &str,
    message: &str,
) -> Election {
    let bot = match pin {
        Some(bot) => bot.clone(),
        None => return election,
    };
    let body = match &election {
        Election::One { body, .. } => body.clone(),
        Election::All { body, .. } => body.clone(),
        _ => message.trim().to_string(),
    };
    Election::One {
        bot,
        reply_chat: target.to_string(),
        body,
        addressed_by: worksgood::notify::telegram_group::AddressedBy::ReplyChain,
    }
}

/// The gateway's explicit choice for an otherwise unaddressed, general
/// web-origin ask.
///
/// This is a CLI argument rather than an environment variable on purpose: a
/// new gateway invoking an old engine must fail loudly on the unknown flag
/// instead of silently falling back to an engine-local coordination owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebDefaultOwner<'a> {
    /// Route a general ask to this configured opaque persona id.
    Designated(&'a str),
    /// There is currently no designated default contact.
    NoneDesignated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeedsContactReason {
    NoDefaultOwner,
    UnknownDefaultOwner,
}

impl NeedsContactReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoDefaultOwner => "no-default-owner",
            Self::UnknownDefaultOwner => "unknown-default-owner",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WebDefaultOwnerResolution {
    Routed(Election),
    NeedsContact(NeedsContactReason),
}

/// Return the one stable machine identity a bot binding declares.
///
/// A canonical nonblank `agent_id` is authoritative. The free-form bot-table
/// key is only the compatibility identity when no nonblank binding exists; it
/// never remains a second alias after an explicit binding is configured.
/// Nonblank bindings with surrounding whitespace are invalid rather than
/// normalized here, because downstream live composition reads the original
/// binding and must not disagree with this routing seam.
fn configured_machine_id<'a>(bot_id: &'a str, bot: &'a TelegramBotConfig) -> Option<&'a str> {
    match bot.agent_id.as_deref() {
        None => Some(bot_id),
        Some(agent_id) if agent_id.trim().is_empty() => Some(bot_id),
        Some(agent_id) if agent_id == agent_id.trim() => Some(agent_id),
        Some(_) => None,
    }
}

/// Resolve an opaque machine identity without the fuzzy username, handle, or
/// prefix fallbacks used for human-authored `@mention`s.
///
/// Exactly one configured bot may claim the exact canonical id. Zero claims,
/// duplicate explicit bindings, and a key-vs-binding cross-claim are all unsafe
/// and fail closed.
fn resolve_machine_bot(machine_id: &str, config: &TelegramConfig) -> Option<ResolvedBot> {
    if machine_id.is_empty() || machine_id != machine_id.trim() {
        return None;
    }
    let mut matches = config.all_bots().into_iter().filter_map(|(bot_id, bot)| {
        let canonical_id = configured_machine_id(&bot_id, &bot)?.to_string();
        if canonical_id != machine_id {
            return None;
        }
        let channel_type = if bot_id == "default" {
            "telegram".to_string()
        } else {
            format!("telegram:{bot_id}")
        };
        Some(ResolvedBot {
            bot_id,
            channel_type,
            agent_id: Some(canonical_id),
        })
    });
    let resolved = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(resolved)
}

/// Validate a gateway-supplied owner pin before any default routing, compose,
/// feed mutation, or send. A present pin is a binding machine contract: if it
/// cannot resolve exactly and uniquely, continuing with a newly elected voice
/// would break the acknowledgement/reply identity guarantee.
fn resolve_owner_pin(pin: Option<&str>, config: &TelegramConfig) -> Result<Option<ResolvedBot>> {
    let Some(pin) = pin else {
        return Ok(None);
    };
    if pin.trim().is_empty() {
        anyhow::bail!("web-inbound owner pin must be a nonblank canonical agent id");
    }
    let bot = resolve_machine_bot(pin, config).ok_or_else(|| {
        anyhow::anyhow!(
            "web-inbound owner pin '{pin}' does not resolve to exactly one configured canonical agent id"
        )
    })?;
    Ok(Some(bot))
}

/// Rebind a persisted clarification exchange to its exact canonical voice.
///
/// The ledger's `voice` is machine-authored continuity state, not a human
/// mention. A stale, renamed, fuzzy-handle, or multiply claimed identity must
/// therefore fail closed instead of falling through to a fresh election and
/// letting a different persona answer the confirmation.
fn bind_clarify_exchange(
    exchange: &ownership::ClarifyExchange,
    config: &TelegramConfig,
    target: &str,
) -> Result<Election> {
    let bot = resolve_machine_bot(&exchange.voice, config).ok_or_else(|| {
        anyhow::anyhow!(
            "web-inbound clarification voice '{}' does not resolve to exactly one configured canonical agent id; refusing a fresh election",
            exchange.voice
        )
    })?;
    Ok(Election::One {
        bot,
        reply_chat: target.to_string(),
        body: exchange.original_ask.clone(),
        addressed_by: worksgood::notify::telegram_group::AddressedBy::ReplyChain,
    })
}

/// Apply the gateway's default-contact declaration only at the web-only
/// general-election seam.
///
/// Explicit addressing, domain routing, collective asks, reply continuity, and
/// a positive owner pin have already made a stronger choice and pass through
/// byte-for-byte. `NoVoicesConfigured` is also a general-election shape: an
/// explicit designated owner may recover it, while no/unknown designation must
/// fail closed rather than inventing a voice.
fn apply_web_default_owner(
    election: Election,
    choice: WebDefaultOwner<'_>,
    config: &TelegramConfig,
    target: &str,
    message: &str,
) -> WebDefaultOwnerResolution {
    let is_general = matches!(
        &election,
        Election::One {
            addressed_by: worksgood::notify::telegram_group::AddressedBy::Concierge,
            ..
        } | Election::Silence(worksgood::notify::telegram_group::SilenceReason::NoVoicesConfigured)
    );
    if !is_general {
        return WebDefaultOwnerResolution::Routed(election);
    }

    let owner = match choice {
        WebDefaultOwner::Designated(owner) => owner,
        WebDefaultOwner::NoneDesignated => {
            return WebDefaultOwnerResolution::NeedsContact(NeedsContactReason::NoDefaultOwner);
        }
    };
    let Some(bot) = resolve_machine_bot(owner, config) else {
        return WebDefaultOwnerResolution::NeedsContact(NeedsContactReason::UnknownDefaultOwner);
    };
    let body = match &election {
        Election::One { body, .. } => body.clone(),
        _ => message.to_string(),
    };
    WebDefaultOwnerResolution::Routed(Election::One {
        bot,
        reply_chat: target.to_string(),
        body,
        addressed_by: worksgood::notify::telegram_group::AddressedBy::Concierge,
    })
}

pub fn run_web_inbound(
    workgraph_dir: &Path,
    sender: &str,
    message: &str,
    chat_id_override: Option<&str>,
    default_owner: WebDefaultOwner<'_>,
    owner_pin: Option<&str>,
    turn_id: Option<&str>,
    attempt_id: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram_conversation as convo;
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;
    let owner_pin = resolve_owner_pin(owner_pin, &config)?;

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
    // The clarify window must reopen against the ORIGINATING conversation (the
    // family group), never a bare 1:1 DM id — otherwise a bare "yes" only
    // continues in one human's private chat and the group that asked is stranded
    // (task nora-clarify-engine). Sanitised once here and used for BOTH the
    // pending-lookup and the ledger open below so the fingerprint chat matches.
    let clarify_chat = clarify_target(&target, &config);
    let clarification = ownership::clarify_continuation(
        &clarify_root,
        &clarify_chat,
        &auth_sender,
        message,
        clarify_now,
        clarify_window,
    );

    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let (mut election, clarify_continued_body) = match clarification {
        Some(ex) => {
            // Resolve persisted machine identity before running any fresh
            // election. An invalid/stale voice returns an error here and cannot
            // fall through to the default contact or another elected persona.
            let election = bind_clarify_exchange(&ex, &config, &target)?;
            println!(
                "[{}] web-inbound clarify-continuation from {} -> {} (reusing original ask)",
                chrono::Utc::now().format("%H:%M:%S"),
                sender,
                ex.voice,
            );
            (election, Some(ex.original_ask))
        }
        None => {
            // FOLLOW-UP CONTINUITY, before a fresh election. "another one about
            // blueberry" names no domain, so the lexical election (docs/47) cannot
            // route it and it fell to the concierge — a two-turn joke exchange changed
            // voice halfway through. Observed live: The Chiller told the first joke and
            // the calendar helper told the second.
            //
            // Unlike a clarify-continuation this carries the NEW text, not the original
            // ask: the follow-up's whole content is the new qualifier ("about
            // blueberry"). And `clarify_continued_body` stays None so the window REOPENS
            // below, which is what lets "another one" work twice in a row.
            // Mentions are parsed here rather than inside, so the continuity check and
            // the fresh election below agree on what "addressed to somebody" means.
            let followup_mentions: Vec<String> = parse_at_mention_tokens(message);
            if let Some(ex) = ownership::followup_continuation(
                &clarify_root,
                &clarify_chat,
                &auth_sender,
                message,
                clarify_now,
                clarify_window,
                &owner_map,
                &followup_mentions,
            ) {
                if let Ok(election) = bind_clarify_exchange(&ex, &config, &target) {
                    println!(
                        "[{}] web-inbound follow-up continuation from {} -> {} (new body, same voice)",
                        chrono::Utc::now().format("%H:%M:%S"),
                        sender,
                        ex.voice,
                    );
                    // Re-body the bound election with THIS turn's text. A stale voice is
                    // the one thing bind_clarify_exchange refuses, and on that refusal we
                    // deliberately fall through to a fresh election rather than guessing.
                    let election = match election {
                        Election::One {
                            bot,
                            reply_chat,
                            addressed_by,
                            ..
                        } => Election::One {
                            bot,
                            reply_chat,
                            body: message.trim().to_string(),
                            addressed_by,
                        },
                        other => other,
                    };
                    (election, None)
                } else {
                    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
                    let human_count = human_agent_id_set(workgraph_dir).len();
                    (
                        elect_group_inbound_with_owner_map(
                            &target,
                            message,
                            &mention_usernames,
                            human_count,
                            &config,
                            &owner_map,
                        ),
                        None,
                    )
                }
            } else {
                // A genuinely fresh web-origin message is first-class GROUP inbound:
                // run the exact listener election seam (supergroup, no reply-chain,
                // never bot-sent). Continuations never enter this branch.
                let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
                let human_count = human_agent_id_set(workgraph_dir).len();
                (
                    elect_group_inbound_with_owner_map(
                        &target,
                        message,
                        &mention_usernames,
                        human_count,
                        &config,
                        &owner_map,
                    ),
                    None,
                )
            }
        }
    };

    // ── OWNER PIN (task owner-pin-engine) ─────────────────────────────────
    // THE LAST ROUTING SEAM. When the gateway forwarded a pinned domain owner
    // for a heavy turn (it dropped that persona's crisp ack), the pin is BINDING
    // for voice selection: the composing/delivering bot IS the pinned persona,
    // not a re-election winner. No pin leaves the election unchanged; a
    // supplied invalid/ambiguous pin was rejected above before reaching any
    // compose or delivery seam. Skipped when this turn is a clarify
    // continuation — that path already binds the ORIGINAL voice carrying the
    // original ask, which must win over a fresh pin. Placed BEFORE the dry-run
    // and decision-summary emits so both report the pinned voice.
    if clarify_continued_body.is_none() {
        if let Some(pin) = owner_pin.as_ref() {
            let rebound = apply_owner_pin(election.clone(), Some(pin), &target, message);
            if let Election::One { bot, .. } = &rebound {
                if !matches!(&election, Election::One { bot: b, .. } if b.bot_id == bot.bot_id) {
                    println!(
                        "[{}] web-inbound owner-pin from {} -> {} (binding — election bypassed for voice)",
                        chrono::Utc::now().format("%H:%M:%S"),
                        sender,
                        bot.agent_id.clone().unwrap_or_else(|| bot.bot_id.clone()),
                    );
                }
            }
            election = rebound;
        }
    }

    // ── DEFAULT CONTACT (web-only contract) ───────────────────────────────
    // Only an otherwise-general/concierge election consults this declaration.
    // It runs AFTER clarification and positive owner pin so those stronger
    // continuity/ownership signals remain binding, and BEFORE constructing any
    // delivery sink, fast-lane mutation, composer, or send.
    election = match apply_web_default_owner(election, default_owner, &config, &target, message) {
        WebDefaultOwnerResolution::Routed(election) => election,
        WebDefaultOwnerResolution::NeedsContact(reason) => {
            println!(
                "[{}] web-inbound election from {} -> msg=none chat=supergroup rule=needs-contact:{} target=none",
                chrono::Utc::now().format("%H:%M:%S"),
                sender,
                reason.as_str(),
            );
            if json {
                let out = serde_json::json!({
                    "dry_run": dry_run,
                    "category": "needs-contact",
                    "sender": sender,
                    "auth_sender": auth_sender,
                    "target": target,
                    "outcome": "needs-contact",
                    "reason": reason.as_str(),
                    "who": serde_json::Value::Null,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "web-inbound [needs-contact] from {sender}: no default contact is available"
                );
            }
            return Ok(());
        }
    };

    // Observability: one decision line, PII-safe (no tokens, no chat id text).
    println!(
        "[{}] web-inbound election from {} -> {}",
        chrono::Utc::now().format("%H:%M:%S"),
        sender,
        election_decision_summary(None, Some("supergroup"), &election),
    );

    let feed_path = casa_feed::feed_path_for(&project_root(workgraph_dir));
    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, &config);

    // One physical-turn key is shared by every elected voice and both delivery
    // shapes. A gateway occurrence id wins; older callers fall back to the
    // election body fingerprint. The attempt id rides alongside it so a
    // self-heal retry of a turn whose first attempt never reached the family is
    // answered instead of suppressed as a refire of that dead attempt.
    let turn_body = match &election {
        Election::All { body, .. } | Election::One { body, .. } => body.as_str(),
        _ => message.trim(),
    };
    let physical_turn_key =
        crate::casa::plan_edits::web_physical_turn_key(&target, turn_body, turn_id, attempt_id);

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
                standup::load_project_roster(&project_root(workgraph_dir), &config)?
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
                // The chat a clarify window would reopen against — the family
                // group, never a bare 1:1 DM id (task nora-clarify-engine). Exposed
                // credential-free so the DM-guard is provable through the real
                // binary without a live compose (which the dry-run seam skips).
                "clarify_target": clarify_chat,
                "turn_fingerprint": physical_turn_key,
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
        let (bot_id, chat) = fast_lane_reply_target(&election, &config, &target);
        let persona = convo::agent_for_bot(&config, &bot_id);
        // The occurrence helper guards the reply before journaling it. Delivery
        // must preserve those exact canonical bytes on an `applied` retry.
        let sink = family_delivery.wrap(
            convo::BotReplySink::new(config.clone()),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
        match rt.block_on(run_web_fast_lane_occurrence(
            workgraph_dir,
            &project_root(workgraph_dir),
            message,
            today,
            owner_map.owner_for_domain(ownership::Domain::Calendar),
            &physical_turn_key,
            &bot_id,
            &chat,
            &auth_sender,
            &persona,
            &family_delivery.family_roster,
            &sink,
        ))? {
            WebFastLaneDispatch::PassedThrough => {}
            WebFastLaneDispatch::Handled {
                outcome,
                resumed_delivery,
                already_delivered,
            } => {
                let phase = if already_delivered {
                    "already delivered"
                } else if resumed_delivery {
                    "stored delivery resumed"
                } else {
                    "applied"
                };
                println!(
                    "[{}] fast-lane {} {} for {} -> {} ({})",
                    chrono::Utc::now().format("%H:%M:%S"),
                    outcome.op_kind,
                    phase,
                    sender,
                    outcome.bot_id,
                    outcome.report,
                );

                if json {
                    let out = serde_json::json!({
                        "category": "fast-lane",
                        "fast_lane_op": outcome.op_kind,
                        "sender": sender,
                        "auth_sender": auth_sender,
                        "target": outcome.chat_id,
                        "outcome": outcome.report,
                        "replayed": resumed_delivery || already_delivered,
                        "already_delivered": already_delivered,
                    });
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    println!(
                        "web-inbound [fast-lane {}] from {sender}: {}",
                        outcome.op_kind, outcome.report,
                    );
                }
                return Ok(());
            }
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
                    crate::casa::group::run_group_discussion(
                        workgraph_dir,
                        &config,
                        reply_chat,
                        &feed_path,
                        body,
                        &auth_sender,
                        &physical_turn_key,
                    )
                    .await?;
                    Ok("discussion round posted".to_string())
                } else {
                    crate::casa::group::run_group_collective(
                        workgraph_dir,
                        &config,
                        reply_chat,
                        &feed_path,
                        body,
                        &auth_sender,
                        &physical_turn_key,
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
                let sink = family_delivery.wrap(
                    convo::BotReplySink::new(config.clone()),
                    ReplyScope::Group,
                    GuardPolicy::AlreadyGuarded,
                );
                let request_id =
                    web_inbound_request_id(reply_chat, &bot.bot_id, &physical_turn_key);
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
                    let voice = bot.agent_id.clone().unwrap_or_else(|| bot.bot_id.clone());
                    if let Err(e) = ownership::ClarifyLedger::open(
                        &clarify_root,
                        &clarify_chat,
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
pub(crate) fn human_agent_id_set(workgraph_dir: &Path) -> HashSet<String> {
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
/// * **Group** — a single-reply command is composed once and sent as the
///   command domain's project-configured owner, regardless of which bot's queue
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
        let scope = if is_group {
            ReplyScope::Group
        } else {
            ReplyScope::Private
        };
        return run_standup_for_scope(workgraph_dir, config, target, scope).await;
    }

    let text = compose_family_reply(workgraph_dir, config, cmd)?;

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

    // Which bot sends: in a group, the project-local domain owner; in a 1:1,
    // the bot the user actually messaged. Ambiguous/missing mappings fail
    // loudly rather than attributing the reply to an arbitrary configured bot.
    let bots = config.all_bots();
    let resolved = if is_group {
        let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
        let owner = cmd.owner(&owner_map).with_context(|| {
            format!(
                "household.toml has no owner for the {} command domain",
                cmd.domain.slug()
            )
        })?;
        resolve_mentioned_bot(owner, config).with_context(|| {
            format!(
                "configured {} owner '{}' has no Telegram bot",
                cmd.domain.slug(),
                owner
            )
        })?
    } else {
        let receiving = receiving_channel
            .strip_prefix("telegram:")
            .unwrap_or(receiving_channel);
        if receiving.is_empty() || receiving == "telegram" || receiving == "default" {
            if bots.len() != 1 {
                anyhow::bail!(
                    "direct {} reply has no unambiguous receiving Telegram bot",
                    cmd.keyword
                );
            }
            resolve_mentioned_bot(&bots[0].0, config)
                .context("the sole configured Telegram bot could not be resolved")?
        } else {
            resolve_mentioned_bot(receiving, config).with_context(|| {
                format!(
                    "direct {} reply names unknown receiving bot '{}'",
                    cmd.keyword, receiving
                )
            })?
        }
    };
    let chosen = bots.iter().find(|(id, _)| id == &resolved.bot_id);
    let bot_id = match chosen {
        Some((id, _)) => id.clone(),
        None => anyhow::bail!(
            "resolved Telegram bot '{}' is not configured",
            resolved.bot_id
        ),
    };

    let family_delivery = FamilyReplyDelivery::load(workgraph_dir, config);
    let scope = if is_group {
        ReplyScope::Group
    } else {
        ReplyScope::Private
    };
    family_delivery
        .send(scope, &bot_id, target, &text)
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
/// clock (overridable in the dry-run via [`crate::casa::one_shot_answers::compose_family_reply_on`]).
fn compose_family_reply(
    workgraph_dir: &Path,
    config: &TelegramConfig,
    cmd: &family_commands::FamilyCommand,
) -> Result<String> {
    crate::casa::one_shot_answers::compose_family_reply_on(
        workgraph_dir,
        config,
        cmd,
        chrono::Local::now().date_naive(),
        chrono::Utc::now(),
    )
}

// ---------------------------------------------------------------------------
// Log lines that touch household identifiers
// ---------------------------------------------------------------------------
//
// THE LEAK. `.casa/telegram.log` outlives every run and is copied into bug
// reports, pasted into chats and read by anyone with the box. Four of its noisiest
// lines printed the RAW negative group / DM chat id, the RAW numeric sender id,
// the message id, and — on the duplicate-drop path, which fires on ORDINARY family
// traffic — the private message body. Together that is a durable, plain-text
// record of who messaged whom, when, and what they said.
//
// These lines are rendered by PURE functions so the proof can assert on the real
// output rather than on the shape of a `println!` — a redaction you cannot capture
// is a redaction you cannot prove. Every identifier goes out as a stable opaque
// tag, so the diagnostics the lines exist for (which chat went quiet, is this the
// same duplicate again, did the report-back land) all still work.

/// The listener's duplicate-drop line.
fn duplicate_drop_line(
    chat_id: &str,
    sender: &str,
    date: i64,
    message_id: &str,
    body: &str,
) -> String {
    use worksgood::notify::telegram::{
        redact_actor_id, redact_body, redact_chat_id, redact_message_id,
    };
    format!(
        "Duplicate group message ({}, from {}, date {}, {}, {}) dropped — already handled",
        redact_chat_id(chat_id),
        redact_actor_id(sender),
        date,
        redact_message_id(message_id),
        redact_body(body),
    )
}

/// Wake the listener's lifecycle runner when an interrupted update needs
/// reconciliation, even if the stale FiredLog otherwise hides every transition.
/// An unreadable journal also wakes the runner so the error is surfaced loudly
/// instead of turning into permanent silence at the cheap pre-run gate.
fn lifecycle_reconciliation_needs_tick(log_path: &Path) -> bool {
    let path = crate::casa::lifecycle::lifecycle_rearm_path(log_path);
    match crate::casa::lifecycle::load_lifecycle_rearm_journal(&path) {
        Ok(journal) => !journal.entries.is_empty(),
        Err(_) => path.exists(),
    }
}

/// `wg telegram standup` — run a standup on demand (for the live demo and the
/// scripted end-to-end test). With `--dry-run` the posts are printed to stdout
/// in roster order instead of being sent, so the flow is verifiable without a
/// live group or real tokens.
pub fn run_standup(workgraph_dir: &Path, chat_id: Option<&str>, dry_run: bool) -> Result<()> {
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;
    let roster = standup::load_project_roster(&project_root(workgraph_dir), &config)?;
    if roster.is_empty() {
        anyhow::bail!("No household roster entries have matching Telegram bots.");
    }

    // Target: explicit --chat-id, else the first roster bot's configured chat
    // id (a household's roster bots normally share the group chat id).
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
    // use (see `load_telegram_config`): try `.wg/notify.toml` from CWD first, then
    // fall back to the global `worksgood/notify.toml` under the user's config dir.
    // (The old `workgraph/` global is read for back-compat only and warns.)
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
            "\nAdd a [telegram] block to .wg/notify.toml in this project, or to the global \
             worksgood/notify.toml in your config dir."
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
                println!("  .wg/notify.toml   (in this project — checked first)");
                println!(
                    "  or {}",
                    worksgood::notify::config::default_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<config dir>/worksgood/notify.toml".to_string()),
                );
                println!();
                println!("  [telegram]");
                println!("  bot_token = \"123456:ABC-DEF...\"");
                println!("  chat_id = \"12345678\"");
            }
        }
    }
    Ok(())
}

/// `wg telegram health` — is the listener actually HEARING the family?
///
/// Reads the per-bot poll health published by the running listener (see
/// [`worksgood::notify::listener_health`]) and returns `Ok(true)` when inbound
/// messages are arriving, `Ok(false)` when the listener is deaf or nothing is
/// polling. The caller maps `false` to a non-zero exit so a shell supervisor can
/// branch on it without parsing text.
///
/// Deliberately credential-free and network-free: it makes no Telegram call, so
/// it is safe to run every minute from a supervisor loop and it works in a
/// hermetic test with no tokens.
pub fn run_health(dir: &Path, json: bool, quiet: bool) -> Result<bool> {
    use worksgood::notify::listener_health::ListenerHealth;

    let now = chrono::Utc::now();
    let health = ListenerHealth::load(dir);
    let alarming = health.is_alarming(now);
    let healthy = !alarming;

    if quiet {
        return Ok(healthy);
    }

    if json {
        let out = serde_json::json!({
            "healthy": healthy,
            "reporting": !health.is_empty(),
            "summary": health.summary_line(now),
            "advice": health.advice_line(now),
            "deaf_bots": health.deaf_bots(now).iter().map(|b| b.bot_id.clone()).collect::<Vec<_>>(),
            "bots": health.bots,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if alarming {
        println!("Listener: ⚠️  {}", health.summary_line(now));
        if let Some(advice) = health.advice_line(now) {
            println!("  {advice}");
        }
        for bot in health.deaf_bots(now) {
            println!(
                "  {}: {} consecutive failure(s), {} total{}",
                bot.bot_id,
                bot.consecutive_failures,
                bot.total_failures,
                bot.last_error
                    .as_deref()
                    .map(|e| format!(" — last: {e}"))
                    .unwrap_or_default()
            );
        }
    } else {
        println!("Listener: {}", health.summary_line(now));
    }

    Ok(healthy)
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
                let identity = worksgood::notify::telegram_sender::identity_from_message(message);
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
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
pub(crate) fn load_telegram_config() -> Result<TelegramConfig> {
    let notify_config = NotifyConfig::load(Some(Path::new(".")))
        .context("Failed to load notification config")?
        .with_context(|| {
            format!(
                "No notify.toml found. Create one at .wg/notify.toml in this project (that is \
                 what is checked first, and what `casa` and the /setup wizard write), or \
                 globally at {}",
                worksgood::notify::config::default_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<config dir>/worksgood/notify.toml".to_string()),
            )
        })?;
    TelegramConfig::from_notify_config(&notify_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use worksgood::agency::{TelegramBinding, TelegramBindingMap};
    use worksgood::notify::telegram::TelegramBotConfig;

    // ── the log lines never carry the household's identifiers ───────────────
    //
    // Hermetic and exact: the fixtures below are the real shapes — a negative
    // supergroup id, a negative DM id, a numeric sender id, a bot-token-shaped
    // string, and a distinctive private body. None of them may appear ANYWHERE in
    // the rendered line, and the assertion is over the line the log really gets.

    const GROUP_CHAT: &str = "-1002233445566";
    const DM_CHAT: &str = "-987654321";
    const SENDER_ID: &str = "6123456789";
    const MESSAGE_ID: &str = "48271";
    const TOKEN_ISH: &str = concat!(
        "8123456789",
        ":",
        "AAH_fa",
        "kefakefakefakefakefakefake-fake"
    );
    const PRIVATE_BODY: &str =
        "Nadin is at the clinic on Thursday, don't tell the kids about the surprise";

    fn assert_opaque(line: &str, raws: &[&str]) {
        for raw in raws {
            assert!(
                !line.contains(raw),
                "a household identifier reached the log verbatim ({raw:?}) in: {line}",
            );
        }
    }

    #[test]
    fn the_duplicate_drop_line_carries_no_raw_ids_or_body() {
        let line = duplicate_drop_line(
            GROUP_CHAT,
            SENDER_ID,
            1_784_500_000,
            MESSAGE_ID,
            PRIVATE_BODY,
        );
        assert_opaque(
            &line,
            &[GROUP_CHAT, DM_CHAT, SENDER_ID, MESSAGE_ID, PRIVATE_BODY],
        );
        // A body PREFIX is not a redaction — of a short message it is the message.
        assert_opaque(&line, &["Nadin", "clinic", "surprise", "don't tell"]);
        // …and the line is still a useful diagnostic.
        assert!(line.contains("Duplicate group message"), "{line}");
        assert!(line.contains("chat:"), "{line}");
        assert!(line.contains("who:"), "{line}");
        assert!(line.contains("body:"), "{line}");
    }

    #[test]
    fn the_lifecycle_delivery_line_carries_no_raw_chat_or_text() {
        let line = crate::casa::lifecycle::lifecycle_delivery_line(
            "task-done",
            "week-start-engine",
            DM_CHAT,
            "telegram:otto",
            MESSAGE_ID,
            PRIVATE_BODY,
        );
        assert_opaque(&line, &[DM_CHAT, GROUP_CHAT, MESSAGE_ID, PRIVATE_BODY]);
        // The work-graph identifiers are NOT household data and must survive.
        assert!(line.contains("week-start-engine"), "{line}");
        assert!(line.contains("task-done"), "{line}");
        assert!(line.contains("telegram:otto"), "{line}");
    }

    /// A token-shaped string is never echoed by these lines either, whichever
    /// field it arrives in — a mis-set config puts a token where an id belongs.
    #[test]
    fn a_token_shaped_value_never_survives_a_log_line() {
        let dup = duplicate_drop_line(TOKEN_ISH, TOKEN_ISH, 0, TOKEN_ISH, TOKEN_ISH);
        assert_opaque(&dup, &[TOKEN_ISH, "AAH_fakefakefakefakefakefakefake-fake"]);
        let life = crate::casa::lifecycle::lifecycle_delivery_line(
            "x", "t", TOKEN_ISH, "b", TOKEN_ISH, TOKEN_ISH,
        );
        assert_opaque(&life, &[TOKEN_ISH, "AAH_fakefakefakefakefakefakefake-fake"]);
    }

    /// The tags must be STABLE (so one chat is followable through a log) and
    /// DISTINCT (so two households, or a chat and a sender sharing a number, never
    /// collapse into one tag).
    #[test]
    fn opaque_tags_are_stable_and_do_not_collide() {
        use worksgood::notify::telegram::{redact_actor_id, redact_body, redact_chat_id};
        assert_eq!(redact_chat_id(GROUP_CHAT), redact_chat_id(GROUP_CHAT));
        assert_ne!(redact_chat_id(GROUP_CHAT), redact_chat_id(DM_CHAT));
        // Domain separation: the same number as a chat and as a sender.
        assert_ne!(redact_chat_id(SENDER_ID), redact_actor_id(SENDER_ID));
        assert_ne!(redact_body("yes"), redact_body("no"));
        assert_eq!(redact_body(""), "body:empty");
        assert_eq!(redact_chat_id("  "), "chat:none");
    }

    fn ts() -> chrono::DateTime<chrono::Utc> {
        "2026-07-10T12:00:00Z".parse().unwrap()
    }

    // --- crate::casa::command_gate::command_gate (fix-command-leaks) ---------------------------------

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
        let g = crate::casa::command_gate::command_gate(&gate_msg("supergroup", false));
        assert!(!g.family && !g.operator, "no slash entity → no command");
        let g = crate::casa::command_gate::command_gate(&gate_msg("private", false));
        assert!(
            !g.family && !g.operator,
            "no slash entity → no command in DM either"
        );
    }

    #[test]
    fn command_gate_operator_reference_never_in_a_group() {
        // Even a genuine slash command in a family GROUP must NOT open the
        // operator claim/done path — that content is coordinator-only.
        let g = crate::casa::command_gate::command_gate(&gate_msg("supergroup", true));
        assert!(
            g.family,
            "a real /command still runs the family set in a group"
        );
        assert!(
            !g.operator,
            "the operator WG reference must never surface in a group"
        );
    }

    #[test]
    fn command_gate_operator_only_in_private_slash() {
        // A 1:1 operator DM with a real slash command is the only place the
        // operator reference may run.
        let g = crate::casa::command_gate::command_gate(&gate_msg("private", true));
        assert!(
            g.operator,
            "operator reference is allowed in a 1:1 slash command"
        );
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
        assert_eq!(
            AUTH_CONFIRM_URL_DEFAULT,
            "http://127.0.0.1:7788/auth/confirm"
        );
    }

    /// Spin a one-shot loopback HTTP stub standing in for the gateway
    /// `POST /auth/confirm`. It captures the request body (so the test can assert
    /// exactly `{nonce, telegram_id}` crossed the wire) and answers with
    /// `response_json`. Drives the REAL `confirm_web_login` POST path end to end.
    fn spawn_confirm_stub(
        response_json: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
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
    fn spawn_confirm_stub_raw(
        response_json: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
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
            std::env::set_var(
                "CASA_AUTH_CONFIRM_SECRET_FILE",
                "/nonexistent/casa/auth-confirm.secret",
            );
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

    /// An unknown telegram id → role-based recovery guidance, no session.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_unknown_user_gets_role_based_reply() {
        let (url, _rx) = spawn_confirm_stub(r#"{"ok":false,"reason":"unknown-user"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "999"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert_eq!(
            reply,
            "I don't recognise you yet — ask someone already in the household to add you."
        );
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

    // ── sign-in confirmation names the REAL device (task sign-in-confirmation) ──

    /// The signed-in confirmation names the device the gateway passed through and
    /// NEVER says "kitchen tablet" for an ordinary personal device. Parameterized
    /// across the device families the gateway maps a User-Agent to.
    #[test]
    fn signed_in_reply_names_the_real_device_not_kitchen_tablet() {
        for label in [
            "your iPhone",
            "your iPad",
            "a Mac",
            "a Windows PC",
            "an Android phone",
        ] {
            let reply = WebLoginOutcome::SignedIn {
                device: label.to_string(),
                tablet: false,
            }
            .reply();
            assert_eq!(reply, format!("You're signed in on {label} ✋"));
            assert!(reply.contains(label), "reply must name the device: {reply}");
            assert!(
                !reply.contains("kitchen tablet"),
                "a personal device must NOT be called the kitchen tablet: {reply}"
            );
        }
    }

    /// "the kitchen tablet" appears ONLY when the marker flag is set — never as a
    /// default — even if a device label also rode along.
    #[test]
    fn signed_in_reply_says_kitchen_tablet_only_when_marker_set() {
        let marked = WebLoginOutcome::SignedIn {
            device: "the kitchen tablet".to_string(),
            tablet: true,
        }
        .reply();
        assert_eq!(marked, "You're signed in on the kitchen tablet ✋");

        // The marker flag WINS over any stray device label — the tablet phrasing is
        // gated on the marker alone.
        let marker_beats_label = WebLoginOutcome::SignedIn {
            device: "your iPhone".to_string(),
            tablet: true,
        }
        .reply();
        assert!(
            marker_beats_label.contains("kitchen tablet"),
            "reply: {marker_beats_label}"
        );
    }

    /// An older gateway that carries no device descriptor → a neutral fallback
    /// label, never a raw UA and never "kitchen tablet".
    #[test]
    fn signed_in_reply_falls_back_when_no_device_descriptor() {
        let reply = WebLoginOutcome::SignedIn {
            device: String::new(),
            tablet: false,
        }
        .reply();
        assert_eq!(reply, "You're signed in on a new device ✋");
        assert!(!reply.contains("kitchen tablet"), "reply: {reply}");
    }

    /// The full confirm path carries the gateway's `device` label through to the
    /// family-voice reply: a signed-in iPhone is named as such, not the tablet.
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_names_the_signing_in_device() {
        let (url, _rx) = spawn_confirm_stub(r#"{"ok":true,"device":"your iPhone"}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "123456789"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert_eq!(reply, "You're signed in on your iPhone ✋");
        assert!(!reply.contains("kitchen tablet"), "reply: {reply}");
    }

    /// The marked family tablet path: the gateway sets `tablet:true`, so the reply
    /// may (and does) say "the kitchen tablet".
    #[test]
    #[serial_test::serial]
    fn confirm_web_login_marked_tablet_says_kitchen_tablet() {
        let (url, _rx) =
            spawn_confirm_stub(r#"{"ok":true,"device":"the kitchen tablet","tablet":true}"#);
        unsafe { std::env::set_var("CASA_AUTH_CONFIRM_URL", &url) };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let reply = rt.block_on(confirm_web_login(&client, "nonce", "123456789"));
        unsafe { std::env::remove_var("CASA_AUTH_CONFIRM_URL") };

        assert_eq!(reply, "You're signed in on the kitchen tablet ✋");
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
    #[serial_test::serial]
    fn onboarding_urls_derive_from_confirm_base() {
        // The two write paths share the confirm base so one override retargets all.
        // Serial because it reads the CASA_AUTH_CONFIRM_URL-derived base, which the
        // confirm/found stub tests set+clear — running in parallel with them races
        // on that env var (task sign-in-confirmation).
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

    /// Build a minimal inbound GROUP message carrying a display `sender` and an
    /// optional numeric `sender_id` — the two fields `resolve_feed_sender` reads.
    fn feed_msg(sender: &str, sender_id: Option<&str>) -> worksgood::notify::IncomingMessage {
        worksgood::notify::IncomingMessage {
            channel: "telegram".to_string(),
            sender: sender.to_string(),
            sender_id: sender_id.map(str::to_string),
            sender_is_bot: false,
            sent_at: Some(1_720_000_000),
            body: "all good?".to_string(),
            action_id: None,
            reply_to: None,
            message_id: Some("1".to_string()),
            chat_id: Some("-100".to_string()),
            chat_type: Some("supergroup".to_string()),
            mention_usernames: Vec::new(),
            reply_to_bot: None,
            has_bot_command: false,
            photo_file_id: None,
            media_group_id: None,
            voice_file_id: None,
            voice_mime: None,
        }
    }

    #[test]
    fn neutralize_raw_sender_scrubs_bare_numeric_id_only() {
        // A bare Telegram user id is never a person — scrub it to the neutral label.
        assert_eq!(neutralize_raw_sender("8905220378"), "family member");
        assert_eq!(neutralize_raw_sender("  8905220378 "), "family member");
        // A real @username / display name is left exactly as written.
        assert_eq!(neutralize_raw_sender("lucapinello"), "lucapinello");
        assert_eq!(neutralize_raw_sender("Luca"), "Luca");
        assert_eq!(neutralize_raw_sender(""), "");
    }

    #[test]
    fn resolve_feed_sender_maps_numeric_id_to_bound_name() {
        // The exact live failure (task mirrored-telegram-sender): Luca is bound by his
        // numeric id and arrives with NO public @username, so the listener decodes his
        // sender to the raw id "8905220378". The mirrored feed line must show "Luca".
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        seed_unconfirmed_binding(dir, "8905220378", "human-luca", "Luca");
        let msg = feed_msg("8905220378", Some("8905220378"));
        assert_eq!(resolve_feed_sender(dir, &msg), "Luca");
    }

    #[test]
    fn resolve_feed_sender_neutralizes_unbound_raw_id() {
        // No binding claims this id → never leak the number; show the neutral label.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let msg = feed_msg("5550001111", Some("5550001111"));
        assert_eq!(resolve_feed_sender(dir, &msg), "family member");
    }

    #[test]
    fn resolve_feed_sender_passes_through_unbound_username() {
        // An unbound sender that DID surface a real @username keeps that handle —
        // only a bare numeric id is neutralized.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let msg = feed_msg("someone", None);
        assert_eq!(resolve_feed_sender(dir, &msg), "someone");
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
    fn machine_routing_resolves_only_exact_unique_canonical_agent_ids() {
        let mut bots = HashMap::new();
        bots.insert(
            "wire-a7".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-a7".to_string()),
                username: Some("mutable_relay_handle_bot".to_string()),
            },
        );
        bots.insert(
            "fallback-b4".to_string(),
            TelegramBotConfig {
                bot_token: "222:BBB".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: None,
                username: None,
            },
        );
        bots.insert(
            "fallback-c9".to_string(),
            TelegramBotConfig {
                bot_token: "333:CCC".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("   ".to_string()),
                username: None,
            },
        );
        bots.insert(
            "invalid-d2".to_string(),
            TelegramBotConfig {
                bot_token: "444:DDD".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some(" relay-d2 ".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let explicit = resolve_machine_bot("relay-a7", &config).unwrap();
        assert_eq!(explicit.bot_id, "wire-a7");
        assert_eq!(explicit.agent_id.as_deref(), Some("relay-a7"));
        for non_identity in [
            "wire-a7",
            "mutable_relay_handle_bot",
            "@mutable_relay_handle_bot",
            "Relay-A7",
        ] {
            assert_eq!(
                resolve_machine_bot(non_identity, &config),
                None,
                "{non_identity:?} is not the exact canonical machine id",
            );
        }
        for invalid_binding in ["invalid-d2", "relay-d2", " relay-d2 "] {
            assert_eq!(
                resolve_machine_bot(invalid_binding, &config),
                None,
                "a padded nonblank binding has no safe canonical machine identity",
            );
        }

        for fallback in ["fallback-b4", "fallback-c9"] {
            let resolved = resolve_machine_bot(fallback, &config).unwrap();
            assert_eq!(resolved.bot_id, fallback);
            assert_eq!(resolved.agent_id.as_deref(), Some(fallback));
        }

        let mut key_shadow = config.clone();
        key_shadow.bots.insert(
            "wire-d2".to_string(),
            TelegramBotConfig {
                bot_token: "444:DDD".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("wire-a7".to_string()),
                username: None,
            },
        );
        let explicit_cross_claim = resolve_machine_bot("wire-a7", &key_shadow).unwrap();
        assert_eq!(
            explicit_cross_claim.bot_id, "wire-d2",
            "an explicit binding must win; another bot's shadowed table key is not identity",
        );

        let mut fallback_cross_claim = config.clone();
        fallback_cross_claim.bots.insert(
            "relay-a7".to_string(),
            TelegramBotConfig {
                bot_token: "555:EEE".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: None,
                username: None,
            },
        );
        assert_eq!(
            resolve_machine_bot("relay-a7", &fallback_cross_claim),
            None,
            "an explicit binding and a fallback table key must not cross-claim one id",
        );

        let mut duplicate = config;
        duplicate.bots.insert(
            "wire-e5".to_string(),
            TelegramBotConfig {
                bot_token: "666:FFF".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-a7".to_string()),
                username: None,
            },
        );
        assert_eq!(
            resolve_machine_bot("relay-a7", &duplicate),
            None,
            "duplicate explicit bindings must fail closed",
        );
    }

    #[test]
    fn clarification_voice_is_exact_and_preserves_the_original_reply_chain() {
        use worksgood::notify::telegram_group::AddressedBy;

        let mut bots = HashMap::new();
        bots.insert(
            "wire-a7".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-a7".to_string()),
                username: Some("mutable_relay_handle_bot".to_string()),
            },
        );
        bots.insert(
            "fallback-b4".to_string(),
            TelegramBotConfig {
                bot_token: "222:BBB".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: None,
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        let exchange = |voice: &str| ownership::ClarifyExchange {
            ts: 1_000,
            chat_id: "-100777".to_string(),
            human: "human-a7".to_string(),
            voice: voice.to_string(),
            original_ask: "could somebody help plan the weekend?".to_string(),
        };

        match bind_clarify_exchange(&exchange("relay-a7"), &config, "-100777").unwrap() {
            Election::One {
                bot,
                reply_chat,
                body,
                addressed_by,
            } => {
                assert_eq!(bot.bot_id, "wire-a7");
                assert_eq!(bot.agent_id.as_deref(), Some("relay-a7"));
                assert_eq!(reply_chat, "-100777");
                assert_eq!(body, "could somebody help plan the weekend?");
                assert_eq!(addressed_by, AddressedBy::ReplyChain);
            }
            other => panic!("valid persisted voice must continue as one reply chain: {other:?}"),
        }

        let fallback = bind_clarify_exchange(&exchange("fallback-b4"), &config, "-100777").unwrap();
        match fallback {
            Election::One { bot, .. } => {
                assert_eq!(bot.bot_id, "fallback-b4");
                assert_eq!(bot.agent_id.as_deref(), Some("fallback-b4"));
            }
            other => panic!("valid key fallback must continue: {other:?}"),
        }

        for invalid in [
            "wire-a7",
            "mutable_relay_handle_bot",
            "@mutable_relay_handle_bot",
            "relay",
            "removed-z9",
        ] {
            assert!(
                bind_clarify_exchange(&exchange(invalid), &config, "-100777").is_err(),
                "persisted alias/handle/prefix/unknown voice {invalid:?} must fail closed",
            );
        }

        let mut key_cross_claim = config.clone();
        key_cross_claim.bots.insert(
            "wire-c9".to_string(),
            TelegramBotConfig {
                bot_token: "333:CCC".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("wire-a7".to_string()),
                username: None,
            },
        );
        match bind_clarify_exchange(&exchange("wire-a7"), &key_cross_claim, "-100777").unwrap() {
            Election::One { bot, .. } => assert_eq!(
                bot.bot_id, "wire-c9",
                "explicit agent binding wins over another bot's shadowed table key",
            ),
            other => panic!("explicit cross-claim must resolve uniquely: {other:?}"),
        }

        let mut duplicate = config.clone();
        duplicate.bots.insert(
            "wire-d2".to_string(),
            TelegramBotConfig {
                bot_token: "444:DDD".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-a7".to_string()),
                username: None,
            },
        );
        assert!(
            bind_clarify_exchange(&exchange("relay-a7"), &duplicate, "-100777").is_err(),
            "a duplicate persisted voice binding must fail closed",
        );

        let mut removed = config;
        removed.bots.remove("wire-a7");
        assert!(
            bind_clarify_exchange(&exchange("relay-a7"), &removed, "-100777").is_err(),
            "a formerly valid but removed voice must not fall through to fresh routing",
        );
    }

    /// OWNER PIN (task owner-pin-engine): the gateway drops the pinned domain
    /// owner's crisp ack and forwards the same canonical id, so the async reply
    /// must come back in the SAME voice — not a re-election winner.
    #[test]
    fn owner_pin_binds_exact_machine_identity_and_rejects_invalid_pins() {
        use worksgood::notify::telegram_group::AddressedBy;

        let mut bots = HashMap::new();
        for (bot_id, agent_id, token, username) in [
            (
                "wire-a7",
                Some("relay-a7"),
                "111:AAA",
                Some("mutable_a7_bot"),
            ),
            (
                "wire-b4",
                Some("relay-b4"),
                "222:BBB",
                Some("mutable_b4_bot"),
            ),
            ("fallback-c9", None, "333:CCC", None),
        ] {
            bots.insert(
                bot_id.to_string(),
                TelegramBotConfig {
                    bot_token: token.to_string(),
                    chat_id: "-100777".to_string(),
                    agent_id: agent_id.map(str::to_string),
                    username: username.map(str::to_string),
                },
            );
        }
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        let elected = Election::One {
            bot: resolve_machine_bot("relay-a7", &config).unwrap(),
            reply_chat: "-100777".to_string(),
            body: "how many calories are in tonight's pasta?".to_string(),
            addressed_by: AddressedBy::ReplyChain,
        };

        let pin = resolve_owner_pin(Some("relay-b4"), &config).unwrap();
        let pinned = apply_owner_pin(
            elected.clone(),
            pin.as_ref(),
            "-100777",
            "how many calories are in tonight's pasta?",
        );
        match &pinned {
            Election::One {
                bot,
                body,
                reply_chat,
                ..
            } => {
                assert_eq!(bot.bot_id, "wire-b4");
                assert_eq!(bot.agent_id.as_deref(), Some("relay-b4"));
                assert_eq!(reply_chat, "-100777");
                assert_eq!(body, "how many calories are in tonight's pasta?");
            }
            other => panic!("pin must yield Election::One, got {other:?}"),
        }

        let no_pin = resolve_owner_pin(None, &config).unwrap();
        assert_eq!(
            apply_owner_pin(elected.clone(), no_pin.as_ref(), "-100777", "m"),
            elected,
            "an absent pin is the only no-op",
        );
        for invalid in [
            "",
            "  ",
            "not-configured",
            "wire-b4",
            "mutable_b4_bot",
            "@mutable_b4_bot",
            "Relay-B4",
            " relay-b4 ",
        ] {
            assert!(
                resolve_owner_pin(Some(invalid), &config).is_err(),
                "supplied invalid pin {invalid:?} must fail closed",
            );
        }
        let mut ambiguous = config.clone();
        ambiguous.bots.insert(
            "wire-d2".to_string(),
            TelegramBotConfig {
                bot_token: "444:DDD".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-b4".to_string()),
                username: None,
            },
        );
        assert!(
            resolve_owner_pin(Some("relay-b4"), &ambiguous).is_err(),
            "a duplicate owner-pin binding must fail closed",
        );

        let fallback = resolve_owner_pin(Some("fallback-c9"), &config)
            .unwrap()
            .unwrap();
        assert_eq!(fallback.bot_id, "fallback-c9");
        assert_eq!(fallback.agent_id.as_deref(), Some("fallback-c9"));

        // A pinned turn the engine would have stayed silent on is still forced
        // to answer in the pinned voice with the raw message as the body.
        let silent = Election::Silence(worksgood::notify::telegram_group::SilenceReason::SmallTalk);
        let forced = apply_owner_pin(silent, Some(&fallback), "-100777", "tell me the calories");
        match &forced {
            Election::One { bot, body, .. } => {
                assert_eq!(bot.bot_id, "fallback-c9");
                assert_eq!(
                    body, "tell me the calories",
                    "raw message becomes the body on a forced pin"
                );
            }
            other => panic!("pin over silence must force a One election, got {other:?}"),
        }
    }

    #[test]
    fn web_default_contact_is_fail_closed_and_only_rebinds_general_elections() {
        use worksgood::notify::ownership::Domain;
        use worksgood::notify::telegram_group::{AddressedBy, SilenceReason};

        let mut bots = HashMap::new();
        for (bot_id, agent_id, token) in [
            ("wire-a7", "relay-a7", "111:AAA"),
            ("wire-b4", "relay-b4", "222:BBB"),
            ("wire-c9", "relay-c9", "333:CCC"),
        ] {
            bots.insert(
                bot_id.to_string(),
                TelegramBotConfig {
                    bot_token: token.to_string(),
                    chat_id: "-100777".to_string(),
                    agent_id: Some(agent_id.to_string()),
                    username: Some(format!("{bot_id}_house_bot")),
                },
            );
        }
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        let one = |id: &str, addressed_by| Election::One {
            bot: resolve_mentioned_bot(id, &config).unwrap(),
            reply_chat: "-100777".to_string(),
            body: "could somebody help with this?".to_string(),
            addressed_by,
        };

        // Non-vacuity: the engine-local concierge election points at relay-a7,
        // while the gateway explicitly designates a different opaque id.
        let general = one("relay-a7", AddressedBy::Concierge);
        let rebound = apply_web_default_owner(
            general.clone(),
            WebDefaultOwner::Designated("relay-b4"),
            &config,
            "-100777",
            "could somebody help with this?",
        );
        match rebound {
            WebDefaultOwnerResolution::Routed(Election::One {
                bot,
                addressed_by,
                body,
                ..
            }) => {
                assert_eq!(bot.agent_id.as_deref(), Some("relay-b4"));
                assert_eq!(addressed_by, AddressedBy::Concierge);
                assert_eq!(body, "could somebody help with this?");
            }
            other => panic!("designated contact must rebind a general ask: {other:?}"),
        }
        match apply_web_default_owner(
            Election::Silence(SilenceReason::NoVoicesConfigured),
            WebDefaultOwner::Designated("relay-b4"),
            &config,
            "-100777",
            "could somebody help with this?",
        ) {
            WebDefaultOwnerResolution::Routed(Election::One { bot, body, .. }) => {
                assert_eq!(bot.agent_id.as_deref(), Some("relay-b4"));
                assert_eq!(body, "could somebody help with this?");
            }
            other => {
                panic!("an explicit contact must recover a general no-voice election: {other:?}")
            }
        }

        for (choice, expected) in [
            (
                WebDefaultOwner::NoneDesignated,
                NeedsContactReason::NoDefaultOwner,
            ),
            (
                WebDefaultOwner::Designated("not-in-config"),
                NeedsContactReason::UnknownDefaultOwner,
            ),
            (
                // A mutable Telegram handle is not the opaque persona id.
                WebDefaultOwner::Designated("wire-b4_house_bot"),
                NeedsContactReason::UnknownDefaultOwner,
            ),
            (
                // An explicit binding supersedes the free-form bot-table key.
                WebDefaultOwner::Designated("wire-b4"),
                NeedsContactReason::UnknownDefaultOwner,
            ),
            (
                // Machine ids are exact, unlike human-authored mentions.
                WebDefaultOwner::Designated("Relay-B4"),
                NeedsContactReason::UnknownDefaultOwner,
            ),
            (
                // Surrounding whitespace is not normalized into machine identity.
                WebDefaultOwner::Designated(" relay-b4 "),
                NeedsContactReason::UnknownDefaultOwner,
            ),
        ] {
            assert_eq!(
                apply_web_default_owner(
                    general.clone(),
                    choice,
                    &config,
                    "-100777",
                    "could somebody help with this?",
                ),
                WebDefaultOwnerResolution::NeedsContact(expected),
            );
            // A missing engine-local concierge is the same general-election
            // seam: an unknown/no designation must not invent a responder.
            assert_eq!(
                apply_web_default_owner(
                    Election::Silence(SilenceReason::NoVoicesConfigured),
                    choice,
                    &config,
                    "-100777",
                    "could somebody help with this?",
                ),
                WebDefaultOwnerResolution::NeedsContact(expected),
            );
        }

        let mut ambiguous = config.clone();
        ambiguous.bots.insert(
            "second-binding".to_string(),
            TelegramBotConfig {
                bot_token: "444:DDD".to_string(),
                chat_id: "-100777".to_string(),
                agent_id: Some("relay-b4".to_string()),
                username: None,
            },
        );
        assert_eq!(
            apply_web_default_owner(
                general.clone(),
                WebDefaultOwner::Designated("relay-b4"),
                &ambiguous,
                "-100777",
                "could somebody help with this?",
            ),
            WebDefaultOwnerResolution::NeedsContact(NeedsContactReason::UnknownDefaultOwner),
            "an opaque id claimed by two bot bindings must fail closed",
        );

        // Stronger election signals must remain byte-for-byte unchanged even
        // when there is no usable default contact.
        for election in [
            one("relay-a7", AddressedBy::Mention),
            one("relay-a7", AddressedBy::Name),
            one("relay-a7", AddressedBy::ReplyChain),
            one("relay-a7", AddressedBy::Domain(Domain::Cooking)),
            Election::All {
                reply_chat: "-100777".to_string(),
                body: "what do you all think?".to_string(),
            },
            Election::Silence(SilenceReason::SmallTalk),
        ] {
            for choice in [
                WebDefaultOwner::NoneDesignated,
                WebDefaultOwner::Designated("not-in-config"),
            ] {
                assert_eq!(
                    apply_web_default_owner(
                        election.clone(),
                        choice,
                        &config,
                        "-100777",
                        "raw message",
                    ),
                    WebDefaultOwnerResolution::Routed(election.clone()),
                    "stronger election {election:?} must ignore {choice:?}",
                );
            }
        }

        // A configured owner pin runs first and becomes a reply-continuity
        // election, so the no-default declaration cannot erase it.
        let owner_pin = resolve_owner_pin(Some("relay-c9"), &config).unwrap();
        let pinned = apply_owner_pin(
            general,
            owner_pin.as_ref(),
            "-100777",
            "could somebody help with this?",
        );
        match apply_web_default_owner(
            pinned,
            WebDefaultOwner::NoneDesignated,
            &config,
            "-100777",
            "could somebody help with this?",
        ) {
            WebDefaultOwnerResolution::Routed(Election::One {
                bot, addressed_by, ..
            }) => {
                assert_eq!(bot.agent_id.as_deref(), Some("relay-c9"));
                assert_eq!(addressed_by, AddressedBy::ReplyChain);
            }
            other => panic!("positive owner pin must outrank no-default: {other:?}"),
        }
    }

    /// date-reminder-fail: the DM seam must CANCEL on a cancel phrase, answer a
    /// "remind me what …" question without filing anything, and never let either
    /// shape reach the registration path.
    #[test]
    fn dm_reminder_seam_cancels_reads_and_never_double_files() {
        use worksgood::notify::reminder::AdHocStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wg = root.join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "garden-relay"
name = "Garden Relay"
domains = ["coordination"]
"#,
        )
        .unwrap();
        seed_confirmed_binding(&wg, "7001001", "member-map", "Household Member");
        let now =
            chrono::NaiveDateTime::parse_from_str("2026-07-12T10:00", "%Y-%m-%dT%H:%M").unwrap();

        // One real reminder is filed.
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "household-handle",
                "remind me Thursday at 7pm to defrost the trout",
                now,
            )
            .is_some()
        );
        assert_eq!(AdHocStore::load(&AdHocStore::path(root)).reminders.len(), 1);

        // (a) A memory question files nothing.
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "household-handle",
                "Remind me what was in Monday's risotto",
                now,
            )
            .is_none()
        );
        assert_eq!(
            AdHocStore::load(&AdHocStore::path(root)).reminders.len(),
            1,
            "a read must not add a reminder"
        );

        // (c) The cancel phrase cancels — and never registers a second reminder.
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "household-handle",
                "cancel the reminder about the trout",
                now,
            )
            .is_none()
        );
        let confirmation = try_cancel_reminder(
            &wg,
            "7001001",
            "household-handle",
            "cancel the reminder about the trout",
            now,
        )
        .expect("the cancel is honoured");
        assert!(
            confirmation.to_lowercase().contains("trout"),
            "the confirmation names what went: {confirmation}"
        );
        assert!(
            AdHocStore::load(&AdHocStore::path(root))
                .reminders
                .is_empty(),
            "the pending reminder should be gone"
        );

        // A cancel that matches nothing leaves the turn to the composer.
        assert!(
            try_cancel_reminder(
                &wg,
                "7001001",
                "household-handle",
                "cancel the reminder about the trout",
                now,
            )
            .is_none()
        );
    }

    /// cross-surface-reminder: this lane sees only the ad-hoc store, so "the one
    /// match" it acts on may be one of TWO live reminders — the other being a
    /// `⏰ Reminder` row in the week plan. It used to delete its own and confirm,
    /// leaving the family a reminder they thought they had cancelled (and the fast
    /// lane a plan row to remove or ask about, with its ad-hoc twin already gone).
    /// Ambiguity is now decided across both surfaces before either is touched.
    #[test]
    fn a_dm_cancel_stands_down_when_the_plan_holds_a_match_too() {
        use worksgood::notify::reminder::AdHocStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wg = root.join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::create_dir_all(root.join("plans")).unwrap();
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "garden-relay"
name = "Garden Relay"
domains = ["coordination"]
"#,
        )
        .unwrap();
        seed_confirmed_binding(&wg, "7001001", "member-map", "Household Member");

        // Sunday 2026-07-12; the plan week that follows carries its OWN pending
        // dentist reminder on the Thursday.
        std::fs::write(
            root.join("plans").join("2026-W29-family-plan.md"),
            "# Family plan — 2026-W29\n\n\
             **Week of Monday 2026-07-13 → Sunday 2026-07-19**\n\n\
             ## 3. Calendar (Otto) — combined projection\n\n\
             | Day | Time | Event | Source |\n\
             |-----|------|-------|--------|\n\
             | Thu 07-16 | 09:00 | ⏰ Reminder: book the dentist | Otto |\n",
        )
        .unwrap();

        let now =
            chrono::NaiveDateTime::parse_from_str("2026-07-12T10:00", "%Y-%m-%dT%H:%M").unwrap();
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "household-handle",
                "remind me Thursday at 9am to call the dentist",
                now,
            )
            .is_some()
        );
        let before = std::fs::read(AdHocStore::path(root)).unwrap();

        assert!(
            try_cancel_reminder(
                &wg,
                "7001001",
                "household-handle",
                "cancel the reminder about the dentist",
                now,
            )
            .is_none(),
            "two live candidates across the two surfaces must not be resolved here"
        );
        assert_eq!(
            std::fs::read(AdHocStore::path(root)).unwrap(),
            before,
            "the ad-hoc reminders must be byte-identical when the lane stands down"
        );

        // CONTROL: with the plan's own row gone, the same words are unambiguous
        // again and this lane does act.
        std::fs::write(
            root.join("plans").join("2026-W29-family-plan.md"),
            "# Family plan — 2026-W29\n\n\
             **Week of Monday 2026-07-13 → Sunday 2026-07-19**\n\n\
             ## 3. Calendar (Otto) — combined projection\n\n\
             | Day | Time | Event | Source |\n\
             |-----|------|-------|--------|\n\
             | Thu 07-16 | 18:30 | Cook: chickpea & spinach curry | Bruno |\n",
        )
        .unwrap();
        let confirmation = try_cancel_reminder(
            &wg,
            "7001001",
            "household-handle",
            "cancel the reminder about the dentist",
            now,
        )
        .expect("the sole remaining candidate is cancelled");
        assert!(
            confirmation.to_lowercase().contains("dentist"),
            "the confirmation names what went: {confirmation}"
        );
        assert!(
            AdHocStore::load(&AdHocStore::path(root))
                .reminders
                .is_empty(),
            "the ad-hoc reminder should be gone once it is the only candidate"
        );
    }
    /// reminder-readback-lane: the DM seam must READ a filed reminder back from
    /// what is on disk — with the date the STORE holds, never the one the
    /// question asserts — scope it to the person asking, and write nothing.
    #[test]
    fn dm_reminder_readback_answers_from_the_store_and_never_across_members() {
        use worksgood::notify::reminder::AdHocStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wg = root.join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "garden-relay"
name = "Garden Relay"
domains = ["coordination"]
"#,
        )
        .unwrap();
        seed_confirmed_binding(&wg, "7001001", "member-one", "Household Member");
        seed_confirmed_binding(&wg, "7001002", "member-two", "Second Member");

        // Friday morning; the reminder is filed for the UPCOMING Monday 09:00.
        let now =
            chrono::NaiveDateTime::parse_from_str("2026-07-24T10:00", "%Y-%m-%dT%H:%M").unwrap();
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "member-one-handle",
                "remind me Monday at 9am to call the dentist",
                now,
            )
            .is_some()
        );
        let filed = AdHocStore::load(&AdHocStore::path(root));
        assert_eq!(filed.reminders.len(), 1);
        assert_eq!(
            filed.reminders[0].due.format("%Y-%m-%dT%H:%M").to_string(),
            "2026-07-27T09:00",
            "the fixture must really hold Jul 27 for the poisoned question to mean anything"
        );
        let before = std::fs::read(AdHocStore::path(root)).unwrap();

        // THE POISONED QUESTION: it asserts Aug 3; the store says Jul 27.
        let answer = try_reminder_readback(
            &wg,
            "7001001",
            "member-one-handle",
            "What exact date and time is the reminder to call the dentist set for — \
             Monday, August 3, 2026 at 9:00 a.m.?",
            now,
        )
        .expect("the read-back lane owns this turn");
        assert!(
            answer.contains("Jul 27") && answer.contains("9:00 am"),
            "the answer must carry the PERSISTED date and time: {answer}"
        );
        assert!(
            !answer.contains("Aug 3") && !answer.contains("August 3"),
            "the answer must not echo the date the question asserted: {answer}"
        );
        assert!(
            answer.to_lowercase().contains("call the dentist"),
            "the answer names what the reminder is about: {answer}"
        );

        // ANOTHER MEMBER learns nothing — not the date, not that it exists.
        let other = try_reminder_readback(
            &wg,
            "7001002",
            "member-two-handle",
            "What date and time is the reminder to call the dentist set for?",
            now,
        )
        .expect("the lane still owns the turn");
        assert_eq!(other, "You don't have a reminder set about that.");
        for leak in ["Jul 27", "9:00", "Household Member"] {
            assert!(
                !other.contains(leak),
                "leaked {leak:?} across members: {other}"
            );
        }

        // A QUESTION carrying the cancel verb is answered, not executed — the
        // read-back runs ahead of the cancel lane precisely for this shape.
        let asked = try_reminder_readback(
            &wg,
            "7001001",
            "member-one-handle",
            "Did you cancel my dentist reminder?",
            now,
        )
        .expect("a question about a cancellation is a read");
        assert!(asked.contains("Jul 27"), "{asked}");

        // A reminder WRITE is never stolen by the read lane.
        assert!(
            try_reminder_readback(
                &wg,
                "7001001",
                "member-one-handle",
                "remind me Tuesday at 8am to set out the bins",
                now,
            )
            .is_none()
        );

        // ZERO WRITES: the store is byte-identical after every read.
        assert_eq!(
            before,
            std::fs::read(AdHocStore::path(root)).unwrap(),
            "a read-back must not touch the reminder file"
        );
    }

    #[test]
    fn proactive_owner_hint_is_empty_without_a_valid_roster() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(crate::casa::digest::coordination_owner_hint(dir.path()), "");

        std::fs::write(dir.path().join("household.toml"), "agent = [").unwrap();
        assert_eq!(
            crate::casa::digest::coordination_owner_hint(dir.path()),
            "",
            "malformed configuration must not manufacture a persona id",
        );
    }

    #[test]
    fn operator_alert_route_refuses_unproven_coordination_dm() {
        use worksgood::notify::lifecycle::OperatorAlert;

        fn config_with_order(reverse: bool) -> TelegramConfig {
            let entries = [
                (
                    "fallback-wire",
                    TelegramBotConfig {
                        bot_token: "100:AAA".to_string(),
                        chat_id: "7001".to_string(),
                        agent_id: Some("pantry-orbit".to_string()),
                        username: None,
                    },
                ),
                (
                    "coordination-wire",
                    TelegramBotConfig {
                        bot_token: "200:BBB".to_string(),
                        chat_id: "7002".to_string(),
                        agent_id: Some("night-orbit".to_string()),
                        username: None,
                    },
                ),
            ];
            let mut bots = HashMap::new();
            let order: &[usize] = if reverse { &[1, 0] } else { &[0, 1] };
            for index in order {
                let (id, bot) = &entries[*index];
                bots.insert((*id).to_string(), bot.clone());
            }
            TelegramConfig {
                bot_token: String::new(),
                chat_id: String::new(),
                bots,
            }
        }

        let alert = OperatorAlert {
            task_id: "stalled-porch-light".to_string(),
            requester: "Household Member".to_string(),
            text: "The porch-light request needs a look.".to_string(),
            notification_id: "alert-stalled-porch-light".to_string(),
        };
        for reverse in [false, true] {
            let config = config_with_order(reverse);
            assert_eq!(
                crate::casa::lifecycle::operator_alert_route(&config, Some("night-orbit")),
                None,
                "coordination ownership does not prove who owns a helper bot's private chat",
            );
            let line = worksgood::notify::lifecycle::dry_run_alert_line(&alert, None);
            assert!(
                line.contains("logged only"),
                "dry-run must expose the fail-closed route: {line}",
            );
        }
    }

    #[test]
    fn operator_alert_route_falls_back_honestly_without_a_valid_owner() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = TelegramConfig {
            bot_token: "300:CCC".to_string(),
            chat_id: "7003".to_string(),
            bots: HashMap::new(),
        };
        for invalid_household in [None, Some("agent = [")] {
            if let Some(body) = invalid_household {
                std::fs::write(dir.path().join("household.toml"), body).unwrap();
            }
            let owner_map = ownership::OwnerMap::load(dir.path());
            let owner = owner_map.owner_for_domain(ownership::Domain::Coordination);
            assert_eq!(owner, None);
            assert_eq!(
                crate::casa::lifecycle::operator_alert_route(&legacy, owner),
                Some((String::new(), "7003".to_string())),
                "missing or malformed ownership must retain the explicit legacy route",
            );
        }

        let mut bots = HashMap::new();
        bots.insert(
            "only-configured-wire".to_string(),
            TelegramBotConfig {
                bot_token: "400:DDD".to_string(),
                chat_id: "7004".to_string(),
                agent_id: Some("unowned-orbit".to_string()),
                username: None,
            },
        );
        let one_bot = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        assert_eq!(
            crate::casa::lifecycle::operator_alert_route(&one_bot, None),
            None,
            "an arbitrary private member chat is not an owner-alert fallback",
        );

        assert_eq!(
            crate::casa::lifecycle::operator_alert_route(&TelegramConfig::default(), None),
            None
        );
        let alert = worksgood::notify::lifecycle::OperatorAlert {
            task_id: "stalled-entry-key".to_string(),
            requester: "Household Member".to_string(),
            text: "The entry-key request needs a look.".to_string(),
            notification_id: "alert-stalled-entry-key".to_string(),
        };
        let line = worksgood::notify::lifecycle::dry_run_alert_line(&alert, None);
        assert!(
            line.contains("no configured bot") && line.contains("logged only"),
            "an unavailable route must be visible in dry-run output: {line}",
        );
    }

    #[test]
    fn operator_alert_route_refuses_group_chat_ids() {
        use worksgood::notify::lifecycle::OperatorAlert;

        let mut bots = HashMap::new();
        bots.insert(
            "coordination-wire".to_string(),
            TelegramBotConfig {
                bot_token: "200:BBB".to_string(),
                chat_id: "-1007002".to_string(),
                agent_id: Some("configured-owner".to_string()),
                username: None,
            },
        );
        bots.insert(
            "fallback-wire".to_string(),
            TelegramBotConfig {
                bot_token: "100:AAA".to_string(),
                chat_id: "-1007001".to_string(),
                agent_id: Some("other-member".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: "300:CCC".to_string(),
            chat_id: "-1007003".to_string(),
            bots,
        };

        assert_eq!(
            crate::casa::lifecycle::operator_alert_route(&config, Some("configured-owner")),
            None,
            "normal negative family-group ids must never masquerade as an owner DM",
        );

        let alert = OperatorAlert {
            task_id: "stalled-porch-light".to_string(),
            requester: "Household Member".to_string(),
            text: "The porch-light request needs a look.".to_string(),
            notification_id: "alert-stalled-porch-light".to_string(),
        };
        let sink = RecordingSink::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(
            !rt.block_on(crate::casa::lifecycle::deliver_operator_alert(
                &sink,
                &config,
                Some("configured-owner"),
                &alert,
            )),
            "group-only configuration must retain the alert as a loud log record",
        );
        assert!(
            sink.sends.lock().unwrap().is_empty(),
            "owner-facing task details must never be sent into a family group",
        );

        let unrelated_private = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: HashMap::from([(
                "unrelated-wire".to_string(),
                TelegramBotConfig {
                    bot_token: "400:DDD".to_string(),
                    chat_id: "7004".to_string(),
                    agent_id: Some("other-member".to_string()),
                    username: None,
                },
            )]),
        };
        assert_eq!(
            crate::casa::lifecycle::operator_alert_route(
                &unrelated_private,
                Some("configured-owner")
            ),
            None,
            "a positive private id is not proof that the chat belongs to the owner",
        );

        let line = worksgood::notify::lifecycle::dry_run_alert_line(&alert, None);
        assert!(
            line.contains("logged only"),
            "the dry-run must report the fail-closed route honestly: {line}",
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
    fn collective_request_id_is_unique_per_turn_stable_on_refire() {
        let mut first = gate_msg("supergroup", false);
        first.channel = "telegram:voice-7".to_string();
        first.sender = "member-4".to_string();
        first.sender_id = Some("member-id-4".to_string());
        first.sent_at = Some(1_720_000_000);
        first.body = "hello household".to_string();
        first.message_id = Some("41".to_string());
        first.chat_id = Some("-100700".to_string());

        let first_turn = telegram_physical_turn_key(&first);
        // The same physical group message arrives through another bot with a
        // different channel and message_id. Those transport-local fields must
        // not split the shared turn key.
        let mut cross_bot_refire = first.clone();
        cross_bot_refire.channel = "telegram:voice-9".to_string();
        cross_bot_refire.message_id = Some("907".to_string());
        let first_refire = telegram_physical_turn_key(&cross_bot_refire);
        assert_eq!(
            first_turn, first_refire,
            "cross-bot Telegram deliveries must retain one physical-turn key",
        );

        let mut later = first.clone();
        later.message_id = Some("42".to_string());
        later.sent_at = Some(1_720_000_001);
        let later_turn = telegram_physical_turn_key(&later);
        assert_ne!(
            first_turn, later_turn,
            "a later same-body Telegram turn must not reuse the earlier key",
        );

        let first_id = crate::casa::group::collective_request_id("-100700", "voice-7", &first_turn);
        let refire_id =
            crate::casa::group::collective_request_id("-100700", "voice-7", &first_refire);
        let later_id = crate::casa::group::collective_request_id("-100700", "voice-7", &later_turn);
        assert_eq!(
            first_id, refire_id,
            "the outbox guard must dedupe a true refire",
        );
        assert_ne!(
            first_id, later_id,
            "the outbox guard must admit the later household turn",
        );
        assert_ne!(
            first_id,
            crate::casa::group::collective_request_id("-100700", "voice-8", &first_turn),
            "each configured roster voice needs its own request id",
        );

        // A transport that omits message_id uses the same key because message_id
        // never participates in the physical fingerprint.
        let mut fallback = first.clone();
        fallback.message_id = None;
        let fallback_key = telegram_physical_turn_key(&fallback);
        assert_eq!(
            first_turn, fallback_key,
            "message_id presence must not affect the cross-bot key",
        );

        // Web collective callers share the explicit occurrence id across voices.
        let web_first = crate::casa::plan_edits::web_physical_turn_key(
            "-100700",
            "hello household",
            Some("turn-fixture-a"),
            None,
        );
        assert_eq!(
            web_first,
            crate::casa::plan_edits::web_physical_turn_key(
                "-100700",
                "body changes do not matter on a true refire",
                Some("turn-fixture-a"),
                None,
            ),
            "web refires retain the explicit occurrence fingerprint",
        );
        assert_ne!(
            web_first,
            crate::casa::plan_edits::web_physical_turn_key(
                "-100700",
                "hello household",
                Some("turn-fixture-b"),
                None
            ),
            "a later web occurrence gets a fresh collective key even with identical words",
        );
    }

    #[test]
    fn durable_turn_ids_have_stable_vectors() {
        let mut message = gate_msg("supergroup", false);
        message.channel = "telegram:voice-7".to_string();
        message.sender = "member-display".to_string();
        message.sender_id = Some("member-id-4".to_string());
        message.sent_at = Some(1_720_000_000);
        message.body = "hello household".to_string();
        message.message_id = Some("transport-local-41".to_string());
        message.chat_id = Some("-100700".to_string());

        let telegram_turn = telegram_physical_turn_key(&message);
        assert_eq!(
            telegram_turn,
            "telegram-turn-b3-v1-5c81580a6d5538a35b1233961550e48f16c2f8633615ecb2501491deb9673a83",
        );

        let web_turn = crate::casa::plan_edits::web_physical_turn_key(
            "-100700",
            "hello household",
            Some("opaque-turn-a7"),
            None,
        );
        assert_eq!(
            web_turn,
            "web-turn-b3-v1-62753b68ec0fbd6e844d7728ecd3ce10560f707a7ef63f394b400dc60eeaa930",
        );
        assert_eq!(
            crate::casa::plan_edits::web_physical_turn_key(
                "-100700",
                "hello household",
                None,
                None
            ),
            "web-turn-b3-v1-6debece65596fe4ed96650f648c8ee482b351c65978c3dfd5bb9652ad7168fab",
        );
        // The attempt-bearing key is its own durable vector: the turn ledger it
        // keys lives on disk across restarts and upgrades, so changing this
        // digest silently orphans every entry a running gateway already wrote.
        assert_eq!(
            crate::casa::plan_edits::web_physical_turn_key(
                "-100700",
                "hello household",
                Some("opaque-turn-a7"),
                Some("opaque-attempt-1"),
            ),
            "web-turn-b3-v1-336cad0496508ff72bb5c0b445d21df2846c2f97588b92cd94a093843e58168f",
        );
        assert_eq!(
            crate::casa::group::collective_request_id("-100700", "voice-7", &telegram_turn),
            "tg-collective-b3-v1-49ba9313a9bf36256388d82b83660fe204196d1aa270cfb2e3add5544b818750",
        );
        assert_eq!(
            web_inbound_request_id("-100700", "voice-7", &web_turn),
            "web-request-b3-v1-96ebd0939919bcb2a67b9baf8c50cb15fcc489edb598f3d5d1085e8e210f5115",
        );
    }

    /// Permanent behavior gate at the mutation boundary. A fresh invocation
    /// retries only the stored delivery after transport failure, a completed
    /// same-turn refire is silent, and a distinct occurrence id admits the same
    /// household words later.
    #[tokio::test]
    async fn web_fast_lane_same_turn_mutates_and_sends_once_after_restart() {
        #[derive(Default)]
        struct ReplaySink {
            fail_next: std::sync::atomic::AtomicBool,
            attempts: std::sync::Mutex<Vec<(String, String, String)>>,
            delivered: std::sync::Mutex<Vec<(String, String, String)>>,
        }

        #[async_trait]
        impl worksgood::notify::telegram_conversation::ReplySink for ReplaySink {
            async fn send(
                &self,
                bot_id: &str,
                chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                let row = (bot_id.to_string(), chat_id.to_string(), text.to_string());
                self.attempts.lock().unwrap().push(row.clone());
                if self
                    .fail_next
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    anyhow::bail!("fixture transport failure");
                }
                let mut delivered = self.delivered.lock().unwrap();
                delivered.push(row);
                Ok(Some(format!("fixture-{}", delivered.len())))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let workgraph_dir = root.join(".wg");
        let plans_dir = root.join("plans");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        std::fs::create_dir_all(&plans_dir).unwrap();
        let plan_path = plans_dir.join("2026-W30-family-plan.md");
        std::fs::write(
            &plan_path,
            r#"# Household weekly plan · 2026-W30

**Week of Monday 2026-07-20 → Sunday 2026-07-26**

## 1. Dinners (Cedar Signal → Copper Ladle)

| Day | Slot type | Dinner | Prep | Note |
|-----|-----------|--------|------|------|
| Wed 07-22 | Vegetarian | Miso aubergine noodles | ~30 min | pantry |

## 2. Calendar (Open Door)

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Wed 07-22 | 18:30 | Cook: miso aubergine noodles | Copper Ladle |

## 3. Shopping list (Open Door)

### Produce
- Aubergines ×2
"#,
        )
        .unwrap();

        let roster = worksgood::notify::grounding::FamilyVoiceRoster::from_names(
            ["Open Door"],
            ["River Guest"],
        );
        let sink = ReplaySink::default();
        sink.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 22).unwrap();
        let words = "add oat milk to the shopping list";
        let first_key =
            crate::casa::plan_edits::web_physical_turn_key("-100700", words, Some("turn-a"), None);

        // First invocation applies once and retains canonical bytes when the
        // transport fails.
        let first = run_web_fast_lane_occurrence(
            &workgraph_dir,
            root,
            words,
            today,
            None,
            &first_key,
            "helper-9",
            "-100700",
            "member-41",
            "helper-9",
            &roster,
            &sink,
        )
        .await;
        assert!(first.is_err());
        assert_eq!(
            std::fs::read_to_string(&plan_path)
                .unwrap()
                .matches("- oat milk")
                .count(),
            1,
        );

        // A fresh-process refire may carry drifted mutable inputs. The persisted
        // outcome still wins even when the body no longer classifies as a fast
        // edit: original route and exact oat-milk reply.
        let retry = run_web_fast_lane_occurrence(
            &workgraph_dir,
            root,
            "please help me with this",
            today,
            None,
            &first_key,
            "decoy-bot",
            "-100999",
            "different-member",
            "decoy-persona",
            &roster,
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(
            retry,
            WebFastLaneDispatch::Handled {
                resumed_delivery: true,
                already_delivered: false,
                ..
            }
        ));
        let plan = std::fs::read_to_string(&plan_path).unwrap();
        assert_eq!(plan.matches("- oat milk").count(), 1);
        let delivered = sink.delivered.lock().unwrap().clone();
        assert_eq!(delivered.len(), 1);
        assert_eq!(&delivered[0].0, "helper-9");
        assert_eq!(&delivered[0].1, "-100700");
        assert!(delivered[0].2.contains("oat milk"));

        // Once delivered, the same occurrence is a full no-op.
        let completed = run_web_fast_lane_occurrence(
            &workgraph_dir,
            root,
            words,
            today,
            None,
            &first_key,
            "helper-9",
            "-100700",
            "member-41",
            "helper-9",
            &roster,
            &sink,
        )
        .await
        .unwrap();
        assert!(matches!(
            completed,
            WebFastLaneDispatch::Handled {
                already_delivered: true,
                ..
            }
        ));
        assert_eq!(sink.attempts.lock().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(&plan_path)
                .unwrap()
                .matches("- oat milk")
                .count(),
            1,
        );

        // Identical words with a later occurrence id remain a new household
        // turn and therefore apply + deliver once of their own.
        let later_key =
            crate::casa::plan_edits::web_physical_turn_key("-100700", words, Some("turn-b"), None);
        run_web_fast_lane_occurrence(
            &workgraph_dir,
            root,
            words,
            today,
            None,
            &later_key,
            "helper-9",
            "-100700",
            "member-41",
            "helper-9",
            &roster,
            &sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.attempts.lock().unwrap().len(), 3);
        assert_eq!(sink.delivered.lock().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(&plan_path)
                .unwrap()
                .matches("- oat milk")
                .count(),
            2,
        );
        let graph = worksgood::parser::load_graph(workgraph_dir.join("graph.jsonl")).unwrap();
        assert_eq!(
            graph
                .tasks()
                .filter(|task| task.tags.iter().any(|tag| tag == "fast-lane"))
                .count(),
            2,
        );
    }

    /// Behavior gate for every conversational group-delivery shape. One
    /// physical Telegram turn is replayed through a fresh invocation, then the
    /// same words arrive one second later as a distinct turn. The replay sends
    /// nothing; the later occurrence sends normally.
    #[tokio::test]
    async fn physical_turn_refire_deduplicates_all_group_reply_modes() {
        use std::time::Duration;
        use worksgood::chat;
        use worksgood::chat_sessions::{SessionKind, create_session};
        use worksgood::graph::OriginChannel;
        use worksgood::notify::grounding::FamilyVoiceRoster;
        use worksgood::notify::telegram_conversation as convo;
        use worksgood::notify::telegram_discussion as discussion;

        #[derive(Default)]
        struct CountingSink {
            sends: std::sync::Mutex<Vec<(String, String, String)>>,
        }
        #[async_trait]
        impl convo::ReplySink for CountingSink {
            async fn send(
                &self,
                bot_id: &str,
                chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                let mut sends = self.sends.lock().unwrap();
                sends.push((bot_id.to_string(), chat_id.to_string(), text.to_string()));
                Ok(Some(format!("stub-{}", sends.len())))
            }
        }

        struct FixedComposer;
        #[async_trait]
        impl convo::ReplyComposer for FixedComposer {
            async fn compose(
                &self,
                _workgraph_dir: &Path,
                _session_ref: &str,
                _agent_id: &str,
                _human_message: &str,
            ) -> Result<String> {
                Ok("That sounds good to me.".to_string())
            }
        }

        async fn append_legacy_reply(
            workgraph_dir: PathBuf,
            session_ref: String,
            request_id: String,
        ) {
            for _ in 0..200 {
                let inbox = chat::read_inbox_ref(&workgraph_dir, &session_ref).unwrap_or_default();
                if inbox.iter().any(|message| message.request_id == request_id) {
                    chat::append_outbox_ref(
                        &workgraph_dir,
                        &session_ref,
                        "The legacy session answered.",
                        &request_id,
                    )
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("legacy fixture never observed request {request_id}");
        }

        let dir = tempfile::tempdir().unwrap();
        let workgraph_dir = dir.path().join(".wg");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let chat_id = "-100700";
        let words = "what does everyone think?";

        let mut first = gate_msg("supergroup", false);
        first.channel = "telegram:wire-1".to_string();
        first.sender = "member-4".to_string();
        first.sender_id = Some("member-id-4".to_string());
        first.sent_at = Some(1_720_000_000);
        first.body = words.to_string();
        first.message_id = Some("41".to_string());
        first.chat_id = Some(chat_id.to_string());
        let mut replay = first.clone();
        replay.channel = "telegram:wire-2".to_string();
        replay.message_id = Some("907".to_string());
        let mut later = first.clone();
        later.sent_at = Some(1_720_000_001);
        later.message_id = Some("42".to_string());

        let first_key = telegram_physical_turn_key(&first);
        let replay_key = telegram_physical_turn_key(&replay);
        let later_key = telegram_physical_turn_key(&later);
        assert_eq!(first_key, replay_key);
        assert_ne!(first_key, later_key);
        let keys = [&first_key, &replay_key, &later_key];

        let sink = CountingSink::default();
        let timing = convo::AckTiming {
            ack_after: Duration::from_secs(1),
            reply_timeout: Duration::from_secs(3),
            poll: Duration::from_millis(5),
        };

        // Collective fallback: this is the direct no-session send seam used by
        // `run_group_collective`.
        for key in keys {
            let request_id =
                crate::casa::group::collective_request_id(chat_id, "wire-fallback", key);
            convo::send_reply_once(
                &workgraph_dir,
                &request_id,
                "wire-fallback",
                chat_id,
                "I am here.",
                &sink,
            )
            .await
            .unwrap();
        }

        // Group sessionless path.
        let sessionless = convo::ConversationPlan::Sessionless {
            agent_id: "persona-sessionless".to_string(),
            route: convo::ReplyRoute {
                bot_id: "wire-sessionless".to_string(),
                chat_id: chat_id.to_string(),
            },
            entry: convo::Entry::GroupElected,
        };
        for key in keys {
            let request_id =
                crate::casa::group::collective_request_id(chat_id, "wire-sessionless", key);
            convo::run_conversation_turn(
                &workgraph_dir,
                &sessionless,
                words,
                &request_id,
                timing,
                None,
                &sink,
            )
            .await
            .unwrap();
        }

        // Legacy session polling path (no injected composer).
        let legacy_session =
            create_session(&workgraph_dir, SessionKind::Interactive, &[], None).unwrap();
        let legacy = convo::ConversationPlan::Converse {
            session_ref: legacy_session.clone(),
            agent_id: "persona-legacy".to_string(),
            route: convo::ReplyRoute {
                bot_id: "wire-legacy".to_string(),
                chat_id: chat_id.to_string(),
            },
            entry: convo::Entry::GroupElected,
            requester: "member-4".to_string(),
            channel: OriginChannel::TelegramGroup,
        };
        let first_legacy_id =
            crate::casa::group::collective_request_id(chat_id, "wire-legacy", &first_key);
        let first_responder = tokio::spawn(append_legacy_reply(
            workgraph_dir.clone(),
            legacy_session.clone(),
            first_legacy_id.clone(),
        ));
        convo::run_conversation_turn(
            &workgraph_dir,
            &legacy,
            words,
            &first_legacy_id,
            timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        first_responder.await.unwrap();
        convo::run_conversation_turn(
            &workgraph_dir,
            &legacy,
            words,
            &crate::casa::group::collective_request_id(chat_id, "wire-legacy", &replay_key),
            timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        let later_legacy_id =
            crate::casa::group::collective_request_id(chat_id, "wire-legacy", &later_key);
        let later_responder = tokio::spawn(append_legacy_reply(
            workgraph_dir.clone(),
            legacy_session,
            later_legacy_id.clone(),
        ));
        convo::run_conversation_turn(
            &workgraph_dir,
            &legacy,
            words,
            &later_legacy_id,
            timing,
            None,
            &sink,
        )
        .await
        .unwrap();
        later_responder.await.unwrap();

        // Real elected single-voice compose/finalize path.
        let single_session =
            create_session(&workgraph_dir, SessionKind::Interactive, &[], None).unwrap();
        let single = convo::ConversationPlan::Converse {
            session_ref: single_session,
            agent_id: "persona-single".to_string(),
            route: convo::ReplyRoute {
                bot_id: "wire-single".to_string(),
                chat_id: chat_id.to_string(),
            },
            entry: convo::Entry::GroupElected,
            requester: "member-4".to_string(),
            channel: OriginChannel::TelegramGroup,
        };
        let composer = FixedComposer;
        for key in keys {
            let request_id = crate::casa::group::collective_request_id(chat_id, "wire-single", key);
            convo::run_conversation_turn(
                &workgraph_dir,
                &single,
                words,
                &request_id,
                timing,
                Some(&composer),
                &sink,
            )
            .await
            .unwrap();
        }

        // Discussion takes plus synthesis each receive a logical sub-key.
        let voices = ["wire-discuss-1", "wire-discuss-2", "wire-discuss-3"]
            .into_iter()
            .map(|bot_id| discussion::DiscussionVoice {
                bot_id: bot_id.to_string(),
                display_name: format!("Voice {bot_id}"),
                agent_id: format!("persona-{bot_id}"),
                session_ref: format!("session-{bot_id}"),
            })
            .collect::<Vec<_>>();
        let family_roster = FamilyVoiceRoster::from_names(
            voices.iter().map(|voice| voice.display_name.as_str()),
            ["Household Member"],
        );
        let discuss_timing = discussion::DiscussionTiming {
            per_voice: Duration::from_secs(2),
            overall: Duration::from_secs(10),
        };
        for key in keys {
            discussion::run_discussion_round(
                &workgraph_dir,
                words,
                &voices,
                "wire-discuss-3",
                &composer,
                &family_roster,
                &sink,
                chat_id,
                key,
                discuss_timing,
            )
            .await
            .unwrap();
        }

        let sends = sink.sends.lock().unwrap();
        let count = |bot_id: &str| {
            sends
                .iter()
                .filter(|(actual, _, _)| actual == bot_id)
                .count()
        };
        for bot_id in [
            "wire-fallback",
            "wire-sessionless",
            "wire-legacy",
            "wire-single",
        ] {
            assert_eq!(
                count(bot_id),
                2,
                "{bot_id}: first and later occurrences send; refire does not",
            );
        }
        assert_eq!(count("wire-discuss-1"), 2);
        assert_eq!(count("wire-discuss-2"), 2);
        assert_eq!(
            count("wire-discuss-3"),
            4,
            "the configured synthesizer sends one take and one synthesis per distinct occurrence",
        );
    }

    /// A SELF-HEAL retry is not a refire.
    ///
    /// The gateway keeps the occurrence id stable across a retry of the same
    /// accepted turn — that is what makes a dispatcher redelivery suppressible.
    /// But the gateway ALSO retries a turn whose first attempt died before the
    /// family got an answer, and under a turn-only key that retry matches the
    /// dead attempt's ledger entry and is dropped as "already answered": the
    /// self-heal heals nothing. The canonical attempt id separates the two —
    /// same `(turn, attempt)` is the same physical delivery, a new attempt on
    /// the same turn is a fresh chance to answer it.
    #[test]
    fn web_turn_key_admits_a_self_heal_retry_and_suppresses_a_true_refire() {
        let chat = "web";
        let voice = "voice-3";
        let words = "start the week";

        let first = crate::casa::plan_edits::web_physical_turn_key(
            chat,
            words,
            Some("turn-a7"),
            Some("attempt-1"),
        );
        assert_eq!(
            first,
            crate::casa::plan_edits::web_physical_turn_key(
                chat,
                words,
                Some("turn-a7"),
                Some("attempt-1")
            ),
            "a true refire — same turn, same attempt — must stay one occurrence",
        );
        let retry = crate::casa::plan_edits::web_physical_turn_key(
            chat,
            words,
            Some("turn-a7"),
            Some("attempt-2"),
        );
        assert_ne!(
            first, retry,
            "a new attempt on the same turn must not be suppressed as already-answered",
        );
        assert_ne!(
            first,
            crate::casa::plan_edits::web_physical_turn_key(
                chat,
                words,
                Some("turn-b9"),
                Some("attempt-1")
            ),
            "a later turn stays distinct even when the attempt counter repeats",
        );

        // An absent — or blank — attempt is the LEGACY key, byte for byte, so an
        // older gateway's live ledger entries keep replaying after this change.
        let legacy =
            crate::casa::plan_edits::web_physical_turn_key(chat, words, Some("turn-a7"), None);
        assert_eq!(
            legacy,
            crate::casa::plan_edits::web_physical_turn_key(
                chat,
                words,
                Some("turn-a7"),
                Some("   ")
            ),
            "a blank attempt id is no attempt id",
        );
        assert_ne!(
            legacy, first,
            "an attempt-bearing turn is its own occurrence, not the legacy one",
        );

        // The fingerprint is still opaque: neither identifier survives into it.
        assert!(
            !first.contains("turn-a7") && !first.contains("attempt-1"),
            "the turn fingerprint leaked an identifier: {first}",
        );

        // …and the request id the one-reply-per-turn guard keys on inherits all
        // of it, which is where the suppression actually happens.
        assert_eq!(
            web_inbound_request_id(chat, voice, &first),
            web_inbound_request_id(
                chat,
                voice,
                &crate::casa::plan_edits::web_physical_turn_key(
                    chat,
                    words,
                    Some("turn-a7"),
                    Some("attempt-1")
                ),
            ),
        );
        assert_ne!(
            web_inbound_request_id(chat, voice, &first),
            web_inbound_request_id(chat, voice, &retry),
            "the retry must reach compose+send instead of matching the dead attempt",
        );
    }

    #[test]
    fn web_inbound_request_id_distinguishes_identical_later_turns() {
        let chat = "-100777";
        let voice = "voice-3";
        let words = "please help with the weekend";
        let first_key = crate::casa::plan_edits::web_physical_turn_key(
            chat,
            words,
            Some("opaque-turn-a7"),
            None,
        );
        let refire_key = crate::casa::plan_edits::web_physical_turn_key(
            chat,
            words,
            Some("opaque-turn-a7"),
            None,
        );
        let later_key = crate::casa::plan_edits::web_physical_turn_key(
            chat,
            words,
            Some("opaque-turn-b9"),
            None,
        );

        let first = web_inbound_request_id(chat, voice, &first_key);
        let refire = web_inbound_request_id(chat, voice, &refire_key);
        let later = web_inbound_request_id(chat, voice, &later_key);
        assert_eq!(
            first, refire,
            "the same explicit turn id must remain stable on dispatcher refire",
        );
        assert_ne!(
            first, later,
            "different occurrence ids must admit later identical words",
        );

        // Missing WG_TURN_ID retains the legacy trimmed-body fallback for an
        // older gateway, including its original refire behavior.
        let fallback = crate::casa::plan_edits::web_physical_turn_key(chat, words, None, None);
        assert_eq!(
            fallback,
            crate::casa::plan_edits::web_physical_turn_key(
                chat,
                "  please help with the weekend  ",
                None,
                None
            ),
        );
        assert_ne!(
            fallback,
            crate::casa::plan_edits::web_physical_turn_key(
                chat,
                "a different legacy message",
                None,
                None
            ),
        );
        assert_ne!(first, web_inbound_request_id("-100888", voice, &first_key),);
        assert_ne!(first, web_inbound_request_id(chat, "voice-8", &first_key),);
        assert!(
            first.starts_with("web-request-b3-v1-"),
            "unexpected id shape: {first}",
        );
    }

    #[test]
    fn clarify_target_reroutes_a_dm_id_to_the_family_group() {
        // THE LIVE REGRESSION (task nora-clarify-engine): the engine opened a
        // clarify window keyed by Luca's positive DM id (8905220378), so a bare
        // "yes" would only continue in his private chat and the family GROUP that
        // asked was stranded. A clarify must reopen against the originating group.
        let mut bots = HashMap::new();
        bots.insert(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".to_string(),
                chat_id: "-1001112223334".to_string(),
                agent_id: Some("nora".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };

        // A positive DM id (Luca's) is REFUSED and rerouted to the negative group.
        assert_eq!(clarify_target("8905220378", &config), "-1001112223334");
        // A group/supergroup id is honoured verbatim (the gateway-forwarded target).
        assert_eq!(clarify_target("-1001112223334", &config), "-1001112223334");
        // A different explicit group target is also honoured, not overwritten.
        assert_eq!(clarify_target("-100999", &config), "-100999");
    }

    #[test]
    fn clarify_target_falls_back_to_legacy_group_then_best_effort() {
        // No bots map, but a negative legacy top-level group is configured: a DM
        // target still reroutes to it.
        let legacy = TelegramConfig {
            bot_token: "123:ABC".to_string(),
            chat_id: "-100555".to_string(),
            bots: HashMap::new(),
        };
        assert_eq!(clarify_target("8905220378", &legacy), "-100555");

        // Nothing configures ANY group (only DM-shaped ids anywhere) → keep the
        // target best-effort rather than dropping the clarify entirely. This is
        // the residual D20 footgun the boot-time warning already flags loudly.
        let no_group = TelegramConfig {
            bot_token: String::new(),
            chat_id: "42".to_string(),
            bots: HashMap::new(),
        };
        assert_eq!(clarify_target("8905220378", &no_group), "8905220378");
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
        assert!(
            !bot.bot_token.is_empty(),
            "empty token would yield a 404 URL"
        );
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
        let err = resolve_send_bot(&config, None, None)
            .unwrap_err()
            .to_string();
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
        assert!(
            err.contains("otto"),
            "error must name the missing persona: {err}"
        );
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
        match route_inbound_reply(
            dir,
            "telegram:otto",
            "luca-1",
            Some("luca-1"),
            "otto, are you there?",
        ) {
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

    // --- final family-reply delivery seam -----------------------------------

    #[derive(Default)]
    struct ExactReplySink {
        sends: std::sync::Mutex<Vec<(String, String, String)>>,
        edits: std::sync::Mutex<Vec<(String, String, String, String)>>,
    }

    #[async_trait::async_trait]
    impl worksgood::notify::telegram_conversation::ReplySink for ExactReplySink {
        async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
            self.sends.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                text.to_string(),
            ));
            Ok(Some("message-1".to_string()))
        }

        async fn edit(
            &self,
            bot_id: &str,
            chat_id: &str,
            message_id: &str,
            text: &str,
        ) -> Result<()> {
            self.edits.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                message_id.to_string(),
                text.to_string(),
            ));
            Ok(())
        }
    }

    fn opaque_delivery(feed: &Path) -> FamilyReplyDelivery {
        use worksgood::notify::telegram_standup::HouseholdPersona;

        let mut config = TelegramConfig::default();
        config.bots.insert(
            "harbor".to_string(),
            TelegramBotConfig {
                bot_token: "stub-token".to_string(),
                chat_id: "group-chat".to_string(),
                agent_id: Some("harbor".to_string()),
                username: Some("harbor_stub".to_string()),
            },
        );
        FamilyReplyDelivery::from_parts(
            feed.to_path_buf(),
            config,
            casa_feed::PersonaCatalog::from_personas(vec![HouseholdPersona {
                id: "harbor".to_string(),
                display_name: "Harbor Voice".to_string(),
                emoji: "🌊".to_string(),
            }]),
            worksgood::notify::grounding::FamilyVoiceRoster::from_names(
                ["harbor", "Harbor Voice"],
                ["Household Member"],
            ),
        )
    }

    #[test]
    fn group_delivery_guards_and_mirrors_exact_sent_bytes_while_private_stays_private() {
        use worksgood::notify::telegram_conversation::ReplySink as _;

        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let group_transport = ExactReplySink::default();
        let group = delivery.wrap(group_transport, ReplyScope::Group, GuardPolicy::Enforce);
        let raw = "**Harbor Voice** 💬 **Dinner is ready.** \
                   That lives over in the pipeline. **Service:** dispatcher healthy — 2 agents.";

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(group.send("harbor", "group-chat", raw))
            .unwrap();

        let sent = group.inner.sends.lock().unwrap();
        assert_eq!(sent.len(), 1, "one confirmed group send");
        assert_eq!(
            sent[0].2, "Dinner is ready.",
            "the engine guard owns final bytes"
        );
        let lines = feed_lines(&feed);
        assert_eq!(
            lines.len(),
            1,
            "one confirmed group send produces one feed line"
        );
        let entry: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(
            entry["text"].as_str().unwrap(),
            sent[0].2,
            "Telegram and the feed receive byte-identical guarded text"
        );
        assert_eq!(entry["sender"], "Harbor Voice");
        assert_eq!(entry["emoji"], "🌊");
        drop(sent);

        let private_transport = ExactReplySink::default();
        let private = delivery.wrap(private_transport, ReplyScope::Private, GuardPolicy::Enforce);
        rt.block_on(private.send("harbor", "private-chat", "A private answer."))
            .unwrap();
        assert_eq!(private.inner.sends.lock().unwrap().len(), 1);
        assert_eq!(
            feed_lines(&feed).len(),
            1,
            "a private delivery appends no shared-feed line"
        );
    }

    // --- the ENGINE's own receipt, written at the delivery seam --------------

    /// A transport that answers with a REAL positive Bot API message id, which
    /// is the only thing that proves a delivery.
    #[derive(Default)]
    struct NumberedReplySink {
        /// The next message id this transport will answer with. Distinct across
        /// transports on purpose: one Telegram message can only be delivered
        /// once, so two sinks handing out the SAME id is a replay, not two
        /// deliveries, and the ledger is right to refuse the second.
        next: std::sync::atomic::AtomicI64,
        sends: std::sync::Mutex<Vec<String>>,
    }

    impl NumberedReplySink {
        fn starting_at(first: i64) -> Self {
            Self {
                next: std::sync::atomic::AtomicI64::new(first),
                sends: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl worksgood::notify::telegram_conversation::ReplySink for NumberedReplySink {
        async fn send(&self, _bot: &str, _chat: &str, text: &str) -> Result<Option<String>> {
            self.sends.lock().unwrap().push(text.to_string());
            let id = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 501;
            Ok(Some(id.to_string()))
        }
        async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
            Ok(())
        }
    }

    const ENGINE_TURN: &str = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3301";

    /// ITEMS 1 + 5 — the engine writes its OWN receipt, and the mirrored row
    /// carries the accepted turn RAW and verbatim.
    ///
    /// This is the whole point of the contract: before it, "the helper replied"
    /// was reconstructed from the row the writer itself had written, and the
    /// relay's `{ok, message_id}` was discarded. Now the transport's own answer
    /// is recorded independently, and the row and the receipt name each other.
    #[test]
    fn an_engine_reply_writes_its_own_receipt_joined_to_the_row_by_global_feed_id() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed).with_turn_override(ENGINE_TURN);
        let sink = delivery.wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(sink.send("harbor", "group-chat", "Dinner is pasta."))
            .unwrap();

        // The ROW carries the RAW turn, verbatim.
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1);
        let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(
            row["turnId"], ENGINE_TURN,
            "the raw accepted turn, verbatim"
        );
        assert_eq!(row["replyPhase"], "final");
        assert_eq!(row["kind"], "agent");

        // The RECEIPT is an independent record in its own ledger.
        let receipts = relay_receipt::read_all(dir.path());
        assert_eq!(receipts.len(), 1, "the engine wrote exactly one receipt");
        let r = &receipts[0];
        assert_eq!(
            r.provenance, "engine",
            "written BY the engine, not inferred"
        );
        assert_eq!(r.turn_id, ENGINE_TURN);
        assert_eq!(r.status, relay_receipt::RelayStatus::Delivered);
        assert_eq!(r.message_id, Some(501), "the transport's OWN answer");
        assert_eq!(r.outcome, relay_receipt::RelayOutcome::Send);
        assert_eq!(r.role_id, "harbor");

        // ITEM 3 — the join is on the GLOBAL FEED ID, and it lands on this row.
        assert_eq!(r.feed_id, 1);
        assert_eq!(
            row["text"].as_str().unwrap(),
            "Dinner is pasta.",
            "the receipt's feed id names THIS row"
        );

        // ITEM 2 — the scope id is the ACTUAL SENDING BOT, minted through a
        // keyed digest. A raw sha256 of the bot id would be reversible by
        // dictionary in milliseconds: a bot roster is a handful of short stable
        // strings.
        assert!(relay_receipt::is_valid_scope_id(&r.transport_scope_id));
        assert!(
            !relay_receipt::is_dictionary_reversible(&r.transport_scope_id, "harbor"),
            "the transport scope id must not be a raw sha of the bot id"
        );
        // And the key that makes it unreversible never reaches the ledger.
        let ledger = std::fs::read_to_string(relay_receipt::ledger_path_for(dir.path())).unwrap();
        assert!(
            !ledger.contains("stub-token"),
            "no credential in the ledger"
        );
    }

    /// ITEM 3, the exact-row join under the condition that breaks an ordinal
    /// one: TWO engine replies in the SAME MILLISECOND. Each receipt must name
    /// its own row. A joiner that matched on the timestamp would pair both
    /// receipts to the first row and leave the second unproven.
    #[test]
    fn two_same_millisecond_engine_replies_each_get_the_receipt_for_their_own_row() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Two DIFFERENT accepted turns landing in the same instant — two
        // household members asking at once is not exotic.
        let second_turn = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3302";
        let first_sink = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(first_sink.send("harbor", "group-chat", "The FIRST answer."))
            .unwrap();
        let second_sink = opaque_delivery(&feed).with_turn_override(second_turn).wrap(
            NumberedReplySink::starting_at(10),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(second_sink.send("harbor", "group-chat", "The SECOND answer."))
            .unwrap();

        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 2);
        let rows: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let receipts = relay_receipt::read_all(dir.path());
        assert_eq!(receipts.len(), 2);
        let by_turn = |t: &str| {
            receipts
                .iter()
                .find(|r| r.turn_id == t)
                .expect("a receipt per turn")
        };
        let first = by_turn(ENGINE_TURN);
        let second = by_turn(second_turn);

        assert_ne!(first.feed_id, second.feed_id, "distinct rows, distinct ids");
        assert_eq!(
            rows[(first.feed_id - 1) as usize]["text"],
            "The FIRST answer."
        );
        assert_eq!(
            rows[(second.feed_id - 1) as usize]["text"],
            "The SECOND answer."
        );
        // And each receipt carries its own Telegram message id: one message can
        // only be delivered once, which is what the replay guard keys on.
        assert_ne!(first.message_id, second.message_id);
    }

    /// A reply the transport ACCEPTED WITHOUT a usable message id is recorded
    /// UNPROVEN — never as a delivery. We cannot tell whether the family has it,
    /// and the honest record says so rather than guessing in either direction.
    #[test]
    fn a_reply_with_no_usable_message_id_is_recorded_unproven_not_delivered() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed).with_turn_override(ENGINE_TURN);
        // `ExactReplySink` answers "message-1" — accepted, but not a positive id.
        let sink = delivery.wrap(
            ExactReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(sink.send("harbor", "group-chat", "Dinner is pasta."))
            .unwrap();

        let receipts = relay_receipt::read_all(dir.path());
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].status, relay_receipt::RelayStatus::Unproven);
        assert_eq!(receipts[0].message_id, None);
    }

    /// ITEM 4 — the replay guard, on the ENGINE's writer. One Telegram message
    /// can only be delivered once, so a second receipt claiming the same
    /// (scope, message id) is a replayed claim of an old delivery. It is
    /// refused AT WRITE, and the delivery still happens — the family's answer is
    /// never held hostage to the ledger.
    #[test]
    fn a_replayed_delivery_claim_is_refused_at_write_without_breaking_delivery() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        // A transport that answers with the SAME message id twice — a re-read of
        // one stored response, replayed.
        #[derive(Default)]
        struct FrozenIdSink;
        #[async_trait::async_trait]
        impl worksgood::notify::telegram_conversation::ReplySink for FrozenIdSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                Ok(Some("777".to_string()))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Ok(())
            }
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let first = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            FrozenIdSink,
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(first.send("harbor", "group-chat", "First."))
            .unwrap();
        // A DIFFERENT turn, so nothing but the replay guard itself can refuse
        // the second receipt.
        let second = opaque_delivery(&feed)
            .with_turn_override("web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3309")
            .wrap(FrozenIdSink, ReplyScope::Group, GuardPolicy::AlreadyGuarded);
        let refused = rt
            .block_on(second.send("harbor", "group-chat", "Second."))
            .expect_err("a replayed delivery claim must not report a clean success");

        assert_eq!(
            relay_receipt::read_all(dir.path()).len(),
            1,
            "the replayed claim of message 777 was refused"
        );
        // ITEM 1, the transaction half: the refused receipt took its row WITH
        // it. One physical Telegram message is one row and one receipt — a
        // second row for the same message id would double the answer in the
        // family's pane while proving nothing.
        assert_eq!(
            feed_lines(&feed).len(),
            1,
            "the rolled-back row must not survive its refused receipt"
        );
        // ITEM 6 — and it is REPORTED, not swallowed. The bytes did go to
        // Telegram, so this is UNPROVEN (the turn's reservation stays held)
        // rather than a proven failure that would license a second send.
        assert!(
            worksgood::notify::telegram_conversation::is_unproven(&refused),
            "a delivery that left no row must surface as unproven: {refused:#}"
        );
    }

    /// ITEM 8, at the ENGINE seam. `crate::casa::plan_edits::web_physical_turn_key()` hashes the turn for
    /// the internal delivery digest; that hash must never reach the causal
    /// position. `canonical_turn_id` refuses it, so the row is written UNBOUND
    /// and NO receipt is minted — rather than a receipt carrying an id that can
    /// never join a gateway row.
    #[test]
    fn a_hashed_turn_id_in_the_environment_produces_no_receipt() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let hashed = "web-turn-1e4d3c2b1a09f8e7d6c5b4a3928170695e4d3c2b1a09f8e7d6c5b4a392817069";

        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed).with_turn_override(hashed);
        let sink = delivery.wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(sink.send("harbor", "group-chat", "Dinner is pasta."))
            .unwrap();

        let row: serde_json::Value = serde_json::from_str(&feed_lines(&feed)[0]).unwrap();
        assert!(
            row["turnId"].is_null(),
            "the internal hashed key never reaches the causal position: {row}"
        );
        assert!(
            relay_receipt::read_all(dir.path()).is_empty(),
            "NO receipt is minted for an id that could never join"
        );
    }

    /// ITEM 5 — THE ACK DOES NOT CONSUME THE TURN'S FINAL.
    ///
    /// The latency ack is a physical send. When it claimed the turn's ONE final
    /// reservation, a crash between the ack and the answer left the family with
    /// an hourglass and the ledger saying the turn was delivered — so the real
    /// answer was suppressed FOREVER on every later attempt.
    ///
    /// Here the ack is stamped as the ack phase, the process "crashes", and the
    /// restarted attempt still delivers exactly one final.
    #[test]
    fn an_ack_leaves_the_final_unreserved_so_a_crash_between_them_still_answers() {
        use worksgood::notify::relay_receipt::ReplyPhase;
        use worksgood::notify::telegram_conversation::{self as convo, ReplySink as _};
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Attempt one: the ack goes out, then the process dies.
        let acking = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let ack = convo::ack_line();
        rt.block_on(convo::send_reply_once_phase(
            dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            &ack,
            &acking,
            ReplyPhase::Ack,
        ))
        .unwrap();

        // Attempt two, after the "restart": the turn was NEVER claimed by the
        // ack, so the final is free to be delivered.
        let answering = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            NumberedReplySink::starting_at(40),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(convo::send_reply_once_phase(
            dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            "Dinner is pasta.",
            &answering,
            ReplyPhase::Final,
        ))
        .unwrap();

        // ONE final row, and it is the answer — not the ack.
        let rows: Vec<serde_json::Value> = feed_lines(&feed)
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let finals: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["replyPhase"] == "final").collect();
        assert_eq!(finals.len(), 1, "exactly one final row: {rows:?}");
        assert_eq!(finals[0]["text"], "Dinner is pasta.");
        // …and the ack IS in the record, stamped `ack`, bound to the same turn.
        // This assertion used to be its exact opposite ("the transient ack must
        // not enter the record"), which is how a physically delivered message
        // the family saw came to have no feed id and nothing proving it. The ack
        // is transient to the CONVERSATION; it is not transient to the evidence.
        let acks: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["replyPhase"] == "ack").collect();
        assert_eq!(acks.len(), 1, "the delivered ack is recorded: {rows:?}");
        assert_eq!(acks[0]["text"], ack.as_str());
        assert_eq!(acks[0]["turnId"], ENGINE_TURN);
        // Both deliveries are proven, and each receipt names its own row.
        let receipts = relay_receipt::read_all(dir.path());
        assert_eq!(receipts.len(), 2, "ack and final each carry a receipt");
        let ack_receipt = receipts
            .iter()
            .find(|r| r.reply_phase == ReplyPhase::Ack)
            .expect("the ack's receipt");
        assert_eq!(ack_receipt.turn_id, ENGINE_TURN);
        assert!(
            ack_receipt.feed_id > 0 && ack_receipt.feed_id != receipts[1].feed_id,
            "one receipt, one row: {receipts:?}"
        );

        // And a THIRD attempt is now suppressed — the final, and only the final,
        // consumed the reservation.
        let repeat_sink = NumberedReplySink::starting_at(90);
        let repeat = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            repeat_sink,
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(convo::send_reply_once_phase(
            dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            "Dinner is pasta.",
            &repeat,
            ReplyPhase::Final,
        ))
        .unwrap();
        assert_eq!(
            feed_lines(&feed)
                .iter()
                .filter(|l| l.contains("Dinner is pasta."))
                .count(),
            1,
            "the final was delivered twice"
        );
    }

    /// THE TWO-VOICE ANSWER — an `addendum` is a real delivery that is NOT the
    /// turn's final (schema v9.2, `multi_voice_answer`).
    ///
    /// The shipped case is the meal-swap fast lane: the meal owner reports the
    /// swap and the nutrition owner adds a one-line companion take. Before the
    /// fifth phase existed there was no shape for the second row — `final` twice
    /// is fatal by cardinality, and it is plainly not an ack, a watchdog or a
    /// failure — so the companion was emitted with NO causal turn. It stayed
    /// stamped and provable and LOST THE JOIN BACK TO THE ASK: nothing on disk
    /// said Nora's line belonged to the turn Bruno answered.
    ///
    /// Driven through `send_reply_once_phase`, the same seam the ack test above
    /// uses and the same one production takes, because the thing under test is
    /// the RESERVATION's reading of the phase and that lives in the sink.
    #[test]
    fn an_addendum_is_bound_receipted_and_never_the_turns_final() {
        use worksgood::notify::relay_receipt::ReplyPhase;
        use worksgood::notify::telegram_conversation::{self as convo};
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();

        let answer = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(convo::send_reply_once_phase(
            dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            "Swapped Thursday to the carbonara.",
            &answer,
            ReplyPhase::Final,
        ))
        .unwrap();

        // The SECOND VOICE, on the SAME accepted turn. A fresh sink with its own
        // message-id run is the honest shape: an addendum is its own Telegram
        // message, not an edit of the final.
        let companion = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            NumberedReplySink::starting_at(70),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        rt.block_on(convo::send_reply_once_phase(
            dir.path(),
            ENGINE_TURN,
            "nutrition",
            "group-chat",
            "Heavier night — worth a walk after.",
            &companion,
            ReplyPhase::Addendum,
        ))
        .unwrap();

        let rows: Vec<serde_json::Value> = feed_lines(&feed)
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // NON-VACUITY, FIRST. If the companion never reached the feed at all,
        // every "…and it is not a final" assertion below would pass trivially
        // against a writer that had simply dropped the line — which is exactly
        // the outcome the v9.2 decision rejected as worse than the bug.
        let addenda: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|r| r["replyPhase"] == "addendum")
            .collect();
        assert_eq!(
            addenda.len(),
            1,
            "the companion line is in the feed: {rows:?}"
        );
        assert_eq!(addenda[0]["text"], "Heavier night — worth a walk after.");

        // THE JOIN THE PHASE EXISTS FOR: same turn as the answer it accompanies,
        // not a single-row occurrence of its own.
        assert_eq!(
            addenda[0]["turnId"], ENGINE_TURN,
            "the addendum lost the durable join back to the ask: {rows:?}"
        );

        // …and exactly ONE final, which is the answer and not the companion.
        let finals: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["replyPhase"] == "final").collect();
        assert_eq!(
            finals.len(),
            1,
            "exactly one final per accepted turn: {rows:?}"
        );
        assert_eq!(finals[0]["text"], "Swapped Thursday to the carbonara.");

        // One row, one receipt — the addendum is certified like any other
        // delivery, and its receipt names its own row.
        let receipts = relay_receipt::read_all(dir.path());
        let addendum_receipt = receipts
            .iter()
            .find(|r| r.reply_phase == ReplyPhase::Addendum)
            .expect("the addendum carries its own receipt");
        assert_eq!(addendum_receipt.turn_id, ENGINE_TURN);
        let final_receipt = receipts
            .iter()
            .find(|r| r.reply_phase == ReplyPhase::Final)
            .expect("the answer carries its own receipt");
        assert!(
            addendum_receipt.feed_id > 0 && addendum_receipt.feed_id != final_receipt.feed_id,
            "one row, one receipt — the addendum's receipt names the answer's row: {receipts:?}"
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.reply_phase == ReplyPhase::Final)
                .count(),
            1,
            "the addendum was counted as a final in the ledger: {receipts:?}"
        );
    }

    /// ITEM 7 — AMBIGUOUS IS NOT FAILED, at the sink stack the production path
    /// actually uses.
    ///
    /// `TurnDeliverySink` used to treat EVERY error as a proven failure: it
    /// released the reservation and marked the turn retryable. A timeout AFTER
    /// Telegram accepted the message therefore posted the same answer a second
    /// time. Only a PROVEN failure may release.
    #[test]
    fn an_ambiguous_transport_holds_the_turn_while_a_proven_failure_releases_it() {
        use worksgood::notify::telegram_conversation::{self as convo, ReplySink};
        let rt = tokio::runtime::Runtime::new().unwrap();

        /// The production ambiguity shapes, as they arrive at the sink: an
        /// error carrying the UNPROVEN marker.
        struct AmbiguousSink;
        #[async_trait::async_trait]
        impl ReplySink for AmbiguousSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                Err(convo::unproven_delivery("the request timed out"))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Err(convo::unproven_delivery("the edit timed out"))
            }
        }
        /// Telegram answered, and the answer was "no".
        struct RefusedSink;
        #[async_trait::async_trait]
        impl ReplySink for RefusedSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                Err(anyhow::anyhow!("Telegram API error (400): chat not found"))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Err(anyhow::anyhow!("Telegram API error (400): chat not found"))
            }
        }

        /// Counts what actually reached the transport.
        #[derive(Default)]
        struct TallySink {
            sends: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ReplySink for TallySink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Some("7".to_string()))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                Ok(())
            }
        }

        // AMBIGUOUS: the reservation stays HELD, so a later attempt sends
        // nothing at all.
        let ambiguous_dir = tempfile::tempdir().unwrap();
        let held = AmbiguousSink;
        assert!(
            rt.block_on(convo::send_reply_once(
                ambiguous_dir.path(),
                ENGINE_TURN,
                "harbor",
                "group-chat",
                "Dinner is pasta.",
                &held,
            ))
            .is_err()
        );
        let after_ambiguous = TallySink::default();
        let _ = rt.block_on(convo::send_reply_once(
            ambiguous_dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            "Dinner is pasta.",
            &after_ambiguous,
        ));
        assert_eq!(
            after_ambiguous
                .sends
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an ambiguous delivery must NOT be re-sent — the family may already have it"
        );

        // PROVEN FAILURE: the turn is released, and the retry does send.
        let refused_dir = tempfile::tempdir().unwrap();
        let refused = RefusedSink;
        assert!(
            rt.block_on(convo::send_reply_once(
                refused_dir.path(),
                ENGINE_TURN,
                "harbor",
                "group-chat",
                "Dinner is pasta.",
                &refused,
            ))
            .is_err()
        );
        let after_refusal = TallySink::default();
        rt.block_on(convo::send_reply_once(
            refused_dir.path(),
            ENGINE_TURN,
            "harbor",
            "group-chat",
            "Dinner is pasta.",
            &after_refusal,
        ))
        .unwrap();
        assert_eq!(
            after_refusal
                .sends
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a PROVEN failure must release the turn so the answer can be retried"
        );
    }

    /// ITEM 7, the edit half — A REFUSED EDIT'S FALLBACK IS A DIFFERENT MESSAGE.
    ///
    /// `edit` returns `Result<()>`, so the fallback send's message id used to be
    /// discarded and the OLD ack id persisted as though the edit had applied.
    /// The row, the receipt and the reservation then all named a message that
    /// never held the answer.
    #[test]
    fn a_refused_edit_records_the_fallback_message_not_the_stale_ack() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());

        /// Refuses every edit (Telegram said no) and answers a fresh send with
        /// its own id — exactly what `BotReplySink` does on a refused edit.
        #[derive(Default)]
        struct RefusedEditSink {
            fallback: std::sync::Mutex<Option<String>>,
        }
        #[async_trait::async_trait]
        impl worksgood::notify::telegram_conversation::ReplySink for RefusedEditSink {
            async fn send(&self, _b: &str, _c: &str, _t: &str) -> Result<Option<String>> {
                Ok(Some("4242".to_string()))
            }
            async fn edit(&self, _b: &str, _c: &str, _m: &str, _t: &str) -> Result<()> {
                // The refused edit falls back to a fresh send, and REMEMBERS the
                // id that send returned.
                *self.fallback.lock().unwrap() = Some("9001".to_string());
                Ok(())
            }
            fn take_fallback_message_id(&self) -> Option<String> {
                self.fallback.lock().unwrap().take()
            }
        }

        let sink = opaque_delivery(&feed).with_turn_override(ENGINE_TURN).wrap(
            RefusedEditSink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        // Edit the ack (message 111) into the final answer; the edit is refused.
        rt.block_on(sink.edit("harbor", "group-chat", "111", "Dinner is pasta."))
            .unwrap();

        let receipts = relay_receipt::read_all(dir.path());
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].message_id,
            Some(9001),
            "the receipt must name the FALLBACK message that carries the answer, not the ack"
        );
        assert_eq!(
            receipts[0].outcome,
            relay_receipt::RelayOutcome::Fallback,
            "a fallback send is not an edit that applied"
        );
    }

    /// ITEM 8 — A SELF-HEAL RETRY IS NOT A REFIRE, at the engine seam.
    ///
    /// The dedupe key is `(turn, attempt)` and `WG_ATTEMPT_ID` is where the
    /// attempt comes from. Reading it is what keeps attempt 2's receipt — the
    /// evidence for the send that actually reached the family — from being
    /// suppressed as a duplicate of attempt 1.
    #[test]
    fn a_second_attempt_of_one_turn_writes_its_own_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(casa_feed::feed_path_for(root).parent().unwrap()).unwrap();

        let write = |feed_id: i64, mid: &str, attempt: Option<&str>| {
            // The engine reads WG_ATTEMPT_ID at the write site, exactly as the
            // production seam does.
            match attempt {
                Some(a) => unsafe { std::env::set_var("WG_ATTEMPT_ID", a) },
                None => unsafe { std::env::remove_var("WG_ATTEMPT_ID") },
            }
            write_engine_receipt_at(
                root,
                ENGINE_TURN,
                feed_id,
                "harbor",
                "harbor",
                Some(mid),
                relay_receipt::RelayOutcome::Send,
                relay_receipt::ReplyPhase::Final,
                None,
            )
        };

        write(
            1,
            "501",
            Some("attempt-3f2504e0-4f89-41d3-9a0c-0305e82c3301"),
        )
        .unwrap()
        .certified()
        .expect("an undisturbed receipt append certifies its own release");
        // A REFIRE of the same attempt is suppressed...
        let refire = write(
            2,
            "502",
            Some("attempt-3f2504e0-4f89-41d3-9a0c-0305e82c3301"),
        );
        assert!(
            matches!(
                refire,
                Err(relay_receipt::ReceiptError::AttemptAlreadyRecorded { .. })
            ),
            "a refire of one attempt must not write a second receipt: {refire:?}"
        );
        // ...while a genuine SELF-HEAL RETRY writes its own.
        write(
            3,
            "503",
            Some("attempt-3f2504e0-4f89-41d3-9a0c-0305e82c3302"),
        )
        .unwrap()
        .certified()
        .expect("an undisturbed receipt append certifies its own release");
        unsafe { std::env::remove_var("WG_ATTEMPT_ID") };

        let receipts = relay_receipt::read_all(root);
        assert_eq!(
            receipts.len(),
            2,
            "attempt 1 and attempt 2, not one of them"
        );
        assert_eq!(
            receipts[1].attempt_id.as_deref(),
            Some("attempt-3f2504e0-4f89-41d3-9a0c-0305e82c3302"),
            "the retry's receipt records WHICH attempt reached the family"
        );
        assert_eq!(receipts[1].message_id, Some(503));
    }

    /// ITEM 7 — during a SEALED run an engine reply that carries no canonical
    /// turn is prevented from writing the certifying feed at all. The family's
    /// message still goes out over Telegram; what is refused is the
    /// unattributable ROW, which is the thing an auditor would otherwise count
    /// as evidence of something nobody can trace.
    #[test]
    fn a_sealed_run_blocks_an_unbound_engine_reply_row_but_not_the_send() {
        use worksgood::notify::telegram_conversation::ReplySink as _;
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        std::fs::create_dir_all(feed.parent().unwrap()).unwrap();
        std::fs::write(casa_feed::seal_path_for(dir.path()), "{\"sealed\":true}").unwrap();

        // No canonical turn at all — the unbound shape.
        let delivery = opaque_delivery(&feed).with_turn_override("");
        let sink = delivery.wrap(
            NumberedReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let refused = rt
            .block_on(sink.send("harbor", "group-chat", "An untraceable report-back."))
            .expect_err("a reply the seal refused to record is not a clean success");

        assert_eq!(
            sink.inner.sends.lock().unwrap().len(),
            1,
            "the send happened"
        );
        assert!(
            feed_lines(&feed).is_empty(),
            "no unbound row entered the certifying feed"
        );
        assert!(relay_receipt::read_all(dir.path()).is_empty());
        // ITEM 6 — the caller is TOLD the row is missing instead of reporting a
        // delivery the record does not contain.
        assert!(
            worksgood::notify::telegram_conversation::is_unproven(&refused),
            "the missing row must surface as unproven: {refused:#}"
        );
    }

    /// The heavy lane's ack-then-edit: TWO rows, one per physical delivery, and
    /// the edit is mirrored exactly once.
    ///
    /// This test was `composed_ack_edit_mirrors_only_the_final_answer_once` and
    /// asserted the ack produced no feed line at all — the exact-tree audit
    /// named it as a green test around the wrong contract. What "only once"
    /// legitimately means is that the EDIT does not mirror twice; it never meant
    /// that a message the family received leaves no trace.
    #[test]
    fn composed_ack_edit_records_both_deliveries_and_mirrors_the_edit_once() {
        use worksgood::notify::telegram_conversation as convo;
        use worksgood::notify::telegram_conversation::ReplySink as _;

        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = delivery.wrap(
            ExactReplySink::default(),
            ReplyScope::Group,
            GuardPolicy::AlreadyGuarded,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(sink.send("harbor", "group-chat", &convo::ack_line()))
            .unwrap();
        let after_ack = feed_lines(&feed);
        assert_eq!(
            after_ack.len(),
            1,
            "the delivered ack is a row: {after_ack:?}"
        );
        let acked: serde_json::Value = serde_json::from_str(&after_ack[0]).unwrap();
        assert_eq!(acked["text"], convo::ack_line());
        // This fixture's sink carries no canonical turn, so the row is the
        // legacy unbound shape — the phase rides on a turn. The point being
        // pinned here is that the DELIVERY leaves a row at all; the phased,
        // turn-bound ack is pinned by
        // `an_ack_leaves_the_final_unreserved_so_a_crash_between_them_still_answers`.
        assert!(acked["turnId"].is_null(), "{acked:?}");

        rt.block_on(sink.edit("harbor", "group-chat", "message-1", "Dinner is ready."))
            .unwrap();

        assert_eq!(sink.inner.sends.lock().unwrap().len(), 1);
        assert_eq!(sink.inner.edits.lock().unwrap().len(), 1);
        let lines = feed_lines(&feed);
        assert_eq!(
            lines.len(),
            2,
            "the final edit mirrors exactly once, on top of the ack: {lines:?}"
        );
        let entry: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(entry["text"], "Dinner is ready.");
        // Turn-less fixture — see the note on the ack row above.
        assert!(entry["turnId"].is_null(), "{entry:?}");
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
            origin: TaskOrigin::new(
                channel,
                "opaque-chat",
                "Household Member",
                "harbor",
                Some("harbor".to_string()),
            ),
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

    /// THE CLI SEAM MAY NOT GUESS THE PHASE (task receipt-engine-reply).
    ///
    /// `casa_feed::validate` refuses a turn-bound row carrying no `reply_phase`
    /// — v9.1's required negative — and `feed-write` walked straight past it by
    /// defaulting the missing flag to `"final"`. That default is not a
    /// convenience: `final` is the STRONGEST of the four claims, the one the
    /// turn's one-final reservation is keyed on, so a caller who never declared
    /// anything silently claimed to be the turn's single answer. It also made
    /// the gate unreachable from this seam — the writer's own default filled the
    /// exact hole the validator exists to catch, which is a gate defeated by a
    /// literal.
    ///
    /// The writer KNOWS which phase it is emitting. If it did not say, it must
    /// be asked, not guessed for.
    #[test]
    fn feed_write_refuses_a_turn_bound_row_whose_phase_was_never_declared() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());

        let err = crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is pasta.",
            None,
            Some(ENGINE_TURN),
            None, // …and no --reply-phase
            None,
            None,
            None,
        )
        .expect_err("a turn-bound agent row was written with a phase nobody declared");
        let detail = format!("{err:#}");
        assert!(
            detail.contains("--reply-phase"),
            "the refusal must name the flag the caller has to supply: {detail}"
        );

        // A REFUSAL LEAVES NO ROW. Refusing after the append would put exactly
        // the unstamped row into the family's conversation that the refusal
        // exists to keep out.
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "the refused row reached the feed anyway: {:?}",
            feed_lines(&feed)
        );

        // THE CONTROL. The same call, with the phase declared, writes the row —
        // so the refusal above is about the missing declaration and not about
        // some other thing wrong with this shape.
        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is pasta.",
            None,
            Some(ENGINE_TURN),
            Some("final"),
            None,
            None,
            None,
        )
        .expect("a fully declared turn-bound row must still be writable");
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(row["turnId"], ENGINE_TURN);
        assert_eq!(row["replyPhase"], "final");
    }

    /// EVERY MEMBER OF THE CLOSED ENUM IS STAMPED VERBATIM BY THIS SEAM.
    ///
    /// `feed-write --reply-phase` is the flag the gateway's cross-impl twin
    /// (`claw3d-bridge/test/replyPhaseEngineTwin.test.mjs`) drives against the
    /// REAL binary, once per member of ITS `REPLY_PHASES`. That twin was red for
    /// exactly one word: `addendum` joined the gateway's enum with schema v9.2
    /// and this parser had four arms, so the engine refused a phase its twin
    /// writes. A vocabulary that differs between two writers of one file is not
    /// a naming quibble — it is a row one of them can produce and the other
    /// cannot read.
    ///
    /// Kept as a LITERAL list rather than iterating over the Rust enum on
    /// purpose: iterating would only prove the parser agrees with itself, and
    /// the thing at risk is agreement with the OTHER repo's list.
    #[test]
    fn feed_write_stamps_every_phase_of_the_closed_enum_verbatim() {
        for phase in ["ack", "final", "addendum", "watchdog", "failure"] {
            let dir = tempfile::tempdir().unwrap();
            let feed = casa_feed::feed_path_for(dir.path());
            crate::casa::feed_write::run_feed_write(
                dir.path(),
                "agent",
                None,
                Some("harbor"),
                &format!("a {phase} line"),
                None,
                Some(ENGINE_TURN),
                Some(phase),
                None,
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("the engine refused a declared '{phase}' row: {e:#}"));
            let lines = feed_lines(&feed);
            assert_eq!(lines.len(), 1, "'{phase}': {lines:?}");
            let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
            assert_eq!(
                row["turnId"], ENGINE_TURN,
                "'{phase}': the turn was dropped"
            );
            assert_eq!(
                row["replyPhase"], phase,
                "the engine stamped something else for '{phase}': {row}"
            );
        }

        // THE CONTROL. The enum is still CLOSED — the loop above would pass just
        // as well against a parser that accepted any string it was handed and
        // echoed it onto the row, which is the normalise-anything failure the
        // twin's third case exists to catch.
        for invented in ["FINAL", "companion", "addendum "] {
            let dir = tempfile::tempdir().unwrap();
            let feed = casa_feed::feed_path_for(dir.path());
            let outcome = crate::casa::feed_write::run_feed_write(
                dir.path(),
                "agent",
                None,
                Some("harbor"),
                "Dinner is the soup.",
                None,
                Some(ENGINE_TURN),
                Some(invented),
                None,
                None,
                None,
            );
            // `"addendum "` is the deliberate near-miss: it is REFUSED for its
            // whitespace only if the parser trims before matching, and accepted
            // as `addendum` if it does. Either way it must not become a row
            // stamped with the untrimmed string.
            if invented.trim() == "addendum" {
                outcome.expect("a trimmed-but-valid phase is the word itself");
                let row: serde_json::Value = serde_json::from_str(&feed_lines(&feed)[0]).unwrap();
                assert_eq!(row["replyPhase"], "addendum", "the padding reached the row");
            } else {
                let err = outcome.expect_err(&format!(
                    "the engine accepted the off-enum phase '{invented}'"
                ));
                assert!(
                    format!("{err:#}").contains("--reply-phase"),
                    "the refusal must name the flag: {err:#}"
                );
                assert!(
                    !feed.exists() || feed_lines(&feed).is_empty(),
                    "the off-enum row reached the feed anyway: {:?}",
                    feed_lines(&feed)
                );
            }
        }
    }

    /// …and the requirement is CONDITIONAL on being turn-bound. A row with no
    /// causal turn is its own single-row occurrence: there is no turn for it to
    /// be a phase of, and demanding one would make the listener's inbound human
    /// mirrors unwritable.
    #[test]
    fn feed_write_still_writes_a_turn_less_row_with_no_phase() {
        // `WG_TURN_ID` is process-global and this call falls back to it, so a
        // leaked value would make the row turn-BOUND and this control vacuous.
        // Say so rather than pass quietly.
        assert!(
            std::env::var("WG_TURN_ID").is_err(),
            "WG_TURN_ID is set in this test process — the turn-less control cannot be trusted"
        );
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());

        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "group",
            Some("Wren"),
            None,
            "what are we doing this weekend?",
            None,
            None,
            None, // no phase, and none is owed
            Some(casa_feed::NON_RELAY_TELEGRAM_INBOUND),
            None,
            None,
        )
        .expect("an inbound human mirror needs no phase");
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let row: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(row["turnId"].is_null(), "{row:?}");
        assert!(row["replyPhase"].is_null(), "{row:?}");
    }

    #[test]
    fn lifecycle_group_report_back_lands_in_feed_and_telegram_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = RecordingSink::default();
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Dinner is underway.",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(crate::casa::lifecycle::deliver_lifecycle_fire(
            &sink, &delivery, &fire,
        ))
        .unwrap();

        // Telegram: exactly one send.
        assert_eq!(
            sink.sends.lock().unwrap().len(),
            1,
            "exactly one telegram send"
        );
        // Pane feed: exactly one `agent` line carrying the report-back.
        let lines = feed_lines(&feed);
        assert_eq!(lines.len(), 1, "exactly one feed line, got {lines:?}");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["kind"], "agent", "{v}");
        assert_eq!(v["agentId"], "harbor", "{v}");
        assert!(
            v["text"].as_str().unwrap().contains("underway"),
            "the ledger carries the guarded report-back: {v}"
        );
    }

    #[test]
    fn lifecycle_direct_report_back_never_leaks_into_the_shared_feed() {
        // A 1:1 DM report-back is private — it reaches Telegram but must NEVER be
        // written into the shared group-feed the pane renders (docs/15 privacy).
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = RecordingSink::default();
        let fire = lc_fire(
            OriginChannel::TelegramDirect,
            LifecycleEvent::Done,
            "Done! that's sorted ✅",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(crate::casa::lifecycle::deliver_lifecycle_fire(
            &sink, &delivery, &fire,
        ))
        .unwrap();

        assert_eq!(
            sink.sends.lock().unwrap().len(),
            1,
            "the 1:1 DM is still sent"
        );
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "a 1:1 DM report-back must not touch the shared group feed"
        );
    }

    #[test]
    fn lifecycle_send_retries_once_then_succeeds_and_still_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = FlakySink::new(1); // first attempt fails, retry succeeds
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Dinner is underway.",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(crate::casa::lifecycle::deliver_lifecycle_fire(
            &sink, &delivery, &fire,
        ))
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
        // Both attempts fail → Err (so crate::casa::lifecycle::run_lifecycle re-arms the FiredLog) and the
        // undelivered line must NOT appear in the pane (no phantom "Done!").
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = FlakySink::new(2);
        let fire = lc_fire(
            OriginChannel::TelegramGroup,
            LifecycleEvent::Started,
            "Dinner is underway.",
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(crate::casa::lifecycle::deliver_lifecycle_fire(
            &sink, &delivery, &fire,
        ));

        assert!(
            res.is_err(),
            "two failures surface an error for the caller to re-arm"
        );
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

    #[test]
    fn pretransport_partial_state_save_reconciles_before_tick() {
        use worksgood::notify::daily_digest::{DigestPolicy, DigestStore};
        use worksgood::notify::lifecycle::{self, FailureShape, LifecycleInput};
        use worksgood::notify::reminder::FiredLog;

        let dir = tempfile::tempdir().unwrap();
        let log_path = FiredLog::path(dir.path());
        let store_path = DigestStore::path(dir.path());
        let blocked_store_path = dir.path().join(".casa").join("blocked-pretransport");
        let now = parse_naive_now("2026-07-24T19:30").unwrap();
        let input = LifecycleInput {
            task_id: "entry-lamp-check".to_string(),
            origin: TaskOrigin::new(
                OriginChannel::TelegramDirect,
                "private-origin",
                "Household Member",
                "harbor",
                Some("harbor".to_string()),
            ),
            event: LifecycleEvent::Started,
            workers: vec!["configured-worker".to_string()],
            summary: None,
            failure: FailureShape::Dropped,
            what: "check the entry lamp".to_string(),
        };
        let policy = DigestPolicy::default();
        let lifecycle_id = lifecycle::notification_id(&input.task_id, input.event);
        let mut log = FiredLog::default();
        let mut store = DigestStore::default();
        let first = lifecycle::lifecycle_tick(
            std::slice::from_ref(&input),
            &mut log,
            &mut store,
            now,
            &policy,
        );
        assert_eq!(first.fired.len(), 1);

        std::fs::create_dir_all(&blocked_store_path).unwrap();
        let failure = crate::casa::lifecycle::persist_lifecycle_state_before_transport(
            &first,
            &log,
            &log_path,
            &store,
            &blocked_store_path,
        )
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("failed to persist pacing state"),
            "the injected second-file failure must be surfaced: {failure:#}",
        );
        assert!(
            FiredLog::load(&log_path).contains(&lifecycle_id),
            "the first state file was durably written before the injected failure",
        );
        assert_eq!(
            crate::casa::lifecycle::load_lifecycle_rearm_journal(
                &crate::casa::lifecycle::lifecycle_rearm_path(&log_path)
            )
            .unwrap()
            .entries
            .len(),
            1,
        );
        assert!(
            lifecycle_reconciliation_needs_tick(&log_path),
            "the listener gate must wake even though the stale FiredLog suppresses the turn",
        );
        std::fs::remove_dir(&blocked_store_path).unwrap();

        let mut reloaded_log = FiredLog::load(&log_path);
        let mut reloaded_store = DigestStore::load(&store_path);
        crate::casa::lifecycle::reconcile_lifecycle_rearms(
            &log_path,
            &store_path,
            &mut reloaded_log,
            &mut reloaded_store,
        )
        .unwrap();
        assert!(
            !lifecycle_reconciliation_needs_tick(&log_path),
            "a fully reconciled empty journal lets idle listener ticks stay quiet",
        );
        let retry = lifecycle::lifecycle_tick(
            &[input],
            &mut reloaded_log,
            &mut reloaded_store,
            now,
            &policy,
        );
        assert_eq!(
            retry.fired.len(),
            1,
            "startup reconciliation must restore the unsent exact id",
        );
    }

    #[test]
    fn failed_delivery_rearm_survives_partial_state_save() {
        use worksgood::notify::daily_digest::{DigestPolicy, DigestStore};
        use worksgood::notify::lifecycle::{self, FailureShape, LifecycleInput};
        use worksgood::notify::reminder::FiredLog;

        let dir = tempfile::tempdir().unwrap();
        let log_path = FiredLog::path(dir.path());
        let store_path = DigestStore::path(dir.path());
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let now = parse_naive_now("2026-07-24T19:30").unwrap();
        let input = LifecycleInput {
            task_id: "stalled-porch-light".to_string(),
            origin: TaskOrigin::new(
                OriginChannel::TelegramDirect,
                "private-origin",
                "Household Member",
                "harbor",
                Some("harbor".to_string()),
            ),
            event: LifecycleEvent::Failed,
            workers: vec!["configured-worker".to_string()],
            summary: None,
            failure: FailureShape::Final,
            what: "replace the porch light".to_string(),
        };
        let policy = DigestPolicy::default();
        let lifecycle_id = lifecycle::notification_id(&input.task_id, LifecycleEvent::Failed);
        let alert_id = lifecycle::alert_notification_id(&input.task_id);

        let mut log = FiredLog::default();
        let mut store = DigestStore::default();
        let first = lifecycle::lifecycle_tick(
            std::slice::from_ref(&input),
            &mut log,
            &mut store,
            now,
            &policy,
        );
        assert_eq!(first.fired.len(), 1);
        assert_eq!(first.operator_alerts.len(), 1);
        assert!(log.contains(&lifecycle_id));
        assert!(log.contains(&alert_id));
        crate::casa::lifecycle::persist_lifecycle_state_before_transport(
            &first,
            &log,
            &log_path,
            &store,
            &store_path,
        )
        .unwrap();

        let mut bots = HashMap::new();
        bots.insert(
            "owner-wire".to_string(),
            TelegramBotConfig {
                bot_token: "500:EEE".to_string(),
                chat_id: "7005".to_string(),
                agent_id: Some("configured-owner".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: "600:FFF".to_string(),
            chat_id: "7006".to_string(),
            bots,
        };
        // Operator alert fails once, then both family-send attempts fail.
        let failing_sink = FlakySink::new(3);
        let blocked_store_path = dir.path().join(".casa").join("blocked-store");
        std::fs::create_dir(&blocked_store_path).unwrap();
        let failure = crate::casa::lifecycle::deliver_lifecycle_tick_result(
            &failing_sink,
            &delivery,
            &config,
            Some("configured-owner"),
            &first,
            &mut log,
            &log_path,
            &mut store,
            &blocked_store_path,
        )
        .unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("failed to persist re-armed lifecycle pacing state"),
            "the injected second-file failure must be surfaced: {failure:#}",
        );
        assert_eq!(
            failing_sink.attempts.lock().unwrap().len(),
            3,
            "one owner-alert attempt plus two family-delivery attempts",
        );
        let journal_path = crate::casa::lifecycle::lifecycle_rearm_path(&log_path);
        assert_eq!(
            crate::casa::lifecycle::load_lifecycle_rearm_journal(&journal_path)
                .unwrap()
                .entries
                .len(),
            2,
            "both exact undelivered ids remain in the reconciliation record",
        );
        std::fs::remove_dir(&blocked_store_path).unwrap();

        // Reload from disk, as the next scheduler process would. The first state
        // file saved its re-arm, but the second still contains the pacing
        // suppressor. Startup must reconcile BOTH before computing the tick.
        let mut reloaded_log = FiredLog::load(&log_path);
        let mut reloaded_store = DigestStore::load(&store_path);
        let mut unreconciled_log = reloaded_log.clone();
        let mut unreconciled_store = reloaded_store.clone();
        let suppressed = lifecycle::lifecycle_tick(
            std::slice::from_ref(&input),
            &mut unreconciled_log,
            &mut unreconciled_store,
            now,
            &policy,
        );
        assert!(
            suppressed.fired.is_empty(),
            "the stale pacing file really would suppress the family retry",
        );
        assert_eq!(
            crate::casa::lifecycle::reconcile_lifecycle_rearms(
                &log_path,
                &store_path,
                &mut reloaded_log,
                &mut reloaded_store,
            )
            .unwrap(),
            2,
        );
        assert!(
            crate::casa::lifecycle::load_lifecycle_rearm_journal(&journal_path)
                .unwrap()
                .entries
                .is_empty(),
            "the record clears only after both reconciled files save",
        );
        assert!(!reloaded_log.contains(&lifecycle_id));
        assert!(!reloaded_log.contains(&alert_id));
        let second = lifecycle::lifecycle_tick(
            std::slice::from_ref(&input),
            &mut reloaded_log,
            &mut reloaded_store,
            now,
            &policy,
        );
        assert_eq!(
            second.fired.len(),
            1,
            "the next tick retries the failed family report-back",
        );
        assert_eq!(
            second.operator_alerts.len(),
            1,
            "the next tick retries the failed private owner alert",
        );

        // A confirmed second-tick delivery becomes durable exactly once again.
        crate::casa::lifecycle::persist_lifecycle_state_before_transport(
            &second,
            &reloaded_log,
            &log_path,
            &reloaded_store,
            &store_path,
        )
        .unwrap();
        let succeeding_sink = RecordingSink::default();
        let delivered = crate::casa::lifecycle::deliver_lifecycle_tick_result(
            &succeeding_sink,
            &delivery,
            &config,
            Some("configured-owner"),
            &second,
            &mut reloaded_log,
            &log_path,
            &mut reloaded_store,
            &store_path,
        )
        .unwrap();
        assert_eq!(delivered.sent, 1);
        assert_eq!(delivered.alerted, 1);
        assert_eq!(delivered.rearmed, 0);
        assert_eq!(succeeding_sink.sends.lock().unwrap().len(), 2);

        let mut final_log = FiredLog::load(&log_path);
        let mut final_store = DigestStore::load(&store_path);
        let third =
            lifecycle::lifecycle_tick(&[input], &mut final_log, &mut final_store, now, &policy);
        assert!(third.fired.is_empty());
        assert!(third.operator_alerts.is_empty());
    }

    // ── Daily-digest flush delivery (task re-arm-the) ──────────────────────
    //
    // A morning digest is a private 1:1 delivery. It uses the same guard/retry
    // seam as lifecycle report-backs but must never enter the shared group feed.

    #[test]
    fn digest_delivers_privately_without_touching_the_group_feed() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = RecordingSink::default();
        let text = "Today: PT check-in at 19:30 · how was last night's salmon?";

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(crate::casa::digest::deliver_digest_fire(
            &sink,
            &delivery,
            "harbor",
            "private-chat",
            text,
        ))
        .unwrap();

        // Telegram: exactly one send, to the resolved chat.
        let sends = sink.sends.lock().unwrap();
        assert_eq!(sends.len(), 1, "exactly one digest telegram send");
        assert_eq!(sends[0].1, "private-chat", "sent to the resolved chat");
        assert_eq!(sends[0].2, text, "the composed digest is what goes out");
        drop(sends);

        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "a private digest must not appear in the shared group feed"
        );
    }

    #[test]
    fn digest_send_that_fails_twice_errors_and_does_not_mirror() {
        // Both attempts fail → Err so `run_digest` leaves the pending queue
        // intact for the next tick, and NOTHING is mirrored (no phantom digest
        // in the pane for a message that never reached the human).
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let delivery = opaque_delivery(&feed);
        let sink = FlakySink::new(2);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(crate::casa::digest::deliver_digest_fire(
            &sink,
            &delivery,
            "harbor",
            "private-chat",
            "Today: something",
        ));

        assert!(
            res.is_err(),
            "two failures surface an error so the queue is kept"
        );
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

    // --- one section per feed-write (feed-lock-section, docs/42 §9) --------
    //
    // THE FINDING THIS PINS. `wg telegram feed-write --kind agent --turn-id
    // --message-id` used to take `.conversation.lock` more than once in one
    // process: the row and its receipt in one section, then the audience record
    // in a second. Every section is a queue position for every other writer, and
    // six concurrent writers of this shape were measured at 785 ms average /
    // 986 ms peak against a 1000 ms budget — a coin flip, decided by the
    // scheduler, over whether the family's message is recorded or the house says
    // it never heard them.
    //
    // The counter is on `feed_lock::acquire` itself and is always compiled, for
    // the reason its doc comment gives: this module is the BINARY crate and
    // `feed_lock` is the library, so a `cfg(test)` counter there would simply not
    // exist here, and the only reachable assertion would be about a look-alike of
    // the seam rather than the seam.

    fn audience_lines(feed: &Path) -> Vec<String> {
        feed_lines(&casa_audience::audience_path_for(feed))
    }

    fn receipt_lines(root: &Path) -> Vec<String> {
        feed_lines(&relay_receipt::ledger_path_for(root))
    }

    /// ONE `feed-write`, ONE SECTION — and all three artifacts still land.
    ///
    /// The count alone would be satisfied by a writer that stopped recording the
    /// audience, which is the cheapest wrong way to make this number go down, so
    /// the row, the receipt AND the audience record are asserted present in the
    /// same test. `reentrant_frames` is asserted zero as well: a nested acquire
    /// of a lock this thread already holds is not a second section, but it is
    /// also not how any of this is meant to work, and counting the two apart is
    /// what stops "one acquisition" from hiding a re-entrant one.
    #[test]
    #[serial_test::serial]
    fn one_feed_write_enters_exactly_one_conversation_lock_section() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        worksgood::notify::feed_lock::reset_acquisition_counters();
        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is the soup.",
            None,
            Some(ENGINE_TURN),
            Some("final"),
            None,
            Some("4242"),
            None,
        )
        .expect("the writer must land the row");

        assert_eq!(
            worksgood::notify::feed_lock::distinct_acquisitions(),
            1,
            "one feed-write took .conversation.lock this many times — the row, its receipt and \
             its audience are ONE section (docs/42 §9, feed-lock-section)"
        );
        assert_eq!(
            worksgood::notify::feed_lock::reentrant_frames(),
            0,
            "a re-entrant frame appeared: the section is nested, not shortened"
        );
        // …and the section still did all three jobs.
        assert_eq!(feed_lines(&feed).len(), 1, "the row");
        assert_eq!(receipt_lines(dir.path()).len(), 1, "the delivery receipt");
        assert_eq!(audience_lines(&feed).len(), 1, "the audience record");
    }

    /// The audience is gated on the TURN, not on the receipt. A turn-bound row
    /// with no `--message-id` has no delivery to prove and still has an audience
    /// — folding the audience into the receipt's branch would silently drop it
    /// for every ack the gateway writes without a transport answer.
    #[test]
    #[serial_test::serial]
    fn a_row_with_a_turn_and_no_delivery_still_records_its_audience_in_one_section() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        worksgood::notify::feed_lock::reset_acquisition_counters();
        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "On it.",
            None,
            Some(ENGINE_TURN),
            Some("ack"),
            None,
            None, // no --message-id: nothing delivered, nothing to prove
            None,
        )
        .expect("the writer must land the row");

        assert_eq!(worksgood::notify::feed_lock::distinct_acquisitions(), 1);
        assert_eq!(feed_lines(&feed).len(), 1, "the row");
        assert!(
            receipt_lines(dir.path()).is_empty(),
            "a receipt was invented for a row with no transport answer"
        );
        assert_eq!(
            audience_lines(&feed).len(),
            1,
            "the audience record is gated on the receipt — an ack with no message id lost it"
        );
    }

    /// THE ROW AND ITS RECEIPT ARE STILL ONE TRANSACTION — neither survives
    /// without the other.
    ///
    /// This is the rule `feed-lock-section` was told not to break while
    /// shortening the section, and the reason the audience record is written
    /// with the held lock but NOT inside the `?` chain: sharing the exclusion is
    /// not the same as sharing the rollback.
    ///
    /// The receipt is refused with a state the ledger's own strictness produces:
    /// a final line with no terminating newline is a write interrupted at the
    /// delimiter, which `read_strict` refuses rather than parses. So the prove
    /// closure fails, and the row it had already appended is truncated back out.
    #[test]
    #[serial_test::serial]
    fn a_refused_receipt_takes_the_row_back_out_and_writes_no_audience() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        let ledger = relay_receipt::ledger_path_for(dir.path());
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        std::fs::write(&ledger, "{\"v\":1,\"turnId\":\"torn").unwrap();
        let ledger_before = std::fs::read(&ledger).unwrap();

        worksgood::notify::feed_lock::reset_acquisition_counters();
        let outcome = crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is the soup.",
            None,
            Some(ENGINE_TURN),
            Some("final"),
            None,
            Some("4242"),
            None,
        );

        let err = outcome.expect_err("a receipt the ledger refuses must fail the write");
        assert!(
            !feed.exists() || feed_lines(&feed).is_empty(),
            "the row survived a receipt that did not: {:?} ({err:#})",
            feed_lines(&feed)
        );
        assert_eq!(
            std::fs::read(&ledger).unwrap(),
            ledger_before,
            "the refused receipt touched the ledger"
        );
        assert!(
            audience_lines(&feed).is_empty(),
            "an audience was recorded for a row that was taken back out"
        );
        assert_eq!(
            worksgood::notify::feed_lock::distinct_acquisitions(),
            1,
            "the failed transaction still took one section, not two"
        );
    }

    /// THE CONTROL FOR THE TEST ABOVE, and it is not optional. `run_feed_write`
    /// erroring proves nothing about the transaction unless the SAME call on a
    /// healthy ledger lands both halves — otherwise a writer that refused every
    /// row would pass the rollback test perfectly.
    #[test]
    #[serial_test::serial]
    fn the_same_write_on_a_healthy_ledger_lands_the_row_and_the_receipt_together() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is the soup.",
            None,
            Some(ENGINE_TURN),
            Some("final"),
            None,
            Some("4242"),
            None,
        )
        .expect("the healthy control must land");
        assert_eq!(feed_lines(&feed).len(), 1, "the row");
        assert_eq!(receipt_lines(dir.path()).len(), 1, "the receipt");
    }

    /// A REFUSED AUDIENCE DOES NOT TAKE THE ROW BACK OUT. The rollback boundary
    /// is where it was: the row and the receipt roll back together, and the
    /// audience — a fact about a message the family has already seen — is
    /// reported rather than allowed to destroy the record of the reply.
    ///
    /// The refusal is produced by putting a DIRECTORY where the ledger's file
    /// belongs, so the append fails with `EISDIR` inside the section.
    #[test]
    #[serial_test::serial]
    fn a_refused_audience_leaves_the_row_and_its_receipt_standing() {
        let dir = tempfile::tempdir().unwrap();
        let feed = casa_feed::feed_path_for(dir.path());
        std::fs::create_dir_all(&casa_audience::audience_path_for(&feed)).unwrap();

        worksgood::notify::feed_lock::reset_acquisition_counters();
        crate::casa::feed_write::run_feed_write(
            dir.path(),
            "agent",
            None,
            Some("harbor"),
            "Dinner is the soup.",
            None,
            Some(ENGINE_TURN),
            Some("final"),
            None,
            Some("4242"),
            None,
        )
        .expect("an audience the ledger refuses must NOT fail the row");
        assert_eq!(feed_lines(&feed).len(), 1, "the row was rolled back");
        assert_eq!(
            receipt_lines(dir.path()).len(),
            1,
            "the receipt went with it"
        );
        assert_eq!(worksgood::notify::feed_lock::distinct_acquisitions(), 1);
    }
    // THESE TWO TESTS STAY HERE ON PURPOSE, while their subject `resolve_dm_target`
    // moved to `casa::digest` (slice 5). They also drive `try_register_reminder`, which
    // has seven callers left in this file and is therefore genuinely shared — moving
    // them would mean widening its visibility here, re-creating exactly the debt slice 5
    // just paid off. They follow when that helper does.

    #[test]
    fn proactive_messages_use_the_configured_coordination_owner() {
        use worksgood::notify::reminder::AdHocStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wg = root.join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "garden-relay"
name = "Garden Relay"
domains = ["coordination"]

[[agent]]
id = "pantry-relay"
name = "Pantry Relay"
domains = ["cooking"]
"#,
        )
        .unwrap();
        seed_confirmed_binding(&wg, "7001001", "member-map", "Household Member");

        let now =
            chrono::NaiveDateTime::parse_from_str("2026-07-12T10:00", "%Y-%m-%dT%H:%M").unwrap();
        assert!(
            try_register_reminder(
                &wg,
                "7001001",
                "household-handle",
                "remind me tomorrow at 7pm to lock the patio",
                now,
            )
            .is_some()
        );
        crate::casa::remind::run_remind(
            &wg,
            false,
            false,
            Some("remind me Tuesday at 8am to set out the bins"),
            Some("Household Member"),
            None,
            None,
            Some("2026-07-12T10:00"),
            false,
        )
        .unwrap();

        let store = AdHocStore::load(&AdHocStore::path(root));
        assert_eq!(store.reminders.len(), 2);
        assert!(
            store
                .reminders
                .iter()
                .all(|reminder| reminder.bot == "garden-relay"),
            "both registration seams must persist only the configured coordination owner: {:?}",
            store.reminders,
        );

        let bindings = TelegramBindingMap::load(&wg.join("agency")).unwrap();
        assert!(
            bindings
                .bindings
                .iter()
                .all(|binding| binding.bot_id.is_none()),
            "the fixture must exercise the missing-bot binding path",
        );
        let mut bots = HashMap::new();
        bots.insert(
            "first-fallback".to_string(),
            TelegramBotConfig {
                bot_token: "100:AAA".to_string(),
                chat_id: "-1001".to_string(),
                agent_id: Some("pantry-relay".to_string()),
                username: None,
            },
        );
        bots.insert(
            "coordination-channel".to_string(),
            TelegramBotConfig {
                bot_token: "200:BBB".to_string(),
                chat_id: "-1002".to_string(),
                agent_id: Some("garden-relay".to_string()),
                username: None,
            },
        );
        let config = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        let hint = crate::casa::digest::coordination_owner_hint(root);
        let (_, bot_id, _) =
            crate::casa::digest::resolve_dm_target(&config, &bindings, "Household Member", &hint)
                .unwrap();
        assert_eq!(
            bot_id, "coordination-channel",
            "a digest for an unbound member must use the configured coordination voice",
        );
    }

    #[test]
    fn ambiguous_plan_source_never_selects_first_bot() {
        use worksgood::notify::family_plan::CalendarEvent;
        use worksgood::notify::reminder::Reminder;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wg = root.join(".wg");
        std::fs::create_dir_all(&wg).unwrap();
        std::fs::write(
            root.join("household.toml"),
            r#"
[[agent]]
id = "coordination-anchor-a"
name = "Shared Lantern"
domains = ["coordination"]

[[agent]]
id = "coordination-anchor-b"
name = "Shared Lantern"
domains = ["calendar"]
"#,
        )
        .unwrap();
        seed_confirmed_binding(&wg, "7001001", "member-map", "Household Member");
        let bindings = TelegramBindingMap::load(&wg.join("agency")).unwrap();
        let owners = ownership::OwnerMap::load(root);
        let event = CalendarEvent {
            weekday: "Tue".into(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 28),
            time: "08:00".into(),
            event: "\u{23f0} Reminder: Household Member set out the bins".into(),
            source: "Shared Lantern".into(),
        };
        assert!(
            Reminder::from_calendar_event(
                "2026-W31",
                &event,
                &["Household Member".to_string()],
                &owners,
            )
            .is_none(),
            "duplicate display labels must not become a guessed stable owner",
        );

        for reverse in [false, true] {
            let entries = [
                (
                    "first-wire",
                    TelegramBotConfig {
                        bot_token: "100:AAA".to_string(),
                        chat_id: "-1001".to_string(),
                        agent_id: Some("coordination-anchor-a".to_string()),
                        username: None,
                    },
                ),
                (
                    "second-wire",
                    TelegramBotConfig {
                        bot_token: "200:BBB".to_string(),
                        chat_id: "-1002".to_string(),
                        agent_id: Some("coordination-anchor-b".to_string()),
                        username: None,
                    },
                ),
            ];
            let mut bots = HashMap::new();
            let order: &[usize] = if reverse { &[1, 0] } else { &[0, 1] };
            for index in order {
                let (id, bot) = &entries[*index];
                bots.insert((*id).to_string(), bot.clone());
            }
            let config = TelegramConfig {
                bot_token: String::new(),
                chat_id: String::new(),
                bots,
            };
            assert!(
                crate::casa::digest::resolve_dm_target(
                    &config,
                    &bindings,
                    "Household Member",
                    "unresolved-source",
                )
                .is_none(),
                "an unresolved non-empty Source must never fall through to map order",
            );
        }
    }
}
