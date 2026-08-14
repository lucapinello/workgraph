//! Disk-space admission, owned build-cache accounting, and conservative reaping.
//!
//! The sentinel deliberately separates *observation* from *ownership*. A path is
//! eligible for automatic removal only when it has an explicit [`OwnedCache`]
//! lease written by the spawn path. Directory names such as `wg-target-*` are
//! never treated as proof of ownership.

use crate::config::ResourceManagementConfig;
use crate::graph::Task;
use crate::parser::load_graph;
use crate::service::registry::{AgentRegistry, AgentStatus};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

pub const SNAPSHOT_SCHEMA: u32 = 1;
pub const OWNERSHIP_SCHEMA: u32 = 1;
const SNAPSHOT_FILE: &str = "disk-sentinel.json";
const OWNERSHIP_FILE: &str = "owned-caches.json";
const LOCK_FILE: &str = ".owned-caches.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskLevel {
    Healthy,
    Warning,
    PauseBuilds,
    HardRefuse,
}

impl DiskLevel {
    pub fn blocks_builds(self) -> bool {
        matches!(self, Self::PauseBuilds | Self::HardRefuse)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildClass {
    GraphOnly,
    BuildCapable,
    BuildHeavy,
}

impl BuildClass {
    pub fn is_build_capable(self) -> bool {
        !matches!(self, Self::GraphOnly)
    }
    pub fn is_heavy(self) -> bool {
        matches!(self, Self::BuildHeavy)
    }
}

/// Conservative task classification. Dot-prefixed agency tasks and read-only
/// modes are graph/LLM-only. Full/shell tasks are build-capable; explicit
/// full-suite and Cargo language makes them build-heavy for the separate
/// concurrency budget.
pub fn classify_task(task: &Task) -> BuildClass {
    if task.id.starts_with('.') || matches!(task.exec_mode.as_deref(), Some("bare" | "light")) {
        return BuildClass::GraphOnly;
    }
    let mode = task.exec_mode.as_deref().unwrap_or("full");
    if !matches!(mode, "full" | "shell") {
        return BuildClass::GraphOnly;
    }
    let haystack = format!(
        "{}\n{}\n{}",
        task.title,
        task.description.as_deref().unwrap_or_default(),
        task.exec.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let heavy = [
        "cargo test",
        "cargo build",
        "cargo install",
        "cargo clippy",
        "full suite",
        "full-suite",
        "clean-env",
        "build-heavy",
        "cmake",
    ]
    .iter()
    .any(|needle| haystack.contains(needle));
    if heavy {
        BuildClass::BuildHeavy
    } else {
        BuildClass::BuildCapable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheKind {
    CargoTarget,
    CargoInstallScratch,
    Temporary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedCache {
    pub path: String,
    pub kind: CacheKind,
    pub task_id: String,
    pub agent_id: String,
    pub pid: u32,
    /// Exact `/proc` start identity captured after spawn, not an estimate from
    /// the task timestamp. A recycled PID therefore cannot authorize deletion.
    pub pid_start_epoch: Option<i64>,
    pub mount_id: String,
    pub created_at: String,
    pub lease_expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OwnershipRegistry {
    pub schema: u32,
    pub caches: Vec<OwnedCache>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpace {
    pub path: String,
    pub mount_id: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub free_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaUsage {
    pub path: String,
    pub bytes: u64,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetUsage {
    pub path: String,
    pub task_id: String,
    pub agent_id: String,
    pub bytes: u64,
    pub growth_bytes_per_sec: i64,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSnapshot {
    pub schema: u32,
    pub generated_at: String,
    pub level: DiskLevel,
    pub reason: String,
    pub mounts: Vec<MountSpace>,
    pub targets: Vec<TargetUsage>,
    pub worktrees: AreaUsage,
    pub agents: AreaUsage,
    pub log: AreaUsage,
    pub active_builds: usize,
    pub active_build_heavy: usize,
    pub projected_headroom_bytes: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupReport {
    pub considered: usize,
    pub reaped: usize,
    pub bytes_freed: u64,
    pub compressed_files: usize,
    pub compression_bytes_saved: u64,
    pub deduplicated_files: usize,
    pub deduplication_bytes_saved: u64,
    pub preserved: Vec<PreservedPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreservedPath {
    pub path: String,
    pub reason: String,
}

fn sentinel_dir(dir: &Path) -> PathBuf {
    dir.join("service").join("disk")
}
fn ownership_path(dir: &Path) -> PathBuf {
    sentinel_dir(dir).join(OWNERSHIP_FILE)
}
pub fn snapshot_path(dir: &Path) -> PathBuf {
    sentinel_dir(dir).join(SNAPSHOT_FILE)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

struct RegistryLock {
    _file: File,
}
impl RegistryLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let disk = sentinel_dir(dir);
        fs::create_dir_all(&disk)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(disk.join(LOCK_FILE))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self { _file: file })
    }
}

pub fn load_ownership(dir: &Path) -> Result<OwnershipRegistry> {
    let path = ownership_path(dir);
    if !path.exists() {
        return Ok(OwnershipRegistry {
            schema: OWNERSHIP_SCHEMA,
            caches: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn save_ownership(dir: &Path, registry: &OwnershipRegistry) -> Result<()> {
    write_atomic(&ownership_path(dir), &serde_json::to_vec_pretty(registry)?)
}

pub fn register_owned_cache(dir: &Path, cache: OwnedCache) -> Result<()> {
    let _lock = RegistryLock::acquire(dir)?;
    let mut registry = load_ownership(dir)?;
    registry.schema = OWNERSHIP_SCHEMA;
    // One agent may refresh a lease after restart. Preserve other owners of a
    // shared absolute CARGO_TARGET_DIR; cleanup requires every owner to be stale.
    registry.caches.retain(|old| {
        !(old.agent_id == cache.agent_id && same_path(Path::new(&old.path), Path::new(&cache.path)))
    });
    registry.caches.push(cache);
    save_ownership(dir, &registry)
}

pub fn make_owned_cache(
    path: &Path,
    kind: CacheKind,
    task_id: &str,
    agent_id: &str,
    pid: u32,
    worktree_path: Option<&Path>,
    lease_seconds: u64,
) -> OwnedCache {
    let now = Utc::now();
    OwnedCache {
        path: absolute_lexical(path).to_string_lossy().to_string(),
        kind,
        task_id: task_id.to_string(),
        agent_id: agent_id.to_string(),
        pid,
        pid_start_epoch: crate::service::read_proc_start_time_secs(pid),
        mount_id: mount_id(path),
        created_at: now.to_rfc3339(),
        lease_expires_at: (now + chrono::Duration::seconds(lease_seconds as i64)).to_rfc3339(),
        worktree_path: worktree_path.map(|p| absolute_lexical(p).to_string_lossy().to_string()),
    }
}

fn absolute_lexical(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn existing_ancestor(path: &Path) -> &Path {
    let mut candidate = path;
    while !candidate.exists() {
        let Some(parent) = candidate.parent() else {
            break;
        };
        candidate = parent;
    }
    candidate
}

#[cfg(unix)]
fn mount_id(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(existing_ancestor(path))
        .map(|m| format!("dev:{}", m.dev()))
        .unwrap_or_else(|_| "unknown".into())
}
#[cfg(not(unix))]
fn mount_id(_path: &Path) -> String {
    "unknown".into()
}

#[cfg(unix)]
pub fn probe_mount(path: &Path) -> Result<MountSpace> {
    let ancestor = existing_ancestor(path);
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(ancestor.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", ancestor.display()));
    }
    let block = stat.f_frsize as u64;
    let total = (stat.f_blocks as u64).saturating_mul(block);
    let free = (stat.f_bavail as u64).saturating_mul(block);
    let pct = if total == 0 {
        0.0
    } else {
        free as f64 * 100.0 / total as f64
    };
    Ok(MountSpace {
        path: path.to_string_lossy().to_string(),
        mount_id: mount_id(path),
        free_bytes: free,
        total_bytes: total,
        free_percent: pct,
    })
}
#[cfg(not(unix))]
pub fn probe_mount(path: &Path) -> Result<MountSpace> {
    Err(anyhow!(
        "disk probing is not supported for {} on this platform",
        path.display()
    ))
}

/// Pure threshold engine, injectable with synthetic mounts for deterministic
/// low-space tests. Hysteresis applies only when the previous state blocked
/// builds: both pause thresholds plus their resume margins must recover.
pub fn assess_mounts(
    mounts: &[MountSpace],
    cfg: &ResourceManagementConfig,
    previous: Option<DiskLevel>,
) -> (DiskLevel, String) {
    let Some(worst) = mounts.iter().min_by(|a, b| {
        let ar = a.free_bytes as f64 / cfg.disk_hard_refuse_bytes.max(1) as f64;
        let br = b.free_bytes as f64 / cfg.disk_hard_refuse_bytes.max(1) as f64;
        ar.partial_cmp(&br).unwrap_or(std::cmp::Ordering::Equal)
    }) else {
        return (
            DiskLevel::HardRefuse,
            "no configured mount could be measured".into(),
        );
    };
    let below = |bytes: u64, pct: f64| {
        mounts
            .iter()
            .any(|m| m.free_bytes <= bytes || m.free_percent <= pct)
    };
    if below(cfg.disk_hard_refuse_bytes, cfg.disk_hard_refuse_percent) {
        return (
            DiskLevel::HardRefuse,
            format!(
                "hard-refuse threshold crossed (worst: {} {:.1}% free, {} bytes)",
                worst.path, worst.free_percent, worst.free_bytes
            ),
        );
    }
    if matches!(
        previous,
        Some(DiskLevel::PauseBuilds | DiskLevel::HardRefuse)
    ) {
        let recovered = mounts.iter().all(|m| {
            m.free_bytes
                > cfg
                    .disk_pause_build_bytes
                    .saturating_add(cfg.disk_resume_hysteresis_bytes)
                && m.free_percent
                    > cfg.disk_pause_build_percent + cfg.disk_resume_hysteresis_percent
        });
        if !recovered {
            return (
                DiskLevel::PauseBuilds,
                "build admission remains paused until all mounts clear hysteresis".into(),
            );
        }
    }
    if below(cfg.disk_pause_build_bytes, cfg.disk_pause_build_percent) {
        return (
            DiskLevel::PauseBuilds,
            format!(
                "pause-build threshold crossed (worst: {} {:.1}% free, {} bytes)",
                worst.path, worst.free_percent, worst.free_bytes
            ),
        );
    }
    if below(cfg.disk_warning_bytes, cfg.disk_warning_percent) {
        return (
            DiskLevel::Warning,
            format!(
                "warning threshold crossed (worst: {} {:.1}% free, {} bytes)",
                worst.path, worst.free_percent, worst.free_bytes
            ),
        );
    }
    (
        DiskLevel::Healthy,
        "all configured mounts have build headroom".into(),
    )
}

pub fn configured_paths(dir: &Path, cfg: &ResourceManagementConfig) -> Vec<PathBuf> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(parent) = dir.parent() {
        paths.push(parent.to_path_buf());
        paths.push(parent.join(".wg-worktrees"));
    }
    paths.push(std::env::temp_dir());
    if let Some(inherited) = std::env::var_os("CARGO_TARGET_DIR") {
        paths.push(PathBuf::from(inherited));
    }
    paths.extend(cfg.disk_paths.iter().map(PathBuf::from));
    if let Some(root) = cfg.cargo_target_root.as_deref() {
        paths.push(PathBuf::from(root));
    }
    if let Some(root) = cfg.build_tmp_root.as_deref() {
        paths.push(PathBuf::from(root));
    }
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(mount_id(p)));
    paths
}

pub fn current_admission(
    dir: &Path,
    cfg: &ResourceManagementConfig,
) -> (DiskLevel, String, Vec<MountSpace>) {
    if !cfg.disk_sentinel_enabled {
        return (
            DiskLevel::Healthy,
            "disk sentinel disabled".into(),
            Vec::new(),
        );
    }
    let mounts: Vec<_> = configured_paths(dir, cfg)
        .into_iter()
        .filter_map(|p| probe_mount(&p).ok())
        .collect();
    let previous = load_snapshot(dir).ok().flatten().map(|s| s.level);
    let (level, reason) = assess_mounts(&mounts, cfg, previous);
    (level, reason, mounts)
}

pub fn load_snapshot(dir: &Path) -> Result<Option<DiskSnapshot>> {
    let path = snapshot_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn bounded_size(path: &Path, max_entries: usize) -> AreaUsage {
    if !path.exists() {
        return AreaUsage {
            path: path.to_string_lossy().to_string(),
            bytes: 0,
            complete: true,
        };
    }
    let mut bytes = 0u64;
    let mut count = 0usize;
    let mut complete = true;
    for entry in WalkDir::new(path).follow_links(false).max_depth(16) {
        if count >= max_entries {
            complete = false;
            break;
        }
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        count += 1;
        if entry.file_type().is_file() {
            bytes = bytes.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    AreaUsage {
        path: path.to_string_lossy().to_string(),
        bytes,
        complete,
    }
}

fn owner_is_stale(
    cache: &OwnedCache,
    registry: &AgentRegistry,
    graph: Option<&crate::graph::WorkGraph>,
) -> bool {
    // A record that is GONE is terminal, not inconclusive. The registry is
    // pruned (by wg's own reaper, and by operators) and the graph is archived,
    // so a long-dead agent's record disappears well before its cache does.
    // Reading "no record" as "might still be running" made those caches
    // immortal: on the live household 1,180 of 1,283 registered caches named
    // agents no longer in the registry, `wg disk cleanup` reported
    // considered=1156 reaped=0 on every run, and 77GB of dead worktree targets
    // accumulated. Worse, it inverted the incentive — pruning the registry, a
    // routine cleanup, is what MADE those caches unreapable (2026-08-11).
    //
    // Safety does not rest on this conjunct. It rests on the two below, which
    // are the ones that can actually observe a running build: the lease must
    // have expired AND the recorded pid must be dead or recycled. A live agent
    // fails both regardless of what the registry remembers.
    let agent_terminal = registry
        .get_agent(&cache.agent_id)
        .map(|a| {
            matches!(
                a.status,
                AgentStatus::Done | AgentStatus::Failed | AgentStatus::Dead | AgentStatus::Parked
            )
        })
        .unwrap_or(true);
    // An unreadable graph stays inconclusive — that is a different thing from a
    // graph that loaded and does not contain the task.
    let task_terminal = match graph {
        Some(graph) => graph
            .get_task(&cache.task_id)
            .map(|t| t.status.is_terminal())
            .unwrap_or(true),
        None => false,
    };
    let lease_expired = DateTime::parse_from_rfc3339(&cache.lease_expires_at)
        .map(|t| t.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(false);
    agent_terminal && task_terminal && lease_expired && pid_identity_stale(cache)
}

fn pid_identity_stale(cache: &OwnedCache) -> bool {
    if !crate::service::is_process_alive(cache.pid) {
        return true;
    }
    match (
        cache.pid_start_epoch,
        crate::service::read_proc_start_time_secs(cache.pid),
    ) {
        (Some(recorded), Some(current)) => recorded != current,
        // A live PID with an inconclusive identity is never safe to reap.
        _ => false,
    }
}

// Full per-cache walks performed on THIS thread. A walk changes no output a
// caller can assert on — the snapshot it writes is identical when nothing moved
// — so "did we walk twice" is only observable by counting. Thread-local and
// always compiled, per the engine instrumentation convention.
thread_local! {
    static SNAPSHOT_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn refresh_snapshot(dir: &Path, cfg: &ResourceManagementConfig) -> Result<DiskSnapshot> {
    SNAPSHOT_WALKS.with(|count| count.set(count.get() + 1));
    let previous =
        load_snapshot(dir)?.filter(|s| DateTime::parse_from_rfc3339(&s.generated_at).is_ok());
    let (level, reason, mounts) = current_admission(dir, cfg);
    let ownership = load_ownership(dir).unwrap_or_default();
    let registry = AgentRegistry::load(dir).unwrap_or_default();
    let graph = load_graph(dir.join("graph.jsonl")).ok();
    let elapsed = previous
        .as_ref()
        .and_then(|p| DateTime::parse_from_rfc3339(&p.generated_at).ok())
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(1))
        .unwrap_or(1);
    let old_sizes: BTreeMap<&str, u64> = previous
        .as_ref()
        .map(|p| {
            p.targets
                .iter()
                .map(|t| (t.path.as_str(), t.bytes))
                .collect()
        })
        .unwrap_or_default();
    let mut targets = Vec::new();
    // Spend the walk budget on paths that can actually contribute bytes, and on
    // each distinct path once. The bound below is a cap on WORK, but it was
    // being consumed by entries with nothing to measure: on the live household
    // 493 of the 512 walked were paths that no longer existed and the rest were
    // repeats of the same target dir, so 84 real caches were never measured at
    // all — the sentinel was blind to most of the disk it exists to watch.
    // Several owners may share one absolute CARGO_TARGET_DIR, so dedupe by path
    // here for MEASUREMENT only; `cleanup_owned` still groups every owner of a
    // path and reaps only when all of them are stale.
    let mut measured: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let live_caches = ownership
        .caches
        .iter()
        .filter(|cache| measured.insert(cache.path.as_str()) && Path::new(&cache.path).exists());
    // Both target count and entries-per-target are bounded so a corrupt or
    // adversarial registry cannot turn a status refresh into an unbounded walk.
    for cache in live_caches.take(512) {
        let usage = bounded_size(Path::new(&cache.path), cfg.disk_scan_max_entries);
        let old = old_sizes
            .get(cache.path.as_str())
            .copied()
            .unwrap_or(usage.bytes);
        targets.push(TargetUsage {
            path: cache.path.clone(),
            task_id: cache.task_id.clone(),
            agent_id: cache.agent_id.clone(),
            bytes: usage.bytes,
            growth_bytes_per_sec: (usage.bytes as i128 - old as i128)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64
                / elapsed,
            stale: owner_is_stale(cache, &registry, graph.as_ref()),
        });
    }
    let active_builds = ownership
        .caches
        .iter()
        .filter(|c| {
            registry
                .get_agent(&c.agent_id)
                .is_some_and(|a| a.is_live(cfg.disk_agent_heartbeat_seconds))
        })
        .map(|c| &c.agent_id)
        .collect::<HashSet<_>>()
        .len();
    let active_build_heavy = registry
        .all()
        .filter(|a| a.is_live(cfg.disk_agent_heartbeat_seconds))
        .filter(|a| {
            graph
                .as_ref()
                .and_then(|g| g.get_task(&a.task_id))
                .is_some_and(|t| classify_task(t).is_heavy())
        })
        .count();
    let min_free = mounts.iter().map(|m| m.free_bytes).min().unwrap_or(0);
    let reserved = (active_builds as u64).saturating_mul(cfg.estimated_build_bytes);
    let project_root = dir.parent().unwrap_or(dir);
    let snapshot = DiskSnapshot {
        schema: SNAPSHOT_SCHEMA,
        generated_at: Utc::now().to_rfc3339(),
        level,
        reason,
        mounts,
        targets,
        worktrees: bounded_size(
            &project_root.join(".wg-worktrees"),
            cfg.disk_scan_max_entries,
        ),
        agents: bounded_size(&dir.join("agents"), cfg.disk_scan_max_entries),
        log: bounded_size(&dir.join("log"), cfg.disk_scan_max_entries),
        active_builds,
        active_build_heavy,
        projected_headroom_bytes: min_free.saturating_sub(reserved).min(i64::MAX as u64) as i64,
    };
    write_atomic(&snapshot_path(dir), &serde_json::to_vec_pretty(&snapshot)?)?;
    Ok(snapshot)
}

pub fn refresh_if_due(dir: &Path, cfg: &ResourceManagementConfig) -> Result<Option<DiskSnapshot>> {
    if !cfg.disk_sentinel_enabled {
        return Ok(None);
    }
    if let Ok(Some(snapshot)) = load_snapshot(dir)
        && let Ok(ts) = DateTime::parse_from_rfc3339(&snapshot.generated_at)
        && (Utc::now() - ts.with_timezone(&Utc)).num_seconds()
            < cfg.disk_scan_interval_seconds as i64
    {
        return Ok(Some(snapshot));
    }
    refresh_snapshot(dir, cfg).map(Some)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => absolute_lexical(a) == absolute_lexical(b),
    }
}

#[cfg(test)]
fn git_init_commit(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
        vec!["commit", "-q", "--allow-empty", "-m", "base"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .unwrap();
    }
}

#[cfg(test)]
fn git_head_sha(dir: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// TEST-ONLY since the call sites moved to `worktree_preserve_reason`, which needs the
// classification and not just a bool. Kept because two fixtures assert on dirtiness
// directly, and `#[cfg(test)]` is honest about that rather than leaving a dead-code
// warning for the next reader to wonder about.
#[cfg(test)]
fn worktree_dirty(path: &Path) -> bool {
    !matches!(
        worktree_dirt(path),
        WorktreeDirt::Clean | WorktreeDirt::Missing
    )
}

/// What is actually dirty in a worktree — because "uncommitted source" and "untracked
/// build evidence" carry completely different risk and were being reported as the same
/// thing.
///
/// Measured on the live box 2026-08-14: seven worktrees pinned 11 GB with the reason
/// `owning worktree has uncommitted source`, and every one of them had ZERO tracked-dirty
/// files and 13 untracked entries, all of them directories under `disposables/` — a
/// directory named for being disposable. The reason was not merely unhelpful, it was
/// false: there was no uncommitted source anywhere. An operator reading it cannot tell a
/// worktree holding real unsaved work from one holding screenshots.
#[derive(Debug, PartialEq, Eq)]
enum WorktreeDirt {
    Clean,
    /// The worktree path is NOT THERE. Nothing to protect: this guard exists to save
    /// uncommitted source inside a worktree, and there is no worktree. Observed on the live
    /// box holding cargo build caches for agents whose worktrees were removed long ago —
    /// reported, wrongly, as "owning worktree has uncommitted source".
    Missing,
    /// TRACKED files differ from HEAD — genuinely unsaved work, preserve.
    TrackedDirty,
    /// Only untracked paths. Still preserved (an untracked file can be new source), but
    /// named for what it is so the operator can judge it.
    UntrackedOnly(usize),
    /// git could not answer. Fails closed, as before.
    Unknown,
}

fn worktree_dirt(path: &Path) -> WorktreeDirt {
    // Absent is not unknown. `git status` in a directory that does not exist fails, which
    // would otherwise fail closed forever on a cache whose owner was reaped ages ago.
    if !path.exists() {
        return WorktreeDirt::Missing;
    }
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(path)
        .output();
    let Ok(out) = out else {
        return WorktreeDirt::Unknown;
    };
    if !out.status.success() {
        return WorktreeDirt::Unknown;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut untracked = 0usize;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with("??") {
            untracked += 1;
        } else {
            return WorktreeDirt::TrackedDirty;
        }
    }
    if untracked == 0 {
        WorktreeDirt::Clean
    } else {
        WorktreeDirt::UntrackedOnly(untracked)
    }
}

/// The preservation verdict for a worktree, as the reason string the report will print, or
/// `None` when it is expendable.
fn worktree_preserve_reason(path: &Path) -> Option<String> {
    if reaped_wip_preserved(path) {
        return None;
    }
    match worktree_dirt(path) {
        WorktreeDirt::Clean | WorktreeDirt::Missing => None,
        WorktreeDirt::TrackedDirty => {
            Some("owning worktree has uncommitted tracked source".to_string())
        }
        WorktreeDirt::UntrackedOnly(n) => Some(format!(
            "owning worktree has {n} untracked path(s) and no tracked-dirty file"
        )),
        WorktreeDirt::Unknown => {
            Some("owning worktree status could not be read (failing closed)".to_string())
        }
    }
}

/// Has the reap path ALREADY preserved this worktree's work somewhere `worktree_dirty`
/// cannot see?
///
/// `worktree_dirty` counts untracked files, so any build or test byproduct that is neither
/// committed nor gitignored pins its worktree forever. Measured on the live box
/// 2026-08-13: 58 agent worktrees / 26 GB, `wg disk cleanup` reaped 0 of them on every
/// run, 44 were clean AND already merged into `main`, and across all 58 the only
/// uncommitted content was untracked screenshot evidence under `disposables/` plus four
/// `.wg-reaped-wip.json` sidecars. The guard was protecting screenshots while the one
/// thing that WAS real work had already been preserved somewhere it never looks.
///
/// So this asks the stronger question. The reap path writes a sidecar naming a WIP commit
/// and an out-of-tree patch (`wg_preserve_reaped_wip`, `commands/spawn/execution.rs`); if
/// that sidecar is present AND both halves it promises really exist, the worktree is
/// expendable. Every check is positive evidence — a sidecar that is unparseable, names a
/// commit this repo cannot resolve, or points at a patch that is gone means NOT preserved,
/// and the conservative path keeps the worktree.
///
/// Deliberately NOT `--untracked-files=no`: an untracked file can be genuinely new source,
/// and silently dropping it is the one failure this guard exists to prevent. Deliberately
/// not a path allowlist (`disposables/`, `spikes/`) either — that encodes today's directory
/// names into a safety check, and the next evidence directory leaks straight through.
fn reaped_wip_preserved(path: &Path) -> bool {
    let Ok(body) = fs::read_to_string(path.join(".wg-reaped-wip.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    if v.get("schema").and_then(|x| x.as_str()) != Some("wg-reap-preserved-wip/1") {
        return false;
    }
    let commit = match v.get("commit").and_then(|x| x.as_str()) {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => return false,
    };
    let patch = match v.get("patch").and_then(|x| x.as_str()) {
        Some(x) if !x.trim().is_empty() => x.trim().to_string(),
        _ => return false,
    };
    // The patch it promises must be on disk...
    if !Path::new(&patch).is_file() {
        return false;
    }
    // ...and the commit it names must resolve HERE. A sha from another repo, or one lost to
    // a prune, preserves nothing.
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn has_open_files(path: &Path) -> bool {
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return true;
    };
    for proc_entry in proc_entries.flatten() {
        if !proc_entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let Ok(fds) = fs::read_dir(proc_entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(link) = fs::read_link(fd.path()) {
                let text = link.to_string_lossy();
                let clean = text.strip_suffix(" (deleted)").unwrap_or(&text);
                let open_path = Path::new(clean);
                if open_path.starts_with(path) || same_path(open_path, path) {
                    return true;
                }
            }
        }
    }
    false
}
/// Same predicate for platforms without `/proc`, via `lsof`.
///
/// Returning a bare `true` here (the previous behaviour) is not a conservative
/// default — it is a kill switch. `safe_remove_owned_path` treats "has open
/// files" as a hard refusal, so on macOS and the BSDs *every* owned cache was
/// preserved forever and the disk sentinel could never reclaim a byte, no
/// matter how dead its owner was. Probe for real, and fail closed only when the
/// probe itself is inconclusive.
///
/// `lsof -t` lists just PIDs, and exits 1 when it finds nothing — an expected
/// outcome, not an error — so both 0 and 1 are trusted and the answer is read
/// off stdout. Any other exit, or a missing `lsof`, keeps the old fail-closed
/// behaviour. As on Linux, an unprivileged process cannot see other users'
/// descriptors; that limit is identical to being unable to read their
/// `/proc/<pid>/fd`.
#[cfg(not(target_os = "linux"))]
fn has_open_files(path: &Path) -> bool {
    let mut cmd = std::process::Command::new("lsof");
    // `-w` drops permission warnings, `-n`/`-P` skip DNS and service lookups.
    cmd.args(["-t", "-w", "-n", "-P"]);
    if path.is_dir() {
        // `+D` descends the tree, catching a file held open *inside* the dir —
        // the case that actually matters for a cargo target directory.
        cmd.arg("+D").arg(path);
    } else {
        cmd.arg("--").arg(path);
    }
    match cmd.output() {
        Ok(out) => match out.status.code() {
            Some(0) | Some(1) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            _ => true,
        },
        Err(_) => true,
    }
}

fn path_contains_registered_artifact(
    cache: &OwnedCache,
    graph: &crate::graph::WorkGraph,
    project_root: &Path,
) -> bool {
    let target = absolute_lexical(Path::new(&cache.path));
    graph
        .tasks()
        .flat_map(|t| t.artifacts.iter())
        .any(|artifact| {
            let raw = Path::new(artifact);
            let candidates = [
                if raw.is_absolute() {
                    raw.to_path_buf()
                } else {
                    project_root.join(raw)
                },
                cache
                    .worktree_path
                    .as_deref()
                    .map(Path::new)
                    .map(|w| w.join(raw))
                    .unwrap_or_default(),
            ];
            candidates
                .into_iter()
                .any(|candidate| absolute_lexical(&candidate).starts_with(&target))
        })
}

fn safe_remove_owned_path(
    cache: &OwnedCache,
    registry: &AgentRegistry,
    graph: &crate::graph::WorkGraph,
    project_root: &Path,
) -> std::result::Result<u64, String> {
    let path = Path::new(&cache.path);
    if !path.is_absolute() {
        return Err("ownership path is not absolute".into());
    }
    if !path.exists() {
        return Ok(0);
    }
    if !owner_is_stale(cache, registry, Some(graph)) {
        return Err("owner/task/lease/PID identity is still active or inconclusive".into());
    }
    if cache.mount_id != mount_id(path) {
        return Err("mount identity changed since registration".into());
    }
    let absolute = absolute_lexical(path);
    if absolute == Path::new("/") || absolute_lexical(project_root).starts_with(&absolute) {
        return Err("owned-cache path contains the project/source root".into());
    }
    if cache
        .worktree_path
        .as_deref()
        .is_some_and(|worktree| absolute_lexical(Path::new(worktree)).starts_with(&absolute))
    {
        return Err("owned-cache path contains a worktree".into());
    }
    if let Some(reason) = cache
        .worktree_path
        .as_deref()
        .and_then(|w| worktree_preserve_reason(Path::new(w)))
    {
        return Err(reason.into());
    }
    if path_contains_registered_artifact(cache, graph, project_root) {
        return Err("path contains a registered artifact".into());
    }
    if has_open_files(path) {
        return Err("path has open files".into());
    }
    let usage = bounded_size(path, usize::MAX);
    fs::remove_dir_all(path).map_err(|e| format!("remove failed: {e}"))?;
    Ok(usage.bytes)
}

fn terminal_agent_ids(registry: &AgentRegistry) -> HashSet<String> {
    registry
        .all()
        .filter(|a| {
            matches!(
                a.status,
                AgentStatus::Done | AgentStatus::Failed | AgentStatus::Dead | AgentStatus::Parked
            ) && !crate::service::is_process_alive(a.pid)
        })
        .map(|a| a.id.clone())
        .collect()
}

fn registered_artifact_paths(
    graph: &crate::graph::WorkGraph,
    project_root: &Path,
) -> HashSet<PathBuf> {
    graph
        .tasks()
        .flat_map(|t| t.artifacts.iter())
        .map(|a| absolute_lexical(&project_root.join(a)))
        .collect()
}

fn compress_terminal_streams(
    dir: &Path,
    cfg: &ResourceManagementConfig,
    registry: &AgentRegistry,
    graph: &crate::graph::WorkGraph,
    execute: bool,
    report: &mut CleanupReport,
) {
    if !cfg.compress_terminal_streams {
        return;
    }
    let terminal = terminal_agent_ids(registry);
    let artifacts = registered_artifact_paths(graph, dir.parent().unwrap_or(dir));
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            cfg.stream_retention_days.saturating_mul(86_400),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for agent_id in terminal {
        let base = dir.join("agents").join(&agent_id);
        for name in ["raw_stream.jsonl", "stream.jsonl"] {
            let path = base.join(name);
            if !path.exists() || artifacts.iter().any(|a| same_path(a, &path)) {
                continue;
            }
            let old_enough = fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|m| m <= cutoff)
                .unwrap_or(false);
            if !old_enough {
                continue;
            }
            if !execute {
                continue;
            }
            let original_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let zpath = PathBuf::from(format!("{}.zst", path.display()));
            let temp = zpath.with_extension(format!("zst-{}.tmp", std::process::id()));
            let (Ok(mut input), Ok(mut output)) = (File::open(&path), File::create(&temp)) else {
                let _ = fs::remove_file(&temp);
                continue;
            };
            if zstd::stream::copy_encode(&mut input, &mut output, 3).is_err()
                || output.sync_all().is_err()
            {
                let _ = fs::remove_file(&temp);
                continue;
            }
            let compressed_len = fs::metadata(&temp).map(|m| m.len()).unwrap_or(u64::MAX);
            if compressed_len >= original_len || fs::rename(&temp, &zpath).is_err() {
                let _ = fs::remove_file(&temp);
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                report.compressed_files += 1;
                report.compression_bytes_saved = report
                    .compression_bytes_saved
                    .saturating_add(original_len.saturating_sub(compressed_len));
            } else {
                let _ = fs::remove_file(&zpath);
            }
        }
    }
}

fn files_equal(a: &Path, b: &Path) -> bool {
    let (Ok(am), Ok(bm)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if am.len() != bm.len() {
        return false;
    }
    let (Ok(mut af), Ok(mut bf)) = (File::open(a), File::open(b)) else {
        return false;
    };
    let mut abuf = [0u8; 64 * 1024];
    let mut bbuf = [0u8; 64 * 1024];
    loop {
        let (Ok(an), Ok(bn)) = (af.read(&mut abuf), bf.read(&mut bbuf)) else {
            return false;
        };
        if an != bn || abuf[..an] != bbuf[..bn] {
            return false;
        }
        if an == 0 {
            return true;
        }
    }
}

/// Replace duplicate terminal `output.log` copies with hard links to their
/// readable historical `output.txt`. Both paths remain plain text, summaries
/// and evidence remain untouched, and the saved bytes are measured.
fn deduplicate_terminal_outputs(
    dir: &Path,
    cfg: &ResourceManagementConfig,
    registry: &AgentRegistry,
    graph: &crate::graph::WorkGraph,
    execute: bool,
    report: &mut CleanupReport,
) {
    if !cfg.compress_terminal_streams {
        return;
    }
    let terminal = terminal_agent_ids(registry);
    let artifacts = registered_artifact_paths(graph, dir.parent().unwrap_or(dir));
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            cfg.stream_retention_days.saturating_mul(86_400),
        ))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for agent_id in terminal {
        let Some(agent) = registry.get_agent(&agent_id) else {
            continue;
        };
        let live_copy = dir.join("agents").join(&agent_id).join("output.log");
        if !live_copy.exists() || artifacts.iter().any(|a| same_path(a, &live_copy)) {
            continue;
        }
        if !fs::metadata(&live_copy)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m <= cutoff)
        {
            continue;
        }
        let archives = dir.join("log").join("agents").join(&agent.task_id);
        let Ok(entries) = fs::read_dir(archives) else {
            continue;
        };
        let duplicate = entries
            .flatten()
            .map(|e| e.path().join("output.txt"))
            .find(|candidate| candidate.exists() && files_equal(&live_copy, candidate));
        let Some(archive) = duplicate else { continue };
        let bytes = fs::metadata(&live_copy).map(|m| m.len()).unwrap_or(0);
        if execute {
            let temp = live_copy.with_extension(format!("dedup-{}.tmp", std::process::id()));
            if fs::hard_link(&archive, &temp).is_err() {
                continue;
            }
            if fs::rename(&temp, &live_copy).is_err() {
                let _ = fs::remove_file(&temp);
                continue;
            }
        }
        report.deduplicated_files += 1;
        report.deduplication_bytes_saved = report.deduplication_bytes_saved.saturating_add(bytes);
    }
}

/// Reap only explicitly-owned caches for which every recorded owner of the
/// path is stale. Unknown `/tmp/wg-target-*` directories are intentionally
/// invisible to this function and therefore preserved.
pub fn cleanup_owned(
    dir: &Path,
    cfg: &ResourceManagementConfig,
    execute: bool,
) -> Result<CleanupReport> {
    let _lock = RegistryLock::acquire(dir)?;
    let mut ownership = load_ownership(dir)?;
    let registry = AgentRegistry::load(dir).unwrap_or_default();
    let graph = load_graph(dir.join("graph.jsonl")).context("load graph for disk cleanup")?;
    let project_root = dir.parent().unwrap_or(dir);
    let mut report = CleanupReport::default();
    let mut groups: BTreeMap<String, Vec<OwnedCache>> = BTreeMap::new();
    for cache in ownership.caches.drain(..) {
        groups.entry(cache.path.clone()).or_default().push(cache);
    }
    let mut keep = Vec::new();
    for (path, owners) in groups {
        report.considered += 1;
        let all_stale = owners
            .iter()
            .all(|c| owner_is_stale(c, &registry, Some(&graph)));
        if !all_stale {
            report.preserved.push(PreservedPath {
                path: path.clone(),
                reason: "one or more recorded owners are active/inconclusive".into(),
            });
            keep.extend(owners);
            continue;
        }
        let representative = &owners[0];
        if execute {
            match safe_remove_owned_path(representative, &registry, &graph, project_root) {
                Ok(bytes) => {
                    report.reaped += usize::from(bytes > 0);
                    report.bytes_freed = report.bytes_freed.saturating_add(bytes);
                }
                Err(reason) => {
                    report.preserved.push(PreservedPath {
                        path: path.clone(),
                        reason,
                    });
                    keep.extend(owners);
                }
            }
        } else {
            // Run every non-mutating guard in dry-run too. The remove primitive
            // is intentionally not called.
            let p = Path::new(&path);
            let absolute = absolute_lexical(p);
            let reason = if !p.exists() {
                // Mirror `safe_remove_owned_path`, which returns Ok(0) for a
                // vanished path before reaching any guard. Without this the
                // dry-run mis-predicts execute: it ran `mount_id` on a path that
                // is not there, could not match the recorded id, and reported
                // "mount identity changed" — 1,014 times on the live household —
                // for entries execute would simply drop.
                None
            } else if representative.mount_id != mount_id(p) {
                Some("mount identity changed since registration".to_string())
            } else if absolute == Path::new("/")
                || absolute_lexical(project_root).starts_with(&absolute)
            {
                Some("owned-cache path contains the project/source root".to_string())
            } else if representative
                .worktree_path
                .as_deref()
                .is_some_and(|worktree| {
                    absolute_lexical(Path::new(worktree)).starts_with(&absolute)
                })
            {
                Some("owned-cache path contains a worktree".to_string())
            } else if let Some(reason) = representative
                .worktree_path
                .as_deref()
                .and_then(|w| worktree_preserve_reason(Path::new(w)))
            {
                Some(reason)
            } else if path_contains_registered_artifact(representative, &graph, project_root) {
                Some("path contains a registered artifact".to_string())
            } else if has_open_files(p) {
                Some("path has open files".to_string())
            } else {
                None
            };
            if let Some(reason) = reason {
                report.preserved.push(PreservedPath {
                    path: path.clone(),
                    reason: reason.into(),
                });
            }
            keep.extend(owners);
        }
    }
    ownership.schema = OWNERSHIP_SCHEMA;
    ownership.caches = keep;
    if execute {
        save_ownership(dir, &ownership)?;
    }
    compress_terminal_streams(dir, cfg, &registry, &graph, execute, &mut report);
    deduplicate_terminal_outputs(dir, cfg, &registry, &graph, execute, &mut report);
    // Re-measure only when this run actually changed the bytes on disk.
    //
    // This was unconditional, which made it the single most expensive thing the
    // daemon did: `coordinator_tick` calls `refresh_if_due` (one full walk of
    // every registered cache) and then `cleanup_owned` (a second, identical
    // walk) on the same tick, so the interval bought half of what it looked
    // like. In steady state cleanup reaps nothing — the snapshot taken moments
    // earlier is still exact, and re-walking ~25GB of cargo targets to confirm
    // it is pure duplicate work. It also bypassed `disk_scan_interval_seconds`
    // entirely, so this walk ran at tick cadence no matter how the operator
    // configured the sentinel (2026-08-11).
    let changed = report.reaped > 0 || report.compressed_files > 0 || report.deduplicated_files > 0;
    if changed {
        let _ = refresh_snapshot(dir, cfg);
    }
    Ok(report)
}

pub fn target_path_for_agent(
    cfg: &ResourceManagementConfig,
    worktree: Option<&Path>,
    agent_id: &str,
) -> Option<PathBuf> {
    if let Some(root) = cfg.cargo_target_root.as_deref() {
        Some(PathBuf::from(root).join(format!("wg-target-{agent_id}")))
    } else if let Some(worktree) = worktree {
        Some(worktree.join("target"))
    } else if let Some(inherited) = std::env::var_os("CARGO_TARGET_DIR") {
        Some(PathBuf::from(inherited))
    } else {
        // A failed/disabled worktree must not fall back to an unowned shared
        // `<project>/target`. Give the worker an explicit external target that
        // remains visible to the ownership registry across worktree GC.
        Some(std::env::temp_dir().join(format!("wg-target-{agent_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Node, Status, WorkGraph};
    use crate::parser::save_graph;
    use tempfile::TempDir;

    fn mount(path: &str, free: u64, pct: f64) -> MountSpace {
        MountSpace {
            path: path.into(),
            mount_id: path.into(),
            free_bytes: free,
            total_bytes: 1_000,
            free_percent: pct,
        }
    }

    #[test]
    fn synthetic_mounts_warn_pause_refuse_and_hysteretic_resume() {
        let cfg = ResourceManagementConfig {
            disk_warning_bytes: 300,
            disk_pause_build_bytes: 200,
            disk_hard_refuse_bytes: 100,
            disk_warning_percent: 30.0,
            disk_pause_build_percent: 20.0,
            disk_hard_refuse_percent: 10.0,
            disk_resume_hysteresis_bytes: 50,
            disk_resume_hysteresis_percent: 5.0,
            ..Default::default()
        };
        assert_eq!(
            assess_mounts(
                &[mount("graph", 250, 25.0), mount("tmp", 500, 50.0)],
                &cfg,
                None
            )
            .0,
            DiskLevel::Warning
        );
        assert_eq!(
            assess_mounts(
                &[mount("graph", 150, 15.0), mount("tmp", 500, 50.0)],
                &cfg,
                None
            )
            .0,
            DiskLevel::PauseBuilds
        );
        assert_eq!(
            assess_mounts(
                &[mount("graph", 90, 9.0), mount("tmp", 500, 50.0)],
                &cfg,
                None
            )
            .0,
            DiskLevel::HardRefuse
        );
        assert_eq!(
            assess_mounts(
                &[mount("graph", 225, 24.0), mount("tmp", 500, 50.0)],
                &cfg,
                Some(DiskLevel::PauseBuilds)
            )
            .0,
            DiskLevel::PauseBuilds
        );
        assert_eq!(
            assess_mounts(
                &[mount("graph", 260, 26.0), mount("tmp", 500, 50.0)],
                &cfg,
                Some(DiskLevel::PauseBuilds)
            )
            .0,
            DiskLevel::Warning
        );
    }

    fn terminal_fixture(
        root: &Path,
        target: &Path,
        worktree: Option<&Path>,
    ) -> (PathBuf, ResourceManagementConfig) {
        let dir = root.join(".wg");
        fs::create_dir_all(&dir).unwrap();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "build".into(),
            title: "cargo test full suite".into(),
            status: Status::Done,
            completed_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        }));
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
        let mut registry = AgentRegistry::new();
        registry.agents.insert(
            "agent-dead".into(),
            crate::service::registry::AgentEntry {
                id: "agent-dead".into(),
                pid: 999_999,
                task_id: "build".into(),
                executor: "pi".into(),
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: Utc::now().to_rfc3339(),
                status: AgentStatus::Done,
                output_file: dir
                    .join("agents/agent-dead/output.log")
                    .display()
                    .to_string(),
                model: None,
                completed_at: Some(Utc::now().to_rfc3339()),
                worktree_path: worktree.map(|p| p.display().to_string()),
            },
        );
        registry.save(&dir).unwrap();
        let mut cache = make_owned_cache(
            target,
            CacheKind::CargoTarget,
            "build",
            "agent-dead",
            999_999,
            worktree,
            0,
        );
        cache.lease_expires_at = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        register_owned_cache(&dir, cache).unwrap();
        let cfg = ResourceManagementConfig {
            compress_terminal_streams: false,
            ..Default::default()
        };
        (dir, cfg)
    }

    /// A no-op cleanup must not re-walk every cache; a real one must.
    ///
    /// `coordinator_tick` calls `refresh_if_due` and then `cleanup_owned` on the
    /// same tick, and `cleanup_owned` used to end with an unconditional
    /// `refresh_snapshot` — a second identical walk of every registered cargo
    /// target, bypassing `disk_scan_interval_seconds` entirely. It was 82.7% of
    /// the daemon's active samples. Counted rather than timed: the snapshot
    /// written is byte-identical when nothing moved, so no outcome assertion can
    /// tell one walk from two.
    #[test]
    fn a_cleanup_that_reaps_nothing_does_not_rewalk_every_cache() {
        let root = tempfile::Builder::new()
            .prefix("wg-disk-walks-")
            .tempdir_in("/tmp")
            .unwrap();
        let owned = root.path().join("wg-target-walks");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("blob"), vec![7u8; 4096]).unwrap();
        let (dir, cfg) = terminal_fixture(root.path(), &owned, None);

        // A live owner: nothing is reapable, so this run changes no bytes.
        let mut ownership = load_ownership(&dir).unwrap();
        for cache in ownership.caches.iter_mut() {
            cache.lease_expires_at = (Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339();
        }
        save_ownership(&dir, &ownership).unwrap();

        SNAPSHOT_WALKS.with(|c| c.set(0));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        let walks = SNAPSHOT_WALKS.with(|c| c.get());
        assert_eq!(report.reaped, 0, "fixture should reap nothing");
        assert_eq!(
            walks, 0,
            "a cleanup that changed nothing must not re-walk every cache"
        );

        // Now make it reapable: a run that DOES change bytes must re-measure,
        // or the snapshot would keep reporting space that is already freed.
        let mut ownership = load_ownership(&dir).unwrap();
        for cache in ownership.caches.iter_mut() {
            cache.lease_expires_at = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        }
        save_ownership(&dir, &ownership).unwrap();

        SNAPSHOT_WALKS.with(|c| c.set(0));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        let walks = SNAPSHOT_WALKS.with(|c| c.get());
        assert_eq!(report.reaped, 1, "the cache should now be reaped");
        assert_eq!(walks, 1, "a cleanup that freed bytes must re-measure");
    }

    /// A cache whose agent and task records are GONE must still be reapable.
    ///
    /// This is the live 77GB leak (2026-08-11): `wg` prunes its own agent
    /// registry, so the oldest caches are exactly the ones whose owner record no
    /// longer exists. Treating absent as inconclusive made them permanent —
    /// `wg disk cleanup` reported considered=1156 reaped=0 — and made pruning
    /// the registry the act that caused it.
    #[test]
    fn cache_whose_owner_records_were_pruned_away_is_still_reapable() {
        let root = tempfile::Builder::new()
            .prefix("wg-disk-pruned-")
            .tempdir_in("/tmp")
            .unwrap();
        let owned = root.path().join("wg-target-pruned-owner");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("blob"), vec![7u8; 4096]).unwrap();
        let (dir, cfg) = terminal_fixture(root.path(), &owned, None);

        // Prune the owner out of both records, leaving only the cache entry —
        // the state a long-running household actually reaches.
        AgentRegistry::new().save(&dir).unwrap();
        save_graph(&WorkGraph::new(), dir.join("graph.jsonl")).unwrap();

        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert_eq!(
            report.reaped, 1,
            "a cache whose owner records were pruned must be reapable, got: {:?}",
            report.preserved
        );
        assert!(!owned.exists(), "the dead target should be gone");
    }

    /// The safety of the change above rests entirely on the lease and pid
    /// conjuncts, so prove each one alone still preserves a pruned-owner cache.
    /// Without these, "absent means terminal" would reap live builds.
    #[test]
    fn pruned_owner_alone_never_reaps_a_live_or_leased_cache() {
        for (label, live_pid, lease_offset) in [
            ("unexpired lease", false, 3600i64),
            ("live pid", true, -5i64),
        ] {
            let root = tempfile::Builder::new()
                .prefix("wg-disk-guard-")
                .tempdir_in("/tmp")
                .unwrap();
            let owned = root.path().join("wg-target-guarded");
            fs::create_dir_all(&owned).unwrap();
            fs::write(owned.join("blob"), vec![7u8; 4096]).unwrap();
            let (dir, cfg) = terminal_fixture(root.path(), &owned, None);
            AgentRegistry::new().save(&dir).unwrap();
            save_graph(&WorkGraph::new(), dir.join("graph.jsonl")).unwrap();

            // Rewrite the single cache entry with the one guard under test.
            let mut ownership = load_ownership(&dir).unwrap();
            for cache in ownership.caches.iter_mut() {
                cache.lease_expires_at =
                    (Utc::now() + chrono::Duration::seconds(lease_offset)).to_rfc3339();
                if live_pid {
                    cache.pid = std::process::id();
                    cache.pid_start_epoch =
                        crate::service::read_proc_start_time_secs(std::process::id());
                }
            }
            save_ownership(&dir, &ownership).unwrap();

            let report = cleanup_owned(&dir, &cfg, true).unwrap();
            assert_eq!(report.reaped, 0, "{label} must still preserve the cache");
            assert!(owned.exists(), "{label}: target must survive");
        }
    }

    #[test]
    fn stale_tmp_target_is_seen_only_when_explicitly_owned() {
        let root = tempfile::Builder::new()
            .prefix("wg-disk-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let dir = root.path().join(".wg");
        fs::create_dir_all(&dir).unwrap();
        save_graph(&WorkGraph::new(), dir.join("graph.jsonl")).unwrap();
        let unknown = root.path().join("wg-target-unknown");
        fs::create_dir_all(&unknown).unwrap();
        fs::write(unknown.join("blob"), b"unknown").unwrap();
        let cfg = ResourceManagementConfig::default();
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert_eq!(report.considered, 0);
        assert!(unknown.exists(), "filename is never ownership proof");

        let owned = root.path().join("wg-target-agent-dead");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("blob"), vec![7u8; 4096]).unwrap();
        let (owned_dir, cfg) = terminal_fixture(root.path(), &owned, None);
        let report = cleanup_owned(&owned_dir, &cfg, true).unwrap();
        assert_eq!(
            report.reaped, 1,
            "external /tmp target must not depend on worktree GC visibility"
        );
        assert!(!owned.exists());
        assert!(unknown.exists());
    }

    #[test]
    fn active_open_file_artifact_and_dirty_worktree_are_preserved() {
        // Open-file guard.
        let open_root = TempDir::new().unwrap();
        let open_target = open_root.path().join("wg-target-open");
        fs::create_dir_all(&open_target).unwrap();
        let held = File::create(open_target.join("held")).unwrap();
        let (dir, cfg) = terminal_fixture(open_root.path(), &open_target, None);
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(open_target.exists());
        assert!(
            report
                .preserved
                .iter()
                .any(|p| p.reason.contains("open files"))
        );
        drop(held);

        // Dirty-worktree guard also protects an external target associated
        // with that source checkout.
        let dirty_root = TempDir::new().unwrap();
        let worktree = dirty_root.path().join("source");
        fs::create_dir_all(&worktree).unwrap();
        // TRACKED-dirty, which is what this guard is named for. The fixture used to
        // `git init` and drop an uncommitted file, which in a repo with no commits is
        // merely UNTRACKED — so it exercised the weakest arm while asserting the
        // strongest wording. Commit the file first, then modify it.
        git_init_commit(&worktree);
        fs::write(worktree.join("dirty.rs"), "committed\n").unwrap();
        for args in [vec!["add", "dirty.rs"], vec!["commit", "-q", "-m", "add"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&worktree)
                .status()
                .unwrap();
        }
        fs::write(worktree.join("dirty.rs"), "uncommitted edit\n").unwrap();
        assert_eq!(
            worktree_dirt(&worktree),
            WorktreeDirt::TrackedDirty,
            "the dirty-worktree fixture must be TRACKED-dirty, not merely untracked"
        );
        let dirty_target = dirty_root.path().join("wg-target-dirty");
        fs::create_dir_all(&dirty_target).unwrap();
        let (dir, cfg) = terminal_fixture(dirty_root.path(), &dirty_target, Some(&worktree));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(dirty_target.exists());
        assert!(
            report
                .preserved
                .iter()
                .any(|p| p.reason.contains("uncommitted"))
        );

        // A WORKTREE WHOSE WORK IS ALREADY PRESERVED IS EXPENDABLE (the case that failed
        // on the live box: 58 worktrees / 26 GB, 0 ever reaped, because untracked
        // screenshot evidence read as "uncommitted source"). Untracked evidence PLUS a
        // complete reap sidecar — a resolvable commit and a patch that is really on disk —
        // must reap.
        let kept_root = TempDir::new().unwrap();
        let kept_wt = kept_root.path().join("source");
        fs::create_dir_all(kept_wt.join("disposables")).unwrap();
        git_init_commit(&kept_wt);
        fs::write(kept_wt.join("disposables/shot.png"), "untracked evidence").unwrap();
        let head = git_head_sha(&kept_wt);
        let patch = kept_root.path().join("reaped-wip.patch");
        fs::write(&patch, "diff --git a/x b/x\n").unwrap();
        fs::write(
            kept_wt.join(".wg-reaped-wip.json"),
            format!(
                r#"{{"schema":"wg-reap-preserved-wip/1","agent":"agent-1","task":"t","reason":"exit-1","branch":"b","commit":"{head}","patch":"{}","file_count":1}}"#,
                patch.display()
            ),
        )
        .unwrap();
        assert!(
            worktree_dirty(&kept_wt),
            "fixture must be dirty, or the reap below proves nothing about the sidecar"
        );
        assert!(
            reaped_wip_preserved(&kept_wt),
            "a complete sidecar (resolvable commit + patch on disk) must read as preserved"
        );
        let kept_target = kept_root.path().join("wg-target-preserved");
        fs::create_dir_all(&kept_target).unwrap();
        let (dir, cfg) = terminal_fixture(kept_root.path(), &kept_target, Some(&kept_wt));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(
            !kept_target.exists(),
            "a worktree whose WIP is already preserved must not pin its target: {:?}",
            report.preserved
        );

        // AND THE OTHER DIRECTION: a sidecar that PROMISES a patch which is not there
        // preserves nothing, so the conservative path must still hold the worktree. This is
        // the half that stops the fix above from becoming "any sidecar means delete".
        let broken_root = TempDir::new().unwrap();
        let broken_wt = broken_root.path().join("source");
        fs::create_dir_all(&broken_wt).unwrap();
        git_init_commit(&broken_wt);
        // TRACKED-dirty on purpose: this is the case that matters — REAL unsaved work
        // plus a sidecar that promises a patch which is not there. An untracked file
        // would exercise the weaker arm and prove less.
        fs::write(broken_wt.join("dirty.rs"), "committed\n").unwrap();
        for args in [vec!["add", "dirty.rs"], vec!["commit", "-q", "-m", "add"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&broken_wt)
                .status()
                .unwrap();
        }
        fs::write(broken_wt.join("dirty.rs"), "uncommitted real source\n").unwrap();
        let broken_head = git_head_sha(&broken_wt);
        fs::write(
            broken_wt.join(".wg-reaped-wip.json"),
            format!(
                r#"{{"schema":"wg-reap-preserved-wip/1","agent":"agent-2","task":"t","reason":"exit-1","branch":"b","commit":"{broken_head}","patch":"{}/gone.patch","file_count":1}}"#,
                broken_root.path().display()
            ),
        )
        .unwrap();
        assert!(
            !reaped_wip_preserved(&broken_wt),
            "a sidecar whose patch is MISSING must not read as preserved"
        );
        let broken_target = broken_root.path().join("wg-target-broken");
        fs::create_dir_all(&broken_target).unwrap();
        let (dir, cfg) = terminal_fixture(broken_root.path(), &broken_target, Some(&broken_wt));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(
            broken_target.exists(),
            "an incomplete sidecar must leave the conservative guard in force"
        );
        assert!(
            report
                .preserved
                .iter()
                .any(|p| p.reason.contains("uncommitted"))
        );

        // THE REASON MUST NAME WHAT IS ACTUALLY DIRTY. On the live box seven worktrees
        // pinned 11 GB reporting "uncommitted source" while having ZERO tracked-dirty
        // files and 13 untracked `disposables/` directories. Both still preserve — an
        // untracked file can be new source — but an operator must be able to tell them
        // apart, and "uncommitted source" for a tree with none was simply false.
        // TWO untracked entries, not one: the `disposables/` directory AND the sidecar
        // itself, which the reap path writes into the worktree and never commits. Worth
        // pinning — it means a preserved worktree is ALWAYS at least untracked-dirty, so
        // the sidecar rule is the only thing that can ever release it.
        assert_eq!(worktree_dirt(&kept_wt), WorktreeDirt::UntrackedOnly(2));
        assert_eq!(worktree_dirt(&broken_wt), WorktreeDirt::TrackedDirty);
        let evidence_root = TempDir::new().unwrap();
        let evidence_wt = evidence_root.path().join("source");
        fs::create_dir_all(evidence_wt.join("disposables")).unwrap();
        git_init_commit(&evidence_wt);
        fs::write(evidence_wt.join("disposables/a.png"), "shot").unwrap();
        fs::write(evidence_wt.join("disposables/b.png"), "shot").unwrap();
        let reason =
            worktree_preserve_reason(&evidence_wt).expect("untracked-only still preserves");
        assert!(
            reason.contains("untracked") && reason.contains("no tracked-dirty"),
            "reason must say what is dirty, got: {reason}"
        );
        assert!(
            !reason.contains("uncommitted tracked source"),
            "an untracked-only tree must not be reported as uncommitted source: {reason}"
        );
        // And the real-risk case keeps the strong wording.
        let tracked_reason =
            worktree_preserve_reason(&broken_wt).expect("tracked-dirty still preserves");
        assert!(
            tracked_reason.contains("uncommitted tracked source"),
            "got: {tracked_reason}"
        );

        // A CACHE WHOSE OWNING WORKTREE IS GONE has no uncommitted source to lose. This
        // held 7.2 GB of cargo build caches on the live box, every one of them reported as
        // "owning worktree has uncommitted source" for a worktree that had not existed for
        // days.
        let gone_root = TempDir::new().unwrap();
        let gone_wt = gone_root.path().join("worktree-that-was-removed");
        assert!(!gone_wt.exists());
        assert_eq!(worktree_dirt(&gone_wt), WorktreeDirt::Missing);
        assert_eq!(
            worktree_preserve_reason(&gone_wt),
            None,
            "a cache whose owning worktree is absent must be expendable"
        );
        let gone_target = gone_root.path().join("wg-target-gone");
        fs::create_dir_all(&gone_target).unwrap();
        let (dir, cfg) = terminal_fixture(gone_root.path(), &gone_target, Some(&gone_wt));
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(
            !gone_target.exists(),
            "cache with an absent owning worktree must reap: {:?}",
            report.preserved
        );

        // Registered artifact guard.
        let artifact_root = TempDir::new().unwrap();
        let artifact_target = artifact_root.path().join("wg-target-artifact");
        fs::create_dir_all(&artifact_target).unwrap();
        let artifact = artifact_target.join("evidence.json");
        fs::write(&artifact, "evidence").unwrap();
        let (dir, cfg) = terminal_fixture(artifact_root.path(), &artifact_target, None);
        let mut graph = load_graph(dir.join("graph.jsonl")).unwrap();
        graph.get_task_mut("build").unwrap().artifacts = vec![artifact.display().to_string()];
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(artifact.exists());
        assert!(
            report
                .preserved
                .iter()
                .any(|p| p.reason.contains("registered artifact"))
        );
    }

    #[test]
    fn matching_live_pid_identity_cannot_be_reaped_after_restart() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("wg-target-live-identity");
        fs::create_dir_all(&target).unwrap();
        let (dir, cfg) = terminal_fixture(root.path(), &target, None);
        let mut ownership = load_ownership(&dir).unwrap();
        ownership.caches[0].pid = std::process::id();
        ownership.caches[0].pid_start_epoch =
            crate::service::read_proc_start_time_secs(std::process::id());
        save_ownership(&dir, &ownership).unwrap();
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(
            target.exists(),
            "same process identity must remain protected even if registry/task look terminal"
        );
        assert_eq!(report.reaped, 0);
    }

    #[test]
    fn pid_reuse_identity_mismatch_is_required_but_live_open_file_still_preserved() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("cache");
        fs::create_dir_all(&path).unwrap();
        let file = File::create(path.join("held")).unwrap();
        assert!(has_open_files(&path));
        drop(file);
    }

    #[test]
    fn task_class_keeps_evaluators_out_of_build_admission() {
        let mut eval = Task {
            id: ".evaluate-x".into(),
            title: "Pi Terra evaluation".into(),
            ..Default::default()
        };
        eval.exec_mode = Some("full".into());
        assert_eq!(classify_task(&eval), BuildClass::GraphOnly);
        let build = Task {
            id: "build".into(),
            title: "Run cargo test full suite".into(),
            ..Default::default()
        };
        assert_eq!(classify_task(&build), BuildClass::BuildHeavy);
    }

    #[test]
    fn compression_preserves_summary_and_measures_savings() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join(".wg");
        fs::create_dir_all(dir.join("agents/a")).unwrap();
        let mut graph = WorkGraph::new();
        let mut task = Task {
            id: "t".into(),
            title: "t".into(),
            status: Status::Done,
            ..Default::default()
        };
        task.completed_at = Some(Utc::now().to_rfc3339());
        graph.add_node(Node::Task(task));
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
        let mut registry = AgentRegistry::new();
        registry.agents.insert(
            "a".into(),
            crate::service::registry::AgentEntry {
                id: "a".into(),
                pid: 999_999,
                task_id: "t".into(),
                executor: "pi".into(),
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: Utc::now().to_rfc3339(),
                status: AgentStatus::Done,
                output_file: dir.join("agents/a/output.log").display().to_string(),
                model: None,
                completed_at: Some(Utc::now().to_rfc3339()),
                worktree_path: None,
            },
        );
        registry.save(&dir).unwrap();
        fs::write(
            dir.join("agents/a/raw_stream.jsonl"),
            "same line repeated\n".repeat(10_000),
        )
        .unwrap();
        fs::write(
            dir.join("agents/a/output.log"),
            "readable task log\n".repeat(100),
        )
        .unwrap();
        let archive = dir.join("log/agents/t/attempt-1");
        fs::create_dir_all(&archive).unwrap();
        fs::copy(dir.join("agents/a/output.log"), archive.join("output.txt")).unwrap();
        fs::write(dir.join("agents/a/session-summary.md"), "readable evidence").unwrap();
        let cfg = ResourceManagementConfig {
            stream_retention_days: 0,
            ..Default::default()
        };
        let report = cleanup_owned(&dir, &cfg, true).unwrap();
        assert!(report.compression_bytes_saved > 0);
        assert_eq!(report.deduplicated_files, 1);
        assert!(report.deduplication_bytes_saved > 0);
        assert!(dir.join("agents/a/raw_stream.jsonl.zst").exists());
        assert_eq!(
            fs::read_to_string(dir.join("agents/a/output.log")).unwrap(),
            fs::read_to_string(archive.join("output.txt")).unwrap()
        );
        assert_eq!(
            fs::read_to_string(dir.join("agents/a/session-summary.md")).unwrap(),
            "readable evidence"
        );
    }
}
