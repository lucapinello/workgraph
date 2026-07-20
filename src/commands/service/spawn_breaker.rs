//! Dispatcher-level, self-healing spawn circuit breaker.
//!
//! The per-task circuit breaker (`check_spawn_circuit_breaker` in
//! [`super::coordinator`]) is a *final give-up*: after `max_spawn_failures`
//! consecutive failures on ONE task, that task is marked incomplete for the
//! evaluator. It does nothing about a *systemic* spawn outage — a crash window,
//! a downed provider, a bad binary — where EVERY spawn fails. In that situation
//! the old dispatcher just kept thrashing (spawn, fail, spawn, fail…) and, once
//! enough tasks tripped their per-task breakers, went quiet with no signal to
//! the operator. That silent freeze is what this module fixes.
//!
//! This breaker tracks *consecutive dispatcher-wide* spawn failures. When they
//! reach [`SpawnBreakerConfig::threshold`] it **opens**: all spawns pause for a
//! cooldown and a loud, plain-language operator alert is armed. After the
//! cooldown it **half-opens** and allows exactly ONE probe spawn:
//!
//! * probe succeeds  → breaker **closes**, normal dispatch resumes;
//! * probe fails     → breaker **re-opens** with an exponentially longer
//!                     cooldown (doubling, capped at
//!                     [`SpawnBreakerConfig::max_cooldown_secs`]).
//!
//! The state is a tiny JSON file under the service dir so it survives daemon
//! restarts (a restart mid-outage must not reset the backoff and start
//! thrashing again). The state machine itself ([`SpawnBreakerState`]) is pure
//! and fully unit-tested; all I/O lives at the edges.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
// `NaiveDateTime` is used only by the casa operator-alert nudge builder below.
#[cfg(feature = "casa")]
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use worksgood::atomic_file::write_atomic;
use worksgood::config::Config;
// The digest `Nudge` type is casa-only (the operator-alert DM rides the casa
// digest pacing layer). The breaker itself is general; only the nudge builder
// below is gated. See docs/38 §4.
#[cfg(feature = "casa")]
use worksgood::notify::daily_digest::{Nudge, NudgeKind};

/// Resolved knobs for the breaker (read from [`Config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnBreakerConfig {
    /// Consecutive dispatcher-wide spawn failures that trip the breaker open.
    /// `0` disables the breaker entirely.
    pub threshold: u32,
    /// Base cooldown, in seconds, before the breaker half-opens.
    pub base_cooldown_secs: i64,
    /// Cap on the exponentially-backed-off cooldown, in seconds.
    pub max_cooldown_secs: i64,
}

impl SpawnBreakerConfig {
    /// Build from the coordinator config, clamping nonsensical values so the
    /// state machine can never divide/shift into a panic.
    pub fn from_config(config: &Config) -> Self {
        let base = config.coordinator.spawn_breaker_cooldown_secs.max(1) as i64;
        let cap = config
            .coordinator
            .spawn_breaker_max_cooldown_secs
            .max(config.coordinator.spawn_breaker_cooldown_secs.max(1))
            as i64;
        Self {
            threshold: config.coordinator.spawn_breaker_threshold,
            base_cooldown_secs: base,
            max_cooldown_secs: cap,
        }
    }

    /// The breaker is only active with a non-zero threshold.
    pub fn enabled(&self) -> bool {
        self.threshold > 0
    }
}

/// Which phase the breaker is in, given "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerPhase {
    /// Normal operation — spawns flow freely.
    Closed,
    /// Tripped and cooling down — no spawns allowed yet.
    Open,
    /// Cooldown elapsed — allow exactly ONE probe spawn.
    HalfOpen,
}

impl BreakerPhase {
    pub fn label(self) -> &'static str {
        match self {
            BreakerPhase::Closed => "closed",
            BreakerPhase::Open => "OPEN",
            BreakerPhase::HalfOpen => "HALF-OPEN",
        }
    }
}

/// What the dispatcher should do with the next spawn, per the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnGate {
    /// Spawn normally (up to the usual slot budget).
    Allow,
    /// Allow exactly ONE probe spawn this tick.
    Probe,
    /// Do not spawn; the breaker is open. Carries seconds left on the cooldown.
    Blocked { cooldown_remaining_secs: i64 },
}

/// The outcome of feeding a spawn result to the breaker — lets the caller emit
/// the right log/alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerEvent {
    /// Nothing changed (still counting, or breaker disabled).
    None,
    /// The breaker just tripped closed → open.
    Opened,
    /// A half-open probe failed; the breaker re-opened with a longer cooldown.
    Reopened,
    /// A half-open probe succeeded; the breaker closed and dispatch is healthy.
    Recovered,
}

/// Durable breaker state. Serialised to `<service>/spawn-breaker.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnBreakerState {
    /// Consecutive dispatcher-wide spawn failures with no success in between.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// RFC3339 timestamp the breaker last opened. `None` ⇒ closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<String>,
    /// How many times the breaker has re-opened without a clean close. Drives the
    /// exponential cooldown backoff (`base * 2^generation`, capped).
    #[serde(default)]
    pub open_generation: u32,
    /// Set when the breaker opens/re-opens; consumed by the daemon to emit the
    /// loud operator alert exactly once per open episode.
    #[serde(default)]
    pub alert_pending: bool,
    /// Total number of times the breaker has opened (telemetry / status).
    #[serde(default)]
    pub total_opens: u32,
    /// RFC3339 timestamp of the most recent recovery (status/telemetry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovered_at: Option<String>,
}

impl SpawnBreakerState {
    /// Standard on-disk path: `<service_dir>/spawn-breaker.json`.
    pub fn path(service_dir: &Path) -> PathBuf {
        service_dir.join("service").join("spawn-breaker.json")
    }

    /// Load from `path`, or a fresh (closed) breaker when missing/corrupt. A lost
    /// breaker file at worst re-learns the outage within one threshold window; it
    /// must never crash the daemon.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    /// Persist atomically (creating the parent dir as needed). Best-effort: an
    /// I/O error is returned but the daemon treats persistence as advisory.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, json.as_bytes())
    }

    fn opened_time(&self) -> Option<DateTime<Utc>> {
        self.opened_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Cooldown length for the current backoff generation, in seconds.
    /// `base * 2^generation`, saturating and capped at `max_cooldown_secs`.
    pub fn cooldown_secs(&self, cfg: &SpawnBreakerConfig) -> i64 {
        let mut secs = cfg.base_cooldown_secs.max(1);
        for _ in 0..self.open_generation {
            secs = secs.saturating_mul(2);
            if secs >= cfg.max_cooldown_secs {
                return cfg.max_cooldown_secs;
            }
        }
        secs.min(cfg.max_cooldown_secs)
    }

    /// Seconds remaining before the breaker half-opens (0 once elapsed).
    pub fn cooldown_remaining_secs(&self, now: DateTime<Utc>, cfg: &SpawnBreakerConfig) -> i64 {
        match self.opened_time() {
            Some(opened) => {
                let elapsed = now.signed_duration_since(opened).num_seconds();
                (self.cooldown_secs(cfg) - elapsed).max(0)
            }
            None => 0,
        }
    }

    /// Current phase given "now".
    pub fn phase(&self, now: DateTime<Utc>, cfg: &SpawnBreakerConfig) -> BreakerPhase {
        if !cfg.enabled() || self.opened_at.is_none() {
            return BreakerPhase::Closed;
        }
        if self.cooldown_remaining_secs(now, cfg) <= 0 {
            BreakerPhase::HalfOpen
        } else {
            BreakerPhase::Open
        }
    }

    /// What the dispatcher should do with the next spawn.
    pub fn gate(&self, now: DateTime<Utc>, cfg: &SpawnBreakerConfig) -> SpawnGate {
        match self.phase(now, cfg) {
            BreakerPhase::Closed => SpawnGate::Allow,
            BreakerPhase::HalfOpen => SpawnGate::Probe,
            BreakerPhase::Open => SpawnGate::Blocked {
                cooldown_remaining_secs: self.cooldown_remaining_secs(now, cfg),
            },
        }
    }

    /// Record a successful spawn. Resets the failure counter and, if the breaker
    /// was open/half-open, closes it (recovery).
    pub fn record_success(&mut self, now: DateTime<Utc>) -> BreakerEvent {
        let was_open = self.opened_at.is_some();
        self.consecutive_failures = 0;
        self.opened_at = None;
        self.open_generation = 0;
        self.alert_pending = false;
        if was_open {
            self.last_recovered_at = Some(now.to_rfc3339());
            BreakerEvent::Recovered
        } else {
            BreakerEvent::None
        }
    }

    /// Record a failed spawn attempt. Opens the breaker on reaching the
    /// threshold; a failure while half-open re-opens it with a longer cooldown.
    pub fn record_failure(&mut self, now: DateTime<Utc>, cfg: &SpawnBreakerConfig) -> BreakerEvent {
        if !cfg.enabled() {
            return BreakerEvent::None;
        }
        let phase = self.phase(now, cfg);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        match phase {
            BreakerPhase::HalfOpen => {
                // The single probe failed — re-open with a longer cooldown.
                self.open_generation = self.open_generation.saturating_add(1);
                self.opened_at = Some(now.to_rfc3339());
                self.alert_pending = true;
                self.total_opens = self.total_opens.saturating_add(1);
                BreakerEvent::Reopened
            }
            BreakerPhase::Open => {
                // Spawns shouldn't happen while open (the gate blocks them), but
                // if one slips through, keep counting without re-arming the alert.
                BreakerEvent::None
            }
            BreakerPhase::Closed => {
                if self.consecutive_failures >= cfg.threshold {
                    self.opened_at = Some(now.to_rfc3339());
                    self.open_generation = 0;
                    self.alert_pending = true;
                    self.total_opens = self.total_opens.saturating_add(1);
                    BreakerEvent::Opened
                } else {
                    BreakerEvent::None
                }
            }
        }
    }

    /// Consume the "alert armed" flag (returns true at most once per open episode).
    pub fn take_alert(&mut self) -> bool {
        let armed = self.alert_pending;
        self.alert_pending = false;
        armed
    }

    /// Human-readable one-line summary for `wg service status` (text mode).
    pub fn status_line(&self, now: DateTime<Utc>, cfg: &SpawnBreakerConfig) -> String {
        if !cfg.enabled() {
            return "disabled".to_string();
        }
        match self.phase(now, cfg) {
            BreakerPhase::Closed => {
                if self.consecutive_failures > 0 {
                    format!(
                        "closed ({}/{} consecutive spawn failures)",
                        self.consecutive_failures, cfg.threshold
                    )
                } else {
                    "closed (healthy)".to_string()
                }
            }
            BreakerPhase::Open => format!(
                "OPEN — spawns paused, {} left before a retry ({} consecutive failures, backoff x{})",
                worksgood::format_duration(self.cooldown_remaining_secs(now, cfg), false),
                self.consecutive_failures,
                self.open_generation + 1,
            ),
            BreakerPhase::HalfOpen => format!(
                "HALF-OPEN — probing the next spawn ({} consecutive failures)",
                self.consecutive_failures
            ),
        }
    }
}

/// The plain-language operator alert body for a tripped/re-tripped breaker.
pub const OPERATOR_ALERT_TEXT: &str =
    "⚠️ The family team's task runner is stuck — it will retry itself shortly. \
If this keeps happening, check the server.";

/// The plain-language operator alert body for a wedged dispatcher (watchdog).
pub const WATCHDOG_ALERT_TEXT: &str =
    "⚠️ The family team's task runner looks stuck — there is work waiting but nothing is \
running. It will keep trying; if this doesn't clear on its own, check the server.";

/// Build a time-critical operator nudge so the alert routes through the digest
/// pacing layer (standalone-capped, honest overflow) rather than DMing raw.
///
/// `episode` should distinguish separate open episodes (e.g. the backoff
/// generation) so a re-open is not deduped against the first open by the digest
/// store's exactly-once `seen` set.
#[cfg(feature = "casa")]
pub fn operator_alert_nudge(
    recipient: impl Into<String>,
    episode: &str,
    due: NaiveDateTime,
    text: impl Into<String>,
) -> Nudge {
    Nudge::time_critical(
        format!("spawn-breaker:{episode}"),
        recipient,
        NudgeKind::Proactive,
        due,
        text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "casa")]
    use worksgood::notify::daily_digest::{DigestPolicy, DigestStore, Offer, Urgency};

    fn cfg() -> SpawnBreakerConfig {
        SpawnBreakerConfig {
            threshold: 3,
            base_cooldown_secs: 600,
            max_cooldown_secs: 3600,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn breaker_starts_closed_and_allows() {
        let s = SpawnBreakerState::default();
        assert_eq!(s.phase(at(0), &cfg()), BreakerPhase::Closed);
        assert_eq!(s.gate(at(0), &cfg()), SpawnGate::Allow);
    }

    #[test]
    fn breaker_trips_open_at_threshold() {
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        assert_eq!(s.record_failure(at(0), &c), BreakerEvent::None);
        assert_eq!(s.record_failure(at(1), &c), BreakerEvent::None);
        // Third failure hits threshold=3 → opens.
        assert_eq!(s.record_failure(at(2), &c), BreakerEvent::Opened);
        assert_eq!(s.phase(at(2), &c), BreakerPhase::Open);
        assert!(matches!(s.gate(at(2), &c), SpawnGate::Blocked { .. }));
        assert!(s.alert_pending, "opening must arm the operator alert");
        assert_eq!(s.total_opens, 1);
    }

    #[test]
    fn breaker_half_opens_after_cooldown_then_recovers() {
        // trip → half-open → recovery (the headline path).
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        for i in 0..3 {
            s.record_failure(at(i), &c);
        }
        assert_eq!(s.phase(at(2), &c), BreakerPhase::Open);
        // Still cooling down 9 minutes later.
        assert_eq!(s.phase(at(2 + 540), &c), BreakerPhase::Open);
        // 10 minutes later → half-open, dispatcher may try ONE probe.
        assert_eq!(s.phase(at(2 + 600), &c), BreakerPhase::HalfOpen);
        assert_eq!(s.gate(at(2 + 600), &c), SpawnGate::Probe);
        // Probe succeeds → breaker closes.
        assert_eq!(s.record_success(at(2 + 601)), BreakerEvent::Recovered);
        assert_eq!(s.phase(at(2 + 601), &c), BreakerPhase::Closed);
        assert_eq!(s.consecutive_failures, 0);
        assert!(s.last_recovered_at.is_some());
        assert!(!s.alert_pending);
    }

    #[test]
    fn breaker_reopens_with_exponential_backoff_on_failed_probe() {
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        // Trip open at t=0 (opened_at = at(0)).
        for _ in 0..3 {
            s.record_failure(at(0), &c);
        }
        // gen 0 cooldown = 600
        assert_eq!(s.cooldown_secs(&c), 600);
        // half-open at +600, probe fails → reopen, gen 1 (opened_at = at(600))
        assert_eq!(s.phase(at(600), &c), BreakerPhase::HalfOpen);
        assert_eq!(s.record_failure(at(600), &c), BreakerEvent::Reopened);
        assert_eq!(s.open_generation, 1);
        assert_eq!(s.cooldown_secs(&c), 1200);
        assert!(s.alert_pending, "re-open must re-arm the alert");
        // still open until +1200 from the reopen (at(600) + 1200 = at(1800))
        assert_eq!(s.phase(at(600 + 1199), &c), BreakerPhase::Open);
        assert_eq!(s.phase(at(1800), &c), BreakerPhase::HalfOpen);
        // probe fails again → gen 2, cooldown 2400 (opened_at = at(1800))
        s.record_failure(at(1800), &c);
        assert_eq!(s.open_generation, 2);
        assert_eq!(s.cooldown_secs(&c), 2400);
        // probe fails again → gen 3 would be 4800, capped at 3600
        s.record_failure(at(1800 + 2400), &c);
        assert_eq!(s.cooldown_secs(&c), 3600, "cooldown caps at max (1h)");
    }

    #[test]
    fn cooldown_never_exceeds_cap_even_at_huge_generation() {
        let c = cfg();
        let mut s = SpawnBreakerState {
            open_generation: 1000,
            ..Default::default()
        };
        assert_eq!(s.cooldown_secs(&c), 3600);
        s.opened_at = Some(at(0).to_rfc3339());
        // no panic / overflow computing remaining
        let _ = s.cooldown_remaining_secs(at(10), &c);
    }

    #[test]
    fn disabled_breaker_never_trips() {
        let c = SpawnBreakerConfig {
            threshold: 0,
            ..cfg()
        };
        let mut s = SpawnBreakerState::default();
        for i in 0..100 {
            assert_eq!(s.record_failure(at(i), &c), BreakerEvent::None);
        }
        assert_eq!(s.phase(at(100), &c), BreakerPhase::Closed);
        assert_eq!(s.gate(at(100), &c), SpawnGate::Allow);
    }

    #[test]
    fn success_before_threshold_resets_counter() {
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        s.record_failure(at(0), &c);
        s.record_failure(at(1), &c);
        assert_eq!(s.consecutive_failures, 2);
        s.record_success(at(2));
        assert_eq!(s.consecutive_failures, 0);
        // Now needs a full fresh run of 3 to trip.
        s.record_failure(at(3), &c);
        s.record_failure(at(4), &c);
        assert_eq!(s.phase(at(4), &c), BreakerPhase::Closed);
    }

    #[test]
    fn take_alert_is_one_shot() {
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        for i in 0..3 {
            s.record_failure(at(i), &c);
        }
        assert!(s.take_alert());
        assert!(!s.take_alert(), "alert consumed exactly once per open episode");
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = SpawnBreakerState::path(dir.path());
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        for i in 0..3 {
            s.record_failure(at(i), &c);
        }
        s.save(&path).unwrap();
        let loaded = SpawnBreakerState::load(&path);
        assert_eq!(loaded, s);
        assert_eq!(loaded.phase(at(2), &c), BreakerPhase::Open);
    }

    #[test]
    fn missing_state_file_loads_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = SpawnBreakerState::path(dir.path());
        let loaded = SpawnBreakerState::load(&path);
        assert_eq!(loaded, SpawnBreakerState::default());
    }

    #[test]
    fn status_line_reflects_phase() {
        let c = cfg();
        let mut s = SpawnBreakerState::default();
        assert!(s.status_line(at(0), &c).contains("healthy"));
        for _ in 0..3 {
            s.record_failure(at(0), &c);
        }
        assert!(s.status_line(at(0), &c).contains("OPEN"));
        assert!(s.status_line(at(600), &c).contains("HALF-OPEN"));
    }

    // -- Alert emission: routes through digest pacing as time-critical --------

    // A fixed non-quiet-hours wall clock (10:00) for the alert tests.
    #[cfg(feature = "casa")]
    fn alert_now() -> NaiveDateTime {
        at(0).naive_utc().date().and_hms_opt(10, 0, 0).unwrap()
    }

    #[cfg(feature = "casa")]
    #[test]
    fn breaker_alert_emits_as_time_critical_standalone() {
        // Due now (10:00), outside quiet hours, under the standalone cap → a
        // time-critical alert DMs immediately.
        let now = alert_now();
        let nudge = operator_alert_nudge("operator", "gen0", now, OPERATOR_ALERT_TEXT);
        assert_eq!(nudge.urgency, Urgency::TimeCritical);
        assert!(nudge.text.contains("task runner is stuck"));

        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        match store.offer(&nudge, now, &policy) {
            Offer::SendNow(text) => assert!(text.contains("task runner is stuck")),
            other => panic!("expected time-critical SendNow, got {other:?}"),
        }
    }

    #[cfg(feature = "casa")]
    #[test]
    fn breaker_alert_reopen_has_distinct_id_so_it_is_not_deduped() {
        let now = alert_now();
        let n0 = operator_alert_nudge("operator", "gen0", now, OPERATOR_ALERT_TEXT);
        let n1 = operator_alert_nudge("operator", "gen1", now, OPERATOR_ALERT_TEXT);
        assert_ne!(n0.id, n1.id, "re-open must not be deduped against first open");

        let mut store = DigestStore::default();
        let policy = DigestPolicy::new();
        assert!(matches!(store.offer(&n0, now, &policy), Offer::SendNow(_)));
        // Same id would be Duplicate; distinct id fires again (subject to cap).
        assert!(matches!(store.offer(&n1, now, &policy), Offer::SendNow(_)));
    }
}
