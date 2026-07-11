//! `/standup` orchestration for the Casa Pinello family group (Telegram).
//!
//! Typing `/standup` once in the family group must produce **exactly one post
//! per named voice, in a fixed roster order** (`nora, bruno, mira, otto`), each
//! in family voice (docs/04), 1–3 sentences, grounded in that persona's *live*
//! graph state (its open / in-progress tasks). See docs/09 §3.
//!
//! ## Why the listener orchestrates (design decision)
//!
//! Telegram delivers a `/command` to **every** bot in a group even under
//! privacy mode, so a naive "each bot answers when it sees `/standup`" design
//! is possible — but it guarantees neither **order** (four bots replying
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
//! four posts in roster order"). Bot tokens live only on the [`StandupMember`]
//! for the send path and are NEVER placed in a [`StandupPost`], a log, or the
//! graph.

use super::telegram::{TelegramBotConfig, TelegramConfig};
use crate::graph::{Status, WorkGraph};

/// Canonical family roster order. `/standup` posts in exactly this order; any
/// configured bot not named here is appended after these, in sorted order, so
/// no configured voice is silently dropped.
pub const DEFAULT_ROSTER: &[&str] = &["nora", "bruno", "mira", "otto"];

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
    /// persona id itself (so a task `assigned` to `"nora"` grounds Nora's
    /// report even without an explicit binding).
    pub fn agent_id(&self) -> &str {
        self.bot.agent_id.as_deref().unwrap_or(&self.bot_id)
    }

    /// The routing discriminator for this bot (`"telegram:<bot_id>"`).
    pub fn channel_type(&self) -> String {
        format!("telegram:{}", self.bot_id)
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

/// Presentation (display name + emoji) for a known persona. Falls back to a
/// title-cased id and a neutral emoji for any unknown bot so a mis-named or
/// newly-added bot still reports rather than panicking.
fn persona_presentation(bot_id: &str) -> (String, String) {
    match bot_id.to_ascii_lowercase().as_str() {
        "nora" => ("Nora".to_string(), "🥗".to_string()),
        "bruno" => ("Bruno".to_string(), "👨\u{200d}🍳".to_string()),
        "mira" => ("Coach Mira".to_string(), "💪".to_string()),
        "otto" => ("Otto".to_string(), "📋".to_string()),
        _ => (title_case(bot_id), "💬".to_string()),
    }
}

/// Title-case an id (`"jane"` → `"Jane"`), ASCII-only; leaves non-ASCII intact.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Build the ordered roster from the configured bots.
///
/// The order is [`DEFAULT_ROSTER`] first (only the entries that are actually
/// configured), then any remaining configured bots in sorted order so nothing
/// is dropped. De-duplicated: each bot appears at most once. The legacy
/// `"default"` single-bot is excluded — the standup is a *named-voice* feature.
pub fn plan_roster(config: &TelegramConfig, order: &[&str]) -> Vec<StandupMember> {
    let bots = config.all_bots();
    let mut out: Vec<StandupMember> = Vec::new();
    let mut used: Vec<String> = Vec::new();

    let mut push = |bot_id: &str, bot: &TelegramBotConfig, out: &mut Vec<StandupMember>| {
        let (display_name, emoji) = persona_presentation(bot_id);
        out.push(StandupMember {
            bot_id: bot_id.to_string(),
            bot: bot.clone(),
            display_name,
            emoji,
        });
    };

    // Canonical roster order first.
    for want in order {
        if used.iter().any(|u| u.eq_ignore_ascii_case(want)) {
            continue;
        }
        if let Some((id, bot)) = bots
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(want) && id != "default")
        {
            push(id, bot, &mut out);
            used.push(id.clone());
        }
    }

    // Any remaining named bots, sorted, so no configured voice is dropped.
    let mut remaining: Vec<&(String, TelegramBotConfig)> = bots
        .iter()
        .filter(|(id, _)| id != "default" && !used.iter().any(|u| u.eq_ignore_ascii_case(id)))
        .collect();
    remaining.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, bot) in remaining {
        push(id, bot, &mut out);
        used.push(id.clone());
    }

    out
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
pub fn plan_standup(
    graph: &WorkGraph,
    config: &TelegramConfig,
    order: &[&str],
) -> Vec<StandupPost> {
    plan_roster(config, order)
        .into_iter()
        .map(|member| {
            let (in_progress, open) = agent_task_lines(graph, member.agent_id());
            render_report(&member, &in_progress, &open)
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

    fn casa_config() -> TelegramConfig {
        let mut bots = HashMap::new();
        // Insert deliberately OUT of roster order to prove ordering is imposed
        // by plan_roster, not by HashMap iteration.
        bots.insert("otto".to_string(), bot("-100", None));
        bots.insert("nora".to_string(), bot("-100", None));
        bots.insert("mira".to_string(), bot("-100", None));
        bots.insert("bruno".to_string(), bot("-100", None));
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        }
    }

    #[test]
    fn roster_is_in_canonical_order_regardless_of_config_order() {
        let cfg = casa_config();
        let roster = plan_roster(&cfg, DEFAULT_ROSTER);
        let ids: Vec<&str> = roster.iter().map(|m| m.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["nora", "bruno", "mira", "otto"]);
    }

    #[test]
    fn standup_produces_exactly_four_posts_in_roster_order() {
        let cfg = casa_config();
        let graph = WorkGraph::new();
        let posts = plan_standup(&graph, &cfg, DEFAULT_ROSTER);
        assert_eq!(posts.len(), 4, "one post per configured named bot");
        let ids: Vec<&str> = posts.iter().map(|p| p.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["nora", "bruno", "mira", "otto"]);
        // Each post is a distinct bot — no duplicates.
        let channels: Vec<&str> = posts.iter().map(|p| p.channel_type.as_str()).collect();
        assert_eq!(
            channels,
            vec![
                "telegram:nora",
                "telegram:bruno",
                "telegram:mira",
                "telegram:otto"
            ]
        );
    }

    #[test]
    fn every_post_has_a_nonempty_family_voice_body() {
        let cfg = casa_config();
        let graph = WorkGraph::new();
        let posts = plan_standup(&graph, &cfg, DEFAULT_ROSTER);
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
    fn display_names_and_emoji_match_personas() {
        let cfg = casa_config();
        let posts = plan_standup(&WorkGraph::new(), &cfg, DEFAULT_ROSTER);
        assert!(posts[0].text.starts_with("Nora 🥗"));
        assert!(posts[1].text.starts_with("Bruno "));
        assert!(posts[2].text.starts_with("Coach Mira 💪"));
        assert!(posts[3].text.starts_with("Otto 📋"));
    }

    #[test]
    fn extra_named_bot_is_appended_not_dropped() {
        let mut cfg = casa_config();
        cfg.bots.insert("zoe".to_string(), bot("-100", None));
        let roster = plan_roster(&cfg, DEFAULT_ROSTER);
        let ids: Vec<&str> = roster.iter().map(|m| m.bot_id.as_str()).collect();
        assert_eq!(ids, vec!["nora", "bruno", "mira", "otto", "zoe"]);
    }

    #[test]
    fn legacy_default_bot_is_excluded_from_standup() {
        let mut cfg = casa_config();
        cfg.bot_token = "LEGACY".to_string();
        cfg.chat_id = "123".to_string();
        let roster = plan_roster(&cfg, DEFAULT_ROSTER);
        assert!(roster.iter().all(|m| m.bot_id != "default"));
        assert_eq!(roster.len(), 4);
    }

    #[test]
    fn report_reflects_in_progress_and_open_tasks() {
        let member = StandupMember {
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
            bot_id: "nora".to_string(),
            bot: bot("-100", Some("agent-42")),
            display_name: "Nora".to_string(),
            emoji: "🥗".to_string(),
        };
        assert_eq!(member.agent_id(), "agent-42");
    }
}
