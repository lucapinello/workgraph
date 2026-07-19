use crate::graph::{LogEntry, Node, Status, Task, WorkGraph};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use cron::Schedule;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CronError {
    #[error("Invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("Cron parsing failed: {0}")]
    ParseError(#[from] cron::error::Error),
}

/// Parse a cron expression string into a Schedule
///
/// Supports both 5-field ("min hour day month dow") and 6-field ("sec min hour day month dow") formats.
/// 5-field expressions are automatically converted to 6-field by prepending "0" for seconds.
///
/// # Arguments
/// * `expr` - A cron expression string (5 or 6 field format)
///
/// # Returns
/// * `Result<Schedule, CronError>` - The parsed schedule or an error
///
/// # Examples
/// ```
/// use worksgood::cron::parse_cron_expression;
///
/// let schedule1 = parse_cron_expression("0 2 * * *").unwrap();    // 5-field: daily at 2 AM
/// let schedule2 = parse_cron_expression("0 0 2 * * *").unwrap();  // 6-field: daily at 2 AM
/// ```
pub fn parse_cron_expression(expr: &str) -> Result<Schedule, CronError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();

    let expr_to_parse = match parts.len() {
        5 => {
            // 5-field format: prepend "0" for seconds
            format!("0 {}", expr)
        }
        6 => {
            // 6-field format: use as-is
            expr.to_string()
        }
        _ => {
            return Err(CronError::InvalidExpression(format!(
                "Expected 5 or 6 fields, got {}",
                parts.len()
            )));
        }
    };

    Schedule::from_str(&expr_to_parse).map_err(CronError::ParseError)
}

/// Calculate the next fire time for a cron schedule from a given datetime
///
/// # Arguments
/// * `schedule` - The cron schedule
/// * `from` - The datetime to calculate from
///
/// # Returns
/// * `Option<DateTime<Utc>>` - The next fire time, or None if no next time exists
///
/// # Examples
/// ```
/// use worksgood::cron::{parse_cron_expression, calculate_next_fire};
/// use chrono::Utc;
///
/// let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM
/// let next_fire = calculate_next_fire(&schedule, Utc::now());
/// ```
pub fn calculate_next_fire(schedule: &Schedule, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    schedule.after(&from).next()
}

/// Maximum jitter in seconds (15 minutes).
const MAX_JITTER_SECS: i64 = 15 * 60;

/// Calculate deterministic jitter for a cron task.
///
/// Jitter is ±10% of the period between consecutive fire times, capped at 15 minutes.
/// The sign and magnitude are determined by hashing the task ID, so the same task
/// always gets the same jitter offset.
///
/// # Arguments
/// * `task_id` - The task ID used as hash seed for deterministic jitter
/// * `schedule` - The parsed cron schedule
/// * `from` - A reference time to compute the period from
///
/// # Returns
/// * `Duration` - The jitter offset (may be negative)
pub fn calculate_jitter(task_id: &str, schedule: &Schedule, from: DateTime<Utc>) -> Duration {
    // Compute the period as the interval between two consecutive fire times
    let mut upcoming = schedule.after(&from);
    let first = match upcoming.next() {
        Some(t) => t,
        None => return Duration::zero(),
    };
    let second = match upcoming.next() {
        Some(t) => t,
        None => return Duration::zero(),
    };
    let period_secs = (second - first).num_seconds();
    if period_secs <= 0 {
        return Duration::zero();
    }

    // 10% of the period, capped at MAX_JITTER_SECS
    let max_offset_secs = (period_secs / 10).min(MAX_JITTER_SECS);
    if max_offset_secs == 0 {
        return Duration::zero();
    }

    // Hash the task ID to get a deterministic value in [-max_offset, +max_offset]
    let mut hasher = DefaultHasher::new();
    task_id.hash(&mut hasher);
    let hash_val = hasher.finish();

    // Map hash to range [-max_offset_secs, +max_offset_secs]
    let range = 2 * max_offset_secs + 1; // inclusive range size
    let offset = (hash_val % range as u64) as i64 - max_offset_secs;

    Duration::seconds(offset)
}

/// Calculate the next fire time for a cron task, including deterministic jitter.
///
/// # Arguments
/// * `task_id` - The task ID (used for jitter hash)
/// * `schedule` - The parsed cron schedule
/// * `from` - The time to calculate from (typically last fire or now)
///
/// # Returns
/// * `Option<DateTime<Utc>>` - The next jittered fire time
pub fn calculate_next_fire_with_jitter(
    task_id: &str,
    schedule: &Schedule,
    from: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let next = schedule.after(&from).next()?;
    let jitter = calculate_jitter(task_id, schedule, from);
    Some(next + jitter)
}

/// Reset a cron task after completion: set status to Open, update fire times.
///
/// # Arguments
/// * `task` - The task to reset (must be cron-enabled and Done)
///
/// # Returns
/// * `bool` - true if the task was reset, false if not applicable
/// Reset a cron task after completion: set status to Open, update fire times.
///
/// If the daemon was down across one or more scheduled fire windows since the
/// last run, a `cron_fire_missed` audit entry is appended to `task.log` naming
/// the missed-fire count and the elapsed delta — so `wg show` surfaces *why*
/// a recurring wakeup fired late instead of silently catching up.
///
/// # Arguments
/// * `task` - The task to reset (must be cron-enabled and Done)
///
/// # Returns
/// * `bool` - true if the task was reset, false if not applicable
pub fn reset_cron_task(task: &mut Task) -> bool {
    if !task.cron_enabled || task.cron_schedule.is_none() {
        return false;
    }
    if task.status != crate::graph::Status::Done {
        return false;
    }

    let cron_expr = task.cron_schedule.as_ref().unwrap();
    let schedule = match parse_cron_expression(cron_expr) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let now = Utc::now();

    // Record a `cron_fire_missed` audit entry when this reset is catching up
    // one or more MISSED fire windows (the daemon was down across scheduled
    // fire times). `missed_fires_before_reset` counts scheduled fires strictly
    // between the last run and now, EXCLUDING the one being caught up by this
    // reset — so a fresh on-time reset logs nothing, and a 6-day daemon outage
    // on a weekly cron logs the missed windows. See
    // `docs/repro-weekly-wakeup-heartbeat.md` (catch-up caveat) and
    // `docs/research/recurring-wakeup-heartbeat-gaps.md` §4.1/§4.7.
    if let Some(missed) = missed_fires_before_reset(task, now)
        && missed > 0
    {
        let last_str = task
            .last_cron_fire
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        task.log.push(LogEntry {
            timestamp: now.to_rfc3339(),
            actor: Some("cron".to_string()),
            user: None,
            message: format!(
                "cron_fire_missed: caught up {} missed fire(s) since last run \
                 (last_cron_fire={}). The WG daemon was likely down or spawning \
                 was paused across the scheduled fire window(s); this reset fires \
                 late instead of dropping the run.",
                missed, last_str
            ),
        });
    }

    // Record last fire time
    task.last_cron_fire = Some(now.to_rfc3339());

    // Compute next fire time with jitter
    task.next_cron_fire =
        calculate_next_fire_with_jitter(&task.id, &schedule, now).map(|dt| dt.to_rfc3339());

    // Reset task to Open for next cron cycle
    task.status = crate::graph::Status::Open;
    task.assigned = None;
    task.completed_at = None;

    true
}

// ── Cron template → instance model (cron-re-registration) ────────────────
//
// The recurring-cron deadlock this fixes: the old model re-registered a
// *completed* cron run by flipping the SAME task id from Done back to Open
// (`reset_cron_task`). Any child created `--after <cron-id>` then re-blocked
// on the next (future) firing — the child's work was finished but `wg done`
// refused ("blocked by <cron-id>: Open"), stranding it. See the coordinator
// Phase 2.94/2.95 and the `tonight_*` regression tests.
//
// The fix: a cron task can be a *template* (`cron_template = true`). A template
// is never dispatched itself; each time it is due the coordinator mints a
// fresh, distinct instance task (`<template-id>-<period-suffix>`, e.g.
// `weekly-plan-sunday-2026-W29`) carrying the template's definition. Children
// bind `--after <instance-id>` to that RUN, so the NEXT firing (a new id)
// cannot re-block them. Legacy crons (`cron_template = false`) keep the
// in-place reset behavior for backward compatibility.

/// Produce a stable, human-readable period suffix for a cron fire at `fire`,
/// used to build a distinct instance id per firing. The granularity adapts to
/// how often the schedule fires so the id stays both unique-per-run and
/// readable:
/// - fires at most weekly (min gap ≳ 6 days)   → ISO year-week  `2026-W29`
/// - fires at most daily  (min gap ≳ 23 hours) → date           `2026-07-12`
/// - finer                                       → date + time    `2026-07-12-0230`
pub fn cron_period_suffix(schedule: &Schedule, fire: DateTime<Utc>) -> String {
    // Estimate the cadence from the gap to the following scheduled fire.
    let gap = calculate_next_fire(schedule, fire)
        .map(|next| next - fire)
        .unwrap_or_else(|| Duration::days(1));
    if gap >= Duration::days(6) {
        let iso = fire.iso_week();
        format!("{}-W{:02}", iso.year(), iso.week())
    } else if gap >= Duration::hours(23) {
        fire.format("%Y-%m-%d").to_string()
    } else {
        fire.format("%Y-%m-%d-%H%M").to_string()
    }
}

/// Mint a fresh, distinct instance task from a cron *template* for the fire at
/// `fire`. The instance carries the template's definition (title, description,
/// spawn/exec config, tags, skills, validation, …) but is an ordinary Open
/// task: it has NO cron scheduling of its own, NO inherited dependency edges,
/// and a distinct id of the form `<template-id>-<period-suffix>`. Children
/// created `--after <instance-id>` therefore bind to this run only, so a later
/// firing (a new instance id) never re-blocks them.
///
/// Returns `None` if the template has no parseable cron schedule.
pub fn mint_cron_instance(template: &Task, fire: DateTime<Utc>) -> Option<Task> {
    let expr = template.cron_schedule.as_ref()?;
    let schedule = parse_cron_expression(expr).ok()?;
    let suffix = cron_period_suffix(&schedule, fire);
    let now = Utc::now();

    // Start from the template so every spawn-relevant field (prompt/model/
    // exec_mode/context_scope/priority/tags/skills/validation/…) is carried
    // over, then override everything that must differ for a standalone run.
    let mut inst = template.clone();
    inst.id = format!("{}-{}", template.id, suffix);
    inst.status = crate::graph::Status::Open;
    inst.assigned = None;
    inst.agent = None;

    // A run stands alone: it inherits none of the template's dependency edges,
    // and it is NOT itself a cron task.
    inst.before.clear();
    inst.after.clear();
    inst.requires.clear();
    inst.cron_enabled = false;
    inst.cron_schedule = None;
    inst.cron_template = false;
    inst.last_cron_fire = None;
    inst.next_cron_fire = None;
    inst.cron_instance_of = Some(template.id.clone());

    // Fresh run bookkeeping — none of the template's accumulated run state
    // should leak into a new instance.
    inst.created_at = Some(now.to_rfc3339());
    inst.started_at = None;
    inst.completed_at = None;
    inst.last_interaction_at = Some(now.to_rfc3339());
    inst.dispatch_count = 0;
    inst.retry_count = 0;
    inst.triage_count = 0;
    inst.resurrection_count = 0;
    inst.last_resurrected_at = None;
    inst.gate_attempts = 0;
    inst.rejection_count = 0;
    inst.verify_failures = 0;
    inst.rescue_count = 0;
    inst.rescued = false;
    inst.meta_eval_attempts = 0;
    inst.spawn_failures = 0;
    inst.tried_models.clear();
    inst.token_usage = None;
    inst.session_id = None;
    inst.loop_iteration = 0;
    inst.last_iteration_completed_at = None;
    inst.failure_reason = None;
    inst.failure_class = None;
    inst.log = vec![LogEntry {
        timestamp: now.to_rfc3339(),
        actor: Some("cron".to_string()),
        user: None,
        message: format!(
            "cron_instance_minted: distinct run minted from template '{}' for \
             scheduled fire {}. Children created `--after {}` bind to THIS run, \
             not the recurring template, so the next firing cannot re-block them.",
            template.id,
            fire.to_rfc3339(),
            inst.id,
        ),
    }];

    Some(inst)
}

/// Advance a cron *template* after it has fired: record the fire time and
/// compute the next scheduled fire (with jitter). The template itself stays
/// Open and dormant — it is never dispatched (see `query::is_time_ready`); it
/// only mints instances. Returns false if the task is not a cron task with a
/// parseable schedule.
pub fn advance_cron_template(template: &mut Task, now: DateTime<Utc>) -> bool {
    if !template.cron_enabled || template.cron_schedule.is_none() {
        return false;
    }
    let expr = template.cron_schedule.as_ref().unwrap();
    let schedule = match parse_cron_expression(expr) {
        Ok(s) => s,
        Err(_) => return false,
    };
    template.last_cron_fire = Some(now.to_rfc3339());
    template.next_cron_fire =
        calculate_next_fire_with_jitter(&template.id, &schedule, now).map(|dt| dt.to_rfc3339());
    // A template is never itself dispatched; keep it Open and unassigned so it
    // stays a dormant recurring definition.
    template.status = crate::graph::Status::Open;
    template.assigned = None;
    template.completed_at = None;
    true
}

/// Fire all due cron *templates* in `graph`: for each template whose next fire
/// is due at `now`, mint a distinct instance (idempotent per period — an
/// instance whose id already exists is not re-minted) and advance the template
/// to its next fire. Returns the ids of newly minted instances.
///
/// This is the coordinator's cron firing step for template-mode crons. Because
/// each firing produces a NEW task id, `--after <instance-id>` edges from child
/// work bind to the finished run and are never re-blocked by the next firing —
/// the deadlock this fixes (see the `tonight_*` regression tests).
pub fn mint_due_cron_instances(graph: &mut WorkGraph, now: DateTime<Utc>) -> Vec<String> {
    // Collect due template ids first (immutable borrow), then mutate.
    let due_templates: Vec<String> = graph
        .tasks()
        .filter(|t| t.cron_enabled && t.cron_template && is_cron_due(t, now))
        .map(|t| t.id.clone())
        .collect();

    let mut minted = Vec::new();
    for tid in due_templates {
        // Build the instance while borrowing the template immutably.
        let instance = {
            let Some(template) = graph.get_task(&tid) else {
                continue;
            };
            // The instant this firing represents: the scheduled fire (so the
            // period suffix reflects the schedule slot, not clock skew),
            // falling back to `now` when unset.
            let fire = template
                .next_cron_fire
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(now);
            match mint_cron_instance(template, fire) {
                Some(inst) => inst,
                None => continue,
            }
        };

        // Idempotent per period: if this period's instance already exists,
        // don't duplicate it — but still advance the template so we don't spin
        // on the same due slot.
        if graph.get_task(&instance.id).is_some() {
            if let Some(template) = graph.get_task_mut(&tid) {
                advance_cron_template(template, now);
            }
            continue;
        }

        let instance_id = instance.id.clone();
        graph.add_node(Node::Task(instance));
        if let Some(template) = graph.get_task_mut(&tid) {
            advance_cron_template(template, now);
        }
        minted.push(instance_id);
    }
    minted
}

/// Reset every completed *legacy* (non-template) cron in `graph` to Open for its
/// next period AND satisfy the in-flight dependents of each reset cron, so a
/// child chained `--after <cron-id>` during the just-completed run is never
/// re-blocked when the recurring cron reschedules to its next firing.
///
/// This is the cron-fanout-orphaning fix (2026-07-19 post-mortem). The template
/// model (`mint_due_cron_instances`) already avoids the deadlock for template
/// crons by minting a distinct run id per firing. Legacy crons flip the SAME id
/// Done→Open, so any dependent created against that id re-blocks the moment the
/// completed run resets — its finished work strands until the NEXT firing (up to
/// a week later for a weekly cron). Here we drop the now-stale edge from every
/// dependent that belonged to the just-completed run.
///
/// A dependent "belongs to the completed run" when its `created_at` is at or
/// before the run's completion timestamp (captured BEFORE reset, which clears
/// `completed_at`). A dependent created AFTER the run completed legitimately
/// waits for the NEXT firing and keeps its edge — it is satisfied a cycle later
/// when that firing completes. Dependents that are themselves cron tasks are
/// left untouched so recurring cron→cron pipelines keep firing every period.
///
/// Returns `(reset_cron_ids, satisfied_dependent_ids)`.
pub fn reset_due_legacy_crons(
    graph: &mut WorkGraph,
    now: DateTime<Utc>,
) -> (Vec<String>, Vec<String>) {
    // Capture completed legacy crons and their run-completion timestamps BEFORE
    // resetting — `reset_cron_task` clears `completed_at`.
    let due: Vec<(String, DateTime<Utc>)> = graph
        .tasks()
        .filter(|t| {
            t.cron_enabled && !t.cron_template && t.status == crate::graph::Status::Done
        })
        .map(|t| {
            let completed_at = t
                .completed_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(now);
            (t.id.clone(), completed_at)
        })
        .collect();

    let mut reset_ids = Vec::new();
    let mut satisfied = Vec::new();
    for (cron_id, completed_at) in due {
        let did_reset = graph
            .get_task_mut(&cron_id)
            .map(reset_cron_task)
            .unwrap_or(false);
        if !did_reset {
            continue;
        }
        reset_ids.push(cron_id.clone());
        satisfied.extend(satisfy_reset_cron_dependents(graph, &cron_id, completed_at, now));
    }
    (reset_ids, satisfied)
}

/// Drop the stale `--after <cron_id>` edge from every dependent that belonged to
/// the cron run completing at `run_completed_at` (i.e. `created_at <=
/// run_completed_at`), so the rescheduled recurring cron does not re-block a
/// finished/in-flight child. Cron dependents are skipped (recurring pipelines).
/// Returns the ids of dependents whose edge was cleared.
fn satisfy_reset_cron_dependents(
    graph: &mut WorkGraph,
    cron_id: &str,
    run_completed_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let dependents: Vec<String> = graph
        .tasks()
        .filter(|t| t.after.iter().any(|a| a == cron_id))
        // Never break a recurring cron→cron pipeline: a cron dependent must
        // re-block every period, that is the whole point of the edge.
        .filter(|t| !t.cron_enabled)
        // Only dependents created during (or before) the completed run — a
        // dependent created after it waits for the NEXT firing.
        .filter(|t| {
            t.created_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc) <= run_completed_at)
                .unwrap_or(true) // no created_at → pre-existing, satisfy
        })
        .map(|t| t.id.clone())
        .collect();

    for dep_id in &dependents {
        if let Some(dep) = graph.get_task_mut(dep_id) {
            dep.after.retain(|a| a != cron_id);
            dep.log.push(LogEntry {
                timestamp: now.to_rfc3339(),
                actor: Some("cron".to_string()),
                user: None,
                message: format!(
                    "cron_dependent_satisfied: dropped stale `--after {}` edge. The \
                     recurring cron run this task was chained on has completed and the \
                     cron rescheduled to its next period; a rescheduled cron must not \
                     re-block a finished/in-flight child (cron-fanout orphaning fix).",
                    cron_id
                ),
            });
        }
    }
    dependents
}

/// Check if a task with cron scheduling is due to run based on current time
///
/// # Arguments
/// * `task` - The task to check
/// * `now` - Current datetime
///
/// # Returns
/// * `bool` - true if the task is due to run, false otherwise
///
/// # Examples
/// ```
/// use worksgood::cron::is_cron_due;
/// use worksgood::graph::Task;
/// use chrono::Utc;
///
/// let task = Task {
///     cron_enabled: true,
///     cron_schedule: Some("0 0 2 * * *".to_string()), // Daily at 2 AM
///     ..Default::default()
/// };
/// let due = is_cron_due(&task, Utc::now());
/// ```
pub fn is_cron_due(task: &Task, now: DateTime<Utc>) -> bool {
    // Check if cron is enabled for this task
    if !task.cron_enabled {
        return false;
    }

    // Must have a cron schedule
    let cron_schedule = match &task.cron_schedule {
        Some(schedule) => schedule,
        None => return false,
    };

    // Parse the cron expression
    let schedule = match parse_cron_expression(cron_schedule) {
        Ok(schedule) => schedule,
        Err(_) => return false, // Invalid cron expression means not due
    };

    // If we have a pre-computed next_cron_fire (includes jitter), use that
    if let Some(ref next_fire_str) = task.next_cron_fire
        && let Ok(next_fire) = DateTime::parse_from_rfc3339(next_fire_str)
    {
        return next_fire.with_timezone(&Utc) <= now;
    }
    // Invalid timestamp, fall through to schedule-based check

    // If no last fire time, check if we should fire now based on schedule
    let last_fire = match &task.last_cron_fire {
        Some(last_fire_str) => {
            match DateTime::parse_from_rfc3339(last_fire_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => return true, // Invalid timestamp, assume we should fire
            }
        }
        None => {
            // No last fire time recorded, check if current time matches schedule
            return schedule.includes(now);
        }
    };

    // Check if there's a next fire time between last fire and now
    match calculate_next_fire(&schedule, last_fire) {
        Some(next_fire) => next_fire <= now,
        None => false,
    }
}

/// Reasons a cron task will **not** fire even though its scheduled fire time
/// has arrived. Returned by [`cron_will_not_fire_reason`].
///
/// The coordinator's `ready_tasks` only dispatches tasks whose status is
/// `Open` / `Incomplete` and which are not paused. A cron task can therefore
/// be *time-due* (`is_cron_due` / `is_time_ready` return true because
/// `next_cron_fire <= now`) and yet be permanently stuck: the scheduler will
/// silently skip it every tick. `wg cron` used to tag such tasks `DUE` /
/// `OVERDUE`, indistinguishable from a healthy cron about to run — which is
/// exactly how the abandoned `daily-digest` smoke-test cron showed up as
/// "DUE, overdue 4.5h" while never producing its morning message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronStuckReason {
    /// The task is paused; the dispatcher skips paused tasks.
    Paused,
    /// The task was abandoned — a terminal, non-dispatchable status.
    Abandoned,
    /// The task failed — a terminal, non-dispatchable status. Covers both
    /// `Failed` and `FailedPendingEval`.
    Failed,
}

impl CronStuckReason {
    /// Lower-case one-word label used in `wg cron`'s `blocking_state` / JSON.
    pub fn label(&self) -> &'static str {
        match self {
            CronStuckReason::Paused => "paused",
            CronStuckReason::Abandoned => "abandoned",
            CronStuckReason::Failed => "failed",
        }
    }
}

/// If this cron task can never fire on its own — because it is paused or in a
/// terminal-dead status — return why. Returns `None` for tasks the coordinator
/// can still dispatch (`Open`/`Incomplete`) or that are transiently mid-cycle
/// (`InProgress`, or `Done` awaiting the coordinator's cron reset, or the
/// transient `Blocked`/`Waiting`/`PendingValidation` dispatch states, each of
/// which `wg cron` already surfaces with its own blocking_state).
///
/// This is the primitive behind `wg cron`'s loud "WILL NOT FIRE" surfacing:
/// an overdue cron that is paused/abandoned/failed must be flagged, never
/// silently reported as `DUE`. A cron *template* (`cron_template`) is never
/// dispatched itself — it mints instances — so a template is not "stuck".
pub fn cron_will_not_fire_reason(task: &Task) -> Option<CronStuckReason> {
    if !task.cron_enabled || task.cron_template {
        return None;
    }
    if task.paused {
        return Some(CronStuckReason::Paused);
    }
    match task.status {
        Status::Abandoned => Some(CronStuckReason::Abandoned),
        Status::Failed | Status::FailedPendingEval => Some(CronStuckReason::Failed),
        _ => None,
    }
}

// ── Diagnostics surface (impl-recurring-heartbeat-diagnostics) ──────────
//
// These helpers power the `wg cron doctor` / `wg list` / `wg show` cron
// diagnostics. They resolve the *actual* weekday(s) and UTC time-of-day a
// cron expression will fire, count missed fires across daemon downtime, and
// describe the schedule in one human-readable line — so a user scheduling
// "Monday" does not silently get "Sunday" (the `cron` crate's
// non-standard 1=Sunday mapping — see
// `cron_dow_mapping_is_nonstandard_one_indexed_sunday`).

/// Weekday names in calendar order (Sunday first), matching the `cron`
/// crate's day-of-week numbering (1=Sunday … 7=Saturday).
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn weekday_short(name: &str) -> &'static str {
    match name {
        "Sunday" => "Sun",
        "Monday" => "Mon",
        "Tuesday" => "Tue",
        "Wednesday" => "Wed",
        "Thursday" => "Thu",
        "Friday" => "Fri",
        "Saturday" => "Sat",
        _ => "???",
    }
}

/// A resolved, human-readable description of a cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronDescription {
    /// The raw expression as stored on the task.
    pub raw: String,
    /// Resolved weekday(s) the expression fires on (e.g. `["Sunday"]`).
    /// `None` when the expression has no day-of-week constraint (fires every
    /// day). Sorted in calendar order, de-duplicated.
    pub weekdays: Option<Vec<String>>,
    /// Resolved UTC `HH:MM` time-of-day the expression fires at. `None` when
    /// the expression fires more than once a day or has no fixed time.
    pub time_utc: Option<String>,
    /// True when the day-of-week field is present in the raw expression — i.e.
    /// the non-standard 1=Sunday mapping is in play and a user might be
    /// surprised. Surfaced as a warning in `wg list` / `wg show` / `wg cron`.
    pub has_dow_field: bool,
    /// One-line human summary, e.g. `"Sun 09:00 UTC (cron dow=1 = Sunday)"`.
    pub summary: String,
}

/// Resolve a cron expression into a human-readable description.
///
/// Samples the next 14 fire times from `now` to infer the weekday(s) and
/// UTC time-of-day the expression actually fires on. Returns `None` if the
/// expression cannot be parsed.
///
/// This is the diagnostic that makes the `cron` crate's non-standard
/// day-of-week mapping (1=Sunday, 2=Monday, …, 7=Saturday) *visible* — a
/// user who writes `0 0 9 * * 1` intending "Monday 09:00" sees
/// `"Sun 09:00 UTC (cron dow: 1=Sun … 7=Sat)"` and catches the wrong-day bug.
pub fn describe_cron(expr: &str) -> Option<CronDescription> {
    let schedule = parse_cron_expression(expr).ok()?;
    let now = Utc::now();

    // Sample the next 14 fires (≈ two weeks) to gather distinct weekdays +
    // times-of-day. For weekly crons 14 samples cover two fires; for daily
    // crons they cover two weeks of the same weekday/time.
    let mut weekday_set: Vec<u32> = Vec::new();
    let mut time_set: Vec<(u32, u32)> = Vec::new();
    let mut upcoming = schedule.after(&now);
    for _ in 0..14 {
        let Some(t) = upcoming.next() else {
            break;
        };
        let dow = t.weekday().num_days_from_sunday(); // 0=Sunday
        if !weekday_set.contains(&dow) {
            weekday_set.push(dow);
        }
        let hm = (t.hour(), t.minute());
        if !time_set.contains(&hm) {
            time_set.push(hm);
        }
    }
    weekday_set.sort_unstable();

    // Determine whether the raw expression has a day-of-week field. Both 5-
    // and 6-field forms are supported by `parse_cron_expression`; the dow is
    // the LAST field in both (5-field: min hour day month dow; 6-field:
    // sec min hour day month dow). A `*` dow means every day — not a
    // user-specified weekday, so no warning.
    let parts: Vec<&str> = expr.split_whitespace().collect();
    let dow_field = parts.last().copied().unwrap_or("");
    let has_dow_field = parts.len() >= 5 && dow_field != "*";

    let weekdays: Option<Vec<String>> = if has_dow_field {
        let names: Vec<String> = weekday_set
            .iter()
            .map(|&i| WEEKDAY_NAMES[i as usize].to_string())
            .collect();
        if names.is_empty() { None } else { Some(names) }
    } else {
        None
    };

    let time_utc: Option<String> = if time_set.len() == 1 {
        let (h, m) = time_set[0];
        Some(format!("{:02}:{:02}", h, m))
    } else {
        None
    };

    let summary = build_cron_summary(expr, &weekdays, time_utc.as_deref(), has_dow_field);

    Some(CronDescription {
        raw: expr.to_string(),
        weekdays,
        time_utc,
        has_dow_field,
        summary,
    })
}

fn build_cron_summary(
    expr: &str,
    weekdays: &Option<Vec<String>>,
    time_utc: Option<&str>,
    has_dow_field: bool,
) -> String {
    let day_part = match weekdays {
        Some(names) if !names.is_empty() => {
            let shorts: Vec<&str> = names.iter().map(|n| weekday_short(n)).collect();
            shorts.join("/")
        }
        _ => "daily".to_string(),
    };
    let time_part = time_utc.unwrap_or("varied");
    let dow_note = if has_dow_field {
        // Name the non-standard mapping so a user who wrote `1` for "Monday"
        // sees that `1` means Sunday in this crate.
        let mapping = "cron dow: 1=Sun 2=Mon … 7=Sat";
        format!(" ({})", mapping)
    } else {
        String::new()
    };
    format!("{} {} UTC{} [{}]", day_part, time_part, dow_note, expr)
}

/// Count the number of scheduled fire windows *missed* between the task's
/// last run (`last_cron_fire`) and `now`, EXCLUDING the one being caught up
/// by a reset (so a fresh on-time reset returns 0).
///
/// Returns `None` when the count cannot be computed (no schedule, no
/// `last_cron_fire`, or unparseable timestamp). Returns `Some(0)` when no
/// windows were missed (on-time reset, or `now` is before/at the last fire).
///
/// This is the unit-level primitive behind the `cron_fire_missed` audit
/// entry written by `reset_cron_task` and the `missed_fires` column in
/// `wg cron doctor`.
pub fn missed_fires_before_reset(task: &Task, now: DateTime<Utc>) -> Option<u32> {
    let expr = task.cron_schedule.as_ref()?;
    let schedule = parse_cron_expression(expr).ok()?;
    let last_str = task.last_cron_fire.as_ref()?;
    let last = DateTime::parse_from_rfc3339(last_str)
        .ok()?
        .with_timezone(&Utc);
    if last >= now {
        return Some(0);
    }
    let mut count = 0u32;
    let mut upcoming = schedule.after(&last);
    while let Some(t) = upcoming.next() {
        if t > now {
            break;
        }
        count += 1;
        if count > 10_000 {
            break; // safety valve against pathological schedules
        }
    }
    // `count` includes the fire being caught up by the current reset (the
    // most recent scheduled window at-or-before `now`). Missed windows are
    // the ones BEFORE that — i.e. count − 1.
    Some(count.saturating_sub(1))
}

/// Number of seconds a due cron task has been waiting past its scheduled
/// fire time (`now - next_cron_fire` when `next_cron_fire <= now`), or `None`
/// when not computable / not yet due. Used by `wg cron doctor` to surface
/// "this task is due but has not dispatched" latency.
pub fn overdue_secs(task: &Task, now: DateTime<Utc>) -> Option<i64> {
    let nf_str = task.next_cron_fire.as_ref()?;
    let nf = DateTime::parse_from_rfc3339(nf_str)
        .ok()?
        .with_timezone(&Utc);
    if nf > now {
        return None;
    }
    Some((now - nf).num_seconds())
}

/// Human-readable "in N" / "N ago" countdown for a fire timestamp, used by
/// `wg cron doctor` and `wg list` so a user can see at a glance whether the
/// next fire is imminent or the task is overdue.
pub fn format_countdown(target: &str, now: DateTime<Utc>) -> String {
    let Ok(ts) = target.parse::<DateTime<Utc>>() else {
        return String::new();
    };
    let secs = (ts - now).num_seconds();
    if secs == 0 {
        return "now".to_string();
    }
    let (abs, suffix) = if secs < 0 {
        ((-secs) as i64, "ago")
    } else {
        (secs, "in")
    };
    let human = if abs < 60 {
        format!("{}s", abs)
    } else if abs < 3600 {
        format!("{}m {}s", abs / 60, abs % 60)
    } else if abs < 86400 {
        format!("{}h {}m", abs / 3600, (abs % 3600) / 60)
    } else {
        format!("{}d {}h", abs / 86400, (abs % 86400) / 3600)
    };
    format!("{} {}", suffix, human)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_parse_cron_expression_valid() {
        // Daily at 2 AM
        let result = parse_cron_expression("0 0 2 * * *");
        assert!(result.is_ok());

        // Every 5 minutes
        let result = parse_cron_expression("0 */5 * * * *");
        assert!(result.is_ok());

        // Weekdays at noon
        let result = parse_cron_expression("0 0 12 * * 1-5");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_cron_expression_invalid() {
        // Invalid format
        let result = parse_cron_expression("invalid cron");
        assert!(result.is_err());

        // Too many fields
        let result = parse_cron_expression("0 0 0 0 0 0");
        assert!(result.is_err());
    }

    #[test]
    fn test_calculate_next_fire() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM

        // Test from 1 AM, next should be 2 AM today
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap());

        // Test from 3 AM, next should be 2 AM tomorrow
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 2, 2, 0, 0).unwrap());
    }

    #[test]
    fn test_is_cron_due_disabled() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: false,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_no_schedule() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: None,
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_invalid_schedule() {
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("invalid".to_string()),
            ..Default::default()
        };

        let now = Utc::now();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_no_last_fire() {
        let schedule_str = "0 0 2 * * *"; // Daily at 2 AM
        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some(schedule_str.to_string()),
            last_cron_fire: None,
            ..Default::default()
        };

        // Test at 2 AM - should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);

        // Test at 3 AM - should not be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 3, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), false);
    }

    #[test]
    fn test_is_cron_due_with_last_fire() {
        let schedule_str = "0 0 2 * * *"; // Daily at 2 AM
        let last_fire = "2024-01-01T02:00:00Z"; // Fired at 2 AM on Jan 1

        let task = Task {
            id: "test".to_string(),
            cron_enabled: true,
            cron_schedule: Some(schedule_str.to_string()),
            last_cron_fire: Some(last_fire.to_string()),
            ..Default::default()
        };

        // Test at 1 AM next day - should not be due yet
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 1, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), false);

        // Test at 2 AM next day - should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 2, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);

        // Test at 3 AM next day - should be due (missed the 2 AM window)
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 0, 0).unwrap();
        assert_eq!(is_cron_due(&task, now), true);
    }

    #[test]
    fn cron_parsing() {
        // Test various cron expressions (6-field format with seconds)
        let result = parse_cron_expression("0 0 2 * * *"); // Daily at 2 AM
        if result.is_err() {
            println!("Debug: Error parsing '0 0 2 * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 */5 * * * *"); // Every 5 minutes
        if result.is_err() {
            println!("Debug: Error parsing '0 */5 * * * *': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 0 12 * * 1-5"); // Weekdays at noon
        if result.is_err() {
            println!("Debug: Error parsing '0 0 12 * * 1-5': {:?}", result);
        }
        assert!(result.is_ok());

        let result = parse_cron_expression("0 30 14 1 * *"); // 2:30 PM on 1st day of month
        if result.is_err() {
            println!("Debug: Error parsing '0 30 14 1 * *': {:?}", result);
        }
        assert!(result.is_ok());

        // Test invalid expressions
        assert!(parse_cron_expression("invalid").is_err());
        assert!(parse_cron_expression("").is_err());
        assert!(parse_cron_expression("60 25 32 13 8 8").is_err()); // Invalid values
    }

    #[test]
    fn test_cron_task_becomes_ready_at_fire_time() {
        // A cron task with next_cron_fire in the past should be due
        let past_fire = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        let task = Task {
            id: "cron-ready-test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            next_cron_fire: Some(past_fire.to_rfc3339()),
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            ..Default::default()
        };

        // Time after fire time → should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 1).unwrap();
        assert!(is_cron_due(&task, now));

        // Time exactly at fire time → should be due
        assert!(is_cron_due(&task, past_fire));

        // Time before fire time → should NOT be due
        let before = Utc.with_ymd_and_hms(2024, 1, 1, 1, 59, 59).unwrap();
        assert!(!is_cron_due(&task, before));
    }

    #[test]
    fn test_cron_task_resets_to_open_after_completion() {
        let mut task = Task {
            id: "cron-reset-test".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            assigned: Some("agent-123".to_string()),
            completed_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let result = reset_cron_task(&mut task);
        assert!(
            result,
            "reset_cron_task should return true for Done cron task"
        );
        assert_eq!(task.status, crate::graph::Status::Open);
        assert!(task.assigned.is_none(), "assigned should be cleared");
        assert!(
            task.completed_at.is_none(),
            "completed_at should be cleared"
        );
        assert!(
            task.last_cron_fire.is_some(),
            "last_cron_fire should be set"
        );
        assert!(
            task.next_cron_fire.is_some(),
            "next_cron_fire should be set"
        );
    }

    #[test]
    fn test_cron_reset_does_not_apply_to_non_cron_task() {
        let mut task = Task {
            id: "non-cron".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: false,
            ..Default::default()
        };
        assert!(!reset_cron_task(&mut task));
    }

    #[test]
    fn test_cron_reset_does_not_apply_to_non_done_task() {
        let mut task = Task {
            id: "cron-not-done".to_string(),
            status: crate::graph::Status::InProgress,
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            ..Default::default()
        };
        assert!(!reset_cron_task(&mut task));
    }

    #[test]
    fn test_jitter_is_deterministic_per_task_id() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap(); // Daily at 2 AM
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        // Same task ID produces same jitter
        let jitter1 = calculate_jitter("my-cron-task", &schedule, from);
        let jitter2 = calculate_jitter("my-cron-task", &schedule, from);
        assert_eq!(jitter1, jitter2, "jitter should be deterministic");

        // Different task IDs produce (likely) different jitter
        let jitter_other = calculate_jitter("other-cron-task", &schedule, from);
        // Can't guarantee different, but with a daily schedule (86400s period, ±8640s jitter range)
        // two random hashes should differ. Let's at least check they're both within bounds.
        let period_secs = 86400i64; // daily
        let max_offset = (period_secs / 10).min(MAX_JITTER_SECS);
        assert!(jitter1.num_seconds().abs() <= max_offset);
        assert!(jitter_other.num_seconds().abs() <= max_offset);
    }

    #[test]
    fn test_jitter_bounded_by_max() {
        // Every-minute schedule: period = 60s, 10% = 6s, max = min(6, 900) = 6
        let schedule = parse_cron_expression("0 * * * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let jitter = calculate_jitter("task-a", &schedule, from);
        assert!(
            jitter.num_seconds().abs() <= 6,
            "jitter should be ≤6s for minute schedule"
        );
    }

    #[test]
    fn test_calculate_next_fire_with_jitter_returns_value() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let next = calculate_next_fire_with_jitter("test-task", &schedule, from);
        assert!(next.is_some());

        // The raw next fire is 2024-01-01 02:00:00. With jitter (±8640s for daily),
        // the result should be within that range.
        let next = next.unwrap();
        let raw_next = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();
        let diff = (next - raw_next).num_seconds().abs();
        assert!(
            diff <= MAX_JITTER_SECS,
            "jitter should be within MAX_JITTER_SECS"
        );
    }

    #[test]
    fn test_is_cron_due_with_next_cron_fire() {
        // When next_cron_fire is set, it takes priority over schedule-based checks
        let future_fire = Utc.with_ymd_and_hms(2024, 6, 15, 10, 0, 0).unwrap();
        let task = Task {
            id: "fire-test".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 2 * * *".to_string()),
            next_cron_fire: Some(future_fire.to_rfc3339()),
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            ..Default::default()
        };

        // Before the fire time → not due
        let before = Utc.with_ymd_and_hms(2024, 6, 15, 9, 59, 59).unwrap();
        assert!(!is_cron_due(&task, before));

        // At the fire time → due
        assert!(is_cron_due(&task, future_fire));

        // After the fire time → due
        let after = Utc.with_ymd_and_hms(2024, 6, 15, 10, 0, 1).unwrap();
        assert!(is_cron_due(&task, after));
    }

    #[test]
    fn test_5_field_cron_expression() {
        // 5-field format should be auto-converted to 6-field
        let result = parse_cron_expression("0 2 * * *"); // Daily at 2:00 AM
        assert!(result.is_ok());

        let schedule = result.unwrap();
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap());
    }

    // ── Weekly Monday-cron wakeup semantics (repro-weekly-wakeup-heartbeat) ─
    //
    // A weekly cron intended to fire every Monday. NOTE: the `cron` crate
    // (0.12.x) uses a NON-STANDARD day-of-week mapping — see
    // `cron_dow_mapping_is_nonstandard_one_indexed_sunday` below — where
    // dow=1 is Sunday and dow=2 is Monday (standard cron is 0=Sun, 1=Mon).
    // So to fire on Monday we use `0 0 9 * * 2`. This pins the weekly
    // day-of-week contract so a future parser change (or a crate upgrade
    // that re-aligns to standard cron) is caught.
    #[test]
    fn weekly_monday_cron_fires_only_on_monday_utc() {
        let schedule = parse_cron_expression("0 0 9 * * 2").unwrap(); // Mon 09:00 UTC

        // Sunday 2024-01-07 09:00 → next fire is Monday 2024-01-08 09:00
        let sun = Utc.with_ymd_and_hms(2024, 1, 7, 9, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, sun).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap());

        // Monday 2024-01-08 08:59 → next fire is today 09:00
        let mon_before = Utc.with_ymd_and_hms(2024, 1, 8, 8, 59, 0).unwrap();
        let next = calculate_next_fire(&schedule, mon_before).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap());

        // Monday 2024-01-08 09:01 → next fire is NEXT Monday 2024-01-15 09:00
        let mon_after = Utc.with_ymd_and_hms(2024, 1, 8, 9, 1, 0).unwrap();
        let next = calculate_next_fire(&schedule, mon_after).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());

        // Tuesday 2024-01-09 09:00 → next fire is Monday 2024-01-15 09:00
        let tue = Utc.with_ymd_and_hms(2024, 1, 9, 9, 0, 0).unwrap();
        let next = calculate_next_fire(&schedule, tue).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());
    }

    // ── DISCOVERED FAILURE: non-standard day-of-week mapping ──────────────
    //
    // The `cron` crate (0.12.x) maps dow=1 → Sunday, dow=2 → Monday, …,
    // dow=7 → Saturday. This is the OPPOSITE of standard cron (0=Sun,
    // 1=Mon). A user who writes `0 0 9 * * 1` intending "Monday 09:00"
    // gets **Sunday** 09:00 — the weekly trigger fires on the WRONG DAY.
    //
    // This is pinned here so the surprising mapping is documented in code,
    // and so a future crate upgrade or a local remapping layer that aligns
    // to standard cron (0=Sun, 1=Mon) flips this test and is forced to be
    // intentional. Acceptance criteria for a downstream fix (see
    // impl-recurring-heartbeat-diagnostics / design-durable-recurring-
    // process-graphs): either remap dow on input so `1` means Monday as
    // users expect, OR surface the mapping loudly in `wg add --cron` /
    // `wg list` output so users don't silently schedule the wrong day.
    #[test]
    fn cron_dow_mapping_is_nonstandard_one_indexed_sunday() {
        // From Sunday 2024-01-07 00:00 UTC, the next fire for each dow:
        let from = Utc.with_ymd_and_hms(2024, 1, 7, 0, 0, 0).unwrap(); // Sunday

        // dow=1 fires TODAY (Sunday) — proves dow=1 means Sunday, not Monday.
        let s1 = parse_cron_expression("0 0 9 * * 1").unwrap();
        assert_eq!(
            calculate_next_fire(&s1, from).unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 7, 9, 0, 0).unwrap(),
            "dow=1 must fire on Sunday — the cron crate's non-standard mapping"
        );

        // dow=2 fires Monday 2024-01-08 — proves dow=2 means Monday.
        let s2 = parse_cron_expression("0 0 9 * * 2").unwrap();
        assert_eq!(
            calculate_next_fire(&s2, from).unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap(),
            "dow=2 must fire on Monday"
        );

        // dow=7 fires Saturday 2024-01-13 — proves 7=Saturday (1-indexed week).
        let s7 = parse_cron_expression("0 0 9 * * 7").unwrap();
        assert_eq!(
            calculate_next_fire(&s7, from).unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 13, 9, 0, 0).unwrap(),
            "dow=7 must fire on Saturday"
        );
    }

    // ── Timezone / DST gap (repro-weekly-wakeup-heartbeat) ────────────────
    //
    // DOCUMENTED BEHAVIOUR (not a bug to fix here — pinned so a future
    // change is intentional): WG cron expressions evaluate in **UTC only**.
    // `is_cron_due` takes `DateTime<Utc>` and the cron crate schedules in
    // UTC. A user who writes `0 0 9 * * 2` expecting "every Monday 09:00
    // local" gets 09:00 UTC — which is a different local wall-clock under
    // any non-UTC zone, and shifts by an hour across DST transitions
    // without the expression changing. There is no local-tz cron mode.
    //
    // Acceptance criteria for a future "local cron" feature (downstream
    // task design-durable-recurring-process-graphs):
    //   - a TZ-aware cron would fire at a fixed LOCAL wall-clock that does
    //     NOT shift across DST; OR the schedule stores an explicit zone.
    // Until then, this test pins the UTC-only contract.
    #[test]
    fn cron_evaluates_in_utc_no_local_dst_shift() {
        // `0 0 9 * * 2` = Monday 09:00 UTC (dow=2 is Monday in this crate —
        // see cron_dow_mapping_is_nonstandard_one_indexed_sunday).
        let task = Task {
            id: "weekly-utc".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * 2".to_string()),
            // No next_cron_fire → schedule-based check via `schedule.includes(now)`
            next_cron_fire: None,
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            last_cron_fire: None,
            ..Default::default()
        };

        // Monday 2024-01-08 08:59 UTC → NOT due (one minute before 09:00)
        let before = Utc.with_ymd_and_hms(2024, 1, 8, 8, 59, 0).unwrap();
        assert!(!is_cron_due(&task, before));

        // Monday 2024-01-08 09:00 UTC → due (matches the minute)
        let at = Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap();
        assert!(is_cron_due(&task, at));

        // Monday 2024-01-08 09:01 UTC → NOT due (minute 1 != 0; the
        // 6-field expr `0 0 9 * * 2` pins min=0, so only 09:00:00 matches)
        let after = Utc.with_ymd_and_hms(2024, 1, 8, 9, 1, 0).unwrap();
        assert!(
            !is_cron_due(&task, after),
            "09:01 UTC on Monday must NOT be due — the 6-field expr `0 0 9 * * 2` pins min=0, so only 09:00:00 matches; this pins the UTC minute-exact contract"
        );

        // Sunday 2024-01-07 09:00 UTC → NOT due (wrong day — Monday cron)
        let sun = Utc.with_ymd_and_hms(2024, 1, 7, 9, 0, 0).unwrap();
        assert!(
            !is_cron_due(&task, sun),
            "Sunday must NOT match a Monday cron — pins the UTC day-of-week gate"
        );
    }

    // ── Missed-trigger catch-up (repro-weekly-wakeup-heartbeat) ───────────
    //
    // If the daemon was DOWN across the scheduled fire time, `next_cron_fire`
    // is already in the past when the daemon restarts. `is_cron_due` must
    // return true so the FIRST tick wakes the task — the missed fire is
    // caught up (fired late), NOT silently dropped. This is the unit-level
    // pin for the smoke scenario `cron_weekly_wakeup_becomes_ready.sh`.
    #[test]
    fn missed_cron_fire_is_caught_up_when_next_fire_is_past() {
        // next_cron_fire was set to last Monday 09:00; the daemon was down
        // across that time and is restarting now (well past it).
        let past_fire = Utc.with_ymd_and_hms(2024, 1, 8, 9, 0, 0).unwrap();
        let task = Task {
            id: "weekly-missed".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * 1".to_string()),
            next_cron_fire: Some(past_fire.to_rfc3339()),
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            ..Default::default()
        };

        // Restart happens two days after the missed fire — must be due.
        let restart = Utc.with_ymd_and_hms(2024, 1, 10, 12, 0, 0).unwrap();
        assert!(
            is_cron_due(&task, restart),
            "a cron task whose next_cron_fire is in the past must be due (missed-trigger catch-up)"
        );
    }

    // ── Diagnostics surface (impl-recurring-heartbeat-diagnostics) ──────
    //
    // `describe_cron` resolves the *actual* weekday/time a cron expression
    // fires, making the `cron` crate's non-standard 1=Sunday mapping
    // visible so a user who writes `1` for "Monday" sees "Sunday" before
    // the weekly trigger fires on the wrong day.
    #[test]
    fn describe_cron_names_actual_weekday_for_dow_1() {
        // dow=1 fires on Sunday in this crate (NOT Monday). The description
        // MUST name Sunday so the wrong-day bug is visible at add time.
        let desc = describe_cron("0 0 9 * * 1").expect("parses");
        assert!(
            desc.summary.contains("Sun"),
            "summary must name Sunday for dow=1, got: {}",
            desc.summary
        );
        assert!(
            desc.summary.contains("09:00"),
            "summary must name 09:00 UTC, got: {}",
            desc.summary
        );
        assert!(desc.has_dow_field, "dow=1 is a user-specified weekday");
        let weekdays = desc.weekdays.expect("weekdays resolved");
        assert_eq!(weekdays, vec!["Sunday".to_string()]);
        assert_eq!(desc.time_utc.as_deref(), Some("09:00"));
    }

    #[test]
    fn describe_cron_names_monday_for_dow_2() {
        // dow=2 is Monday in this crate — the value a user MUST use to fire
        // on Monday.
        let desc = describe_cron("0 0 9 * * 2").expect("parses");
        assert!(
            desc.summary.contains("Mon"),
            "summary must name Monday for dow=2, got: {}",
            desc.summary
        );
        assert_eq!(desc.weekdays.as_deref(), Some(&["Monday".to_string()][..]));
    }

    #[test]
    fn describe_cron_weekday_range_resolves_all_days() {
        // dow=1-5 in this crate is Sun..Thu (1=Sun..5=Thu). The description
        // must list all five, not just one.
        let desc = describe_cron("0 0 9 * * 1-5").expect("parses");
        let weekdays = desc.weekdays.expect("weekdays resolved");
        assert_eq!(
            weekdays,
            vec![
                "Sunday".to_string(),
                "Monday".to_string(),
                "Tuesday".to_string(),
                "Wednesday".to_string(),
                "Thursday".to_string(),
            ]
        );
    }

    #[test]
    fn describe_cron_no_dow_field_is_daily() {
        // No dow constraint → fires every day, no warning.
        let desc = describe_cron("0 0 2 * * *").expect("parses");
        assert!(!desc.has_dow_field, "no dow field for daily cron");
        assert!(desc.weekdays.is_none(), "daily cron has no weekday list");
        assert!(
            desc.summary.contains("daily"),
            "summary must say daily, got: {}",
            desc.summary
        );
    }

    #[test]
    fn describe_cron_invalid_returns_none() {
        assert!(describe_cron("not a cron").is_none());
    }

    // ── missed-fire counting (impl-recurring-heartbeat-diagnostics) ─────
    //
    // `missed_fires_before_reset` counts scheduled windows the daemon was
    // down across, EXCLUDING the one being caught up by the current reset.
    // It powers the `cron_fire_missed` audit entry in `reset_cron_task` and
    // the `missed` column in `wg cron doctor`.
    #[test]
    fn missed_fires_zero_when_on_time() {
        // last_cron_fire = yesterday 09:00; now = today 09:05 (just fired on
        // time). One scheduled window occurred (today 09:00), being caught
        // up now → missed = 0.
        let last = Utc.with_ymd_and_hms(2024, 1, 7, 9, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2024, 1, 8, 9, 5, 0).unwrap();
        let task = Task {
            id: "daily".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            last_cron_fire: Some(last.to_rfc3339()),
            ..Default::default()
        };
        assert_eq!(missed_fires_before_reset(&task, now), Some(0));
    }

    #[test]
    fn missed_fires_counts_downtime_windows_for_daily_cron() {
        // Daily cron; daemon down for 3 days. last_cron_fire = Jan 7 09:00,
        // now = Jan 10 09:05. Scheduled windows at Jan 8/9/10 09:00 = 3.
        // The Jan 10 window is being caught up now → missed = 2.
        let last = Utc.with_ymd_and_hms(2024, 1, 7, 9, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2024, 1, 10, 9, 5, 0).unwrap();
        let task = Task {
            id: "daily-down".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            last_cron_fire: Some(last.to_rfc3339()),
            ..Default::default()
        };
        assert_eq!(missed_fires_before_reset(&task, now), Some(2));
    }

    #[test]
    fn missed_fires_none_without_last_cron_fire() {
        // First-ever reset: no last_cron_fire → cannot compute.
        let now = Utc.with_ymd_and_hms(2024, 1, 10, 9, 5, 0).unwrap();
        let task = Task {
            id: "first".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            last_cron_fire: None,
            ..Default::default()
        };
        assert_eq!(missed_fires_before_reset(&task, now), None);
    }

    #[test]
    fn reset_cron_task_logs_missed_fire_audit_on_catchup() {
        // A Done cron task whose last_cron_fire is multiple daily-windows
        // behind now must, on reset, append a `cron_fire_missed` LogEntry
        // naming the count. We use a 5-day-stale last_cron_fire so real-now
        // is several daily windows ahead — guaranteeing missed >= 1.
        let stale = (Utc::now() - chrono::Duration::days(5)).to_rfc3339();
        let mut task = Task {
            id: "catchup-audit".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            last_cron_fire: Some(stale.clone()),
            ..Default::default()
        };
        let log_len_before = task.log.len();
        assert!(reset_cron_task(&mut task), "reset should apply");
        assert!(
            task.log.len() > log_len_before,
            "a missed-fire audit entry must be appended on catch-up reset"
        );
        let audit = task.log.last().expect("audit entry present");
        assert_eq!(audit.actor.as_deref(), Some("cron"));
        assert!(
            audit.message.contains("cron_fire_missed"),
            "audit message must start with cron_fire_missed: {}",
            audit.message
        );
    }

    #[test]
    fn reset_cron_task_no_audit_on_fresh_first_reset() {
        // First-ever reset (no last_cron_fire) must NOT log a missed-fire
        // audit — there is no prior window to have missed.
        let mut task = Task {
            id: "first-reset".to_string(),
            status: crate::graph::Status::Done,
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            last_cron_fire: None,
            ..Default::default()
        };
        let log_len_before = task.log.len();
        assert!(reset_cron_task(&mut task));
        assert_eq!(
            task.log.len(),
            log_len_before,
            "first-ever reset must not log a missed-fire audit"
        );
    }

    #[test]
    fn overdue_secs_none_when_not_yet_due() {
        let future = (Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        let task = Task {
            id: "future".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            next_cron_fire: Some(future),
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            ..Default::default()
        };
        assert!(overdue_secs(&task, Utc::now()).is_none());
    }

    #[test]
    fn overdue_secs_some_when_past_due() {
        let past = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let task = Task {
            id: "past".to_string(),
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            next_cron_fire: Some(past),
            cron_template: false,
            cron_instance_of: None,
            origin: None,
            ..Default::default()
        };
        let overdue = overdue_secs(&task, Utc::now()).expect("past due");
        assert!(
            overdue >= 7100 && overdue <= 7300,
            "overdue ~= 2h, got {}",
            overdue
        );
    }

    #[test]
    fn format_countdown_future_and_past() {
        let now = Utc::now();
        let future = (now + chrono::Duration::hours(2)).to_rfc3339();
        let past = (now - chrono::Duration::minutes(5)).to_rfc3339();
        let f = format_countdown(&future, now);
        assert!(f.starts_with("in "), "future countdown: {}", f);
        let p = format_countdown(&past, now);
        assert!(p.starts_with("ago "), "past countdown: {}", p);
        // Invalid timestamp → empty string (no noisy spam).
        assert_eq!(format_countdown("not-a-ts", now), "");
    }

    // ── Cron template → instance model (cron-re-registration) ────────────

    // `Node` and `WorkGraph` are already in scope via `use super::*`; only
    // `Status` needs importing (cron.rs refers to it fully-qualified).
    use crate::graph::Status;

    /// Build a weekly (Sunday 20:30 UTC) cron *template* whose next fire is in
    /// the past (i.e. currently due), mirroring `weekly-plan-sunday`.
    fn due_weekly_template(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: "Weekly plan".to_string(),
            status: Status::Open,
            cron_enabled: true,
            cron_template: true,
            cron_schedule: Some("0 30 20 * * 1".to_string()), // dow 1 = Sunday
            next_cron_fire: Some((Utc::now() - Duration::minutes(5)).to_rfc3339()),
            ..Default::default()
        }
    }

    #[test]
    fn cron_period_suffix_weekly_is_iso_week() {
        let schedule = parse_cron_expression("0 30 20 * * 1").unwrap();
        // 2026-07-12 is a Sunday in ISO week 28.
        let fire = Utc.with_ymd_and_hms(2026, 7, 12, 20, 30, 0).unwrap();
        let suffix = cron_period_suffix(&schedule, fire);
        assert_eq!(suffix, "2026-W28", "weekly cron → ISO year-week suffix");
    }

    #[test]
    fn cron_period_suffix_daily_is_date() {
        let schedule = parse_cron_expression("0 0 2 * * *").unwrap();
        let fire = Utc.with_ymd_and_hms(2026, 7, 12, 2, 0, 0).unwrap();
        assert_eq!(cron_period_suffix(&schedule, fire), "2026-07-12");
    }

    #[test]
    fn cron_period_suffix_subdaily_includes_time() {
        let schedule = parse_cron_expression("0 */5 * * * *").unwrap(); // every 5 min
        let fire = Utc.with_ymd_and_hms(2026, 7, 12, 2, 30, 0).unwrap();
        assert_eq!(cron_period_suffix(&schedule, fire), "2026-07-12-0230");
    }

    #[test]
    fn cron_mint_instance_is_distinct_open_non_cron_task() {
        let template = due_weekly_template("weekly-plan-sunday");
        let fire = Utc.with_ymd_and_hms(2026, 7, 12, 20, 30, 0).unwrap();
        let inst = mint_cron_instance(&template, fire).expect("should mint");

        // Distinct, readable id bound to the run.
        assert_eq!(inst.id, "weekly-plan-sunday-2026-W28");
        assert_ne!(inst.id, template.id, "instance id must differ from template");
        // A plain, dispatchable Open task — NOT itself a cron.
        assert_eq!(inst.status, Status::Open);
        assert!(!inst.cron_enabled, "instance is not cron-enabled");
        assert!(!inst.cron_template, "instance is not a template");
        assert!(inst.cron_schedule.is_none());
        assert!(inst.next_cron_fire.is_none());
        assert_eq!(inst.cron_instance_of.as_deref(), Some("weekly-plan-sunday"));
        // Stands alone: no inherited edges, fresh assignment/bookkeeping.
        assert!(inst.after.is_empty() && inst.before.is_empty());
        assert!(inst.assigned.is_none());
        assert_eq!(inst.dispatch_count, 0);
        // Carries the template's definition.
        assert_eq!(inst.title, template.title);
    }

    #[test]
    fn cron_template_is_never_time_ready() {
        // A due template must NOT be dispatched directly (only its instances are).
        let template = due_weekly_template("weekly-plan-sunday");
        assert!(
            !crate::query::is_time_ready(&template),
            "cron template must never be time-ready"
        );

        // A legacy (non-template) due cron IS ready — backward compatibility.
        let mut legacy = template.clone();
        legacy.cron_template = false;
        assert!(
            crate::query::is_time_ready(&legacy),
            "legacy due cron stays ready (unchanged behavior)"
        );
    }

    #[test]
    fn cron_advance_template_stays_open_and_advances() {
        let mut template = due_weekly_template("weekly-plan-sunday");
        let before = template.next_cron_fire.clone();
        assert!(advance_cron_template(&mut template, Utc::now()));
        assert_eq!(template.status, Status::Open, "template stays Open/dormant");
        assert!(template.assigned.is_none());
        assert!(template.last_cron_fire.is_some());
        assert_ne!(
            template.next_cron_fire, before,
            "next fire advanced to the future"
        );
        let next: DateTime<Utc> = template.next_cron_fire.unwrap().parse().unwrap();
        assert!(next > Utc::now(), "next fire is in the future");
    }

    #[test]
    fn cron_mint_due_instances_is_idempotent_per_period() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(due_weekly_template("weekly-plan-sunday")));

        // First tick fires: exactly one instance minted, template advanced.
        let minted = mint_due_cron_instances(&mut graph, Utc::now());
        assert_eq!(minted.len(), 1, "one instance minted for the due period");
        let inst_id = minted[0].clone();
        assert!(graph.get_task(&inst_id).is_some());
        // Template is no longer due (next fire advanced to the future).
        assert!(!is_cron_due(graph.get_task("weekly-plan-sunday").unwrap(), Utc::now()));

        // Second tick with the template not due: nothing new minted.
        let again = mint_due_cron_instances(&mut graph, Utc::now());
        assert!(again.is_empty(), "no duplicate instance while not due");
        let instance_count = graph
            .tasks()
            .filter(|t| t.cron_instance_of.as_deref() == Some("weekly-plan-sunday"))
            .count();
        assert_eq!(instance_count, 1, "still exactly one instance");
    }

    /// The exact tonight deadlock, as a regression test.
    ///
    /// Old behavior (reset-in-place, reproduced by `reset_cron_task`): a child
    /// created `--after weekly-plan-sunday` re-blocks the moment the completed
    /// cron flips Done→Open — its finished work strands. The template/instance
    /// model must NOT do this: children bind to a distinct run id, so the next
    /// firing (a new id) never re-blocks them.
    #[test]
    fn cron_tonight_scenario_reregistration_does_not_reblock_children() {
        // Two consecutive Sundays (distinct ISO weeks W28 / W29) so each firing
        // yields a distinct instance id — the exact tonight timeline.
        let week1_fire = Utc.with_ymd_and_hms(2026, 7, 12, 20, 30, 0).unwrap();
        let week2_fire = Utc.with_ymd_and_hms(2026, 7, 19, 20, 30, 0).unwrap();

        let mut graph = WorkGraph::new();
        let mut template = due_weekly_template("weekly-plan-sunday");
        template.next_cron_fire = Some(week1_fire.to_rfc3339());
        graph.add_node(Node::Task(template));

        // Week 1 fires → a distinct instance is minted and dispatched.
        let minted = mint_due_cron_instances(&mut graph, week1_fire + Duration::minutes(5));
        assert_eq!(minted.len(), 1);
        let run1 = minted[0].clone();
        assert_eq!(run1, "weekly-plan-sunday-2026-W28");

        // The run's agent creates a child `--after <run1>` and finishes it; the
        // run itself completes (stays Done forever — a finished run, not the
        // recurring definition).
        let child = Task {
            id: "plan-child".to_string(),
            title: "downstream plan work".to_string(),
            status: Status::Done,
            after: vec![run1.clone()],
            ..Default::default()
        };
        graph.add_node(Node::Task(child));
        graph.get_task_mut(&run1).unwrap().status = Status::Done;

        // Nothing blocks the child now (its blocker run1 is Done).
        assert!(
            crate::query::after(&graph, "plan-child").is_empty(),
            "child not blocked after its run completes"
        );

        // NEXT week fires: force the template due again and tick. A NEW instance
        // id is minted — the re-registration.
        graph.get_task_mut("weekly-plan-sunday").unwrap().next_cron_fire =
            Some(week2_fire.to_rfc3339());
        let minted2 = mint_due_cron_instances(&mut graph, week2_fire + Duration::minutes(5));
        assert_eq!(minted2.len(), 1, "next week mints a fresh instance");
        let run2 = minted2[0].clone();
        assert_eq!(run2, "weekly-plan-sunday-2026-W29");
        assert_ne!(run1, run2, "re-registration uses a DISTINCT id, not the same one");

        // THE FIX: the child is STILL not blocked. Its `--after run1` edge binds
        // to the finished run, so the new firing (run2) cannot re-block it —
        // `wg done` on the child would not refuse.
        assert!(
            crate::query::after(&graph, "plan-child").is_empty(),
            "re-registration must NOT re-block the already-finished child"
        );
        assert_eq!(
            graph.get_task(&run1).unwrap().status,
            Status::Done,
            "the finished run stays Done — never flipped back to Open"
        );
    }

    /// Documents the OLD bug the fix avoids: reset-in-place re-blocks children.
    /// This is the failure mode template mode replaces (kept for legacy crons).
    #[test]
    fn cron_legacy_reset_in_place_reblocks_children_the_old_bug() {
        let mut graph = WorkGraph::new();
        let mut legacy = due_weekly_template("legacy-weekly");
        legacy.cron_template = false; // legacy in-place cron
        legacy.status = Status::Done; // just completed a run
        legacy.next_cron_fire = None;
        graph.add_node(Node::Task(legacy));

        let child = Task {
            id: "legacy-child".to_string(),
            title: "child".to_string(),
            status: Status::Done,
            after: vec!["legacy-weekly".to_string()],
            ..Default::default()
        };
        graph.add_node(Node::Task(child));

        // Before reset: child unblocked (blocker Done).
        assert!(crate::query::after(&graph, "legacy-child").is_empty());

        // Reset-in-place flips the SAME id Done→Open → child re-blocks. This is
        // exactly the deadlock; the template model above does not exhibit it.
        reset_cron_task(graph.get_task_mut("legacy-weekly").unwrap());
        assert_eq!(graph.get_task("legacy-weekly").unwrap().status, Status::Open);
        assert!(
            !crate::query::after(&graph, "legacy-child").is_empty(),
            "legacy reset re-blocks the child — the bug template mode fixes"
        );
    }

    /// FIX 1 (cron-fanout orphaning post-mortem, 2026-07-19): a legacy
    /// (reset-in-place) cron fires, a child is chained `--after <cron-id>` during
    /// the run, the cron RESCHEDULES to its next period — and the finished child
    /// must become READY, not re-block behind the next firing. This is the direct
    /// cause of tonight's ~1h W30 stall (children of `weekly-plan-sunday`
    /// re-blocked until NEXT Sunday). `reset_due_legacy_crons` reschedules the
    /// cron AND drops the stale edge from dependents of the completed run.
    #[test]
    fn cron_reschedule_satisfies_in_flight_dependents_fanout_fix() {
        let mut graph = WorkGraph::new();

        // A weekly legacy cron that just completed a run at `run_done`.
        let run_done = Utc::now() - Duration::minutes(30);
        let mut legacy = due_weekly_template("weekly-plan-sunday");
        legacy.cron_template = false; // legacy reset-in-place cron (the prod one)
        legacy.status = Status::Done; // its run just completed
        legacy.completed_at = Some(run_done.to_rfc3339());
        legacy.next_cron_fire = None;
        graph.add_node(Node::Task(legacy));

        // A child chained on the cron id DURING the run (created before the run
        // completed). It is still Open — real downstream fanout work.
        let child = Task {
            id: "nora-plan-child".to_string(),
            title: "downstream plan work".to_string(),
            status: Status::Open,
            created_at: Some((run_done - Duration::minutes(10)).to_rfc3339()),
            after: vec!["weekly-plan-sunday".to_string()],
            ..Default::default()
        };
        graph.add_node(Node::Task(child));

        // A child created AFTER this run completed: it legitimately waits for the
        // NEXT firing and must KEEP its edge (proves we don't over-satisfy).
        let future_child = Task {
            id: "next-week-child".to_string(),
            title: "waits for next week".to_string(),
            status: Status::Open,
            created_at: Some((run_done + Duration::minutes(5)).to_rfc3339()),
            after: vec!["weekly-plan-sunday".to_string()],
            ..Default::default()
        };
        graph.add_node(Node::Task(future_child));

        // Tick: reschedule the legacy cron + satisfy in-flight dependents.
        let (reset_ids, satisfied) = reset_due_legacy_crons(&mut graph, Utc::now());

        // The cron rescheduled to its next period (Open, next fire in the future).
        assert_eq!(reset_ids, vec!["weekly-plan-sunday".to_string()]);
        let cron = graph.get_task("weekly-plan-sunday").unwrap();
        assert_eq!(cron.status, Status::Open, "cron rescheduled to Open");
        let next: DateTime<Utc> = cron.next_cron_fire.clone().unwrap().parse().unwrap();
        assert!(next > Utc::now(), "next fire advanced to the future");

        // THE FIX: the in-flight child is READY (its stale edge was dropped),
        // even though the cron flipped back to Open for its next firing.
        assert_eq!(satisfied, vec!["nora-plan-child".to_string()]);
        assert!(
            crate::query::after(&graph, "nora-plan-child").is_empty(),
            "rescheduled cron must NOT re-block the in-flight child (the fanout fix)"
        );
        assert!(
            !graph
                .get_task("nora-plan-child")
                .unwrap()
                .after
                .iter()
                .any(|a| a == "weekly-plan-sunday"),
            "stale cron edge dropped from the finished-run child"
        );

        // The next-week child KEEPS its edge and stays blocked — it waits for the
        // firing it was actually created for.
        assert!(
            !crate::query::after(&graph, "next-week-child").is_empty(),
            "a child created after the run must still wait for the next firing"
        );
    }

    fn daily_cron_task(id: &str, status: Status) -> Task {
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        Task {
            id: id.to_string(),
            status,
            cron_enabled: true,
            cron_schedule: Some("0 0 9 * * *".to_string()),
            next_cron_fire: Some(past),
            ..Default::default()
        }
    }

    #[test]
    fn cron_will_not_fire_flags_abandoned_and_failed() {
        // The abandoned `daily-digest` smoke-test cron is time-due yet the
        // coordinator will never dispatch a terminal-dead task — it must be
        // flagged so `wg cron` can surface it loudly.
        assert_eq!(
            cron_will_not_fire_reason(&daily_cron_task("dead", Status::Abandoned)),
            Some(CronStuckReason::Abandoned)
        );
        assert_eq!(
            cron_will_not_fire_reason(&daily_cron_task("boom", Status::Failed)),
            Some(CronStuckReason::Failed)
        );
        assert_eq!(
            cron_will_not_fire_reason(&daily_cron_task("boom2", Status::FailedPendingEval)),
            Some(CronStuckReason::Failed)
        );
    }

    #[test]
    fn cron_will_not_fire_flags_paused_over_status() {
        let mut t = daily_cron_task("napping", Status::Open);
        t.paused = true;
        assert_eq!(
            cron_will_not_fire_reason(&t),
            Some(CronStuckReason::Paused),
            "a paused cron never fires regardless of an otherwise-healthy Open status"
        );
    }

    #[test]
    fn cron_will_not_fire_none_for_dispatchable_and_transient() {
        // Open / Incomplete are dispatchable; InProgress / Done (awaiting cron
        // reset) / Blocked / Waiting are transient and carry their own
        // blocking_state — none of these are the "will never fire" condition.
        for status in [
            Status::Open,
            Status::Incomplete,
            Status::InProgress,
            Status::Done,
            Status::Blocked,
            Status::Waiting,
            Status::PendingValidation,
        ] {
            assert_eq!(
                cron_will_not_fire_reason(&daily_cron_task("live", status)),
                None,
                "status {:?} should not be flagged as will-not-fire",
                status
            );
        }
    }

    #[test]
    fn cron_will_not_fire_none_for_template() {
        // A cron *template* is never dispatched itself (it mints instances), so
        // it is not "stuck" even in a non-Open status.
        let mut t = daily_cron_task("tmpl", Status::Done);
        t.cron_template = true;
        assert_eq!(cron_will_not_fire_reason(&t), None);
    }

    #[test]
    fn cron_will_not_fire_none_for_non_cron_task() {
        let t = Task {
            id: "plain".to_string(),
            status: Status::Abandoned,
            cron_enabled: false,
            ..Default::default()
        };
        assert_eq!(cron_will_not_fire_reason(&t), None);
    }
}
