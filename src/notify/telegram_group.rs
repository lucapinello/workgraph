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

/// True when the message carries a Telegram `bot_command` entity at **offset 0**
/// — i.e. it opens with a genuine `/slash` command.
///
/// Telegram only tags an actual leading `/command` token with a `bot_command`
/// entity; ordinary chatter, a bare `?`, or any other punctuation carries none.
/// This is the single load-bearing signal the listener uses to decide "is this a
/// command at all": a message is only ever treated as a command when this is
/// true (see `fix-command-leaks` — a bare `?` was being parsed as an operator
/// HELP command and leaked the WG claim/done reference into the family group).
///
/// `entities` is the raw JSON array from the update's `message.entities`
/// (or `null`/absent, in which case the result is `false`).
pub fn has_leading_bot_command(entities: &serde_json::Value) -> bool {
    let arr = match entities.as_array() {
        Some(a) => a,
        None => return false,
    };
    arr.iter().any(|ent| {
        ent.get("type").and_then(|t| t.as_str()) == Some("bot_command")
            && ent.get("offset").and_then(|o| o.as_u64()) == Some(0)
    })
}

/// Extract `@handle` mention tokens from a plain text string, the way the
/// `wg telegram elect` / `route` / `classify` diagnostics approximate the live
/// listener (which reads them from Telegram `mention` entities via
/// [`parse_mention_usernames`]).
///
/// Each whitespace token starting with `@` contributes its handle with the `@`
/// removed, **trailing punctuation stripped**, and lower-cased. The trailing
/// strip is load-bearing: without it `@bruno?` yields the token `bruno?`, which
/// resolves to no bot, so an explicit mention silently degrades. Leading/inner
/// `_` are preserved so real handles (`nora_casapinello_bot`) survive intact.
/// Empty results (a bare `@`) are dropped.
pub fn parse_at_mention_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|t| t.starts_with('@'))
        .map(|t| {
            t.trim_start_matches('@')
                .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
        .collect()
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
/// (case-insensitively); as a fallback the `[telegram.bots.<id>]` key **or** the
/// fronted `agent_id` is matched, so a config that omits `username` still routes
/// if the operator named the bot after its handle or agent.
///
/// **Resilience fallback (fix-mention-precedence).** The live config frequently
/// omits the optional `username` field, yet an explicit `@mention` is the
/// strongest possible address signal and must NEVER be dropped for a missing
/// data field. Telegram bot usernames are, by BotFather rule, always of the form
/// `<name>…bot` and end in `bot`; the family handles follow the
/// `<agent>_casapinello_bot` convention. So when the handle *looks like a bot
/// handle* (contains `_` and ends in `bot`) we also match its **leading
/// underscore-segment** against the bot id / agent id — `@nora_casapinello_bot`
/// resolves to the `nora` bot even with no `username` configured. The
/// `ends_with("bot")` guard keeps this from ever matching ordinary prose tokens
/// (`nora_from_work` does not end in `bot`, so it is not treated as a handle).
///
/// Returns `None` when no bot claims the handle. Tokens are never read into the
/// result.
pub fn resolve_mentioned_bot(username: &str, config: &TelegramConfig) -> Option<ResolvedBot> {
    let want = username.trim_start_matches('@').to_ascii_lowercase();
    if want.is_empty() {
        return None;
    }

    // Leading underscore-segment of a compound Telegram bot handle
    // ("nora_casapinello_bot" -> "nora"), used only when `want` looks like a bot
    // handle (see the resilience fallback in the doc comment above).
    let looks_like_bot_handle = want.contains('_') && want.ends_with("bot");
    let handle_segment = want.split('_').next().unwrap_or(want.as_str());

    for (bot_id, bot) in config.all_bots() {
        let by_username = bot
            .username
            .as_deref()
            .map(|u| u.trim_start_matches('@').eq_ignore_ascii_case(&want))
            .unwrap_or(false);
        let by_bot_id = bot_id.eq_ignore_ascii_case(&want);
        let by_agent_id = bot
            .agent_id
            .as_deref()
            .map(|a| a.eq_ignore_ascii_case(&want))
            .unwrap_or(false);
        let by_handle_segment = looks_like_bot_handle
            && (bot_id.eq_ignore_ascii_case(handle_segment)
                || bot
                    .agent_id
                    .as_deref()
                    .map(|a| a.eq_ignore_ascii_case(handle_segment))
                    .unwrap_or(false));
        if by_username || by_bot_id || by_agent_id || by_handle_segment {
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
    /// Nobody was named, but the ask's CONTENT clearly falls in a household
    /// [`Domain`] whose owner is not the concierge — so the DOMAIN OWNER answers
    /// as the voice (meals → Nora/Bruno, workouts → Mira), the same classifier
    /// that decides task ownership. Carries the domain so `wg telegram elect` can
    /// show the reasoning. Calendar/shopping/coordination and genuinely ambiguous
    /// asks stay [`Concierge`](AddressedBy::Concierge) (Otto).
    Domain(crate::notify::ownership::Domain),
}

impl std::fmt::Display for AddressedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressedBy::Mention => f.write_str("@mention"),
            AddressedBy::Name => f.write_str("name"),
            AddressedBy::ReplyChain => f.write_str("reply-chain"),
            AddressedBy::Concierge => f.write_str("concierge"),
            AddressedBy::Domain(d) => write!(f, "domain:{}", d.slug()),
        }
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

// ===========================================================================
// All-bots-privacy-off responder election
// ===========================================================================
//
// When ALL four bots run with BotFather privacy OFF, every plain group message
// reaches every bot. After cross-bot dedupe (`telegram_dedupe`) leaves exactly
// one copy, [`elect_responders`] decides WHO answers, via Luca's confirmed
// ordered table:
//
//   b. @botusername mention          -> that bot          (mention beats name)
//   a. explicit agent name in text   -> that agent
//   c. reply to a bot's message      -> that agent        (conversation continuity)
//   d. COLLECTIVE address            -> ALL FOUR respond   (roster order)
//   e. team-directed unaddressed ask -> OTTO (coordinator)
//   f. pure human-to-human small talk-> SILENCE   (2+ humans in the group)
//   f'. single-human group, unaddressed:
//         greeting-shaped ("hello")  -> ALL respond briefly (warm greeting)
//         anything else              -> OTTO (concierge)
//
// Precedence is most-explicit-first: a single named/mentioned/replied target
// beats a collective trigger (addressing one person is more specific than "hey
// guys"); the d/e/f boundary is heuristic and documented on each helper. When
// unsure between e and f we prefer f (silence) — a missed summon is better than
// a chatty bot.
//
// The f-vs-f' split is MEMBERSHIP-AWARE (`human_count`): rule f's silence exists
// only to protect human-to-human chatter. A group with a single human (the Casa
// Pinello case: one human, four bots) has none — every message is addressed to
// the team — so a bare "hello" deserves a warm greeting, not silence. The
// conservative silence returns unchanged the moment a second human joins.

/// Collective-address triggers: phrases that address the whole family at once
/// ("hey guys", "everyone", "ciao a tutti"). Case-insensitive. Multi-word
/// entries are matched as substrings; single-word entries are matched as whole
/// words (so `"team"` fires on "hey team" but not on "teamwork"). Tunable — add
/// or remove greetings here to adjust how broadcast is detected.
pub const COLLECTIVE_TRIGGERS: &[&str] = &[
    // English greetings to the group (multi-word: substring-matched)
    "hey guys",
    "hi guys",
    "hello guys",
    "hey everyone",
    "hi everyone",
    "hello everyone",
    "hey all",
    "hi all",
    "hello all",
    "hey team",
    "hi team",
    "hello team",
    "hey folks",
    "hi folks",
    // single-word (whole-word) collective addresses. Kept deliberately tight —
    // bare "guys"/"folks"/"you all" appear too often in affectionate small talk
    // ("love you all", "those guys") to be reliable broadcast signals, so they
    // are excluded; prefer silence over a false four-way reply.
    "everyone",
    "everybody",
    "team",
    // Italian
    "ciao ragazzi",
    "ciao ragazza",
    "ciao a tutti",
    "a tutti",
    "ragazzi",
];

/// Greeting tokens that *open* a message aimed at whoever is listening ("hey …",
/// "ciao …"). Used by the structural summon heuristic: a message that STARTS with
/// one of these AND carries a question mark leans collective even when no trigger
/// phrase matches — the shape of "hey guyd are you aroind?", a group summon a
/// human reads unambiguously but exact-phrase matching misses. Fuzzy-matched for
/// tokens of 4+ chars (so "helo"/"ciap" still open), exact for the short ones
/// ("hey"/"hi"/"yo") to avoid firing on unrelated 2–3 letter words. Tunable.
pub const GREETING_TOKENS: &[&str] = &[
    "hey", "hi", "hello", "hiya", "heya", "yo", "sup", "howdy", "ciao", "hola",
    "hallo",
    // Time-of-day greetings, written as one word ("goodnight", "goodmorning").
    // The two-word forms ("good night", "good morning") are handled by
    // [`is_greeting_shaped`], which pairs a leading "good"/"g" with the
    // time-of-day word so bare "morning"/"night" don't count as greetings.
    "goodnight", "goodmorning", "goodevening", "goodafternoon",
    // Italian
    "buongiorno", "buonasera", "buonanotte", "salve",
];

/// Time-of-day words that turn a leading "good"/"g" into a greeting
/// ("good morning", "g'night"). Fuzzy-matched for 4+ chars. Kept separate from
/// [`GREETING_TOKENS`] so bare "morning"/"night" only read as a greeting when
/// they follow "good" — "morning meeting moved?" is not a greeting.
pub const TIME_OF_DAY_WORDS: &[&str] = &["morning", "evening", "afternoon", "night", "nite"];

/// Broad plural-you address words that, when they accompany a greeting, address
/// the whole family at once ("goodnight guys", "morning folks"). Fuzzy-matched
/// for 4+ chars. Distinct from [`COLLECTIVE_TRIGGERS`], whose single-word entries
/// are deliberately tight — this set only ever fires *together with* a greeting
/// (see [`is_greeting_collective`]), so it can safely include the looser "guys"
/// / "folks" / "all" that would over-fire on their own.
pub const BROAD_ADDRESS_TOKENS: &[&str] = &[
    "guys", "folks", "all", "y'all", "yall", "everyone", "everybody", "team",
    "ragazzi", "ragazza", "tutti",
];

/// Plural-you follower words: when one *immediately follows* "you"/"u" it turns
/// the pronoun into a second-person-plural address to the whole family — "you
/// guys", "you all", "you folks", "you both", "you two", "you lot". Distinct from
/// a third-person "the guys"/"those guys" (no leading "you"), which stays
/// concierge. Fuzzy-matched for 4+ chars. See [`is_plural_you_address`].
pub const PLURAL_YOU_FOLLOWERS: &[&str] = &[
    "guys", "all", "folks", "both", "two", "lot", "everyone", "everybody",
    "team", "crew", "gang", "people", "ragazzi", "tutti", "yous",
];

/// Nouns that, following "the whole …", name the family as one body — "the whole
/// team/family/crew". Fuzzy-matched for 4+ chars. See [`is_plural_you_address`].
pub const WHOLE_GROUP_NOUNS: &[&str] = &[
    "team", "family", "crew", "gang", "group", "household", "fam", "bunch", "lot",
    "squad", "gruppo", "famiglia",
];

/// Opening gratitude tokens ("thanks", "thank", "thx", "grazie", "cheers"). A
/// message that *opens* with one is thank-you small-talk the bots stay out of,
/// even when it names the family ("thanks everyone", "thank you all"). Fuzzy for
/// 4+ chars. See [`is_gratitude_opener`].
pub const GRATITUDE_OPENERS: &[&str] = &[
    "thanks", "thank", "thankyou", "thanx", "thx", "ty", "tysm", "grazie",
    "cheers", "gracias", "merci",
];

/// Endearment verbs ("love you all", "miss you guys"). When one *opens* the
/// message, or sits immediately before the second-person pronoun, the "you all"
/// is the object of affection — small-talk the bots stay out of — not an address
/// asking the family to weigh in. Fuzzy for 4+ chars. See [`is_plural_you_address`].
pub const ENDEARMENT_VERBS: &[&str] = &[
    "love", "loved", "loves", "loving", "miss", "missed", "adore", "adored",
    "appreciate", "bless", "cherish", "hug", "hugs",
];

/// Interjections/verbs that, immediately before a family name, mark it as an
/// *address* rather than narrative mention ("tell bruno", "hey nora"). Tunable.
pub const ADDRESSING_CUES: &[&str] = &[
    "tell", "ask", "hey", "hi", "hello", "get", "ping", "summon", "call", "tag",
    "notify", "remind", "yo", "ciao",
];

/// Words that, immediately AFTER a leading family name, signal it is a vocative
/// opening a request/question ("nora **can** you…", "mira **what's** for
/// dinner"). This lets a leading name with no comma still count as an address,
/// while a leading name followed by an ordinary preposition/verb
/// ("nora **from** work said hi") does NOT — the cheap fix for the
/// name-about-a-human false positive. Tunable.
pub const ADDRESS_FOLLOWERS: &[&str] = &[
    "can", "could", "would", "will", "please", "pls", "plz", "what", "what's",
    "whats", "when", "where", "why", "how", "do", "does", "did", "are", "is",
    "you", "u", "help", "we", "let's", "lets", "i'm", "im", "i",
];

/// Indefinite-agent words that ask "someone in the group" rather than a named
/// person — a strong signal a request is team-directed ("can *someone* …").
pub const INDEFINITE_AGENTS: &[&str] = &["someone", "somebody", "anyone", "anybody"];

/// Household/planning domain keywords. A question or request touching one of
/// these is plausibly *for the team* (the concierge) rather than idle chatter.
/// Tunable — this is the heart of the e-vs-f (ask-vs-small-talk) boundary.
pub const DOMAIN_KEYWORDS: &[&str] = &[
    "dinner", "lunch", "breakfast", "meal", "meals", "menu", "food", "cook",
    "cooking", "recipe",
    "recipes", "grocery", "groceries", "shopping", "shop", "fridge", "pantry",
    "plan", "planning", "schedule", "scheduling", "calendar", "remind", "reminder",
    "reminders", "book", "booking", "appointment", "appointments", "week",
    "weekend", "workout", "workouts", "exercise", "gym", "training", "chore",
    "chores", "clean", "cleaning", "budget", "todo", "task", "tasks", "errand",
    "errands", "dishes", "laundry",
];

/// Sentence-lead phrases that mark a request aimed at the group ("can we …",
/// "let's …", "who can …") rather than a specific person.
pub const REQUEST_LEADS: &[&str] = &[
    "can we", "could we", "should we", "shall we", "let's", "lets ", "we need",
    "we should", "who can", "who could", "who wants", "can someone", "can somebody",
    "could someone", "could somebody", "can anyone", "does anyone", "is anyone",
];

/// Multi-word discussion/opinion phrases. A COLLECTIVELY-elected message that
/// contains one of these reads as an *invitation to deliberate* — the family
/// should talk it through, not fire four independent one-liners. Matched as an
/// ordered fuzzy phrase ([`contains_fuzzy_phrase`], so "what do you all thinl"
/// still fires). See [`is_discussion_ask`]. Tunable.
pub const DISCUSSION_TRIGGERS: &[&[&str]] = &[
    &["what", "do", "you", "think"],
    &["what", "do", "you", "all", "think"],
    &["what", "do", "you", "guys", "think"],
    &["what", "do", "we", "think"],
    &["what", "do", "you", "reckon"],
    &["what", "are", "your", "thoughts"],
    &["your", "thoughts"],
    &["any", "thoughts"],
    &["lets", "discuss"],
    &["weigh", "in"],
    &["hash", "it", "out"],
    &["hash", "this", "out"],
    &["talk", "it", "through"],
    &["find", "consensus"],
    &["reach", "consensus"],
    &["come", "to", "a", "consensus"],
    &["come", "to", "an", "agreement"],
    &["tell", "me", "what", "you", "think"],
    &["tell", "me", "what", "you", "all", "think"],
    &["let", "me", "know", "what", "you", "think"],
];

/// Single discussion/opinion words. A COLLECTIVELY-elected message carrying one
/// of these is an opinion ask — run a discussion round rather than four parallel
/// replies. Fuzzy-matched for 4+ chars (typo-tolerant like the greeting set).
/// Kept to words that a plain group *greeting* never contains, so gating a round
/// on them can never turn "hey guys" into a deliberation. See [`is_discussion_ask`].
pub const DISCUSSION_WORDS: &[&str] = &[
    "discuss", "discussion", "debate", "consensus", "deliberate", "brainstorm",
    "thoughts", "opinions", "opinion", "reckon", "disagree",
    // Italian
    "discutere", "consenso", "opinione", "opinioni",
];

/// Discussion *verbs* — the imperative/hortative core of a "let's talk this
/// through" ask ("discuss this", "let's debate", "can you guys deliberate"). A
/// message opening with one, or carrying one right after a request/hortative cue
/// ([`DISCUSSION_VERB_CUES`]), reads as a deliberation invitation even without a
/// question mark. Fuzzy-matched for 4+ chars. See [`is_discussion_ask`].
pub const DISCUSSION_VERBS: &[&str] = &[
    "discuss", "debate", "deliberate", "brainstorm", "discutere", "dibattere",
];

/// Request/hortative cues that, immediately before a [`DISCUSSION_VERBS`] verb,
/// mark it as an *ask to the group* ("let's **discuss**", "can you guys
/// **debate**", "we should **brainstorm**"). Fuzzy-matched for 4+ chars.
pub const DISCUSSION_VERB_CUES: &[&str] = &[
    "let's", "lets", "let", "please", "pls", "plz", "can", "could", "would",
    "will", "should", "you", "u", "guys", "we", "everyone", "all", "y'all",
    "yall", "someone", "anyone", "gonna", "wanna",
];

/// Who should respond to a de-duplicated inbound group message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Election {
    /// A private (1:1) chat — not a group. Fall through to existing 1:1 handling.
    Private,
    /// No one responds. Either there is nothing to reply to, or the message is
    /// family small-talk that bots deliberately stay out of. Carries the reason
    /// for logging/tests.
    Silence(SilenceReason),
    /// Exactly one family voice answers — an @mention, an addressed name, a
    /// reply-chain, or otto acting as the group coordinator for an unaddressed
    /// ask. `addressed_by` records which rule fired.
    One {
        bot: ResolvedBot,
        reply_chat: String,
        body: String,
        addressed_by: AddressedBy,
    },
    /// A collective address — the WHOLE roster answers, each briefly and
    /// in-voice, in roster order. The caller composes the per-voice replies
    /// (reusing the standup composition path, conversationally). If no named
    /// voices are configured the caller should treat this as silence.
    All { reply_chat: String, body: String },
}

/// Why an [`Election`] resolved to silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceReason {
    /// The update carried no usable chat id — nothing to reply to.
    NoChatId,
    /// Family small-talk with no team address and no ask — bots stay quiet.
    SmallTalk,
    /// The message was team-directed but the concierge voice isn't configured.
    NoVoicesConfigured,
    /// The inbound message was **sent by a bot** (`from.is_bot`). The Fix #0
    /// bot-loop guard: with the family bots running as group admins they receive
    /// each other's replies, so electing on a bot-sent message spawned a
    /// feedback storm (one human message → roster reply → each bot's poller sees
    /// the other bots' replies → re-election → 12 replies). This guard is
    /// unconditional and fires before any rule.
    BotSender,
}

impl std::fmt::Display for SilenceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SilenceReason::NoChatId => "no-chat-id",
            SilenceReason::SmallTalk => "small-talk",
            SilenceReason::NoVoicesConfigured => "no-voices-configured",
            SilenceReason::BotSender => "bot-sender",
        };
        f.write_str(s)
    }
}

/// A concise, PII-safe, single-line summary of an [`Election`] decision for the
/// listener's per-message observability log.
///
/// Exactly one such line is emitted per consumed group message — **including
/// silence** — so "why did no bot reply?" is answerable from the logs alone
/// (small-talk silence previously produced no line at all, making a missing
/// reply indistinguishable from a dropped message). The format is:
///
/// ```text
/// msg=<id> chat=<type> rule=<rule> target=<target>
/// ```
///
/// * `rule` — which election rule fired, one of `mention` / `name` / `reply` /
///   `otto-concierge` / `collective` / `silence:<reason>` / `private`.
/// * `target` — the elected agent id (or `<bot_id>(unbound)` when the bot fronts
///   no agent), `roster` for a collective address, `silence` when no one
///   answers, or `passthrough` for a private 1:1 chat.
///
/// No message text and no bot tokens are ever included — the caller logs the
/// message *id*, never its body, from this function.
pub fn election_decision_summary(
    msg_id: Option<&str>,
    chat_type: Option<&str>,
    election: &Election,
) -> String {
    let (rule, target): (String, String) = match election {
        Election::Private => ("private".to_string(), "passthrough".to_string()),
        Election::Silence(reason) => (format!("silence:{reason}"), "silence".to_string()),
        Election::All { .. } => ("collective".to_string(), "roster".to_string()),
        Election::One {
            bot, addressed_by, ..
        } => {
            let rule = match addressed_by {
                AddressedBy::Mention => "mention".to_string(),
                AddressedBy::Name => "name".to_string(),
                AddressedBy::ReplyChain => "reply".to_string(),
                AddressedBy::Concierge => "otto-concierge".to_string(),
                // Show WHY a non-concierge voice was elected for an unaddressed
                // ask: the domain the classifier read the content into.
                AddressedBy::Domain(d) => format!("domain-{}", d.slug()),
            };
            let target = bot
                .agent_id
                .clone()
                .unwrap_or_else(|| format!("{}(unbound)", bot.bot_id));
            (rule, target)
        }
    };
    format!(
        "msg={} chat={} rule={} target={}",
        msg_id.unwrap_or("none"),
        chat_type.unwrap_or("none"),
        rule,
        target,
    )
}

/// Lower-case whole-word set of `text` (alphanumeric runs, apostrophes kept so
/// `y'all` survives). Used by the collective/ask heuristics.
fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// Ordered, lower-cased whole-word list of `text` (same tokenizer as
/// [`word_set`], but position-preserving). Needed by the fuzzy multi-word
/// trigger match, which walks consecutive tokens, and by the greeting heuristic,
/// which cares which word comes *first*.
fn word_list(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// True when `a` and `b` are within Levenshtein edit distance 1 — at most one
/// substitution, insertion, or deletion. Bounded (no full DP matrix): a length
/// gap over 1 short-circuits, equal lengths allow one mismatch, and a
/// length-1 gap allows a single skip in the longer string. Compares by Unicode
/// scalar so accented Italian letters count as one char each. `a == b` returns
/// true (distance 0).
fn edit_distance_le_1(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (longer, shorter) = if a.len() >= b.len() { (&a, &b) } else { (&b, &a) };
    let ldiff = longer.len() - shorter.len();
    if ldiff > 1 {
        return false;
    }
    if ldiff == 0 {
        // Equal length → at most one substitution.
        return longer
            .iter()
            .zip(shorter.iter())
            .filter(|(x, y)| x != y)
            .count()
            <= 1;
    }
    // Length differs by one → `shorter` must embed in `longer` with a single
    // insertion. Walk both, permitting exactly one skip in `longer`.
    let (mut i, mut j) = (0usize, 0usize);
    let mut skipped = false;
    while i < longer.len() && j < shorter.len() {
        if longer[i] == shorter[j] {
            i += 1;
            j += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
            i += 1;
        }
    }
    true
}

/// A message `word` matches a `trigger` token when it is identical, or — for
/// triggers of 4+ chars — within edit distance 1 (typo tolerance: "guyd"→"guys",
/// "aroind"→"around", "helo"→"hello"). Triggers shorter than 4 chars ("hi",
/// "yo", "hey") require an exact match, so a single-letter slip in a common short
/// word can't summon the whole roster.
fn fuzzy_token_matches(word: &str, trigger: &str) -> bool {
    word == trigger || (trigger.chars().count() >= 4 && edit_distance_le_1(word, trigger))
}

/// True if the ordered `tokens` contain the consecutive `phrase` tokens, each
/// matched with [`fuzzy_token_matches`]. Lets "hey guyd" satisfy the "hey guys"
/// trigger while still requiring both words to line up in order.
fn contains_fuzzy_phrase(tokens: &[String], phrase: &[&str]) -> bool {
    if phrase.is_empty() || tokens.len() < phrase.len() {
        return false;
    }
    tokens.windows(phrase.len()).any(|window| {
        window
            .iter()
            .zip(phrase.iter())
            .all(|(w, p)| fuzzy_token_matches(w, p))
    })
}

/// The structural summon heuristic (task fuzzy-summon, part 2): a message that
/// STARTS with a greeting token ([`GREETING_TOKENS`], fuzzy) AND carries a
/// question mark reads as a group summon even when no trigger phrase matches —
/// "hey guyd are you aroind?". The greeting must be the FIRST word, so narrative
/// chatter that merely mentions a greeting mid-sentence ("he said hey to me
/// yesterday?") is NOT treated as a summon — that keeps the silence preference
/// for non-greeting-shaped chatter.
pub fn is_greeting_shaped_summon(text: &str) -> bool {
    if !text.contains('?') {
        return false;
    }
    let tokens = word_list(text);
    match tokens.first() {
        Some(first) => GREETING_TOKENS
            .iter()
            .any(|g| fuzzy_token_matches(first, g)),
        None => false,
    }
}

/// True if `text` *opens with a greeting* — the shape of "hello", "hey guys",
/// "goodnight", "good morning everyone". Typo-tolerant (task fuzzy-summon): the
/// leading word is fuzzy-matched against [`GREETING_TOKENS`], and a leading
/// "good"/"g" followed by a [`TIME_OF_DAY_WORDS`] word ("good night") also
/// counts. Unlike [`is_greeting_shaped_summon`] this needs no `?` — a bare
/// "hello" qualifies. Keys on the FIRST word only, so narrative chatter that
/// merely mentions a greeting mid-sentence ("he said hi to me") is not a
/// greeting. This is the membership-aware silence rule's greeting test.
pub fn is_greeting_shaped(text: &str) -> bool {
    let tokens = word_list(text);
    let first = match tokens.first() {
        Some(f) => f.as_str(),
        None => return false,
    };
    if GREETING_TOKENS.iter().any(|g| fuzzy_token_matches(first, g)) {
        return true;
    }
    // "good <morning|night|…>" / "g'night" — a leading good-wish + time-of-day.
    if (first == "good" || first == "g") || fuzzy_token_matches(first, "good") {
        if let Some(second) = tokens.get(1) {
            if TIME_OF_DAY_WORDS
                .iter()
                .any(|t| fuzzy_token_matches(second, t))
            {
                return true;
            }
        }
    }
    false
}

/// True if `text` is a greeting that *also* addresses the whole family —
/// "goodnight guys", "morning folks", "hello all". This fires a collective
/// (whole-roster) reply **regardless of how many humans are in the group**,
/// because a greeting explicitly aimed at everyone is a broadcast in any
/// membership. It is the greeting shape ([`is_greeting_shaped`]) AND a
/// [`BROAD_ADDRESS_TOKENS`] word present (fuzzy-matched). Folded into
/// [`is_collective_address`] so it participates in rule d.
pub fn is_greeting_collective(text: &str) -> bool {
    if !is_greeting_shaped(text) {
        return false;
    }
    let tokens = word_list(text);
    tokens.iter().any(|w| {
        BROAD_ADDRESS_TOKENS
            .iter()
            .any(|b| fuzzy_token_matches(w, b))
    })
}

/// True if `text` collectively addresses the family (see [`COLLECTIVE_TRIGGERS`]).
///
/// Matching is typo-tolerant (task fuzzy-summon): multi-word triggers match as
/// consecutive tokens each within edit distance 1 for 4+-char words (so "hey
/// guyd" satisfies "hey guys"); single-word triggers still match only as an
/// exact whole word, so "teamwork" / "everyones" (missing apostrophe) don't
/// over-fire. A greeting-shaped question ("hey … ?") counts as a collective
/// summon via [`is_greeting_shaped_summon`], and a greeting that names the whole
/// group ("goodnight guys") counts via [`is_greeting_collective`] — both even
/// with no trigger phrase at all.
pub fn is_collective_address(text: &str) -> bool {
    // Gratitude / endearment small-talk that names the family ("thanks everyone",
    // "love you all") is NOT a broadcast summons — checked FIRST so a bare
    // "everyone"/"guys" trigger word inside it doesn't fire. Bots stay out
    // (rule f silence).
    if is_gratitude_opener(text) || is_endearment_opener(text) {
        return false;
    }
    let tokens = word_list(text);
    for trig in COLLECTIVE_TRIGGERS {
        if trig.contains(' ') {
            let phrase: Vec<&str> = trig.split(' ').collect();
            if contains_fuzzy_phrase(&tokens, &phrase) {
                return true;
            }
        } else if tokens.iter().any(|w| w == trig) {
            return true;
        }
    }
    is_greeting_shaped_summon(text) || is_greeting_collective(text)
}

/// True if `text` *opens with* a gratitude token ("thanks …", "thank you …",
/// "grazie …"). Keyed on the FIRST word so a mid-sentence "…and thanks" doesn't
/// count. Typo-tolerant for 4+-char tokens. Used to keep thank-you small-talk
/// ("thanks everyone", "thank you all") out of the collective / plural-address
/// broadcast rules — the family thanking each other is not a team summons.
pub fn is_endearment_opener(text: &str) -> bool {
    let tokens = word_list(text);
    match tokens.first() {
        Some(first) => ENDEARMENT_VERBS
            .iter()
            .any(|t| fuzzy_token_matches(first, t)),
        None => false,
    }
}

pub fn is_gratitude_opener(text: &str) -> bool {
    let tokens = word_list(text);
    match tokens.first() {
        Some(first) => GRATITUDE_OPENERS
            .iter()
            .any(|t| fuzzy_token_matches(first, t)),
        None => false,
    }
}

/// True if `text` addresses the whole family in the **second person plural**
/// *anywhere* in the sentence — "can you guys discuss this", "tell me what you
/// all think", "what do all of you reckon?", "the whole team should weigh in".
/// Unlike [`is_collective_address`] (greeting-shaped openers only) this fires
/// mid-sentence, and unlike a plain team-directed ask it elects the WHOLE roster:
/// a message that explicitly asks *everyone present* to weigh in wants every
/// voice, not one concierge answer (this is the group-discussion prompt).
///
/// Recognised second-person-plural shapes (all typo-tolerant for 4+-char words):
/// * "you guys" / "you all" / "you folks" / "you both" / "you two" / "you lot"
///   — "you"/"u" immediately followed by a [`PLURAL_YOU_FOLLOWERS`] word;
/// * "all of you" / "each of you" / "both of you" / "any of you" — a quantifier
///   then "of you";
/// * "the whole team/family/crew" — "whole" + a [`WHOLE_GROUP_NOUNS`] word.
///
/// Deliberately does NOT fire on third-person mentions ("tell the guys I said
/// hi", "those guys were loud") — "guys"/"folks" only count when the preceding
/// token is the second-person "you"/"u". Gratitude openers ("thank you all") are
/// excluded so they fall through to silence. Bare "everyone"/"everybody" is left
/// to [`is_collective_address`] (rule d, after the ask check) so a third-person
/// "tell everyone dinner's ready" still routes to the concierge.
pub fn is_plural_you_address(text: &str) -> bool {
    // Gratitude ("thank you all") and endearment ("love you all so much") that
    // name the family are affectionate small-talk, not a request for the roster
    // to weigh in — bots stay out.
    if is_gratitude_opener(text) || is_endearment_opener(text) {
        return false;
    }
    let tokens = word_list(text);
    for (i, w) in tokens.iter().enumerate() {
        // "you guys" / "you all" / "you both" / …
        if w == "you" || w == "u" {
            // "…love you all", "…miss you guys", "…thank you all" — the pronoun is
            // the object of affection/thanks, not an address. Skip when the word
            // right before "you" is an endearment or gratitude verb.
            let prev_is_endearment = i
                .checked_sub(1)
                .and_then(|j| tokens.get(j))
                .map(|p| {
                    ENDEARMENT_VERBS
                        .iter()
                        .chain(GRATITUDE_OPENERS.iter())
                        .any(|e| fuzzy_token_matches(p, e))
                })
                .unwrap_or(false);
            if prev_is_endearment {
                continue;
            }
            if let Some(next) = tokens.get(i + 1) {
                if PLURAL_YOU_FOLLOWERS
                    .iter()
                    .any(|t| fuzzy_token_matches(next, t))
                {
                    return true;
                }
            }
        }
        // "all of you" / "each of you" / "both of you" / "any of you"
        if w == "of" {
            let prev_quantifier = i
                .checked_sub(1)
                .and_then(|j| tokens.get(j))
                .map(|p| {
                    ["all", "each", "both", "any", "some", "many", "one"]
                        .iter()
                        .any(|q| *q == p.as_str())
                })
                .unwrap_or(false);
            let next_you = tokens
                .get(i + 1)
                .map(|n| n == "you" || n == "u" || fuzzy_token_matches(n, "yous"))
                .unwrap_or(false);
            if prev_quantifier && next_you {
                return true;
            }
        }
        // "the whole team/family/crew"
        if fuzzy_token_matches(w, "whole") {
            if let Some(next) = tokens.get(i + 1) {
                if WHOLE_GROUP_NOUNS
                    .iter()
                    .any(|t| fuzzy_token_matches(next, t))
                {
                    return true;
                }
            }
        }
    }
    false
}

/// True if `text` reads as an *opinion / discussion ask* — an invitation for the
/// family to deliberate and land on a view ("can you guys discuss this and find
/// consensus", "what do you all think?", "thoughts?", "let's debate the plan"),
/// as opposed to a plain collective greeting ("hey guys are you around?") or an
/// idle statement that merely mentions a discussion word ("I had some thoughts
/// today").
///
/// It elects the WHOLE roster (see [`elect_responders`], rule d-discussion) and
/// then drives a **discussion round** — sequenced, reacting takes plus an Otto
/// wrap-up (see [`telegram_discussion`]). Because that fans four bots out, this
/// deliberately requires an *ask shape*, not just a keyword:
/// * any multi-word discussion phrase in [`DISCUSSION_TRIGGERS`] ("what do you
///   all think", "find consensus", "weigh in") — these are inherently asks; or
/// * a `?`-question that also carries a [`DISCUSSION_WORDS`] word ("thoughts?",
///   "consensus on this?"); or
/// * an imperative/hortative [`DISCUSSION_VERBS`] verb — one that opens the
///   message or follows a [`DISCUSSION_VERB_CUES`] cue ("let's discuss",
///   "can you guys debate").
///
/// A bare discussion word in a plain statement ("I had some thoughts today")
/// matches none of these, so it never fans out the roster. All matching is
/// typo-tolerant for 4+-char tokens.
///
/// [`telegram_discussion`]: crate::notify::telegram_discussion
pub fn is_discussion_ask(text: &str) -> bool {
    let tokens = word_list(text);
    if tokens.is_empty() {
        return false;
    }
    // 1. Multi-word discussion phrases are inherently asks.
    if DISCUSSION_TRIGGERS
        .iter()
        .any(|phrase| contains_fuzzy_phrase(&tokens, phrase))
    {
        return true;
    }
    // 2. An interrogative sentence carrying a discussion word.
    let is_question = text.trim_end().ends_with('?');
    if is_question
        && tokens
            .iter()
            .any(|w| DISCUSSION_WORDS.iter().any(|d| fuzzy_token_matches(w, d)))
    {
        return true;
    }
    // 3. An imperative/hortative discussion verb.
    has_imperative_discussion_verb(&tokens)
}

/// True if `tokens` contain a [`DISCUSSION_VERBS`] verb used imperatively — it
/// opens the message ("discuss this") or sits immediately after a
/// [`DISCUSSION_VERB_CUES`] cue ("let's debate", "can you guys deliberate"). A
/// discussion verb buried elsewhere ("we had a long discussion yesterday" — note
/// "discussion" is a noun, not in [`DISCUSSION_VERBS`]) does not qualify.
fn has_imperative_discussion_verb(tokens: &[String]) -> bool {
    for (i, w) in tokens.iter().enumerate() {
        if !DISCUSSION_VERBS.iter().any(|v| fuzzy_token_matches(w, v)) {
            continue;
        }
        if i == 0 {
            return true;
        }
        if let Some(prev) = tokens.get(i - 1) {
            if DISCUSSION_VERB_CUES
                .iter()
                .any(|c| fuzzy_token_matches(prev, c))
            {
                return true;
            }
        }
    }
    false
}

/// Find the first family name in `text` that is used to *address* an agent
/// (not merely mentioned in passing) and resolve it to a configured bot.
///
/// A bare family name only counts when it is in an addressing position:
/// * a leading vocative — the first word, followed by `,` `:` `!` `?` `-` `;`
///   `.`, or the whole message ("nora, …" / "nora");
/// * a trailing vocative — the last word, with a comma on the preceding token
///   ("what's for dinner, nora?");
/// * immediately after an addressing cue ("tell bruno …", "hey nora").
///
/// This deliberately does NOT match a name buried mid-sentence, so
/// "**nora** from work said hi" (talking *about* a human named Nora) does not
/// summon the Nora bot. **Known limitation:** the heuristic keys on position and
/// cue words, not meaning — "hey nora" from one human to another human also
/// named Nora would still route to the bot. This is accepted as cheap-and-good;
/// the escape hatch is that a real summon almost always uses a comma or a cue,
/// and when in doubt the family can @mention. See docs/09 §natural-group.
pub fn addressed_name_bot(text: &str, config: &TelegramConfig) -> Option<ResolvedBot> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, raw) in words.iter().enumerate() {
        // Strip leading noise (keep '@' and '_' so handles survive), then take
        // the bare word and remember the punctuation that trailed it.
        let lead_trimmed =
            raw.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '@');
        let word = lead_trimmed.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if word.is_empty() {
            continue;
        }
        let bot = match resolve_mentioned_bot(word, config) {
            Some(b) => b,
            None => continue,
        };

        let trailing = &lead_trimmed[word.len()..];
        let vocative_punct = trailing.starts_with([',', ':', '!', '?', '-', ';', '.']);
        let is_first = i == 0;
        let is_last = i + 1 == words.len();
        let prev = if i > 0 { Some(words[i - 1]) } else { None };
        let prev_word = prev
            .map(|p| {
                p.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let prev_is_cue = ADDRESSING_CUES.contains(&prev_word.as_str());
        let prev_ends_comma = prev.map(|p| p.trim_end().ends_with(',')).unwrap_or(false);
        // The word after the name (cleaned), for the leading-vocative-without-
        // comma case ("nora can you …").
        let next_word = words
            .get(i + 1)
            .map(|p| {
                p.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let next_is_request = ADDRESS_FOLLOWERS.contains(&next_word.as_str());

        let addressed = (is_first && (vocative_punct || words.len() == 1 || next_is_request))
            || prev_is_cue
            || (is_last && prev_ends_comma);
        if addressed {
            return Some(bot);
        }
    }
    None
}

/// True if `text` is a team-directed request/question with no named target —
/// the e case that routes to otto (the coordinator). This is the deliberately
/// conservative half of the e-vs-f boundary: it fires only on reasonably clear
/// asks, and everything else falls through to silence.
///
/// It fires when any of:
/// * an indefinite agent ("can **someone** …") appears with a question, a
///   request lead, or a domain keyword;
/// * a group request lead ("can we …", "let's …") touches a [`DOMAIN_KEYWORDS`]
///   topic;
/// * a `?`-question touches a domain topic and is NOT aimed at a specific person
///   ("what's the plan for dinner?" fires; "did you eat?" does not).
///
/// The second-person guard (`you`/`your`/`u`) suppresses the domain-question
/// branch so human-to-human questions ("you free this weekend?") stay silent —
/// unless an indefinite agent or explicit group lead overrides it.
pub fn is_team_directed_ask(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    let words = word_set(text);
    let is_question = lower.ends_with('?');
    let has_indefinite = INDEFINITE_AGENTS.iter().any(|w| words.contains(*w));
    let has_domain = DOMAIN_KEYWORDS.iter().any(|w| words.contains(*w));
    let request_lead = REQUEST_LEADS.iter().any(|p| lower.starts_with(p));
    let human_directed =
        words.contains("you") || words.contains("your") || words.contains("u");

    // Strongest signal: explicitly asking "someone/anyone" in the group.
    if has_indefinite && (is_question || has_domain || request_lead) {
        return true;
    }
    // A group request ("can we …", "let's …") about a household domain.
    if request_lead && has_domain {
        return true;
    }
    // A domain question not aimed at a specific person.
    if is_question && has_domain && !human_directed {
        return true;
    }
    false
}

/// Elect the responder(s) for one de-duplicated inbound group message.
///
/// See the section header above for the ordered table and the d/e/f boundary
/// rationale. Non-group chats yield [`Election::Private`]; a group message with
/// no chat id yields [`Election::Silence`]`(NoChatId)`.
///
/// `human_count` is how many **human** members the group has (onboarded humans
/// present in the chat — see `human_agent_id_set`). It makes the silence-vs-
/// respond boundary membership-aware: the conservative small-talk silence (rule
/// f) exists to protect *human-to-human* chatter, but a group with a single
/// human has none — every message is necessarily addressed to the team. So when
/// `human_count <= 1`, a greeting-shaped message ("hello", "goodnight") earns a
/// brief whole-roster greeting and any other unaddressed text leans on the
/// concierge (otto) instead of falling silent. With 2+ humans the conservative
/// silence returns unchanged (family chatter is protected the moment a second
/// human joins).
/// The family voice that should ANSWER an unaddressed but content-classifiable
/// ask — the round-2 election refinement (Luca, 2026-07-14: "otto replied for
/// food instead of the chef or dietician").
///
/// It reuses the SAME [`ownership::classify_domain`] + owner map that decides task
/// ownership, so the VOICE that answers and the persona that eventually OWNS the
/// resulting task agree: food → Bruno (cooking/recipes) or Nora (nutrition /
/// what-should-we-eat), workouts → Mira. Returns `Some((bot, domain))` only when
/// the resolved owner is a NON-concierge persona that the group actually has a bot
/// for — so calendar / shopping / coordination (Otto's domains) and any genuinely
/// ambiguous ask fall through to the concierge unchanged, and a voice the group
/// never configured is never elected. Explicit addressing (mention / name / reply)
/// is resolved BEFORE this in [`elect_responders`], so a named voice always wins.
///
/// The owner map is the shipped Casa roster ([`OwnerMap::casa_default`]); a
/// household that reassigns domains still gets authoritative ownership at the
/// creation choke point (which reads `household.toml`), and its election voice
/// follows the default roster.
///
/// [`ownership::classify_domain`]: crate::notify::ownership::classify_domain
/// [`OwnerMap::casa_default`]: crate::notify::ownership::OwnerMap::casa_default
fn domain_voice(
    text: &str,
    config: &TelegramConfig,
) -> Option<(ResolvedBot, crate::notify::ownership::Domain)> {
    use crate::notify::ownership::{classify_domain, OwnerMap};
    let domain = classify_domain(text);
    let map = OwnerMap::casa_default();
    let owner = map.owner_for_domain(domain)?;
    // An Otto-owned domain (calendar / shopping / coordination) or ambiguous ask
    // keeps the concierge rule — only a more-specific in-domain voice refines it.
    if owner.eq_ignore_ascii_case(CONCIERGE_BOT) {
        return None;
    }
    let bot = resolve_mentioned_bot(owner, config)?;
    Some((bot, domain))
}

pub fn elect_responders(
    chat_type: Option<&str>,
    chat_id: Option<&str>,
    text: &str,
    mention_usernames: &[String],
    reply_to_bot: Option<&str>,
    sender_is_bot: bool,
    human_count: usize,
    config: &TelegramConfig,
) -> Election {
    // Fix #0 — the bot-loop guard. UNCONDITIONAL and first: a message sent by a
    // bot (ANY bot, including our own four seen on a sibling bot's poller) is
    // never elected, routed, or composed. Without this a single human message
    // fanned out into a self-amplifying storm of roster replies. Applies in
    // every chat type — a bot DMing a bot must not compose either.
    if sender_is_bot {
        return Election::Silence(SilenceReason::BotSender);
    }

    let is_group = matches!(chat_type, Some("group") | Some("supergroup"));
    if !is_group {
        return Election::Private;
    }
    let reply_chat = match chat_id {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return Election::Silence(SilenceReason::NoChatId),
    };

    // b. @mention — the most explicit signal; mention beats name-in-text.
    for username in mention_usernames {
        if let Some(bot) = resolve_mentioned_bot(username, config) {
            return Election::One {
                bot,
                reply_chat,
                body: strip_mention(text, username),
                addressed_by: AddressedBy::Mention,
            };
        }
    }

    // a. Explicit addressed name.
    if let Some(bot) = addressed_name_bot(text, config) {
        return Election::One {
            bot,
            reply_chat,
            body: text.to_string(),
            addressed_by: AddressedBy::Name,
        };
    }

    // c. Reply threaded onto a bot's own message — conversation continuity.
    if let Some(uname) = reply_to_bot {
        if let Some(bot) = resolve_mentioned_bot(uname, config) {
            return Election::One {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::ReplyChain,
            };
        }
    }

    // d-plural. Explicit SECOND-PERSON-PLURAL address anywhere in the message —
    // "can you guys discuss this", "tell me what you all think", "all of you",
    // "the whole team" — elects the WHOLE roster, and does so BEFORE the ask
    // check below. A message that explicitly asks *everyone present* to weigh in
    // wants every voice, not one concierge answer (this is the group-discussion
    // prompt Luca reported swallowed on 2026-07-12). Third-person mentions
    // ("tell the guys …") and gratitude ("thanks everyone") do not match here —
    // see [`is_plural_you_address`] — so they still fall through to the concierge
    // / silence rules. Bare "everyone"/"everybody" stays with rule d (below) so
    // the ask check keeps precedence for "tell everyone dinner's ready".
    if is_plural_you_address(text) {
        return Election::All {
            reply_chat,
            body: text.to_string(),
        };
    }

    // d-discussion. An opinion / DISCUSSION ask — "let's discuss this and find
    // consensus", "what do you all think?", "thoughts?" — elects the WHOLE
    // roster, BEFORE the team-directed-ask rule below. Luca asked the group to
    // "discuss this and find consensus" and wanted every voice + a wrap-up, not
    // one concierge answer; a discussion ask that also happens to name a
    // household topic ("let's discuss the weekend plan") must therefore NOT be
    // swallowed to otto here. `is_discussion_ask` requires an ask *shape* (a
    // discussion phrase, a `?`-question with a discussion word, or an imperative
    // discuss/debate verb), so an idle statement mentioning a discussion word
    // ("I had some thoughts today") does not fan out the roster. The
    // `Election::All` handler then runs the discussion ROUND (sequenced in-voice
    // takes + an Otto synthesis) rather than four independent replies — see
    // [`is_discussion_ask`] and `telegram_discussion`.
    if is_discussion_ask(text) {
        return Election::All {
            reply_chat,
            body: text.to_string(),
        };
    }

    // e-before-d for ASKS (Fix #4a). Collective (rule d) is reserved for
    // *greetings that address everyone* — "hey guys", "ciao a tutti". A message
    // that carries an actual question or request must NOT trigger a four-way
    // roster broadcast (that produced Luca's "elected collective, answered with
    // generic greetings, wrong answer"): it is a single ask, answered by ONE
    // voice. A named voice already won above (rules a–c); an unaddressed ask is
    // the concierge's (otto). So a team-directed ask is routed to otto here even
    // when it also happens to contain a collective trigger word.
    if is_team_directed_ask(text) {
        // Round-2 refinement: when the ask's CONTENT belongs to a household domain
        // whose owner is not the concierge (food → Bruno/Nora, workouts → Mira),
        // that owner ANSWERS as the voice. Otherwise the unaddressed ask is the
        // concierge's (Otto), exactly as before.
        if let Some((bot, domain)) = domain_voice(text, config) {
            return Election::One {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::Domain(domain),
            };
        }
        return match resolve_mentioned_bot(CONCIERGE_BOT, config) {
            Some(bot) => Election::One {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::Concierge,
            },
            None => Election::Silence(SilenceReason::NoVoicesConfigured),
        };
    }

    // d. Collective address — a greeting to the whole family, no specific ask.
    // The whole roster answers, each briefly and in-voice, in roster order.
    if is_collective_address(text) {
        return Election::All {
            reply_chat,
            body: text.to_string(),
        };
    }

    // Membership-aware silence boundary. Rule f's silence protects human-to-
    // human chatter — but a group with a single human has no such chatter to
    // protect: every message is addressed to the team. So when the group holds
    // at most one human, an unaddressed message is answered rather than silenced:
    //   * a greeting ("hello", "goodnight") → a brief whole-roster greeting;
    //   * anything else → the concierge (otto), the group's coordinator voice.
    // The moment a second human joins (`human_count >= 2`) the conservative
    // silence below returns and family small-talk is protected again.
    if human_count <= 1 {
        if is_greeting_shaped(text) {
            return Election::All {
                reply_chat,
                body: text.to_string(),
            };
        }
        // Round-2 refinement (same as the team-ask branch): a content-classifiable
        // ask goes to its DOMAIN owner's voice; anything ambiguous stays with the
        // concierge (Otto).
        if let Some((bot, domain)) = domain_voice(text, config) {
            return Election::One {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::Domain(domain),
            };
        }
        return match resolve_mentioned_bot(CONCIERGE_BOT, config) {
            Some(bot) => Election::One {
                bot,
                reply_chat,
                body: text.to_string(),
                addressed_by: AddressedBy::Concierge,
            },
            None => Election::Silence(SilenceReason::NoVoicesConfigured),
        };
    }

    // f. Pure human-to-human small talk (2+ humans present) — bots stay silent.
    Election::Silence(SilenceReason::SmallTalk)
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

    // --- has_leading_bot_command (fix-command-leaks) ----------------------

    #[test]
    fn bot_command_at_offset_zero_is_a_command() {
        // A genuine `/help` — Telegram tags it with a bot_command entity at 0.
        let entities = serde_json::json!([{ "type": "bot_command", "offset": 0, "length": 5 }]);
        assert!(has_leading_bot_command(&entities));
    }

    #[test]
    fn bare_punctuation_is_not_a_command() {
        // The reported leak: `?` (and any punctuation) carries NO entity, so it
        // must never be treated as a command.
        assert!(!has_leading_bot_command(&serde_json::Value::Null));
        assert!(!has_leading_bot_command(&serde_json::json!([])));
    }

    #[test]
    fn a_mention_is_not_a_command() {
        // `@nora_casapinello_bot ?` — a mention entity, but no bot_command.
        let entities = serde_json::json!([{ "type": "mention", "offset": 0, "length": 21 }]);
        assert!(!has_leading_bot_command(&entities));
    }

    #[test]
    fn bot_command_after_a_mention_is_not_at_offset_zero() {
        // `@otto /shopping` — the slash command sits after the mention, so it is
        // NOT a leading command; by Fix (2) the @mention election owns it.
        let entities = serde_json::json!([
            { "type": "mention", "offset": 0, "length": 5 },
            { "type": "bot_command", "offset": 6, "length": 9 },
        ]);
        assert!(!has_leading_bot_command(&entities));
    }

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

    // =======================================================================
    // All-bots-privacy-off responder election (Luca's ordered table a–f)
    // =======================================================================

    /// Run the election in the Casa Pinello group with the given mentions /
    /// reply-chain against the four-bot roster, in a **two-human** group. Two
    /// humans keep the conservative silence rule (rule f) in force, so the a–f
    /// table's silence expectations hold; the single-human rule is exercised by
    /// [`elect_solo`].
    fn elect(text: &str, mentions: &[&str], reply_to_bot: Option<&str>) -> Election {
        elect_with_humans(text, mentions, reply_to_bot, 2)
    }

    /// Run the election in a **single-human** Casa Pinello group (one human,
    /// four bots) — the live case that motivates the membership-aware rule.
    fn elect_solo(text: &str, mentions: &[&str], reply_to_bot: Option<&str>) -> Election {
        elect_with_humans(text, mentions, reply_to_bot, 1)
    }

    fn elect_with_humans(
        text: &str,
        mentions: &[&str],
        reply_to_bot: Option<&str>,
        human_count: usize,
    ) -> Election {
        let mentions: Vec<String> = mentions.iter().map(|s| s.to_string()).collect();
        elect_responders(
            Some("supergroup"),
            Some("-100999"),
            text,
            &mentions,
            reply_to_bot,
            false, // human sender in the a–f election tests
            human_count,
            &casa_config(),
        )
    }

    /// Assert the election picked exactly one voice, by which rule, replying to
    /// the group.
    fn assert_one(election: &Election, agent: &str, by: AddressedBy) {
        match election {
            Election::One {
                bot,
                reply_chat,
                addressed_by,
                ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some(agent), "elected agent");
                assert_eq!(*addressed_by, by, "addressed_by");
                assert_eq!(reply_chat, "-100999", "reply target is the group");
            }
            other => panic!("expected One({agent}), got {other:?}"),
        }
    }

    // ---- a. explicit name ------------------------------------------------

    #[test]
    fn elect_leading_name_with_comma_to_that_agent() {
        assert_one(
            &elect("nora, what's for dinner?", &[], None),
            "nora",
            AddressedBy::Name,
        );
    }

    #[test]
    fn elect_addressing_cue_before_name() {
        assert_one(
            &elect("tell bruno the curry was great", &[], None),
            "bruno",
            AddressedBy::Name,
        );
    }

    #[test]
    fn elect_leading_name_then_request_word_no_comma() {
        // "mira can you …" — leading vocative without a comma still addresses.
        assert_one(
            &elect("mira can you sort the schedule?", &[], None),
            "mira",
            AddressedBy::Name,
        );
    }

    #[test]
    fn elect_trailing_vocative_after_comma() {
        assert_one(
            &elect("what's for dinner, bruno?", &[], None),
            "bruno",
            AddressedBy::Name,
        );
    }

    // ---- b. mention beats name-in-text -----------------------------------

    #[test]
    fn elect_mention_beats_name_in_text() {
        // Text names nora, but bruno is @mentioned → mention wins, handle
        // stripped from the routed body.
        let election = elect(
            "nora can you ask @bruno_casapinello_bot about dinner",
            &["bruno_casapinello_bot"],
            None,
        );
        match &election {
            Election::One {
                bot,
                body,
                addressed_by,
                ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some("bruno"));
                assert_eq!(*addressed_by, AddressedBy::Mention);
                assert!(
                    !body.contains("@bruno_casapinello_bot"),
                    "addressed handle stripped from body, got {body:?}"
                );
            }
            other => panic!("expected One(bruno) by mention, got {other:?}"),
        }
    }

    // ---- b (regression). @mention MUST win even without a configured
    //        `username`, and even when the rest of the text looks like small
    //        talk. This mirrors the LIVE .wg/notify.toml, which omits the
    //        optional `username` field — the exact config that produced
    //        `elect "@nora_casapinello_bot what about you?" -> silence`.
    //        (fix-mention-precedence)

    /// The four-bot roster with NO `username` configured — a faithful copy of
    /// the live deployment's config, where only `agent_id` and the bot-id key
    /// identify each bot.
    fn casa_config_no_usernames() -> TelegramConfig {
        cfg_with_bots(&[
            ("nora", "-100999", Some("nora"), None),
            ("bruno", "-100999", Some("bruno"), None),
            ("mira", "-100999", Some("mira"), None),
            ("otto", "-100999", Some("otto"), None),
        ])
    }

    #[test]
    fn resolve_real_handle_without_configured_username() {
        // The core defect: the real Telegram handle must resolve to its bot even
        // though `username` is unset in the live config.
        let cfg = casa_config_no_usernames();
        for (handle, agent) in [
            ("nora_casapinello_bot", "nora"),
            ("BRUNO_casapinello_bot", "bruno"), // case-insensitive
            ("mira_casapinello_bot", "mira"),
            ("otto_casapinello_bot", "otto"),
        ] {
            let resolved = resolve_mentioned_bot(handle, &cfg)
                .unwrap_or_else(|| panic!("{handle} must resolve without a configured username"));
            assert_eq!(resolved.agent_id.as_deref(), Some(agent), "handle {handle:?}");
        }
    }

    #[test]
    fn resolve_prose_token_is_not_mistaken_for_a_handle() {
        // The `ends_with("bot")` guard: an ordinary underscore token that shares
        // a leading segment with a bot id must NOT resolve — only real bot
        // handles (…bot) get the segment fallback.
        let cfg = casa_config_no_usernames();
        assert!(resolve_mentioned_bot("nora_from_work", &cfg).is_none());
        assert!(resolve_mentioned_bot("bruno_and_friends", &cfg).is_none());
        // A bare exact bot-id / agent-id still resolves.
        assert_eq!(
            resolve_mentioned_bot("bruno", &cfg).and_then(|b| b.agent_id),
            Some("bruno".to_string())
        );
    }

    #[test]
    fn elect_mention_wins_over_small_talk_without_configured_username() {
        // THE regression under test. `@nora_casapinello_bot what about you?`
        // used to elect `silence(small-talk)` on the live username-less config;
        // an explicit @mention must ALWAYS route to that bot's agent.
        let mentions = parse_at_mention_tokens("@nora_casapinello_bot what about you?");
        let election = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "@nora_casapinello_bot what about you?",
            &mentions,
            None,
            false,
            2,
            &casa_config_no_usernames(),
        );
        match &election {
            Election::One {
                bot, addressed_by, ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some("nora"));
                assert_eq!(*addressed_by, AddressedBy::Mention);
            }
            other => panic!("expected One(nora) by mention, got {other:?}"),
        }
    }

    #[test]
    fn parse_at_mention_tokens_strips_trailing_punctuation() {
        // `@bruno?` must yield the clean handle `bruno` (not `bruno?`), so the
        // mention resolves instead of silently degrading.
        assert_eq!(parse_at_mention_tokens("@bruno?"), vec!["bruno".to_string()]);
        assert_eq!(
            parse_at_mention_tokens("@nora_casapinello_bot what about you?"),
            vec!["nora_casapinello_bot".to_string()]
        );
        assert_eq!(
            parse_at_mention_tokens("hey @Mira, @otto!"),
            vec!["mira".to_string(), "otto".to_string()]
        );
        // A bare `@` contributes nothing.
        assert!(parse_at_mention_tokens("email me @ home").is_empty());
    }

    #[test]
    fn elect_bruno_question_mention_resolves_via_mention_path() {
        // End-to-end for `@bruno?`: CLI-style extraction + election must land on
        // bruno by @mention (the strongest signal), not the name fallback.
        let mentions = parse_at_mention_tokens("@bruno?");
        let election = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "@bruno?",
            &mentions,
            None,
            false,
            2,
            &casa_config_no_usernames(),
        );
        match &election {
            Election::One {
                bot, addressed_by, ..
            } => {
                assert_eq!(bot.agent_id.as_deref(), Some("bruno"));
                assert_eq!(*addressed_by, AddressedBy::Mention);
            }
            other => panic!("expected One(bruno) by mention, got {other:?}"),
        }
    }

    /// SCOPE ADDITION (Luca, 2026-07-14: "otto replied for food instead of the
    /// chef or dietician"). An UNADDRESSED ask whose content clearly falls in a
    /// household domain is answered by that domain's OWNER voice — the same
    /// classifier that decides task ownership — not always the concierge. Explicit
    /// addressing (tested elsewhere) still wins; these rows are all name-free.
    #[test]
    fn elect_unaddressed_ask_routes_to_domain_owner_voice() {
        let cfg = casa_config();
        // (text, expected voice agent, expected addressed_by). human_count = 1
        // (the Casa single-human group), so an unaddressed ask is answered.
        let expect_one = |text: &str, agent: &str, by: AddressedBy| {
            let e = elect_responders(
                Some("supergroup"),
                Some("-100999"),
                text,
                &[],
                None,
                false,
                1,
                &cfg,
            );
            match &e {
                Election::One { bot, addressed_by, .. } => {
                    assert_eq!(bot.agent_id.as_deref(), Some(agent), "voice for {text:?}");
                    assert_eq!(*addressed_by, by, "addressed_by for {text:?}");
                }
                other => panic!("expected One({agent}) for {text:?}, got {other:?}"),
            }
        };
        use crate::notify::ownership::Domain;
        // Food → the chef (Bruno), not Otto.
        expect_one("pizza on friday", "bruno", AddressedBy::Domain(Domain::Cooking));
        // Workouts → Coach Mira.
        expect_one(
            "am I training tomorrow?",
            "mira",
            AddressedBy::Domain(Domain::Workouts),
        );
        // Calendar/logistics and genuinely ambiguous asks stay with the concierge.
        expect_one("when is the dentist?", "otto", AddressedBy::Concierge);
        expect_one("can you help me?", "otto", AddressedBy::Concierge);
    }

    /// Explicit addressing is SUPREME: naming Otto on a food ask keeps Otto, even
    /// though the content would otherwise elect the chef. The domain-voice refine
    /// only ever applies to UNADDRESSED asks.
    #[test]
    fn elect_explicit_name_beats_domain_voice() {
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "otto, sort out pizza for friday",
            &[],
            None,
            false,
            1,
            &casa_config(),
        );
        match &e {
            Election::One { bot, addressed_by, .. } => {
                assert_eq!(bot.agent_id.as_deref(), Some("otto"));
                assert_eq!(*addressed_by, AddressedBy::Name);
            }
            other => panic!("expected One(otto) by name, got {other:?}"),
        }
    }

    /// `wg telegram elect` surfaces the domain reasoning: a domain-elected voice
    /// shows `rule=domain-<slug>` in the observability summary, so an operator can
    /// see WHY the chef (not Otto) answered.
    #[test]
    fn election_summary_shows_domain_reasoning() {
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "pizza on friday",
            &[],
            None,
            false,
            1,
            &casa_config(),
        );
        let summary = election_decision_summary(Some("m1"), Some("supergroup"), &e);
        assert!(
            summary.contains("rule=domain-cooking"),
            "summary should show the domain reasoning, got: {summary}"
        );
        assert!(summary.contains("target=bruno"), "got: {summary}");
    }

    #[test]
    fn elect_full_precedence_ladder_on_live_username_less_config() {
        // Every precedence level, exercised against the live config shape (no
        // `username`), proving the whole a–f ladder holds in production, not just
        // the mention rung. Order asserted: mention > name > reply > collective >
        // ask(otto) > silence.
        let cfg = casa_config_no_usernames();
        let elect = |text: &str, mentions: &[&str], reply: Option<&str>| {
            let m: Vec<String> = mentions.iter().map(|s| s.to_string()).collect();
            elect_responders(
                Some("supergroup"),
                Some("-100999"),
                text,
                &m,
                reply,
                false,
                2,
                &cfg,
            )
        };
        let agent_of = |e: &Election| match e {
            Election::One { bot, addressed_by, .. } => {
                (bot.agent_id.clone(), Some(*addressed_by))
            }
            _ => (None, None),
        };

        // 1. @mention beats name-in-text (mention wins over "ask nora").
        assert_eq!(
            agent_of(&elect(
                "nora can you ask @bruno_casapinello_bot?",
                &["bruno_casapinello_bot"],
                None
            )),
            (Some("bruno".to_string()), Some(AddressedBy::Mention))
        );
        // 2. name-in-text (no mention).
        assert_eq!(
            agent_of(&elect("tell mira the plan", &[], None)),
            (Some("mira".to_string()), Some(AddressedBy::Name))
        );
        // 3. reply-chain — the replied-to handle resolves without a username too.
        assert_eq!(
            agent_of(&elect("yes that works", &[], Some("otto_casapinello_bot"))),
            (Some("otto".to_string()), Some(AddressedBy::ReplyChain))
        );
        // 4. collective greeting → the whole roster.
        assert!(matches!(elect("hey everyone!", &[], None), Election::All { .. }));
        // 5. unaddressed coordination ask → the concierge (otto). A food/workout
        //    ask instead reaches its domain owner (rung 5' below).
        assert_eq!(
            agent_of(&elect("can someone sort out the logistics?", &[], None)),
            (Some("otto".to_string()), Some(AddressedBy::Concierge))
        );
        // 5'. unaddressed DOMAIN ask → that domain's owner voice (round-2): a
        //     food question reaches the meal owner (nora), not the concierge.
        assert_eq!(
            agent_of(&elect("what's for dinner tonight?", &[], None)),
            (
                Some("nora".to_string()),
                Some(AddressedBy::Domain(crate::notify::ownership::Domain::MealPlanning))
            )
        );
        // 6. pure small talk → silence.
        assert_eq!(
            elect("haha yeah that was fun", &[], None),
            Election::Silence(SilenceReason::SmallTalk)
        );
    }

    // ---- c. reply-chain ---------------------------------------------------

    #[test]
    fn elect_reply_chain_to_replied_bot() {
        assert_one(
            &elect("yes that works", &[], Some("mira_casapinello_bot")),
            "mira",
            AddressedBy::ReplyChain,
        );
    }

    #[test]
    fn elect_name_overrides_reply_chain() {
        // Addressed name beats the reply-chain fallback.
        assert_one(
            &elect("nora, actually can you?", &[], Some("mira_casapinello_bot")),
            "nora",
            AddressedBy::Name,
        );
    }

    // ---- d. collective address -> ALL FOUR in roster order ---------------

    #[test]
    fn elect_collective_hey_guys_is_all() {
        assert_eq!(
            elect("hey guys, how's it going?", &[], None),
            Election::All {
                reply_chat: "-100999".to_string(),
                body: "hey guys, how's it going?".to_string(),
            }
        );
    }

    #[test]
    fn elect_collective_variants_all_fire() {
        for t in [
            "hi everyone!",
            "hello all",
            "team, quick update",
            "ciao a tutti",
            "ciao ragazzi",
            "morning everybody",
        ] {
            assert!(
                matches!(elect(t, &[], None), Election::All { .. }),
                "expected collective for {t:?}"
            );
        }
    }

    // ---- d-plural. mid-sentence second-person-plural address -> ALL ------
    // Regression for the 2026-07-12 live failure: plural addressing buried in a
    // question/request ("can you guys discuss …", "what you all think") was
    // swallowed to otto-concierge instead of electing the whole roster.

    #[test]
    fn elect_plural_you_address_elects_roster_even_mid_sentence() {
        // (message, expected_collective) — the two live misses plus siblings.
        let collective = [
            "can you guys discuss this and find consensus", // live miss #1
            "the meaning of life tell me what you all think", // live miss #2
            "what do you all think?",
            "hey guys", // greeting-shaped collective still fires (via rule d)
            "so what do all of you reckon we should do?",
            "each of you should weigh in on this",
            "the whole team should decide together",
            "can you guys help me plan the weekend?", // ask + plural-you -> roster
        ];
        for t in collective {
            assert!(
                matches!(elect(t, &[], None), Election::All { .. }),
                "expected collective (whole roster) for {t:?}, got {:?}",
                elect(t, &[], None)
            );
        }
        // The live failure was in a SINGLE-human group, where an unaddressed
        // message otherwise falls to the concierge (otto). The plural-address rule
        // must beat that fallback too — the two live misses must elect the roster.
        for t in [
            "can you guys discuss this and find consensus",
            "the meaning of life tell me what you all think",
        ] {
            assert!(
                matches!(elect_solo(t, &[], None), Election::All { .. }),
                "live miss {t:?} must elect the roster even in a single-human group, got {:?}",
                elect_solo(t, &[], None)
            );
        }
    }

    #[test]
    fn elect_third_person_and_gratitude_are_not_plural_collective() {
        // Third-person "the guys" is NOT a second-person address — it stays a
        // single concierge answer, never a four-way broadcast.
        assert!(
            !matches!(elect("tell the guys dinner is ready", &[], None), Election::All { .. }),
            "third-person 'the guys' must not elect the roster"
        );
        assert!(
            !matches!(
                elect_solo("tell the guys dinner is ready", &[], None),
                Election::All { .. }
            ),
            "third-person 'the guys' must not elect the roster (solo)"
        );
        // "tell the guys I said hi" — the task's canonical third-person case —
        // is still the concierge's, not a broadcast.
        assert_one(
            &elect_solo("tell the guys I said hi", &[], None),
            "otto",
            AddressedBy::Concierge,
        );
        // Gratitude that names the family is thank-you small-talk -> silence.
        assert_eq!(
            elect("thanks everyone", &[], None),
            Election::Silence(SilenceReason::SmallTalk),
            "'thanks everyone' is gratitude small-talk, not a collective summons"
        );
        assert_eq!(
            elect("thank you all so much", &[], None),
            Election::Silence(SilenceReason::SmallTalk),
            "'thank you all' is gratitude small-talk, not a collective summons"
        );
    }

    #[test]
    fn is_plural_you_address_unit_cases() {
        // Second-person plural anywhere -> true.
        for t in [
            "can you guys discuss this and find consensus",
            "the meaning of life tell me what you all think",
            "what do you all think?",
            "all of you should come",
            "each of you has a say",
            "the whole family needs to agree",
            "you folks are the best team",
            "hey u guys ready?",
        ] {
            assert!(is_plural_you_address(t), "expected plural-you for {t:?}");
        }
        // Third-person / gratitude / singular -> false.
        for t in [
            "tell the guys dinner is ready",
            "those guys were loud",
            "tell the guys I said hi",
            "thanks everyone",
            "thank you all so much",
            "can you plan dinner?", // singular "you"
            "what's for dinner tonight?",
        ] {
            assert!(!is_plural_you_address(t), "expected NOT plural-you for {t:?}");
        }
    }

    #[test]
    fn is_discussion_ask_unit_cases() {
        // Opinion / discussion asks -> true.
        for t in [
            "can you guys discuss this and find consensus",
            "tell me what you all think",
            "what do you all think?",
            "what do you think about pizza friday?",
            "let's debate the holiday plan",
            "thoughts?",
            "any thoughts on the new schedule",
            "your thoughts on the budget?",
            "can we reach consensus on dinner",
            "let's discuss the weekend",
            "everyone weigh in on this please",
            // typo tolerance (4+ char fuzzy, edit distance ≤1)
            "what are your thoughs",
            "lets discus the plan",
        ] {
            assert!(is_discussion_ask(t), "expected discussion ask for {t:?}");
        }
        // Plain greetings / non-deliberative collective asks -> false.
        for t in [
            "hey guys are you around?",
            "hi everyone!",
            "goodnight all",
            "hey guys ready for dinner?",
            "morning team",
            "you guys up yet?",
            "hey folks what's for dinner tonight?", // a plain ask, not a debate
            "",
        ] {
            assert!(!is_discussion_ask(t), "expected NOT discussion ask for {t:?}");
        }
    }

    /// The classification table that gates a discussion ROUND: it fires ONLY when
    /// the message is elected COLLECTIVE (`Election::All`) AND reads as a
    /// discussion ask. Greetings stay collective-but-not-a-round; concierge asks
    /// stay a single voice; small talk stays silent. Mirrors the live
    /// `Election::All` handler's `is_discussion_ask` branch.
    #[test]
    fn discussion_round_gating_table() {
        // (message, elected collective?, discussion ask?) — a round runs iff both.
        // A discussion ask ALWAYS elects collective (rule d-discussion promotes it
        // over the concierge/team-directed rule), so want_discussion ⇒
        // want_collective.
        let cases: &[(&str, bool, bool)] = &[
            // Discussion asks that ARE collectively elected → ROUND.
            ("can you guys discuss this and find consensus", true, true),
            ("tell me what you all think", true, true),
            ("what do you all think?", true, true),
            // A discussion ask with NO second-person-plural address is promoted to
            // collective too (would otherwise be swallowed to the concierge).
            ("let's discuss the weekend plan and find consensus", true, true),
            ("thoughts on the holiday plan everyone?", true, true),
            // Collective greeting → collective, but NOT a round.
            ("hey guys are you around?", true, false),
            ("hi everyone!", true, false),
            // Plain concierge / single-voice ask → not collective (so no round).
            ("what's for dinner tonight?", false, false),
            // Named address → not collective.
            ("nora, can you help?", false, false),
        ];
        for (msg, want_collective, want_discussion) in cases {
            let is_collective = matches!(elect_solo(msg, &[], None), Election::All { .. });
            assert_eq!(
                is_collective, *want_collective,
                "collective election mismatch for {msg:?}",
            );
            assert_eq!(
                is_discussion_ask(msg),
                *want_discussion,
                "discussion-ask mismatch for {msg:?}",
            );
            // A round runs exactly when both hold.
            let runs_round = is_collective && is_discussion_ask(msg);
            assert_eq!(
                runs_round,
                *want_collective && *want_discussion,
                "round-gating mismatch for {msg:?}",
            );
        }
    }

    #[test]
    fn collective_reply_plans_four_posts_in_roster_order() {
        use crate::graph::WorkGraph;
        use crate::notify::telegram_standup::{DEFAULT_ROSTER, plan_group_reply};
        let cfg = casa_config();
        let posts = plan_group_reply(&WorkGraph::new(), &cfg, DEFAULT_ROSTER);
        let ids: Vec<&str> = posts.iter().map(|p| p.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["nora", "bruno", "mira", "otto"], "roster order");
        // Conversational, not a status report — grounded "all quiet" line.
        assert!(posts.iter().all(|p| !p.text.is_empty()));
    }

    // ---- e. team-directed unaddressed ask -> otto coordinator ------------

    #[test]
    fn elect_unaddressed_someone_ask_to_otto() {
        // A coordination ask (no clear domain) → the concierge (Otto). A food or
        // workout ask instead reaches its domain owner — see
        // `elect_unaddressed_ask_routes_to_domain_owner_voice`.
        assert_one(
            &elect("can someone sort out the logistics?", &[], None),
            "otto",
            AddressedBy::Concierge,
        );
    }

    #[test]
    fn elect_unaddressed_group_request_to_otto() {
        assert_one(
            &elect("we need to sort the grocery shopping this week", &[], None),
            "otto",
            AddressedBy::Concierge,
        );
    }

    #[test]
    fn elect_domain_question_not_second_person_to_domain_owner() {
        // A meal question addressed to no one is a SINGLE-voice answer (not a
        // roster broadcast) — and post round-2 that voice is the meal owner
        // (Nora), not the concierge.
        assert_one(
            &elect("what's the plan for dinner tonight?", &[], None),
            "nora",
            AddressedBy::Domain(crate::notify::ownership::Domain::MealPlanning),
        );
    }

    // ---- f. small talk -> SILENCE ----------------------------------------

    #[test]
    fn elect_small_talk_is_silence() {
        for t in [
            "haha that was so funny",
            "ok see you later",
            "love you all so much", // affectionate, but not a team ask
            "did you have a good day?",
            "you free this weekend?", // 2nd-person human question, domain word present
        ] {
            assert_eq!(
                elect(t, &[], None),
                Election::Silence(SilenceReason::SmallTalk),
                "expected silence for {t:?}"
            );
        }
    }

    #[test]
    fn elect_name_about_a_human_does_not_summon() {
        // "nora from work said hi" talks ABOUT a person named Nora — the bot
        // must not be summoned. Documented cheap-heuristic case.
        assert_eq!(
            elect("nora from work said hi to everyone", &[], None),
            // NB: "everyone" makes this collective — but the point being tested
            // is that the leading "nora" did NOT route to the Nora bot.
            Election::All {
                reply_chat: "-100999".to_string(),
                body: "nora from work said hi to everyone".to_string(),
            }
        );
        // Without the collective word it is plain small talk → silence, and
        // still does not summon Nora.
        assert_eq!(
            elect("nora from work said hi today", &[], None),
            Election::Silence(SilenceReason::SmallTalk),
        );
    }

    #[test]
    fn elect_non_group_is_private_passthrough() {
        let e = elect_responders(
            Some("private"),
            Some("111"),
            "nora, hi",
            &[],
            None,
            false,
            2,
            &casa_config(),
        );
        assert_eq!(e, Election::Private);
    }

    #[test]
    fn elect_group_without_chat_id_is_silence_no_chat() {
        let e =
            elect_responders(Some("group"), None, "hey guys", &[], None, false, 2, &casa_config());
        assert_eq!(e, Election::Silence(SilenceReason::NoChatId));
    }

    #[test]
    fn elect_unaddressed_ask_without_otto_is_silence() {
        // Roster without otto → a team ask has no coordinator to answer.
        let cfg = cfg_with_bots(&[
            ("nora", "-100999", Some("nora"), Some("nora_bot")),
            ("bruno", "-100999", Some("bruno"), Some("bruno_bot")),
        ]);
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            // A coordination ask (owner = Otto, absent here) → silence. A food ask
            // would reach Nora, who is present, so it must NOT be used here.
            "can someone sort out the logistics?",
            &[],
            None,
            false,
            2,
            &cfg,
        );
        assert_eq!(e, Election::Silence(SilenceReason::NoVoicesConfigured));
    }

    // ---- Fix #0: the bot-loop guard --------------------------------------

    #[test]
    fn elect_bot_sender_produces_zero_elections_even_for_collective_text() {
        // The exact storm trigger: a roster reply ("Hey everyone!…") composed and
        // sent by one of our OWN bots, received on a sibling bot's poller. It is
        // collective-shaped text that WOULD elect the whole roster — but because
        // the sender is a bot the guard fires FIRST and nobody is elected.
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "Hey everyone! All quiet on my end",
            &[],
            None,
            true, // sender is a bot
            2,
            &casa_config(),
        );
        assert_eq!(e, Election::Silence(SilenceReason::BotSender));
    }

    #[test]
    fn elect_bot_sender_guard_is_unconditional() {
        // The guard beats EVERY rule — an explicit @mention, an addressed name,
        // and even a private 1:1 — so no path can compose on a bot-sent message.
        assert_eq!(
            elect_responders(
                Some("supergroup"),
                Some("-100999"),
                "nora can you plan dinner?",
                &["bruno_casapinello_bot".to_string()],
                Some("mira_casapinello_bot"),
                true,
                2,
                &casa_config(),
            ),
            Election::Silence(SilenceReason::BotSender)
        );
        assert_eq!(
            elect_responders(
                Some("private"),
                Some("111"),
                "hi",
                &[],
                None,
                true,
                2,
                &casa_config(),
            ),
            Election::Silence(SilenceReason::BotSender)
        );
    }

    #[test]
    fn decision_line_for_bot_sender_silence() {
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "hey everyone",
            &[],
            None,
            true,
            2,
            &casa_config(),
        );
        let line = election_decision_summary(Some("55"), Some("supergroup"), &e);
        assert_eq!(
            line,
            "msg=55 chat=supergroup rule=silence:bot-sender target=silence"
        );
    }

    // ---- Fix #4a: content questions do NOT elect collective --------------

    #[test]
    fn elect_menu_question_routes_to_dietician_not_silence() {
        // Luca's realistic question. It names no one and carries no greeting, so
        // it must be ANSWERED — NOT silenced and NOT fanned out to the whole
        // roster. Post round-2 the meal owner (Nora) answers it, not Otto ("otto
        // replied for food instead of the dietician").
        assert_one(
            &elect("what is on the menu tomorrow?", &[], None),
            "nora",
            AddressedBy::Domain(crate::notify::ownership::Domain::MealPlanning),
        );
    }

    #[test]
    fn elect_named_leading_then_i_routes_to_that_agent() {
        // "otto I am heading to the gym, where is my bag?" — a leading name
        // followed by "I" is a vocative address, so it lands on the named agent
        // (otto), whose session then answers the actual question.
        assert_one(
            &elect("otto I am heading to the gym, where is my bag?", &[], None),
            "otto",
            AddressedBy::Name,
        );
    }

    #[test]
    fn elect_greeting_plus_question_prefers_ask_over_collective() {
        // A collective trigger ("everyone") AND a real ask ("what's for
        // dinner?") → the ask wins: one grounded answer, not a four-way roster
        // broadcast of greetings. Post round-2 the meal owner (Nora) is that
        // single grounded voice.
        assert_one(
            &elect("hey everyone what's for dinner tonight?", &[], None),
            "nora",
            AddressedBy::Domain(crate::notify::ownership::Domain::MealPlanning),
        );
    }

    #[test]
    fn elect_pure_greeting_still_collective() {
        // Regression guard: a greeting with NO ask stays collective (rule d).
        assert!(matches!(
            elect("hey everyone, how's it going?", &[], None),
            Election::All { .. }
        ));
    }

    // ---- fuzzy-summon: typo-tolerant collective detection ----------------

    #[test]
    fn elect_luca_typo_greeting_question_is_collective() {
        // THE LIVE CASE. Luca wrote "hey guyd are you aroind?" — the typos
        // ("guyd","aroind") made exact-phrase matching miss, so the roster
        // wrongly elected silence:small-talk. A human reads this as an
        // unambiguous group summon → the whole roster now answers.
        assert!(
            matches!(elect("hey guyd are you aroind?", &[], None), Election::All { .. }),
            "Luca's typo'd greeting-question must elect the collective, not silence"
        );
    }

    #[test]
    fn elect_greeting_mentioned_midsentence_stays_silent() {
        // Counter-case: "he said hey to me yesterday?" is narration about a
        // greeting, not a summon. "hey" is not the FIRST word and no trigger
        // phrase matches, so the silence preference for non-greeting-shaped
        // chatter holds — bots do NOT get chatty.
        assert_eq!(
            elect("he said hey to me yesterday?", &[], None),
            Election::Silence(SilenceReason::SmallTalk)
        );
    }

    #[test]
    fn elect_more_typo_summons_are_collective() {
        // Fuzzy trigger phrase ("hi guyz") and fuzzy greeting-question openers.
        assert!(matches!(elect("hi guyz!", &[], None), Election::All { .. }));
        assert!(matches!(elect("hey are you all aroind?", &[], None), Election::All { .. }));
        assert!(matches!(elect("helo everyone up yet?", &[], None), Election::All { .. }));
    }

    #[test]
    fn elect_nongreeting_typos_still_silent() {
        // Typo tolerance must not flip ordinary human-to-human chatter. None of
        // these start with a greeting or hit a trigger phrase.
        assert_eq!(
            elect("did you feed the cat?", &[], None),
            Election::Silence(SilenceReason::SmallTalk)
        );
        assert_eq!(
            elect("those guys were so loud last night", &[], None),
            Election::Silence(SilenceReason::SmallTalk)
        );
    }

    // ---- membership-aware silence (single-human vs 2+ humans) ------------

    #[test]
    fn elect_solo_bare_hello_is_brief_collective() {
        // THE LIVE CASE (02:45): Luca posted a bare "hello" in a group that holds
        // one human and four bots. With no human-to-human chatter to protect the
        // greeting is necessarily for the team → a brief whole-roster greeting,
        // NOT silence:small-talk.
        assert_eq!(
            elect_solo("hello", &[], None),
            Election::All {
                reply_chat: "-100999".to_string(),
                body: "hello".to_string(),
            },
            "a bare greeting in a single-human group must warmly greet, not go silent"
        );
    }

    #[test]
    fn elect_two_humans_bare_hello_is_silence() {
        // The moment a second human is present the conservative rule returns:
        // "hello" is human-to-human small talk the bots stay out of.
        assert_eq!(
            elect("hello", &[], None),
            Election::Silence(SilenceReason::SmallTalk),
            "with 2+ humans a bare greeting is protected family chatter → silence"
        );
    }

    #[test]
    fn elect_goodnight_guys_is_collective_in_both_memberships() {
        // "goodnight guys" carries a collective word ("guys") on a greeting, so it
        // is a broadcast in ANY membership — the whole roster answers whether the
        // group has one human or five.
        let expected = Election::All {
            reply_chat: "-100999".to_string(),
            body: "goodnight guys".to_string(),
        };
        assert_eq!(elect_solo("goodnight guys", &[], None), expected, "solo");
        assert_eq!(elect("goodnight guys", &[], None), expected, "two-human");
    }

    #[test]
    fn elect_solo_greeting_variants_are_collective() {
        // Every greeting shape earns a brief roster greeting in a single-human
        // group, including typo'd and time-of-day forms.
        for t in [
            "hi", "hey", "helo", "hey there", "goodnight", "goodmorning",
            "good morning", "good night", "buongiorno",
        ] {
            assert!(
                matches!(elect_solo(t, &[], None), Election::All { .. }),
                "expected brief collective for solo greeting {t:?}"
            );
        }
    }

    #[test]
    fn elect_solo_nongreeting_unaddressed_leans_otto_concierge() {
        // Non-greeting chatter with no named target is NOT silenced in a
        // single-human group — it leans on otto (the concierge) so the human is
        // never left talking to an empty room.
        for t in ["that movie was great", "ok cool", "just got home"] {
            assert_one(&elect_solo(t, &[], None), "otto", AddressedBy::Concierge);
        }
        // And with 2+ humans the very same lines stay silent (family chatter).
        for t in ["that movie was great", "ok cool", "just got home"] {
            assert_eq!(
                elect(t, &[], None),
                Election::Silence(SilenceReason::SmallTalk),
                "2+ humans: {t:?} is protected small talk"
            );
        }
    }

    #[test]
    fn elect_solo_still_honours_named_mention_and_ask_precedence() {
        // Membership only changes the d/e/f *fallback*. Explicit targets and
        // team-directed asks still win exactly as before, even in a solo group.
        assert_one(&elect_solo("nora, hi", &[], None), "nora", AddressedBy::Name);
        assert_one(
            &elect_solo("can someone sort out the logistics?", &[], None),
            "otto",
            AddressedBy::Concierge,
        );
        // A pure collective greeting is still a roster broadcast.
        assert!(matches!(
            elect_solo("hey everyone!", &[], None),
            Election::All { .. }
        ));
    }

    #[test]
    fn elect_solo_unaddressed_without_otto_is_silence_not_crash() {
        // Single-human, non-greeting, but no concierge configured → there is no
        // voice to lean on, so it degrades to silence (NoVoicesConfigured) rather
        // than an empty election.
        let cfg = cfg_with_bots(&[
            ("nora", "-100999", Some("nora"), Some("nora_bot")),
            ("bruno", "-100999", Some("bruno"), Some("bruno_bot")),
        ]);
        let e = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "just got home",
            &[],
            None,
            false,
            1,
            &cfg,
        );
        assert_eq!(e, Election::Silence(SilenceReason::NoVoicesConfigured));
    }

    #[test]
    fn is_greeting_shaped_covers_bare_and_time_of_day_greetings() {
        assert!(is_greeting_shaped("hello"));
        assert!(is_greeting_shaped("hey guys"));
        assert!(is_greeting_shaped("goodnight"));
        assert!(is_greeting_shaped("good morning everyone"));
        assert!(is_greeting_shaped("helo")); // typo-tolerant
        // Not greetings: mid-sentence greeting, or a bare time-of-day word.
        assert!(!is_greeting_shaped("he said hi to me"));
        assert!(!is_greeting_shaped("morning meeting moved to 3?"));
        assert!(!is_greeting_shaped("what's for dinner?"));
    }

    #[test]
    fn is_greeting_collective_needs_greeting_and_broad_address() {
        assert!(is_greeting_collective("goodnight guys"));
        assert!(is_greeting_collective("hey folks"));
        assert!(is_greeting_collective("good morning everyone"));
        assert!(is_greeting_collective("hello all"));
        // A bare time-of-day word is not a recognized greeting on its own, so
        // "morning folks" does not count (matches the tight bare-word policy).
        assert!(!is_greeting_collective("morning folks"));
        // A broad-address word without a greeting is not a greeting-collective.
        assert!(!is_greeting_collective("those guys were loud"));
        // A greeting with no broad-address word is not a greeting-collective
        // (its membership handling happens in elect_responders instead).
        assert!(!is_greeting_collective("hello"));
    }

    #[test]
    fn edit_distance_le_1_boundaries() {
        assert!(edit_distance_le_1("guys", "guys")); // identical
        assert!(edit_distance_le_1("guyd", "guys")); // substitution
        assert!(edit_distance_le_1("aroind", "around")); // substitution
        assert!(edit_distance_le_1("guyz", "guys")); // substitution
        assert!(edit_distance_le_1("helo", "hello")); // deletion
        assert!(edit_distance_le_1("helloo", "hello")); // insertion
        assert!(!edit_distance_le_1("gdyx", "guys")); // two substitutions
        assert!(!edit_distance_le_1("cat", "guys")); // far apart
    }

    #[test]
    fn fuzzy_token_matches_gates_short_triggers() {
        // 4+ char triggers tolerate one typo…
        assert!(fuzzy_token_matches("guyd", "guys"));
        assert!(fuzzy_token_matches("aroind", "around"));
        // …but short triggers demand an exact match so a slip in a common short
        // word can't summon the roster.
        assert!(fuzzy_token_matches("hey", "hey"));
        assert!(!fuzzy_token_matches("he", "hey"));
        assert!(!fuzzy_token_matches("ho", "hi"));
    }

    #[test]
    fn is_greeting_shaped_summon_needs_leading_greeting_and_question() {
        assert!(is_greeting_shaped_summon("hey guyd are you aroind?"));
        assert!(is_greeting_shaped_summon("ciao is dinner ready?"));
        // Missing the question mark → not a summon shape.
        assert!(!is_greeting_shaped_summon("hey everyone"));
        // Greeting not in leading position → narration, not a summon.
        assert!(!is_greeting_shaped_summon("he said hey to me yesterday?"));
        // No greeting at all.
        assert!(!is_greeting_shaped_summon("is the car booked?"));
    }

    #[test]
    fn is_collective_address_is_typo_tolerant_but_conservative() {
        // Fuzzy multi-word trigger + fuzzy greeting-question.
        assert!(is_collective_address("hey guyd are you aroind?"));
        assert!(is_collective_address("hi guyz"));
        // Single-word triggers stay EXACT (regression: "everyones" without the
        // apostrophe is a statement, not an address).
        assert!(!is_collective_address("everyones coming"));
        assert!(!is_collective_address("he said hey to me yesterday?"));
    }

    // ---- helper-level unit checks ----------------------------------------

    #[test]
    fn is_collective_address_whole_word_only() {
        assert!(is_collective_address("hey team"));
        assert!(is_collective_address("everyone ready?"));
        assert!(!is_collective_address("teamwork makes the dream work"));
        assert!(!is_collective_address("everyones coming")); // no apostrophe form here
        assert!(!is_collective_address("just a normal sentence"));
    }

    #[test]
    fn is_team_directed_ask_boundary() {
        assert!(is_team_directed_ask("can someone plan dinner?"));
        assert!(is_team_directed_ask("what's for dinner tonight?"));
        assert!(is_team_directed_ask("let's sort the shopping"));
        // human-to-human, no team signal:
        assert!(!is_team_directed_ask("did you eat yet?"));
        assert!(!is_team_directed_ask("how are you?"));
        assert!(!is_team_directed_ask("that movie was great"));
    }

    #[test]
    fn addressed_name_bot_rejects_name_about_human() {
        let cfg = casa_config();
        assert!(addressed_name_bot("nora from work said hi", &cfg).is_none());
        assert!(addressed_name_bot("i saw bruno at the shop", &cfg).is_none());
        // But real addresses resolve:
        assert_eq!(
            addressed_name_bot("nora, thanks", &cfg).unwrap().agent_id.as_deref(),
            Some("nora")
        );
        assert_eq!(
            addressed_name_bot("hey mira", &cfg).unwrap().agent_id.as_deref(),
            Some("mira")
        );
    }

    // =======================================================================
    // election_decision_summary — one PII-safe log line per election case
    // =======================================================================

    /// Run the real election and format its decision line, as the listener does.
    fn decision(text: &str, mentions: &[&str], reply_to_bot: Option<&str>) -> String {
        let election = elect(text, mentions, reply_to_bot);
        election_decision_summary(Some("42"), Some("supergroup"), &election)
    }

    #[test]
    fn decision_line_for_mention_names_agent_and_rule() {
        // Text names nora, but bruno is @mentioned → mention rule, target bruno.
        let line = decision(
            "nora can you ask @bruno_casapinello_bot about dinner",
            &["bruno_casapinello_bot"],
            None,
        );
        assert_eq!(line, "msg=42 chat=supergroup rule=mention target=bruno");
    }

    #[test]
    fn decision_line_for_name() {
        let line = decision("nora, what's for dinner?", &[], None);
        assert_eq!(line, "msg=42 chat=supergroup rule=name target=nora");
    }

    #[test]
    fn decision_line_for_reply_chain() {
        let line = decision("yes that works", &[], Some("mira_casapinello_bot"));
        assert_eq!(line, "msg=42 chat=supergroup rule=reply target=mira");
    }

    #[test]
    fn decision_line_for_otto_concierge() {
        // Unaddressed coordination ask with otto present → otto coordinates.
        let line = decision("can someone sort out the logistics?", &[], None);
        assert_eq!(line, "msg=42 chat=supergroup rule=otto-concierge target=otto");
    }

    #[test]
    fn decision_line_for_collective() {
        let line = decision("hey guys, how's it going?", &[], None);
        assert_eq!(line, "msg=42 chat=supergroup rule=collective target=roster");
    }

    #[test]
    fn decision_line_for_silence_small_talk() {
        // Human-to-human small talk → silence, and it STILL logs a line (this is
        // the observability gap this task closes).
        let line = decision("did you eat yet?", &[], None);
        assert_eq!(
            line,
            "msg=42 chat=supergroup rule=silence:small-talk target=silence"
        );
    }

    #[test]
    fn decision_line_for_silence_no_chat_id() {
        let election =
            elect_responders(Some("group"), None, "hey guys", &[], None, false, 2, &casa_config());
        let line = election_decision_summary(None, Some("group"), &election);
        // No transport message id → "none"; reason surfaced in the rule.
        assert_eq!(
            line,
            "msg=none chat=group rule=silence:no-chat-id target=silence"
        );
    }

    #[test]
    fn decision_line_for_silence_no_voices_configured() {
        let cfg = cfg_with_bots(&[
            ("nora", "-100999", Some("nora"), Some("nora_bot")),
            ("bruno", "-100999", Some("bruno"), Some("bruno_bot")),
        ]);
        let election = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            // A coordination ask whose owner (Otto) is absent → no coordinator, so
            // silence. (A food ask here would reach Nora, who IS configured.)
            "can someone sort out the logistics?",
            &[],
            None,
            false,
            2,
            &cfg,
        );
        let line = election_decision_summary(Some("7"), Some("supergroup"), &election);
        assert_eq!(
            line,
            "msg=7 chat=supergroup rule=silence:no-voices-configured target=silence"
        );
    }

    #[test]
    fn decision_line_for_private_passthrough() {
        let election = elect_responders(
            Some("private"),
            Some("111"),
            "nora, hi",
            &[],
            None,
            false,
            2,
            &casa_config(),
        );
        let line = election_decision_summary(Some("9"), Some("private"), &election);
        assert_eq!(line, "msg=9 chat=private rule=private target=passthrough");
    }

    #[test]
    fn decision_line_targets_bot_id_when_agent_unbound() {
        // A bot fronting no agent falls back to "<bot_id>(unbound)" — the target
        // is never blank, so the log always names a landing point.
        let cfg = cfg_with_bots(&[("otto", "-100999", None, Some("otto_bot"))]);
        let election = elect_responders(
            Some("supergroup"),
            Some("-100999"),
            "can someone help?",
            &[],
            None,
            false,
            2,
            &cfg,
        );
        let line = election_decision_summary(Some("3"), Some("supergroup"), &election);
        assert_eq!(
            line,
            "msg=3 chat=supergroup rule=otto-concierge target=otto(unbound)"
        );
    }

    #[test]
    fn decision_line_never_contains_message_text() {
        // The summary takes only the message id + chat type + election — never
        // the body — so no PII/tokens can leak into the log line.
        let secret = "my password is hunter2 and my token is 987:ABC";
        let line = decision(secret, &[], None);
        assert!(
            !line.contains("hunter2") && !line.contains("987:ABC"),
            "decision line must not echo message text, got {line:?}"
        );
    }
}
