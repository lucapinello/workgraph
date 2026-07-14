//! Provider health detection and auto-pause system
//!
//! Tracks provider failure patterns and implements circuit-breaker logic
//! to pause the service when providers repeatedly fail with fatal errors.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
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
    if is_wg_done_refusal(&stderr_lower) {
        return ProviderErrorKind::FatalTask;
    }

    // Auth/Authorization failures (Fatal-Provider)
    if stderr_lower.contains("authentication failed")
        || stderr_lower.contains("http 401")
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

    // Context length issues (Fatal-Task)
    if stderr_lower.contains("http 413")
        || stderr_lower.contains("payload too large")
        || (stderr_lower.contains("http 400")
            && (stderr_lower.contains("context")
                || stderr_lower.contains("too long")
                || stderr_lower.contains("too large")
                || stderr_lower.contains("token")
                || stderr_lower.contains("maximum")
                || stderr_lower.contains("prompt")))
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
