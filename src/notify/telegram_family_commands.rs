//! The family command set: `/dinner`, `/shopping`, `/week`, `/reminders`,
//! `/standup`, `/help` for the Casa Pinello group (Telegram).
//!
//! # One table, many commands
//!
//! Every command is one row of [`FAMILY_COMMANDS`]. A row declares:
//!
//! * `keyword` — the slash word (`"/dinner"`),
//! * `owner` — the bot id whose voice answers in the group (`"bruno"`),
//! * `description` — the one-line `/help` + Telegram autocomplete blurb
//!   (family voice, docs/04),
//! * `data_source` — a short label of where the answer comes from (docs/debug),
//! * `kind` — [`CommandKind::Single`] (one reply from `owner`) or
//!   [`CommandKind::Roster`] (`/standup`: one post per voice), and
//! * `compose` — a **pure** function `fn(&CommandContext) -> String` that
//!   renders the reply from live data (the graph, `notify.toml`, and the parsed
//!   `plans/`), never a placeholder.
//!
//! Adding a command later is exactly one new row plus its `compose` function —
//! [`crate::notify::telegram_family_commands::help`] and the
//! `setMyCommands` registration both iterate the table, so a new row shows up in
//! `/help` and in Telegram autocomplete automatically.
//!
//! # Exactly-once and all-bots-off safe
//!
//! Commands ride on the same dedupe + election layer as everything else. The
//! single `wg telegram listen` process de-duplicates the four copies of a group
//! message (privacy-off delivers each `/command` to every bot) down to one, then
//! composes a single reply and sends it **as the owner bot**. One orchestrator,
//! one send ⇒ exactly-once, regardless of how many bots are muted.
//!
//! # Group vs 1:1 (the voice choice)
//!
//! * **Group** — the *owner* answers: `/dinner` is always Bruno's voice, even if
//!   another bot happened to receive the copy that won dedupe. This keeps each
//!   command in its natural persona.
//! * **1:1** — the bot **you messaged** answers, in its own send path, with the
//!   same composed content. We deliberately do *not* relay a 1:1 `/dinner` over
//!   to Bruno's DM thread: the person asked *this* bot, so *this* bot replies.
//!   The content is identical (same `compose`), only the sending bot differs.
//!   Documented in docs/09 §commands.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, Utc};

use super::family_plan::{self, PlanDoc};
use super::telegram::TelegramConfig;
use super::telegram_standup::{self, humanize_title, DEFAULT_ROSTER};
use crate::graph::{Status, WorkGraph};

/// How a command's reply is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// One reply, sent as the owner bot (or, in a 1:1, as the receiving bot).
    Single,
    /// The whole roster answers, one post per voice (only `/standup`). The
    /// listener orchestrates the sequential sends; [`FamilyCommand::compose`]
    /// still renders a single combined text for the 1:1 / dry-run paths.
    Roster,
}

/// A pure reply composer. Takes everything grounded (graph + config + parsed
/// plans + the current date/time) and returns family-voice text.
pub type ComposeFn = fn(&CommandContext<'_>) -> String;

/// One command in the shared family set.
pub struct FamilyCommand {
    /// The slash keyword, lowercase, with leading slash, e.g. `"/dinner"`.
    pub keyword: &'static str,
    /// The bot id whose voice owns this command in the group, e.g. `"bruno"`.
    pub owner: &'static str,
    /// One-line description (family voice) for `/help` and `setMyCommands`.
    pub description: &'static str,
    /// Where the answer comes from — a short label for docs / diagnostics.
    pub data_source: &'static str,
    /// Single reply or whole-roster.
    pub kind: CommandKind,
    /// Pure composer of the reply text.
    pub compose: ComposeFn,
}

impl FamilyCommand {
    /// The bare command name (no slash) as Telegram's `setMyCommands` wants it.
    pub fn name(&self) -> &str {
        self.keyword.trim_start_matches('/')
    }
}

/// Everything a `compose` function may read. All borrowed and pre-resolved, so
/// composing is pure and unit-testable without a live filesystem or network.
pub struct CommandContext<'a> {
    /// The live work graph (for `/reminders`, `/week`, `/standup`). `None` when
    /// the graph could not be loaded — composers then report an empty/quiet
    /// state rather than failing.
    pub graph: Option<&'a WorkGraph>,
    /// The Telegram roster config (for `/standup` voice ordering).
    pub config: &'a TelegramConfig,
    /// Parsed weekly plans, sorted oldest→newest (for `/dinner`, `/shopping`,
    /// `/week`).
    pub plans: &'a [PlanDoc],
    /// The date "today" is resolved to (dinner + current-week selection).
    pub today: NaiveDate,
    /// The instant "now" for scheduling math (`/reminders` cron next-fire).
    pub now: DateTime<Utc>,
    /// Agent ids that are human operators (for `/reminders` + `/week` pending
    /// confirmations). Pre-computed so composing stays filesystem-free.
    pub human_agents: &'a HashSet<String>,
}

/// The shared command table. **Adding a command is one row here** plus its
/// `compose` fn; `/help` and `setMyCommands` iterate this list.
pub static FAMILY_COMMANDS: &[FamilyCommand] = &[
    FamilyCommand {
        keyword: "/dinner",
        owner: "bruno",
        description: "What's for dinner tonight",
        data_source: "current week plan — today's dinner",
        kind: CommandKind::Single,
        compose: compose_dinner,
    },
    FamilyCommand {
        keyword: "/shopping",
        owner: "otto",
        description: "This week's shopping list",
        data_source: "current week plan — shopping list",
        kind: CommandKind::Single,
        compose: compose_shopping,
    },
    FamilyCommand {
        keyword: "/week",
        owner: "otto",
        description: "The week at a glance",
        data_source: "current week plan — meals + workouts; graph — confirmations",
        kind: CommandKind::Single,
        compose: compose_week,
    },
    FamilyCommand {
        keyword: "/reminders",
        owner: "otto",
        description: "What's pending or coming up",
        data_source: "graph — parked human tasks + cron next-fire",
        kind: CommandKind::Single,
        compose: compose_reminders,
    },
    FamilyCommand {
        keyword: "/standup",
        owner: "otto",
        description: "A quick check-in from the whole team",
        data_source: "graph — each voice's live tasks",
        kind: CommandKind::Roster,
        compose: compose_standup,
    },
    FamilyCommand {
        keyword: "/help",
        owner: "otto",
        description: "Show what you can ask us",
        data_source: "the command table itself",
        kind: CommandKind::Single,
        compose: compose_help,
    },
];

/// Match `text` against the command table, returning the command it invokes.
///
/// Accepts the bare `/dinner`, the group-suffixed `/dinner@bruno_chef_bot`, a
/// leading `wg ` prefix, and any trailing arguments — mirroring how Telegram
/// delivers `/commands` in groups (and [`telegram_standup::is_standup_command`]).
pub fn match_command(text: &str) -> Option<&'static FamilyCommand> {
    let t = text.trim();
    let (t, prefixed) = match t.strip_prefix("wg ") {
        Some(rest) => (rest.trim_start(), true),
        None => (t, false),
    };
    let first = t.split_whitespace().next().unwrap_or("");
    // Strip the `@bot` suffix Telegram appends in groups.
    let cmd = first.split('@').next().unwrap_or(first);
    if cmd.is_empty() {
        return None;
    }
    let bare = cmd.trim_start_matches('/').to_ascii_lowercase();
    FAMILY_COMMANDS.iter().find(|c| {
        c.name().eq_ignore_ascii_case(&bare)
            // With no `wg` prefix, require the leading slash so ordinary chatter
            // ("help me carry this") is never taken for a command.
            && (cmd.starts_with('/') || prefixed)
    })
}

/// Compose a command's reply against live context. `/standup` renders the whole
/// roster as one combined text here (used by the 1:1 and dry-run paths); the
/// group listener posts the roster one voice at a time instead.
pub fn compose(cmd: &FamilyCommand, ctx: &CommandContext<'_>) -> String {
    (cmd.compose)(ctx)
}

/// Operator/coordinator vocabulary that must never appear in family-facing text
/// — the WG claim/done reference and friends. Checked case-insensitively as
/// whole-ish tokens so ordinary family words ("ready in ten minutes") don't trip
/// it. See `fix-command-leaks`.
const OPERATOR_VOCAB: &[&str] = &[
    "claim ", "unclaim", "wg claim", "wg done", "workgraph", "task id", "task_id",
    "coordinator", "`claim", "`done", "`status`",
];

/// Whether `text` is safe to render into a FAMILY chat: no markdown code
/// backticks and none of the operator WG vocabulary. The listener gates every
/// command reply bound for a family chat through this so coordinator content
/// (the raw claim/done/status reference) can never leak into the group — the
/// belt to the structural brace that already keeps the operator command path
/// out of groups. `fix-command-leaks`.
pub fn is_family_voice(text: &str) -> bool {
    if text.contains('`') {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    !OPERATOR_VOCAB.iter().any(|v| lower.contains(v))
}

// ---------------------------------------------------------------------------
// Compose functions — one per command, all pure and family-voice (docs/04).
// ---------------------------------------------------------------------------

/// `/dinner` — Bruno. Tonight's dinner from the plan covering today.
fn compose_dinner(ctx: &CommandContext<'_>) -> String {
    let plan = ctx.plans.iter().find(|p| p.covers(ctx.today));
    let meal = plan.and_then(|p| p.meal_on(ctx.today));
    match meal {
        Some(m) => {
            let prep = if m.prep.is_empty() {
                String::new()
            } else {
                format!(" — about {} at the stove", m.prep.trim_start_matches('~'))
            };
            format!(
                "\u{1f373} Tonight it's {}{}, and I've got it covered.\nFancy a swap? Just tell me and I'll sort something else. \u{1f44c}",
                m.dish, prep
            )
        }
        None => "\u{1f373} I don't have tonight's dinner in the plan yet. \
             Want me to sort something out? Just say the word."
            .to_string(),
    }
}

/// `/shopping` — Otto. The current week's shopping list, phone-friendly, one
/// block per store section.
fn compose_shopping(ctx: &CommandContext<'_>) -> String {
    let plan = family_plan::current_plan(ctx.plans, ctx.today);
    let sections = plan.map(|p| p.shopping.as_slice()).unwrap_or(&[]);
    if sections.is_empty() {
        return "\u{1f6d2} No shopping list yet for this week — I'll put one \
             together once the plan's set."
            .to_string();
    }
    let mut out = String::from("\u{1f6d2} Here's the shopping list for this week:\n");
    for sec in sections {
        out.push('\n');
        out.push_str(&sec.heading);
        out.push('\n');
        for item in &sec.items {
            out.push_str("\u{2022} ");
            out.push_str(item);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// `/week` — Otto. Meals by day + a workouts line + pending confirmations.
/// Dates render as weekday names; the block stays compact.
fn compose_week(ctx: &CommandContext<'_>) -> String {
    let plan = family_plan::current_plan(ctx.plans, ctx.today);
    let mut out = String::from("\u{1f4c5} This week at a glance:\n");

    match plan {
        Some(p) if !p.meals.is_empty() => {
            for m in &p.meals {
                let day = m
                    .date
                    .map(family_plan::long_weekday)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| family_plan::expand_weekday(&m.weekday));
                out.push_str(&format!("\n{} — {}", day, m.dish));
            }
        }
        _ => out.push_str("\nNo meals planned yet."),
    }

    // Workouts: sessions per person, from the same plan.
    if let Some(p) = plan {
        let workout_line = workout_summary(p);
        if !workout_line.is_empty() {
            out.push_str(&format!("\n\n\u{1f4aa} Workouts: {}", workout_line));
        }
    }

    // Pending confirmations, from the live graph.
    let pending = pending_confirmations(ctx);
    if !pending.is_empty() {
        out.push_str(&format!(
            "\n\u{23f3} Waiting on {} {} from the family.",
            pending.len(),
            plural(pending.len(), "confirmation", "confirmations")
        ));
    } else if ctx.graph.is_some() {
        out.push_str("\n\u{2705} Nothing waiting on the family right now.");
    }

    out
}

/// `/reminders` — Otto. Pending human confirmations + upcoming scheduled items,
/// from the graph. Cheerful when there's nothing.
fn compose_reminders(ctx: &CommandContext<'_>) -> String {
    let pending = pending_confirmations(ctx);
    let upcoming = upcoming_scheduled(ctx);

    if pending.is_empty() && upcoming.is_empty() {
        return "\u{1f4cc} You're all caught up — nothing waiting and nothing \
             on the schedule. \u{1f389}"
            .to_string();
    }

    let mut out = String::from("\u{1f4cc} Here's what's still open:\n");
    if !pending.is_empty() {
        out.push_str("\nWaiting on a reply:\n");
        for title in &pending {
            out.push_str(&format!("\u{2022} {}\n", title));
        }
    }
    if !upcoming.is_empty() {
        out.push_str("\nComing up:\n");
        for line in &upcoming {
            out.push_str(&format!("\u{2022} {}\n", line));
        }
    }
    out.trim_end().to_string()
}

/// `/standup` — the whole-team check-in, rendered as one combined text (used in
/// 1:1 and dry-run; the group listener posts each voice separately).
fn compose_standup(ctx: &CommandContext<'_>) -> String {
    let empty = WorkGraph::new();
    let graph = ctx.graph.unwrap_or(&empty);
    let posts = telegram_standup::plan_standup(graph, ctx.config, DEFAULT_ROSTER);
    posts
        .iter()
        .map(|p| p.text.clone())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `/help` — the command list, one line per row of the table, family voice.
fn compose_help(_ctx: &CommandContext<'_>) -> String {
    let mut out = String::from("\u{1f44b} Here's what you can ask us:\n");
    for cmd in FAMILY_COMMANDS {
        out.push_str(&format!("\n{} — {}", cmd.keyword, cmd.description));
    }
    out.push_str(
        "\n\nType any of these in the group, or message a bot directly — either works.",
    );
    out
}

// ---------------------------------------------------------------------------
// Grounding helpers
// ---------------------------------------------------------------------------

/// Sessions-per-person summary, e.g. `"Luca 4 sessions, Nadin 3"`.
fn workout_summary(plan: &PlanDoc) -> String {
    // Preserve first-seen person order.
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for w in &plan.workouts {
        if !order.iter().any(|p| p == &w.person) {
            order.push(w.person.clone());
        }
        *counts.entry(w.person.clone()).or_insert(0) += 1;
    }
    order
        .iter()
        .map(|p| {
            let n = counts[p];
            format!("{} {} {}", p, n, plural(n, "session", "sessions"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Titles of tasks parked awaiting a human reply — the pending confirmations.
/// A task counts when it is `Waiting` and its assigned/agent id is a known human
/// operator. Titles are humanized (week codes / task ids stripped, docs/04).
fn pending_confirmations(ctx: &CommandContext<'_>) -> Vec<String> {
    let graph = match ctx.graph {
        Some(g) => g,
        None => return Vec::new(),
    };
    graph
        .tasks()
        .filter(|t| t.status == Status::Waiting)
        .filter(|t| task_is_human(t, ctx.human_agents))
        .map(|t| bullet_title(&t.title))
        .collect()
}

/// Upcoming scheduled items from cron-enabled tasks, each as
/// `"<humanized title> — <friendly next fire>"`, soonest first.
fn upcoming_scheduled(ctx: &CommandContext<'_>) -> Vec<String> {
    let graph = match ctx.graph {
        Some(g) => g,
        None => return Vec::new(),
    };
    let mut items: Vec<(DateTime<Utc>, String)> = graph
        .tasks()
        .filter(|t| t.cron_enabled)
        .filter_map(|t| {
            let fire = next_fire(t, ctx.now)?;
            if fire < ctx.now {
                return None;
            }
            Some((fire, format!("{} — {}", bullet_title(&t.title), friendly_when(fire, ctx.now))))
        })
        .collect();
    items.sort_by_key(|(when, _)| *when);
    items.into_iter().map(|(_, line)| line).collect()
}

/// The next fire time for a cron task: the stored `next_cron_fire` when present
/// and valid, else computed from the schedule.
fn next_fire(task: &crate::graph::Task, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(s) = task.next_cron_fire.as_deref() {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    let schedule = crate::cron::parse_cron_expression(task.cron_schedule.as_deref()?).ok()?;
    crate::cron::calculate_next_fire(&schedule, now)
}

/// A friendly relative time like `"today"`, `"tomorrow"`, or `"in 3 days"`.
fn friendly_when(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let days = (when.date_naive() - now.date_naive()).num_days();
    match days {
        d if d <= 0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        d if d < 7 => format!("in {} days", d),
        d if d < 14 => "next week".to_string(),
        d => format!("in {} weeks", d / 7),
    }
}

/// True when a task is assigned to one of the known human operators (checking
/// both the resolved `agent` and the human-friendly `assigned` fields).
fn task_is_human(task: &crate::graph::Task, humans: &HashSet<String>) -> bool {
    task.agent.as_deref().map(|a| humans.contains(a)).unwrap_or(false)
        || task
            .assigned
            .as_deref()
            .map(|a| humans.contains(a))
            .unwrap_or(false)
}

/// Humanize a task title (strip week codes / task ids, docs/04) and capitalize
/// the first letter so it reads well as a standalone bullet — the opposite of
/// [`humanize_title`]'s mid-sentence lowercasing.
fn bullet_title(title: &str) -> String {
    let h = humanize_title(title);
    let mut chars = h.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => h,
    }
}

/// Singular/plural helper.
fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Node, Task, WaitCondition, WaitSpec};
    use std::collections::HashMap;

    const W29: &str = include_str!("../../tests/fixtures/family_plan_w29.md");

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn now() -> DateTime<Utc> {
        "2026-07-15T09:00:00Z".parse().unwrap()
    }

    fn casa_config() -> TelegramConfig {
        use super::super::telegram::TelegramBotConfig;
        let mut bots = HashMap::new();
        for id in ["nora", "bruno", "mira", "otto"] {
            bots.insert(
                id.to_string(),
                TelegramBotConfig {
                    bot_token: "T".to_string(),
                    chat_id: "-100".to_string(),
                    agent_id: Some(id.to_string()),
                    username: None,
                },
            );
        }
        TelegramConfig {
            bot_token: String::new(),
            chat_id: String::new(),
            bots,
        }
    }

    fn ctx<'a>(
        plans: &'a [PlanDoc],
        graph: Option<&'a WorkGraph>,
        humans: &'a HashSet<String>,
        config: &'a TelegramConfig,
        today: NaiveDate,
    ) -> CommandContext<'a> {
        CommandContext {
            graph,
            config,
            plans,
            today,
            now: now(),
            human_agents: humans,
        }
    }

    fn w29() -> Vec<PlanDoc> {
        vec![PlanDoc::parse("2026-W29", W29)]
    }

    // --- Table / dispatch -------------------------------------------------

    #[test]
    fn every_command_matches_its_keyword_and_forms() {
        for cmd in FAMILY_COMMANDS {
            let kw = cmd.keyword;
            assert_eq!(match_command(kw).unwrap().keyword, kw);
            assert_eq!(
                match_command(&format!("{kw}@bruno_chef_bot")).unwrap().keyword,
                kw
            );
            assert_eq!(match_command(&format!("  {kw} please ")).unwrap().keyword, kw);
            assert_eq!(
                match_command(&format!("wg {}", kw.trim_start_matches('/')))
                    .unwrap()
                    .keyword,
                kw
            );
        }
    }

    #[test]
    fn plain_chatter_is_not_a_command() {
        assert!(match_command("help me carry this in").is_none());
        assert!(match_command("what's for dinner tonight?").is_none());
        assert!(match_command("").is_none());
        assert!(match_command("/unknown").is_none());
    }

    #[test]
    fn family_voice_gate_rejects_operator_reference_accepts_family_help() {
        // The operator WG help — backticks + claim/done vocabulary — must FAIL
        // the family-voice gate so it can never leak into a family chat.
        let operator_help = "\u{1f4cb} *WG commands*\n\n\u{2022} `claim <task>` \\- Claim a task\n\u{2022} `done <task>` \\- Mark done";
        assert!(
            !is_family_voice(operator_help),
            "operator claim/done reference must not pass the family-voice gate"
        );

        // Every family command reply — /help included — must PASS the gate.
        let plans: Vec<PlanDoc> = Vec::new();
        let humans = HashSet::new();
        let cfg = casa_config();
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 15));
        for cmd in FAMILY_COMMANDS {
            let out = compose(cmd, &c);
            assert!(
                is_family_voice(&out),
                "{} reply is not family-voice: {out:?}",
                cmd.keyword
            );
        }
    }

    #[test]
    fn help_lists_every_command_in_the_table() {
        let plans: Vec<PlanDoc> = Vec::new();
        let humans = HashSet::new();
        let cfg = casa_config();
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 15));
        let help = compose_help(&c);
        for cmd in FAMILY_COMMANDS {
            assert!(help.contains(cmd.keyword), "help missing {}", cmd.keyword);
            assert!(
                help.contains(cmd.description),
                "help missing description for {}",
                cmd.keyword
            );
        }
    }

    // --- Data grounding ---------------------------------------------------

    #[test]
    fn dinner_returns_the_actual_dish_for_today() {
        let plans = w29();
        let humans = HashSet::new();
        let cfg = casa_config();
        // Wednesday 07-15 in the fixture is the lentil & beet salad.
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 15));
        let out = compose_dinner(&c);
        assert!(out.contains("Lentil & roasted-beet salad"), "got: {out}");
        assert!(out.contains("30 min"), "prep time surfaced: {out}");
        assert!(out.to_lowercase().contains("swap"), "swap tail: {out}");
    }

    #[test]
    fn dinner_is_honest_when_today_has_no_plan() {
        let plans = w29();
        let humans = HashSet::new();
        let cfg = casa_config();
        // 07-12 is the day before W29 begins — no covering plan.
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 12));
        let out = compose_dinner(&c);
        assert!(out.to_lowercase().contains("don't have"), "got: {out}");
        assert!(out.to_lowercase().contains("plan"), "offers to plan: {out}");
    }

    #[test]
    fn shopping_lists_real_items_under_store_sections() {
        let plans = w29();
        let humans = HashSet::new();
        let cfg = casa_config();
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 15));
        let out = compose_shopping(&c);
        assert!(out.contains("Fishmonger"), "store section: {out}");
        assert!(out.contains("Salmon fillets"), "grounded item: {out}");
        assert!(out.contains("\u{2022}"), "phone-friendly bullets: {out}");
    }

    #[test]
    fn week_shows_meals_as_weekday_names_plus_workouts() {
        let plans = w29();
        let humans = HashSet::new();
        let cfg = casa_config();
        let c = ctx(&plans, None, &humans, &cfg, date(2026, 7, 15));
        let out = compose_week(&c);
        assert!(out.contains("Monday"), "weekday name, not a date: {out}");
        assert!(!out.contains("07-13"), "no raw dates: {out}");
        assert!(out.contains("Chickpea & spinach curry"), "meal grounded: {out}");
        assert!(out.contains("Workouts:"), "workouts line: {out}");
        assert!(out.contains("Luca"), "workout person: {out}");
    }

    #[test]
    fn reminders_lists_pending_human_task_and_is_cheerful_when_empty() {
        let cfg = casa_config();
        let plans: Vec<PlanDoc> = Vec::new();

        // Empty graph → cheerful.
        let empty = WorkGraph::new();
        let humans = HashSet::new();
        let c = ctx(&plans, Some(&empty), &humans, &cfg, date(2026, 7, 15));
        let out = compose_reminders(&c);
        assert!(out.to_lowercase().contains("caught up"), "got: {out}");

        // A Waiting task assigned to a human → listed as pending.
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "nora-ask-nadin-curry-ok".to_string(),
            title: "Ask Nadin if Monday curry works".to_string(),
            status: Status::Waiting,
            agent: Some("human-nadin".to_string()),
            wait_condition: Some(WaitSpec::All(vec![WaitCondition::HumanInput])),
            ..Default::default()
        }));
        let mut humans = HashSet::new();
        humans.insert("human-nadin".to_string());
        let c = ctx(&plans, Some(&graph), &humans, &cfg, date(2026, 7, 15));
        let out = compose_reminders(&c);
        assert!(out.contains("Waiting on a reply"), "section: {out}");
        assert!(out.contains("Ask Nadin"), "grounded title: {out}");
    }

    #[test]
    fn reminders_surfaces_upcoming_cron_task() {
        let cfg = casa_config();
        let plans: Vec<PlanDoc> = Vec::new();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "weekly-plan-sunday".to_string(),
            title: "Draft next week's family plan".to_string(),
            status: Status::Open,
            cron_enabled: true,
            cron_schedule: Some("0 0 18 * * SUN".to_string()),
            next_cron_fire: Some("2026-07-19T18:00:00Z".to_string()),
            cron_template: false,
            cron_instance_of: None,
            ..Default::default()
        }));
        let humans = HashSet::new();
        let c = ctx(&plans, Some(&graph), &humans, &cfg, date(2026, 7, 15));
        let out = compose_reminders(&c);
        assert!(out.contains("Coming up"), "section: {out}");
        assert!(out.contains("Draft next week"), "grounded title: {out}");
    }

    #[test]
    fn standup_combines_the_roster_voices() {
        let cfg = casa_config();
        let plans: Vec<PlanDoc> = Vec::new();
        let graph = WorkGraph::new();
        let humans = HashSet::new();
        let c = ctx(&plans, Some(&graph), &humans, &cfg, date(2026, 7, 15));
        let out = compose_standup(&c);
        assert!(out.contains("Nora"));
        assert!(out.contains("Bruno"));
        assert!(out.contains("Coach Mira"));
        assert!(out.contains("Otto"));
    }

    #[test]
    fn owners_are_the_expected_voices() {
        let by = |kw: &str| FAMILY_COMMANDS.iter().find(|c| c.keyword == kw).unwrap();
        assert_eq!(by("/dinner").owner, "bruno");
        assert_eq!(by("/shopping").owner, "otto");
        assert_eq!(by("/week").owner, "otto");
        assert_eq!(by("/reminders").owner, "otto");
        assert_eq!(by("/standup").kind, CommandKind::Roster);
    }
}
