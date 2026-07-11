//! Ingest a completed disposable's durable outputs into its spawner's
//! persistent session memory.
//!
//! A **disposable** (`docs/14-disposable-lifecycle.md`) is spawn-and-discard:
//! its worktree, transcript, and process are thrown away, and its *only*
//! durable value is the artifact(s) it records plus the `wg log` breadcrumb(s)
//! it leaves — a contract the `wg done` gate guarantees are present before a
//! disposable may complete.
//!
//! This module is the **middle path** for disposable persistence (iteration-2
//! directive 6b): when a disposable completes, its artifact + breadcrumbs are
//! folded into the *spawning* named agent's persistent `session-summary.md`,
//! riding the #50 agent↔session binding. So Bruno's recipe-scrape disposable
//! stays ephemeral, but its *result* persists into Bruno's memory and is
//! injected into Bruno's next task via `{{bound_session_summary}}` — the same
//! path `bind-named-agents` proved for direct recall (`docs/09`).
//!
//! The link from disposable → spawner is a `spawned-by:<agent>` tag
//! ([`crate::graph::SPAWNED_BY_TAG_PREFIX`]) written at `wg add` time; the
//! ingest resolves that agent's bound session via
//! [`crate::chat_sessions::session_for_agent`] and appends to its
//! `session-summary.md`. Ingest is **idempotent**: each disposable folds in at
//! most once, keyed by a per-task HTML-comment marker, so re-running `wg done`
//! never double-appends.

use crate::chat_sessions::{chat_dir_for_uuid, session_for_agent};
use crate::graph::Task;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Outcome of an ingest attempt, for callers that want to log/report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    /// The spawner's `session-summary.md` that was (or already was) updated.
    pub summary_path: PathBuf,
    /// The spawner agent (content-hash / id) the disposable was folded into.
    pub spawner: String,
    /// True when the disposable had already been ingested (idempotent no-op).
    pub already_present: bool,
}

/// The per-disposable idempotency marker embedded in the ingest block.
fn ingest_marker(task_id: &str) -> String {
    format!("<!-- disposable-ingest:{} -->", task_id)
}

/// Ingest a completed disposable's durable outputs into its spawner's bound
/// session summary.
///
/// Returns:
/// - `Ok(None)` when there is nothing to do — the task is not a disposable, it
///   records no spawner (`spawned-by:` tag absent), or the spawner has no bound
///   session to fold memory into. All three are benign: a disposable without a
///   named spawner simply has no memory to persist into.
/// - `Ok(Some(report))` when the spawner's summary was updated (or already
///   contained this disposable, in which case `report.already_present` is true
///   and the file is left untouched).
///
/// The caller is expected to invoke this only *after* the disposable has been
/// promoted to `Done` (its artifact + breadcrumb contract is by then
/// guaranteed by the `wg done` disposable gate), but the function is safe to
/// call at any time — it simply reads the task's current artifacts and
/// breadcrumbs.
pub fn ingest_disposable_into_spawner(
    workgraph_dir: &Path,
    task: &Task,
) -> Result<Option<IngestReport>> {
    if !task.is_disposable() {
        return Ok(None);
    }
    let spawner = match task.spawned_by() {
        Some(s) => s,
        None => return Ok(None),
    };
    // Resolve the spawner's bound session (#50). No binding ⇒ no persistent
    // memory to fold into; that is a benign no-op, not an error.
    let uuid = match session_for_agent(workgraph_dir, spawner) {
        Some(u) => u,
        None => return Ok(None),
    };
    let summary_path = chat_dir_for_uuid(workgraph_dir, &uuid).join("session-summary.md");

    let marker = ingest_marker(&task.id);
    let existing = std::fs::read_to_string(&summary_path).unwrap_or_default();
    if existing.contains(&marker) {
        // Already ingested — idempotent no-op, leave the file untouched.
        return Ok(Some(IngestReport {
            summary_path,
            spawner: spawner.to_string(),
            already_present: true,
        }));
    }

    let block = render_ingest_block(task, &marker);
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&block);

    crate::executor::native::resume::store_session_summary(&summary_path, &updated)
        .with_context(|| {
            format!(
                "failed to ingest disposable '{}' into spawner session summary {}",
                task.id,
                summary_path.display()
            )
        })?;

    Ok(Some(IngestReport {
        summary_path,
        spawner: spawner.to_string(),
        already_present: false,
    }))
}

/// Render the markdown block folded into the spawner's session summary. The
/// block is written in the spawner's first-person "your own memory" voice
/// (matching `resolve_bound_session_summary`'s framing) and carries the
/// idempotency `marker` so a re-run of `wg done` can detect it.
fn render_ingest_block(task: &Task, marker: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n## Disposable result — {} ({})\n{}\n\nA disposable you spawned completed. \
         Its durable outputs are folded into your memory below.\n\n",
        task.title, task.id, marker
    ));

    out.push_str("**Artifacts:**\n");
    if task.artifacts.is_empty() {
        out.push_str("- (none recorded)\n");
    } else {
        for a in &task.artifacts {
            out.push_str(&format!("- {}\n", a));
        }
    }

    let breadcrumbs = task.agent_log_breadcrumbs();
    out.push_str("\n**What it found (wg log):**\n");
    if breadcrumbs.is_empty() {
        out.push_str("- (no breadcrumb)\n");
    } else {
        for b in breadcrumbs {
            out.push_str(&format!("- {}\n", b));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_sessions::{bind_agent, create_session, SessionKind};
    use crate::graph::{LogEntry, Task};
    use tempfile::tempdir;

    /// Build a disposable task spawned by `spawner`, with one artifact and one
    /// agent breadcrumb — the minimum a disposable that passed the `wg done`
    /// contract gate carries.
    fn disposable_spawned_by(id: &str, spawner: &str) -> Task {
        let mut task = Task {
            id: id.to_string(),
            title: "Scrape 3 chicken recipes".to_string(),
            tags: vec![
                "disposable".to_string(),
                format!("spawned-by:{}", spawner),
            ],
            artifacts: vec!["docs/artifacts/chicken-recipes.md".to_string()],
            ..Task::default()
        };
        // An agent-authored breadcrumb (actor = None).
        task.log.push(LogEntry {
            timestamp: "2026-07-11T00:00:00Z".to_string(),
            actor: None,
            user: Some("bruno".to_string()),
            message: "Found 3 recipes; lemon-garlic is the family favourite.".to_string(),
        });
        task
    }

    /// The named test written first: a completed disposable's artifact string
    /// is folded into its spawner's bound `session-summary.md`, so the spawner
    /// recalls it on its next task.
    #[test]
    fn test_disposable_artifact_ingested_into_spawner_session() {
        let dir = tempdir().unwrap();
        let wg = dir.path();

        // Bind "bruno" (agent id `bruno-agent`) to a persistent session, the
        // #50 binding this feature rides.
        let uuid = create_session(wg, SessionKind::Other, &["bruno".into()], None).unwrap();
        bind_agent(wg, "bruno-agent", "bruno").unwrap();
        // Seed a prior-week decision so we prove we *append*, not clobber.
        let summary_path = chat_dir_for_uuid(wg, &uuid).join("session-summary.md");
        std::fs::write(&summary_path, "## Prior work\nWe chose a 50-50 pasta split.\n").unwrap();

        let task = disposable_spawned_by("scrape-recipes", "bruno-agent");

        let report = ingest_disposable_into_spawner(wg, &task)
            .unwrap()
            .expect("disposable with a bound spawner should ingest");
        assert!(!report.already_present, "first ingest should write");
        assert_eq!(report.spawner, "bruno-agent");
        assert_eq!(report.summary_path, summary_path);

        // The artifact string now lives in the spawner's memory, alongside the
        // pre-existing decision (append, not clobber).
        let summary = std::fs::read_to_string(&summary_path).unwrap();
        assert!(
            summary.contains("docs/artifacts/chicken-recipes.md"),
            "spawner summary must contain the disposable's artifact string, got:\n{summary}"
        );
        assert!(
            summary.contains("lemon-garlic is the family favourite"),
            "spawner summary must contain the disposable's breadcrumb finding"
        );
        assert!(
            summary.contains("50-50 pasta split"),
            "ingest must append, not overwrite prior memory"
        );

        // And it is exactly what the spawn path injects as bound memory.
        let injected =
            crate::chat_sessions::agent_session_summary(wg, "bruno-agent").expect("memory present");
        assert!(
            injected.contains("docs/artifacts/chicken-recipes.md"),
            "the disposable's artifact must appear in the spawner's NEXT-task injected memory"
        );
    }

    #[test]
    fn ingest_is_idempotent() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        let uuid = create_session(wg, SessionKind::Other, &["bruno".into()], None).unwrap();
        bind_agent(wg, "bruno-agent", "bruno").unwrap();
        let summary_path = chat_dir_for_uuid(wg, &uuid).join("session-summary.md");

        let task = disposable_spawned_by("scrape-recipes", "bruno-agent");
        ingest_disposable_into_spawner(wg, &task).unwrap().unwrap();
        let after_first = std::fs::read_to_string(&summary_path).unwrap();

        let report = ingest_disposable_into_spawner(wg, &task).unwrap().unwrap();
        assert!(report.already_present, "second ingest must be a no-op");
        let after_second = std::fs::read_to_string(&summary_path).unwrap();
        assert_eq!(
            after_first, after_second,
            "re-running ingest must not double-append"
        );
    }

    #[test]
    fn non_disposable_is_not_ingested() {
        let dir = tempdir().unwrap();
        let wg = dir.path();
        create_session(wg, SessionKind::Other, &["bruno".into()], None).unwrap();
        bind_agent(wg, "bruno-agent", "bruno").unwrap();

        let mut task = disposable_spawned_by("ordinary", "bruno-agent");
        task.tags.retain(|t| t != "disposable"); // strip the disposable tag
        assert!(
            ingest_disposable_into_spawner(wg, &task).unwrap().is_none(),
            "a non-disposable task must never be ingested"
        );
    }

    #[test]
    fn disposable_without_spawner_or_binding_is_noop() {
        let dir = tempdir().unwrap();
        let wg = dir.path();

        // No spawned-by tag → nothing to ingest into.
        let mut orphan = disposable_spawned_by("orphan", "bruno-agent");
        orphan.tags.retain(|t| !t.starts_with("spawned-by:"));
        assert!(ingest_disposable_into_spawner(wg, &orphan).unwrap().is_none());

        // Spawner named but not bound to any session → benign no-op.
        let unbound = disposable_spawned_by("unbound", "nobody-agent");
        assert!(ingest_disposable_into_spawner(wg, &unbound).unwrap().is_none());
    }
}
