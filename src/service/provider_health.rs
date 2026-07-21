//! Provider health detection and auto-pause system
//!
//! Tracks provider failure patterns and implements circuit-breaker logic
//! to pause the service when providers repeatedly fail with fatal errors.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Plain-language operator alert emitted when the provider pause trips. Mirrors
/// the spawn breaker's alert voice: no jargon, says what happened and that it
/// will recover on its own.
pub const PROVIDER_PAUSED_ALERT_TEXT: &str =
    "⚠️ The family team can't reach its AI right now — usually a login issue. \
The task runner has paused and will keep checking; it resumes on its own once the connection is back.";

/// Plain-language operator alert emitted when the provider pause auto-resumes.
pub const PROVIDER_RESUMED_ALERT_TEXT: &str =
    "✅ The family team can reach its AI again — the task runner has resumed and is back to work.";

use crate::config::{ExecutionSystemKey, execution_system_key, parse_model_spec};
use crate::dispatch::SpawnPlan;

/// Stable handler + wire + endpoint identity used by the health breaker.
///
/// The fingerprint is derived only from endpoint configuration metadata. API
/// key values are deliberately excluded, so this key is safe to persist and
/// print. Two Nex endpoints on the same wire remain independent breaker
/// domains, while self-authenticating CLI handlers share their own domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRouteKey {
    pub system: ExecutionSystemKey,
    pub endpoint_fingerprint: String,
}

impl HealthRouteKey {
    pub fn from_spawn_plan(plan: &SpawnPlan) -> Self {
        // `SpawnPlan` is the routing authority. Match its already-resolved
        // handler name rather than independently constructing an ExecutorKind
        // (the spawn-site isolation contract forbids a second route planner).
        let executor = plan.executor.as_str();
        let handler = if executor == "native" {
            "nex"
        } else {
            executor
        }
        .to_string();

        let provider = match executor {
            "claude" => "anthropic-cli".to_string(),
            "codex" => "openai-codex-cli".to_string(),
            "native" => plan
                .endpoint
                .as_ref()
                .map(|ep| ep.provider.to_ascii_lowercase())
                .or_else(|| {
                    execution_system_key(&plan.model.raw)
                        .ok()
                        .map(|key| key.provider)
                })
                .unwrap_or_else(|| "oai-compat".to_string()),
            "pi" => pi_provider(&plan.model.raw),
            _ => parse_model_spec(&plan.model.raw)
                .provider
                .unwrap_or_else(|| "self-authenticated-cli".to_string()),
        };

        let endpoint_fingerprint = match &plan.endpoint {
            Some(ep) => {
                // Endpoint identity deliberately excludes every credential
                // field. In particular, `api_key_ref` may legally be a
                // `literal:...` reference, so even hashing it would disclose a
                // stable verifier for secret material. Strip URL user-info,
                // query, and fragment for the same reason.
                let safe_url = ep.url.as_deref().and_then(sanitized_endpoint_url);
                let material = format!(
                    "name={}\nurl={}\nprovider={}",
                    ep.name,
                    safe_url.as_deref().unwrap_or(""),
                    ep.provider,
                );
                format!("b3:{}", blake3::hash(material.as_bytes()).to_hex())
            }
            None => "self-authenticated".to_string(),
        };

        Self {
            system: ExecutionSystemKey { handler, provider },
            endpoint_fingerprint,
        }
    }

    pub fn id(&self) -> String {
        format!(
            "{}|{}|{}",
            self.system.handler, self.system.provider, self.endpoint_fingerprint
        )
    }
}

fn sanitized_endpoint_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    // These setters can fail only for URL schemes that cannot carry
    // credentials; in that case there is nothing sensitive to preserve.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

fn pi_provider(route: &str) -> String {
    let inner = route.strip_prefix("pi:").unwrap_or(route);
    inner
        .split_once(':')
        .map(|(provider, _)| provider)
        .or_else(|| inner.split_once('/').map(|(provider, _)| provider))
        .filter(|provider| !provider.is_empty())
        .unwrap_or("pi-cli")
        .to_ascii_lowercase()
}

/// Stable reason codes emitted only from the trusted `wg done` command
/// boundary. Human-facing error prose is never used by triage to decide
/// whether a refusal is a provider outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionRefusalCode {
    Blocked,
    Deliverable,
    Smoke,
    Verify,
    Validation,
    Worktree,
    Merge,
    AgentBypass,
    Other,
}

/// Typed executor failures used by the pure health classifier and fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "status")]
pub enum ExecutorFailure {
    Authentication,
    Quota,
    HandlerUnavailable,
    Timeout,
    Transport,
    Http(u16),
    TaskInput,
    Unknown,
}

/// Per-run completion provenance. It is stored in `agents/<id>/outcome.json`
/// and accepted by triage only when agent, task, and run identities match the
/// spawn metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentExecutionOutcome {
    pub agent_id: String,
    pub task_id: String,
    pub run_id: String,
    pub recorded_at: String,
    pub outcome: ExecutionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum ExecutionOutcome {
    CompletionAccepted,
    CompletionRefused { code: CompletionRefusalCode },
    ExecutorFailed { failure: ExecutorFailure },
}

pub const AGENT_OUTCOME_FILE: &str = "outcome.json";

/// Classification over typed provenance. `None` means that no provider
/// failure occurred and therefore neither counters nor pause state may move.
pub fn classify_execution_outcome(outcome: &ExecutionOutcome) -> Option<ProviderErrorKind> {
    match outcome {
        ExecutionOutcome::CompletionAccepted | ExecutionOutcome::CompletionRefused { .. } => None,
        ExecutionOutcome::ExecutorFailed { failure } => Some(match failure {
            ExecutorFailure::Authentication
            | ExecutorFailure::Quota
            | ExecutorFailure::HandlerUnavailable => ProviderErrorKind::FatalProvider,
            ExecutorFailure::Timeout | ExecutorFailure::TaskInput => ProviderErrorKind::FatalTask,
            ExecutorFailure::Transport | ExecutorFailure::Unknown => ProviderErrorKind::Transient,
            ExecutorFailure::Http(status) => match *status {
                401..=403 => ProviderErrorKind::FatalProvider,
                408 | 429 | 500..=599 => ProviderErrorKind::Transient,
                400..=499 => ProviderErrorKind::FatalTask,
                _ => ProviderErrorKind::Transient,
            },
        }),
    }
}

/// Map `wg done`'s own bounded error vocabulary to a stable reason code.
/// This helper is intentionally called only inside `commands::done::run`,
/// never on provider/model output.
pub fn completion_refusal_code(error: &str) -> CompletionRefusalCode {
    if error.starts_with("Agents cannot use --skip-") {
        CompletionRefusalCode::AgentBypass
    } else if error.starts_with("Cannot mark '") && error.contains(": blocked by ") {
        CompletionRefusalCode::Blocked
    } else if error.contains("deliverable preflight refused") {
        CompletionRefusalCode::Deliverable
    } else if error.contains("smoke gate") || error.contains("Smoke gate") {
        CompletionRefusalCode::Smoke
    } else if error.starts_with("Verify command failed") {
        CompletionRefusalCode::Verify
    } else if error.contains("integrated validation")
        || error.starts_with("Integrated validation failed")
    {
        CompletionRefusalCode::Validation
    } else if error.starts_with("Worktree has uncommitted changes") {
        CompletionRefusalCode::Worktree
    } else if error.contains("worktree merge conflict") || error.contains("merge conflict") {
        CompletionRefusalCode::Merge
    } else {
        CompletionRefusalCode::Other
    }
}

/// Best-effort atomic outcome write. The caller must be the real spawned agent:
/// task/agent/run identity is validated again by triage against metadata.
pub fn record_done_outcome(dir: &Path, task_id: &str, outcome: ExecutionOutcome) -> Result<()> {
    let Ok(agent_id) = std::env::var("WG_AGENT_ID") else {
        return Ok(());
    };
    let Ok(env_task_id) = std::env::var("WG_TASK_ID") else {
        return Ok(());
    };
    let Ok(run_id) = std::env::var("WG_SPAWN_RUN_ID") else {
        return Ok(());
    };
    if env_task_id != task_id || agent_id.trim().is_empty() || run_id.trim().is_empty() {
        return Ok(());
    }

    // Retry-in-place runs can have an agent id that differs from the reused
    // output-directory name. Resolve the sidecar location through the
    // registry's authoritative output path instead of assuming
    // `agents/<current-id>`.
    let agent_dir = outcome_agent_dir(dir, &agent_id);
    fs::create_dir_all(&agent_dir)?;
    let record = AgentExecutionOutcome {
        agent_id,
        task_id: task_id.to_string(),
        run_id,
        recorded_at: Utc::now().to_rfc3339(),
        outcome,
    };
    let target = agent_dir.join(AGENT_OUTCOME_FILE);
    let temp = agent_dir.join(format!(
        ".{AGENT_OUTCOME_FILE}.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let bytes = serde_json::to_vec_pretty(&record)?;
    let mut file = fs::File::create(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temp, target)?;
    Ok(())
}

fn outcome_agent_dir(dir: &Path, agent_id: &str) -> PathBuf {
    crate::service::registry::AgentRegistry::load(dir)
        .ok()
        .and_then(|registry| registry.get_agent(agent_id).cloned())
        .and_then(|agent| {
            Path::new(&agent.output_file)
                .parent()
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| dir.join("agents").join(agent_id))
}

pub fn load_agent_outcome(output_file: &str) -> Result<Option<AgentExecutionOutcome>> {
    let Some(agent_dir) = Path::new(output_file).parent() else {
        return Ok(None);
    };
    let path = agent_dir.join(AGENT_OUTCOME_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("Failed to parse execution outcome from {}", path.display())
    })?))
}

/// Classification of provider errors based on exit codes and stderr patterns
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderErrorKind {
    /// Temporary network issues, rate limits - should retry with backoff
    Transient,
    /// Provider-level failures: auth, quota, CLI missing - should pause provider
    FatalProvider,
    /// Task-level failures: context too long, malformed input - should fail task
    FatalTask,
}

/// Health status of a single provider/executor combination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealthStatus {
    /// Provider/executor identifier (e.g., "claude", "native:anthropic")
    pub provider_id: String,
    /// Count of consecutive fatal-provider errors
    pub consecutive_failures: u32,
    /// Timestamp of last fatal-provider error
    pub last_failure_at: Option<String>,
    /// Last error message that caused failure
    pub last_error: Option<String>,
    /// Whether this provider is currently paused
    pub is_paused: bool,
    /// When the provider was paused (if paused)
    pub paused_at: Option<String>,
    /// Reason for pausing
    pub pause_reason: Option<String>,
}

impl ProviderHealthStatus {
    pub fn new(provider_id: String) -> Self {
        Self {
            provider_id,
            consecutive_failures: 0,
            last_failure_at: None,
            last_error: None,
            is_paused: false,
            paused_at: None,
            pause_reason: None,
        }
    }

    /// Record a failure for this provider
    pub fn record_failure(&mut self, error_kind: ProviderErrorKind, error_message: String) {
        match error_kind {
            ProviderErrorKind::FatalProvider => {
                self.consecutive_failures += 1;
                self.last_failure_at = Some(Utc::now().to_rfc3339());
                self.last_error = Some(error_message);
            }
            ProviderErrorKind::Transient | ProviderErrorKind::FatalTask => {
                // Don't count transient or task-level errors for provider health
            }
        }
    }

    /// Record a successful task completion - resets failure count
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_failure_at = None;
        self.last_error = None;
    }

    /// Pause this provider with a reason
    pub fn pause(&mut self, reason: String) {
        self.is_paused = true;
        self.paused_at = Some(Utc::now().to_rfc3339());
        self.pause_reason = Some(reason);
    }

    /// Resume this provider (clear pause state)
    pub fn resume(&mut self) {
        self.is_paused = false;
        self.paused_at = None;
        self.pause_reason = None;
        // Also reset failure count on resume
        self.consecutive_failures = 0;
        self.last_failure_at = None;
        self.last_error = None;
    }

    /// Check if this provider should be paused based on failure threshold
    pub fn should_pause(&self, threshold: u32) -> bool {
        !self.is_paused && self.consecutive_failures >= threshold
    }
}

/// Global provider health tracker
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderHealth {
    /// Health status per provider/executor
    pub providers: HashMap<String, ProviderHealthStatus>,
    /// Global service pause state
    pub service_paused: bool,
    /// Why the service is paused (if paused)
    pub pause_reason: Option<String>,
    /// When the service was paused
    pub paused_at: Option<String>,
    /// Auto-resume cooldown period (if configured)
    pub auto_resume_at: Option<String>,
    /// Monotonic counter of how many times the service transitioned
    /// unpaused→paused. Used as the alert episode id so a re-pause is never
    /// deduped against the first pause by the digest store.
    #[serde(default)]
    pub pause_generation: u32,
    /// Armed on the unpaused→paused transition; consumed once by the daemon to
    /// emit the operator alert (mirrors the spawn breaker's one-shot alert).
    #[serde(default)]
    pub pending_pause_alert: bool,
    /// RFC3339 timestamp of the last auto-probe attempt while paused. Drives the
    /// probe cadence so we probe at most once per configured interval.
    #[serde(default)]
    pub last_probe_at: Option<String>,
}

impl ProviderHealth {
    /// Load provider health from disk
    pub fn load(dir: &Path) -> Result<Self> {
        let path = provider_health_path(dir);
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read provider health from {:?}", path))?;
        let health: ProviderHealth = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse provider health from {:?}", path))?;
        Ok(health)
    }

    /// Save provider health to disk
    pub fn save(&self, dir: &Path) -> Result<()> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }

        let path = provider_health_path(dir);
        let content =
            serde_json::to_string_pretty(self).context("Failed to serialize provider health")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write provider health to {:?}", path))?;
        Ok(())
    }

    /// Get or create health status for a provider
    pub fn get_or_create_provider(&mut self, provider_id: &str) -> &mut ProviderHealthStatus {
        self.providers
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderHealthStatus::new(provider_id.to_string()))
    }

    /// Record a failure for a provider
    pub fn record_failure(
        &mut self,
        provider_id: &str,
        error_kind: ProviderErrorKind,
        error_message: String,
    ) {
        let provider = self.get_or_create_provider(provider_id);
        provider.record_failure(error_kind, error_message);
    }

    /// Record a success for a provider
    pub fn record_success(&mut self, provider_id: &str) {
        let provider = self.get_or_create_provider(provider_id);
        provider.record_success();
    }

    /// Check if any providers should be paused and apply pause
    pub fn check_and_apply_pauses(&mut self, threshold: u32, behavior: &str) -> Vec<String> {
        let mut paused_providers = Vec::new();
        // Remember whether the *service* was already paused so we only arm the
        // operator alert on a genuine unpaused→paused edge (not on every triage
        // pass that finds the service already frozen).
        let was_service_paused = self.service_paused;

        for provider in self.providers.values_mut() {
            if provider.should_pause(threshold) {
                let reason = format!(
                    "{} consecutive fatal-provider errors (threshold: {}). Last error: {}",
                    provider.consecutive_failures,
                    threshold,
                    provider.last_error.as_deref().unwrap_or("unknown")
                );
                provider.pause(reason.clone());
                paused_providers.push(provider.provider_id.clone());

                match behavior {
                    "pause" => {
                        // Pause the entire service
                        self.service_paused = true;
                        self.pause_reason = Some(format!(
                            "Provider '{}' failed {} consecutive times",
                            provider.provider_id, provider.consecutive_failures
                        ));
                        self.paused_at = Some(Utc::now().to_rfc3339());
                    }
                    "fallback" => {
                        // Just pause this provider, service continues with others
                        // Fallback logic will be handled by the coordinator
                    }
                    "continue" => {
                        // Just log the failure, don't pause anything
                        provider.resume(); // Immediately unpause
                    }
                    _ => {
                        // Default to pause behavior
                        self.service_paused = true;
                        self.pause_reason = Some(format!(
                            "Provider '{}' failed {} consecutive times",
                            provider.provider_id, provider.consecutive_failures
                        ));
                        self.paused_at = Some(Utc::now().to_rfc3339());
                    }
                }
            }
        }

        // Arm the one-shot operator alert only on a real unpaused→paused edge.
        if self.service_paused && !was_service_paused {
            self.pause_generation = self.pause_generation.saturating_add(1);
            self.pending_pause_alert = true;
            // A fresh pause window starts fresh: the next probe should fire
            // after one interval, not immediately reuse a stale probe stamp.
            self.last_probe_at = None;
        }

        paused_providers
    }

    /// Resume the service (clear global pause state)
    pub fn resume_service(&mut self) {
        self.service_paused = false;
        self.pause_reason = None;
        self.paused_at = None;
        self.auto_resume_at = None;
        // Clear any un-consumed pause alert and the probe stamp so a future
        // pause starts from a clean slate. `pause_generation` is monotonic and
        // deliberately preserved (episode ids must never repeat).
        self.pending_pause_alert = false;
        self.last_probe_at = None;

        // Also resume all paused providers. resume() resets each provider's
        // consecutive_failures to 0 — so one bad window never permanently
        // lowers the effective trip threshold.
        for provider in self.providers.values_mut() {
            if provider.is_paused {
                provider.resume();
            }
        }
    }

    /// Consume the one-shot pause alert. Returns the pause generation (episode
    /// id) exactly once per unpaused→paused edge, then disarms. Mirrors the
    /// spawn breaker's `take_alert()`.
    pub fn take_pause_alert(&mut self) -> Option<u32> {
        if self.pending_pause_alert {
            self.pending_pause_alert = false;
            Some(self.pause_generation)
        } else {
            None
        }
    }

    /// How long the service has been paused, in seconds, relative to `now`.
    /// `None` if not paused or the stamp is unparseable.
    pub fn pause_duration_secs(&self, now: chrono::DateTime<Utc>) -> Option<i64> {
        let paused_at = self.paused_at.as_deref()?;
        let parsed = chrono::DateTime::parse_from_rfc3339(paused_at).ok()?;
        Some(now.signed_duration_since(parsed).num_seconds().max(0))
    }

    /// Whether an auto-probe is due: the service is paused, probing is enabled
    /// (`interval_secs > 0`), and either we have never probed this window or at
    /// least `interval_secs` have elapsed since the last probe.
    pub fn should_probe(&self, now: chrono::DateTime<Utc>, interval_secs: u64) -> bool {
        if !self.service_paused || interval_secs == 0 {
            return false;
        }
        match self.last_probe_at.as_deref() {
            None => true,
            Some(stamp) => match chrono::DateTime::parse_from_rfc3339(stamp) {
                Ok(last) => {
                    now.signed_duration_since(last).num_seconds() >= interval_secs as i64
                }
                // Unparseable stamp → don't get stuck; probe now.
                Err(_) => true,
            },
        }
    }

    /// Record that a probe was just attempted (regardless of outcome).
    pub fn mark_probed(&mut self, now: chrono::DateTime<Utc>) {
        self.last_probe_at = Some(now.to_rfc3339());
    }

    /// Ids of providers currently flagged paused (for targeted probing).
    pub fn paused_provider_ids(&self) -> Vec<String> {
        self.providers
            .values()
            .filter(|p| p.is_paused)
            .map(|p| p.provider_id.clone())
            .collect()
    }

    /// Check if the service should be paused
    pub fn should_pause_spawning(&self) -> bool {
        self.service_paused
    }

    /// Get a summary of current health status
    pub fn get_status_summary(&self) -> String {
        if self.service_paused {
            format!(
                "Service PAUSED: {}",
                self.pause_reason.as_deref().unwrap_or("unknown reason")
            )
        } else {
            let paused_count = self.providers.values().filter(|p| p.is_paused).count();
            let total_count = self.providers.len();
            if paused_count > 0 {
                format!(
                    "Service running, {}/{} providers paused",
                    paused_count, total_count
                )
            } else {
                "Service running, all providers healthy".to_string()
            }
        }
    }
}

/// Path to the provider health state file
fn provider_health_path(dir: &Path) -> PathBuf {
    dir.join("service").join("provider_health.json")
}

/// Classify an error based on exit code and stderr content
pub fn classify_error(exit_code: Option<i32>, stderr: &str) -> ProviderErrorKind {
    // Classification based on the research in provider_error_patterns.md

    // Handle exit codes first
    if let Some(code) = exit_code {
        match code {
            0 => return ProviderErrorKind::FatalTask, // Success but marked as failure - weird state
            124 => return ProviderErrorKind::FatalTask, // Hard timeout - task complexity issue
            143 => return ProviderErrorKind::Transient, // SIGTERM - likely coordinator shutdown
            _ => {}                                   // Continue to stderr analysis
        }
    }

    // Analyze stderr patterns
    let stderr_lower = stderr.to_lowercase();

    // Workflow / task-logic refusals from `wg done` (Fatal-Task, NEVER Fatal-Provider).
    //
    // ROOT CAUSE of the 2026-07-14 flapping: when a satellite agent runs to
    // completion but `wg done` REFUSES for a graph/workflow reason ("blocked by
    // unresolved parent", failed deliverable preflight, uncommitted worktree,
    // etc.), the agent exits non-zero. The AI provider worked perfectly — the
    // agent ran and produced output — the *graph* declined the completion. These
    // refusals must never count against provider health, or a handful of
    // FailedPendingEval parents + their retrying satellites can poison the
    // provider counter into pause after pause even with a fully working provider.
    //
    // This guard runs BEFORE the auth/quota/CLI keyword matching on purpose: a
    // refusal message embeds the blocker task list (whose titles may contain
    // words like "authentication"/"quota") or a git 401 from the merge step, and
    // must not be mistaken for a provider auth failure. If the provider itself
    // were down the agent would never have reached `wg done`, so this text can
    // only appear when the provider is healthy.
    //
    // BUG-A6-1 (done-spoof, inverted): the guard above is TOO eager. A provider
    // can echo a done-looking sentence in its OWN error text while the real
    // failure is a live outage — e.g. `provider said: Cannot mark 'x' as done;
    // Authentication failed HTTP 401`. Blindly returning FatalTask there
    // suppresses the outage: the provider-health counter never trips and the
    // service flaps silently (the exact failure this comment warns about, now
    // running the other way). The typed path (`classify_execution_outcome` on
    // `CompletionRefused`) is spoof-proof via provenance; when only the string
    // survives we must still resist it. So: keep the refusal as FatalTask UNLESS
    // an unambiguous HARD provider signal co-occurs with the done-sentence AND
    // the message is neither a blocked-by-unresolved-task list (whose embedded
    // blocker titles legitimately carry provider words) nor a git/merge-back
    // refusal (whose 401 is a push credential, not the model provider). In that
    // narrow case, fall through to the provider matchers below.
    if is_wg_done_refusal(&stderr_lower) {
        let outage_spoof = has_hard_provider_signal(&stderr_lower)
            && !is_blocked_task_list_refusal(&stderr_lower)
            && !is_git_merge_refusal(&stderr_lower);
        if !outage_spoof {
            return ProviderErrorKind::FatalTask;
        }
        // else: a real provider outage wearing a done-sentence — fall through to
        // the auth/quota/CLI matchers so the outage is classified as such.
    }

    // Auth/Authorization failures (Fatal-Provider)
    if stderr_lower.contains("authentication failed")
        || stderr_lower.contains("http 401")
        || stderr_lower.contains("http 402")
        || stderr_lower.contains("access denied")
        || stderr_lower.contains("http 403")
        || stderr_lower.contains("check your api key")
        || stderr_lower.contains("insufficient permissions")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // CLI/Infrastructure failures (Fatal-Provider)
    if stderr_lower.contains("claude' cli is required but was not found")
        || stderr_lower.contains("command not found")
        || stderr_lower.contains("failed to spawn claude cli")
        || stderr_lower.contains("failed to create tokio runtime")
        || stderr_lower.contains("failed to create anthropic client")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // Quota/Billing failures (Fatal-Provider)
    if stderr_lower.contains("quota")
        || stderr_lower.contains("balance exhausted")
        || stderr_lower.contains("monthly")
        || stderr_lower.contains("daily")
        || stderr_lower.contains("cost cap")
        || stderr_lower.contains("billing")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // Rate limiting (Transient)
    if stderr_lower.contains("http 429")
        || stderr_lower.contains("rate limit")
        || stderr_lower.contains("rate_limit_event")
        || stderr_lower.contains("retry-after")
    {
        return ProviderErrorKind::Transient;
    }

    // Network/Connectivity (Transient)
    if stderr_lower.contains("timeout")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("dns resolution")
        || stderr_lower.contains("network")
        || stderr_lower.contains("timed out")
        || stderr_lower.contains("connection reset")
    {
        return ProviderErrorKind::Transient;
    }

    // Upstream/server outages remain visible as provider diagnostics but are
    // transient: they must be retried, not trip the fatal auth/quota breaker.
    if (500..=599).any(|status| stderr_lower.contains(&format!("http {status}"))) {
        return ProviderErrorKind::Transient;
    }

    // Client/input failures (Fatal-Task). Authentication 401/403 and rate-limit
    // 429 were handled above. A real provider 4xx must not fall through merely
    // because it lacks context-length prose.
    if stderr_lower.contains("http 400")
        || stderr_lower.contains("http 404")
        || stderr_lower.contains("http 405")
        || stderr_lower.contains("http 409")
        || stderr_lower.contains("http 410")
        || stderr_lower.contains("http 413")
        || stderr_lower.contains("http 422")
        || stderr_lower.contains("payload too large")
    {
        return ProviderErrorKind::FatalTask;
    }

    // Empty response (Fatal-Task)
    if stderr_lower.contains("empty response")
        || stderr_lower.contains("failed to parse json")
        || stderr_lower.contains("malformed json")
    {
        return ProviderErrorKind::FatalTask;
    }

    // Lock contention (Transient)
    if stderr_lower.contains("lock contention")
        || stderr_lower.contains("file lock")
        || stderr_lower.contains("index.lock")
        || stderr_lower.contains("cargo.lock")
    {
        return ProviderErrorKind::Transient;
    }

    // Default to transient for unknown errors (conservative approach)
    ProviderErrorKind::Transient
}

/// Detect whether `stderr` (already lowercased) is a `wg done` workflow refusal —
/// i.e. the agent ran to completion but the GRAPH declined to mark the task done.
///
/// These are task-logic failures, not provider failures: the AI provider was
/// reachable and did its job. The phrases below are the exact refusals emitted by
/// `wg done` (see `src/commands/done.rs`): blocked-by-unresolved-parent,
/// deliverable-preflight, disposable-contract, integrated-validation, smoke-gate,
/// verify-gate, uncommitted-worktree, merge-conflict, and the agent skip-flag
/// guards. Matching any one means "count this against the TASK, never the provider."
fn is_wg_done_refusal(stderr_lower: &str) -> bool {
    // The shared prefix of the blocked / preflight / disposable / validation
    // refusals — "Cannot mark '<id>' as done: ...".
    (stderr_lower.contains("cannot mark") && stderr_lower.contains("as done"))
        // Blocked by unresolved parent(s) — the exact scenario that poisoned the
        // counter on 2026-07-14.
        || (stderr_lower.contains("blocked by") && stderr_lower.contains("unresolved task"))
        // Deliverable preflight refused.
        || stderr_lower.contains("deliverable preflight refused")
        // Disposable completion contract unmet.
        || stderr_lower.contains("completion contract is unmet")
        // Integrated validation requires a validation log entry.
        || stderr_lower.contains("requires a validation log entry")
        // Smoke gate refused the completion.
        || stderr_lower.contains("smoke gate refused")
        // Verify gate: the verify command must pass.
        || stderr_lower.contains("the verify command must pass")
        // Agent tried to bypass a gate.
        || stderr_lower.contains("cannot use --skip-smoke")
        || stderr_lower.contains("cannot use --skip-verify")
        // Worktree has uncommitted changes — refusing to mark done.
        || (stderr_lower.contains("uncommitted") && stderr_lower.contains("refusing to mark"))
        // Merge conflict blocking the done-time merge-back.
        || (stderr_lower.contains("merge conflict") && stderr_lower.contains("cannot mark"))
}

/// An UNAMBIGUOUS hard provider outage signal (`stderr` already lowercased).
///
/// These are phrases only the model provider / API transport emits — a real
/// auth rejection, a spent quota, or a billing block — as opposed to a bare word
/// like "authentication" that a blocker task title might carry. Used by
/// `classify_error` to detect a provider outage that has been wrapped in a
/// `wg done`-looking sentence (BUG-A6-1). Kept deliberately narrow: the strong
/// two-word/HTTP-prefixed forms, never the single loose token.
fn has_hard_provider_signal(stderr_lower: &str) -> bool {
    stderr_lower.contains("authentication failed")
        || stderr_lower.contains("http 401")
        || stderr_lower.contains("http 402")
        || stderr_lower.contains("http 403")
        || stderr_lower.contains("check your api key")
        || stderr_lower.contains("insufficient permissions")
        || stderr_lower.contains("balance exhausted")
        || stderr_lower.contains("quota")
        || stderr_lower.contains("cost cap")
        || stderr_lower.contains("billing")
}

/// Whether the refusal is a "blocked by N unresolved task(s)" list (`stderr`
/// already lowercased). Such refusals splice UNRELATED blocker task titles into
/// the message, and those titles may legally contain provider-looking words
/// (`fix-authentication`, `... HTTP 401 quota ...`). The embedded words describe
/// OTHER tasks, never this execution's provider, so a blocked-list refusal is
/// always task logic regardless of any hard-signal co-occurrence.
fn is_blocked_task_list_refusal(stderr_lower: &str) -> bool {
    stderr_lower.contains("blocked by") && stderr_lower.contains("unresolved task")
}

/// Whether the refusal comes from the done-time git merge-back / worktree step
/// (`stderr` already lowercased). A 401 here is a git PUSH credential failure,
/// not the model provider going down, so it stays task logic even when a hard
/// provider signal appears to co-occur.
fn is_git_merge_refusal(stderr_lower: &str) -> bool {
    stderr_lower.contains("merge conflict")
        || stderr_lower.contains("merge-back")
        || stderr_lower.contains("git push")
        || stderr_lower.contains("failed to push")
        || (stderr_lower.contains("uncommitted") && stderr_lower.contains("refusing to mark"))
}

/// Extract provider/executor identifier from configuration
pub fn extract_provider_id(executor: &str, model: Option<&str>) -> String {
    match executor {
        "claude" => "claude".to_string(),
        "native" => {
            if let Some(model) = model {
                if model.contains("gpt") || model.contains("openai") {
                    "native:openai".to_string()
                } else if model.contains("claude") || model.contains("anthropic") {
                    "native:anthropic".to_string()
                } else {
                    format!("native:{}", model.split(':').next().unwrap_or("unknown"))
                }
            } else {
                "native:unknown".to_string()
            }
        }
        "shell" => "shell".to_string(),
        _ => executor.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_classification() {
        // Auth failures
        assert_eq!(
            classify_error(Some(1), "Authentication failed (HTTP 401)"),
            ProviderErrorKind::FatalProvider
        );
        assert_eq!(
            classify_error(Some(1), "Access denied (HTTP 403)"),
            ProviderErrorKind::FatalProvider
        );

        // Rate limiting
        assert_eq!(
            classify_error(Some(1), "HTTP 429: Rate limit exceeded"),
            ProviderErrorKind::Transient
        );

        // Context length
        assert_eq!(
            classify_error(Some(1), "HTTP 413: Payload too large"),
            ProviderErrorKind::FatalTask
        );

        // Hard timeout
        assert_eq!(
            classify_error(Some(124), "Agent exceeded hard timeout"),
            ProviderErrorKind::FatalTask
        );

        // Unknown error defaults to transient
        assert_eq!(
            classify_error(Some(1), "Some random error"),
            ProviderErrorKind::Transient
        );
    }

    /// A `wg done` refusal because the parent is blocked is a TASK-LOGIC failure,
    /// not a provider failure — the agent ran fine, the graph declined it.
    #[test]
    fn test_wg_done_blocked_refusal_is_task_logic_not_provider() {
        let refusal =
            "Cannot mark 'satellite-x' as done: blocked by 1 unresolved task(s):\n  \
             parent-y (failed_pending_eval)";
        assert_eq!(
            classify_error(Some(1), refusal),
            ProviderErrorKind::FatalTask,
            "a blocked-parent done-refusal must never count against provider health"
        );
    }

    /// Every `wg done` workflow refusal classifies as FatalTask, including ones
    /// whose embedded text (blocker titles, git 401 from the merge step) would
    /// otherwise trip the auth/quota keyword matchers.
    #[test]
    fn test_all_wg_done_refusals_are_task_logic() {
        let refusals = [
            "Cannot mark 'a' as done: blocked by 3 unresolved task(s):\n  fix-authentication (open)",
            "Cannot mark 'b' as done: deliverable preflight refused — required deliverables were not produced.",
            "Cannot mark 'c' as done: this is a disposable and its completion contract is unmet.",
            "Cannot mark 'd' as done: integrated validation requires a validation log entry.",
            "Smoke gate refused 'wg done e': 2 scenario(s) broken",
            "The verify command must pass:\n  cargo test",
            "Agents cannot use --skip-smoke. The smoke gate is the regression contract;",
            "Agents cannot use --skip-verify. The verify command must pass:",
            "Worktree has uncommitted changes — refusing to mark 'f' as done.",
            "Merge conflict — cannot mark 'g' as done.",
        ];
        for r in refusals {
            assert_eq!(
                classify_error(Some(1), r),
                ProviderErrorKind::FatalTask,
                "wg-done refusal misclassified: {r:?}"
            );
        }
        // A real auth 401 (never wrapped in a done-refusal, because the agent
        // never reached `wg done`) still counts as a provider failure.
        assert_eq!(
            classify_error(Some(1), "authentication failed (HTTP 401)"),
            ProviderErrorKind::FatalProvider
        );
    }

    /// BUG-A6-1 (done-spoof, inverted): a provider echoes a `wg done`-looking
    /// sentence in its OWN error text while the real failure is a live outage.
    /// The string path must not let the done-sentence suppress the outage, yet
    /// must still keep genuine refusals (blocked-task lists, git merge-back 401s)
    /// as task logic — the embedded provider words there describe OTHER tasks or
    /// a git push credential, not this execution's model provider.
    #[test]
    fn done_spoof_does_not_suppress_a_real_provider_outage() {
        // Spoof: provider quotes a done-sentence AND reports a real 401 → outage.
        assert_eq!(
            classify_error(
                Some(1),
                "provider said: Cannot mark 'x' as done; Authentication failed HTTP 401"
            ),
            ProviderErrorKind::FatalProvider,
            "a provider outage wearing a done-sentence must not be suppressed"
        );
        assert_eq!(
            classify_error(
                Some(1),
                "Cannot mark 'x' as done — provider returned HTTP 402: balance exhausted"
            ),
            ProviderErrorKind::FatalProvider,
            "a quota/billing outage wearing a done-sentence must not be suppressed"
        );

        // Blocked-task list: the auth/quota words live in an UNRELATED blocker
        // title → still task logic, the provider counter must stay untouched.
        assert_eq!(
            classify_error(
                Some(1),
                "Cannot mark '.flip-X' as done: blocked by 1 unresolved task(s):\n  \
                 - X (Authentication failed HTTP 401 quota): FailedPendingEval"
            ),
            ProviderErrorKind::FatalTask,
            "provider words in a blocker title must not be read as an outage"
        );

        // Git merge-back 401: a push credential failure, not the model provider.
        assert_eq!(
            classify_error(
                Some(1),
                "Cannot mark 'x' as done: git push failed — fatal: Authentication failed for 'https://...'"
            ),
            ProviderErrorKind::FatalTask,
            "a git-push 401 during merge-back is task logic, not a provider outage"
        );
    }

    /// The exact 2026-07-14 poisoning scenario, end to end at the health level:
    /// a satellite completes, `wg done` is refused because its parent is blocked,
    /// this repeats well past the pause threshold — and the PROVIDER counter never
    /// moves and the service never pauses. This mirrors the classify → record
    /// flow in `commands::service::triage::track_provider_health`.
    #[test]
    fn test_done_refused_for_blocked_parent_never_pauses_provider() {
        let mut health = ProviderHealth::default();
        let provider_id = "claude";
        let refusal =
            "Cannot mark 'satellite-x' as done: blocked by 1 unresolved task(s):\n  \
             parent-y (failed_pending_eval)";

        // The satellite's agent ran fine but was done-refused, five times — well
        // past the threshold of 3 that would otherwise pause the service.
        for _ in 0..5 {
            let kind = classify_error(Some(1), refusal);
            // triage records the failure with the classified kind; a FatalTask is
            // a no-op for provider health (the agent ran, only `wg done` refused).
            health.record_failure(provider_id, kind, refusal.to_string());
        }

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(
            provider.consecutive_failures, 0,
            "wg-done refusals for a blocked parent must not touch the provider counter"
        );
        assert!(!provider.should_pause(3), "the provider must not be near pausing");

        let paused = health.check_and_apply_pauses(3, "pause");
        assert!(
            paused.is_empty(),
            "no provider should be paused by done-refusals"
        );
        assert!(
            !health.service_paused,
            "the service must never pause because the graph refused a completion"
        );
    }

    #[test]
    fn test_provider_health_tracking() {
        let mut health = ProviderHealth::default();
        let provider_id = "claude";

        // Record a fatal provider error
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Auth failed".to_string(),
        );

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 1);
        assert!(!provider.should_pause(3)); // Below threshold

        // Record more failures
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Auth failed again".to_string(),
        );
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Still failing".to_string(),
        );

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 3);
        assert!(provider.should_pause(3)); // At threshold

        // Success should reset count
        health.record_success(provider_id);
        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 0);
        assert!(!provider.should_pause(3));
    }

    #[test]
    fn test_pause_arms_one_shot_alert_on_edge_only() {
        let mut health = ProviderHealth::default();
        for _ in 0..3 {
            health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        }
        let paused = health.check_and_apply_pauses(3, "pause");
        assert_eq!(paused, vec!["claude".to_string()]);
        assert!(health.service_paused);
        assert_eq!(health.pause_generation, 1);

        // The alert is armed exactly once for this edge.
        assert_eq!(health.take_pause_alert(), Some(1));
        assert_eq!(health.take_pause_alert(), None);

        // A second triage pass while STILL paused must not re-arm the alert.
        health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        let paused2 = health.check_and_apply_pauses(3, "pause");
        assert!(paused2.is_empty(), "already-paused provider does not re-pause");
        assert_eq!(health.take_pause_alert(), None);
        assert_eq!(health.pause_generation, 1);
    }

    #[test]
    fn test_resume_resets_failures_and_bumps_generation_on_repause() {
        let mut health = ProviderHealth::default();
        for _ in 0..3 {
            health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        }
        health.check_and_apply_pauses(3, "pause");
        assert_eq!(health.pause_generation, 1);
        let _ = health.take_pause_alert();

        // Auto-resume: counter must be back to 0 so one bad window never
        // permanently lowers the effective trip threshold.
        health.resume_service();
        assert!(!health.service_paused);
        assert!(!health.pending_pause_alert);
        assert!(health.last_probe_at.is_none());
        assert_eq!(
            health.get_or_create_provider("claude").consecutive_failures,
            0
        );

        // A fresh bad window re-pauses and arms a NEW episode id.
        for _ in 0..3 {
            health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        }
        health.check_and_apply_pauses(3, "pause");
        assert_eq!(health.pause_generation, 2);
        assert_eq!(health.take_pause_alert(), Some(2));
    }

    #[test]
    fn test_should_probe_cadence() {
        let mut health = ProviderHealth::default();
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-07-14T15:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);

        // Not paused → never probe.
        assert!(!health.should_probe(t0, 300));

        for _ in 0..3 {
            health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        }
        health.check_and_apply_pauses(3, "pause");

        // Paused, never probed → probe now (unless disabled).
        assert!(health.should_probe(t0, 300));
        assert!(!health.should_probe(t0, 0), "interval 0 disables probing");

        // After probing, wait the full interval before the next probe.
        health.mark_probed(t0);
        let t_soon = t0 + chrono::Duration::seconds(299);
        let t_due = t0 + chrono::Duration::seconds(300);
        assert!(!health.should_probe(t_soon, 300));
        assert!(health.should_probe(t_due, 300));
    }

    #[test]
    fn test_pause_duration_and_paused_ids() {
        let mut health = ProviderHealth::default();
        for _ in 0..3 {
            health.record_failure("claude", ProviderErrorKind::FatalProvider, "auth".into());
        }
        health.check_and_apply_pauses(3, "pause");
        assert_eq!(health.paused_provider_ids(), vec!["claude".to_string()]);

        let paused_at = chrono::DateTime::parse_from_rfc3339(
            health.paused_at.as_deref().unwrap(),
        )
        .unwrap()
        .with_timezone(&Utc);
        let later = paused_at + chrono::Duration::seconds(90);
        assert_eq!(health.pause_duration_secs(later), Some(90));
    }

    #[test]
    fn outcome_sidecar_follows_registry_output_dir_on_retry_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let reused_dir = dir.join("agents").join("agent-from-prior-attempt");
        fs::create_dir_all(&reused_dir).unwrap();
        let output = reused_dir.join("output.log");

        let mut registry = crate::service::registry::AgentRegistry::new();
        let current_agent = registry.register_agent_with_model(
            12345,
            "retry-task",
            "codex",
            &output.to_string_lossy(),
            Some("gpt-5.5"),
        );
        registry.save(dir).unwrap();

        assert_eq!(outcome_agent_dir(dir, &current_agent), reused_dir);
        assert_ne!(
            outcome_agent_dir(dir, &current_agent),
            dir.join("agents").join(&current_agent),
            "the retry's new agent id must not redirect its outcome away from the reused run dir"
        );
    }

    #[test]
    fn typed_completion_refusals_never_poison_provider_health() {
        // Regression diagnosed in Luca 02204b19: graph-owned blocker prose may
        // legally contain provider-looking words. Provenance, not those words,
        // decides whether this is a provider failure.
        let refusal = "Cannot mark '.flip-X' as done: blocked by 1 unresolved task(s):\n  - X (Authentication failed HTTP 401 quota): FailedPendingEval";
        let outcome = ExecutionOutcome::CompletionRefused {
            code: completion_refusal_code(refusal),
        };
        assert_eq!(
            outcome,
            ExecutionOutcome::CompletionRefused {
                code: CompletionRefusalCode::Blocked
            }
        );
        assert_eq!(classify_execution_outcome(&outcome), None);

        let mut health = ProviderHealth::default();
        for _ in 0..3 {
            if let Some(kind) = classify_execution_outcome(&outcome) {
                health.record_failure("codex", kind, refusal.to_string());
            }
        }
        assert!(health.check_and_apply_pauses(3, "pause").is_empty());
        assert!(!health.service_paused);
        assert!(health.providers.is_empty());
    }

    #[test]
    fn refusal_codes_are_bounded_to_the_wg_done_boundary() {
        let fixtures = [
            (
                "Cannot mark 'x' as done: blocked by 1 unresolved task(s):",
                CompletionRefusalCode::Blocked,
            ),
            (
                "Cannot mark 'x' as done: deliverable preflight refused — missing",
                CompletionRefusalCode::Deliverable,
            ),
            ("Smoke gate failed for 'x'", CompletionRefusalCode::Smoke),
            (
                "Verify command failed (exit code 1): cargo test",
                CompletionRefusalCode::Verify,
            ),
            (
                "Cannot mark 'x' as done: integrated validation requires a validation log entry.",
                CompletionRefusalCode::Validation,
            ),
            (
                "Worktree has uncommitted changes — refusing to mark 'x' as done.",
                CompletionRefusalCode::Worktree,
            ),
            (
                "Cannot complete worktree merge conflict for 'x'",
                CompletionRefusalCode::Merge,
            ),
            (
                "Agents cannot use --skip-verify. The verify command must pass",
                CompletionRefusalCode::AgentBypass,
            ),
            (
                "Agents cannot use --skip-smoke. The smoke gate is required",
                CompletionRefusalCode::AgentBypass,
            ),
        ];
        for (message, expected) in fixtures {
            assert_eq!(completion_refusal_code(message), expected, "{message}");
        }
    }

    #[test]
    fn typed_provider_and_task_failure_matrix() {
        use ExecutorFailure::*;
        let fixtures = [
            (Authentication, ProviderErrorKind::FatalProvider),
            (Quota, ProviderErrorKind::FatalProvider),
            (HandlerUnavailable, ProviderErrorKind::FatalProvider),
            (Timeout, ProviderErrorKind::FatalTask),
            (Transport, ProviderErrorKind::Transient),
            (Http(400), ProviderErrorKind::FatalTask),
            (Http(401), ProviderErrorKind::FatalProvider),
            (Http(402), ProviderErrorKind::FatalProvider),
            (Http(403), ProviderErrorKind::FatalProvider),
            (Http(404), ProviderErrorKind::FatalTask),
            (Http(408), ProviderErrorKind::Transient),
            (Http(429), ProviderErrorKind::Transient),
            (Http(500), ProviderErrorKind::Transient),
            (Http(503), ProviderErrorKind::Transient),
        ];
        for (failure, expected) in fixtures {
            let outcome = ExecutionOutcome::ExecutorFailed { failure };
            assert_eq!(classify_execution_outcome(&outcome), Some(expected));
        }

        // A provider can quote a wg-done-looking sentence. Typed executor
        // provenance prevents that spoof from suppressing the real outage.
        let spoof = ExecutionOutcome::ExecutorFailed {
            failure: Authentication,
        };
        assert_eq!(
            classify_execution_outcome(&spoof),
            Some(ProviderErrorKind::FatalProvider)
        );
        assert_eq!(
            classify_error(
                Some(1),
                "provider said: Cannot mark 'x' as done; Authentication failed HTTP 401"
            ),
            ProviderErrorKind::FatalProvider
        );
    }

    #[test]
    fn legacy_http_fixtures_remain_narrow() {
        assert_eq!(
            classify_error(Some(1), "HTTP 400 invalid request"),
            ProviderErrorKind::FatalTask
        );
        assert_eq!(
            classify_error(Some(1), "HTTP 402 payment required"),
            ProviderErrorKind::FatalProvider
        );
        assert_eq!(
            classify_error(Some(1), "HTTP 404 model not found"),
            ProviderErrorKind::FatalTask
        );
        assert_eq!(
            classify_error(Some(1), "HTTP 500 upstream unavailable"),
            ProviderErrorKind::Transient
        );
        assert_eq!(
            classify_error(Some(1), "connection refused by transport"),
            ProviderErrorKind::Transient
        );
    }

    #[test]
    fn health_route_key_distinguishes_handler_wire_and_endpoint() {
        use crate::dispatch::{Placement, ResolvedModelSpec, SpawnProvenance};
        use std::collections::HashMap;

        fn plan(model: &str, endpoint: Option<crate::config::EndpointConfig>) -> SpawnPlan {
            SpawnPlan {
                executor: crate::dispatch::handler_for_model(model),
                model: ResolvedModelSpec::from_raw(model),
                reasoning: None,
                endpoint,
                env: HashMap::new(),
                argv: vec![],
                placement: Placement::Local,
                provenance: SpawnProvenance::default(),
            }
        }

        let endpoint_a = crate::config::EndpointConfig {
            name: "a".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://a.invalid/v1".to_string()),
            api_key_ref: Some("keyring:openrouter-a".to_string()),
            ..Default::default()
        };
        let endpoint_b = crate::config::EndpointConfig {
            name: "b".to_string(),
            url: Some("https://b.invalid/v1".to_string()),
            api_key_ref: Some("keyring:openrouter-b".to_string()),
            ..endpoint_a.clone()
        };
        let nex_a = HealthRouteKey::from_spawn_plan(&plan(
            "nex:openrouter:anthropic/claude",
            Some(endpoint_a),
        ));
        let nex_b = HealthRouteKey::from_spawn_plan(&plan(
            "nex:openrouter:anthropic/claude",
            Some(endpoint_b),
        ));
        let pi = HealthRouteKey::from_spawn_plan(&plan("pi:openrouter:anthropic/claude", None));
        assert_ne!(nex_a, nex_b, "Nex endpoints must have separate breakers");
        assert_ne!(nex_a.system.handler, pi.system.handler);
        assert_eq!(nex_a.system.provider, "openrouter");
        assert_eq!(pi.system.provider, "openrouter");
        let nex_default =
            HealthRouteKey::from_spawn_plan(&plan("nex:openrouter:anthropic/claude", None));
        assert_eq!(nex_default.system.provider, "openrouter");
        assert!(!nex_a.id().contains("keyring:openrouter-a"));

        let secret_endpoint = crate::config::EndpointConfig {
            name: "secret-url".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://user:password@example.invalid/v1?token=secret#fragment".to_string()),
            api_key: Some("inline-secret".to_string()),
            api_key_ref: Some("literal:another-secret".to_string()),
            ..Default::default()
        };
        let secret_key = HealthRouteKey::from_spawn_plan(&plan(
            "nex:openrouter:anthropic/claude",
            Some(secret_endpoint),
        ));
        for secret in [
            "user",
            "password",
            "token",
            "inline-secret",
            "another-secret",
        ] {
            assert!(!secret_key.id().contains(secret));
        }
    }

    #[test]
    fn test_provider_id_extraction() {
        assert_eq!(extract_provider_id("claude", None), "claude");
        assert_eq!(
            extract_provider_id("native", Some("gpt-4")),
            "native:openai"
        );
        assert_eq!(
            extract_provider_id("native", Some("claude-3-sonnet")),
            "native:anthropic"
        );
        assert_eq!(
            extract_provider_id("native", Some("custom:model")),
            "native:custom"
        );
    }
}
