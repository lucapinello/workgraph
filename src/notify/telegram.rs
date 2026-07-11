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
            client: reqwest::Client::new(),
        }
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
            "https://api.telegram.org/bot{}/{}",
            self.bot.bot_token, method
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

    /// Extract the message_id from a sendMessage response.
    fn extract_message_id(json: &serde_json::Value) -> MessageId {
        let mid = json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64())
            .unwrap_or(0);
        MessageId(mid.to_string())
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
        let client = self.client.clone();
        // Pre-compute the channel-type tag so each IncomingMessage carries
        // the bot identity ("telegram" for the legacy bot, "telegram:<id>"
        // for named ones). The awaiting-human router
        // (`commands::service::human_dispatch::route_inbound_reply`) uses this
        // to decide which open `awaiting-human` task should receive the reply.
        let channel_tag = self.channel_type.clone();

        tokio::spawn(async move {
            let mut offset: i64 = offset_path.as_deref().map(load_offset).unwrap_or(0);
            loop {
                let url = format!("https://api.telegram.org/bot{}/getUpdates", bot.bot_token);
                let body = serde_json::json!({
                    "offset": offset,
                    "timeout": 30,
                    "allowed_updates": ["message", "callback_query"],
                });

                let resp = match client.post(&url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Telegram poll error: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let json: serde_json::Value = match resp.json().await {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("Telegram parse error: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let updates = match json.get("result").and_then(|r| r.as_array()) {
                    Some(arr) => arr.clone(),
                    None => continue,
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
                        if tx.send(msg).await.is_err() {
                            return; // receiver dropped — stop polling this bot
                        }
                    }
                }
            }
        })
    }
}

/// Decode a single Telegram `getUpdates` element into an [`IncomingMessage`],
/// tagging it with `channel_tag` (the receiving bot's channel type). Returns
/// `None` for updates that are neither a callback query nor a text message.
///
/// Extracted from the poll loop so the single-bot and multi-bot pollers share
/// exactly one decoder — the two must never diverge in how they parse a
/// button press vs. a group @mention.
fn decode_update(update: &serde_json::Value, channel_tag: &str) -> Option<IncomingMessage> {
    // Handle callback queries (button presses)
    if let Some(cb) = update.get("callback_query") {
        let sender = cb
            .get("from")
            .and_then(|f| f.get("username"))
            .and_then(|u| u.as_str())
            .or_else(|| {
                cb.get("from")
                    .and_then(|f| f.get("id"))
                    .and_then(|i| i.as_i64())
                    .map(|_| "unknown")
            })
            .unwrap_or("unknown");

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
            sender: sender.to_string(),
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
        });
    }

    // Handle regular messages
    if let Some(message) = update.get("message") {
        let sender = message
            .get("from")
            .and_then(|f| f.get("username"))
            .and_then(|u| u.as_str())
            .unwrap_or("unknown");

        let body = message
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

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
        let mention_usernames = super::telegram_group::parse_mention_usernames(
            message.get("text").and_then(|t| t.as_str()).unwrap_or(""),
            message.get("entities").unwrap_or(&serde_json::Value::Null),
        );
        // Reply-chain: if this replies to a bot's own message,
        // name that bot so the reply routes to its agent.
        let reply_to_bot = super::telegram_group::reply_to_bot_username(message);

        return Some(IncomingMessage {
            channel: channel_tag.to_string(),
            sender: sender.to_string(),
            body,
            action_id: None,
            reply_to,
            message_id,
            chat_id,
            chat_type,
            mention_usernames,
            reply_to_bot,
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
}
