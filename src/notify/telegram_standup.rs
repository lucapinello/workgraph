//! `/standup` orchestration for a configured household group (Telegram).
//!
//! Typing `/standup` once in the family group must produce **exactly one post
//! per named voice, in the ordered roster authored in `household.toml`, each in
//! family voice (docs/04), 1–3 sentences, grounded in that persona's *live*
//! graph state (its open / in-progress tasks). See docs/09 §3.
//!
//! ## Why the listener orchestrates (design decision)
//!
//! Telegram delivers a `/command` to **every** bot in a group even under
//! privacy mode, so a naive "each bot answers when it sees `/standup`" design
//! is possible — but it guarantees neither **order** (multiple bots replying
//! concurrently race) nor **no-duplicates** (nothing stops two from posting).
//! Instead the single `wg telegram listen` process — which already owns the
//! long-poll for the whole multi-bot config — acts as the sole orchestrator:
//! on `/standup` it walks the roster *in order* and posts each persona's report
//! **as that bot** (using that bot's own token) to the group. One orchestrator,
//! sequential sends ⇒ exactly N posts in roster order, no duplicates, no race.
//!
//! This module is deliberately split into a **pure planning core**
//! ([`plan_roster`], [`render_report`], [`plan_standup`]) that takes a
//! [`TelegramConfig`] plus graph state and returns the ordered list of posts to
//! make — no network, no tokens in the output text — and a thin async driver
//! ([`run_standup`], in `commands::telegram`) that actually sends them. The
//! pure core is what the scripted test asserts against ("`/standup` → exactly
//! one post per configured roster member, in authored order"). Bot tokens live
//! only on the [`StandupMember`] for the send path and are NEVER placed in a
//! [`StandupPost`], a log, or the graph.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::telegram::{TelegramBotConfig, TelegramConfig};
use crate::graph::{Status, WorkGraph};

/// One public persona record from the ordered project-local `[[agent]]` list.
///
/// Tokens never belong here. `id` joins this committable presentation record to
/// the secret-bearing `[telegram.bots.<id>]` table in notify configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseholdPersona {
    pub id: String,
    pub display_name: String,
    pub emoji: String,
}

/// Load the ordered household roster from `<project_root>/household.toml`.
///
/// The whole file fails closed for roster purposes when it is missing,
/// malformed, has no agents, has a duplicate id, or any agent lacks the three
/// identity fields used by Telegram. Callers must not recover by iterating the
/// configured-bot `HashMap`: that would make speaking order process-dependent.
pub fn load_household_personas(project_root: &Path) -> Result<Vec<HouseholdPersona>> {
    let path = project_root.join("household.toml");
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read household roster {}", path.display()))?;
    let value: toml::Value = body
        .parse()
        .with_context(|| format!("invalid household roster {}", path.display()))?;
    let agents = value
        .get("agent")
        .and_then(toml::Value::as_array)
        .context("household.toml must contain at least one [[agent]]")?;
    if agents.is_empty() {
        bail!("household.toml must contain at least one [[agent]]");
    }

    let mut seen = HashSet::new();
    let mut personas = Vec::with_capacity(agents.len());
    for (index, agent) in agents.iter().enumerate() {
        let field = |name: &str| -> Result<String> {
            let value = agent
                .get(name)
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .with_context(|| format!("household.toml [[agent]] #{index} needs `{name}`"))?;
            Ok(value.to_string())
        };
        let id = field("id")?;
        if !seen.insert(id.to_lowercase()) {
            bail!("household.toml has duplicate agent id `{id}`");
        }
        personas.push(HouseholdPersona {
            id,
            display_name: field("name")?,
            emoji: field("emoji")?,
        });
    }
    Ok(personas)
}

/// True when `text` is the `/standup` command. Accepts the bare command, the
/// Telegram group-suffixed form (`/standup@otto_casapinello_bot`), a leading
/// `wg` prefix, and any trailing arguments — matching how Telegram delivers
/// `/commands` in groups.
pub fn is_standup_command(text: &str) -> bool {
    let t = text.trim();
    // Allow an optional "wg " prefix (mirrors telegram_commands::parse). With
    // the `wg` prefix the bare word `standup` is accepted; otherwise the
    // leading slash is required so ordinary chatter ("standup comedy") that
    // reaches a bot via @mention is never mistaken for the command.
    let (t, prefixed) = match t.strip_prefix("wg ") {
        Some(rest) => (rest.trim_start(), true),
        None => (t, false),
    };
    let first = t.split_whitespace().next().unwrap_or("");
    // Strip the `@bot` suffix Telegram appends in groups.
    let cmd = first.split('@').next().unwrap_or(first);
    cmd.eq_ignore_ascii_case("/standup") || (prefixed && cmd.eq_ignore_ascii_case("standup"))
}

/// One member of the standup roster: the persona/bot, its presentation, and the
/// graph agent whose live tasks ground the report. Carries the bot token (for
/// the send path only) — never serialize or log this whole struct.
#[derive(Debug, Clone)]
pub struct StandupMember {
    /// Stable persona id from the matching `household.toml` entry.
    pub persona_id: String,
    /// The `[telegram.bots.<id>]` key — the persona id, e.g. `"nora"`.
    pub bot_id: String,
    /// Per-bot config (token + chat id + optional agent binding).
    pub bot: TelegramBotConfig,
    /// Human display name for the post header, e.g. `"Nora"`, `"Coach Mira"`.
    pub display_name: String,
    /// A single leading emoji for the post header, e.g. `"🥗"`.
    pub emoji: String,
}

impl StandupMember {
    /// The graph agent id whose tasks ground this persona's report. Uses the
    /// bot's explicit `agent_id` binding when set, otherwise falls back to the
    /// project-local household persona id.
    pub fn agent_id(&self) -> &str {
        self.bot.agent_id.as_deref().unwrap_or(&self.persona_id)
    }

    /// The routing discriminator (`telegram:<bot_id>`, or bare `telegram` for
    /// the legacy single bot).
    pub fn channel_type(&self) -> String {
        if self.bot_id == "default" {
            "telegram".to_string()
        } else {
            format!("telegram:{}", self.bot_id)
        }
    }
}

/// One rendered standup post: what to send, and which bot sends it. Contains no
/// secret — safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandupPost {
    /// The persona/bot id that posts this (`"nora"`).
    pub bot_id: String,
    /// The routing discriminator (`"telegram:nora"`), for logging/debugging.
    pub channel_type: String,
    /// The family-voice message body, ready to `send_text` verbatim (plain
    /// text — no Markdown escaping needed by the Telegram `send_text` path).
    pub text: String,
}

/// Join an already-parsed ordered household roster to configured Telegram bots.
///
/// Only the intersection is eligible: an extra configured bot is not silently
/// promoted to a household voice, and an agent without a bot is skipped. The
/// household file supplies order, display name, and emoji; bot-map iteration
/// order supplies none of them.
///
/// A legacy top-level bot is supported only when the household roster has
/// exactly one persona. With multiple personas it is ambiguous which identity
/// that one bot represents, so the join fails closed.
pub fn plan_roster(
    config: &TelegramConfig,
    personas: &[HouseholdPersona],
) -> Result<Vec<StandupMember>> {
    if config.bots.is_empty() {
        if config.bot_token.is_empty() || config.chat_id.is_empty() {
            return Ok(Vec::new());
        }
        let [persona] = personas else {
            bail!(
                "one legacy Telegram bot cannot represent {} household personas",
                personas.len()
            );
        };
        return Ok(vec![StandupMember {
            persona_id: persona.id.clone(),
            bot_id: "default".to_string(),
            bot: TelegramBotConfig {
                bot_token: config.bot_token.clone(),
                chat_id: config.chat_id.clone(),
                agent_id: Some(persona.id.clone()),
                username: None,
            },
            display_name: persona.display_name.clone(),
            emoji: persona.emoji.clone(),
        }]);
    }

    let mut normalized_bot_ids = HashSet::new();
    for bot_id in config.bots.keys() {
        if !normalized_bot_ids.insert(bot_id.to_lowercase()) {
            bail!("Telegram bot ids differ only by case: `{bot_id}`");
        }
    }

    let mut out = Vec::new();
    for persona in personas {
        let Some((bot_id, bot)) = config
            .bots
            .iter()
            .find(|(bot_id, _)| bot_id.eq_ignore_ascii_case(&persona.id))
        else {
            continue;
        };
        out.push(StandupMember {
            persona_id: persona.id.clone(),
            bot_id: bot_id.clone(),
            bot: bot.clone(),
            display_name: persona.display_name.clone(),
            emoji: persona.emoji.clone(),
        });
    }
    Ok(out)
}

/// Load and join the project-local household roster in one fail-closed step.
pub fn load_project_roster(
    project_root: &Path,
    config: &TelegramConfig,
) -> Result<Vec<StandupMember>> {
    let personas = load_household_personas(project_root)?;
    plan_roster(config, &personas)
}

/// Render one persona's family-voice report from its live task lists.
///
/// `in_progress` / `open` are the titles of that persona's in-progress and
/// open tasks (already humanized-enough titles — see [`humanize_title`]). The
/// body is 1–2 sentences, natural language, no task ids / week codes / tool
/// verbs (docs/04 read-aloud test). An empty plate yields an honest
/// "all caught up" line rather than silence, so the roster always has N posts.
pub fn render_report(
    member: &StandupMember,
    in_progress: &[String],
    open: &[String],
) -> StandupPost {
    let name = &member.display_name;
    let body = if !in_progress.is_empty() {
        let first = humanize_title(&in_progress[0]);
        let mut s = format!("I'm right in the middle of {first}.");
        let others = in_progress.len() - 1;
        if others > 0 {
            s.push_str(&format!(
                " I've also got {} other {} on the go.",
                others,
                thing_word(others)
            ));
        }
        if !open.is_empty() {
            s.push_str(&format!(
                " {} more lined up after that — nothing needs you yet.",
                open.len()
            ));
        }
        s
    } else if !open.is_empty() {
        let first = humanize_title(&open[0]);
        let extra = open.len() - 1;
        if extra > 0 {
            format!(
                "I've got {} {} queued up, starting with {first}. Nothing needs you from me yet.",
                open.len(),
                thing_word(open.len())
            )
        } else {
            format!("I've got {first} queued up next. Nothing needs you from me yet.")
        }
    } else {
        "All caught up on my end — nothing needs your attention right now.".to_string()
    };

    let text = format!("{} {}\n{}", name, member.emoji, body);
    StandupPost {
        bot_id: member.bot_id.clone(),
        channel_type: member.channel_type(),
        text,
    }
}

/// `"thing"` / `"things"` for a count.
fn thing_word(n: usize) -> &'static str {
    if n == 1 { "thing" } else { "things" }
}

/// Lightly naturalize a task title for family voice: drop leading week codes
/// (`W29`, `2026-W29`) and task-id-ish prefixes, strip trailing punctuation,
/// and lowercase the first letter for mid-sentence embedding. This is a best
/// effort — the read-aloud rule (docs/04) is ultimately a human's job, but this
/// removes the most common jargon leaks (week codes, ids) automatically.
pub fn humanize_title(title: &str) -> String {
    let cleaned = title
        .split_whitespace()
        .filter(|tok| !is_week_code(tok) && !is_task_id_ish(tok))
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned = cleaned.trim().trim_end_matches(['.', '!', ':', ';']);
    let cleaned = if cleaned.is_empty() {
        title.trim()
    } else {
        cleaned
    };
    lower_first(cleaned)
}

/// True for `W29` / `2026-W29` style week codes.
fn is_week_code(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| !c.is_alphanumeric());
    // "W29"
    if let Some(rest) = t.strip_prefix(['W', 'w']) {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    // "2026-W29"
    if let Some((year, wk)) = t.split_once('-') {
        if year.len() == 4
            && year.chars().all(|c| c.is_ascii_digit())
            && (wk.starts_with('W') || wk.starts_with('w'))
        {
            return true;
        }
    }
    false
}

/// True for tokens that look like bare task ids (`r17-group-mention`,
/// `[abc-123]`) — kebab tokens with a digit and no spaces are almost always
/// ids, not prose, so they read badly aloud.
fn is_task_id_ish(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| c == '[' || c == ']' || c == '(' || c == ')');
    let has_dash = t.contains('-');
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    let all_id_chars = !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    has_dash && has_digit && all_id_chars
}

/// Lowercase just the first character (for mid-sentence embedding), leaving the
/// rest untouched so acronyms and names keep their case.
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Collect a persona's live task titles from the graph, split into
/// (in_progress, open). Only tasks whose `assigned` equals `agent_id` count.
pub fn agent_task_lines(graph: &WorkGraph, agent_id: &str) -> (Vec<String>, Vec<String>) {
    let mut in_progress = Vec::new();
    let mut open = Vec::new();
    for task in graph.tasks() {
        if task.assigned.as_deref() != Some(agent_id) {
            continue;
        }
        match task.status {
            Status::InProgress => in_progress.push(task.title.clone()),
            Status::Open => open.push(task.title.clone()),
            _ => {}
        }
    }
    (in_progress, open)
}

/// Plan the full standup: the ordered list of posts to make, one per roster
/// member, each grounded in that member's live graph state. This is the pure
/// function the scripted test asserts against.
pub fn plan_standup(graph: &WorkGraph, roster: &[StandupMember]) -> Vec<StandupPost> {
    roster
        .iter()
        .map(|member| {
            let (in_progress, open) = agent_task_lines(graph, member.agent_id());
            render_report(member, &in_progress, &open)
        })
        .collect()
}

/// Render one persona's **conversational** reply to a collective greeting.
///
/// This is the collective-address (rule d) sibling of [`render_report`]: same
/// grounding in the persona's live tasks, but phrased as *answering a greeting*
/// rather than *filing a status report* — shorter, warmer, in-voice. Reused by
/// the all-bots-off election so "hey guys" gets a brief hello from each voice,
/// grounded in what they're actually doing, not a four-way standup dump.
pub fn render_conversational(
    member: &StandupMember,
    in_progress: &[String],
    open: &[String],
) -> StandupPost {
    let body = if !in_progress.is_empty() {
        let first = humanize_title(&in_progress[0]);
        format!("Hey! I'm on {first} right now — shout if you need me.")
    } else if !open.is_empty() {
        let first = humanize_title(&open[0]);
        format!("Hi! Nothing urgent on my side — {first} is next up.")
    } else {
        "Hi! All quiet on my end — here if you need anything. \u{1f44b}".to_string()
    };

    let text = format!("{} {}\n{}", member.display_name, member.emoji, body);
    StandupPost {
        bot_id: member.bot_id.clone(),
        channel_type: member.channel_type(),
        text,
    }
}

/// Plan a whole-roster **conversational** reply (rule d): one brief in-voice
/// hello per named voice, in roster order, each grounded in that voice's live
/// graph state. The collective-address analogue of [`plan_standup`]; the pure
/// function the collective-address test asserts against.
pub fn plan_group_reply(graph: &WorkGraph, roster: &[StandupMember]) -> Vec<StandupPost> {
    roster
        .iter()
        .map(|member| {
            let (in_progress, open) = agent_task_lines(graph, member.agent_id());
            render_conversational(member, &in_progress, &open)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn bot(chat: &str, agent: Option<&str>) -> TelegramBotConfig {
        TelegramBotConfig {
            bot_token: "TESTTOKEN".to_string(),
            chat_id: chat.to_string(),
            agent_id: agent.map(|s| s.to_string()),
            username: None,
        }
    }

    fn bot_tok(chat: &str, agent: Option<&str>, token: &str) -> TelegramBotConfig {
        TelegramBotConfig {
            bot_token: token.to_string(),
            chat_id: chat.to_string(),
            agent_id: agent.map(|s| s.to_string()),
            username: None,
        }
    }

    fn fixture_personas() -> Vec<HouseholdPersona> {
        vec![
            HouseholdPersona {
                id: "voice-zeta".to_string(),
                display_name: "North Star".to_string(),
                emoji: "🌙".to_string(),
            },
            HouseholdPersona {
                id: "voice-alpha".to_string(),
                display_name: "Green Lantern".to_string(),
                emoji: "🌿".to_string(),
            },
            HouseholdPersona {
                id: "voice-kappa".to_string(),
                display_name: "Quiet Harbor".to_string(),
                emoji: "🧭".to_string(),
            },
        ]
    }

    fn fixture_config() -> TelegramConfig {
        let mut bots = HashMap::new();
        // Insert deliberately OUT of roster order to prove ordering is imposed
        // by plan_roster, not by HashMap iteration.
        bots.insert("voice-kappa".to_string(), bot("-100", None));
        bots.insert("voice-alpha".to_string(), bot("-100", None));
        bots.insert("voice-zeta".to_string(), bot("-100", None));
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        }
    }

    fn fixture_roster() -> Vec<StandupMember> {
        plan_roster(&fixture_config(), &fixture_personas()).unwrap()
    }

    #[test]
    fn roster_uses_household_order_regardless_of_bot_map_order() {
        let roster = fixture_roster();
        let ids: Vec<&str> = roster.iter().map(|m| m.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["voice-zeta", "voice-alpha", "voice-kappa"]);
        assert_eq!(roster[0].display_name, "North Star");
        assert_eq!(roster[1].emoji, "🌿");
    }

    // Fix #3 regression: each roster member must carry its OWN bot token and
    // channel_type. This locks the 1:1 pairing at the planning layer, where
    // every send/log loop reads member.bot / member.bot_id.
    #[test]
    fn each_roster_member_carries_its_own_token_and_channel() {
        let mut bots = HashMap::new();
        for persona in fixture_personas() {
            bots.insert(
                persona.id.clone(),
                bot_tok(
                    "-100",
                    Some(&persona.id),
                    &format!("TOK-{}", persona.id.to_ascii_uppercase()),
                ),
            );
        }
        let cfg = TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        };
        let roster = plan_roster(&cfg, &fixture_personas()).unwrap();
        for member in &roster {
            let want = format!("TOK-{}", member.bot_id.to_ascii_uppercase());
            assert_eq!(
                member.bot.bot_token, want,
                "{} must carry ITS OWN token, not another voice's",
                member.bot_id
            );
            assert_eq!(member.channel_type(), format!("telegram:{}", member.bot_id));
        }
    }

    #[test]
    fn standup_produces_one_post_per_joined_household_persona() {
        let roster = fixture_roster();
        let graph = WorkGraph::new();
        let posts = plan_standup(&graph, &roster);
        assert_eq!(posts.len(), 3, "one post per configured roster persona");
        let ids: Vec<&str> = posts.iter().map(|p| p.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["voice-zeta", "voice-alpha", "voice-kappa"]);
        // Each post is a distinct bot — no duplicates.
        let channels: Vec<&str> = posts.iter().map(|p| p.channel_type.as_str()).collect();
        assert_eq!(
            channels,
            vec![
                "telegram:voice-zeta",
                "telegram:voice-alpha",
                "telegram:voice-kappa"
            ]
        );
    }

    #[test]
    fn every_post_has_a_nonempty_family_voice_body() {
        let graph = WorkGraph::new();
        let posts = plan_standup(&graph, &fixture_roster());
        for p in &posts {
            // Header line + body.
            let (header, body) = p.text.split_once('\n').expect("header + body");
            assert!(!header.is_empty());
            assert!(
                !body.trim().is_empty(),
                "body must not be empty: {}",
                p.text
            );
            // No jargon leaks in an empty-plate standup.
            assert!(!p.text.contains("W29"));
            assert!(!p.text.to_lowercase().contains("task"));
        }
    }

    #[test]
    fn display_names_and_emoji_come_from_household_roster() {
        let posts = plan_standup(&WorkGraph::new(), &fixture_roster());
        assert!(posts[0].text.starts_with("North Star 🌙"));
        assert!(posts[1].text.starts_with("Green Lantern 🌿"));
        assert!(posts[2].text.starts_with("Quiet Harbor 🧭"));
    }

    #[test]
    fn configured_bot_outside_household_is_not_promoted_to_voice() {
        let mut cfg = fixture_config();
        cfg.bots
            .insert("unlisted-service".to_string(), bot("-100", None));
        let roster = plan_roster(&cfg, &fixture_personas()).unwrap();
        let ids: Vec<&str> = roster.iter().map(|m| m.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["voice-zeta", "voice-alpha", "voice-kappa"]);
    }

    #[test]
    fn single_legacy_bot_is_unambiguous_only_for_one_persona() {
        let cfg = TelegramConfig {
            bot_token: "LEGACY".to_string(),
            chat_id: "-100".to_string(),
            bots: HashMap::new(),
        };
        let one = vec![fixture_personas().remove(0)];
        let roster = plan_roster(&cfg, &one).unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].bot_id, "default");
        assert_eq!(roster[0].persona_id, "voice-zeta");
        assert_eq!(roster[0].channel_type(), "telegram");
        assert!(plan_roster(&cfg, &fixture_personas()).is_err());
    }

    #[test]
    fn household_loader_preserves_opaque_ids_names_emoji_and_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("household.toml"),
            r#"
[[agent]]
id = "agent-7f3"
name = "Morning Compass"
emoji = "🧭"

[[agent]]
id = "agent-a91"
name = "Garden Lamp"
emoji = "🏮"
"#,
        )
        .unwrap();
        let personas = load_household_personas(root.path()).unwrap();
        assert_eq!(
            personas,
            vec![
                HouseholdPersona {
                    id: "agent-7f3".to_string(),
                    display_name: "Morning Compass".to_string(),
                    emoji: "🧭".to_string(),
                },
                HouseholdPersona {
                    id: "agent-a91".to_string(),
                    display_name: "Garden Lamp".to_string(),
                    emoji: "🏮".to_string(),
                },
            ]
        );
    }

    #[test]
    fn missing_malformed_or_duplicate_household_roster_fails_closed() {
        let missing = tempfile::tempdir().unwrap();
        assert!(load_household_personas(missing.path()).is_err());

        let malformed = tempfile::tempdir().unwrap();
        std::fs::write(malformed.path().join("household.toml"), "not = [valid").unwrap();
        assert!(load_household_personas(malformed.path()).is_err());

        let incomplete = tempfile::tempdir().unwrap();
        std::fs::write(
            incomplete.path().join("household.toml"),
            r#"
[[agent]]
id = "agent-x"
name = "Missing Emoji"
"#,
        )
        .unwrap();
        assert!(load_household_personas(incomplete.path()).is_err());

        let duplicate = tempfile::tempdir().unwrap();
        std::fs::write(
            duplicate.path().join("household.toml"),
            r#"
[[agent]]
id = "agent-x"
name = "First"
emoji = "1"

[[agent]]
id = "AGENT-X"
name = "Second"
emoji = "2"
"#,
        )
        .unwrap();
        assert!(load_household_personas(duplicate.path()).is_err());
    }

    #[test]
    fn report_reflects_in_progress_and_open_tasks() {
        let member = StandupMember {
            persona_id: "voice-zeta".to_string(),
            bot_id: "nora".to_string(),
            bot: bot("-100", None),
            display_name: "Nora".to_string(),
            emoji: "🥗".to_string(),
        };
        let post = render_report(
            &member,
            &["Draft the dinner plan".to_string()],
            &["Order groceries".to_string()],
        );
        assert!(post.text.contains("dinner plan"));
        assert!(post.text.contains("1 more lined up"));
    }

    #[test]
    fn empty_plate_reports_all_caught_up() {
        let member = StandupMember {
            persona_id: "voice-kappa".to_string(),
            bot_id: "otto".to_string(),
            bot: bot("-100", None),
            display_name: "Otto".to_string(),
            emoji: "📋".to_string(),
        };
        let post = render_report(&member, &[], &[]);
        assert!(post.text.to_lowercase().contains("caught up"));
    }

    #[test]
    fn humanize_title_strips_week_codes_and_ids() {
        assert_eq!(humanize_title("W29 meal plan"), "meal plan");
        assert_eq!(humanize_title("2026-W29 shopping list"), "shopping list");
        assert_eq!(
            humanize_title("Finish r17-group-mention routing"),
            "finish routing"
        );
        // Pure prose is only lowercased at the front.
        assert_eq!(humanize_title("Plan the week"), "plan the week");
    }

    #[test]
    fn recognizes_standup_command_forms() {
        assert!(is_standup_command("/standup"));
        assert!(is_standup_command("  /standup  "));
        assert!(is_standup_command("/standup@otto_casapinello_bot"));
        assert!(is_standup_command("/standup please"));
        assert!(is_standup_command("wg standup"));
        assert!(is_standup_command("wg /standup"));
        // Not the command: bare word chatter, other commands, empty.
        assert!(!is_standup_command("standup"));
        assert!(!is_standup_command("standup comedy tonight?"));
        assert!(!is_standup_command("/status"));
        assert!(!is_standup_command(""));
    }

    #[test]
    fn agent_binding_overrides_persona_id_for_grounding() {
        let member = StandupMember {
            persona_id: "voice-zeta".to_string(),
            bot_id: "nora".to_string(),
            bot: bot("-100", Some("agent-42")),
            display_name: "Nora".to_string(),
            emoji: "🥗".to_string(),
        };
        assert_eq!(member.agent_id(), "agent-42");
    }
}
