//! Per-session handler lock — enforces at-most-one live handler per
//! chat session at a time.
//!
//! See `docs/design/sessions-as-identity.md` for the full model. This
//! module implements the lock contract:
//!
//!   * `acquire(dir, kind)`: O_EXCL create on `<dir>/.handler.pid`.
//!     If the file exists with a LIVE PID, refuses. If the file
//!     exists with a DEAD PID, recovers (removes + retakes).
//!   * `SessionLock::drop`: removes the file on clean exit.
//!   * `read_holder(dir)`: non-destructive read of who currently
//!     holds the lock (for takeover signalling).
//!   * `request_release(dir)`: writes `<dir>/.handler.release-requested`
//!     as a cooperative signal — the live handler should notice this
//!     at its next turn boundary and exit cleanly.
//!
//! The lock file format is deliberately small and human-readable so a
//! user running `cat .handler.pid` can understand what's going on:
//!
//! ```text
//! <pid>\n
//! <iso-8601-start-time>\n
//! <kind-label>\n
//! ```
//!
//! Stale detection uses `kill(pid, 0)` on Unix — returns 0 if the
//! process exists, error otherwise. On Windows we fall back to
//! treating the lock as always-fresh (Windows handler is a follow-up).

use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

const LOCK_FILENAME: &str = ".handler.pid";
const RELEASE_MARKER: &str = ".handler.release-requested";
const TUI_DRIVER_SENTINEL: &str = ".tui-driven";

/// What kind of handler owns the lock. Used for diagnostics — when
/// you see the lock file, you know what kind of thing is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerKind {
    /// Interactive `wg nex` at a terminal (rustyline-backed).
    InteractiveNex,
    /// Autonomous `wg nex` — task agent or background coordinator.
    AutonomousNex,
    /// Chat-tethered `wg nex` driving a chat session from inbox.
    ChatNex,
    /// TUI-owned PTY-backed handler.
    TuiPty,
    /// Other / adapter-dispatched handler (claude, codex, etc.).
    Adapter,
}

impl HandlerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::InteractiveNex => "interactive-nex",
            Self::AutonomousNex => "autonomous-nex",
            Self::ChatNex => "chat-nex",
            Self::TuiPty => "tui-pty",
            Self::Adapter => "adapter",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "interactive-nex" => Some(Self::InteractiveNex),
            "autonomous-nex" => Some(Self::AutonomousNex),
            "chat-nex" => Some(Self::ChatNex),
            "tui-pty" => Some(Self::TuiPty),
            "adapter" => Some(Self::Adapter),
            _ => None,
        }
    }
}

/// Snapshot of who currently holds the lock. Returned by
/// `read_holder`. Useful for takeover decisions and diagnostics.
#[derive(Clone, Debug)]
pub struct LockInfo {
    pub pid: u32,
    pub started_at: String,
    pub kind: Option<HandlerKind>,
    /// Whether the holder process is still alive (via `kill(pid, 0)`
    /// on Unix). If `false`, the lock is stale and safe to take.
    pub alive: bool,
}

/// Snapshot of the TUI process that has claimed this chat session.
#[derive(Clone, Debug)]
pub struct TuiDriverInfo {
    pub pid: u32,
    pub written_at: String,
    /// Whether the TUI process is still alive. Stale sentinels are
    /// ignored by readers, matching stale lock recovery semantics.
    pub alive: bool,
}

/// RAII lock handle. Drop removes the file (idempotent — safe even
/// if already removed). Call `release()` for an explicit early
/// release; otherwise the lock lives for the `SessionLock`'s scope.
pub struct SessionLock {
    path: PathBuf,
    /// Set to true once we've removed the file so Drop doesn't try
    /// a second time. Not a correctness issue (remove is idempotent)
    /// but saves a syscall and avoids log noise.
    released: bool,
}

impl SessionLock {
    /// Path where the lock file lives for a given chat session dir.
    pub fn lock_path(chat_dir: &Path) -> PathBuf {
        chat_dir.join(LOCK_FILENAME)
    }

    /// Path of the cooperative release marker.
    pub fn release_marker_path(chat_dir: &Path) -> PathBuf {
        chat_dir.join(RELEASE_MARKER)
    }

    /// Try to acquire the lock. Creates the chat dir if it doesn't
    /// exist (we own the lock so we own the dir initialisation).
    ///
    /// Returns `Err` if the lock is currently held by a live process.
    /// The error includes the holder's PID and kind so callers can
    /// decide whether to takeover.
    pub fn acquire(chat_dir: &Path, kind: HandlerKind) -> Result<Self> {
        std::fs::create_dir_all(chat_dir)
            .with_context(|| format!("create chat dir {:?}", chat_dir))?;
        let path = Self::lock_path(chat_dir);

        // O_EXCL create. Two racing processes: one wins, the other
        // gets EEXIST and falls into the stale-check branch.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o644);
        }
        let create_result = options.open(&path);

        match create_result {
            Ok(mut f) => {
                // We own the lock. Write our identity.
                let contents = format!(
                    "{}\n{}\n{}\n",
                    std::process::id(),
                    chrono::Utc::now().to_rfc3339(),
                    kind.label(),
                );
                f.write_all(contents.as_bytes())
                    .with_context(|| format!("write lock file {:?}", path))?;
                f.sync_all().context("fsync lock file")?;
                Ok(Self {
                    path,
                    released: false,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Someone else has it. Decide: recover (stale) or fail.
                match read_holder_at(&path)? {
                    Some(holder) if holder.alive && pid_reused_by_foreign(holder.pid) => {
                        // Alive by kill(0), but the PID was recycled to a
                        // foreign process (multi-day uptime). Treat as stale and
                        // recover instead of refusing forever — otherwise a
                        // respawn triggered by the fix-wedge reap would just hit
                        // this "held by live handler" error and give up.
                        eprintln!(
                            "[session-lock] recovering recycled lock (pid={} is a foreign process) at {:?}",
                            holder.pid, path
                        );
                        std::fs::remove_file(&path)
                            .with_context(|| format!("remove recycled lock {:?}", path))?;
                        Self::acquire(chat_dir, kind)
                    }
                    Some(holder) if holder.alive => Err(anyhow!(
                        "session lock held by live handler pid={} kind={} started={}",
                        holder.pid,
                        holder.kind.map(|k| k.label()).unwrap_or("unknown"),
                        holder.started_at
                    )),
                    Some(holder) => {
                        // Stale — clean up and retake.
                        eprintln!(
                            "[session-lock] recovering stale lock (dead pid={}, kind={}) at {:?}",
                            holder.pid,
                            holder.kind.map(|k| k.label()).unwrap_or("unknown"),
                            path
                        );
                        std::fs::remove_file(&path)
                            .with_context(|| format!("remove stale lock {:?}", path))?;
                        // Recurse once; if someone raced us here,
                        // this will either succeed or hit the live
                        // branch above.
                        Self::acquire(chat_dir, kind)
                    }
                    None => {
                        // File existed but couldn't parse — treat as
                        // stale. Same recovery as above.
                        eprintln!("[session-lock] recovering unparseable lock at {:?}", path);
                        std::fs::remove_file(&path)
                            .with_context(|| format!("remove corrupt lock {:?}", path))?;
                        Self::acquire(chat_dir, kind)
                    }
                }
            }
            Err(e) => Err(anyhow!("open lock file {:?}: {}", path, e)),
        }
    }

    /// Explicitly release the lock. Idempotent. The Drop impl also
    /// calls this, so manual release is only needed when you want
    /// release to happen before scope end (e.g., before spawning a
    /// successor handler).
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        if self.path.exists()
            && let Err(e) = std::fs::remove_file(&self.path)
        {
            eprintln!(
                "[session-lock] warning: failed to remove lock {:?}: {}",
                self.path, e
            );
        }
        self.released = true;
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Read the current lock holder, if any. Non-destructive — does not
/// touch the file, just reports what's there.
pub fn read_holder(chat_dir: &Path) -> Result<Option<LockInfo>> {
    read_holder_at(&SessionLock::lock_path(chat_dir))
}

fn read_holder_at(path: &Path) -> Result<Option<LockInfo>> {
    let mut f = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("open lock file {:?}: {}", path, e)),
    };
    let mut buf = String::new();
    f.read_to_string(&mut buf)
        .with_context(|| format!("read lock file {:?}", path))?;

    let mut lines = buf.lines();
    let pid_line = match lines.next() {
        Some(s) => s,
        None => return Ok(None),
    };
    let pid: u32 = match pid_line.trim().parse() {
        Ok(n) => n,
        Err(_) => return Ok(None),
    };
    let started_at = lines.next().unwrap_or("").to_string();
    let kind = lines.next().and_then(HandlerKind::parse);

    let alive = pid_is_alive(pid);
    Ok(Some(LockInfo {
        pid,
        started_at,
        kind,
        alive,
    }))
}

/// Ask the current lock holder (if any) to release the lock
/// cooperatively. Writes a marker file at
/// `<chat_dir>/.handler.release-requested`. The holder's conversation
/// loop checks for this marker at each turn boundary and exits
/// cleanly when it sees it.
///
/// The marker is **generation-aware**: it records the PID of the
/// handler the requester wants to release (read from the live lock
/// file). A freshly-(re)started handler with a different PID treats
/// such a marker as stale and ignores it — see `release_requested_for`.
/// Without this, a stale marker left by a previous handoff would make
/// every successor handler exit immediately at `turns=0` with
/// `reason=eof`, the exact failure this guards against.
///
/// Does nothing meaningful if there's no holder (records target pid 0,
/// which no live handler will match). Idempotent — writing the
/// marker twice is fine.
pub fn request_release(chat_dir: &Path) -> Result<()> {
    let target = read_holder(chat_dir).ok().flatten().map(|h| h.pid);
    request_release_for(chat_dir, target.unwrap_or(0))
}

/// Ask a *specific* handler generation (by PID) to release. The marker
/// records `target_pid` so only that handler acts on it; a successor
/// handler with a different PID ignores it as stale.
pub fn request_release_for(chat_dir: &Path, target_pid: u32) -> Result<()> {
    let marker = SessionLock::release_marker_path(chat_dir);
    std::fs::write(
        &marker,
        format!("{}\n{}\n", chrono::Utc::now().to_rfc3339(), target_pid),
    )
    .with_context(|| format!("write release marker {:?}", marker))?;
    Ok(())
}

/// The handler PID a pending release marker targets, if any.
///
/// Returns:
///   * `None` — no marker present.
///   * `Some(0)` — a marker with no recorded target (legacy format or
///     written when no handler held the lock). No live handler matches
///     pid 0, so such markers are treated as stale.
///   * `Some(pid)` — the handler generation the requester wants gone.
pub fn release_target(chat_dir: &Path) -> Option<u32> {
    let marker = SessionLock::release_marker_path(chat_dir);
    let contents = std::fs::read_to_string(&marker).ok()?;
    // Line 0 is the ISO timestamp; line 1 (if present) is the target pid.
    let pid = contents
        .lines()
        .nth(1)
        .and_then(|l| l.trim().parse::<u32>().ok())
        .unwrap_or(0);
    Some(pid)
}

/// True if a release has been requested for this session, regardless of
/// which handler generation it targets. Used by external observers
/// (TUI, `wg session`) that only care whether a request is outstanding.
/// Running handlers must use `release_requested_for` so a stale marker
/// from a prior generation does not kill them.
pub fn release_requested(chat_dir: &Path) -> bool {
    SessionLock::release_marker_path(chat_dir).exists()
}

/// True iff a release was requested for the handler generation running
/// as `my_pid`. A marker targeting a *different* (older) PID — or one
/// with no recorded target — is stale and returns `false`, so a
/// freshly (re)started handler is never killed by a leftover marker
/// from a previous handoff. This is the generation-aware check every
/// live handler should poll at its turn boundary / inbox read.
pub fn release_requested_for(chat_dir: &Path, my_pid: u32) -> bool {
    matches!(release_target(chat_dir), Some(pid) if pid == my_pid)
}

/// True if a marker exists but does NOT target `my_pid` — i.e. it is a
/// stale request left by a previous handler generation (or an
/// untargeted legacy marker). The current holder can safely clear such
/// markers since at most one handler holds the lock at a time.
pub fn stale_release_marker(chat_dir: &Path, my_pid: u32) -> bool {
    matches!(release_target(chat_dir), Some(pid) if pid != my_pid)
}

/// Clear any pending release marker. Called by a handler after it
/// observes the marker and acts on it (so a successor handler doesn't
/// see a stale marker and immediately exit).
pub fn clear_release_marker(chat_dir: &Path) {
    let marker = SessionLock::release_marker_path(chat_dir);
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
    }
}

/// Path of the TUI ownership sentinel for a chat session.
pub fn tui_driver_sentinel_path(chat_dir: &Path) -> PathBuf {
    chat_dir.join(TUI_DRIVER_SENTINEL)
}

/// Mark a chat session as currently driven by a live `wg tui` process.
pub fn write_tui_driver_sentinel(chat_dir: &Path, pid: u32) -> Result<()> {
    std::fs::create_dir_all(chat_dir).with_context(|| format!("create chat dir {:?}", chat_dir))?;
    let path = tui_driver_sentinel_path(chat_dir);
    let contents = format!("{}\n{}\n", pid, chrono::Utc::now().to_rfc3339());
    std::fs::write(&path, contents).with_context(|| format!("write TUI sentinel {:?}", path))?;
    Ok(())
}

/// Read the TUI ownership sentinel, if present.
pub fn read_tui_driver_sentinel(chat_dir: &Path) -> Result<Option<TuiDriverInfo>> {
    let path = tui_driver_sentinel_path(chat_dir);
    let mut f = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("open TUI sentinel {:?}: {}", path, e)),
    };
    let mut buf = String::new();
    f.read_to_string(&mut buf)
        .with_context(|| format!("read TUI sentinel {:?}", path))?;

    let mut lines = buf.lines();
    let pid_line = match lines.next() {
        Some(s) => s,
        None => return Ok(None),
    };
    let pid: u32 = match pid_line.trim().parse() {
        Ok(n) => n,
        Err(_) => return Ok(None),
    };
    let written_at = lines.next().unwrap_or("").to_string();
    let alive = pid_is_alive(pid);
    Ok(Some(TuiDriverInfo {
        pid,
        written_at,
        alive,
    }))
}

/// Clear the TUI ownership sentinel. Idempotent.
pub fn clear_tui_driver_sentinel(chat_dir: &Path) {
    let path = tui_driver_sentinel_path(chat_dir);
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// True when a live `wg tui` process has claimed this chat surface.
///
/// The sentinel is written before the asynchronous PTY/tmux spawn so the
/// daemon cannot race a duplicate handler into that short handoff window.
/// Vendor CLIs such as Pi and Codex run directly in tmux and never acquire
/// WG's `.handler.pid`, so requiring a lock here incorrectly erased their
/// valid ownership. The TUI now clears the sentinel on every pane-spawn error
/// and child-death path; PID identity checking still reaps dead/recycled
/// markers (including the historical wedge shape).
pub fn active_tui_driver_pid(chat_dir: &Path) -> Option<u32> {
    let tui = read_tui_driver_sentinel(chat_dir).ok().flatten()?;
    if !pid_is_live_ours(tui.pid) {
        clear_tui_driver_sentinel(chat_dir);
        return None;
    }
    Some(tui.pid)
}

/// True when a live `wg tui` process currently owns the chat surface.
pub fn tui_driver_sentinel_alive(chat_dir: &Path) -> bool {
    read_tui_driver_sentinel(chat_dir)
        .ok()
        .flatten()
        .is_some_and(|info| info.alive)
}

/// Wait for the lock at `chat_dir` to become free. Polls every
/// `poll_interval` up to `timeout`. Returns `Ok(())` once the lock
/// is gone, `Err` on timeout.
pub fn wait_for_release(chat_dir: &Path, timeout: Duration) -> Result<()> {
    let poll = Duration::from_millis(100);
    let start = std::time::Instant::now();
    loop {
        match read_holder(chat_dir)? {
            None => return Ok(()),
            Some(info) if !info.alive => return Ok(()),
            Some(_) => {
                if start.elapsed() >= timeout {
                    return Err(anyhow!(
                        "timed out waiting for lock release after {:?}",
                        timeout
                    ));
                }
                std::thread::sleep(poll);
            }
        }
    }
}

/// PID-is-alive check using `kill(pid, 0)`. A signal of 0 is the
/// "does this process exist and am I allowed to signal it?" query —
/// it does NOT actually deliver a signal.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is safe for any pid. It probes existence
    // and permission; it does not deliver a signal.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        true
    } else {
        // ESRCH = no such process. EPERM = process exists but we
        // can't signal it — still counts as alive.
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    // Conservative: assume alive on non-Unix. This makes stale
    // recovery a no-op on Windows, which is acceptable until we add
    // proper Windows support.
    true
}

/// Best-effort process *identity* string for `pid` (comm + cmdline), used only
/// to defeat PID reuse. Linux-only; returns `None` when identity can't be
/// established (no `/proc`, unreadable) so callers fall back to bare liveness.
#[cfg(target_os = "linux")]
fn pid_process_identity(pid: u32) -> Option<String> {
    // `/proc/<pid>/comm` is the (15-char-truncated) executable/thread name;
    // `/proc/<pid>/cmdline` is the full nul-separated argv. Both are readable
    // for any process by any user on a default Linux, so a recycled PID owned
    // by another user is still identifiable.
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "));
    match (comm, cmdline) {
        (None, None) => None,
        (c, cl) => Some(format!(
            "{} {}",
            c.unwrap_or_default().trim(),
            cl.unwrap_or_default().trim()
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn pid_process_identity(_pid: u32) -> Option<String> {
    None
}

/// True when `pid` is *positively* a foreign (OS-recycled) process — i.e. not
/// our own PID and not a `wg`/`nex`-family process.
///
/// Over multi-day daemon uptime a PID recorded in a stale `.tui-driven`
/// sentinel or `.handler.pid` lock can be recycled by the kernel to an
/// unrelated process. The bare `kill(pid, 0)` liveness probe then reports
/// "alive" forever, which wedged the coordinator supervisor into an endless
/// respawn-deferral loop (the `fix-wedge` incident). This narrows the notion
/// of "alive" enough to break out of that loop.
///
/// Fails **safe**: it returns `false` (not-foreign, preserve legacy behavior)
/// whenever identity cannot be established — no `/proc`, unreadable, or the
/// name plausibly belongs to our own process family. It only returns `true`
/// when it can affirmatively read a name that is clearly something else, so a
/// genuinely-live handler is never misread as stale.
fn pid_reused_by_foreign(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false; // ourselves / invalid — never foreign
    }
    let Some(target) = pid_process_identity(pid) else {
        return false; // undeterminable → preserve pre-fix behavior
    };
    let target_l = target.to_ascii_lowercase();
    // wg/nex family? Covers `wg`, `wg tui`, `wg nex --chat`, `wg <handler>`,
    // the standalone `nex`, and any path that mentions them. Biased toward
    // "ours" so we never reap a real handler.
    if target_l.contains("wg") || target_l.contains("nex") {
        return false;
    }
    // Same executable as us? In dev/test the driving process is the test
    // harness binary (not named `wg`); recognizing our own comm keeps that
    // path — and any oddly-named production binary — from being reaped.
    if let Some(mine) = pid_process_identity(std::process::id()) {
        let mine_comm = mine.split_whitespace().next().unwrap_or_default();
        let their_comm = target.split_whitespace().next().unwrap_or_default();
        if !mine_comm.is_empty() && mine_comm == their_comm {
            return false;
        }
    }
    true
}

/// Liveness that also defeats PID reuse: alive by `kill(pid, 0)` AND not a
/// positively-foreign recycled process. Use at stale-sentinel / stale-lock
/// decision points where a recycled PID would otherwise wedge a respawn loop.
fn pid_is_live_ours(pid: u32) -> bool {
    pid_is_alive(pid) && !pid_reused_by_foreign(pid)
}

// ── Boot-time session-lock reconciliation (zombie-handler reaping) ────────
//
// `wg service stop` leaves handler subprocesses running by design, so a handler
// from a previous daemon generation can keep holding a session lock. Two shapes
// wedged dispatch in the 2026-07-19 outage:
//   * a `claude-handler` from two days earlier squatted `chat-2`'s lock; every
//     new coordinator subprocess found the lock "held by a live handler" and
//     exited as a cooperative handoff with backoff — forever.
//   * four `wg`/`nex` processes ran from a `.wg-worktrees/agent-3024/target/
//     debug/wg` binary whose directory had since been deleted — orphans of a
//     removed worktree, still holding locks.
//
// `reconcile_session_locks` runs once on service start and reaps a lock whose
// holder is: dead, a recycled foreign PID, running a binary whose path no longer
// exists (deleted worktree), or older than a caller-supplied cutoff (a
// generation predating the current daemon boot). Live, current handlers are left
// untouched — the decision fails SAFE.

/// The best-effort on-disk path of the executable backing `pid`, or `None` when
/// it can't be determined (no `/proc`, permission, unsupported platform). Used
/// only to detect a handler whose binary was deleted out from under it.
#[cfg(target_os = "linux")]
pub fn process_exe_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    // `/proc/<pid>/exe` is a symlink to the running executable. For a deleted
    // binary the kernel appends " (deleted)" to the link target, so the returned
    // path will not `exists()` — exactly the signal we want.
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub fn process_exe_path(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    // macOS has no `/proc`; `ps -p <pid> -o comm=` prints the full executable
    // path of the process (e.g. `/Users/.../target/debug/wg`).
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_exe_path(_pid: u32) -> Option<PathBuf> {
    None
}

/// True when `pid`'s backing executable path can be determined AND no longer
/// exists on disk — the deleted-worktree-binary case. Fails SAFE: returns
/// `false` whenever the path is undeterminable (so a live handler on a platform
/// without exe introspection is never reaped for this reason). An absolute path
/// is required; a bare comm name (unresolved) is treated as undeterminable.
pub fn pid_binary_missing(pid: u32) -> bool {
    match process_exe_path(pid) {
        Some(path) if path.is_absolute() => !path.exists(),
        _ => false,
    }
}

/// Why a session lock was (or would be) reaped during reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapReason {
    /// Lock file couldn't be parsed — no usable holder identity.
    Unparseable,
    /// Holder PID is not alive (`kill(pid,0)` failed).
    DeadPid,
    /// Holder PID is alive but recycled to a foreign (non-wg/nex) process.
    RecycledForeign,
    /// Holder's executable path no longer exists (deleted worktree binary).
    BinaryMissing,
    /// Holder started before the reconcile cutoff — a stale generation squatter.
    StaleGeneration,
}

impl ReapReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unparseable => "unparseable lock",
            Self::DeadPid => "dead pid",
            Self::RecycledForeign => "recycled foreign pid",
            Self::BinaryMissing => "binary path no longer exists (deleted worktree)",
            Self::StaleGeneration => "handler generation predates daemon boot",
        }
    }

    /// Whether the still-live holder process should be signalled (SIGTERM) when
    /// reaped. We only kill orphans that are unambiguously OURS-gone-bad — a
    /// deleted-binary handler or a pre-boot generation squatter. A recycled
    /// foreign PID belongs to an unrelated process and must NEVER be signalled.
    pub fn should_kill_holder(self) -> bool {
        matches!(self, Self::BinaryMissing | Self::StaleGeneration)
    }
}

/// Pure reap decision from injected facts, so the policy is unit-testable
/// without live processes. Ordered by severity/confidence.
fn reap_reason_for(
    parseable: bool,
    alive: bool,
    foreign: bool,
    binary_missing: bool,
    predates_cutoff: bool,
) -> Option<ReapReason> {
    if !parseable {
        return Some(ReapReason::Unparseable);
    }
    if !alive {
        return Some(ReapReason::DeadPid);
    }
    if foreign {
        return Some(ReapReason::RecycledForeign);
    }
    if binary_missing {
        return Some(ReapReason::BinaryMissing);
    }
    if predates_cutoff {
        return Some(ReapReason::StaleGeneration);
    }
    None
}

/// Policy for `reconcile_session_locks`.
#[derive(Clone, Debug, Default)]
pub struct ReconcilePolicy {
    /// Reap a lock whose holder started strictly before this RFC3339-parseable
    /// instant — a generation predating the current daemon boot. `None` disables
    /// the age check (only dead/foreign/binary-missing locks are reaped).
    pub reap_before: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether to SIGTERM a still-live orphan holder (binary-missing / stale
    /// generation) before removing its lock file.
    pub kill_orphans: bool,
}

/// One reaped lock, for the boot log / report.
#[derive(Clone, Debug)]
pub struct ReapedLock {
    pub chat_dir: PathBuf,
    pub pid: u32,
    pub kind: Option<HandlerKind>,
    pub reason: ReapReason,
    /// Whether the live holder process was signalled during reaping.
    pub killed: bool,
}

/// Reconcile every `<chat_root>/*/.handler.pid` session lock against live
/// process facts, reaping zombie/stale/orphan holders. Returns the reaped
/// locks. Live, current handlers are left untouched.
///
/// Intended to run ONCE on service start (lock reconcile is engine-side). It
/// removes the stale lock file (and any paired release marker) so the next
/// handler generation can acquire cleanly, and — for orphans that are ours
/// (deleted-binary / pre-boot generation) — best-effort SIGTERMs the squatter
/// when `policy.kill_orphans` is set.
pub fn reconcile_session_locks(chat_root: &Path, policy: &ReconcilePolicy) -> Vec<ReapedLock> {
    let mut reaped = Vec::new();
    let entries = match std::fs::read_dir(chat_root) {
        Ok(e) => e,
        Err(_) => return reaped, // no chat root yet → nothing to reconcile
    };
    for entry in entries.flatten() {
        let chat_dir = entry.path();
        if !chat_dir.is_dir() {
            continue;
        }
        let lock_path = SessionLock::lock_path(&chat_dir);
        if !lock_path.exists() {
            continue;
        }

        let holder = read_holder_at(&lock_path).ok().flatten();
        let (parseable, pid, kind, alive, started_at) = match &holder {
            Some(h) => (true, h.pid, h.kind, h.alive, h.started_at.clone()),
            None => (false, 0, None, false, String::new()),
        };

        // Gather live-process facts only when there's a live PID to inspect.
        let foreign = alive && pid_reused_by_foreign(pid);
        let binary_missing = alive && !foreign && pid_binary_missing(pid);
        let predates_cutoff = alive
            && policy.reap_before.is_some_and(|cutoff| {
                chrono::DateTime::parse_from_rfc3339(&started_at)
                    .map(|dt| dt.with_timezone(&chrono::Utc) < cutoff)
                    .unwrap_or(false)
            });

        let Some(reason) =
            reap_reason_for(parseable, alive, foreign, binary_missing, predates_cutoff)
        else {
            continue; // live, current handler — leave it alone
        };

        // Best-effort SIGTERM for orphans that are unambiguously ours-gone-bad.
        let mut killed = false;
        if policy.kill_orphans && alive && reason.should_kill_holder() {
            killed = terminate_pid(pid);
        }

        // Remove the lock file and any paired cooperative release marker so the
        // successor handler starts from a clean slate.
        let _ = std::fs::remove_file(&lock_path);
        clear_release_marker(&chat_dir);

        eprintln!(
            "[session-lock] reconcile: reaped {} lock (pid={}, kind={}, reason={}{})",
            chat_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            pid,
            kind.map(|k| k.label()).unwrap_or("unknown"),
            reason.label(),
            if killed { ", holder SIGTERM'd" } else { "" },
        );

        reaped.push(ReapedLock {
            chat_dir,
            pid,
            kind,
            reason,
            killed,
        });
    }
    reaped
}

/// Best-effort SIGTERM to a still-live orphan holder. Never signals pid 0 or
/// ourselves. Returns whether the signal was delivered.
#[cfg(unix)]
fn terminate_pid(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    // SAFETY: kill with SIGTERM to a specific pid is safe; failure is ignored.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
}

#[cfg(not(unix))]
fn terminate_pid(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Spawn a foreign (`sleep`) child for the PID-reuse tests, then — on Linux —
    /// wait until its `/proc/<pid>/comm` reflects the post-`exec` image rather
    /// than the still-shared parent (test-harness) comm.
    ///
    /// A freshly `spawn()`ed child is `fork`ed from the test binary and, until
    /// its `exec` of `sleep` completes, transiently shares the parent's `comm`
    /// (`worksgood-<hash>` / the cargo test binary). During that window
    /// `pid_reused_by_foreign`'s "same executable as us" guard
    /// (src/session_lock.rs:578-584) sees `their_comm == mine_comm` and returns
    /// `false` — so asserting foreignness during the window flakes on loaded CI
    /// runners where the `exec` is delayed (observed on graphwork/wg CI runs
    /// 29090834963, 29090740682, 29102594830). Production PID reuse involves
    /// stable, long-lived recycled processes and is unaffected; this raciness is
    /// purely a test-harness artifact. Waiting for `exec` to land removes the
    /// race without weakening the real identity/liveness checks.
    fn spawn_foreign_child() -> std::process::Child {
        // `sleep 120` (not 30): the child must outlive the exec-wait below plus
        // the rest of the test even on a badly starved runner. Every caller
        // kills+waits it, so the long duration never lingers.
        let child = std::process::Command::new("sleep")
            .arg("120")
            .spawn()
            .expect("spawn sleep");
        #[cfg(target_os = "linux")]
        {
            let pid = child.id();
            let comm_of = |p: u32| {
                pid_process_identity(p)
                    .and_then(|id| id.split_whitespace().next().map(str::to_owned))
            };
            let mine_comm = comm_of(std::process::id()).unwrap_or_default();
            // Wait until the child's comm is readable and differs from ours,
            // i.e. `exec` of `sleep` has replaced the still-shared parent image.
            // This is bounded by a *wall-clock* deadline, not an iteration count:
            // the earlier 2000×1ms budget could elapse in real time before a
            // forked-but-unscheduled child ever ran its `exec` on a saturated CI
            // runner, so the helper returned a child whose comm still shadowed
            // the harness's — `pid_reused_by_foreign`'s "same executable as us"
            // guard then read `false` and the foreignness assert flaked
            // (graphwork/wg CI runs 29137074236, 29162578216). A 30s deadline is
            // orders of magnitude longer than a real `exec` and well inside the
            // child's 120s lifetime; exceeding it is a genuine environment fault,
            // surfaced as a clear panic rather than a confusing downstream assert.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                match comm_of(pid) {
                    Some(their_comm)
                        if !their_comm.is_empty()
                            && !mine_comm.is_empty()
                            && their_comm != mine_comm =>
                    {
                        break;
                    }
                    _ => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "foreign child (pid {pid}) never completed `exec` within 30s: \
                             comm still reads {:?} vs ours {mine_comm:?}",
                            comm_of(pid)
                        );
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
            }
        }
        child
    }

    #[test]
    fn acquire_creates_lock_file_with_pid() {
        let dir = tempdir().unwrap();
        let lock = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex).unwrap();
        let p = SessionLock::lock_path(dir.path());
        assert!(p.exists());
        let contents = std::fs::read_to_string(&p).unwrap();
        assert!(contents.contains(&format!("{}", std::process::id())));
        assert!(contents.contains("interactive-nex"));
        drop(lock);
        assert!(!p.exists(), "Drop should remove the lock file");
    }

    #[test]
    fn second_acquire_fails_while_first_held() {
        let dir = tempdir().unwrap();
        let _first = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex).unwrap();
        let second = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex);
        assert!(second.is_err(), "second acquire must fail while first held");
        let err = second.err().unwrap().to_string();
        assert!(
            err.contains("pid="),
            "error must name the holder pid: {}",
            err
        );
    }

    #[test]
    fn stale_lock_recovers() {
        let dir = tempdir().unwrap();
        // Write a lock file pointing at a dead PID.
        let p = SessionLock::lock_path(dir.path());
        std::fs::write(&p, "999999\n2020-01-01T00:00:00Z\nchat-nex\n").unwrap();
        // New acquire should detect dead PID and recover.
        let lock = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
        let contents = std::fs::read_to_string(&p).unwrap();
        // Now owned by our process.
        assert!(contents.contains(&format!("{}", std::process::id())));
        drop(lock);
    }

    #[test]
    fn read_holder_returns_live_flag() {
        let dir = tempdir().unwrap();
        let _lock = SessionLock::acquire(dir.path(), HandlerKind::TuiPty).unwrap();
        let info = read_holder(dir.path()).unwrap().unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.kind, Some(HandlerKind::TuiPty));
        assert!(info.alive);
    }

    #[test]
    fn read_holder_none_when_no_lock() {
        let dir = tempdir().unwrap();
        assert!(read_holder(dir.path()).unwrap().is_none());
    }

    #[test]
    fn release_marker_round_trip() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        assert!(!release_requested(dir.path()));
        request_release(dir.path()).unwrap();
        assert!(release_requested(dir.path()));
        clear_release_marker(dir.path());
        assert!(!release_requested(dir.path()));
    }

    #[test]
    fn request_release_embeds_live_holder_pid() {
        let dir = tempdir().unwrap();
        let _lock = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
        request_release(dir.path()).unwrap();
        // The marker should target the live holder (this process).
        assert_eq!(release_target(dir.path()), Some(std::process::id()));
        assert!(release_requested_for(dir.path(), std::process::id()));
    }

    #[test]
    fn release_request_targets_only_its_generation() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        // A request explicitly aimed at handler generation pid=4242.
        request_release_for(dir.path(), 4242).unwrap();

        // The targeted generation must observe it.
        assert!(release_requested_for(dir.path(), 4242));
        // A DIFFERENT (e.g. freshly respawned) generation must NOT — this is
        // the core guard against a stale marker killing a successor handler
        // at turns=0 with reason=eof.
        assert!(!release_requested_for(dir.path(), 9999));
        // ...and it is detectable as stale from the successor's view.
        assert!(stale_release_marker(dir.path(), 9999));
        assert!(!stale_release_marker(dir.path(), 4242));
    }

    #[test]
    fn legacy_untargeted_marker_is_treated_as_stale() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        // Simulate a marker written by the pre-generation-aware code:
        // a single timestamp line, no target pid.
        std::fs::write(
            SessionLock::release_marker_path(dir.path()),
            "2020-01-01T00:00:00Z\n",
        )
        .unwrap();
        assert_eq!(release_target(dir.path()), Some(0));
        // No live handler runs as pid 0, so no generation honors it.
        assert!(!release_requested_for(dir.path(), 1234));
        assert!(stale_release_marker(dir.path(), 1234));
    }

    #[test]
    fn no_marker_has_no_target() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        assert_eq!(release_target(dir.path()), None);
        assert!(!release_requested_for(dir.path(), 1));
        assert!(!stale_release_marker(dir.path(), 1));
    }

    #[test]
    fn explicit_release_allows_reacquire() {
        let dir = tempdir().unwrap();
        let mut lock = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
        lock.release();
        // New lock can be taken.
        let _new = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
    }

    #[test]
    fn corrupt_lock_recovers() {
        let dir = tempdir().unwrap();
        let p = SessionLock::lock_path(dir.path());
        std::fs::write(&p, "not a pid\ngarbage\n").unwrap();
        let lock = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
        drop(lock);
    }

    #[test]
    fn wait_for_release_succeeds_when_free() {
        let dir = tempdir().unwrap();
        // No lock held — should return immediately.
        wait_for_release(dir.path(), Duration::from_millis(50)).unwrap();
    }

    #[test]
    fn wait_for_release_times_out_while_held() {
        let dir = tempdir().unwrap();
        let _lock = SessionLock::acquire(dir.path(), HandlerKind::ChatNex).unwrap();
        let r = wait_for_release(dir.path(), Duration::from_millis(100));
        assert!(r.is_err());
    }

    #[test]
    fn tui_driver_sentinel_round_trip() {
        let dir = tempdir().unwrap();
        write_tui_driver_sentinel(dir.path(), std::process::id()).unwrap();

        let info = read_tui_driver_sentinel(dir.path()).unwrap().unwrap();
        assert_eq!(info.pid, std::process::id());
        assert!(info.alive);
        assert!(tui_driver_sentinel_alive(dir.path()));

        clear_tui_driver_sentinel(dir.path());
        assert!(read_tui_driver_sentinel(dir.path()).unwrap().is_none());
        assert!(!tui_driver_sentinel_alive(dir.path()));
    }

    #[test]
    fn active_tui_driver_covers_vendor_pty_without_handler_lock() {
        let dir = tempdir().unwrap();
        write_tui_driver_sentinel(dir.path(), std::process::id()).unwrap();

        assert_eq!(active_tui_driver_pid(dir.path()), Some(std::process::id()));
        assert!(
            read_tui_driver_sentinel(dir.path()).unwrap().is_some(),
            "a live TUI-owned Pi/Codex pane has no WG handler lock; its claim must survive"
        );

        let _lock = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex).unwrap();
        assert_eq!(active_tui_driver_pid(dir.path()), Some(std::process::id()));
    }

    #[test]
    fn stale_tui_driver_sentinel_is_not_alive() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            tui_driver_sentinel_path(dir.path()),
            "999999\n2020-01-01T00:00:00Z\n",
        )
        .unwrap();

        let info = read_tui_driver_sentinel(dir.path()).unwrap().unwrap();
        assert_eq!(info.pid, 999999);
        assert!(!info.alive);
        assert!(!tui_driver_sentinel_alive(dir.path()));
    }

    #[test]
    fn foreign_pid_identity_defeats_pid_reuse() {
        // Our own PID and pid 0 are never foreign.
        assert!(!pid_reused_by_foreign(0));
        assert!(!pid_reused_by_foreign(std::process::id()));

        // A live, unrelated (non-wg) process IS foreign — this is the
        // multi-day PID-reuse shape that used to wedge the supervisor.
        // `spawn_foreign_child` waits for the child's `exec` to land so its
        // `/proc` comm no longer shadows the test binary's (see helper doc).
        let mut child = spawn_foreign_child();
        let sleep_pid = child.id();
        assert!(pid_is_alive(sleep_pid), "sleep child should be alive");
        if cfg!(target_os = "linux") {
            assert!(
                pid_reused_by_foreign(sleep_pid),
                "a live `sleep` process must be recognized as a foreign (recycled) PID"
            );
            assert!(
                !pid_is_live_ours(sleep_pid),
                "foreign PID must not count as a live handler of ours"
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn active_tui_driver_reaps_recycled_sentinel_pid() {
        // Sentinel points at a LIVE but foreign PID (`sleep`), and a genuine
        // live handler lock is held. Pre-fix this returned Some(foreign_pid)
        // forever (the wedge); now the recycled sentinel is reaped.
        let dir = tempdir().unwrap();
        let mut child = spawn_foreign_child();
        let sleep_pid = child.id();

        let _lock = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex).unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            tui_driver_sentinel_path(dir.path()),
            format!("{sleep_pid}\n2020-01-01T00:00:00Z\n"),
        )
        .unwrap();

        if cfg!(target_os = "linux") {
            assert_eq!(
                active_tui_driver_pid(dir.path()),
                None,
                "a sentinel whose PID was recycled to a foreign process must be reaped, not deferred"
            );
            assert!(
                read_tui_driver_sentinel(dir.path()).unwrap().is_none(),
                "recycled sentinel should be cleared"
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    // ── Fix 2: boot-time session-lock reconciliation ────────────────────

    #[test]
    fn reap_reason_ordering_and_safe_default() {
        // A live, current, non-foreign handler with a good binary → NOT reaped.
        assert_eq!(reap_reason_for(true, true, false, false, false), None);
        // Unparseable wins even if "alive".
        assert_eq!(
            reap_reason_for(false, true, true, true, true),
            Some(ReapReason::Unparseable)
        );
        // Dead pid.
        assert_eq!(
            reap_reason_for(true, false, false, false, false),
            Some(ReapReason::DeadPid)
        );
        // Foreign recycled pid (before binary/age checks).
        assert_eq!(
            reap_reason_for(true, true, true, true, true),
            Some(ReapReason::RecycledForeign)
        );
        // Deleted-worktree binary.
        assert_eq!(
            reap_reason_for(true, true, false, true, false),
            Some(ReapReason::BinaryMissing)
        );
        // Pre-boot generation squatter.
        assert_eq!(
            reap_reason_for(true, true, false, false, true),
            Some(ReapReason::StaleGeneration)
        );
    }

    #[test]
    fn reap_reason_kill_policy_never_kills_foreign() {
        assert!(!ReapReason::RecycledForeign.should_kill_holder());
        assert!(!ReapReason::DeadPid.should_kill_holder());
        assert!(!ReapReason::Unparseable.should_kill_holder());
        assert!(ReapReason::BinaryMissing.should_kill_holder());
        assert!(ReapReason::StaleGeneration.should_kill_holder());
    }

    #[test]
    fn reconcile_reaps_dead_and_unparseable_but_spares_live_current() {
        let root = tempdir().unwrap();
        // Session A: a dead-pid lock (zombie generation, no live process).
        let a = root.path().join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(
            SessionLock::lock_path(&a),
            "999999\n2020-01-01T00:00:00Z\nadapter\n",
        )
        .unwrap();
        // Session B: an unparseable lock.
        let b = root.path().join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(SessionLock::lock_path(&b), "garbage\n").unwrap();
        // Session C: a live, current handler (this process) — must be spared.
        let c = root.path().join("cccccccc-cccc-cccc-cccc-cccccccccccc");
        let _live = SessionLock::acquire(&c, HandlerKind::ChatNex).unwrap();

        let policy = ReconcilePolicy {
            reap_before: None,
            kill_orphans: false,
        };
        let reaped = reconcile_session_locks(root.path(), &policy);

        // A and B reaped; C left intact.
        let reasons: std::collections::HashMap<_, _> = reaped
            .iter()
            .map(|r| (r.chat_dir.clone(), r.reason))
            .collect();
        assert_eq!(reasons.get(&a), Some(&ReapReason::DeadPid));
        assert_eq!(reasons.get(&b), Some(&ReapReason::Unparseable));
        assert!(!reasons.contains_key(&c), "live current handler must be spared");
        assert!(!SessionLock::lock_path(&a).exists(), "dead lock removed");
        assert!(!SessionLock::lock_path(&b).exists(), "corrupt lock removed");
        assert!(SessionLock::lock_path(&c).exists(), "live lock preserved");
    }

    #[test]
    fn reconcile_reaps_pre_boot_generation_squatter() {
        // A live handler (this process) whose lock records a start time BEFORE
        // the daemon-boot cutoff is a stale generation and must be reaped — the
        // 2-day claude-handler squatting the lock. We reap without killing so
        // the test process is unharmed.
        let root = tempdir().unwrap();
        let dir = root.path().join("dddddddd-dddd-dddd-dddd-dddddddddddd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            SessionLock::lock_path(&dir),
            format!("{}\n2020-01-01T00:00:00Z\nadapter\n", std::process::id()),
        )
        .unwrap();

        let policy = ReconcilePolicy {
            reap_before: Some(chrono::Utc::now()), // boot is "now"; lock is from 2020
            kill_orphans: false,
        };
        let reaped = reconcile_session_locks(root.path(), &policy);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].reason, ReapReason::StaleGeneration);
        assert!(!reaped[0].killed, "kill_orphans=false → holder not signalled");
        assert!(!SessionLock::lock_path(&dir).exists());
    }

    #[test]
    fn reconcile_spares_recent_live_generation() {
        // Same live process, but the lock's start time is AFTER the cutoff — a
        // current-generation handler that must NOT be reaped.
        let root = tempdir().unwrap();
        let dir = root.path().join("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee");
        let _live = SessionLock::acquire(&dir, HandlerKind::ChatNex).unwrap();
        let policy = ReconcilePolicy {
            reap_before: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            kill_orphans: true,
        };
        let reaped = reconcile_session_locks(root.path(), &policy);
        assert!(reaped.is_empty(), "a handler started after the cutoff is current");
        assert!(SessionLock::lock_path(&dir).exists());
    }

    #[test]
    fn pid_binary_missing_is_false_for_live_self() {
        // Our own process has a real, existing executable → not missing.
        assert!(!pid_binary_missing(std::process::id()));
    }

    #[test]
    fn acquire_recovers_lock_held_by_recycled_foreign_pid() {
        // A lock file whose PID is alive but foreign (recycled) must be
        // recovered on acquire, not treated as a live handler forever.
        let dir = tempdir().unwrap();
        let mut child = spawn_foreign_child();
        let sleep_pid = child.id();

        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            SessionLock::lock_path(dir.path()),
            format!("{sleep_pid}\n2020-01-01T00:00:00Z\nchat-nex\n"),
        )
        .unwrap();

        let acquired = SessionLock::acquire(dir.path(), HandlerKind::InteractiveNex);
        if cfg!(target_os = "linux") {
            assert!(
                acquired.is_ok(),
                "acquire must recover a lock held by a recycled foreign PID: {:?}",
                acquired.err()
            );
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}
