//! Durable, route-stable evaluation lifecycle primitives.
//!
//! Agency satellites are part of the evaluation gate, not ordinary work.  This
//! module gives each source attempt one pipeline identity, persists the exact
//! handler-first routes selected at scaffold time, and records semantic verdicts
//! before any graph transition.  Dispatcher reconciliation can therefore link
//! and consume a verdict idempotently after a crash without invoking a model
//! again.

use crate::agency::Evaluation;
use crate::config::{
    Config, DispatchRole, ExecutionSystemKey, ReasoningLevel, execution_system_key,
};
use crate::graph::{LogEntry, Status, Task, WorkGraph};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const AGENCY_PLAN_SCHEMA: u16 = 1;
pub const EVAL_LIFECYCLE_SCHEMA: u16 = 1;
/// Schema 1 hashed an in-memory `Evaluation`. Its `HashMap` field order was
/// process-random, so the durable JSON could deserialize to different bytes.
/// Schema 2 pins the exact durable evaluation file instead.
pub const EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA: u16 = 2;
pub const MAX_EXECUTION_ATTEMPTS_PER_ROUTE_GENERATION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgencyStage {
    FlipInference,
    FlipComparison,
    Evaluate,
}

impl AgencyStage {
    pub fn role(self) -> DispatchRole {
        match self {
            Self::FlipInference => DispatchRole::FlipInference,
            Self::FlipComparison => DispatchRole::FlipComparison,
            Self::Evaluate => DispatchRole::Evaluator,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DispatchSelectionSource {
    ScaffoldConfig,
    PersistedPlan,
    LegacyHandlerFirst,
    LegacyCodexSplit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgencyCallPlan {
    pub stage: AgencyStage,
    /// Canonical handler-first route. Invocation must never reconstruct this
    /// value from the compatibility `Task.model` / `Task.provider` mirrors.
    pub route: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningLevel>,
    pub system: ExecutionSystemKey,
    pub source: DispatchSelectionSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgencyDispatchPlan {
    pub schema: u16,
    pub pipeline_id: String,
    pub source_task: String,
    pub source_attempt: u32,
    pub task_id: String,
    pub calls: Vec<AgencyCallPlan>,
    pub plan_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EvaluationExecutionState {
    #[default]
    Ready,
    Claimed,
    Waiting,
    Blocked,
    VerdictDurable,
    Consumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationLifecycle {
    pub schema: u16,
    pub pipeline_id: String,
    pub source_attempt: u32,
    #[serde(default)]
    pub route_generation: u32,
    #[serde(default)]
    pub schedule_attempts: u32,
    #[serde(default)]
    pub transport_attempts: u32,
    #[serde(default)]
    pub semantic_attempts: u32,
    #[serde(default)]
    pub execution_state: EvaluationExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_flip_verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_eval_verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_verdict: Option<String>,
    #[serde(default)]
    pub repair_version: u16,
}

impl EvaluationLifecycle {
    pub fn for_source(task: &Task) -> Self {
        // A low-score in-place rescue is a new semantic source attempt even
        // though it intentionally preserves the worker identity/worktree and
        // therefore does not increment `retry_count`.
        let source_attempt = task
            .retry_count
            .saturating_add(task.rescue_count)
            .saturating_add(1);
        Self {
            schema: EVAL_LIFECYCLE_SCHEMA,
            pipeline_id: pipeline_id(&task.id, source_attempt, task.loop_iteration),
            source_attempt,
            route_generation: 0,
            schedule_attempts: 0,
            transport_attempts: 0,
            semantic_attempts: 0,
            execution_state: EvaluationExecutionState::Ready,
            linked_flip_verdict: None,
            linked_eval_verdict: None,
            consumed_verdict: None,
            repair_version: 0,
        }
    }

    /// Reserve one claimed transport run within this immutable route
    /// generation. The caller performs this while atomically claiming the task.
    pub fn reserve_transport_attempt(&mut self) -> Result<u32> {
        if self.transport_attempts >= MAX_EXECUTION_ATTEMPTS_PER_ROUTE_GENERATION {
            self.execution_state = EvaluationExecutionState::Blocked;
            anyhow::bail!(
                "error[WG-EXEC-AGENCY-EXECUTION-EXHAUSTED]: {} claimed transport attempts exhausted",
                self.transport_attempts
            );
        }
        self.transport_attempts = self.transport_attempts.saturating_add(1);
        self.execution_state = EvaluationExecutionState::Claimed;
        Ok(self.transport_attempts)
    }
}

/// Ensure a source entering a soft evaluation state has the lifecycle identity
/// for its current worker/rescue attempt. A consumed prior attempt is retained
/// until the source actually completes again, then replaced atomically here.
pub fn refresh_source_lifecycle(task: &mut Task) {
    let expected = EvaluationLifecycle::for_source(task);
    if task
        .evaluation_lifecycle
        .as_ref()
        .is_none_or(|current| current.pipeline_id != expected.pipeline_id)
    {
        task.evaluation_lifecycle = Some(expected);
    }
}

pub fn pipeline_id(source_task: &str, source_attempt: u32, loop_iteration: u32) -> String {
    let material = format!("wg-eval-v1\0{source_task}\0{source_attempt}\0{loop_iteration}");
    format!(
        "evalp-{}",
        &blake3::hash(material.as_bytes()).to_hex()[..24]
    )
}

pub fn stages_for_task(task_id: &str) -> Result<Vec<AgencyStage>> {
    if task_id.starts_with(".flip-") {
        Ok(vec![
            AgencyStage::FlipInference,
            AgencyStage::FlipComparison,
        ])
    } else if task_id.starts_with(".evaluate-") {
        Ok(vec![AgencyStage::Evaluate])
    } else {
        anyhow::bail!("{task_id:?} is not an evaluation lifecycle satellite")
    }
}

pub fn build_plan(
    config: &Config,
    source_task: &Task,
    task_id: &str,
    source: DispatchSelectionSource,
) -> Result<AgencyDispatchPlan> {
    let source_attempt = source_task
        .retry_count
        .saturating_add(source_task.rescue_count)
        .saturating_add(1);
    let mut calls = Vec::new();
    for stage in stages_for_task(task_id)? {
        let role = stage.role();
        let dispatch = crate::service::llm::resolve_agency_dispatch(config, role)
            .with_context(|| format!("selecting agency route for stage {stage:?}"))?;
        let system = execution_system_key(&dispatch.raw_spec)?;
        let fallbacks = config.execution.models_for(&dispatch.raw_spec).to_vec();
        validate_fallbacks(&system, &fallbacks)?;
        calls.push(AgencyCallPlan {
            stage,
            route: dispatch.raw_spec,
            endpoint: config
                .models
                .get_role(role)
                .and_then(|model| model.endpoint.clone()),
            reasoning: dispatch.reasoning,
            system,
            source,
            fallbacks,
        });
    }
    let mut plan = AgencyDispatchPlan {
        schema: AGENCY_PLAN_SCHEMA,
        pipeline_id: pipeline_id(&source_task.id, source_attempt, source_task.loop_iteration),
        source_task: source_task.id.clone(),
        source_attempt,
        task_id: task_id.to_string(),
        calls,
        plan_hash: String::new(),
    };
    plan.plan_hash = compute_plan_hash(&plan)?;
    validate_plan(&plan)?;
    Ok(plan)
}

/// Lossless migration for a historical satellite. Bare OpenRouter is
/// deliberately ambiguous (Pi vs Nex) and therefore fails closed.
pub fn migrate_legacy_plan(source_task: &Task, satellite: &Task) -> Result<AgencyDispatchPlan> {
    let raw = satellite
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("error[WG-EXEC-AGENCY-ROUTE-UNSELECTED]: no persisted route")
        })?;
    let (route, source) = match execution_system_key(raw) {
        Ok(_) => (raw.to_string(), DispatchSelectionSource::LegacyHandlerFirst),
        Err(_) if satellite.provider.as_deref() == Some("codex") => (
            format!("codex:{raw}"),
            DispatchSelectionSource::LegacyCodexSplit,
        ),
        Err(_) if satellite.provider.as_deref() == Some("openrouter") => anyhow::bail!(
            "error[WG-EXEC-AGENCY-ROUTE-AMBIGUOUS]: legacy provider=openrouter cannot identify pi versus nex"
        ),
        Err(error) => anyhow::bail!(
            "error[WG-EXEC-AGENCY-ROUTE-AMBIGUOUS]: historical route {raw:?} is not handler-first: {error}"
        ),
    };
    let system = execution_system_key(&route)?;
    let source_attempt = source_task
        .retry_count
        .saturating_add(source_task.rescue_count)
        .saturating_add(1);
    let calls = stages_for_task(&satellite.id)?
        .into_iter()
        .map(|stage| AgencyCallPlan {
            stage,
            route: route.clone(),
            endpoint: satellite.endpoint.clone(),
            reasoning: satellite.reasoning,
            system: system.clone(),
            source,
            fallbacks: Vec::new(),
        })
        .collect();
    let mut plan = AgencyDispatchPlan {
        schema: AGENCY_PLAN_SCHEMA,
        pipeline_id: pipeline_id(&source_task.id, source_attempt, source_task.loop_iteration),
        source_task: source_task.id.clone(),
        source_attempt,
        task_id: satellite.id.clone(),
        calls,
        plan_hash: String::new(),
    };
    plan.plan_hash = compute_plan_hash(&plan)?;
    validate_plan(&plan)?;
    Ok(plan)
}

pub fn validate_plan(plan: &AgencyDispatchPlan) -> Result<()> {
    if plan.schema != AGENCY_PLAN_SCHEMA {
        anyhow::bail!("unsupported agency plan schema {}", plan.schema);
    }
    if plan.calls.is_empty() {
        anyhow::bail!("agency plan contains no calls");
    }
    let expected_hash = compute_plan_hash(plan)?;
    if expected_hash != plan.plan_hash {
        anyhow::bail!(
            "error[WG-EXEC-AGENCY-PLAN-HASH]: stored={} computed={}",
            plan.plan_hash,
            expected_hash
        );
    }
    for call in &plan.calls {
        let actual = execution_system_key(&call.route)?;
        if actual != call.system {
            anyhow::bail!(
                "error[WG-EXEC-AGENCY-SYSTEM-MISMATCH]: route {:?} is {}, plan recorded {}",
                call.route,
                actual,
                call.system
            );
        }
        validate_fallbacks(&call.system, &call.fallbacks)?;
    }
    Ok(())
}

fn validate_fallbacks(primary: &ExecutionSystemKey, fallbacks: &[String]) -> Result<()> {
    for fallback in fallbacks {
        let system = execution_system_key(fallback)?;
        if &system != primary {
            anyhow::bail!(
                "error[WG-EXEC-FALLBACK-CROSS-SYSTEM]: primary={} fallback={fallback:?} fallback_system={system}",
                primary
            );
        }
    }
    Ok(())
}

fn compute_plan_hash(plan: &AgencyDispatchPlan) -> Result<String> {
    let mut material = plan.clone();
    material.plan_hash.clear();
    let bytes = serde_json::to_vec(&material)?;
    Ok(format!("b3:{}", blake3::hash(&bytes).to_hex()))
}

pub fn call<'a>(plan: &'a AgencyDispatchPlan, stage: AgencyStage) -> Result<&'a AgencyCallPlan> {
    validate_plan(plan)?;
    plan.calls
        .iter()
        .find(|call| call.stage == stage)
        .ok_or_else(|| anyhow::anyhow!("agency plan has no {stage:?} call"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableEvalVerdict {
    pub schema: u16,
    pub verdict_id: String,
    /// Digest of this verdict record with this field blank. The filename,
    /// record and separately persisted Evaluation are all verified on load.
    #[serde(default)]
    pub verdict_digest: String,
    pub evaluation_id: String,
    pub pipeline_id: String,
    pub source_task: String,
    pub source_attempt: u32,
    pub stage: AgencyStage,
    pub producer_run_id: String,
    pub score: f64,
    /// Digest scheme for `evaluation_digest`. Missing means the historical
    /// schema 1 compact-JSON digest; schema 2 hashes the durable file bytes.
    #[serde(
        default = "legacy_evaluation_digest_schema",
        skip_serializing_if = "is_legacy_evaluation_digest_schema"
    )]
    pub evaluation_digest_schema: u16,
    pub evaluation_digest: String,
    pub created_at: String,
}

fn legacy_evaluation_digest_schema() -> u16 {
    1
}

fn is_legacy_evaluation_digest_schema(schema: &u16) -> bool {
    *schema == 1
}

pub fn verdicts_dir(dir: &Path) -> PathBuf {
    dir.join("agency").join("eval-lifecycle").join("verdicts")
}

/// Persist semantic evidence create-if-absent. Replaying the same verdict is a
/// no-op; a different body at the same id is corruption and fails closed.
pub fn write_durable_verdict(
    dir: &Path,
    source_task: &Task,
    satellite: &Task,
    stage: AgencyStage,
    evaluation: &Evaluation,
) -> Result<PathBuf> {
    let plan = satellite.agency_dispatch.as_ref().ok_or_else(|| {
        anyhow::anyhow!("satellite {} has no persisted agency plan", satellite.id)
    })?;
    validate_plan(plan)?;
    if plan.source_task != source_task.id {
        anyhow::bail!("agency plan source mismatch");
    }
    // The evaluation writer has already atomically renamed its JSON before
    // reaching this call. Pin those exact durable bytes rather than serializing
    // the in-memory `HashMap` again: HashMap iteration order is not stable
    // across the evaluator and daemon processes.
    let evidence = load_evaluation_evidence(dir, &evaluation.id)?;
    if canonical_evaluation_bytes(&evidence.evaluation)? != canonical_evaluation_bytes(evaluation)?
    {
        anyhow::bail!(
            "error[WG-EVAL-VERDICT-EVIDENCE]: durable evaluation {:?} differs from writer value",
            evaluation.id
        );
    }
    let evaluation_digest = digest_bytes(&evidence.bytes);
    let verdict_id = format!(
        "verdict-{}-{}-{}",
        plan.pipeline_id,
        match stage {
            AgencyStage::FlipInference | AgencyStage::FlipComparison => "flip",
            AgencyStage::Evaluate => "evaluate",
        },
        &blake3::hash(evaluation.id.as_bytes()).to_hex()[..16]
    );
    let verdict = DurableEvalVerdict {
        schema: EVAL_LIFECYCLE_SCHEMA,
        verdict_id: verdict_id.clone(),
        verdict_digest: String::new(),
        evaluation_id: evaluation.id.clone(),
        pipeline_id: plan.pipeline_id.clone(),
        source_task: source_task.id.clone(),
        source_attempt: plan.source_attempt,
        stage,
        producer_run_id: satellite
            .assigned
            .clone()
            .unwrap_or_else(|| "manual".to_string()),
        score: evaluation.score,
        evaluation_digest_schema: EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA,
        evaluation_digest,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut verdict = verdict;
    verdict.verdict_digest = compute_verdict_digest(&verdict)?;
    let bytes = serde_json::to_vec_pretty(&verdict)?;
    let directory = verdicts_dir(dir);
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{verdict_id}.json"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&path)?;
            let parsed: DurableEvalVerdict = serde_json::from_slice(&existing)?;
            // `created_at` and the current wrapper assignment are observational;
            // semantic identity is the pipeline/stage/evaluation digest. Verify
            // the immutable record we already have, then compare only canonical
            // semantic content. A crash replaying the same completed model result
            // is therefore a no-op even when time/run identity changed, while a
            // different result at the same key is quarantined.
            if parsed.verdict_digest != compute_verdict_digest(&parsed)? {
                anyhow::bail!(
                    "error[WG-EVAL-VERDICT-INTEGRITY]: verdict digest mismatch at {}",
                    path.display()
                );
            }
            // Independently validate the existing record against the durable
            // evaluation. This also makes an upgrade replay from legacy scheme
            // 1 to scheme 2 a no-op: both schemes must pin the same bytes, but
            // observational timestamps/run ids and the digest encoding itself
            // are not semantic conflicts.
            verify_evaluation_digest(dir, &parsed)?;
            if parsed.schema != verdict.schema
                || parsed.verdict_id != verdict.verdict_id
                || parsed.evaluation_id != verdict.evaluation_id
                || parsed.pipeline_id != verdict.pipeline_id
                || parsed.source_task != verdict.source_task
                || parsed.source_attempt != verdict.source_attempt
                || parsed.stage != verdict.stage
                || parsed.score != verdict.score
            {
                anyhow::bail!(
                    "error[WG-EVAL-VERDICT-CONFLICT]: verdict id {} has conflicting content",
                    verdict_id
                );
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn compute_verdict_digest(verdict: &DurableEvalVerdict) -> Result<String> {
    let mut material = verdict.clone();
    material.verdict_digest.clear();
    Ok(format!(
        "b3:{}",
        blake3::hash(&serde_json::to_vec(&material)?).to_hex()
    ))
}

struct EvaluationEvidence {
    evaluation: Evaluation,
    bytes: Vec<u8>,
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

/// Serialize through `Value`, whose default map representation is ordered.
/// This is used only to prove the just-written file has the same semantics as
/// the writer value; integrity itself pins the exact durable file bytes.
fn canonical_evaluation_bytes(evaluation: &Evaluation) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::to_value(evaluation)?)?)
}

/// Remove JSON formatting whitespace while preserving object member order and
/// every byte inside strings. The schema-1 writer hashed compact serde JSON and
/// saved pretty serde JSON from the same object, so this exactly reconstructs
/// its pre-restart byte sequence without guessing HashMap order.
fn compact_durable_json(bytes: &[u8]) -> Vec<u8> {
    let mut compact = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            compact.push(*byte);
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
        } else if *byte == b'"' {
            in_string = true;
            compact.push(*byte);
        } else if !byte.is_ascii_whitespace() {
            compact.push(*byte);
        }
    }
    compact
}

/// One-pass index of durable evaluation evidence, keyed by evaluation id.
///
/// Verifying N durable verdicts used to call `load_evaluation_evidence` — a
/// full read+parse of *every* file in `agency/evaluations/` — once per verdict,
/// i.e. O(N × E) file reads. At ~9.5k evaluation files that quadratic wedged the
/// coordinator tick for over an hour (engine-eval-verdict). Building the index
/// once and verifying against it collapses a load to O(E) reads.
///
/// Matching is by the record's INTERNAL `id`, not the filename stem: a durable
/// verdict references its evaluation by id, and a real 2026-07-19 incident
/// fixture stores that evaluation as `evaluation.json` (stem ≠ id). Only files
/// whose parsed id is `wanted` are retained, bounding memory to the referenced
/// set. Per-tick O(new files) comes from the verdict CACHE skipping this scan
/// entirely while nothing changed — never from guessing ids off filenames.
struct EvidenceIndex {
    by_id: HashMap<String, Vec<EvaluationEvidence>>,
}

impl EvidenceIndex {
    fn build(dir: &Path, wanted: &HashSet<String>) -> Result<Self> {
        let directory = dir.join("agency/evaluations");
        let mut by_id: HashMap<String, Vec<EvaluationEvidence>> = HashMap::new();
        if directory.exists() {
            for entry in fs::read_dir(&directory)? {
                let path = entry?.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let bytes = fs::read(&path)?;
                let evaluation: Evaluation = serde_json::from_slice(&bytes)
                    .with_context(|| format!("loading evaluation evidence {}", path.display()))?;
                if !wanted.contains(&evaluation.id) {
                    continue;
                }
                by_id
                    .entry(evaluation.id.clone())
                    .or_default()
                    .push(EvaluationEvidence { evaluation, bytes });
            }
        }
        Ok(Self { by_id })
    }

    fn lookup(&self, evaluation_id: &str) -> Result<&EvaluationEvidence> {
        match self.by_id.get(evaluation_id) {
            Some(list) if list.len() == 1 => Ok(&list[0]),
            other => anyhow::bail!(
                "error[WG-EVAL-VERDICT-EVIDENCE]: evaluation {:?} has {} durable matches",
                evaluation_id,
                other.map_or(0, Vec::len)
            ),
        }
    }
}

/// Load the single durable evaluation whose internal id is `evaluation_id`.
/// Off the hot path (one call in `write_durable_verdict`); shares the index
/// builder, keeping only the one wanted record.
fn load_evaluation_evidence(dir: &Path, evaluation_id: &str) -> Result<EvaluationEvidence> {
    let wanted: HashSet<String> = std::iter::once(evaluation_id.to_string()).collect();
    let mut index = EvidenceIndex::build(dir, &wanted)?;
    index.lookup(evaluation_id)?;
    Ok(index
        .by_id
        .remove(evaluation_id)
        .and_then(|mut list| list.pop())
        .expect("lookup verified exactly one match"))
}

/// Single-verdict verification used off the hot path (e.g. `write_durable_verdict`
/// replay). Builds a one-id index; the batch loader shares one index across all
/// verdicts instead of paying this per verdict.
fn verify_evaluation_digest(dir: &Path, verdict: &DurableEvalVerdict) -> Result<()> {
    let wanted: HashSet<String> = std::iter::once(verdict.evaluation_id.clone()).collect();
    let index = EvidenceIndex::build(dir, &wanted)?;
    verify_evaluation_digest_indexed(&index, verdict)
}

fn verify_evaluation_digest_indexed(
    index: &EvidenceIndex,
    verdict: &DurableEvalVerdict,
) -> Result<()> {
    let evidence = index.lookup(&verdict.evaluation_id).map_err(|error| {
        anyhow::anyhow!(
            "error[WG-EVAL-VERDICT-EVIDENCE]: verdict {}: {error:#}",
            verdict.verdict_id
        )
    })?;
    if evidence.evaluation.task_id != verdict.source_task
        || evidence.evaluation.score != verdict.score
    {
        anyhow::bail!(
            "error[WG-EVAL-VERDICT-EVIDENCE]: verdict {} source/score evaluation mismatch",
            verdict.verdict_id
        );
    }
    let digest = match verdict.evaluation_digest_schema {
        1 => digest_bytes(&compact_durable_json(&evidence.bytes)),
        EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA => digest_bytes(&evidence.bytes),
        schema => anyhow::bail!(
            "error[WG-EVAL-VERDICT-EVIDENCE]: verdict {} uses unsupported evaluation digest schema {}",
            verdict.verdict_id,
            schema
        ),
    };
    if digest != verdict.evaluation_digest {
        anyhow::bail!(
            "error[WG-EVAL-VERDICT-EVIDENCE]: verdict {} evaluation digest mismatch",
            verdict.verdict_id
        );
    }
    Ok(())
}

pub fn load_durable_verdicts(dir: &Path) -> Result<Vec<DurableEvalVerdict>> {
    let directory = verdicts_dir(dir);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    // Pass 1: read + integrity-check every verdict record. No evidence yet, so
    // this is one cheap scan of the (small) verdicts dir.
    let mut verdicts = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let verdict: DurableEvalVerdict = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("loading durable verdict {}", path.display()))?;
        let expected_file = format!("{}.json", verdict.verdict_id);
        if path.file_name().and_then(|name| name.to_str()) != Some(expected_file.as_str()) {
            anyhow::bail!(
                "error[WG-EVAL-VERDICT-INTEGRITY]: verdict id/filename mismatch at {}",
                path.display()
            );
        }
        if verdict.verdict_digest != compute_verdict_digest(&verdict)? {
            anyhow::bail!(
                "error[WG-EVAL-VERDICT-INTEGRITY]: verdict digest mismatch at {}",
                path.display()
            );
        }
        verdicts.push(verdict);
    }
    // Pass 2: verify every verdict against durable evidence using ONE shared
    // index built from a single evaluations-dir scan, reading only the files the
    // verdicts actually reference. Was O(verdicts × all-evaluations) — the
    // quadratic that wedged the dispatcher at ~9.5k files (engine-eval-verdict).
    let wanted: HashSet<String> = verdicts
        .iter()
        .map(|verdict| verdict.evaluation_id.clone())
        .collect();
    let index = EvidenceIndex::build(dir, &wanted)?;
    for verdict in &verdicts {
        verify_evaluation_digest_indexed(&index, verdict)?;
    }
    verdicts.sort_by(|a, b| a.verdict_id.cmp(&b.verdict_id));
    Ok(verdicts)
}

// ---------------------------------------------------------------------------
// Incremental evidence loading (engine-eval-verdict)
//
// The coordinator tick must never pay O(all-history) to link verdicts. Two
// dir fingerprints (cheap stat-only listings) let the hot path reuse an
// already-verified result whenever nothing changed, falling back to a full
// indexed rescan only on a real change. A full rescan otherwise belongs in the
// explicit `wg service eval-repair` command, not the tick.
// ---------------------------------------------------------------------------

const VERDICT_CACHE_SCHEMA: u16 = 1;
const MIGRATION_MARKER_SCHEMA: u16 = 1;

fn eval_lifecycle_dir(dir: &Path) -> PathBuf {
    dir.join("agency").join("eval-lifecycle")
}

fn verdict_cache_path(dir: &Path) -> PathBuf {
    eval_lifecycle_dir(dir).join("verdicts-cache.json")
}

fn migration_marker_path(dir: &Path) -> PathBuf {
    eval_lifecycle_dir(dir).join("migration-marker.json")
}

fn evaluations_dir(dir: &Path) -> PathBuf {
    dir.join("agency").join("evaluations")
}

/// Cheap, read-free fingerprint of a directory: entry count plus the newest
/// entry mtime plus the directory's own mtime. Detects adds, removals, and
/// in-place rewrites without reading a single file body, so recomputing it each
/// tick is O(dir-listing), not O(all-history).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirFingerprint {
    pub count: u64,
    pub latest_mtime_ns: u128,
    pub dir_mtime_ns: u128,
}

fn metadata_mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |delta| delta.as_nanos())
}

pub fn dir_fingerprint(dir: &Path) -> DirFingerprint {
    let mut fingerprint = DirFingerprint::default();
    let Ok(dir_meta) = fs::metadata(dir) else {
        return fingerprint;
    };
    fingerprint.dir_mtime_ns = metadata_mtime_ns(&dir_meta);
    let Ok(entries) = fs::read_dir(dir) else {
        return fingerprint;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        fingerprint.count += 1;
        if let Ok(meta) = entry.metadata() {
            fingerprint.latest_mtime_ns = fingerprint.latest_mtime_ns.max(metadata_mtime_ns(&meta));
        }
    }
    fingerprint
}

#[derive(Serialize, Deserialize)]
struct VerdictCache {
    schema: u16,
    verdicts_fp: DirFingerprint,
    evaluations_fp: DirFingerprint,
    verdicts: Vec<DurableEvalVerdict>,
}

fn write_json_best_effort<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, &bytes).is_ok() && fs::rename(&tmp, path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// Incremental wrapper over [`load_durable_verdicts`]. Returns the cached,
/// already-verified verdicts when neither the verdicts dir nor the evaluations
/// dir has changed since the cache was written, so the hot coordinator tick is
/// O(dir-listing) instead of O(all-history). Any fingerprint change falls back
/// to a full indexed load (fail-closed integrity re-runs on every real change)
/// and refreshes the cache.
pub fn load_durable_verdicts_cached(dir: &Path) -> Result<Vec<DurableEvalVerdict>> {
    let verdicts_fp = dir_fingerprint(&verdicts_dir(dir));
    let evaluations_fp = dir_fingerprint(&evaluations_dir(dir));
    if let Ok(bytes) = fs::read(verdict_cache_path(dir)) {
        if let Ok(cache) = serde_json::from_slice::<VerdictCache>(&bytes) {
            if cache.schema == VERDICT_CACHE_SCHEMA
                && cache.verdicts_fp == verdicts_fp
                && cache.evaluations_fp == evaluations_fp
            {
                return Ok(cache.verdicts);
            }
        }
    }
    let verdicts = load_durable_verdicts(dir)?;
    // Re-fingerprint AFTER the scan so a write that raced our load invalidates
    // the cache next tick rather than being masked by a pre-scan fingerprint.
    let cache = VerdictCache {
        schema: VERDICT_CACHE_SCHEMA,
        verdicts_fp: dir_fingerprint(&verdicts_dir(dir)),
        evaluations_fp: dir_fingerprint(&evaluations_dir(dir)),
        verdicts: verdicts.clone(),
    };
    write_json_best_effort(&verdict_cache_path(dir), &cache);
    Ok(verdicts)
}

#[derive(Serialize, Deserialize)]
struct MigrationMarker {
    schema: u16,
    evaluations_fp: DirFingerprint,
    verdicts_fp: DirFingerprint,
}

/// Outcome of an incremental migration attempt. `scanned` is `false` when the
/// marker short-circuited the whole scan (the O(new files) fast path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub migrated: usize,
    pub scanned: bool,
}

/// Incremental front door for [`migrate_unambiguous_legacy_verdicts`]. A marker
/// records the evaluations + verdicts fingerprints at the last completed scan;
/// while both are unchanged there is provably nothing new to migrate, so the
/// tick skips the scan entirely. This is the "verified high-water mark" the hot
/// path relies on to stay O(new files).
pub fn migrate_unambiguous_legacy_verdicts_incremental(dir: &Path) -> Result<MigrationOutcome> {
    let evaluations_fp = dir_fingerprint(&evaluations_dir(dir));
    let verdicts_fp = dir_fingerprint(&verdicts_dir(dir));
    if let Ok(bytes) = fs::read(migration_marker_path(dir)) {
        if let Ok(marker) = serde_json::from_slice::<MigrationMarker>(&bytes) {
            if marker.schema == MIGRATION_MARKER_SCHEMA
                && marker.evaluations_fp == evaluations_fp
                && marker.verdicts_fp == verdicts_fp
            {
                return Ok(MigrationOutcome {
                    migrated: 0,
                    scanned: false,
                });
            }
        }
    }
    let migrated = migrate_unambiguous_legacy_verdicts(dir)?;
    // Capture post-scan fingerprints so a migration that wrote new verdicts does
    // not immediately re-scan on the next tick.
    let marker = MigrationMarker {
        schema: MIGRATION_MARKER_SCHEMA,
        evaluations_fp: dir_fingerprint(&evaluations_dir(dir)),
        verdicts_fp: dir_fingerprint(&verdicts_dir(dir)),
    };
    write_json_best_effort(&migration_marker_path(dir), &marker);
    Ok(MigrationOutcome {
        migrated,
        scanned: true,
    })
}

/// Archive-not-delete retention for `agency/evaluations/`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationArchiveReport {
    pub archived: usize,
    pub kept: usize,
    pub protected: usize,
    pub bytes_archived: u64,
}

/// Cap the evaluations dir at `keep_newest` files (by mtime), moving the rest to
/// `agency/evaluations-archive/`. Evaluations still referenced by a durable
/// verdict — or belonging to a task awaiting evaluation — are never archived, so
/// this can never break `verify_evaluation_digest` or a pending migration.
/// `keep_newest == 0` disables retention. Archive, never delete: the operator
/// (or `wg service eval-repair`) can always move files back.
pub fn archive_stale_evaluations(dir: &Path, keep_newest: usize) -> Result<EvaluationArchiveReport> {
    let mut report = EvaluationArchiveReport::default();
    if keep_newest == 0 {
        return Ok(report);
    }
    let evaluations = evaluations_dir(dir);
    if !evaluations.is_dir() {
        return Ok(report);
    }

    let mut entries: Vec<(PathBuf, String, u128)> = Vec::new();
    for entry in fs::read_dir(&evaluations)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let mtime = entry.metadata().map(|m| metadata_mtime_ns(&m)).unwrap_or(0);
        entries.push((path, id, mtime));
    }
    // Newest first: the first `keep_newest` survive unconditionally.
    entries.sort_by(|a, b| b.2.cmp(&a.2));
    if entries.len() <= keep_newest {
        report.kept = entries.len();
        return Ok(report);
    }

    // Protected id set: evaluations referenced by a durable verdict never move.
    let mut protected: HashSet<String> = HashSet::new();
    if let Ok(verdicts) = load_durable_verdicts_cached(dir) {
        for verdict in &verdicts {
            protected.insert(verdict.evaluation_id.clone());
        }
    }
    // Tasks awaiting evaluation: their candidate evaluations may not yet have a
    // verdict, so protect by task id too (read only the archive candidates, and
    // only when such tasks exist — bounded, off the hot tick).
    let pending_tasks: HashSet<String> = crate::parser::load_graph(&dir.join("graph.jsonl"))
        .map(|graph| {
            graph
                .tasks()
                .filter(|task| {
                    matches!(
                        task.status,
                        Status::PendingEval | Status::FailedPendingEval
                    )
                })
                .map(|task| task.id.clone())
                .collect()
        })
        .unwrap_or_default();

    let archive_dir = dir.join("agency").join("evaluations-archive");
    for (index, (path, id, _mtime)) in entries.iter().enumerate() {
        if index < keep_newest {
            report.kept += 1;
            continue;
        }
        if protected.contains(id) {
            report.protected += 1;
            continue;
        }
        if !pending_tasks.is_empty() {
            if let Ok(bytes) = fs::read(path) {
                if let Ok(evaluation) = serde_json::from_slice::<Evaluation>(&bytes) {
                    if pending_tasks.contains(&evaluation.task_id) {
                        report.protected += 1;
                        continue;
                    }
                }
            }
        }
        if fs::create_dir_all(&archive_dir).is_err() {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if fs::rename(path, archive_dir.join(file_name)).is_ok() {
            report.archived += 1;
            report.bytes_archived += size;
        }
    }
    Ok(report)
}

/// Report from the explicit full-rescan repair path.
#[derive(Debug, Default, Clone, Copy)]
pub struct EvalRepairReport {
    pub migrated: usize,
    pub verdicts_loaded: usize,
}

/// Explicit, operator-invoked full rescan. Clears the incremental cache and
/// marker, runs a complete migration + verify, then repopulates both so the
/// next tick is fast again. This is where an O(all-history) pass belongs — never
/// the hot coordinator tick.
pub fn repair_eval_verdicts(dir: &Path) -> Result<EvalRepairReport> {
    let _ = fs::remove_file(verdict_cache_path(dir));
    let _ = fs::remove_file(migration_marker_path(dir));
    let migrated = migrate_unambiguous_legacy_verdicts(dir)?;
    let verdicts = load_durable_verdicts(dir)?;
    // Repopulate cache + marker for the fast path.
    let _ = load_durable_verdicts_cached(dir);
    let _ = migrate_unambiguous_legacy_verdicts_incremental(dir);
    Ok(EvalRepairReport {
        migrated,
        verdicts_loaded: verdicts.len(),
    })
}

/// Upgrade an unambiguous pre-schema Evaluation into durable pipeline evidence.
/// Missing source timestamps and zero/multiple candidates are deliberately left
/// untouched for operator review; this function never chooses "latest".
pub fn migrate_unambiguous_legacy_verdicts(dir: &Path) -> Result<usize> {
    // Migration only ever produces evidence for sources awaiting evaluation.
    // Load the (single-file) graph first and bail before touching the durable
    // verdict store or the evaluations dir when there is nothing pending — the
    // steady-state common case — so the scan is O(graph), not O(all-history).
    let graph = crate::parser::load_graph(&dir.join("graph.jsonl"))?;
    let has_pending = graph
        .tasks()
        .any(|task| matches!(task.status, Status::PendingEval | Status::FailedPendingEval));
    if !has_pending {
        return Ok(0);
    }
    let existing = load_durable_verdicts_cached(dir)?;
    let evaluations = crate::agency::load_all_evaluations_or_warn(&dir.join("agency/evaluations"));
    let mut migrated = 0;

    for source in graph
        .tasks()
        .filter(|task| matches!(task.status, Status::PendingEval | Status::FailedPendingEval))
    {
        let Some(started_at) = source
            .started_at
            .as_deref()
            .and_then(|value| value.parse::<chrono::DateTime<Utc>>().ok())
        else {
            continue;
        };
        for (task_id, stage, is_candidate) in [
            (
                format!(".flip-{}", source.id),
                AgencyStage::FlipComparison,
                true,
            ),
            (
                format!(".evaluate-{}", source.id),
                AgencyStage::Evaluate,
                false,
            ),
        ] {
            let Some(satellite) = graph.get_task(&task_id) else {
                continue;
            };
            let plan = if let Some(plan) = satellite.agency_dispatch.clone() {
                validate_plan(&plan)?;
                plan
            } else {
                // A completed, claimed pre-schema evaluator is execution evidence,
                // not a route-retry candidate. We may backfill its plan only when
                // its display route is losslessly handler-qualified; this never
                // reopens or invokes the historical row.
                if satellite.status != Status::Done
                    || satellite.assigned.is_none()
                    || satellite.started_at.is_none()
                {
                    continue;
                }
                let Ok(plan) = migrate_legacy_plan(source, satellite) else {
                    continue;
                };
                plan
            };
            if existing.iter().any(|verdict| {
                verdict.pipeline_id == plan.pipeline_id
                    && verdict.source_attempt == plan.source_attempt
                    && verdict.stage == stage
            }) {
                continue;
            }
            let evidence_started_at = satellite
                .started_at
                .as_deref()
                .and_then(|value| value.parse::<chrono::DateTime<Utc>>().ok())
                .map_or(started_at, |satellite_start| {
                    started_at.max(satellite_start)
                });
            let candidates: Vec<_> = evaluations
                .iter()
                .filter(|evaluation| evaluation.task_id == source.id)
                .filter(|evaluation| evaluation.loop_iteration == source.loop_iteration)
                .filter(|evaluation| {
                    evaluation
                        .timestamp
                        .parse::<chrono::DateTime<Utc>>()
                        .is_ok_and(|timestamp| timestamp >= evidence_started_at)
                })
                .filter(|evaluation| {
                    if is_candidate {
                        evaluation.source == crate::agency::eval_source::FLIP
                    } else {
                        evaluation.source != crate::agency::eval_source::FLIP
                            && evaluation.source != "system"
                    }
                })
                .collect();
            if candidates.len() != 1 {
                continue;
            }
            let mut planned_satellite = satellite.clone();
            planned_satellite.agency_dispatch = Some(plan);
            write_durable_verdict(dir, source, &planned_satellite, stage, candidates[0])?;
            migrated += 1;
        }
    }
    Ok(migrated)
}

fn lifecycle_for_plan(plan: &AgencyDispatchPlan) -> EvaluationLifecycle {
    EvaluationLifecycle {
        schema: EVAL_LIFECYCLE_SCHEMA,
        pipeline_id: plan.pipeline_id.clone(),
        source_attempt: plan.source_attempt,
        route_generation: 0,
        schedule_attempts: 0,
        transport_attempts: 0,
        semantic_attempts: 0,
        execution_state: EvaluationExecutionState::Ready,
        linked_flip_verdict: None,
        linked_eval_verdict: None,
        consumed_verdict: None,
        repair_version: 0,
    }
}

fn lifecycle_conflict(task: &mut Task, message: String) -> bool {
    if task.failure_reason.as_deref() == Some(message.as_str()) {
        return false;
    }
    task.failure_reason = Some(message.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some("eval-lifecycle-reconcile".to_string()),
        user: None,
        message,
    });
    true
}

/// Repair historical pre-claim rows using only lossless evidence already in
/// the graph. A legacy Codex split is canonical; an OpenRouter provider without
/// a handler is deliberately parked because it cannot distinguish Pi from Nex.
/// Each row is rearmed at most once per lifecycle schema.
pub fn repair_historical_rows(graph: &mut WorkGraph) -> bool {
    let satellite_ids: Vec<String> = graph
        .tasks()
        .filter(|task| task.id.starts_with(".flip-") || task.id.starts_with(".evaluate-"))
        .filter(|task| task.agency_dispatch.is_none())
        // Never rewrite an active or previously claimed legacy run. Route
        // repair is automatic only for rows with pre-claim evidence.
        .filter(|task| task.assigned.is_none() && task.started_at.is_none())
        .map(|task| task.id.clone())
        .collect();
    let mut modified = false;

    for satellite_id in satellite_ids {
        let source_id = satellite_id
            .strip_prefix(".flip-")
            .or_else(|| satellite_id.strip_prefix(".evaluate-"))
            .expect("filtered satellite id");
        let Some(source) = graph.get_task(source_id).cloned() else {
            continue;
        };
        if !matches!(
            source.status,
            Status::PendingEval | Status::FailedPendingEval
        ) {
            continue;
        }
        let satellite_snapshot = graph
            .get_task(&satellite_id)
            .expect("collected satellite")
            .clone();
        match migrate_legacy_plan(&source, &satellite_snapshot) {
            Ok(plan) => {
                let satellite = graph
                    .get_task_mut(&satellite_id)
                    .expect("collected satellite");
                satellite.model = Some(plan.calls[0].route.clone());
                satellite.provider = Some(plan.calls[0].system.handler.clone());
                satellite.endpoint = plan.calls[0].endpoint.clone();
                satellite.reasoning = plan.calls[0].reasoning;
                satellite.agency_dispatch = Some(plan.clone());
                let lifecycle = satellite
                    .evaluation_lifecycle
                    .get_or_insert_with(|| lifecycle_for_plan(&plan));
                if satellite.status == Status::Incomplete
                    && satellite.assigned.is_none()
                    && satellite.started_at.is_none()
                    && satellite.spawn_failures > 0
                    && lifecycle.repair_version < EVAL_LIFECYCLE_SCHEMA
                {
                    satellite.status = Status::Open;
                    satellite.spawn_failures = 0;
                    satellite.failure_reason = None;
                    lifecycle.repair_version = EVAL_LIFECYCLE_SCHEMA;
                    lifecycle.execution_state = EvaluationExecutionState::Ready;
                }
                satellite.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("eval-lifecycle-repair".to_string()),
                    user: None,
                    message: format!(
                        "Installed lossless historical plan {}; route={}",
                        plan.plan_hash, plan.calls[0].route
                    ),
                });
                modified = true;
            }
            Err(error) => {
                let diagnostic = format!("Lifecycle route repair required: {error:#}");
                let satellite = graph
                    .get_task_mut(&satellite_id)
                    .expect("collected satellite");
                if satellite.status != Status::Blocked
                    || satellite.failure_reason.as_deref() != Some(diagnostic.as_str())
                {
                    satellite.status = Status::Blocked;
                    satellite.wait_condition = None;
                    modified |= lifecycle_conflict(satellite, diagnostic);
                }
            }
        }
    }
    modified
}

/// Backfill a plan on a completed, claimed pre-schema satellite only after a
/// verified durable verdict proves that its semantic call already completed.
/// This is deliberately separate from `repair_historical_rows`: it never
/// rearms claimed work and cannot cause another model invocation.
fn install_completed_legacy_plan(
    graph: &mut WorkGraph,
    task_id: &str,
    verdict: &DurableEvalVerdict,
) -> bool {
    let Some(satellite_snapshot) = graph.get_task(task_id).cloned() else {
        return false;
    };
    if satellite_snapshot.agency_dispatch.is_some()
        || satellite_snapshot.status != Status::Done
        || satellite_snapshot.assigned.is_none()
        || satellite_snapshot.started_at.is_none()
    {
        return false;
    }
    let Some(source) = graph.get_task(&verdict.source_task).cloned() else {
        return false;
    };
    let Ok(plan) = migrate_legacy_plan(&source, &satellite_snapshot) else {
        return false;
    };
    if plan.pipeline_id != verdict.pipeline_id
        || plan.source_attempt != verdict.source_attempt
        || plan.source_task != verdict.source_task
        || !plan.calls.iter().any(|call| call.stage == verdict.stage)
    {
        return false;
    }

    let satellite = graph
        .get_task_mut(task_id)
        .expect("legacy satellite snapshot came from graph");
    satellite.model = Some(plan.calls[0].route.clone());
    satellite.provider = Some(plan.calls[0].system.handler.clone());
    satellite.endpoint = plan.calls[0].endpoint.clone();
    satellite.reasoning = plan.calls[0].reasoning;
    satellite.agency_dispatch = Some(plan.clone());
    satellite.evaluation_lifecycle = Some(lifecycle_for_plan(&plan));
    satellite.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some("eval-lifecycle-reconcile".to_string()),
        user: None,
        message: format!(
            "Backfilled completed historical plan {} from verified verdict {}; no semantic rerun",
            plan.plan_hash, verdict.verdict_id
        ),
    });
    true
}

fn mark_satellite_verdict(
    graph: &mut WorkGraph,
    task_id: &str,
    verdict: &DurableEvalVerdict,
) -> bool {
    let Some(task) = graph.get_task_mut(task_id) else {
        return false;
    };
    let Some(plan) = task.agency_dispatch.as_ref() else {
        return lifecycle_conflict(
            task,
            format!(
                "Durable verdict {} has no persisted agency plan",
                verdict.verdict_id
            ),
        );
    };
    if plan.pipeline_id != verdict.pipeline_id || plan.source_attempt != verdict.source_attempt {
        return lifecycle_conflict(
            task,
            format!(
                "Durable verdict {} does not match persisted pipeline {}",
                verdict.verdict_id, plan.pipeline_id
            ),
        );
    }
    let plan = plan.clone();
    task.evaluation_lifecycle
        .get_or_insert_with(|| lifecycle_for_plan(&plan));
    let existing = task.evaluation_lifecycle.as_ref().and_then(|lifecycle| {
        if verdict.stage == AgencyStage::Evaluate {
            lifecycle.linked_eval_verdict.clone()
        } else {
            lifecycle.linked_flip_verdict.clone()
        }
    });
    if let Some(existing) = existing
        && existing != verdict.verdict_id
    {
        return lifecycle_conflict(
            task,
            format!(
                "error[WG-EVAL-CONSUMPTION-CONFLICT]: stage linked {} but found {}",
                existing, verdict.verdict_id
            ),
        );
    }
    let lifecycle = task
        .evaluation_lifecycle
        .as_mut()
        .expect("inserted lifecycle");
    let slot = if verdict.stage == AgencyStage::Evaluate {
        &mut lifecycle.linked_eval_verdict
    } else {
        &mut lifecycle.linked_flip_verdict
    };
    let mut modified = false;
    if slot.is_none() {
        *slot = Some(verdict.verdict_id.clone());
        modified = true;
    }
    if task.status != Status::Done {
        task.status = Status::Done;
        task.assigned = None;
        task.completed_at
            .get_or_insert_with(|| Utc::now().to_rfc3339());
        modified = true;
    }
    if lifecycle.semantic_attempts == 0 {
        lifecycle.semantic_attempts = 1;
        modified = true;
    }
    if lifecycle.execution_state != EvaluationExecutionState::VerdictDurable {
        lifecycle.execution_state = EvaluationExecutionState::VerdictDurable;
        modified = true;
    }
    if modified {
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: Some("eval-lifecycle-reconcile".to_string()),
            user: None,
            message: format!(
                "Linked durable {:?} verdict {} without semantic rerun",
                verdict.stage, verdict.verdict_id
            ),
        });
    }
    modified
}

fn rebind_plan_to_source(plan: &AgencyDispatchPlan, source: &Task) -> Result<AgencyDispatchPlan> {
    let mut rebound = plan.clone();
    rebound.source_attempt = source
        .retry_count
        .saturating_add(source.rescue_count)
        .saturating_add(1);
    rebound.pipeline_id = pipeline_id(&source.id, rebound.source_attempt, source.loop_iteration);
    rebound.plan_hash = compute_plan_hash(&rebound)?;
    validate_plan(&rebound)?;
    Ok(rebound)
}

fn reset_satellite_for_source(graph: &mut WorkGraph, task_id: &str, source: &Task) -> bool {
    let Some(previous_plan) = graph
        .get_task(task_id)
        .and_then(|task| task.agency_dispatch.clone())
    else {
        return false;
    };
    let Ok(plan) = rebind_plan_to_source(&previous_plan, source) else {
        return false;
    };
    let task = graph.get_task_mut(task_id).expect("plan came from task");
    task.status = Status::Open;
    task.assigned = None;
    task.started_at = None;
    task.completed_at = None;
    task.failure_reason = None;
    task.wait_condition = None;
    task.spawn_failures = 0;
    task.agency_dispatch = Some(plan.clone());
    task.evaluation_lifecycle = Some(lifecycle_for_plan(&plan));
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some("eval-lifecycle-reconcile".to_string()),
        user: None,
        message: format!(
            "Rearmed exact persisted route for source attempt {}; plan={}",
            plan.source_attempt, plan.plan_hash
        ),
    });
    true
}

/// Rearm an existing evaluation chain for an explicit source retry while
/// preserving the exact prior routes. The source keeps its consumed old
/// lifecycle as audit evidence until it next enters a soft evaluation state.
pub fn rearm_satellites_for_source(graph: &mut WorkGraph, source: &Task) -> bool {
    if source.id.starts_with('.') {
        return false;
    }
    let mut modified = reset_satellite_for_source(graph, &format!(".flip-{}", source.id), source);
    modified |= reset_satellite_for_source(graph, &format!(".evaluate-{}", source.id), source);
    modified
}

/// Link durable stage evidence and atomically consume an evaluator verdict into
/// its source task. The caller runs this inside the graph's single
/// `modify_graph` transaction, so `consumed_verdict` and the source transition
/// always land in the same atomic rename. `pending_is_gated` preserves the
/// existing distinction between advisory evaluations and hard eval gates.
pub fn reconcile_durable_verdicts<F>(
    graph: &mut WorkGraph,
    verdicts: &[DurableEvalVerdict],
    threshold: f64,
    auto_rescue: bool,
    max_rescues: u32,
    pending_is_gated: F,
) -> bool
where
    F: Fn(&Task) -> bool,
{
    let source_ids: Vec<String> = graph
        .tasks()
        .filter(|task| {
            matches!(task.status, Status::PendingEval | Status::FailedPendingEval)
                || task
                    .evaluation_lifecycle
                    .as_ref()
                    .and_then(|lifecycle| lifecycle.consumed_verdict.as_ref())
                    .is_some()
        })
        .map(|task| task.id.clone())
        .collect();
    let mut modified = false;

    for source_id in source_ids {
        let source_snapshot = graph
            .get_task(&source_id)
            .expect("collected source")
            .clone();
        let source_lifecycle = source_snapshot
            .evaluation_lifecycle
            .clone()
            .unwrap_or_else(|| EvaluationLifecycle::for_source(&source_snapshot));
        let matching: Vec<&DurableEvalVerdict> = verdicts
            .iter()
            .filter(|verdict| {
                verdict.source_task == source_id
                    && verdict.pipeline_id == source_lifecycle.pipeline_id
                    && verdict.source_attempt == source_lifecycle.source_attempt
            })
            .collect();
        let flips: Vec<_> = matching
            .iter()
            .copied()
            .filter(|verdict| verdict.stage != AgencyStage::Evaluate)
            .collect();
        let evals: Vec<_> = matching
            .iter()
            .copied()
            .filter(|verdict| verdict.stage == AgencyStage::Evaluate)
            .collect();

        if flips.len() > 1 || evals.len() > 1 {
            let ids = matching
                .iter()
                .map(|verdict| verdict.verdict_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let source = graph.get_task_mut(&source_id).expect("collected source");
            modified |= lifecycle_conflict(
                source,
                format!(
                    "error[WG-EVAL-VERDICT-AMBIGUOUS]: multiple stage verdicts require operator selection: {ids}"
                ),
            );
            continue;
        }

        if let Some(consumed) = source_lifecycle.consumed_verdict.as_deref() {
            if let Some(eval) = evals.first()
                && eval.verdict_id != consumed
            {
                let source = graph.get_task_mut(&source_id).expect("collected source");
                modified |= lifecycle_conflict(
                    source,
                    format!(
                        "error[WG-EVAL-CONSUMPTION-CONFLICT]: source consumed {} but found {}",
                        consumed, eval.verdict_id
                    ),
                );
            }
            continue;
        }

        if let Some(flip) = flips.first() {
            let task_id = format!(".flip-{source_id}");
            modified |= install_completed_legacy_plan(graph, &task_id, flip);
            modified |= mark_satellite_verdict(graph, &task_id, flip);
        }
        let Some(eval) = evals.first() else {
            if matches!(
                source_snapshot.status,
                Status::PendingEval | Status::FailedPendingEval
            ) && source_snapshot.evaluation_lifecycle.is_none()
            {
                graph
                    .get_task_mut(&source_id)
                    .expect("collected source")
                    .evaluation_lifecycle = Some(source_lifecycle);
                modified = true;
            }
            continue;
        };

        let flip_required = graph.get_task(&format!(".flip-{source_id}")).is_some();
        let flip_linked = !flip_required
            || graph
                .get_task(&format!(".flip-{source_id}"))
                .and_then(|task| task.evaluation_lifecycle.as_ref())
                .and_then(|lifecycle| lifecycle.linked_flip_verdict.as_ref())
                .is_some();
        if !flip_linked {
            continue;
        }
        let eval_task_id = format!(".evaluate-{source_id}");
        modified |= install_completed_legacy_plan(graph, &eval_task_id, eval);
        modified |= mark_satellite_verdict(graph, &eval_task_id, eval);

        let source = graph.get_task_mut(&source_id).expect("collected source");
        source
            .evaluation_lifecycle
            .get_or_insert(source_lifecycle.clone());
        let consumed = source
            .evaluation_lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.consumed_verdict.clone());
        if let Some(existing) = consumed {
            if existing != eval.verdict_id {
                modified |= lifecycle_conflict(
                    source,
                    format!(
                        "error[WG-EVAL-CONSUMPTION-CONFLICT]: source consumed {} but found {}",
                        existing, eval.verdict_id
                    ),
                );
            }
            continue;
        }
        if !matches!(
            source_snapshot.status,
            Status::PendingEval | Status::FailedPendingEval
        ) {
            continue;
        }

        let hard_reject = eval.score < threshold
            && (source_snapshot.status == Status::FailedPendingEval
                || pending_is_gated(&source_snapshot));
        let retry_source = hard_reject
            && source_snapshot.status == Status::PendingEval
            && auto_rescue
            && max_rescues > 0
            && source_snapshot.rescue_count < max_rescues;

        let lifecycle = source
            .evaluation_lifecycle
            .as_mut()
            .expect("inserted lifecycle");
        lifecycle.linked_flip_verdict = flips.first().map(|verdict| verdict.verdict_id.clone());
        lifecycle.linked_eval_verdict = Some(eval.verdict_id.clone());
        lifecycle.consumed_verdict = Some(eval.verdict_id.clone());
        lifecycle.execution_state = EvaluationExecutionState::Consumed;

        if retry_source {
            source.status = Status::Open;
            source.rescue_count = source.rescue_count.saturating_add(1);
            source.assigned = None;
            source.started_at = None;
            source.completed_at = None;
            source.failure_reason = None;
        } else if hard_reject {
            source.status = Status::Failed;
            source.retry_count = source.retry_count.saturating_add(1);
            source.failure_reason = Some(format!(
                "evaluation verdict {} rejected: score={:.2} < threshold={:.2}",
                eval.verdict_id, eval.score, threshold
            ));
            source.completed_at = Some(Utc::now().to_rfc3339());
        } else {
            source.status = Status::Done;
            source.rescued |= source_snapshot.status == Status::FailedPendingEval;
            source.completed_at = Some(Utc::now().to_rfc3339());
        }
        source.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: Some("eval-lifecycle-reconcile".to_string()),
            user: None,
            message: format!(
                "Consumed durable verdict {} exactly once: score={:.2}, outcome={}",
                eval.verdict_id, eval.score, source.status
            ),
        });
        modified = true;

        if retry_source {
            let rebound_source = graph
                .get_task(&source_id)
                .expect("source still exists")
                .clone();
            modified |= rearm_satellites_for_source(graph, &rebound_source);
        }
    }
    modified
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ReasoningLevel, RoleModelConfig};

    fn source() -> Task {
        Task {
            id: "source".into(),
            title: "source".into(),
            ..Task::default()
        }
    }

    #[test]
    fn handler_first_plan_round_trips_for_supported_systems() {
        for route in [
            "codex:gpt-5.5",
            "pi:openai-codex:gpt-5.6-sol",
            "pi:openrouter:z-ai/glm-5.2",
            "nex:openrouter:z-ai/glm-5.2",
            "claude:haiku",
        ] {
            let mut config = Config::default();
            config.models.evaluator = Some(RoleModelConfig {
                provider: None,
                model: Some(route.into()),
                tier: None,
                endpoint: Some("named-endpoint".into()),
                reasoning: Some(ReasoningLevel::High),
            });
            let plan = build_plan(
                &config,
                &source(),
                ".evaluate-source",
                DispatchSelectionSource::ScaffoldConfig,
            )
            .unwrap();
            assert_eq!(plan.calls[0].route, route);
            assert_eq!(plan.calls[0].endpoint.as_deref(), Some("named-endpoint"));
            assert_eq!(plan.calls[0].reasoning, Some(ReasoningLevel::High));
            validate_plan(&serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn legacy_explicit_codex_role_is_canonicalized_before_persistence() {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            provider: Some("codex".into()),
            model: Some("gpt-5.4-mini".into()),
            tier: None,
            endpoint: None,
            reasoning: None,
        });
        let plan = build_plan(
            &config,
            &source(),
            ".evaluate-source",
            DispatchSelectionSource::ScaffoldConfig,
        )
        .unwrap();
        assert_eq!(plan.calls[0].route, "codex:gpt-5.4-mini");
        assert_eq!(plan.calls[0].system.handler, "codex");
    }

    #[test]
    fn flip_plan_keeps_distinct_routes() {
        let mut config = Config::default();
        config.models.flip_inference = Some(RoleModelConfig {
            provider: None,
            model: Some("codex:gpt-5.5".into()),
            tier: None,
            endpoint: None,
            reasoning: None,
        });
        config.models.flip_comparison = Some(RoleModelConfig {
            provider: None,
            model: Some("pi:openai-codex:gpt-5.6-sol".into()),
            tier: None,
            endpoint: None,
            reasoning: None,
        });
        let plan = build_plan(
            &config,
            &source(),
            ".flip-source",
            DispatchSelectionSource::ScaffoldConfig,
        )
        .unwrap();
        assert_eq!(plan.calls[0].route, "codex:gpt-5.5");
        assert_eq!(plan.calls[1].route, "pi:openai-codex:gpt-5.6-sol");
    }

    #[test]
    fn ambiguous_openrouter_split_fails_closed() {
        let satellite = Task {
            id: ".evaluate-source".into(),
            title: "eval".into(),
            model: Some("z-ai/glm-5.2".into()),
            provider: Some("openrouter".into()),
            ..Task::default()
        };
        let error = migrate_legacy_plan(&source(), &satellite)
            .unwrap_err()
            .to_string();
        assert!(error.contains("AMBIGUOUS"));
    }

    fn planned_satellite(id: &str, source: &Task) -> Task {
        let mut config = Config::default();
        config.models.evaluator = Some(RoleModelConfig {
            provider: None,
            model: Some("codex:gpt-5.5".into()),
            tier: None,
            endpoint: None,
            reasoning: None,
        });
        config.models.flip_inference = config.models.evaluator.clone();
        config.models.flip_comparison = config.models.evaluator.clone();
        let plan =
            build_plan(&config, source, id, DispatchSelectionSource::ScaffoldConfig).unwrap();
        Task {
            id: id.into(),
            title: id.into(),
            status: Status::InProgress,
            agency_dispatch: Some(plan),
            ..Task::default()
        }
    }

    fn verdict(source: &Task, stage: AgencyStage, score: f64) -> DurableEvalVerdict {
        let pipeline = EvaluationLifecycle::for_source(source);
        let suffix = if stage == AgencyStage::Evaluate {
            "eval"
        } else {
            "flip"
        };
        DurableEvalVerdict {
            schema: EVAL_LIFECYCLE_SCHEMA,
            verdict_id: format!("verdict-{suffix}"),
            verdict_digest: String::new(),
            evaluation_id: format!("evaluation-{suffix}"),
            pipeline_id: pipeline.pipeline_id,
            source_task: source.id.clone(),
            source_attempt: pipeline.source_attempt,
            stage,
            producer_run_id: "run-1".into(),
            score,
            evaluation_digest_schema: EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA,
            evaluation_digest: format!("b3:{suffix}"),
            created_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn durable_verdict_consumption_is_atomic_and_idempotent() {
        let mut source = source();
        source.status = Status::FailedPendingEval;
        source.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&source));
        let flip = planned_satellite(".flip-source", &source);
        let eval = planned_satellite(".evaluate-source", &source);
        let flip_verdict = verdict(&source, AgencyStage::FlipComparison, 1.0);
        let eval_verdict = verdict(&source, AgencyStage::Evaluate, 0.9);
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source));
        graph.add_node(crate::graph::Node::Task(flip));
        graph.add_node(crate::graph::Node::Task(eval));

        assert!(reconcile_durable_verdicts(
            &mut graph,
            &[flip_verdict.clone(), eval_verdict.clone()],
            0.7,
            true,
            3,
            |_| true,
        ));
        let source = graph.get_task("source").unwrap();
        assert_eq!(source.status, Status::Done);
        assert_eq!(
            source
                .evaluation_lifecycle
                .as_ref()
                .unwrap()
                .consumed_verdict
                .as_deref(),
            Some(eval_verdict.verdict_id.as_str())
        );
        assert!(!reconcile_durable_verdicts(
            &mut graph,
            &[flip_verdict, eval_verdict],
            0.7,
            true,
            3,
            |_| true,
        ));
    }

    #[test]
    fn advisory_low_score_completes_but_gated_score_retries_exact_plan() {
        let mut advisory = source();
        advisory.status = Status::PendingEval;
        advisory.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&advisory));
        let advisory_eval = planned_satellite(".evaluate-source", &advisory);
        let low = verdict(&advisory, AgencyStage::Evaluate, 0.2);
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(advisory));
        graph.add_node(crate::graph::Node::Task(advisory_eval));
        assert!(reconcile_durable_verdicts(
            &mut graph,
            &[low],
            0.7,
            true,
            3,
            |_| false,
        ));
        assert_eq!(graph.get_task("source").unwrap().status, Status::Done);

        let mut gated = source();
        gated.status = Status::PendingEval;
        gated.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&gated));
        let old_pipeline = gated
            .evaluation_lifecycle
            .as_ref()
            .unwrap()
            .pipeline_id
            .clone();
        let gated_eval = planned_satellite(".evaluate-source", &gated);
        let old_route = gated_eval.agency_dispatch.as_ref().unwrap().calls[0]
            .route
            .clone();
        let low = verdict(&gated, AgencyStage::Evaluate, 0.2);
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(gated));
        graph.add_node(crate::graph::Node::Task(gated_eval));
        assert!(reconcile_durable_verdicts(
            &mut graph,
            &[low.clone()],
            0.7,
            true,
            3,
            |_| true,
        ));
        let source = graph.get_task("source").unwrap();
        assert_eq!(source.status, Status::Open);
        assert_eq!(source.rescue_count, 1);
        let eval = graph.get_task(".evaluate-source").unwrap();
        let rebound = eval.agency_dispatch.as_ref().unwrap();
        assert_eq!(rebound.calls[0].route, old_route);
        assert_ne!(rebound.pipeline_id, old_pipeline);
        assert_eq!(eval.status, Status::Open);
        assert!(!reconcile_durable_verdicts(
            &mut graph,
            &[low],
            0.7,
            true,
            3,
            |_| true,
        ));
    }

    #[test]
    fn explicit_source_retry_rebinds_existing_satellites_without_route_drift() {
        let mut old_source = source();
        old_source.status = Status::Failed;
        old_source.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&old_source));
        old_source
            .evaluation_lifecycle
            .as_mut()
            .unwrap()
            .consumed_verdict = Some("verdict-old".into());
        let eval = planned_satellite(".evaluate-source", &old_source);
        let old_plan = eval.agency_dispatch.as_ref().unwrap().clone();
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(old_source.clone()));
        graph.add_node(crate::graph::Node::Task(eval));

        let mut retry_source = old_source;
        retry_source.status = Status::Open;
        retry_source.retry_count = 1;
        assert!(rearm_satellites_for_source(&mut graph, &retry_source));
        let rebound = graph
            .get_task(".evaluate-source")
            .unwrap()
            .agency_dispatch
            .as_ref()
            .unwrap();
        assert_eq!(rebound.calls, old_plan.calls);
        assert_ne!(rebound.pipeline_id, old_plan.pipeline_id);
        assert_eq!(rebound.source_attempt, 2);
    }

    #[test]
    fn claimed_transport_retry_budget_is_bounded() {
        let source = source();
        let mut lifecycle = EvaluationLifecycle::for_source(&source);
        assert_eq!(lifecycle.reserve_transport_attempt().unwrap(), 1);
        lifecycle.execution_state = EvaluationExecutionState::Waiting;
        assert_eq!(lifecycle.reserve_transport_attempt().unwrap(), 2);
        assert!(lifecycle.reserve_transport_attempt().is_err());
        assert_eq!(lifecycle.transport_attempts, 2);
        assert_eq!(lifecycle.execution_state, EvaluationExecutionState::Blocked);
    }

    #[test]
    fn historical_claimed_row_is_never_rearmed_as_preclaim() {
        let mut source = source();
        source.status = Status::FailedPendingEval;
        let satellite = Task {
            id: ".evaluate-source".into(),
            title: "eval".into(),
            status: Status::Incomplete,
            model: Some("gpt-5.5".into()),
            provider: Some("codex".into()),
            spawn_failures: 5,
            started_at: Some(Utc::now().to_rfc3339()),
            ..Task::default()
        };
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source));
        graph.add_node(crate::graph::Node::Task(satellite));
        assert!(!repair_historical_rows(&mut graph));
        let row = graph.get_task(".evaluate-source").unwrap();
        assert_eq!(row.status, Status::Incomplete);
        assert!(row.agency_dispatch.is_none());
    }

    #[test]
    fn historical_codex_preclaim_repair_is_bounded_and_idempotent() {
        let mut source = source();
        source.status = Status::FailedPendingEval;
        let satellite = Task {
            id: ".evaluate-source".into(),
            title: "eval".into(),
            status: Status::Incomplete,
            model: Some("gpt-5.5".into()),
            provider: Some("codex".into()),
            spawn_failures: 5,
            ..Task::default()
        };
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source));
        graph.add_node(crate::graph::Node::Task(satellite));
        assert!(repair_historical_rows(&mut graph));
        let repaired = graph.get_task(".evaluate-source").unwrap();
        assert_eq!(repaired.status, Status::Open);
        assert_eq!(repaired.spawn_failures, 0);
        assert_eq!(repaired.model.as_deref(), Some("codex:gpt-5.5"));
        assert!(!repair_historical_rows(&mut graph));
    }

    #[test]
    fn unambiguous_legacy_evaluation_migrates_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = source();
        source.status = Status::FailedPendingEval;
        source.started_at = Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        source.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&source));
        let satellite = planned_satellite(".evaluate-source", &source);
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source.clone()));
        graph.add_node(crate::graph::Node::Task(satellite));
        crate::parser::save_graph(&graph, &dir.path().join("graph.jsonl")).unwrap();
        let evaluation = Evaluation {
            id: "legacy-eval-source".into(),
            task_id: source.id.clone(),
            agent_id: "agent-legacy".into(),
            role_id: "role".into(),
            tradeoff_id: "tradeoff".into(),
            score: 0.9,
            dimensions: std::collections::HashMap::new(),
            notes: "legacy but unambiguous".into(),
            evaluator: "codex:gpt-5.5".into(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("codex:gpt-5.5".into()),
            source: "llm".into(),
            loop_iteration: 0,
        };
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();
        assert_eq!(migrate_unambiguous_legacy_verdicts(dir.path()).unwrap(), 1);
        assert_eq!(migrate_unambiguous_legacy_verdicts(dir.path()).unwrap(), 0);
        assert_eq!(load_durable_verdicts(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn durable_verdict_replay_ignores_observational_time_and_run_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source = source();
        let mut satellite = planned_satellite(".evaluate-source", &source);
        satellite.assigned = Some("agent-original".into());
        let evaluation = Evaluation {
            id: "eval-source-replay".into(),
            task_id: source.id.clone(),
            agent_id: "agent-original".into(),
            role_id: "role".into(),
            tradeoff_id: "tradeoff".into(),
            score: 0.9,
            dimensions: std::collections::HashMap::new(),
            notes: "same semantic evidence".into(),
            evaluator: "codex:gpt-5.5".into(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("codex:gpt-5.5".into()),
            source: "llm".into(),
            loop_iteration: 0,
        };
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();
        let first = write_durable_verdict(
            dir.path(),
            &source,
            &satellite,
            AgencyStage::Evaluate,
            &evaluation,
        )
        .unwrap();
        // Re-encode the record exactly as a pre-schema-2 writer did. The
        // durable evaluation's pretty member order reconstructs the original
        // compact digest losslessly.
        let evaluation_path = dir
            .path()
            .join("agency/evaluations")
            .join(format!("{}.json", evaluation.id));
        let mut legacy: DurableEvalVerdict =
            serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
        legacy.evaluation_digest_schema = 1;
        legacy.evaluation_digest =
            digest_bytes(&compact_durable_json(&fs::read(evaluation_path).unwrap()));
        legacy.verdict_digest = compute_verdict_digest(&legacy).unwrap();
        fs::write(&first, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let first_bytes = fs::read(&first).unwrap();

        satellite.assigned = Some("agent-restarted-wrapper".into());
        let replay = write_durable_verdict(
            dir.path(),
            &source,
            &satellite,
            AgencyStage::Evaluate,
            &evaluation,
        )
        .unwrap();
        assert_eq!(replay, first);
        assert_eq!(fs::read(&replay).unwrap(), first_bytes);
        assert_eq!(load_durable_verdicts(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn completed_claimed_legacy_evaluator_migrates_once_without_semantic_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut source = source();
        source.status = Status::PendingEval;
        source.started_at = Some((now - chrono::Duration::seconds(30)).to_rfc3339());
        let satellite = Task {
            id: ".evaluate-source".into(),
            title: "legacy completed evaluator".into(),
            status: Status::Done,
            model: Some("pi:openai-codex:gpt-5.6-terra".into()),
            assigned: Some("agent-legacy".into()),
            started_at: Some((now - chrono::Duration::seconds(20)).to_rfc3339()),
            completed_at: Some((now - chrono::Duration::seconds(5)).to_rfc3339()),
            ..Task::default()
        };
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source.clone()));
        graph.add_node(crate::graph::Node::Task(satellite));
        crate::parser::save_graph(&graph, &dir.path().join("graph.jsonl")).unwrap();
        let evaluation = Evaluation {
            id: "legacy-completed-eval".into(),
            task_id: source.id.clone(),
            agent_id: "agent-legacy".into(),
            role_id: "role".into(),
            tradeoff_id: "tradeoff".into(),
            score: 0.91,
            dimensions: std::collections::HashMap::new(),
            notes: "one post-start evaluation".into(),
            evaluator: "pi:openai-codex:gpt-5.6-terra".into(),
            timestamp: (now - chrono::Duration::seconds(4)).to_rfc3339(),
            model: Some("pi:openai-codex:gpt-5.6-terra".into()),
            source: "llm".into(),
            loop_iteration: 0,
        };
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();

        assert_eq!(migrate_unambiguous_legacy_verdicts(dir.path()).unwrap(), 1);
        let verdicts = load_durable_verdicts(dir.path()).unwrap();
        assert_eq!(verdicts.len(), 1);

        // Simulate a daemon restart after durable migration but before the graph
        // transaction. Claimed-row preflight repair remains correctly disabled;
        // verified evidence performs metadata backfill and consumption instead.
        let mut restarted = crate::parser::load_graph(&dir.path().join("graph.jsonl")).unwrap();
        assert!(!repair_historical_rows(&mut restarted));
        assert!(reconcile_durable_verdicts(
            &mut restarted,
            &verdicts,
            0.7,
            true,
            3,
            |_| false,
        ));
        let source = restarted.get_task("source").unwrap();
        assert_eq!(source.status, Status::Done);
        assert_eq!(
            source
                .evaluation_lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.consumed_verdict.as_deref()),
            Some(verdicts[0].verdict_id.as_str())
        );
        let evaluator = restarted.get_task(".evaluate-source").unwrap();
        assert!(evaluator.agency_dispatch.is_some());
        assert_eq!(
            evaluator
                .evaluation_lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.linked_eval_verdict.as_deref()),
            Some(verdicts[0].verdict_id.as_str())
        );

        crate::parser::save_graph(&restarted, &dir.path().join("graph.jsonl")).unwrap();
        assert_eq!(migrate_unambiguous_legacy_verdicts(dir.path()).unwrap(), 0);
        let mut second_restart =
            crate::parser::load_graph(&dir.path().join("graph.jsonl")).unwrap();
        assert!(!reconcile_durable_verdicts(
            &mut second_restart,
            &load_durable_verdicts(dir.path()).unwrap(),
            0.7,
            true,
            3,
            |_| false,
        ));
        assert_eq!(
            crate::agency::load_all_evaluations_or_warn(&dir.path().join("agency/evaluations"))
                .len(),
            1
        );
    }

    #[test]
    fn incident_legacy_verdict_verifies_from_durable_member_order() {
        // Exact bytes from the 2026-07-19 Pi/Terra live incident. The writer's
        // dimensions order was correctness, completeness, ...; deserializing
        // into a newly seeded HashMap changed that order, so schema 1's old
        // `to_vec(&evaluation)` reader rejected its own valid verdict.
        let dir = tempfile::tempdir().unwrap();
        let evaluations = dir.path().join("agency/evaluations");
        let verdicts = verdicts_dir(dir.path());
        fs::create_dir_all(&evaluations).unwrap();
        fs::create_dir_all(&verdicts).unwrap();
        fs::write(
            evaluations.join("evaluation.json"),
            include_bytes!("../tests/fixtures/live_eval_digest_mismatch/evaluation.json"),
        )
        .unwrap();
        fs::write(
            verdicts.join("verdict-evalp-499fd5ddac13a90963448679-evaluate-97e39ad55786b39b.json"),
            include_bytes!("../tests/fixtures/live_eval_digest_mismatch/verdict.json"),
        )
        .unwrap();

        let loaded = load_durable_verdicts(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].evaluation_digest_schema, 1);
        assert_eq!(loaded[0].score, 0.88);
    }

    #[test]
    fn durable_verdict_load_verifies_record_and_evaluation_digests() {
        let dir = tempfile::tempdir().unwrap();
        let source = source();
        let satellite = planned_satellite(".evaluate-source", &source);
        let mut evaluation = Evaluation {
            id: "eval-source-fixed".into(),
            task_id: source.id.clone(),
            agent_id: "agent-1".into(),
            role_id: "role".into(),
            tradeoff_id: "tradeoff".into(),
            score: 0.9,
            dimensions: std::collections::HashMap::new(),
            notes: "valid".into(),
            evaluator: "codex:gpt-5.5".into(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("codex:gpt-5.5".into()),
            source: "llm".into(),
            loop_iteration: 0,
        };
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();
        let verdict_path = write_durable_verdict(
            dir.path(),
            &source,
            &satellite,
            AgencyStage::Evaluate,
            &evaluation,
        )
        .unwrap();
        let loaded = load_durable_verdicts(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].evaluation_digest_schema,
            EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA
        );

        let original = fs::read(&verdict_path).unwrap();
        let mut tampered: serde_json::Value = serde_json::from_slice(&original).unwrap();
        tampered["score"] = serde_json::json!(0.1);
        fs::write(&verdict_path, serde_json::to_vec_pretty(&tampered).unwrap()).unwrap();
        assert!(
            load_durable_verdicts(dir.path())
                .unwrap_err()
                .to_string()
                .contains("INTEGRITY")
        );

        fs::write(&verdict_path, original).unwrap();
        evaluation.notes = "tampered after verdict".into();
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();
        assert!(
            load_durable_verdicts(dir.path())
                .unwrap_err()
                .to_string()
                .contains("EVIDENCE")
        );
    }

    // -------------------------------------------------------------------------
    // engine-eval-verdict: incremental migration/cache, retention, and the
    // synthetic 10k perf regression. The pre-fix hot path re-scanned EVERY file
    // in agency/evaluations/ once per durable verdict — O(verdicts × all-history)
    // — which wedged the coordinator for an hour at ~9.5k files.
    // -------------------------------------------------------------------------

    fn make_evaluation(id: &str, task_id: &str, score: f64) -> Evaluation {
        Evaluation {
            id: id.into(),
            task_id: task_id.into(),
            agent_id: "agent".into(),
            role_id: "role".into(),
            tradeoff_id: "tradeoff".into(),
            score,
            dimensions: std::collections::HashMap::new(),
            notes: "synthetic".into(),
            evaluator: "codex:gpt-5.5".into(),
            timestamp: Utc::now().to_rfc3339(),
            model: Some("codex:gpt-5.5".into()),
            source: "llm".into(),
            loop_iteration: 0,
        }
    }

    fn set_mtime(path: &Path, secs: u64) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn migration_marker_prevents_rescan_until_dir_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = source();
        source.status = Status::FailedPendingEval;
        source.started_at = Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        source.evaluation_lifecycle = Some(EvaluationLifecycle::for_source(&source));
        let satellite = planned_satellite(".evaluate-source", &source);
        let mut graph = WorkGraph::new();
        graph.add_node(crate::graph::Node::Task(source.clone()));
        graph.add_node(crate::graph::Node::Task(satellite));
        crate::parser::save_graph(&graph, &dir.path().join("graph.jsonl")).unwrap();
        let evaluation = make_evaluation("legacy-eval-source", &source.id, 0.9);
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();

        // First pass scans and migrates the unambiguous legacy verdict.
        let first = migrate_unambiguous_legacy_verdicts_incremental(dir.path()).unwrap();
        assert!(first.scanned);
        assert_eq!(first.migrated, 1);

        // Nothing changed → the marker short-circuits the whole scan (O(new)).
        let second = migrate_unambiguous_legacy_verdicts_incremental(dir.path()).unwrap();
        assert!(!second.scanned, "unchanged dirs must not re-scan");
        assert_eq!(second.migrated, 0);

        // A new evaluation file changes the fingerprint → the scan resumes.
        let another = make_evaluation("another-eval", "other-task", 0.5);
        crate::agency::save_evaluation(&another, &dir.path().join("agency/evaluations")).unwrap();
        let third = migrate_unambiguous_legacy_verdicts_incremental(dir.path()).unwrap();
        assert!(third.scanned, "a changed evaluations dir must re-scan");
    }

    #[test]
    fn durable_verdict_cache_matches_uncached_and_refreshes_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let source = source();
        let satellite = planned_satellite(".evaluate-source", &source);
        let evaluation = make_evaluation("eval-cache", &source.id, 0.9);
        crate::agency::save_evaluation(&evaluation, &dir.path().join("agency/evaluations"))
            .unwrap();
        write_durable_verdict(
            dir.path(),
            &source,
            &satellite,
            AgencyStage::Evaluate,
            &evaluation,
        )
        .unwrap();

        let uncached = load_durable_verdicts(dir.path()).unwrap();
        let cached = load_durable_verdicts_cached(dir.path()).unwrap();
        assert_eq!(uncached, cached, "cache must match a full verified load");
        assert_eq!(cached.len(), 1);
        assert!(
            verdict_cache_path(dir.path()).exists(),
            "cache file is written on a miss"
        );

        // A warm hit returns the same already-verified result.
        assert_eq!(load_durable_verdicts_cached(dir.path()).unwrap(), cached);

        // Adding a second verdict changes the verdicts-dir fingerprint → the
        // cache refreshes rather than serving a stale set.
        let flip_eval = make_evaluation("eval-cache-flip", &source.id, 1.0);
        crate::agency::save_evaluation(&flip_eval, &dir.path().join("agency/evaluations"))
            .unwrap();
        let flip_sat = planned_satellite(".flip-source", &source);
        write_durable_verdict(
            dir.path(),
            &source,
            &flip_sat,
            AgencyStage::FlipComparison,
            &flip_eval,
        )
        .unwrap();
        assert_eq!(load_durable_verdicts_cached(dir.path()).unwrap().len(), 2);
    }

    #[test]
    fn retention_archives_oldest_keeps_newest_and_protects_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let evaluations = dir.path().join("agency/evaluations");

        // A referenced evaluation (given the oldest mtime) — protected because a
        // durable verdict still needs it, even though it is beyond keep_newest.
        let source = source();
        let satellite = planned_satellite(".evaluate-source", &source);
        let referenced = make_evaluation("eval-referenced", &source.id, 0.9);
        crate::agency::save_evaluation(&referenced, &evaluations).unwrap();
        write_durable_verdict(
            dir.path(),
            &source,
            &satellite,
            AgencyStage::Evaluate,
            &referenced,
        )
        .unwrap();
        set_mtime(&evaluations.join("eval-referenced.json"), 1_000_000);

        // Four unreferenced evaluations with strictly newer mtimes.
        for i in 0..4u64 {
            let ev = make_evaluation(&format!("eval-extra-{i}"), "other", 0.1);
            crate::agency::save_evaluation(&ev, &evaluations).unwrap();
            set_mtime(&evaluations.join(format!("eval-extra-{i}.json")), 2_000_000 + i);
        }

        // 5 files, keep_newest = 1. The newest survives; of the 4 remaining
        // candidates the 3 unreferenced archive and the referenced-oldest is
        // protected.
        let report = archive_stale_evaluations(dir.path(), 1).unwrap();
        assert_eq!(report.kept, 1);
        assert_eq!(report.protected, 1);
        assert_eq!(report.archived, 3);
        assert!(
            evaluations.join("eval-referenced.json").exists(),
            "a verdict-referenced evaluation is never archived"
        );
        // Archive-not-delete: the 3 moved files live under evaluations-archive/.
        let archive = dir.path().join("agency/evaluations-archive");
        assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 3);

        // keep_newest = 0 disables retention entirely.
        assert_eq!(archive_stale_evaluations(dir.path(), 0).unwrap().archived, 0);
    }

    /// Write a valid schema-2 durable verdict directly (bypasses the single-source
    /// verdict-id collision in `write_durable_verdict`) so many verdicts can point
    /// at distinct evaluations in the synthetic fixture.
    fn write_synthetic_verdict(dir: &Path, index: usize, evaluation: &Evaluation) {
        let eval_path = dir
            .join("agency/evaluations")
            .join(format!("{}.json", evaluation.id));
        let bytes = fs::read(&eval_path).unwrap();
        let mut verdict = DurableEvalVerdict {
            schema: EVAL_LIFECYCLE_SCHEMA,
            verdict_id: format!("verdict-{index:05}"),
            verdict_digest: String::new(),
            evaluation_id: evaluation.id.clone(),
            pipeline_id: format!("pipeline-{index}"),
            source_task: evaluation.task_id.clone(),
            source_attempt: 0,
            stage: AgencyStage::Evaluate,
            producer_run_id: "run".into(),
            score: evaluation.score,
            evaluation_digest_schema: EVALUATION_DIGEST_DURABLE_BYTES_SCHEMA,
            evaluation_digest: digest_bytes(&bytes),
            created_at: Utc::now().to_rfc3339(),
        };
        verdict.verdict_digest = compute_verdict_digest(&verdict).unwrap();
        let verdicts = verdicts_dir(dir);
        fs::create_dir_all(&verdicts).unwrap();
        fs::write(
            verdicts.join(format!("{}.json", verdict.verdict_id)),
            serde_json::to_vec_pretty(&verdict).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn synthetic_10k_evaluations_link_under_budget_and_cache_is_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let evaluations = dir.path().join("agency/evaluations");
        fs::create_dir_all(&evaluations).unwrap();
        // Empty graph so the incremental migration front door has something to
        // read (no pending tasks → it early-returns and just writes the marker).
        crate::parser::save_graph(&WorkGraph::new(), &dir.path().join("graph.jsonl")).unwrap();

        const TOTAL: usize = 10_000;
        const REFERENCED: usize = 200;
        for i in 0..TOTAL {
            let ev = make_evaluation(&format!("eval-{i:05}"), "bulk", 0.5);
            // Direct write (no per-file fsync) keeps seeding 10k files fast.
            fs::write(
                evaluations.join(format!("eval-{i:05}.json")),
                serde_json::to_vec_pretty(&ev).unwrap(),
            )
            .unwrap();
            if i < REFERENCED {
                write_synthetic_verdict(dir.path(), i, &ev);
            }
        }

        // Cold: full indexed load over a 10k-file evaluations dir. Pre-fix this
        // was ~200 × 10k ≈ 2M file reads; now it is one dir listing + 200 reads.
        let cold_start = std::time::Instant::now();
        let cold = load_durable_verdicts_cached(dir.path()).unwrap();
        let cold_ms = cold_start.elapsed().as_millis();
        assert_eq!(cold.len(), REFERENCED);

        // Warm: fingerprints unchanged → served from cache, no evidence scan.
        let warm_start = std::time::Instant::now();
        let warm = load_durable_verdicts_cached(dir.path()).unwrap();
        let warm_ms = warm_start.elapsed().as_millis();
        assert_eq!(warm, cold);

        // Migration marker: first pass scans, second short-circuits.
        let first = migrate_unambiguous_legacy_verdicts_incremental(dir.path()).unwrap();
        let second = migrate_unambiguous_legacy_verdicts_incremental(dir.path()).unwrap();
        assert!(first.scanned);
        assert!(!second.scanned, "marker must prevent a re-scan");

        eprintln!(
            "[engine-eval-verdict] 10k fixture: cold link {cold_ms}ms, warm(cached) link {warm_ms}ms"
        );
        // Generous budget: the point is it finishes in well under the 7+ minutes
        // that wedged the live dispatcher.
        let budget = std::time::Duration::from_secs(60);
        assert!(
            cold_start.elapsed() < budget,
            "cold link must finish under budget, took {cold_ms}ms"
        );
        assert!(
            warm_ms <= cold_ms,
            "warm cached link must not be slower than the cold scan"
        );
    }
}
