//! Inbound-listener health: make a DEAF listener loud instead of log-only.
//!
//! # The gap this closes (task `investigate-telegram-getupdates`)
//!
//! The Telegram listener supervisor (`.casa/listener-supervisor.sh`) restarts a
//! listener that *dies*. It has no opinion about a listener that is **alive and
//! healthy-looking while every single poll fails** — which is exactly what
//! happened on 2026-07-24: the corporate Cisco WSA proxy on the host's network
//! blocks `api.telegram.org` by policy, so all five bots' `getUpdates` calls
//! died with `client error (Connect): tls handshake eof` for over two hours.
//! The process was up, the supervisor was content, `wg service status` said
//! everything was fine, and the family chat was silently deaf and mute. The
//! only evidence was 889 identical lines buried in `.casa/telegram.log`.
//!
//! A failure that only exists in a log file is a failure nobody sees. This
//! module gives the poll loop a place to publish per-bot poll health that a
//! *different process* (`wg service status`, the casa supervisor, a smoke
//! scenario) can read and shout about.
//!
//! # Shape
//!
//! One JSON file **per bot** under `<wg-dir>/service/listener_health/`:
//!
//! ```text
//! .wg/service/listener_health/nora.json
//! .wg/service/listener_health/bruno.json
//! ```
//!
//! Per-bot files (rather than one shared document) are deliberate: five bots
//! poll concurrently from five independent tasks, and a single shared file
//! would need a lock to avoid lost updates. Each bot owning its own file makes
//! every write a whole-file replace with no cross-bot race at all, and the
//! aggregate view is a directory read.
//!
//! # Classification
//!
//! [`PollFailureKind::classify`] turns an opaque transport error into an
//! operator-actionable diagnosis. "tls handshake eof" is meaningless to the
//! person who has to fix it; "egress to api.telegram.org is being blocked
//! (network policy / proxy / firewall)" tells them where to look. The
//! classifier is the reason the status line can say something useful instead of
//! echoing a reqwest Display impl.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Consecutive failed polls after which a bot counts as DEAF.
///
/// Three is past "one flaky poll" (Telegram long-polls for 30s and the odd
/// timeout is normal) but well inside the window where a family would still be
/// waiting for an answer. At the configured backoff ladder three failures is
/// reached within roughly a minute of a genuine outage.
pub const DEAF_AFTER_FAILURES: u32 = 3;

/// How long a bot may go without a *successful* poll before it counts as deaf
/// even if the failure counter was reset by a partial recovery flap.
///
/// Deliberately longer than the maximum backoff (60s) plus one long-poll window
/// (30s) so a healthy-but-idle listener is never reported deaf.
pub const DEAF_AFTER_SILENCE_SECS: i64 = 300;

/// A health record untouched for this long means no listener is publishing it.
///
/// A live poll task writes on EVERY iteration, and its slowest iteration is one
/// 30s long-poll plus the 60s maximum backoff — so four minutes of silence
/// cannot happen while a listener is running.
pub const STALE_AFTER_SECS: i64 = 240;

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// What kind of failure a poll error represents — the operator-facing
/// interpretation, not the wire error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollFailureKind {
    /// The TCP connection is accepted but the TLS handshake is cut off, or the
    /// connection is reset/refused outright. A middlebox (corporate proxy,
    /// firewall, ISP filter) is terminating the connection: it cannot serve an
    /// HTTPS block page without MITM, so it drops the socket after our
    /// ClientHello. This is the 2026-07-24 Cisco-WSA shape.
    EgressBlocked,
    /// Name resolution failed — no DNS, or the resolver is filtering the name.
    DnsFailure,
    /// The request was made but no response arrived in time. Normal in small
    /// doses on a long-poll; sustained means a degraded path.
    Timeout,
    /// Telegram answered and rejected us: a revoked/wrong bot token, or the bot
    /// was removed from the chat. Not a network problem.
    Unauthorized,
    /// Telegram answered 429 — we are polling too hard, or another listener is
    /// polling the same bot (a duplicate-listener smell).
    RateLimited,
    /// Anything unrecognised. Kept distinct so a new failure mode is visible as
    /// "unclassified" rather than silently mislabelled as a network block.
    Other,
}

impl PollFailureKind {
    /// Classify a poll error from its rendered message chain.
    ///
    /// Matching is on the *lowercased* text and ordered most-specific-first:
    /// an "unauthorized" or "429" answer means Telegram was reached, so it must
    /// win over any connect-level pattern that happens to co-occur.
    pub fn classify(err: &str) -> Self {
        let e = err.to_lowercase();

        // Telegram answered — an application-level rejection, not a network fault.
        if e.contains("401") || e.contains("unauthorized") || e.contains("bot token") {
            return Self::Unauthorized;
        }
        if e.contains("429") || e.contains("too many requests") {
            return Self::RateLimited;
        }
        // A block page (Cisco WSA and friends answer plain HTTP with 403 while
        // silently dropping HTTPS) is a policy block, not an auth failure.
        if e.contains("403") || e.contains("forbidden") || e.contains("blocked by") {
            return Self::EgressBlocked;
        }
        if e.contains("dns error")
            || e.contains("failed to lookup address")
            || e.contains("nodename nor servname")
            || e.contains("name or service not known")
        {
            return Self::DnsFailure;
        }
        // The connect-phase kill signatures. `tls handshake eof` is the one the
        // live incident produced; the others are the same middlebox behaviour
        // seen through different TLS stacks / kill points.
        if e.contains("tls handshake eof")
            || e.contains("unexpected eof")
            || e.contains("handshake failed")
            || e.contains("connection reset by peer")
            || e.contains("connection refused")
            || e.contains("connection closed before message completed")
        {
            return Self::EgressBlocked;
        }
        if e.contains("timed out") || e.contains("timeout") || e.contains("operation timed out") {
            return Self::Timeout;
        }
        Self::Other
    }

    /// Stable machine label (also what lands in JSON status output).
    pub fn label(&self) -> &'static str {
        match self {
            Self::EgressBlocked => "egress-blocked",
            Self::DnsFailure => "dns-failure",
            Self::Timeout => "timeout",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate-limited",
            Self::Other => "unclassified",
        }
    }

    /// One-line operator diagnosis: what this failure means and where to look.
    ///
    /// This is the whole point of the classifier — the raw error told the
    /// operator nothing they could act on.
    pub fn diagnosis(&self) -> &'static str {
        match self {
            Self::EgressBlocked => {
                "network egress to api.telegram.org is being blocked (corporate proxy, firewall, or ISP filter cutting the TLS handshake) — verify with: curl -i http://api.telegram.org/ (a block page names the proxy in its Via: header)"
            }
            Self::DnsFailure => {
                "api.telegram.org does not resolve — check DNS/resolver config on this host"
            }
            Self::Timeout => {
                "polls are timing out — the path to api.telegram.org is degraded or saturated"
            }
            Self::Unauthorized => {
                "Telegram rejected the bot token — it was revoked/rotated, or the bot was removed from the chat; re-check notify.toml"
            }
            Self::RateLimited => {
                "Telegram is rate-limiting these polls — most often a SECOND listener polling the same bot (check for duplicate `wg telegram listen` processes)"
            }
            Self::Other => {
                "unrecognised poll failure — read the raw error in the listener log"
            }
        }
    }

    /// Whether this kind is worth waking an operator for. Every kind here stops
    /// inbound family messages, so all of them qualify; the method exists so a
    /// future "transient, self-healing" kind can opt out without touching
    /// callers.
    pub fn is_operator_actionable(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Per-bot record
// ---------------------------------------------------------------------------

/// Poll health for ONE bot, as published by that bot's poll task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotPollHealth {
    /// The bot id from `[telegram.bots.<id>]` (or `"default"`).
    pub bot_id: String,
    /// Consecutive failed polls; reset to 0 by any success.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Redacted text of the most recent failure.
    #[serde(default)]
    pub last_error: Option<String>,
    /// [`PollFailureKind::label`] of the most recent failure.
    #[serde(default)]
    pub last_failure_kind: Option<String>,
    /// RFC3339 timestamp of the most recent failure.
    #[serde(default)]
    pub last_failure_at: Option<String>,
    /// RFC3339 timestamp of the most recent SUCCESSFUL poll. `None` means this
    /// bot has never completed a poll since the listener started — the worst
    /// case, and the one the 2026-07-24 incident sat in for two hours.
    #[serde(default)]
    pub last_success_at: Option<String>,
    /// Lifetime failure count for this listener run — the number that makes an
    /// 889-line log storm legible as a single figure.
    #[serde(default)]
    pub total_failures: u64,
    /// When this record was last written.
    pub updated_at: String,
}

impl BotPollHealth {
    /// A fresh record for a bot with no observations yet.
    pub fn new(bot_id: &str, now: DateTime<Utc>) -> Self {
        Self {
            bot_id: bot_id.to_string(),
            consecutive_failures: 0,
            last_error: None,
            last_failure_kind: None,
            last_failure_at: None,
            last_success_at: None,
            total_failures: 0,
            updated_at: now.to_rfc3339(),
        }
    }

    /// The classified kind of the last failure, if any.
    pub fn kind(&self) -> Option<PollFailureKind> {
        match self.last_failure_kind.as_deref() {
            Some("egress-blocked") => Some(PollFailureKind::EgressBlocked),
            Some("dns-failure") => Some(PollFailureKind::DnsFailure),
            Some("timeout") => Some(PollFailureKind::Timeout),
            Some("unauthorized") => Some(PollFailureKind::Unauthorized),
            Some("rate-limited") => Some(PollFailureKind::RateLimited),
            Some("unclassified") => Some(PollFailureKind::Other),
            _ => None,
        }
    }

    /// Is this bot deaf as of `now`?
    ///
    /// Two independent triggers, because either alone can be fooled:
    /// - a failure streak at or past [`DEAF_AFTER_FAILURES`] (the common case), or
    /// - no successful poll for [`DEAF_AFTER_SILENCE_SECS`] while failures are
    ///   being recorded (catches a flapping bot whose streak keeps resetting).
    ///
    /// A bot that has never failed and never succeeded (record just created) is
    /// NOT deaf — it is starting up.
    pub fn is_deaf(&self, now: DateTime<Utc>) -> bool {
        if self.consecutive_failures >= DEAF_AFTER_FAILURES {
            return true;
        }
        if self.total_failures == 0 {
            return false;
        }
        match self.last_success_at.as_deref().and_then(parse_ts) {
            Some(last_ok) => (now - last_ok).num_seconds() >= DEAF_AFTER_SILENCE_SECS,
            None => match self.last_failure_at.as_deref().and_then(parse_ts) {
                // Never succeeded: measure the silence from the FIRST evidence
                // we have that this bot was trying at all.
                Some(last_fail) => (now - last_fail).num_seconds() >= 0,
                None => false,
            },
        }
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Directory holding one health file per bot.
pub fn health_dir(dir: &Path) -> PathBuf {
    dir.join("service").join("listener_health")
}

/// Path of one bot's health file. The bot id is sanitised the same way the
/// offset files sanitise it, so a bot id with a slash can never escape the
/// directory.
pub fn bot_health_path(dir: &Path, bot_id: &str) -> PathBuf {
    let safe: String = bot_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    health_dir(dir).join(format!("{safe}.json"))
}

// ---------------------------------------------------------------------------
// Publishing (called from the poll loop)
// ---------------------------------------------------------------------------

/// Record a failed poll for `bot_id` and return the classified kind.
///
/// Best-effort persistence: a health-file write failure must never take down
/// the poll loop, so errors are returned for the caller to ignore rather than
/// bubbled into the polling path.
pub fn record_failure(
    dir: &Path,
    bot_id: &str,
    consecutive_failures: u32,
    redacted_error: &str,
    now: DateTime<Utc>,
) -> Result<PollFailureKind> {
    let kind = PollFailureKind::classify(redacted_error);
    let path = bot_health_path(dir, bot_id);
    let mut rec = load_bot(&path).unwrap_or_else(|| BotPollHealth::new(bot_id, now));
    rec.consecutive_failures = consecutive_failures;
    rec.total_failures = rec.total_failures.saturating_add(1);
    // Cap the stored error so a pathological error chain cannot grow the file
    // without bound.
    rec.last_error = Some(truncate(redacted_error, 500));
    rec.last_failure_kind = Some(kind.label().to_string());
    rec.last_failure_at = Some(now.to_rfc3339());
    rec.updated_at = now.to_rfc3339();
    save_bot(&path, &rec)?;
    Ok(kind)
}

/// Reset every bot's record at listener start, so a new run is never judged on
/// a previous run's failure streak.
///
/// Without this, a listener restarted after an outage would report DEAF (stale
/// streak, no success yet) until its first successful poll — and a health file
/// left behind by a listener that exited hours ago would make `wg service
/// status` blame a live-looking outage on a process that no longer exists. The
/// staleness check ([`ListenerHealth::stale_bots`]) covers the second case; this
/// covers the first.
pub fn reset_for_new_run(dir: &Path, bot_ids: &[String], now: DateTime<Utc>) -> Result<()> {
    for bot_id in bot_ids {
        let path = bot_health_path(dir, bot_id);
        save_bot(&path, &BotPollHealth::new(bot_id, now))?;
    }
    Ok(())
}

/// Record a successful poll for `bot_id`, clearing the failure streak.
pub fn record_success(dir: &Path, bot_id: &str, now: DateTime<Utc>) -> Result<()> {
    let path = bot_health_path(dir, bot_id);
    let mut rec = load_bot(&path).unwrap_or_else(|| BotPollHealth::new(bot_id, now));
    rec.consecutive_failures = 0;
    rec.last_success_at = Some(now.to_rfc3339());
    rec.updated_at = now.to_rfc3339();
    save_bot(&path, &rec)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn load_bot(path: &Path) -> Option<BotPollHealth> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_bot(path: &Path, rec: &BotPollHealth) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {:?}", parent))?;
    }
    let content = serde_json::to_string_pretty(rec).context("failed to serialize bot health")?;
    // Write-temp + rename so a reader (`wg service status`, the supervisor)
    // never observes a half-written file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("failed to write {:?}", tmp))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to rename into {:?}", path))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Aggregate view (read by `wg service status` and the supervisor)
// ---------------------------------------------------------------------------

/// Every bot's poll health, keyed by bot id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListenerHealth {
    pub bots: BTreeMap<String, BotPollHealth>,
}

impl ListenerHealth {
    /// Read every bot health file under `dir`. A missing directory yields an
    /// empty (not failed) view — "no listener has ever published health here"
    /// is a legitimate state, e.g. telegram is not configured.
    pub fn load(dir: &Path) -> Self {
        let mut bots = BTreeMap::new();
        let hdir = health_dir(dir);
        if let Ok(entries) = std::fs::read_dir(&hdir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(rec) = load_bot(&path) {
                    bots.insert(rec.bot_id.clone(), rec);
                }
            }
        }
        Self { bots }
    }

    /// True when no listener has published anything — nothing to report on.
    pub fn is_empty(&self) -> bool {
        self.bots.is_empty()
    }

    /// The bot ids that are deaf as of `now`, in stable order.
    pub fn deaf_bots(&self, now: DateTime<Utc>) -> Vec<&BotPollHealth> {
        self.bots.values().filter(|b| b.is_deaf(now)).collect()
    }

    /// Are ALL known bots deaf? The total-outage shape (one blocked
    /// destination takes every bot down at once, since the block is per-host
    /// and not per-token) — worth distinguishing from one sick bot.
    pub fn is_totally_deaf(&self, now: DateTime<Utc>) -> bool {
        !self.bots.is_empty() && self.deaf_bots(now).len() == self.bots.len()
    }

    /// Bots whose record has not been touched for `max_age_secs`.
    ///
    /// A live listener writes on every poll (success or failure) and the longest
    /// gap between writes is one long-poll window plus one backoff step, so a
    /// record older than a few minutes means **nobody is polling this bot** —
    /// the listener process is gone. That is a different failure from a deaf
    /// listener and must not be reported as one.
    pub fn stale_bots(&self, now: DateTime<Utc>, max_age_secs: i64) -> Vec<&BotPollHealth> {
        self.bots
            .values()
            .filter(|b| match parse_ts(&b.updated_at) {
                Some(ts) => (now - ts).num_seconds() >= max_age_secs,
                None => true,
            })
            .collect()
    }

    /// True when EVERY record is stale — no listener is publishing at all.
    pub fn is_stale(&self, now: DateTime<Utc>, max_age_secs: i64) -> bool {
        !self.bots.is_empty() && self.stale_bots(now, max_age_secs).len() == self.bots.len()
    }

    /// The dominant failure kind among deaf bots, for the headline diagnosis.
    pub fn dominant_kind(&self, now: DateTime<Utc>) -> Option<PollFailureKind> {
        let mut counts: BTreeMap<&'static str, (usize, PollFailureKind)> = BTreeMap::new();
        for bot in self.deaf_bots(now) {
            if let Some(k) = bot.kind() {
                let entry = counts.entry(k.label()).or_insert((0, k));
                entry.0 += 1;
            }
        }
        counts.values().max_by_key(|(n, _)| *n).map(|(_, k)| *k)
    }

    /// One-line summary for a status readout.
    ///
    /// Shapes:
    /// - `"not reporting (no listener health published)"`
    /// - `"OK — 5 bot(s) polling, last success 12s ago"`
    /// - `"DEAF — 5/5 bot(s) cannot poll (egress-blocked, 889 failures)"`
    pub fn summary_line(&self, now: DateTime<Utc>) -> String {
        if self.is_empty() {
            return "not reporting (no listener health published)".to_string();
        }
        // Nobody is publishing: the listener process is gone. Report THAT
        // rather than the last streak it happened to leave behind.
        if self.is_stale(now, STALE_AFTER_SECS) {
            let age = self
                .bots
                .values()
                .filter_map(|b| parse_ts(&b.updated_at))
                .max()
                .map(|ts| (now - ts).num_seconds().max(0));
            return match age {
                Some(secs) => format!(
                    "NOT POLLING — no listener has published health for {}s ({} bot(s) known); is `wg telegram listen` running?",
                    secs,
                    self.bots.len()
                ),
                None => "NOT POLLING — listener health is unreadable; is `wg telegram listen` running?".to_string(),
            };
        }
        let deaf = self.deaf_bots(now);
        if deaf.is_empty() {
            let freshest = self
                .bots
                .values()
                .filter_map(|b| b.last_success_at.as_deref().and_then(parse_ts))
                .max();
            match freshest {
                Some(ts) => format!(
                    "OK — {} bot(s) polling, last success {}s ago",
                    self.bots.len(),
                    (now - ts).num_seconds().max(0)
                ),
                None => format!("starting up — {} bot(s), no poll completed yet", self.bots.len()),
            }
        } else {
            let total: u64 = deaf.iter().map(|b| b.total_failures).sum();
            let kind = self
                .dominant_kind(now)
                .map(|k| k.label())
                .unwrap_or("unknown");
            format!(
                "DEAF — {}/{} bot(s) cannot poll ({}, {} failed poll(s))",
                deaf.len(),
                self.bots.len(),
                kind,
                total
            )
        }
    }

    /// The actionable second line for a status readout, when deaf or not polling.
    pub fn advice_line(&self, now: DateTime<Utc>) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        if self.is_stale(now, STALE_AFTER_SECS) {
            return Some(
                "the family chat receives nothing while no listener polls — start it with `casa up` (or `wg telegram listen`) and check .casa/telegram.log".to_string(),
            );
        }
        let kind = self.dominant_kind(now)?;
        Some(kind.diagnosis().to_string())
    }

    /// Should this be shouted about? True when inbound family messages are NOT
    /// being received — either because nothing is polling, or because every
    /// poll is failing. The single predicate the status readout and the casa
    /// supervisor both branch on, so the two can never disagree about whether
    /// the family is being heard.
    pub fn is_alarming(&self, now: DateTime<Utc>) -> bool {
        if self.is_empty() {
            return false; // telegram may simply not be configured
        }
        self.is_stale(now, STALE_AFTER_SECS) || !self.deaf_bots(now).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // --- classification -----------------------------------------------------

    #[test]
    fn classifies_the_live_wsa_incident_error_as_egress_blocked() {
        // Verbatim (already token-redacted) shape from .casa/telegram.log,
        // 2026-07-24 — 889 of these while the family chat was deaf.
        let err = "getUpdates request failed: error sending request for url \
                   (https://api.telegram.org/bot<redacted>/getUpdates): \
                   client error (Connect): tls handshake eof";
        assert_eq!(PollFailureKind::classify(err), PollFailureKind::EgressBlocked);
        assert!(PollFailureKind::classify(err)
            .diagnosis()
            .contains("blocked"));
    }

    #[test]
    fn classifies_block_page_403_as_egress_blocked_not_unauthorized() {
        // A Cisco WSA answers plain HTTP with a 403 block page. That is a
        // POLICY block, not a bad bot token — mislabelling it would send the
        // operator to rotate credentials for no reason.
        let err = "HTTP status 403 Forbidden (Via: 1.1 phswsa3.partners.org:80 (Cisco-WSA/15.2.0-164))";
        assert_eq!(PollFailureKind::classify(err), PollFailureKind::EgressBlocked);
    }

    #[test]
    fn classifies_real_auth_failure_as_unauthorized() {
        let err = "getUpdates request failed: 401 Unauthorized";
        assert_eq!(PollFailureKind::classify(err), PollFailureKind::Unauthorized);
    }

    #[test]
    fn classifies_dns_and_timeout_and_rate_limit_distinctly() {
        assert_eq!(
            PollFailureKind::classify("error sending request: dns error: failed to lookup address information"),
            PollFailureKind::DnsFailure
        );
        assert_eq!(
            PollFailureKind::classify("operation timed out"),
            PollFailureKind::Timeout
        );
        assert_eq!(
            PollFailureKind::classify("429 Too Many Requests: retry after 5"),
            PollFailureKind::RateLimited
        );
    }

    #[test]
    fn unknown_error_is_unclassified_not_silently_a_network_block() {
        // Guard against the classifier growing an over-broad catch-all: a
        // brand-new failure mode must surface as "unclassified" so nobody
        // chases a nonexistent firewall.
        assert_eq!(
            PollFailureKind::classify("the moon exploded"),
            PollFailureKind::Other
        );
    }

    // --- per-bot deafness ---------------------------------------------------

    #[test]
    fn fresh_record_is_not_deaf() {
        let rec = BotPollHealth::new("nora", t("2026-07-24T19:00:00Z"));
        assert!(!rec.is_deaf(t("2026-07-24T19:00:01Z")));
    }

    #[test]
    fn streak_at_threshold_is_deaf() {
        let mut rec = BotPollHealth::new("nora", t("2026-07-24T19:00:00Z"));
        rec.consecutive_failures = DEAF_AFTER_FAILURES - 1;
        rec.total_failures = u64::from(DEAF_AFTER_FAILURES - 1);
        rec.last_failure_at = Some(t("2026-07-24T19:00:05Z").to_rfc3339());
        rec.last_success_at = Some(t("2026-07-24T19:00:00Z").to_rfc3339());
        assert!(!rec.is_deaf(t("2026-07-24T19:00:06Z")), "under threshold");
        rec.consecutive_failures = DEAF_AFTER_FAILURES;
        assert!(rec.is_deaf(t("2026-07-24T19:00:06Z")));
    }

    #[test]
    fn flapping_bot_with_reset_streak_is_still_deaf_after_prolonged_silence() {
        // The trap a naive counter falls into: a bot that succeeds once every
        // few minutes resets its streak, so a streak-only check reports
        // healthy while the family gets nothing.
        let mut rec = BotPollHealth::new("bruno", t("2026-07-24T18:00:00Z"));
        rec.consecutive_failures = 1;
        rec.total_failures = 400;
        rec.last_success_at = Some(t("2026-07-24T18:00:00Z").to_rfc3339());
        rec.last_failure_at = Some(t("2026-07-24T19:00:00Z").to_rfc3339());
        assert!(rec.is_deaf(t("2026-07-24T19:00:00Z")), "1h without a success");
    }

    #[test]
    fn healthy_idle_bot_is_not_deaf() {
        // A quiet family produces long-poll successes with no messages; that
        // must never read as deaf.
        let mut rec = BotPollHealth::new("mira", t("2026-07-24T19:00:00Z"));
        rec.total_failures = 2;
        rec.consecutive_failures = 0;
        rec.last_success_at = Some(t("2026-07-24T19:04:00Z").to_rfc3339());
        assert!(!rec.is_deaf(t("2026-07-24T19:05:00Z")));
    }

    // --- persistence + aggregate -------------------------------------------

    #[test]
    fn failure_then_success_round_trips_through_the_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = t("2026-07-24T19:00:00Z");

        let kind = record_failure(
            dir,
            "nora",
            1,
            "client error (Connect): tls handshake eof",
            now,
        )
        .unwrap();
        assert_eq!(kind, PollFailureKind::EgressBlocked);

        let health = ListenerHealth::load(dir);
        let nora = health.bots.get("nora").expect("nora published health");
        assert_eq!(nora.consecutive_failures, 1);
        assert_eq!(nora.total_failures, 1);
        assert_eq!(nora.last_failure_kind.as_deref(), Some("egress-blocked"));
        assert!(nora.last_success_at.is_none());

        record_success(dir, "nora", t("2026-07-24T19:00:30Z")).unwrap();
        let health = ListenerHealth::load(dir);
        let nora = &health.bots["nora"];
        assert_eq!(nora.consecutive_failures, 0);
        assert_eq!(nora.total_failures, 1, "lifetime count is not reset");
        assert!(nora.last_success_at.is_some());
    }

    #[test]
    fn five_bots_blocked_reports_total_deafness_with_the_egress_diagnosis() {
        // The exact live incident: all five bots, same destination block.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = t("2026-07-24T19:00:00Z");
        for bot in ["nora", "bruno", "otto", "mira", "chiller"] {
            record_failure(
                dir,
                bot,
                65,
                "getUpdates request failed: client error (Connect): tls handshake eof",
                now,
            )
            .unwrap();
        }
        let health = ListenerHealth::load(dir);
        assert_eq!(health.bots.len(), 5);
        assert!(health.is_totally_deaf(now));
        assert_eq!(
            health.dominant_kind(now),
            Some(PollFailureKind::EgressBlocked)
        );
        let line = health.summary_line(now);
        assert!(line.starts_with("DEAF — 5/5"), "got: {line}");
        assert!(line.contains("egress-blocked"), "got: {line}");
        assert!(health.advice_line(now).unwrap().contains("api.telegram.org"));
    }

    #[test]
    fn all_healthy_reports_ok_and_no_advice() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = t("2026-07-24T19:00:00Z");
        for bot in ["nora", "bruno"] {
            record_success(dir, bot, t("2026-07-24T18:59:50Z")).unwrap();
        }
        let health = ListenerHealth::load(dir);
        assert!(!health.is_totally_deaf(now));
        let line = health.summary_line(now);
        assert!(line.starts_with("OK — 2 bot(s) polling"), "got: {line}");
        assert!(health.advice_line(now).is_none());
    }

    #[test]
    fn no_published_health_is_reported_as_not_reporting_never_as_ok() {
        // The status-lie guard: an absent health file must NOT read as healthy.
        let tmp = tempfile::tempdir().unwrap();
        let health = ListenerHealth::load(tmp.path());
        assert!(health.is_empty());
        assert!(health.summary_line(t("2026-07-24T19:00:00Z")).contains("not reporting"));
        assert!(!health.is_totally_deaf(t("2026-07-24T19:00:00Z")));
    }

    #[test]
    fn one_sick_bot_is_partial_not_total_deafness() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = t("2026-07-24T19:00:00Z");
        record_success(dir, "nora", t("2026-07-24T18:59:55Z")).unwrap();
        record_failure(dir, "bruno", 10, "401 Unauthorized", now).unwrap();
        let health = ListenerHealth::load(dir);
        assert!(!health.is_totally_deaf(now));
        assert_eq!(health.deaf_bots(now).len(), 1);
        assert_eq!(health.dominant_kind(now), Some(PollFailureKind::Unauthorized));
        assert!(health.summary_line(now).starts_with("DEAF — 1/2"));
    }

    #[test]
    fn abandoned_health_files_report_not_polling_not_a_stale_deaf_streak() {
        // The status-lie in the other direction: a listener that exited hours
        // ago leaves a failure streak on disk. Reporting that as "DEAF —
        // egress-blocked" sends the operator hunting a firewall when the real
        // answer is "nothing is running".
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record_failure(dir, "nora", 65, "tls handshake eof", t("2026-07-24T12:00:00Z")).unwrap();
        let health = ListenerHealth::load(dir);
        let now = t("2026-07-24T19:00:00Z"); // seven hours later
        assert!(health.is_stale(now, STALE_AFTER_SECS));
        let line = health.summary_line(now);
        assert!(line.starts_with("NOT POLLING"), "got: {line}");
        assert!(health.is_alarming(now), "silence is still an alarm");
        assert!(health.advice_line(now).unwrap().contains("casa up"));
    }

    #[test]
    fn a_live_listener_inside_the_backoff_ladder_is_not_stale() {
        // A blocked listener writes once per backoff step (max 60s). It must
        // read as DEAF, never as "not polling" — the cause matters.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record_failure(dir, "nora", 65, "tls handshake eof", t("2026-07-24T18:59:15Z")).unwrap();
        let health = ListenerHealth::load(dir);
        let now = t("2026-07-24T19:00:00Z"); // 45s later — inside one backoff step
        assert!(!health.is_stale(now, STALE_AFTER_SECS));
        assert!(health.summary_line(now).starts_with("DEAF"));
    }

    #[test]
    fn reset_for_new_run_clears_a_previous_runs_streak() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record_failure(dir, "nora", 65, "tls handshake eof", t("2026-07-24T12:00:00Z")).unwrap();
        let start = t("2026-07-24T19:00:00Z");
        reset_for_new_run(dir, &["nora".to_string(), "bruno".to_string()], start).unwrap();

        let health = ListenerHealth::load(dir);
        assert_eq!(health.bots.len(), 2);
        assert!(!health.is_alarming(t("2026-07-24T19:00:01Z")));
        assert!(health.summary_line(t("2026-07-24T19:00:01Z")).contains("starting up"));
        assert_eq!(health.bots["nora"].total_failures, 0);
    }

    #[test]
    fn unconfigured_telegram_is_not_an_alarm() {
        // No bots, no health files — a household that does not use Telegram
        // must not see a permanent warning.
        let tmp = tempfile::tempdir().unwrap();
        let health = ListenerHealth::load(tmp.path());
        assert!(!health.is_alarming(t("2026-07-24T19:00:00Z")));
        assert!(health.advice_line(t("2026-07-24T19:00:00Z")).is_none());
    }

    #[test]
    fn bot_id_with_path_separators_cannot_escape_the_health_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = bot_health_path(tmp.path(), "../../etc/passwd");
        assert_eq!(p.parent().unwrap(), health_dir(tmp.path()));
        // Dots are replaced too, so no `..` component survives at all.
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "______etc_passwd.json"
        );
    }
}
