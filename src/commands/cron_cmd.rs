//! `wg cron` — diagnostics & control surface for recurring cron tasks.
//!
//! Implements the recurring-wakeup diagnostics surface decided in
//! `docs/research/recurring-wakeup-heartbeat-gaps.md` §6 and the acceptance
//! criteria in `docs/repro-weekly-wakeup-heartbeat.md`
//! (`impl-recurring-heartbeat-diagnostics`).
//!
//! `wg cron doctor` lists every cron-enabled task with: schedule, resolved
//! weekday + UTC time-of-day (so the `cron` crate's non-standard 1=Sunday
//! mapping is *visible*), resolved next-fire, last-fire, whether the task is
//! currently due / overdue, paused / blocking state, and the missed-fire count
//! across daemon downtime. `wg cron list` is the JSON-friendly variant.

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::path::Path;
use worksgood::cron::{
    CronDescription, cron_will_not_fire_reason, describe_cron, format_countdown,
    missed_fires_before_reset, overdue_secs,
};
use worksgood::graph::{Status, Task};
use worksgood::query::is_time_ready;

use super::load_workgraph;

/// JSON-serializable row for `wg cron list --json` / `wg cron doctor --json`.
#[derive(Debug, Serialize)]
struct CronRow {
    id: String,
    title: String,
    status: String,
    /// Raw cron expression.
    cron_schedule: String,
    /// Resolved weekday(s) the expression fires on (e.g. `["Sunday"]`). `None`
    /// when the expression has no day-of-week constraint (fires every day).
    #[serde(skip_serializing_if = "Option::is_none")]
    weekdays: Option<Vec<String>>,
    /// Resolved UTC `HH:MM` time-of-day. `None` when the expression fires more
    /// than once a day.
    #[serde(skip_serializing_if = "Option::is_none")]
    time_utc: Option<String>,
    /// True when the day-of-week field is present — i.e. the non-standard
    /// 1=Sunday mapping is in play.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_dow_field: bool,
    /// One-line human summary (e.g. `"Sun 09:00 UTC (cron dow: 1=Sun … 7=Sat)"`).
    summary: String,
    /// RFC3339 timestamp of the next scheduled fire (with jitter).
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cron_fire: Option<String>,
    /// RFC3339 timestamp of the last fire.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_cron_fire: Option<String>,
    /// True when the task is currently due to run (`is_time_ready`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    due: bool,
    /// Seconds the task has been waiting past its scheduled fire time, when due
    /// but not yet dispatched. `None` when not overdue.
    #[serde(skip_serializing_if = "Option::is_none")]
    overdue_secs: Option<i64>,
    /// Number of scheduled fire windows missed across daemon downtime since the
    /// last run (excluding the one being caught up now). `None` when not
    /// computable.
    #[serde(skip_serializing_if = "Option::is_none")]
    missed_fires: Option<u32>,
    /// True when the task is paused (will not dispatch even when due).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    paused: bool,
    /// True when this cron is *time-due* yet can never fire on its own because
    /// it is paused or in a terminal-dead status (abandoned/failed). The
    /// coordinator silently skips it every tick — `wg cron` surfaces it loudly
    /// as "WILL NOT FIRE" instead of the misleading "DUE".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    will_not_fire: bool,
    /// Human-readable current blocking state, e.g. `"paused"`, `"abandoned"`,
    /// `"failed"`, `"overdue"`, `"waiting"`, or `""` (ready / not due).
    blocking_state: String,
    /// True when this cron task is a TEMPLATE that mints a distinct instance
    /// task per firing (see `cron::mint_cron_instance`). Templates are never
    /// dispatched directly.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cron_template: bool,
    /// Ids of the most recent instances minted from this template (newest
    /// first, capped). Empty for legacy (non-template) crons.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    instances: Vec<String>,
}

fn row_for(task: &Task, now: chrono::DateTime<Utc>) -> Option<CronRow> {
    if !task.cron_enabled {
        return None;
    }
    let raw = task.cron_schedule.clone()?;
    let desc: CronDescription = describe_cron(&raw).unwrap_or(CronDescription {
        raw: raw.clone(),
        weekdays: None,
        time_utc: None,
        has_dow_field: false,
        summary: format!("[unparseable: {}]", raw),
    });

    // `is_time_ready` is purely a *time* gate — it returns true as soon as
    // `next_cron_fire <= now`, regardless of task status. But the coordinator's
    // `ready_tasks` only dispatches Open/Incomplete, non-paused tasks. Reconcile
    // the two here so a cron that is time-due but stuck in a terminal-dead
    // status (abandoned/failed) or paused is surfaced loudly instead of being
    // mislabeled "DUE" — the silent-skip bug that hid the abandoned digest cron.
    let due = is_time_ready(task);
    let stuck = cron_will_not_fire_reason(task);
    let will_not_fire = due && stuck.is_some();
    let overdue = if due { overdue_secs(task, now) } else { None };
    let missed = missed_fires_before_reset(task, now);

    let blocking_state = if let Some(reason) = stuck {
        // paused / abandoned / failed — loud, whether or not the clock says due.
        reason.label().to_string()
    } else if due {
        match task.status {
            Status::Waiting | Status::PendingValidation => "waiting".to_string(),
            Status::Blocked => "blocked".to_string(),
            Status::Open | Status::Incomplete if overdue.is_some() => "overdue".to_string(),
            _ => "due".to_string(),
        }
    } else {
        String::new()
    };

    Some(CronRow {
        id: task.id.clone(),
        title: task.title.clone(),
        status: format!("{:?}", task.status).to_lowercase(),
        cron_schedule: raw,
        weekdays: desc.weekdays.clone(),
        time_utc: desc.time_utc.clone(),
        has_dow_field: desc.has_dow_field,
        summary: desc.summary.clone(),
        next_cron_fire: task.next_cron_fire.clone(),
        last_cron_fire: task.last_cron_fire.clone(),
        due,
        overdue_secs: overdue,
        missed_fires: missed,
        paused: task.paused,
        will_not_fire,
        blocking_state,
        cron_template: task.cron_template,
        instances: Vec::new(),
    })
}

/// Run `wg cron doctor` / `wg cron list` — same output, two names.
pub fn run(dir: &Path, json: bool) -> Result<()> {
    let (graph, _path) = load_workgraph(dir)?;
    let now = Utc::now();

    let mut rows: Vec<CronRow> = graph
        .tasks()
        .filter(|t| t.cron_enabled)
        .filter_map(|t| row_for(t, now))
        .collect();

    // Attach recently-minted instances to each template row so `wg cron` shows
    // the template alongside its last runs (cron-re-registration display).
    for row in &mut rows {
        if !row.cron_template {
            continue;
        }
        let mut insts: Vec<(String, String)> = graph
            .tasks()
            .filter(|t| t.cron_instance_of.as_deref() == Some(row.id.as_str()))
            .map(|t| (t.created_at.clone().unwrap_or_default(), t.id.clone()))
            .collect();
        // Newest first by created_at (id as tiebreaker), capped at 5.
        insts.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        row.instances = insts.into_iter().take(5).map(|(_, id)| id).collect();
    }

    // Sort: due/overdue first, then by next_cron_fire (soonest first), then id.
    rows.sort_by(|a, b| {
        // due tasks float to the top
        match (a.due, b.due) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        let na = a.next_cron_fire.as_deref().unwrap_or("");
        let nb = b.next_cron_fire.as_deref().unwrap_or("");
        nb.cmp(na).reverse().then(a.id.cmp(&b.id))
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No cron-scheduled tasks.");
        return Ok(());
    }

    println!("Cron-scheduled tasks ({}):", rows.len());
    println!();
    for row in &rows {
        // A cron that is stuck (paused / abandoned / failed) is painted RED and
        // labeled "WILL NOT FIRE" so an operator can never mistake it for a
        // healthy cron that is merely due. Everything else keeps the cyan tag.
        let (status_tag, tag_color) = match row.blocking_state.as_str() {
            "paused" => ("PAUSED — WILL NOT FIRE", "\x1b[31m"),
            "abandoned" => ("ABANDONED — WILL NOT FIRE", "\x1b[31m"),
            "failed" => ("FAILED — WILL NOT FIRE", "\x1b[31m"),
            "overdue" => ("OVERDUE", "\x1b[36m"),
            "waiting" => ("WAITING", "\x1b[36m"),
            "blocked" => ("BLOCKED", "\x1b[36m"),
            "due" => ("DUE", "\x1b[36m"),
            _ => {
                if row.due {
                    ("DUE", "\x1b[36m")
                } else {
                    ("scheduled", "\x1b[36m")
                }
            }
        };
        let next_tag = match &row.next_cron_fire {
            Some(ts) => format!("next: {}", format_countdown(ts, now)),
            None => "next: unknown".to_string(),
        };
        let last_tag = match &row.last_cron_fire {
            Some(ts) => format!("last: {}", format_countdown(ts, now)),
            None => "last: never".to_string(),
        };
        let missed_tag = match row.missed_fires {
            Some(n) if n > 0 => format!(" \x1b[33m[missed: {}]\x1b[0m", n),
            _ => String::new(),
        };
        let overdue_tag = match row.overdue_secs {
            Some(s) => format!(" \x1b[31m[overdue: {}s]\x1b[0m", s),
            None => String::new(),
        };
        let template_tag = if row.cron_template {
            "  \x1b[35m[template]\x1b[0m"
        } else {
            ""
        };
        println!(
            "  \x1b[1m{}\x1b[0m — {}  [{}{}\x1b[0m]{}  {}{}{}",
            row.id,
            row.title,
            tag_color,
            status_tag,
            template_tag,
            next_tag,
            missed_tag,
            overdue_tag
        );
        println!("    {}", row.summary);
        println!("    {}  {}", last_tag, row.status);
        if !row.instances.is_empty() {
            println!("    instances: {}", row.instances.join(", "));
        }
    }

    // Loud, grouped footer: any cron that looks due/overdue but can NEVER fire
    // on its own. This is the surface that would have caught the abandoned
    // `daily-digest` cron ("DUE, overdue 4.5h" but never sending a message).
    let stuck: Vec<&CronRow> = rows.iter().filter(|r| r.will_not_fire).collect();
    if !stuck.is_empty() {
        println!();
        let list = stuck
            .iter()
            .map(|r| format!("{} ({})", r.id, r.blocking_state))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "\x1b[31mwarning:\x1b[0m {} cron task(s) are past their scheduled fire time but will \
             NEVER fire on their own: {}. The scheduler silently skips paused/abandoned/failed \
             tasks — re-open (`wg reopen`), unpause, retry, or delete them.",
            stuck.len(),
            list
        );
    }

    // Surface the non-standard dow mapping as a single grouped hint (no per-row
    // spam — the summary already names it).
    let dow_warn = rows.iter().any(|r| r.has_dow_field);
    if dow_warn {
        println!();
        println!(
            "\x1b[33mnote:\x1b[0m the `cron` crate maps day-of-week as 1=Sunday, 2=Monday, …, \
             7=Saturday (NOT standard cron's 0=Sunday, 1=Monday). Each cron summary above names \
             the actual weekday it will fire on — verify you are not scheduling the wrong day."
        );
    }
    Ok(())
}

/// Mark (or unmark) a cron task as a TEMPLATE (cron-re-registration).
///
/// This is the migration path for existing crons and the wiring behind
/// `wg add --cron-template` / `wg edit --cron-template`: it flips the
/// `cron_template` flag on an existing cron-enabled task so that, from the next
/// firing on, the coordinator mints a distinct instance per run instead of
/// re-registering this id (which re-blocks `--after` children). Errors if the
/// task is missing or is not cron-enabled.
pub fn set_cron_template(dir: &Path, id: &str, enable: bool) -> Result<()> {
    let path = super::graph_path(dir);
    let mut err: Option<anyhow::Error> = None;
    worksgood::parser::modify_graph(&path, |graph| {
        let Some(task) = graph.get_task_mut(id) else {
            err = Some(anyhow::anyhow!("Task '{}' not found", id));
            return false;
        };
        if !task.cron_enabled {
            err = Some(anyhow::anyhow!(
                "Task '{}' is not a cron task — set a schedule with --cron first",
                id
            ));
            return false;
        }
        if task.cron_template == enable {
            return false; // no change
        }
        task.cron_template = enable;
        true
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    if enable {
        println!(
            "Task '{}' is now a cron template — each firing mints a distinct instance task.",
            id
        );
    } else {
        println!("Task '{}' is no longer a cron template.", id);
    }
    Ok(())
}

/// `wg cron --rearm <id> [--protect]` — recover a stuck production cron.
///
/// A cron whose instance was abandoned / failed / paused is *time-due* yet the
/// coordinator will never dispatch it (see [`cron_will_not_fire_reason`]) — the
/// silent-skip that killed the `daily-digest` cron (task `re-arm-the`). Re-arming
/// puts it back to a healthy `scheduled` state:
///   * status → `Open`, `paused` cleared, `failure_reason` / `superseded_by`
///     cleared, `assigned` / `completed_at` cleared;
///   * `next_cron_fire` recomputed from *now* so it fires at the next real
///     schedule boundary (not a stale past timestamp that reads "overdue");
///   * with `--protect`, the [`PROTECTED_TAG`](worksgood::graph::PROTECTED_TAG)
///     is added so a future sweep cannot abandon/gc it without `--force`.
///
/// Errors if the task is missing or is not cron-enabled.
pub fn rearm(dir: &Path, id: &str, protect: bool) -> Result<()> {
    use worksgood::cron::{calculate_next_fire, parse_cron_expression};
    use worksgood::graph::{LogEntry, PROTECTED_TAG};

    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("WG not initialized. Run 'wg init' first.");
    }

    let mut err: Option<anyhow::Error> = None;
    let mut next_fire_str: Option<String> = None;
    let mut newly_protected = false;

    worksgood::parser::modify_graph(&path, |graph| {
        let Some(task) = graph.get_task_mut(id) else {
            err = Some(anyhow::anyhow!("Task '{}' not found", id));
            return false;
        };
        if !task.cron_enabled {
            err = Some(anyhow::anyhow!(
                "Task '{}' is not a cron task — set a schedule with --cron first",
                id
            ));
            return false;
        }
        let Some(raw) = task.cron_schedule.clone() else {
            err = Some(anyhow::anyhow!(
                "Task '{}' has cron enabled but no schedule to re-arm from",
                id
            ));
            return false;
        };
        let schedule = match parse_cron_expression(&raw) {
            Ok(s) => s,
            Err(e) => {
                err = Some(anyhow::anyhow!("Invalid cron schedule '{}': {}", raw, e));
                return false;
            }
        };

        let now = Utc::now();
        let next = calculate_next_fire(&schedule, now).map(|dt| dt.to_rfc3339());
        next_fire_str = next.clone();

        let prev_status = task.status;
        task.status = Status::Open;
        task.paused = false;
        task.assigned = None;
        task.completed_at = None;
        task.failure_reason = None;
        task.superseded_by = Vec::new();
        task.next_cron_fire = next;

        if protect && !task.is_protected() {
            task.tags.push(PROTECTED_TAG.to_string());
            newly_protected = true;
        }

        task.log.push(LogEntry {
            timestamp: now.to_rfc3339(),
            actor: Some("cron".to_string()),
            user: Some(worksgood::current_user()),
            message: format!(
                "cron re-armed (was {:?}) → Open; next fire {}{}",
                prev_status,
                next_fire_str.as_deref().unwrap_or("unresolved"),
                if newly_protected {
                    "; marked protected"
                } else {
                    ""
                }
            ),
        });
        true
    })?;

    if let Some(e) = err {
        return Err(e);
    }

    super::notify_graph_changed(dir);
    println!(
        "Re-armed cron '{}' → scheduled (next fire: {}){}",
        id,
        next_fire_str.as_deref().unwrap_or("unresolved"),
        if newly_protected {
            ", marked protected"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use worksgood::graph::{Node, Task, WorkGraph};
    use worksgood::parser::save_graph;

    fn write_graph(dir: &Path, tasks: Vec<Task>) {
        fs::create_dir_all(dir).unwrap();
        let mut g = WorkGraph::new();
        for t in tasks {
            g.add_node(Node::Task(t));
        }
        save_graph(&g, &dir.join("graph.jsonl")).unwrap();
    }

    fn cron_task(id: &str, schedule: &str, next: Option<&str>, last: Option<&str>) -> Task {
        let mut t = Task {
            id: id.to_string(),
            title: format!("task {}", id),
            cron_enabled: true,
            cron_schedule: Some(schedule.to_string()),
            ..Default::default()
        };
        t.next_cron_fire = next.map(|s| s.to_string());
        t.last_cron_fire = last.map(|s| s.to_string());
        t
    }

    #[test]
    fn doctor_no_cron_tasks_succeeds() {
        let dir = tempdir().unwrap();
        write_graph(dir.path(), vec![]);
        let r = run(dir.path(), false);
        assert!(r.is_ok(), "{:?}", r);
    }

    #[test]
    fn doctor_json_emits_array_with_summary() {
        let dir = tempdir().unwrap();
        // dow=1 → Sunday (non-standard mapping). The summary must name Sunday.
        let future = (Utc::now() + chrono::Duration::days(7)).to_rfc3339();
        let t = cron_task("weekly", "0 0 9 * * 1", Some(&future), None);
        write_graph(dir.path(), vec![t]);
        // Capture stdout by running the function — it prints. We only assert
        // the row_for helper (which the printer uses) names Sunday.
        let now = Utc::now();
        let row = row_for(
            &cron_task("weekly", "0 0 9 * * 1", Some(&future), None),
            Utc::now(),
        )
        .expect("row");
        assert!(row.summary.contains("Sun"), "summary: {}", row.summary);
        assert!(row.has_dow_field);
        // JSON path should succeed (it just prints).
        let r = run(dir.path(), true);
        assert!(r.is_ok());
    }

    #[test]
    fn doctor_surfaces_paused_and_overdue_blocking_state() {
        let now = Utc::now();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let mut t = cron_task("paused-due", "0 0 9 * * *", Some(&past), None);
        t.paused = true;
        let row = row_for(&t, now).expect("row");
        assert!(row.due, "past next_cron_fire ⇒ due");
        assert!(row.paused);
        assert_eq!(row.blocking_state, "paused", "paused wins over due");

        let mut t2 = cron_task("overdue-due", "0 0 9 * * *", Some(&past), None);
        t2.status = Status::Open;
        let row2 = row_for(&t2, now).expect("row");
        assert_eq!(row2.blocking_state, "overdue");
        assert!(row2.overdue_secs.unwrap_or(0) > 0);
    }

    #[test]
    fn doctor_missed_fires_column_populates_with_stale_last_fire() {
        let now = Utc::now();
        let stale = (now - chrono::Duration::days(5)).to_rfc3339();
        let t = cron_task("daily-stale", "0 0 9 * * *", None, Some(&stale));
        let row = row_for(&t, now).expect("row");
        // 5 days of daily windows behind now → missed >= 4 (one being caught up).
        let missed = row.missed_fires.expect("computable");
        assert!(
            missed >= 4,
            "expected >=4 missed daily windows, got {}",
            missed
        );
    }

    #[test]
    fn doctor_skips_non_cron_tasks() {
        let t = Task {
            id: "plain".to_string(),
            title: "plain".to_string(),
            cron_enabled: false,
            ..Default::default()
        };
        let now = Utc::now();
        assert!(row_for(&t, now).is_none());
    }

    #[test]
    fn doctor_invalid_cron_schedule_shows_unparseable_summary() {
        let t = cron_task("broken", "not a cron", None, None);
        let now = Utc::now();
        let row = row_for(&t, now).expect("row (cron_enabled)");
        assert!(
            row.summary.contains("unparseable"),
            "summary: {}",
            row.summary
        );
    }

    // ── Regression: overdue-but-non-firing crons must be surfaced loudly, not
    //    mislabeled "DUE". This is the abandoned `daily-digest` bug: a cron
    //    showing "DUE, overdue 4.5h" that could never fire.

    #[test]
    fn doctor_abandoned_cron_is_will_not_fire_not_due() {
        let now = Utc::now();
        let past = (now - chrono::Duration::hours(4)).to_rfc3339();
        let mut t = cron_task("daily-digest", "0 0 9 * * *", Some(&past), None);
        t.status = Status::Abandoned;
        let row = row_for(&t, now).expect("row");
        assert!(
            row.will_not_fire,
            "an overdue abandoned cron must be flagged will_not_fire"
        );
        assert_eq!(
            row.blocking_state, "abandoned",
            "blocking_state must name the terminal-dead status, not 'due'"
        );
        assert_ne!(row.blocking_state, "due");
        assert_ne!(row.blocking_state, "overdue");
        // It is still time-overdue — we keep the overdue seconds so the operator
        // sees HOW long it has silently not fired.
        assert!(row.overdue_secs.unwrap_or(0) > 0);
    }

    #[test]
    fn doctor_failed_cron_is_will_not_fire() {
        let now = Utc::now();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let mut t = cron_task("failed-daily", "0 0 9 * * *", Some(&past), None);
        t.status = Status::Failed;
        let row = row_for(&t, now).expect("row");
        assert!(row.will_not_fire);
        assert_eq!(row.blocking_state, "failed");
    }

    #[test]
    fn doctor_paused_cron_is_will_not_fire() {
        let now = Utc::now();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let mut t = cron_task("paused-daily", "0 0 9 * * *", Some(&past), None);
        t.paused = true;
        let row = row_for(&t, now).expect("row");
        assert!(row.will_not_fire);
        assert_eq!(row.blocking_state, "paused");
    }

    #[test]
    fn doctor_template_cron_is_not_will_not_fire() {
        // A cron template is never dispatched itself (it mints instances), so it
        // must never be flagged as stuck even in a non-Open status.
        let now = Utc::now();
        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let mut t = cron_task("weekly-plan", "0 0 21 * * SUN", Some(&past), None);
        t.cron_template = true;
        t.status = Status::Done;
        let row = row_for(&t, now).expect("row");
        assert!(!row.will_not_fire, "a template is not a stuck cron");
    }

    #[test]
    fn doctor_healthy_open_daily_cron_is_due_and_dispatchable() {
        // Regression for requirement (a): a daily cron registered against a
        // running service must fire at its next boundary. Model "the 08:00
        // boundary just passed" as next_cron_fire 1 minute ago on an Open task
        // with satisfied (no) dependencies, and assert BOTH that `wg cron`
        // reports it as a healthy overdue/due cron (NOT will_not_fire) AND that
        // the coordinator's `ready_tasks` actually returns it for dispatch —
        // i.e. it is not silently skipped the way the abandoned one was.
        use worksgood::graph::{Node, WorkGraph};
        use worksgood::query::ready_tasks;

        let now = Utc::now();
        let boundary_passed = (now - chrono::Duration::minutes(1)).to_rfc3339();
        let mut t = cron_task("live-digest", "0 0 9 * * *", Some(&boundary_passed), None);
        t.status = Status::Open;

        let row = row_for(&t, now).expect("row");
        assert!(row.due, "boundary passed ⇒ due");
        assert!(!row.will_not_fire, "an Open cron fires — not stuck");
        assert_eq!(row.blocking_state, "overdue");

        let mut g = WorkGraph::new();
        g.add_node(Node::Task(t));
        let ready: Vec<String> = ready_tasks(&g).iter().map(|t| t.id.clone()).collect();
        assert!(
            ready.iter().any(|id| id == "live-digest"),
            "an Open daily cron past its boundary must be dispatchable, got {:?}",
            ready
        );
    }

    #[test]
    fn doctor_open_daily_cron_before_boundary_not_yet_due() {
        // The same cron, before its next boundary, must NOT be due and must NOT
        // be dispatched — proving the boundary gate, not an always-fire bug.
        use worksgood::graph::{Node, WorkGraph};
        use worksgood::query::ready_tasks;

        let now = Utc::now();
        let future = (now + chrono::Duration::hours(2)).to_rfc3339();
        let mut t = cron_task("future-digest", "0 0 9 * * *", Some(&future), None);
        t.status = Status::Open;

        let row = row_for(&t, now).expect("row");
        assert!(!row.due, "before boundary ⇒ not due");
        assert!(!row.will_not_fire);
        assert_eq!(row.blocking_state, "");

        let mut g = WorkGraph::new();
        g.add_node(Node::Task(t));
        let ready: Vec<String> = ready_tasks(&g).iter().map(|t| t.id.clone()).collect();
        assert!(!ready.iter().any(|id| id == "future-digest"));
    }
}
