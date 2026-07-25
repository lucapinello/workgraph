//! Telegram notification channel implementation using teloxide.
//!
//! Implements [`NotificationChannel`] for Telegram bots. Supports:
//! - Outbound: text, rich (Markdown), and action-button messages (inline keyboards)
//! - Inbound: long-polling listener that yields [`IncomingMessage`]s
//!
//! # Configuration
//!
//! Two forms are accepted (and may coexist) under the `[telegram]` section
//! of `notify.toml`:
//!
//! **Legacy single-bot** — kept for backwards compatibility:
//!
//! ```toml
//! [telegram]
//! bot_token = "123456:ABC-DEF..."
//! chat_id = "12345678"
//! ```
//!
//! This synthesises one bot keyed `"default"` whose [`NotificationChannel::channel_type`]
//! returns `"telegram"` (so existing routing rules like `default = ["telegram"]`
//! keep working without edits).
//!
//! **Multi-bot** — one bot per persistent named agent (the family-team
//! experiment shape: `@nora_planner_bot`, `@bruno_chef_bot`, etc.):
//!
//! ```toml
//! [telegram.bots.nora]
//! bot_token = "123456:ABC..."
//! chat_id   = "78901234"
//! agent_id  = "nora"        # workgraph agent this bot fronts (optional)
//!
//! [telegram.bots.bruno]
//! bot_token = "654321:XYZ..."
//! chat_id   = "78901234"
//! agent_id  = "bruno"
//! ```
//!
//! Each named bot registers as a distinct channel whose `channel_type()` is
//! `"telegram:<bot_id>"`, so the router can address them independently.
//! Inbound messages are tagged with the receiving bot's qualified type so the
//! awaiting-human task router can route replies to the right open task. That
//! router now lives in `commands::service::human_dispatch::route_inbound_reply`
//! (R13): it maps the receiving bot's `channel_type()` back to the human agent
//! it fronts and records the reply on that agent's parked task, satisfying the
//! task's `WaitCondition::HumanInput`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Action, ActionStyle, IncomingMessage, MessageId, NotificationChannel, RichMessage};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Per-bot configuration. One of these is what each `[telegram.bots.<id>]`
/// table parses into; one is also synthesised from the legacy top-level
/// `[telegram] bot_token` + `chat_id` fields when present.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TelegramBotConfig {
    pub bot_token: String,
    pub chat_id: String,
    /// Workgraph agent id this bot fronts (e.g. `"nora"`). When `None`, the
    /// bot is a shared/group bot — outbound routing falls back to it when
    /// no agent-specific bot matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The bot's Telegram @username (e.g. `"bruno_chef_bot"`), WITHOUT the
    /// leading `@`. Used to map a group @mention back to this bot (and thus
    /// its `agent_id`) without a live `getMe` call. When `None`, group
    /// @mention routing falls back to matching the `[telegram.bots.<id>]` key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

/// Telegram-specific configuration parsed from the `[telegram]` section.
///
/// Holds both legacy single-bot fields (for backwards compat) and a
/// multi-bot map. They may coexist; [`TelegramConfig::all_bots`] resolves
/// them into a single ordered list of `(bot_id, TelegramBotConfig)`.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct TelegramConfig {
    /// Legacy single-bot token. Empty when the config uses only the
    /// multi-bot `bots` map.
    #[serde(default)]
    pub bot_token: String,
    /// Legacy single-bot chat id. Empty when the config uses only the
    /// multi-bot `bots` map.
    #[serde(default)]
    pub chat_id: String,
    /// Multi-bot map. Key is the bot id (free-form identifier used in
    /// `channel_type()` as `"telegram:<bot_id>"`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub bots: HashMap<String, TelegramBotConfig>,
}

impl TelegramConfig {
    /// Extract from the opaque channel map in [`super::config::NotifyConfig`].
    pub fn from_notify_config(config: &super::config::NotifyConfig) -> Result<Self> {
        let val = config
            .channels
            .get("telegram")
            .context("no [telegram] section in notify config")?;
        let cfg: Self = val
            .clone()
            .try_into()
            .context("invalid [telegram] config")?;
        Ok(cfg)
    }

    /// Resolve all configured bots into a flat list. The legacy single-bot
    /// fields, when both `bot_token` and `chat_id` are non-empty, contribute
    /// one entry keyed `"default"` (with no agent binding); each entry of
    /// the `bots` map contributes its own entry. Iteration order is:
    /// legacy first (when present), then the named bots in their insertion
    /// order.
    ///
    /// Returns an empty vec when the config has no usable bot — callers
    /// should treat this as "telegram is not configured."
    pub fn all_bots(&self) -> Vec<(String, TelegramBotConfig)> {
        let mut out = Vec::new();
        if !self.bot_token.is_empty() && !self.chat_id.is_empty() {
            out.push((
                "default".to_string(),
                TelegramBotConfig {
                    bot_token: self.bot_token.clone(),
                    chat_id: self.chat_id.clone(),
                    agent_id: None,
                    username: None,
                },
            ));
        }
        for (id, cfg) in &self.bots {
            out.push((id.clone(), cfg.clone()));
        }
        out
    }
}

/// Loudly flag a `chat_id` that CANNOT be the family group.
///
/// Per the Telegram Bot API, **group / supergroup / channel ids are negative**
/// (supergroups begin `-100…`) while a **private 1:1 chat id is the positive
/// user id**. The family relay is meant to post into the FAMILY GROUP — the same
/// negative id for every bot. A *positive* `chat_id` where a group is expected
/// silently DMs one person instead: the kiosk→group relay reports success
/// (`relayError: null`) while the group sees nothing (docs/05 §5.4 ground truth).
///
/// Returns `Some(warning)` describing the misconfiguration when `chat_id` parses
/// as a positive integer (a DM target), or `None` when it is a plausible group
/// target (negative id), unset/empty (a separate "not configured" concern), or a
/// non-numeric `@channelusername`. `bot_id` only makes the warning specific.
pub fn group_chat_id_warning(bot_id: &str, chat_id: &str) -> Option<String> {
    let trimmed = chat_id.trim();
    match trimmed.parse::<i64>() {
        // Negative → group / supergroup / channel: the expected relay target.
        Ok(n) if n < 0 => None,
        // Positive → a 1:1 DM id. This is the silent-misroute footgun.
        Ok(n) => Some(format!(
            "bot '{bot_id}': chat_id={n} is a POSITIVE id — that is a 1:1 DM, not the family \
             GROUP. Group ids are negative (supergroups begin -100…). A kiosk→group relay will \
             SILENTLY DM one person instead of posting to the group (relayError stays null). Fix \
             .wg/notify.toml: set chat_id to the negative family-group id (docs/05 §5.4)."
        )),
        // Empty (unconfigured) or @channelusername — not the positive-DM footgun.
        Err(_) => None,
    }
}

/// True when `chat_id` is a Telegram **1:1 DM** target — a positive user id.
///
/// Per the Bot API, group / supergroup / channel ids are negative (supergroups
/// begin `-100…`); a private chat id is the positive user id. An empty string or
/// a non-numeric `@channelusername` is neither a DM nor the footgun. This is the
/// predicate behind [`group_chat_id_warning`], exposed so routing code (e.g. the
/// clarify emission) can REFUSE to target a bare human DM and fall back to the
/// family group instead. See task `nora-clarify-engine` / docs/05 §5.4.
pub fn is_dm_chat_id(chat_id: &str) -> bool {
    matches!(chat_id.trim().parse::<i64>(), Ok(n) if n > 0)
}

/// Emit [`group_chat_id_warning`] to stderr for every configured bot at
/// listener/gateway start, so a misrouting `chat_id` is caught LOUDLY on boot
/// rather than discovered when the family never gets a relayed message. Returns
/// the number of warnings emitted (0 = every chat_id is a plausible group).
pub fn warn_on_dm_chat_ids(bots: &[(String, TelegramBotConfig)]) -> usize {
    let mut warned = 0;
    for (bot_id, cfg) in bots {
        if let Some(w) = group_chat_id_warning(bot_id, &cfg.chat_id) {
            eprintln!("⚠️  telegram chat_id misconfiguration — {w}");
            warned += 1;
        }
    }
    warned
}

// ---------------------------------------------------------------------------
// Channel implementation
// ---------------------------------------------------------------------------

/// A Telegram notification channel backed by the Telegram Bot API via `reqwest`.
///
/// This uses the Bot API directly over HTTP rather than pulling in the full
/// teloxide runtime, keeping the non-listener path lightweight.
///
/// One instance per bot. For multi-bot setups (one bot per persistent named
/// agent), build N `TelegramChannel` instances via
/// [`TelegramChannel::all_from_notify_config`] — each gets its own long-poll
/// task on `listen()` and its own `channel_type()` discriminator.
pub struct TelegramChannel {
    bot_id: String,
    bot: TelegramBotConfig,
    /// Pre-computed channel-type string. The legacy `"default"` bot returns
    /// the bare `"telegram"` so existing routing rules remain valid; named
    /// bots return `"telegram:<bot_id>"` so the router can address them
    /// distinctly.
    channel_type: String,
    client: reqwest::Client,
    /// Where this bot's poll task publishes its health (see
    /// [`super::listener_health`]). `None` (the default) keeps health
    /// publishing off — used by the legacy single-bot `listen()` and by tests
    /// that do not care. The multi-bot listener sets it to the project's
    /// `.wg` dir so `wg service status` and the casa supervisor can SEE a deaf
    /// listener instead of it existing only as log noise.
    health_dir: Option<std::path::PathBuf>,
}

impl TelegramChannel {
    /// Construct a single-bot channel from the legacy `[telegram]` block
    /// (`bot_token` + `chat_id` at the top level).
    ///
    /// Kept for backwards compatibility — existing CLI commands
    /// (`wg telegram listen / send / status`) call this with a
    /// [`TelegramConfig`] populated only from the legacy fields. New code
    /// driving multi-bot setups should prefer [`TelegramChannel::from_bot`]
    /// or [`TelegramChannel::all_from_notify_config`].
    pub fn new(config: TelegramConfig) -> Self {
        Self::from_bot(
            "default".to_string(),
            TelegramBotConfig {
                bot_token: config.bot_token,
                chat_id: config.chat_id,
                agent_id: None,
                username: None,
            },
        )
    }

    /// Construct a channel for a specific bot. The `bot_id` is the key from
    /// the `[telegram.bots.<id>]` table (or the literal `"default"` for the
    /// legacy single-bot case).
    pub fn from_bot(bot_id: String, bot: TelegramBotConfig) -> Self {
        let channel_type = if bot_id == "default" {
            "telegram".to_string()
        } else {
            format!("telegram:{}", bot_id)
        };
        Self {
            bot_id,
            bot,
            channel_type,
            client: build_poll_client(),
            health_dir: None,
        }
    }

    /// Publish this bot's poll health under `wg_dir` (the project's `.wg`
    /// directory) so a *different* process can tell a deaf listener from a
    /// healthy one.
    ///
    /// Builder-style rather than a constructor argument because only the
    /// multi-bot listener knows the project dir; every other construction site
    /// (send-only paths, tests) legitimately has no dir and must keep working
    /// unchanged.
    pub fn publishing_health_to(mut self, wg_dir: impl Into<std::path::PathBuf>) -> Self {
        self.health_dir = Some(wg_dir.into());
        self
    }

    /// Build all configured Telegram channels from a [`super::config::NotifyConfig`].
    ///
    /// Iterates [`TelegramConfig::all_bots`] and constructs one channel per
    /// entry. Returns an empty vec when no `[telegram]` section is present
    /// (callers should treat that as "telegram not configured" rather than an
    /// error — same behaviour as if the section were absent in legacy code).
    pub fn all_from_notify_config(config: &super::config::NotifyConfig) -> Result<Vec<Self>> {
        if !config.channels.contains_key("telegram") {
            return Ok(Vec::new());
        }
        let cfg = TelegramConfig::from_notify_config(config)?;
        Ok(cfg
            .all_bots()
            .into_iter()
            .map(|(id, bot)| Self::from_bot(id, bot))
            .collect())
    }

    /// The bot id (`"default"` for the legacy single-bot, otherwise the user-
    /// supplied key from `[telegram.bots.<id>]`).
    pub fn bot_id(&self) -> &str {
        &self.bot_id
    }

    /// The workgraph agent id this bot fronts, when bound. `None` for the
    /// legacy default bot (and for any named bot configured without an
    /// `agent_id` field).
    pub fn agent_id(&self) -> Option<&str> {
        self.bot.agent_id.as_deref()
    }

    /// The default chat id this bot writes to (the bot's own DM thread or
    /// the configured group chat, depending on the bot's role).
    pub fn chat_id(&self) -> &str {
        &self.bot.chat_id
    }

    /// A redacted preview of the bot token suitable for logs and JSON output —
    /// `"123456...XYZ"`. Avoids exposing the full token while still letting an
    /// operator visually distinguish bots when reading `wg telegram list-bots`.
    pub fn bot_token_preview(&self) -> String {
        let t = &self.bot.bot_token;
        if t.len() <= 10 {
            return "(unset)".to_string();
        }
        format!("{}...{}", &t[..6], &t[t.len().saturating_sub(4)..])
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            telegram_api_base(),
            self.bot.bot_token,
            method,
        )
    }

    /// Send a request to the Telegram Bot API and return the result.
    pub async fn api_call(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post(self.api_url(method))
            .json(body)
            .send()
            .await
            .context("Telegram API request failed")?;

        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Telegram API response")?;

        if !status.is_success() || json.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let desc = json
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Telegram API error ({}): {}", status, desc);
        }

        Ok(json)
    }

    /// Register this bot's slash-command menu via the Telegram `setMyCommands`
    /// API, so the commands autocomplete when a user types `/` in any chat the
    /// bot is in. `commands` is a list of `(name, description)` pairs — the name
    /// is the bare command WITHOUT the leading slash (`"dinner"`), lowercase,
    /// as the Bot API requires. Every bot registers the full shared set so any
    /// bot can receive a `/command`; the listener's election then decides who
    /// actually answers. Returns the raw API response (contains no token).
    pub async fn set_my_commands(&self, commands: &[(String, String)]) -> Result<serde_json::Value> {
        let cmds: Vec<serde_json::Value> = commands
            .iter()
            .map(|(name, desc)| serde_json::json!({ "command": name, "description": desc }))
            .collect();
        let body = serde_json::json!({ "commands": cmds });
        self.api_call("setMyCommands", &body).await
    }

    /// Read back this bot's registered command menu via `getMyCommands` — used
    /// to VERIFY a `set_my_commands` call landed. Returns the raw API response
    /// (a `result` array of `{command, description}`); contains no token.
    pub async fn get_my_commands(&self) -> Result<serde_json::Value> {
        self.api_call("getMyCommands", &serde_json::json!({})).await
    }

    /// Edit an already-sent message in place via the Telegram `editMessageText`
    /// API. Used by the conversational composer to turn the "On it — one sec…"
    /// latency ack INTO the final answer (or a graceful "glitched" line) rather
    /// than leaving a stale hourglass and posting a second message. `message_id`
    /// is the id returned by a prior [`send_text`](NotificationChannel::send_text);
    /// a non-numeric/empty id is a no-op-safe error the caller falls back from by
    /// sending a fresh message. Contains no token in its return value.
    pub async fn edit_text(&self, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        let mid: i64 = message_id
            .trim()
            .parse()
            .with_context(|| format!("editMessageText: non-numeric message_id {message_id:?}"))?;
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": mid,
            "text": text,
        });
        self.api_call("editMessageText", &body).await?;
        Ok(())
    }

    /// Extract the message_id from a sendMessage response.
    fn extract_message_id(json: &serde_json::Value) -> MessageId {
        let mid = json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64())
            .unwrap_or(0);
        MessageId(mid.to_string())
    }

    /// Download a photo this bot received (`getFile` → GET the file URL) to
    /// `dest`, enforcing a `max_file_size` byte cap BEFORE spending bandwidth.
    /// The `file_id` is bot-specific, so this MUST be called on the channel of
    /// the bot that received the photo. Both URLs embed the bot token, so any
    /// transport error is scrubbed through [`redact_bot_token`] before it can be
    /// returned/logged — the token never leaks.
    pub async fn download_photo_to(
        &self,
        file_id: &str,
        dest: &std::path::Path,
        max_file_size: u64,
    ) -> Result<()> {
        // 1. Resolve the on-server file path (and its declared size).
        let resp = self
            .api_call("getFile", &serde_json::json!({ "file_id": file_id }))
            .await?;
        let result = resp.get("result").context("getFile: no result")?;
        if let Some(size) = result.get("file_size").and_then(|s| s.as_u64()) {
            if size > max_file_size {
                anyhow::bail!("photo {size} bytes exceeds max file size {max_file_size}");
            }
        }
        let file_path = result
            .get("file_path")
            .and_then(|p| p.as_str())
            .context("getFile: result missing file_path")?;

        // 2. Download the bytes (scrub the token from any transport error).
        let file_url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot.bot_token, file_path
        );
        let bytes = self
            .client
            .get(&file_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(redact_bot_token(&format!("{e:#}"))))?
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!(redact_bot_token(&format!("{e:#}"))))?;

        if bytes.len() as u64 > max_file_size {
            anyhow::bail!(
                "downloaded photo {} bytes exceeds max file size {max_file_size}",
                bytes.len()
            );
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(dest, &bytes)
            .with_context(|| format!("failed to write photo to {}", dest.display()))?;
        Ok(())
    }

    /// Download a file this bot received (`getFile` → GET the file URL) and
    /// return its raw bytes, enforcing `max_file_size` BEFORE spending
    /// bandwidth. Used for voice notes / audio recordings, whose bytes are
    /// POSTed straight to the gateway's transcriber (no on-disk temp). The
    /// `file_id` is bot-specific, so this MUST be called on the channel of the
    /// bot that received the recording. Both URLs embed the bot token, so any
    /// transport error is scrubbed through [`redact_bot_token`] before it can be
    /// returned/logged — the token never leaks.
    pub async fn download_file_bytes(
        &self,
        file_id: &str,
        max_file_size: u64,
    ) -> Result<Vec<u8>> {
        // 1. Resolve the on-server file path (and its declared size).
        let resp = self
            .api_call("getFile", &serde_json::json!({ "file_id": file_id }))
            .await?;
        let result = resp.get("result").context("getFile: no result")?;
        if let Some(size) = result.get("file_size").and_then(|s| s.as_u64()) {
            if size > max_file_size {
                anyhow::bail!("recording {size} bytes exceeds max file size {max_file_size}");
            }
        }
        let file_path = result
            .get("file_path")
            .and_then(|p| p.as_str())
            .context("getFile: result missing file_path")?;

        // 2. Download the bytes (scrub the token from any transport error).
        let file_url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot.bot_token, file_path
        );
        let bytes = self
            .client
            .get(&file_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!(redact_bot_token(&format!("{e:#}"))))?
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!(redact_bot_token(&format!("{e:#}"))))?;

        if bytes.len() as u64 > max_file_size {
            anyhow::bail!(
                "downloaded recording {} bytes exceeds max file size {max_file_size}",
                bytes.len()
            );
        }
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl super::telegram_photo::PhotoDownloader for TelegramChannel {
    async fn download(&self, file_id: &str, dest: &std::path::Path) -> Result<()> {
        let max = super::telegram_photo::PhotoLimits::default().max_file_size;
        self.download_photo_to(file_id, dest, max).await
    }
}

#[async_trait]
impl super::telegram_voice::VoiceDownloader for TelegramChannel {
    async fn download_bytes(&self, file_id: &str) -> Result<Vec<u8>> {
        let max = super::telegram_voice::VoiceLimits::default().max_file_size;
        self.download_file_bytes(file_id, max).await
    }
}

#[async_trait]
impl NotificationChannel for TelegramChannel {
    fn channel_type(&self) -> &str {
        &self.channel_type
    }

    async fn send_text(&self, target: &str, message: &str) -> Result<MessageId> {
        let body = serde_json::json!({
            "chat_id": target,
            "text": message,
        });
        let resp = self.api_call("sendMessage", &body).await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_rich(&self, target: &str, message: &RichMessage) -> Result<MessageId> {
        // Prefer Markdown, fall back to HTML, then plain text.
        let (text, parse_mode) = if let Some(ref md) = message.markdown {
            (md.clone(), Some("MarkdownV2"))
        } else if let Some(ref html) = message.html {
            (html.clone(), Some("HTML"))
        } else {
            (message.plain_text.clone(), None)
        };

        let mut body = serde_json::json!({
            "chat_id": target,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::Value::String(mode.to_string());
        }

        let resp = self.api_call("sendMessage", &body).await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_with_actions(
        &self,
        target: &str,
        message: &str,
        actions: &[Action],
    ) -> Result<MessageId> {
        // Build inline keyboard from actions.
        let buttons: Vec<serde_json::Value> = actions
            .iter()
            .map(|a| {
                serde_json::json!({
                    "text": format_button_label(&a.label, a.style),
                    "callback_data": &a.id,
                })
            })
            .collect();

        let body = serde_json::json!({
            "chat_id": target,
            "text": message,
            "reply_markup": {
                "inline_keyboard": [buttons],
            },
        });

        let resp = self.api_call("sendMessage", &body).await?;
        Ok(Self::extract_message_id(&resp))
    }

    fn supports_receive(&self) -> bool {
        true
    }

    async fn listen(&self) -> Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        // Legacy single-bot path: poll this one bot into its own receiver,
        // with an in-memory offset that resets to 0 on restart. Multi-bot
        // listeners use [`TelegramChannel::spawn_poll`] instead, which shares
        // one receiver across every bot and persists each bot's offset.
        self.spawn_poll(tx, None);
        Ok(rx)
    }
}

impl TelegramChannel {
    /// Long-poll THIS bot's `getUpdates` queue, forwarding every decoded
    /// [`IncomingMessage`] into `tx`.
    ///
    /// This is the shared engine behind both the legacy single-bot
    /// [`listen`](NotificationChannel::listen) and the multi-bot listener in
    /// `commands::telegram::run_listen`. The multi-bot caller builds one
    /// [`TelegramChannel`] per configured bot (see
    /// [`all_from_notify_config`](Self::all_from_notify_config)), then calls
    /// `spawn_poll` on each with a *clone of one shared `tx`* — so a single
    /// routing pipeline consumes every bot's queue concurrently. Each
    /// [`IncomingMessage`] carries this bot's `channel_type` tag so the
    /// downstream router knows which bot received it.
    ///
    /// `offset_path`:
    /// - `Some(path)` — persist the `getUpdates` offset to `path` (one file
    ///   per bot, keyed by `bot_id` by the caller) so a listener restart does
    ///   not replay already-acknowledged updates. The offset is seeded from
    ///   the file at startup and rewritten after each update is consumed.
    /// - `None` — keep the offset in memory only (legacy `listen` behaviour).
    ///
    /// Returns the spawned task handle so the caller can await/abort it.
    pub fn spawn_poll(
        &self,
        tx: tokio::sync::mpsc::Sender<IncomingMessage>,
        offset_path: Option<std::path::PathBuf>,
    ) -> tokio::task::JoinHandle<()> {
        let bot = self.bot.clone();
        // Reuse ONE reqwest client across every poll iteration (its clone
        // shares the underlying connection pool). `mut` so we can REBUILD it
        // after a sustained failure streak — see `POLL_REBUILD_AFTER`.
        let mut client = self.client.clone();
        // Pre-compute the channel-type tag so each IncomingMessage carries
        // the bot identity ("telegram" for the legacy bot, "telegram:<id>"
        // for named ones). The awaiting-human router
        // (`commands::service::human_dispatch::route_inbound_reply`) uses this
        // to decide which open `awaiting-human` task should receive the reply.
        let channel_tag = self.channel_type.clone();
        let bot_id = self.bot_id.clone();
        // Where to publish poll health, if this channel was built with a dir.
        let health_dir = self.health_dir.clone();

        tokio::spawn(async move {
            let mut offset: i64 = offset_path.as_deref().map(load_offset).unwrap_or(0);
            // Tracks the consecutive-failure streak; drives exponential backoff
            // and the periodic client rebuild.
            let mut backoff_state = PollBackoffState::default();
            // The failure kind we have already diagnosed out loud for this
            // episode. Cleared on recovery so a NEW outage always gets a fresh
            // diagnosis line rather than being deduped against the last one.
            let mut diagnosed: Option<super::listener_health::PollFailureKind> = None;

            loop {
                let updates = match get_updates_once(
                    &client,
                    TELEGRAM_API_BASE,
                    &bot.bot_token,
                    offset,
                    POLL_TIMEOUT_SECS,
                )
                .await
                {
                    Ok(updates) => {
                        // Recovery breadcrumb: log once when a bot starts
                        // succeeding again after a failure streak, so an
                        // operator can see the wedge clear without having to
                        // notice the *absence* of error lines.
                        if let Some(streak) = backoff_state.on_success() {
                            eprintln!(
                                "polling {} resumed after {} consecutive failure(s)",
                                bot_id, streak
                            );
                            diagnosed = None;
                        }
                        // Publish the success so an external reader can tell
                        // "quiet but healthy" from "wedged". Best-effort: a
                        // health-file write must never break polling.
                        if let Some(ref hd) = health_dir {
                            let _ = super::listener_health::record_success(
                                hd,
                                &bot_id,
                                chrono::Utc::now(),
                            );
                        }
                        updates
                    }
                    Err(e) => {
                        let action = backoff_state.on_failure();
                        let rendered = redact_bot_token(&format!("{e:#}"));
                        // A reqwest transport error's `Display` embeds the full
                        // request URL — which contains the bot token
                        // (`.../bot<token>/getUpdates`). Redact it before it
                        // reaches the log file; keep the bot NAME (`bot_id`)
                        // and the error chain otherwise intact.
                        eprintln!(
                            "polling {} error (failure #{}, backing off {}s): {}",
                            bot_id,
                            backoff_state.consecutive_failures,
                            action.backoff.as_secs(),
                            rendered
                        );
                        // Publish health + emit ONE loud, actionable diagnosis
                        // per (bot, failure-kind) episode. The 2026-07-24
                        // incident produced 889 identical raw transport lines
                        // and not one line saying what to do about them; the
                        // raw error repeats for the backoff ladder's benefit,
                        // the diagnosis prints once so it stays findable.
                        let kind = super::listener_health::PollFailureKind::classify(&rendered);
                        if let Some(ref hd) = health_dir {
                            let _ = super::listener_health::record_failure(
                                hd,
                                &bot_id,
                                backoff_state.consecutive_failures,
                                &rendered,
                                chrono::Utc::now(),
                            );
                        }
                        let deaf_now = backoff_state.consecutive_failures
                            >= super::listener_health::DEAF_AFTER_FAILURES;
                        if deaf_now && diagnosed != Some(kind) {
                            diagnosed = Some(kind);
                            eprintln!(
                                "polling {bot_id}: DEAF after {} consecutive failures [{}] — {}",
                                backoff_state.consecutive_failures,
                                kind.label(),
                                kind.diagnosis()
                            );
                        }
                        // After a sustained streak the connection pool itself
                        // may be wedged (half-dead sockets, a lost IPv6 path,
                        // leaked FDs). Rebuilding drops every pooled connection
                        // and forces fresh DNS + happy-eyeballs on the next
                        // poll — recovering from failures a single reused
                        // connection cannot.
                        if action.rebuild_client {
                            eprintln!(
                                "polling {}: rebuilding HTTP client after {} consecutive failures",
                                bot_id, backoff_state.consecutive_failures
                            );
                            client = build_poll_client();
                        }
                        tokio::time::sleep(action.backoff).await;
                        continue;
                    }
                };

                for update in &updates {
                    if let Some(uid) = update.get("update_id").and_then(|u| u.as_i64()) {
                        offset = uid + 1;
                        // Persist BEFORE dispatch so a crash mid-processing
                        // still advances the cursor (the update was pulled off
                        // Telegram's queue the moment we sent `offset`).
                        if let Some(ref path) = offset_path {
                            save_offset(path, offset);
                        }
                    }

                    if let Some(msg) = decode_update(update, &channel_tag) {
                        // One line per handled message: which bot received a
                        // message from whom, in which chat. Closes the earlier
                        // observability gap where inbound traffic was invisible
                        // in the logs until a reply was sent.
                        eprintln!(
                            "polling {}: handling message from {} in chat {}",
                            bot_id,
                            msg.sender,
                            msg.chat_id.as_deref().unwrap_or("?")
                        );
                        if tx.send(msg).await.is_err() {
                            return; // receiver dropped — stop polling this bot
                        }
                    }
                }
            }
        })
    }
}

/// Telegram Bot API base URL. A constant (rather than inlined) so the
/// resilience tests can point [`get_updates_once`] at a local mock server.
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Optional loopback-only Telegram API base used by hermetic real-binary
/// scenarios. The production default remains Telegram itself; an override is
/// accepted only for plain HTTP on an exact loopback IP, so a stray environment
/// value cannot redirect bot credentials to another host.
const TELEGRAM_API_BASE_OVERRIDE: &str = "WG_TELEGRAM_API_BASE";

fn validated_telegram_api_base(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value.trim()).ok()?;
    if url.scheme() != "http"
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("::1") | Some("[::1]")
        )
    {
        return None;
    }
    let mut base = url.to_string();
    while base.ends_with('/') {
        base.pop();
    }
    Some(base)
}

fn telegram_api_base() -> String {
    std::env::var(TELEGRAM_API_BASE_OVERRIDE)
        .ok()
        .and_then(|value| validated_telegram_api_base(&value))
        .unwrap_or_else(|| TELEGRAM_API_BASE.to_string())
}

/// Server-side long-poll timeout (seconds) sent to `getUpdates`. Telegram holds
/// the request open up to this long when no update is pending, so the loop
/// blocks cheaply instead of hot-spinning.
const POLL_TIMEOUT_SECS: u64 = 30;

/// After this many *consecutive* failed polls, drop and rebuild the reqwest
/// client. A rebuild purges the connection pool — including any sockets stuck
/// half-closed after an IPv6/NAT path loss — and forces fresh DNS resolution,
/// recovering from a wedge that a single long-lived connection cannot.
const POLL_REBUILD_AFTER: u32 = 5;

/// Build the reqwest client shared by the send path and reused across every
/// long-poll iteration.
///
/// Why the explicit config matters (root cause of the recurring listener wedge,
/// task `listener-reconnect`): the poll loop long-polls `getUpdates` forever.
/// With the default client and NO request timeout, a half-dead socket (IPv6
/// path loss, NAT rebind) leaves the `send()` future hanging indefinitely while
/// its file descriptor stays open; with four bots polling, blocked/leaked FDs
/// eventually exhaust the process limit and *every* request fails until a
/// restart. A bounded `timeout` guarantees a wedged connection errors out
/// (freeing its FD) so the loop can back off and rebuild, and
/// `pool_max_idle_per_host(1)` keeps exactly one warm connection per bot so we
/// reuse — not reopen — a socket on each poll instead of leaking a fresh one.
pub(crate) fn build_poll_client() -> reqwest::Client {
    reqwest::Client::builder()
        // getUpdates long-polls up to POLL_TIMEOUT_SECS server-side; a 60s
        // ceiling bounds a wedged socket without cutting off a healthy poll.
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(1)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        // A builder failure is a TLS-backend init problem, not per-call — fall
        // back to the default client rather than taking the whole listener down.
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Strip Telegram bot tokens out of any string bound for a log line.
///
/// A reqwest transport error's `Display` includes the request URL, e.g.
/// `error sending request for url (https://api.telegram.org/bot123456:AA-Ee/getUpdates): ...`.
/// The `bot<id>:<secret>` segment IS the bot token — a credential that must
/// NEVER land in a log file (`.wg/notify.toml` discipline: tokens never in
/// git, graph, or logs, because logs get pasted into issues, monitors, and
/// chats). This rewrites every `/bot<token>/` URL segment to
/// `/bot<redacted>/`, leaving the rest of the message — including the bot NAME
/// already present in the line — untouched. Token-free strings pass through
/// unchanged. Route EVERY error string that can carry an API URL through here
/// before logging (poll loop, send/edit paths, `getMe`, `setMyCommands`).
pub fn redact_bot_token(s: &str) -> String {
    // Telegram tokens are `<digits>:<35+ url-safe base64 chars>`; in an API URL
    // they follow the literal `bot` path segment. Match that shape so a real
    // token is redacted while the bot NAME (e.g. `telegram:otto`) — which has
    // no leading `bot<digits>:` — is preserved.
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"bot\d+:[A-Za-z0-9_-]+")
            .expect("bot-token redaction regex is a valid literal")
    });
    re.replace_all(s, "bot<redacted>").into_owned()
}

/// Perform ONE `getUpdates` long-poll round-trip against `api_base`, returning
/// the decoded `result` array (empty when the response carries no updates).
/// Transport errors and non-JSON bodies propagate so the caller can apply
/// backoff + client rebuild.
///
/// Extracted from [`TelegramChannel::spawn_poll`] so the FD-stability test can
/// drive it in a tight loop against a local mock and assert the process's
/// open-socket count stays flat across iterations.
async fn get_updates_once(
    client: &reqwest::Client,
    api_base: &str,
    bot_token: &str,
    offset: i64,
    timeout_secs: u64,
) -> Result<Vec<serde_json::Value>> {
    let url = format!("{}/bot{}/getUpdates", api_base, bot_token);
    let body = serde_json::json!({
        "offset": offset,
        "timeout": timeout_secs,
        "allowed_updates": ["message", "callback_query"],
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("getUpdates request failed")?;
    let json: serde_json::Value = resp
        .json()
        .await
        .context("getUpdates response was not valid JSON")?;
    Ok(json
        .get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Exponential backoff for the poll loop: `2^(failures-1)` seconds, capped at
/// 60s. `failures` is the count of *consecutive* failures (1 on the first), so
/// the sequence is 1, 2, 4, 8, 16, 32, 60, 60, … A single blip costs ~1s while
/// a sustained outage settles at one retry per minute.
fn poll_backoff(failures: u32) -> std::time::Duration {
    let secs = 1u64
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u64::MAX)
        .min(60);
    std::time::Duration::from_secs(secs)
}

/// What the poll loop should do after a failed poll.
#[derive(Debug, PartialEq, Eq)]
struct FailureAction {
    /// How long to sleep before the next attempt.
    backoff: std::time::Duration,
    /// Whether to drop and rebuild the reqwest client before the next attempt.
    rebuild_client: bool,
}

/// Failure-streak state for the poll loop, extracted from [`spawn_poll`] so the
/// streak → backoff → rebuild → recovery transitions are unit-testable without
/// touching the network. Each bot's loop owns exactly one.
#[derive(Debug, Default)]
struct PollBackoffState {
    /// Number of consecutive failed polls; reset to 0 on any success.
    consecutive_failures: u32,
}

impl PollBackoffState {
    /// Record a successful poll. Returns `Some(streak)` — the length of the
    /// failure streak that just ended — when recovering (so the caller can emit
    /// the "resumed" breadcrumb), or `None` on a normal success.
    fn on_success(&mut self) -> Option<u32> {
        if self.consecutive_failures > 0 {
            let streak = self.consecutive_failures;
            self.consecutive_failures = 0;
            Some(streak)
        } else {
            None
        }
    }

    /// Record a failed poll and return the resulting backoff plus whether the
    /// client should be rebuilt (every [`POLL_REBUILD_AFTER`] consecutive
    /// failures).
    fn on_failure(&mut self) -> FailureAction {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        FailureAction {
            backoff: poll_backoff(self.consecutive_failures),
            rebuild_client: self.consecutive_failures % POLL_REBUILD_AFTER == 0,
        }
    }
}

/// Decode a single Telegram `getUpdates` element into an [`IncomingMessage`],
/// tagging it with `channel_tag` (the receiving bot's channel type). Returns
/// `None` for updates that are neither a callback query nor a text message.
///
/// Extracted from the poll loop so the single-bot and multi-bot pollers share
/// exactly one decoder — the two must never diverge in how they parse a
/// button press vs. a group @mention. Public so the `wg telegram decide`
/// diagnostic can run the SAME parse the live listener uses on a raw update.
pub fn decode_update(update: &serde_json::Value, channel_tag: &str) -> Option<IncomingMessage> {
    // Handle callback queries (button presses)
    if let Some(cb) = update.get("callback_query") {
        // Sender identity (id + username + is_bot), read once at the boundary so
        // a button press from a human with no @username still resolves to their
        // binding rather than decoding to "unknown". See `telegram_sender`.
        let identity = super::telegram_sender::extract_sender(update);
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
            .map(|mid| MessageId(mid.to_string()));

        // A button press carries the chat it was pressed in so
        // the response goes back to that chat (group or DM).
        let chat_id = cb
            .get("message")
            .and_then(|m| m.get("chat"))
            .and_then(|c| c.get("id"))
            .and_then(|id| id.as_i64())
            .map(|id| id.to_string());
        let chat_type = cb
            .get("message")
            .and_then(|m| m.get("chat"))
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());

        return Some(IncomingMessage {
            channel: channel_tag.to_string(),
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
            // Button presses are 1:1 with the bot whose message carried the
            // button, so they never arrive four times — no dedupe key needed.
            message_id: None,
            chat_id,
            chat_type,
            mention_usernames: Vec::new(),
            reply_to_bot: None,
            // A button press is an action, not a slash command.
            has_bot_command: false,
            // A button press carries no photo.
            photo_file_id: None,
            media_group_id: None,
            // A button press carries no audio recording.
            voice_file_id: None,
            voice_mime: None,
        });
    }

    // Handle regular messages
    if let Some(message) = update.get("message") {
        // Sender identity (id + username + is_bot), read once at the boundary.
        // `sender` is the display label (username → id → "unknown"); the numeric
        // id and bot flag ride alongside for binding resolution and the Fix #0
        // bot-loop guard. See `telegram_sender`.
        let identity = super::telegram_sender::identity_from_message(message);
        let sender = identity.display();

        // A photo message carries no `text`; its human-typed words (if any) ride
        // in `caption`. Treat the caption as the body so a captioned photo routes
        // exactly like text — `bruno what do we still need?` on a fridge photo
        // elects Bruno by the same name/mention rules. Prefer `text` when present
        // (a message is never both), else fall back to `caption`.
        let body = message
            .get("text")
            .and_then(|t| t.as_str())
            .or_else(|| message.get("caption").and_then(|c| c.as_str()))
            .unwrap_or("")
            .to_string();

        // Photo intake: Telegram sends an array of rendered sizes, smallest
        // first. The last element is the largest; its `file_id` is the download
        // handle. A message without a `photo` array is text/other. See
        // `super::telegram_photo`.
        let photo_file_id = super::telegram_photo::largest_photo_file_id(message);
        let media_group_id = message
            .get("media_group_id")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());

        // Audio intake: a `voice` note (OGG/Opus), an `audio` file, or a
        // `video_note` carries no `text` — its words are SPOKEN. Pull the
        // download handle + declared mime so the listener can fetch the bytes
        // and hand them to the gateway's whisper. See `super::telegram_voice`.
        let voice = super::telegram_voice::voice_meta(message);
        let (voice_file_id, voice_mime) = match voice {
            Some(v) => (Some(v.file_id), v.mime_type),
            None => (None, None),
        };

        let reply_to = message
            .get("reply_to_message")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64())
            .map(|mid| MessageId(mid.to_string()));

        // This message's own id — the cross-bot dedupe key half. Stable across
        // every bot that received this same physical group message.
        let message_id = message
            .get("message_id")
            .and_then(|m| m.as_i64())
            .map(|m| m.to_string());

        // Chat context for group @mention routing (R17): the
        // chat id is the reply target (in a group, the group
        // itself — never the bot's default chat) and the chat
        // type drives privacy-mode filtering downstream.
        let chat_id = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(|id| id.as_i64())
            .map(|id| id.to_string());
        let chat_type = message
            .get("chat")
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        // A photo carries its words + entities under `caption`/`caption_entities`
        // instead of `text`/`entities`; fall back to those so a captioned photo's
        // @mention and leading-command signals parse identically to text.
        let entities = message
            .get("entities")
            .or_else(|| message.get("caption_entities"))
            .unwrap_or(&serde_json::Value::Null);
        let mention_usernames =
            super::telegram_group::parse_mention_usernames(&body, entities);
        // A genuine leading `/slash` command carries a `bot_command` entity at
        // offset 0 — the ONLY signal we treat as "this is a command". A bare `?`
        // or ordinary chatter has none. See `fix-command-leaks`.
        let has_bot_command = super::telegram_group::has_leading_bot_command(entities);
        // Reply-chain: if this replies to a bot's own message,
        // name that bot so the reply routes to its agent.
        let reply_to_bot = super::telegram_group::reply_to_bot_username(message);

        return Some(IncomingMessage {
            channel: channel_tag.to_string(),
            sender,
            sender_id: identity.user_id,
            sender_is_bot: identity.is_bot,
            sent_at: message.get("date").and_then(|d| d.as_i64()),
            body,
            action_id: None,
            reply_to,
            message_id,
            chat_id,
            chat_type,
            mention_usernames,
            reply_to_bot,
            has_bot_command,
            photo_file_id,
            media_group_id,
            voice_file_id,
            voice_mime,
        });
    }

    None
}

/// Load a persisted `getUpdates` offset from `path`. A missing or malformed
/// file yields `0` (start from the front of the queue) — the same default the
/// in-memory poller uses on a cold start.
fn load_offset(path: &std::path::Path) -> i64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// Persist the next `getUpdates` offset to `path`, creating the parent
/// directory if needed. Failures are logged and swallowed — a listener that
/// cannot checkpoint its cursor must keep running (it will simply re-see
/// recent updates after a restart), never crash.
fn save_offset(path: &std::path::Path, offset: i64) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("Failed to create Telegram offset dir {}: {e}", parent.display());
            return;
        }
    }
    if let Err(e) = std::fs::write(path, offset.to_string()) {
        eprintln!("Failed to persist Telegram offset {}: {e}", path.display());
    }
}

/// Add a visual prefix to button labels based on style.
fn format_button_label(label: &str, style: ActionStyle) -> String {
    match style {
        ActionStyle::Primary => format!("✅ {label}"),
        ActionStyle::Danger => format!("❌ {label}"),
        ActionStyle::Secondary => label.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_config(token: &str, chat: &str) -> TelegramConfig {
        TelegramConfig {
            bot_token: token.into(),
            chat_id: chat.into(),
            bots: HashMap::new(),
        }
    }

    #[test]
    fn redact_bot_token_scrubs_reqwest_error_url() {
        // Exactly the poll-failure line that leaked 24 full tokens on
        // 2026-07-12: a reqwest transport error whose Display embeds the URL.
        let leaked = "getUpdates request failed: error sending request for url \
             (https://api.telegram.org/bot123456789:AAE-abc_DEF-gHIjkLmNoPqRstUvwx/getUpdates): \
             operation timed out";
        let redacted = redact_bot_token(leaked);
        assert!(
            !redacted.contains("123456789:AAE-abc_DEF-gHIjkLmNoPqRstUvwx"),
            "token must not survive redaction: {redacted}"
        );
        assert!(
            redacted.contains("/bot<redacted>/getUpdates"),
            "URL shape should be preserved with the token replaced: {redacted}"
        );
        // Surrounding context (method name, error tail) stays readable.
        assert!(redacted.contains("getUpdates request failed"));
        assert!(redacted.contains("operation timed out"));
    }

    #[test]
    fn redact_bot_token_scrubs_voice_file_download_url() {
        // The voice-note download (task telegram-voice-notes) GETs the file
        // API URL `https://api.telegram.org/file/bot<token>/<path>`, which
        // embeds the token. A transport error there is scrubbed through
        // `redact_bot_token` before it is returned/logged — the token from a
        // failed recording download must NEVER reach the logs.
        let leaked = "error sending request for url \
             (https://api.telegram.org/file/bot987654321:AAF-voice_TOKEN-xyz123/voice/file_9.oga): \
             connection reset";
        let redacted = redact_bot_token(leaked);
        assert!(
            !redacted.contains("987654321:AAF-voice_TOKEN-xyz123"),
            "voice download token must not survive redaction: {redacted}"
        );
        assert!(
            redacted.contains("/bot<redacted>/"),
            "file URL shape preserved with the token replaced: {redacted}"
        );
    }

    #[test]
    fn redact_bot_token_leaves_token_free_strings_unchanged() {
        let clean = "polling telegram:otto error (failure #3, backing off 4s): \
             connection refused";
        assert_eq!(redact_bot_token(clean), clean);
        // The human-facing bot NAME (`telegram:otto`, `bot_id`) must be kept —
        // it has no `bot<digits>:` shape so it is never touched.
        assert!(redact_bot_token(clean).contains("telegram:otto"));
    }

    #[test]
    fn redact_bot_token_scrubs_every_token_in_the_line() {
        // A single log line can mention the same URL twice (send + retry).
        let two = "bot111:AAAAAAAAAAAA and again bot222:BBBBBBBBBBBB";
        let redacted = redact_bot_token(two);
        assert_eq!(redacted, "bot<redacted> and again bot<redacted>");
    }

    #[test]
    fn telegram_config_from_toml() {
        let toml_str = r#"
[routing]
default = ["telegram"]

[telegram]
bot_token = "123:ABC"
chat_id = "456"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let tg = TelegramConfig::from_notify_config(&config).unwrap();
        assert_eq!(tg.bot_token, "123:ABC");
        assert_eq!(tg.chat_id, "456");
        assert!(
            tg.bots.is_empty(),
            "legacy form should not populate bots map"
        );
    }

    #[test]
    fn telegram_config_missing_section() {
        let config = super::super::config::NotifyConfig::default();
        assert!(TelegramConfig::from_notify_config(&config).is_err());
    }

    #[test]
    fn format_button_labels() {
        assert_eq!(
            format_button_label("Approve", ActionStyle::Primary),
            "✅ Approve"
        );
        assert_eq!(
            format_button_label("Reject", ActionStyle::Danger),
            "❌ Reject"
        );
        assert_eq!(format_button_label("Skip", ActionStyle::Secondary), "Skip");
    }

    #[test]
    fn channel_type_is_telegram() {
        // Legacy single-bot construction returns the bare "telegram" type so
        // existing routing rules (`default = ["telegram"]`) keep matching.
        let ch = TelegramChannel::new(legacy_config("test", "test"));
        assert_eq!(ch.channel_type(), "telegram");
        assert_eq!(ch.bot_id(), "default");
        assert_eq!(ch.agent_id(), None);
    }

    #[test]
    fn supports_receive_is_true() {
        let ch = TelegramChannel::new(legacy_config("test", "test"));
        assert!(ch.supports_receive());
    }

    #[test]
    fn api_url_format() {
        let ch = TelegramChannel::new(legacy_config("123:ABC", "456"));
        assert_eq!(
            ch.api_url("sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }

    #[test]
    fn telegram_api_override_accepts_only_plain_http_loopback() {
        assert_eq!(
            validated_telegram_api_base("http://127.0.0.1:7788/").as_deref(),
            Some("http://127.0.0.1:7788"),
        );
        assert_eq!(
            validated_telegram_api_base("http://[::1]:7788/").as_deref(),
            Some("http://[::1]:7788"),
        );
        for rejected in [
            "https://127.0.0.1:7788",
            "http://localhost:7788",
            "http://192.0.2.8:7788",
            "http://user:pass@127.0.0.1:7788",
            "not-a-url",
        ] {
            assert_eq!(
                validated_telegram_api_base(rejected),
                None,
                "unsafe test endpoint was accepted: {rejected}",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Multi-bot tests (R16): per-agent named bots, qualified channel types,
    // backwards-compat with the legacy single-bot form.
    // -----------------------------------------------------------------------

    #[test]
    fn parse_multi_bot_config() {
        // Three bots: legacy single (no agent_id, becomes "default"), plus
        // two named with agent bindings. all_bots() resolves them in order:
        // legacy first, then named.
        let toml_str = r#"
[routing]
default = ["telegram"]

[telegram]
bot_token = "111:AAA"
chat_id = "111"

[telegram.bots.nora]
bot_token = "222:BBB"
chat_id = "222"
agent_id = "nora"

[telegram.bots.bruno]
bot_token = "333:CCC"
chat_id = "333"
agent_id = "bruno"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let tg = TelegramConfig::from_notify_config(&config).unwrap();
        assert_eq!(tg.bot_token, "111:AAA");
        assert_eq!(tg.bots.len(), 2);

        let bots = tg.all_bots();
        assert_eq!(bots.len(), 3, "legacy + 2 named = 3 bots");
        assert_eq!(bots[0].0, "default", "legacy bot comes first");
        assert_eq!(bots[0].1.bot_token, "111:AAA");
        assert_eq!(bots[0].1.agent_id, None);

        // The named bots are in HashMap order — we just check both exist.
        let nora = bots.iter().find(|(id, _)| id == "nora").expect("nora bot");
        assert_eq!(nora.1.bot_token, "222:BBB");
        assert_eq!(nora.1.agent_id.as_deref(), Some("nora"));

        let bruno = bots
            .iter()
            .find(|(id, _)| id == "bruno")
            .expect("bruno bot");
        assert_eq!(bruno.1.bot_token, "333:CCC");
        assert_eq!(bruno.1.agent_id.as_deref(), Some("bruno"));
    }

    #[test]
    fn parse_multi_bot_only_no_legacy() {
        // Configs that use ONLY the new bots map (no top-level bot_token)
        // must parse cleanly and produce no "default" entry.
        let toml_str = r#"
[telegram.bots.nora]
bot_token = "222:BBB"
chat_id = "222"
agent_id = "nora"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let tg = TelegramConfig::from_notify_config(&config).unwrap();
        assert!(tg.bot_token.is_empty(), "no legacy token");
        assert!(tg.chat_id.is_empty(), "no legacy chat_id");
        assert_eq!(tg.bots.len(), 1);

        let bots = tg.all_bots();
        assert_eq!(bots.len(), 1, "no legacy entry should appear");
        assert_eq!(bots[0].0, "nora");
    }

    #[test]
    fn parse_legacy_only_still_works() {
        // The exact legacy form must keep round-tripping without any new keys.
        let toml_str = r#"
[telegram]
bot_token = "123:ABC"
chat_id = "456"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let tg = TelegramConfig::from_notify_config(&config).unwrap();
        assert!(tg.bots.is_empty());

        let bots = tg.all_bots();
        assert_eq!(bots.len(), 1);
        assert_eq!(bots[0].0, "default");
        assert_eq!(bots[0].1.agent_id, None);
    }

    #[test]
    fn named_bot_channel_type_is_qualified() {
        // Named bots return "telegram:<bot_id>" so the router can address
        // them distinctly. This is what makes per-agent routing possible.
        let ch = TelegramChannel::from_bot(
            "nora".to_string(),
            TelegramBotConfig {
                bot_token: "222:BBB".into(),
                chat_id: "222".into(),
                agent_id: Some("nora".into()),
                username: None,
            },
        );
        assert_eq!(ch.channel_type(), "telegram:nora");
        assert_eq!(ch.bot_id(), "nora");
        assert_eq!(ch.agent_id(), Some("nora"));
        assert_eq!(ch.chat_id(), "222");
    }

    #[test]
    fn default_bot_channel_type_is_unqualified() {
        // The "default" bot keeps the bare "telegram" channel_type so existing
        // routing rules and tests don't break — this is the load-bearing
        // backwards-compat invariant.
        let ch = TelegramChannel::from_bot(
            "default".to_string(),
            TelegramBotConfig {
                bot_token: "111:AAA".into(),
                chat_id: "111".into(),
                agent_id: None,
                username: None,
            },
        );
        assert_eq!(ch.channel_type(), "telegram");
    }

    #[test]
    fn all_from_notify_config_builds_one_channel_per_bot() {
        let toml_str = r#"
[telegram]
bot_token = "111:AAA"
chat_id = "111"

[telegram.bots.nora]
bot_token = "222:BBB"
chat_id = "222"
agent_id = "nora"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let channels = TelegramChannel::all_from_notify_config(&config).unwrap();
        assert_eq!(channels.len(), 2);

        let types: Vec<&str> = channels.iter().map(|c| c.channel_type()).collect();
        assert!(types.contains(&"telegram"), "default bot present");
        assert!(types.contains(&"telegram:nora"), "named bot present");
    }

    #[test]
    fn group_chat_id_warning_flags_positive_dm_ids_only() {
        // Negative supergroup id — the correct family-group target. No warning.
        assert!(group_chat_id_warning("nora", "-1001234567890").is_none());
        // Plain negative group id — also a group. No warning.
        assert!(group_chat_id_warning("nora", "-99").is_none());
        // Empty / unset — a separate "not configured" concern, not this footgun.
        assert!(group_chat_id_warning("nora", "").is_none());
        assert!(group_chat_id_warning("nora", "   ").is_none());
        // @channelusername — non-numeric, not the positive-DM footgun.
        assert!(group_chat_id_warning("nora", "@casa_family").is_none());

        // POSITIVE id — a 1:1 DM where the group is expected. LOUD warning that
        // names the bot and points at the fix, so a silent misroute is caught.
        let w = group_chat_id_warning("nora", "78901234").expect("positive id must warn");
        assert!(w.contains("nora"), "warning names the bot: {w}");
        assert!(w.contains("78901234"), "warning shows the id: {w}");
        assert!(w.contains("DM"), "warning explains the DM misroute: {w}");
        // Whitespace-padded positive id still warns (trimmed).
        assert!(group_chat_id_warning("bruno", "  555  ").is_some());
    }

    #[test]
    fn warn_on_dm_chat_ids_counts_only_the_misrouting_bots() {
        let bots = vec![
            (
                "nora".to_string(),
                TelegramBotConfig {
                    bot_token: "t".into(),
                    chat_id: "-1001".into(), // group — fine
                    agent_id: Some("nora".into()),
                    username: None,
                },
            ),
            (
                "bruno".to_string(),
                TelegramBotConfig {
                    bot_token: "t".into(),
                    chat_id: "78901234".into(), // DM — misroute
                    agent_id: Some("bruno".into()),
                    username: None,
                },
            ),
        ];
        assert_eq!(warn_on_dm_chat_ids(&bots), 1);
    }

    #[test]
    fn all_from_notify_config_empty_when_no_telegram_section() {
        // A NotifyConfig without `[telegram]` must produce an empty vec, not
        // an error — this lets callers treat "telegram not configured" the
        // same way the legacy code does (skip silently).
        let config = super::super::config::NotifyConfig::default();
        let channels = TelegramChannel::all_from_notify_config(&config).unwrap();
        assert!(channels.is_empty());
    }

    #[test]
    fn extract_message_id_from_response() {
        let json = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42,
                "chat": { "id": 123 },
                "text": "hello"
            }
        });
        let mid = TelegramChannel::extract_message_id(&json);
        assert_eq!(mid.0, "42");
    }

    #[test]
    fn extract_message_id_missing_returns_zero() {
        let json = serde_json::json!({"ok": true});
        let mid = TelegramChannel::extract_message_id(&json);
        assert_eq!(mid.0, "0");
    }

    // ---- multi-poll dispatch ----------------------------------------------
    //
    // The poll fan-out (one `spawn_poll` task per bot, all feeding one shared
    // receiver) rests on three units below: (1) `decode_update` tags each
    // message with the *receiving* bot's channel type, so a merged stream
    // still tells the router which bot got the update; (2) offset persistence
    // survives a restart; (3) the shared-tx merge delivers every bot's
    // messages onto one receiver.

    #[test]
    fn decode_update_tags_message_with_receiving_bot() {
        // Same raw update, decoded under two different bot tags, must come out
        // tagged with whichever bot polled it — this is the identity plumbing
        // that lets one pipeline serve every bot's queue.
        let update = serde_json::json!({
            "update_id": 100,
            "message": {
                "message_id": 7,
                "from": { "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "text": "hey @bruno_chef_bot what's for dinner",
                "entities": [
                    { "type": "mention", "offset": 4, "length": 15 }
                ]
            }
        });

        let as_bruno = decode_update(&update, "telegram:bruno").unwrap();
        assert_eq!(as_bruno.channel, "telegram:bruno");
        assert_eq!(as_bruno.sender, "luca");
        assert_eq!(as_bruno.chat_id.as_deref(), Some("-1001"));
        assert_eq!(as_bruno.chat_type.as_deref(), Some("supergroup"));
        assert_eq!(
            as_bruno.mention_usernames,
            vec!["bruno_chef_bot".to_string()],
            "group @mention parsed for routing"
        );
        assert!(as_bruno.action_id.is_none());

        let as_mira = decode_update(&update, "telegram:mira").unwrap();
        assert_eq!(as_mira.channel, "telegram:mira");
        // A plain @mention is NOT a slash command — `fix-command-leaks`.
        assert!(!as_bruno.has_bot_command);
    }

    #[test]
    fn decode_update_flags_leading_slash_command() {
        // A genuine `/help` carries a bot_command entity at offset 0 → the ONLY
        // signal the listener treats as a command (fix-command-leaks).
        let update = serde_json::json!({
            "update_id": 101,
            "message": {
                "message_id": 8,
                "from": { "id": 8905220378_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "text": "/help",
                "entities": [ { "type": "bot_command", "offset": 0, "length": 5 } ]
            }
        });
        let msg = decode_update(&update, "telegram:otto").unwrap();
        assert!(msg.has_bot_command, "/help is a leading slash command");
    }

    #[test]
    fn decode_update_bare_question_mark_is_not_a_command() {
        // The exact reported leak: `@nora_casapinello_bot ?` — a mention entity,
        // NO bot_command — must NOT be flagged as a command, so it can never fire
        // the operator HELP path and leak the WG claim/done reference.
        let update = serde_json::json!({
            "update_id": 102,
            "message": {
                "message_id": 9,
                "from": { "id": 8905220378_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "text": "@nora_casapinello_bot ?",
                "entities": [ { "type": "mention", "offset": 0, "length": 21 } ]
            }
        });
        let msg = decode_update(&update, "telegram:nora").unwrap();
        assert!(!msg.has_bot_command, "a bare `?` after a mention is conversation");
        assert_eq!(msg.mention_usernames, vec!["nora_casapinello_bot".to_string()]);
    }

    #[test]
    fn decode_update_handles_callback_query() {
        let update = serde_json::json!({
            "update_id": 200,
            "callback_query": {
                "from": { "username": "luca" },
                "data": "approve:my-task",
                "message": {
                    "message_id": 9,
                    "chat": { "id": 555, "type": "private" }
                }
            }
        });
        let msg = decode_update(&update, "telegram:otto").unwrap();
        assert_eq!(msg.channel, "telegram:otto");
        assert_eq!(msg.action_id.as_deref(), Some("approve:my-task"));
        assert_eq!(msg.body, "approve:my-task");
        assert_eq!(msg.chat_id.as_deref(), Some("555"));
    }

    #[test]
    fn decode_update_photo_populates_file_id_caption_and_group() {
        // A captioned photo (task photo-to-shopping): the caption becomes the
        // body so it routes like text, the largest size's file_id is captured,
        // and the media_group_id + caption @mention/command parse from the
        // caption fields (not the text/entities fields, which a photo lacks).
        let update = serde_json::json!({
            "update_id": 400,
            "message": {
                "message_id": 58,
                "from": { "id": 8905220378_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "media_group_id": "AG9",
                "caption": "@bruno_casapinello_bot what do we still need?",
                "caption_entities": [ { "type": "mention", "offset": 0, "length": 22 } ],
                "photo": [
                    { "file_id": "thumb", "width": 90, "height": 60, "file_size": 900 },
                    { "file_id": "biggest", "width": 1280, "height": 720, "file_size": 90000 }
                ]
            }
        });
        let msg = decode_update(&update, "telegram:bruno").unwrap();
        assert_eq!(msg.photo_file_id.as_deref(), Some("biggest"), "largest size chosen");
        assert_eq!(msg.media_group_id.as_deref(), Some("AG9"));
        assert_eq!(msg.body, "@bruno_casapinello_bot what do we still need?", "caption is the body");
        assert_eq!(
            msg.mention_usernames,
            vec!["bruno_casapinello_bot".to_string()],
            "caption @mention parsed from caption_entities"
        );
        assert!(!msg.has_bot_command, "a captioned photo is conversation, not a command");
    }

    #[test]
    fn decode_update_text_message_has_no_photo() {
        // A plain text message carries no photo fields — the photo branch must
        // never fabricate one.
        let update = serde_json::json!({
            "update_id": 401,
            "message": {
                "message_id": 7,
                "from": { "id": 1_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "text": "hey bruno"
            }
        });
        let msg = decode_update(&update, "telegram:bruno").unwrap();
        assert!(msg.photo_file_id.is_none());
        assert!(msg.media_group_id.is_none());
        assert_eq!(msg.body, "hey bruno");
    }

    #[test]
    fn decode_update_voice_note_populates_file_id_and_mime() {
        // A real Bot API `voice` note (task telegram-voice-notes): OGG/Opus,
        // carries NO text — its words are spoken. decode_update must surface the
        // download handle + declared mime, leave the body empty (the listener
        // fills it from the transcript), and NOT be a command.
        let update = serde_json::json!({
            "update_id": 500,
            "message": {
                "message_id": 91,
                "from": { "id": 8905220378_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "date": 1_700_000_100_i64,
                "voice": {
                    "duration": 4,
                    "mime_type": "audio/ogg",
                    "file_id": "AwACAgQAAxVOICE",
                    "file_unique_id": "uniq91",
                    "file_size": 12345
                }
            }
        });
        let msg = decode_update(&update, "telegram:otto").unwrap();
        assert_eq!(msg.voice_file_id.as_deref(), Some("AwACAgQAAxVOICE"));
        assert_eq!(msg.voice_mime.as_deref(), Some("audio/ogg"));
        assert_eq!(msg.body, "", "a recording carries no text body yet");
        assert!(msg.photo_file_id.is_none(), "a voice note is not a photo");
        assert!(!msg.has_bot_command, "a voice note is conversation, not a command");
    }

    #[test]
    fn decode_update_text_message_has_no_voice() {
        // A plain text message must never fabricate a voice handle.
        let update = serde_json::json!({
            "update_id": 501,
            "message": {
                "message_id": 8,
                "from": { "id": 1_i64, "username": "luca" },
                "chat": { "id": -1001, "type": "supergroup" },
                "text": "what's for dinner"
            }
        });
        let msg = decode_update(&update, "telegram:otto").unwrap();
        assert!(msg.voice_file_id.is_none());
        assert!(msg.voice_mime.is_none());
    }

    #[test]
    fn decode_update_ignores_non_message_updates() {
        // e.g. an edited_message / poll update we didn't ask for — no panic,
        // no phantom IncomingMessage.
        let update = serde_json::json!({ "update_id": 300, "edited_message": {} });
        assert!(decode_update(&update, "telegram:otto").is_none());
    }

    #[test]
    fn offset_persist_roundtrip_and_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        // Nested path exercises the create_dir_all in save_offset.
        let path = dir.path().join("nested").join("telegram_update_id_bruno");

        // Cold start: missing file → 0 (front of the queue).
        assert_eq!(load_offset(&path), 0);

        save_offset(&path, 4242);
        assert_eq!(load_offset(&path), 4242, "offset survives a restart");

        // A garbage file also defaults to 0 rather than crashing the poller.
        std::fs::write(&path, "not-a-number").unwrap();
        assert_eq!(load_offset(&path), 0);
    }

    #[tokio::test]
    async fn shared_tx_merges_every_bots_messages() {
        // Mirrors run_listen's fan-in: N producers (one per bot) each send
        // their decoded messages into ONE shared receiver. The single pipeline
        // must see all of them, tagged per bot.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(64);
        let bots = ["telegram:nora", "telegram:bruno", "telegram:mira", "telegram:otto"];
        for tag in bots {
            let tx = tx.clone();
            let tag = tag.to_string();
            tokio::spawn(async move {
                let update = serde_json::json!({
                    "update_id": 1,
                    "message": {
                        "message_id": 1,
                        "from": { "username": "luca" },
                        "chat": { "id": -1, "type": "supergroup" },
                        "text": "ping"
                    }
                });
                let msg = decode_update(&update, &tag).unwrap();
                tx.send(msg).await.unwrap();
            });
        }
        drop(tx); // so the loop terminates once all producers finish

        let mut seen: Vec<String> = Vec::new();
        while let Some(msg) = rx.recv().await {
            seen.push(msg.channel);
        }
        seen.sort();
        let mut expected: Vec<String> = bots.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(seen, expected, "every bot's message reached the one receiver");
    }

    // -----------------------------------------------------------------------
    // Listener resilience: backoff / rebuild / recovery + FD stability
    // (task `listener-reconnect`)
    // -----------------------------------------------------------------------

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    #[test]
    fn poll_backoff_grows_exponentially_and_caps_at_60s() {
        assert_eq!(poll_backoff(1), Duration::from_secs(1));
        assert_eq!(poll_backoff(2), Duration::from_secs(2));
        assert_eq!(poll_backoff(3), Duration::from_secs(4));
        assert_eq!(poll_backoff(4), Duration::from_secs(8));
        assert_eq!(poll_backoff(5), Duration::from_secs(16));
        assert_eq!(poll_backoff(6), Duration::from_secs(32));
        assert_eq!(poll_backoff(7), Duration::from_secs(60), "capped at 60s");
        assert_eq!(poll_backoff(100), Duration::from_secs(60), "huge streak stays capped");
        // A pathological streak must never panic on the shift overflow.
        assert_eq!(poll_backoff(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn poll_state_streak_then_rebuild_then_recovery() {
        let mut s = PollBackoffState::default();
        // Clean start: nothing to recover, no "resumed" line.
        assert_eq!(s.on_success(), None);

        // Failures 1–4: growing backoff, no rebuild yet.
        assert_eq!(
            s.on_failure(),
            FailureAction { backoff: Duration::from_secs(1), rebuild_client: false }
        );
        assert_eq!(
            s.on_failure(),
            FailureAction { backoff: Duration::from_secs(2), rebuild_client: false }
        );
        assert!(!s.on_failure().rebuild_client); // #3
        assert!(!s.on_failure().rebuild_client); // #4

        // Failure 5: rebuild fires (5 % POLL_REBUILD_AFTER == 0).
        let a5 = s.on_failure();
        assert_eq!(a5.backoff, Duration::from_secs(16));
        assert!(a5.rebuild_client, "client rebuilds after {POLL_REBUILD_AFTER} consecutive failures");
        assert_eq!(s.consecutive_failures, 5);

        // Recovery: the ended streak length is reported, then the state resets.
        assert_eq!(s.on_success(), Some(5));
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.on_success(), None, "already recovered — no duplicate breadcrumb");

        // A fresh failure restarts the streak from 1s with no immediate rebuild.
        let b1 = s.on_failure();
        assert_eq!(b1.backoff, Duration::from_secs(1));
        assert!(!b1.rebuild_client);
    }

    /// Count file descriptors open by THIS process. macOS exposes `/dev/fd`,
    /// Linux `/proc/self/fd`; either lists one entry per open fd.
    fn open_fd_count() -> usize {
        for p in ["/dev/fd", "/proc/self/fd"] {
            if let Ok(rd) = std::fs::read_dir(p) {
                return rd.count();
            }
        }
        0
    }

    /// Read one HTTP/1.1 request (headers + Content-Length body) off `stream`,
    /// so the keep-alive mock can serve the next request on the same socket.
    fn drain_one_http_request(stream: &mut TcpStream) -> std::io::Result<()> {
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if stream.read(&mut byte)? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "peer closed"));
            }
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&header);
        let content_length = text
            .lines()
            .find_map(|l| {
                let l = l.trim();
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            stream.read_exact(&mut body)?;
        }
        Ok(())
    }

    /// Spin an HTTP/1.1 **keep-alive** server that answers every request on a
    /// connection with `body`. Reusing one connection across many requests is
    /// exactly what a non-leaking pooled client should do — the FD test relies
    /// on this to prove the socket count stays flat.
    fn spawn_keepalive_json_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut stream = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // Serve requests until the client drops the connection.
                loop {
                    if drain_one_http_request(&mut stream).is_err() {
                        break;
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            }
        });
        base
    }

    /// Like [`spawn_keepalive_json_server`] but also counts how many distinct
    /// TCP connections it accepts, so a test can prove the poll loop REUSES one
    /// connection instead of opening (and leaking) a fresh one per poll.
    fn spawn_conn_counting_server(
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let conns = std::sync::Arc::new(AtomicUsize::new(0));
        let conns_srv = conns.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut stream = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                conns_srv.fetch_add(1, Ordering::SeqCst);
                loop {
                    if drain_one_http_request(&mut stream).is_err() {
                        break;
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            }
        });
        (base, conns)
    }

    /// Soak the poll round-trip for thousands of iterations against a local
    /// mock, asserting (a) the process's open-FD count stays flat and (b) the
    /// client opens only a HANDFUL of TCP connections total — i.e. it REUSES
    /// one pooled connection instead of a fresh one per poll (the root cause of
    /// the FD-exhaustion wedge). Ignored by default (thousands of round-trips);
    /// run with `cargo test -- --ignored poll_loop_fd_and_connection_soak`.
    #[tokio::test]
    #[ignore = "soak: thousands of round-trips; run explicitly with --ignored"]
    async fn poll_loop_fd_and_connection_soak() {
        use std::sync::atomic::Ordering;
        let (base, conns) = spawn_conn_counting_server(r#"{"ok":true,"result":[]}"#);
        let client = build_poll_client();

        // Warm up so pool + runtime FDs are established before the baseline.
        for _ in 0..5 {
            get_updates_once(&client, &base, "SOAKTOKEN", 0, 0).await.unwrap();
        }
        let baseline_fds = open_fd_count();
        eprintln!("soak baseline: open_fds={baseline_fds}");

        const ITERS: u32 = 5000;
        let mut max_fds = baseline_fds;
        for i in 1..=ITERS {
            get_updates_once(&client, &base, "SOAKTOKEN", 0, 0).await.unwrap();
            if i % 1000 == 0 {
                let fds = open_fd_count();
                max_fds = max_fds.max(fds);
                eprintln!(
                    "soak iter {i}: open_fds={fds} conns_accepted={}",
                    conns.load(Ordering::SeqCst)
                );
            }
        }

        let total_conns = conns.load(Ordering::SeqCst);
        eprintln!("soak done: baseline_fds={baseline_fds} max_fds={max_fds} total_conns={total_conns}");
        assert!(
            max_fds <= baseline_fds + 5,
            "poll loop leaked FDs over {ITERS} iters: baseline={baseline_fds} max={max_fds}"
        );
        assert!(
            total_conns <= 5,
            "poll loop opened {total_conns} connections over {ITERS} polls — a reused pooled \
             connection should open only a handful (the per-poll-connection bug is back)"
        );
    }

    #[tokio::test]
    async fn get_updates_once_decodes_result_array() {
        let base = spawn_keepalive_json_server(
            r#"{"ok":true,"result":[{"update_id":42,"message":{"message_id":1}}]}"#,
        );
        let client = build_poll_client();
        let updates = get_updates_once(&client, &base, "TESTTOKEN", 0, 0).await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["update_id"].as_i64(), Some(42));
    }

    #[tokio::test]
    async fn get_updates_once_errors_on_non_json_body() {
        let base = spawn_keepalive_json_server("<html>502 bad gateway</html>");
        let client = build_poll_client();
        let err = get_updates_once(&client, &base, "TESTTOKEN", 0, 0).await;
        assert!(err.is_err(), "a non-JSON body must surface as an error the loop can back off on");
    }

    /// The core FD-leak regression guard: many poll iterations against a
    /// keep-alive server must NOT grow the process's open-socket count. A
    /// per-poll client (the old bug) opened a fresh connection every iteration
    /// and leaked FDs until the process limit was hit; a reused pooled client
    /// keeps the count flat.
    #[tokio::test]
    async fn poll_loop_does_not_leak_file_descriptors() {
        let base = spawn_keepalive_json_server(r#"{"ok":true,"result":[]}"#);
        let client = build_poll_client();

        // Warm up so the pooled connection + runtime FDs are established before
        // we take the baseline.
        for _ in 0..5 {
            let updates = get_updates_once(&client, &base, "TESTTOKEN", 0, 0).await.unwrap();
            assert!(updates.is_empty());
        }

        let before = open_fd_count();
        for _ in 0..50 {
            let updates = get_updates_once(&client, &base, "TESTTOKEN", 0, 0).await.unwrap();
            assert!(updates.is_empty());
        }

        // `open_fd_count()` is process-GLOBAL, and this binary runs its tests
        // concurrently — sibling tests opening sockets/temp files transiently
        // inflate the count and used to flake this assertion. A genuine per-poll
        // leak is different in kind: it adds ~1 fd/iteration (~50 here) and is
        // PERSISTENT, whereas concurrent noise dissipates in milliseconds. So on a
        // spike we let the noise settle and re-measure before failing — a real
        // leak stays above the bound, transient noise falls back under it.
        // Settle for up to ~2s: the suite's file-heavy conversational tests
        // (collective single-owner routing, parity) churn fds concurrently for
        // several hundred ms, so a short window could sample before the noise
        // clears. A genuine per-poll leak is PERSISTENT and stays elevated no
        // matter how long we wait, so a longer settle only removes false spikes.
        let tolerance = 8;
        let mut after = open_fd_count();
        if after > before + tolerance {
            for _ in 0..80 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                after = open_fd_count();
                if after <= before + tolerance {
                    break;
                }
            }
        }

        assert!(
            after <= before + tolerance,
            "poll loop leaked file descriptors: before={before} after={after} \
             (a reused pooled connection should keep this flat; a per-poll client \
             leaks ~1 fd/iteration and stays elevated after settling)"
        );
    }
}
