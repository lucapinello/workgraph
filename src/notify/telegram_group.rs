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
}
