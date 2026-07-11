//! Group @mention routing for Telegram (R17).
//!
//! In a private (1:1) chat every inbound message is for the bot, so the
//! listener handles all of them. In a **group** or **supergroup** the bot only
//! wants the messages actually addressed to it — Telegram's privacy mode
//! delivers exactly the @mentions, replies-to-the-bot, and `/commands`. This
//! module reproduces that filter for the long-poll listener and resolves a
//! group @mention (`@bruno_chef_bot`) back to the workgraph agent the mentioned
//! bot fronts, so the message can be routed "like a 1:1" to that agent while
//! the reply is sent back to the *group* chat.
//!
//! Everything here is pure (no network, no filesystem): it takes the parsed
//! Telegram update JSON plus the local [`TelegramConfig`] and returns a
//! routing decision. Bot tokens are read from the config only to identify the
//! matching bot's channel — they are NEVER placed in the returned value, logs,
//! or the graph.

use super::telegram::TelegramConfig;

/// Extract the bot @usernames mentioned in a Telegram message via its native
/// `mention` entities.
///
/// Telegram tags `@username` spans with a message entity of `type == "mention"`
/// whose `offset`/`length` are measured in **UTF-16 code units** (per the Bot
/// API), so we index the text as UTF-16 to slice the exact span rather than
/// guessing from byte offsets (which breaks on any non-BMP or multi-byte
/// character earlier in the text). Only `mention` entities are considered —
/// `text_mention` (which targets a user with no public @username) and every
/// other entity type are ignored. Returned usernames are lower-cased and
/// stripped of the leading `@`, in order of appearance, de-duplicated.
///
/// `entities` is the raw JSON array from the update's `message.entities`
/// (or `null`/absent, in which case the result is empty).
pub fn parse_mention_usernames(text: &str, entities: &serde_json::Value) -> Vec<String> {
    let arr = match entities.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    // Telegram entity offsets/lengths are UTF-16 code units.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let mut out: Vec<String> = Vec::new();

    for ent in arr {
        if ent.get("type").and_then(|t| t.as_str()) != Some("mention") {
            continue;
        }
        let offset = match ent.get("offset").and_then(|o| o.as_u64()) {
            Some(o) => o as usize,
            None => continue,
        };
        let length = match ent.get("length").and_then(|l| l.as_u64()) {
            Some(l) => l as usize,
            None => continue,
        };
        let end = match offset.checked_add(length) {
            Some(e) if e <= utf16.len() => e,
            _ => continue, // out-of-range span — skip rather than panic
        };
        let span = match String::from_utf16(&utf16[offset..end]) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let username = span.trim().trim_start_matches('@').to_ascii_lowercase();
        if username.is_empty() {
            continue;
        }
        if !out.contains(&username) {
            out.push(username);
        }
    }

    out
}

/// Extract the `@username` of the bot whose message an inbound Telegram
/// `message` is a reply to, for reply-chain routing.
///
/// Returns `Some(username)` (lower-cased, no leading `@`) only when
/// `message.reply_to_message.from.is_bot == true` and that bot has a public
/// `username`. Returns `None` when the message is not a reply, replies to a
/// human, or the replied-to bot has no username. Under Telegram privacy mode a
/// bot only ever receives replies to its *own* messages, so this reliably names
/// the bot a threaded reply is aimed at. `message` is the raw update
/// `message` object.
pub fn reply_to_bot_username(message: &serde_json::Value) -> Option<String> {
    let from = message.get("reply_to_message")?.get("from")?;
    if from.get("is_bot").and_then(|b| b.as_bool()) != Some(true) {
        return None;
    }
    let username = from.get("username").and_then(|u| u.as_str())?;
    let cleaned = username.trim().trim_start_matches('@').to_ascii_lowercase();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// A bot resolved from a group @mention: the identity the downstream 1:1
/// routing path needs, with the token deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBot {
    /// The `[telegram.bots.<id>]` key (or `"default"` for the legacy bot).
    pub bot_id: String,
    /// The channel-type discriminator the rest of the system routes on —
    /// `"telegram"` for the default bot, `"telegram:<bot_id>"` otherwise. This
    /// is exactly what `human_dispatch::route_inbound_reply` maps back to an
    /// agent, so a group message can be routed identically to a 1:1.
    pub channel_type: String,
    /// The workgraph agent id this bot fronts, if it declares one.
    pub agent_id: Option<String>,
}

/// Map a mentioned @username to the configured bot that carries it.
///
/// A bot matches when its declared `username` equals `username`
/// (case-insensitively); as a fallback the `[telegram.bots.<id>]` key itself is
/// matched, so a config that omits `username` still routes if the operator
/// named the bot after its handle. Returns `None` when no bot claims the
/// handle. Tokens are never read into the result.
pub fn resolve_mentioned_bot(username: &str, config: &TelegramConfig) -> Option<ResolvedBot> {
    let want = username.trim_start_matches('@').to_ascii_lowercase();
    if want.is_empty() {
        return None;
    }

    for (bot_id, bot) in config.all_bots() {
        let by_username = bot
            .username
            .as_deref()
            .map(|u| u.trim_start_matches('@').eq_ignore_ascii_case(&want))
            .unwrap_or(false);
        let by_bot_id = bot_id.eq_ignore_ascii_case(&want);
        if by_username || by_bot_id {
            let channel_type = if bot_id == "default" {
                "telegram".to_string()
            } else {
                format!("telegram:{}", bot_id)
            };
            return Some(ResolvedBot {
                bot_id,
                channel_type,
                agent_id: bot.agent_id.clone(),
            });
        }
    }
    None
}

/// The routing decision for one inbound Telegram message, once its chat type is
/// known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRoute {
    /// A private (1:1) chat — not a group. The caller should fall through to
    /// its existing 1:1 handling unchanged.
    Private,
    /// A group/supergroup message that privacy mode drops: no @mention of a
    /// bot, not a reply to the bot, and not a `/command`. Ignored silently.
    IgnoredByPrivacy,
    /// A group message addressed to a known bot (via @mention). Route the
    /// `body` to `bot` exactly as a 1:1 would be, but send any reply to
    /// `reply_chat` — the *group's* chat id, never the bot's default chat.
    RouteToBot {
        bot: ResolvedBot,
        reply_chat: String,
        body: String,
    },
    /// A group message that passed privacy (a `/command`, a reply to the bot,
    /// or an @mention) but did NOT resolve to any configured bot. The caller
    /// may still handle a bare command here; replies go to `reply_chat` (the
    /// group).
    Unaddressed { reply_chat: String },
}

/// Decide how to route an inbound message given its chat context.
///
/// * `chat_type` — `message.chat.type` (`"private"`, `"group"`, `"supergroup"`,
///   `"channel"`). Anything other than `group`/`supergroup` yields
///   [`GroupRoute::Private`] (the 1:1 path).
/// * `chat_id` — `message.chat.id`; becomes the reply target for group routes.
/// * `text` — the message text.
/// * `mention_usernames` — bot @usernames already extracted by
///   [`parse_mention_usernames`].
/// * `is_reply` — whether this message is a reply to an earlier message. Under
///   Telegram bot privacy mode (default ON) the only replies the listener ever
///   receives in a group are replies to the bot's own messages, so a received
///   reply is treated as addressed to the bot.
/// * `config` — the local Telegram config used to resolve the mention.
///
/// The first mention that resolves to a configured bot wins; the addressed
/// `@bot` handle is stripped from the routed `body` so the agent sees the bare
/// instruction, matching how a 1:1 message would arrive.
pub fn route_group_message(
    chat_type: Option<&str>,
    chat_id: Option<&str>,
    text: &str,
    mention_usernames: &[String],
    is_reply: bool,
    config: &TelegramConfig,
) -> GroupRoute {
    let is_group = matches!(chat_type, Some("group") | Some("supergroup"));
    if !is_group {
        return GroupRoute::Private;
    }

    // Reply target is always the originating group chat. If the update somehow
    // lacked a chat id there is nothing to reply to, so drop it.
    let reply_chat = match chat_id {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return GroupRoute::IgnoredByPrivacy,
    };

    let is_command = is_bot_command(text);
    let has_mention = !mention_usernames.is_empty();

    // Privacy mode: in a group we only act on @mentions, replies to the bot,
    // and commands. Everything else is ordinary group chatter we ignore.
    if !has_mention && !is_command && !is_reply {
        return GroupRoute::IgnoredByPrivacy;
    }

    // Resolve the first mention that maps to a configured bot.
    for username in mention_usernames {
        if let Some(bot) = resolve_mentioned_bot(username, config) {
            let body = strip_mention(text, username);
            return GroupRoute::RouteToBot {
                bot,
                reply_chat,
                body,
            };
        }
    }

    GroupRoute::Unaddressed { reply_chat }
}

/// The persona the group's concierge fallback routes to when a message names
/// no one. Otto is the Family Assistant — the "keeps the trains running" voice
/// (docs/01 §2.4) — so unaddressed group chatter lands with him.
pub const CONCIERGE_BOT: &str = "otto";

/// How a natural-routed group message picked its target agent. Carried on
/// [`NaturalRoute::ToBot`] purely for logging and tests — it never changes what
/// the downstream 1:1 router does with the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressedBy {
    /// An explicit `@bot` mention (the R17 path).
    Mention,
    /// The first family-agent name appearing in the text
    /// (`"nora, what's for dinner?"`).
    Name,
    /// A reply to a message that bot itself posted in the group.
    ReplyChain,
    /// Nobody was named — routed to the concierge ([`CONCIERGE_BOT`]).
    Concierge,
}

impl std::fmt::Display for AddressedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AddressedBy::Mention => "@mention",
            AddressedBy::Name => "name",
            AddressedBy::ReplyChain => "reply-chain",
            AddressedBy::Concierge => "concierge",
        };
        f.write_str(s)
    }
}

/// The natural-routing decision for one inbound group message.
///
/// This is the "make the group feel natural" layer on top of R17's
/// privacy-aware [`route_group_message`]. Where R17 answers *"is this message
/// even for a bot?"*, [`route_natural`] answers *"which of our family voices
/// should this land on?"* — resolving, in order: an explicit `@mention`, the
/// first family name in the text, the bot a reply is threaded onto, and finally
/// the concierge ([`CONCIERGE_BOT`]) when no one is named. It assumes the
/// receiving bot is the concierge running with Telegram privacy mode **off**
/// (so plain chatter actually reaches the listener); see docs/09 §natural-group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NaturalRoute {
    /// A private (1:1) chat — not a group. Fall through to the existing 1:1
    /// handling unchanged.
    Private,
    /// A group message with no usable chat id: there is nothing to reply to,
    /// so it is dropped. (Also returned if no target could be resolved because
    /// the concierge bot is not configured.)
    Drop,
    /// Route `body` to `bot`'s agent exactly as a 1:1 would be routed, sending
    /// any reply to `reply_chat` — the *group's* chat id, never the bot's
    /// default DM. `addressed_by` records why this bot was chosen.
    ToBot {
        bot: ResolvedBot,
        reply_chat: String,
        body: String,
        addressed_by: AddressedBy,
    },
}

/// Find the first family-agent **name** appearing as a word in `text` and
/// resolve it to a configured bot.
///
/// The text is split on word boundaries (runs of non-alphanumeric, keeping
/// `_` so bare usernames still match) and each token is offered to
/// [`resolve_mentioned_bot`], which matches a bot's `username` **or** its
/// `[telegram.bots.<id>]` key case-insensitively. So `"tell bruno the curry was
/// great"` resolves the token `bruno` to the `bruno` bot. The first token that
/// resolves wins (left-to-right), matching how a person reads the sentence.
/// Returns `None` when no token names a configured bot.
pub fn first_named_bot(text: &str, config: &TelegramConfig) -> Option<ResolvedBot> {
    for tok in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.is_empty() {
            continue;
        }
        if let Some(bot) = resolve_mentioned_bot(tok, config) {
            return Some(bot);
        }
    }
    None
}

/// Route an inbound group message to a family voice, the "natural" way.
///
/// Precedence (most explicit first):
/// 1. **`@mention`** of a configured bot — the R17 behaviour; the addressed
///    handle is stripped from the routed body.
/// 2. **Name in the text** — the first family name a reader would see
///    ([`first_named_bot`]); the body is passed through verbatim (the name is
///    part of natural speech, not noise to strip).
/// 3. **Reply-chain** — `reply_to_bot` is the `@username` of the bot whose own
///    message this is a reply to (Telegram delivers replies-to-a-bot even under
///    privacy mode), so a threaded "yes that works" lands on that bot.
/// 4. **Concierge** — nobody was named, so it goes to [`CONCIERGE_BOT`] (otto),
///    who fronts the group. This only fires for messages the listener actually
///    received, i.e. the concierge bot's privacy mode is off.
///
/// Non-group chats yield [`NaturalRoute::Private`]. A group message with no chat
/// id — or one that names no one when the concierge bot is not configured —
/// yields [`NaturalRoute::Drop`].
pub fn route_natural(
    chat_type: Option<&str>,
    chat_id: Option<&str>,
    text: &str,
    mention_usernames: &[String],
    reply_to_bot: Option<&str>,
    config: &TelegramConfig,
) -> NaturalRoute {
    let is_group = matches!(chat_type, Some("group") | Some("supergroup"));
    if !is_group {
        return NaturalRoute::Private;
    }

    // Reply target is always the originating group chat. Without a chat id there
    // is nothing to reply to, so drop.
    let reply_chat = match chat_id {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return NaturalRoute::Drop,
    };

    // 1. Explicit @mention — first resolvable mention wins, handle stripped.
    for username in mention_usernames {
        if let Some(bot) = resolve_mentioned_bot(username, config) {
            return NaturalRoute::ToBot {
                bot,
                reply_chat,
                body: strip_mention(text, username),
                addressed_by: AddressedBy::Mention,
            };
        }
    }

    // 2. First family name in the text.
    if let Some(bot) = first_named_bot(text, config) {
        return NaturalRoute::ToBot {
            bot,
            reply_chat,
            body: text.to_string(),
            addressed_by: AddressedBy::Name,
        };
    }

    // 3. Reply threaded onto a bot's own message.
    if let Some(uname) = reply_to_bot {
        if let Some(bot) = resolve_mentioned_bot(uname, config) {
            return NaturalRoute::ToBot {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::ReplyChain,
            };
        }
    }

    // 4. Concierge fallback — otto.
    if let Some(bot) = resolve_mentioned_bot(CONCIERGE_BOT, config) {
        return NaturalRoute::ToBot {
            bot,
            reply_chat,
            body: text.to_string(),
            addressed_by: AddressedBy::Concierge,
        };
    }

    NaturalRoute::Drop
}

/// True if `text` begins with a Telegram bot command (`/word`). In groups these
/// are commonly suffixed with the target bot (`/status@bruno_chef_bot`).
fn is_bot_command(text: &str) -> bool {
    let t = text.trim_start();
    let mut chars = t.chars();
    match chars.next() {
        Some('/') => chars.next().map(|c| c.is_alphanumeric()).unwrap_or(false),
        _ => false,
    }
}

/// Remove the addressed `@username` token from the message body so the routed
/// text reads like a direct 1:1 instruction. Only the addressed handle is
/// stripped; other mentions are left intact. Collapses the surrounding
/// whitespace left behind.
fn strip_mention(text: &str, username: &str) -> String {
    let handle = format!("@{}", username);
    let stripped = text
        .split_whitespace()
        .filter(|tok| !tok.eq_ignore_ascii_case(&handle))
        .collect::<Vec<_>>()
        .join(" ");
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::telegram::TelegramBotConfig;
    use std::collections::HashMap;

    fn cfg_with_bots(bots: &[(&str, &str, Option<&str>, Option<&str>)]) -> TelegramConfig {
        // (bot_id, chat_id, agent_id, username)
        let mut map = HashMap::new();
        for (id, chat, agent, uname) in bots {
            map.insert(
                (*id).to_string(),
                TelegramBotConfig {
                    bot_token: "123:SECRET".to_string(),
                    chat_id: (*chat).to_string(),
                    agent_id: agent.map(|s| s.to_string()),
                    username: uname.map(|s| s.to_string()),
                },
            );
        }
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots: map,
        }
    }

    fn entities(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    // ---- Test 1: mention parse -------------------------------------------

    #[test]
    fn parse_mention_extracts_username_from_entity_span() {
        // "@bruno_chef_bot please plan dinner" — a single mention entity at
        // offset 0, length 15 (len of "@bruno_chef_bot").
        let text = "@bruno_chef_bot please plan dinner";
        let ents = entities(r#"[{"type":"mention","offset":0,"length":15}]"#);
        assert_eq!(parse_mention_usernames(text, &ents), vec!["bruno_chef_bot"]);
    }

    #[test]
    fn parse_mention_multiple_and_dedup_and_lowercase() {
        // Two mentions, second repeated; offsets are UTF-16 code units.
        // "hi @Nora_Bot and @bruno_chef_bot and @Nora_Bot"
        let text = "hi @Nora_Bot and @bruno_chef_bot and @Nora_Bot";
        let ents = entities(
            r#"[
                {"type":"mention","offset":3,"length":9},
                {"type":"mention","offset":17,"length":15},
                {"type":"mention","offset":37,"length":9}
            ]"#,
        );
        assert_eq!(
            parse_mention_usernames(text, &ents),
            vec!["nora_bot", "bruno_chef_bot"]
        );
    }

    #[test]
    fn parse_mention_honours_utf16_offsets_after_emoji() {
        // A non-BMP emoji (🍝, 2 UTF-16 units) precedes the mention. A
        // byte-offset parser would slice the wrong span; UTF-16 indexing gets
        // "@bruno_chef_bot" exactly.
        let text = "🍝 @bruno_chef_bot";
        // "🍝"=2 units, " "=1 => mention starts at UTF-16 offset 3, length 15.
        let ents = entities(r#"[{"type":"mention","offset":3,"length":15}]"#);
        assert_eq!(parse_mention_usernames(text, &ents), vec!["bruno_chef_bot"]);
    }

    #[test]
    fn parse_mention_ignores_non_mention_entities_and_missing() {
        let text = "/status@bruno_chef_bot now";
        // bot_command entity, not a mention → no usernames.
        let ents = entities(r#"[{"type":"bot_command","offset":0,"length":22}]"#);
        assert!(parse_mention_usernames(text, &ents).is_empty());
        // Absent entities → empty.
        assert!(parse_mention_usernames(text, &serde_json::Value::Null).is_empty());
    }

    #[test]
    fn parse_mention_out_of_range_span_is_skipped_not_panic() {
        let text = "@x";
        let ents = entities(r#"[{"type":"mention","offset":0,"length":999}]"#);
        assert!(parse_mention_usernames(text, &ents).is_empty());
    }

    // ---- Test 2: bot -> agent map ----------------------------------------

    #[test]
    fn resolve_mention_maps_username_to_agent_bot() {
        let config = cfg_with_bots(&[
            ("nora", "78901234", Some("nora"), Some("nora_planner_bot")),
            ("bruno", "78901234", Some("bruno"), Some("bruno_chef_bot")),
        ]);
        let resolved = resolve_mentioned_bot("bruno_chef_bot", &config).unwrap();
        assert_eq!(resolved.bot_id, "bruno");
        assert_eq!(resolved.channel_type, "telegram:bruno");
        assert_eq!(resolved.agent_id.as_deref(), Some("bruno"));
    }

    #[test]
    fn resolve_mention_is_case_insensitive_and_strips_at() {
        let config = cfg_with_bots(&[("bruno", "1", Some("bruno"), Some("Bruno_Chef_Bot"))]);
        assert_eq!(
            resolve_mentioned_bot("@BRUNO_CHEF_BOT", &config)
                .unwrap()
                .agent_id
                .as_deref(),
            Some("bruno")
        );
    }

    #[test]
    fn resolve_mention_falls_back_to_bot_id_when_no_username() {
        // No `username` declared — the bot_id itself is matched.
        let config = cfg_with_bots(&[("bruno_chef_bot", "1", Some("bruno"), None)]);
        let resolved = resolve_mentioned_bot("bruno_chef_bot", &config).unwrap();
        assert_eq!(resolved.channel_type, "telegram:bruno_chef_bot");
        assert_eq!(resolved.agent_id.as_deref(), Some("bruno"));
    }

    #[test]
    fn resolve_mention_unknown_username_is_none() {
        let config = cfg_with_bots(&[("bruno", "1", Some("bruno"), Some("bruno_chef_bot"))]);
        assert!(resolve_mentioned_bot("someone_else_bot", &config).is_none());
    }

    // ---- Test 3: group reply routing -------------------------------------

    #[test]
    fn route_group_reply_goes_to_group_chat_not_bot_default() {
        // The bot's own default chat is "111"; the group is "-100999". A group
        // @mention must route to the agent AND reply to the GROUP chat.
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let mentions = vec!["bruno_chef_bot".to_string()];
        let route = route_group_message(
            Some("supergroup"),
            Some("-100999"),
            "@bruno_chef_bot plan dinner for tonight",
            &mentions,
            false,
            &config,
        );
        match route {
            GroupRoute::RouteToBot {
                bot,
                reply_chat,
                body,
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some("bruno"));
                assert_eq!(bot.channel_type, "telegram:bruno");
                // The load-bearing assertion: reply target is the group chat,
                // NOT the bot's configured default chat ("111").
                assert_eq!(reply_chat, "-100999");
                assert_ne!(reply_chat, "111");
                // The addressed @handle is stripped from the routed body.
                assert_eq!(body, "plan dinner for tonight");
            }
            other => panic!("expected RouteToBot, got {other:?}"),
        }
    }

    #[test]
    fn route_private_chat_is_passthrough() {
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let route = route_group_message(
            Some("private"),
            Some("55501234"),
            "hi bruno",
            &[],
            false,
            &config,
        );
        assert_eq!(route, GroupRoute::Private);
    }

    #[test]
    fn route_group_plain_chatter_dropped_by_privacy() {
        // No mention, not a command, not a reply-to-bot → ignored.
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let route = route_group_message(
            Some("group"),
            Some("-100999"),
            "what's for dinner everyone?",
            &[],
            false,
            &config,
        );
        assert_eq!(route, GroupRoute::IgnoredByPrivacy);
    }

    #[test]
    fn route_group_reply_to_bot_without_mention_is_not_dropped() {
        // A reply to the bot (privacy mode delivers it) with no mention and no
        // command resolves to Unaddressed (still handled), replying to group.
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let route = route_group_message(
            Some("group"),
            Some("-100999"),
            "yes that works",
            &[],
            true,
            &config,
        );
        assert_eq!(
            route,
            GroupRoute::Unaddressed {
                reply_chat: "-100999".to_string()
            }
        );
    }

    #[test]
    fn route_group_command_without_mention_is_unaddressed_replying_to_group() {
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let route = route_group_message(
            Some("supergroup"),
            Some("-100999"),
            "/status",
            &[],
            false,
            &config,
        );
        assert_eq!(
            route,
            GroupRoute::Unaddressed {
                reply_chat: "-100999".to_string()
            }
        );
    }

    #[test]
    fn route_group_mention_of_unknown_bot_is_unaddressed() {
        let config = cfg_with_bots(&[("bruno", "111", Some("bruno"), Some("bruno_chef_bot"))]);
        let mentions = vec!["stranger_bot".to_string()];
        let route = route_group_message(
            Some("group"),
            Some("-100999"),
            "@stranger_bot hello",
            &mentions,
            false,
            &config,
        );
        assert_eq!(
            route,
            GroupRoute::Unaddressed {
                reply_chat: "-100999".to_string()
            }
        );
    }

    #[test]
    fn strip_mention_removes_only_addressed_handle() {
        assert_eq!(
            strip_mention("@bruno_chef_bot and @nora_bot help", "bruno_chef_bot"),
            "and @nora_bot help"
        );
    }

    // ---- Natural routing (name-addressed + reply-chain + concierge) -------

    /// The Casa Pinello group: four named voices, all sharing the group chat
    /// (`-100999`), each with its `@handle` username.
    fn casa_config() -> TelegramConfig {
        cfg_with_bots(&[
            (
                "nora",
                "-100999",
                Some("nora"),
                Some("nora_casapinello_bot"),
            ),
            (
                "bruno",
                "-100999",
                Some("bruno"),
                Some("bruno_casapinello_bot"),
            ),
            (
                "mira",
                "-100999",
                Some("mira"),
                Some("mira_casapinello_bot"),
            ),
            (
                "otto",
                "-100999",
                Some("otto"),
                Some("otto_casapinello_bot"),
            ),
        ])
    }

    fn route(text: &str, reply_to_bot: Option<&str>) -> NaturalRoute {
        route_natural(
            Some("supergroup"),
            Some("-100999"),
            text,
            &[],
            reply_to_bot,
            &casa_config(),
        )
    }

    fn assert_routed(route: &NaturalRoute, agent: &str, by: AddressedBy) {
        match route {
            NaturalRoute::ToBot {
                bot,
                reply_chat,
                addressed_by,
                ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some(agent), "agent");
                assert_eq!(*addressed_by, by, "addressed_by");
                // Every group route replies to the group, never a bot's DM.
                assert_eq!(reply_chat, "-100999", "reply target is the group");
            }
            other => panic!("expected ToBot({agent}), got {other:?}"),
        }
    }

    // Name-addressed routing — case 1/3.
    #[test]
    fn natural_name_routes_leading_name_with_comma() {
        assert_routed(
            &route("nora, what's for dinner?", None),
            "nora",
            AddressedBy::Name,
        );
    }

    // Name-addressed routing — case 2/3 (name mid-sentence, case-insensitive).
    #[test]
    fn natural_name_routes_midsentence_name() {
        assert_routed(
            &route("tell Bruno the curry was great", None),
            "bruno",
            AddressedBy::Name,
        );
    }

    // Name-addressed routing — case 3/3 (a different voice).
    #[test]
    fn natural_name_routes_first_of_several() {
        // "mira" appears before "otto" → mira wins (left-to-right).
        assert_routed(
            &route("mira can you and otto sort the schedule?", None),
            "mira",
            AddressedBy::Name,
        );
    }

    // No name in the text → the concierge (otto) picks it up.
    #[test]
    fn natural_no_name_routes_to_otto_concierge() {
        assert_routed(
            &route("hi guys what are you doing", None),
            "otto",
            AddressedBy::Concierge,
        );
    }

    // Reply-chain: a threaded reply with no name lands on the replied-to bot.
    #[test]
    fn natural_reply_chain_routes_to_replied_bot() {
        assert_routed(
            &route("yes that works", Some("bruno_casapinello_bot")),
            "bruno",
            AddressedBy::ReplyChain,
        );
    }

    // A name in the text overrides the reply target (explicit beats context).
    #[test]
    fn natural_name_overrides_reply_chain() {
        assert_routed(
            &route("actually ask nora", Some("bruno_casapinello_bot")),
            "nora",
            AddressedBy::Name,
        );
    }

    // An explicit @mention still wins (R17 path), handle stripped from body.
    #[test]
    fn natural_mention_still_wins_and_strips_handle() {
        let r = route_natural(
            Some("supergroup"),
            Some("-100999"),
            "@bruno_casapinello_bot can we swap Friday?",
            &["bruno_casapinello_bot".to_string()],
            None,
            &casa_config(),
        );
        match r {
            NaturalRoute::ToBot {
                bot,
                body,
                addressed_by,
                ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some("bruno"));
                assert_eq!(addressed_by, AddressedBy::Mention);
                assert_eq!(body, "can we swap Friday?");
            }
            other => panic!("expected ToBot(bruno), got {other:?}"),
        }
    }

    // A private (1:1) chat is never natural-routed — passthrough to 1:1.
    #[test]
    fn natural_private_chat_is_passthrough() {
        let r = route_natural(
            Some("private"),
            Some("55501234"),
            "hi guys what are you doing",
            &[],
            None,
            &casa_config(),
        );
        assert_eq!(r, NaturalRoute::Private);
    }

    // A group message with no chat id has nothing to reply to → dropped.
    #[test]
    fn natural_group_without_chat_id_is_dropped() {
        let r = route_natural(Some("group"), None, "nora hello", &[], None, &casa_config());
        assert_eq!(r, NaturalRoute::Drop);
    }

    // With no concierge configured, an unaddressed message has nowhere to go.
    #[test]
    fn natural_no_concierge_configured_drops_unaddressed() {
        let config = cfg_with_bots(&[("nora", "-100999", Some("nora"), Some("nora_bot"))]);
        let r = route_natural(
            Some("group"),
            Some("-100999"),
            "hi guys what are you doing",
            &[],
            None,
            &config,
        );
        assert_eq!(r, NaturalRoute::Drop);
    }

    // A group `/standup` update, as the listener sees it, is intercepted (the
    // routed body still satisfies `is_standup_command`) and the plan is exactly
    // four posts in roster order. This exercises the *listener* decision path
    // (route_natural → is_standup_command → plan_standup), not just the
    // on-demand `wg telegram standup` subcommand the smoke test drives.
    #[test]
    fn group_standup_update_is_intercepted_and_plans_four_posts() {
        use crate::graph::WorkGraph;
        use crate::notify::telegram_standup as standup;
        let cfg = casa_config();

        // The three forms Telegram delivers `/standup` in a group.
        for text in ["/standup", "/standup@otto_casapinello_bot", "wg standup"] {
            let body =
                match route_natural(Some("supergroup"), Some("-100999"), text, &[], None, &cfg) {
                    NaturalRoute::ToBot { body, .. } => body,
                    other => panic!("expected ToBot for {text:?}, got {other:?}"),
                };
            assert!(
                standup::is_standup_command(&body),
                "listener must intercept {text:?} (routed body {body:?})"
            );
        }

        // And the intercept posts exactly four voices in roster order.
        let posts = standup::plan_standup(&WorkGraph::new(), &cfg, standup::DEFAULT_ROSTER);
        let ids: Vec<&str> = posts.iter().map(|p| p.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["nora", "bruno", "mira", "otto"]);
    }

    #[test]
    fn reply_to_bot_username_extracts_bot_handle_only() {
        // Reply to a BOT's message → its @username (lower-cased, no @).
        let msg = serde_json::json!({
            "text": "yes that works",
            "reply_to_message": {
                "from": { "is_bot": true, "username": "Bruno_Casapinello_Bot" }
            }
        });
        assert_eq!(
            reply_to_bot_username(&msg).as_deref(),
            Some("bruno_casapinello_bot")
        );

        // Reply to a HUMAN → None (reply-chain must not fire).
        let human = serde_json::json!({
            "text": "sure",
            "reply_to_message": { "from": { "is_bot": false, "username": "luca" } }
        });
        assert!(reply_to_bot_username(&human).is_none());

        // Not a reply at all → None.
        let plain = serde_json::json!({ "text": "hello" });
        assert!(reply_to_bot_username(&plain).is_none());
    }

    #[test]
    fn first_named_bot_finds_first_word_that_names_a_bot() {
        let cfg = casa_config();
        assert_eq!(
            first_named_bot("please tell mira and nora", &cfg)
                .unwrap()
                .agent_id
                .as_deref(),
            Some("mira")
        );
        assert!(first_named_bot("nothing to see here", &cfg).is_none());
    }
}
