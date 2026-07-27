//! THE RUST TWIN of the gateway's cross-process project lock
//! (`claw3d-bridge/src/projectLock.mjs`). The normative text is
//! **`docs/42-cross-process-lock-protocol.md`** in the family repo; this module
//! implements §§1–6 of it, and the tests at the bottom walk the §8 conformance
//! checklist item by item.
//!
//! WHY THIS EXISTS. The gateway serialises every protected week mutation — the
//! shopping overlay, carry-forward, plan/meal writes, parked dinner suggestions —
//! through `<projectRoot>/.casa/locks/week-mutation.lock`. The engine did not
//! participate: `wg`'s week-start path wrote the weekly plan file directly and
//! took no lock, so a gateway mutation and an engine week-start could still
//! interleave and lose an update. **A protocol only one of two writers speaks is
//! not a protocol** (docs/42 §9, audit P0 #4).
//!
//! WHY NOT `feed_lock.rs`. That module is this protocol with a fixed pathname
//! (`.conversation.lock`) and no rank, written one slice earlier. As it stands it
//! is missing the two things the exact-tree audit of the Node side rejected and
//! docs/42 then made normative: the §4a ownership pre-check, and treating a
//! refused `fsync` as a refused lock (it has no directory `fsync` at all).
//! Adopting it for week mutations would have imported both defects. Collapsing
//! the two is docs/42 §10 / task `unify-feed-and-project-lock`; until then, this
//! is the module a week mutation takes and it carries the fixes.
//!
//! THE PROTOCOL, in the document's own numbering:
//!
//! §1 LOCK PATH — `<projectRoot>/.casa/locks/<name>.lock`, the root made absolute
//!    exactly the way the Node twin's `path.resolve` makes it absolute
//!    (LEXICALLY — see [`lock_path_for`]; resolving symlinks here and not there
//!    would give the two implementations two different strings for one file).
//!    The identity of a lock is the pair (root, name), never the name alone.
//!    Ranks are strict: a holder may take only a lock of STRICTLY GREATER rank,
//!    and an inversion is an immediate loud error at the acquire that would
//!    otherwise have hung.
//!
//! §2 ACQUIRE — stage, verify, then publish with `link(2)`. The owner record is
//!    one line of JSON plus a trailing newline, byte-identical to the Node twin's
//!    `JSON.stringify({v,token,pid,host,acquiredMs}) + "\n"`, carrying a 32-hex
//!    `token` of 16 crypto-random bytes. `pid`/`host` are EVIDENCE for a report,
//!    never identity. We write into a PRIVATE staging file, `write_all` (the
//!    short-write loop), `fsync`, `fstat` to capture the `(dev, ino)` we are
//!    about to publish, close, read back and compare byte for byte, and only then
//!    `link(staging, lockPath)` — as exclusive as `O_EXCL`, but the visible
//!    transition goes straight from "no lock" to "a whole, parseable lock".
//!    Finally we `fsync` the CONTAINING DIRECTORY.
//!
//!    **A refused `fsync` is a refused lock.** `EIO` is the storage saying the
//!    bytes are not on the device; returning `locked: true` anyway authorises a
//!    week mutation on a record whose owner may never have existed after a power
//!    cut. Both fsyncs are hard requirements, and the directory one is the only
//!    failure path in acquisition allowed to touch the lock path — and only to
//!    remove the entry it has just proved, by `(dev, ino)`, is its own.
//!
//! §3 FAIL CLOSED, ALWAYS. Not acquired ⇒ the caller's work does NOT run. Not for
//!    a read-only mount, not for an `ENOTDIR` root, not for a full disk.
//!
//! §4 REMOVAL IS ALWAYS DETACH-THEN-DECIDE. `read` → compare → `unlink` touches a
//!    PATHNAME, not the inode that was judged. So: `rename` to a private
//!    quarantine (one atomic syscall), inspect ONLY the moved inode, unlink it if
//!    it carries our token, and otherwise `link` it straight back (and leave it
//!    quarantined as evidence if the path was retaken).
//!
//! §4a OWNERSHIP IS ESTABLISHED BEFORE THE PATHNAME MOVES. Detach-then-decide is
//!    safe for the inode it moves and NOT safe for the pathname it frees: between
//!    the rename and the restoring link the authority pathname is EMPTY, and an
//!    audit drove a third process into that gap and came out with two holders. So
//!    release `stat`s the lock path first and compares it against the `(dev, ino)`
//!    captured at §2 — the kernel's own answer to "which lock is mine". A
//!    different inode means a successor owns it: report `stolen` and touch
//!    NOTHING. Both halves are required; the pre-check alone is a
//!    compare-then-unlink race, and detach-then-decide alone is the successor gap.
//!
//! §5 NO AUTOMATIC RECLAIM — NOT EVEN OF A PROVABLY DEAD OWNER. Acquisition
//!    classifies and REPORTS; it never removes. `staleMs` exists only to tell
//!    `held` from `stale-unrecovered` in that report; **no code path breaks a lock
//!    because of its age**. `kill(pid, 0)` with `ESRCH` — and only `ESRCH` — as
//!    the verdict of death. The cure for a wedged lock is a human removing the
//!    file, and the report names it.
//!
//! §6 RE-ENTRANCY is in-process only and keyed on the RESOLVED LOCK PATH, so
//!    house B's callback is never handed a lock taken for house A. Here it is
//!    keyed per THREAD as well: the Node twin's "process" is one call stack, while
//!    this binary runs listeners on many threads, and a process-global map would
//!    hand a second thread a lock the first one holds instead of making it wait on
//!    the file like any other writer.
//!
//! The lock is advisory: it binds only writers that take it.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// Guards every protected mutation of the week (docs/42 §1).
pub const WEEK_MUTATION: &str = "week-mutation";
/// Guards the feed + receipts rotation transaction (docs/42 §1). Present so the
/// rank table is the whole table; the feed writers still take `feed_lock`.
pub const FEED_ROTATION: &str = "feed-rotation";

/// The rank table, byte-for-byte the Node twin's `LOCK_RANKS`. An unknown name is
/// a loud error rather than an unranked lock that silently opts out of ordering.
pub fn rank_of(name: &str) -> Option<u32> {
    match name {
        WEEK_MUTATION => Some(10),
        FEED_ROTATION => Some(20),
        _ => None,
    }
}

/// How long a taker waits for a contended lock before failing closed (the Node
/// twin's `DEFAULT_WAIT_MS`).
pub const DEFAULT_WAIT_MS: u64 = 5000;
/// How old an attributable lock whose owner is provably dead must be before the
/// REPORT calls it stale rather than held (the twin's `DEFAULT_STALE_MS`).
/// **Nothing acts on this** — see §5.
pub const DEFAULT_STALE_MS: u64 = 15000;
/// How many times a release retries a TRANSIENT failure before handing the
/// problem back with ownership still retained (§4).
const RELEASE_ATTEMPTS: u32 = 5;
/// The twin's ~4 ms retry sleep.
const SLEEP_MS: u64 = 4;

// ─────────────────────────────────────────────────────────────────────────────
// Typed refusals (§3). Every variant means the caller's work did NOT run.
// ─────────────────────────────────────────────────────────────────────────────

/// Why a lock was not taken. `detail` carries the twin's classification string
/// (`held`, `timeout`, `stale-unrecovered`, `unattributable`, `foreign-host`,
/// `create-failed`, `record-write-failed`, `release-pending`, `no-project-root`,
/// `unknown-lock`, `order`) so diagnostics and tests can read the exact reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockRefusal {
    /// Somebody is holding it right now. RETRYABLE — the honest family answer is
    /// "someone else is changing that — try again in a moment".
    Busy { name: String, detail: String },
    /// The lock could not be taken and retrying will not help: the filesystem
    /// refused it, the record could not be written, or the file on disk is
    /// unbreakable (§5) and needs a human. Never fail-open.
    Unavailable {
        name: String,
        detail: String,
        /// The exact file a human must remove, when there is one.
        cure: String,
    },
    /// An ordering inversion — a bug in OUR code, never a runtime condition.
    Order(String),
}

impl LockRefusal {
    /// The twin's `detail` classification.
    pub fn detail(&self) -> &str {
        match self {
            LockRefusal::Busy { detail, .. } => detail,
            LockRefusal::Unavailable { detail, .. } => detail,
            LockRefusal::Order(_) => "order",
        }
    }

    /// Is retrying in a moment honest? (The twin's `BUSY_DETAILS`.)
    pub fn retryable(&self) -> bool {
        matches!(self, LockRefusal::Busy { .. })
    }
}

impl std::fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockRefusal::Busy { name, detail } => write!(
                f,
                "\"{name}\" is held by another process ({detail}) — nothing was written"
            ),
            LockRefusal::Unavailable { name, detail, cure } => write!(
                f,
                "\"{name}\" could not be locked ({detail}) — the mutation was NOT run. {cure}"
            ),
            LockRefusal::Order(m) => write!(f, "lock order violation: {m}"),
        }
    }
}

impl std::error::Error for LockRefusal {}

/// What a release PROVED. Ownership is dropped only on proof (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    /// The unlink provably hit the inode carrying our token.
    Released,
    /// An inner frame of a re-entrant acquisition exited; the outermost holder
    /// still owns the lock.
    Reentrant,
    /// A successor owns the lock now. We touched nothing (§4a) or put the record
    /// straight back (§4d). A slow holder can lose the lock; it can never take
    /// its successor's lock away.
    Stolen,
    /// The lock path was already empty and stayed empty.
    Gone,
    /// A transient failure. We STILL OWN the lock and will retry — on the next
    /// acquire in this thread, or on the guard's drop.
    Retained(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-thread bookkeeping (§6).
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Holder {
    token: String,
    depth: u32,
    /// A holder whose release could not be VERIFIED: it still owns the lock and
    /// still has the authority to retry (§4). NOT re-enterable.
    pending: bool,
    /// The `(dev, ino)` of the inode we published — §4a's grounded identity.
    dev: u64,
    ino: u64,
}

#[derive(Debug, Clone)]
struct Frame {
    name: String,
    rank: u32,
    path: PathBuf,
}

thread_local! {
    /// Lock PATHS this thread holds → holder state. Keyed on the resolved path,
    /// i.e. on (root, name) (§6).
    static HELD: std::cell::RefCell<std::collections::HashMap<PathBuf, Holder>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Locks held by this thread, innermost last — the ordering invariant (§1).
    static STACK: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Which "this lock needs a human" lines have already been printed.
    static WARNED: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

// ─────────────────────────────────────────────────────────────────────────────
// §1 — where a named lock lives.
// ─────────────────────────────────────────────────────────────────────────────

/// `<resolve(root)>/.casa/locks/<name>.lock`.
///
/// The root is made absolute LEXICALLY — `.` and `..` are folded, the cwd is
/// prepended to a relative path, and symlinks are left alone — because that is
/// precisely what the Node twin's `path.resolve` does. Calling `canonicalize`
/// here instead would make `/var/x` (what the gateway resolves) and
/// `/private/var/x` (what the engine would resolve on macOS) two different
/// strings for what a human sees as one lock. They would still be one FILE, so
/// exclusion would survive — but the two implementations must also agree on what
/// they PRINT and compare, so we match the twin exactly.
pub fn lock_path_for(root: &Path, name: &str) -> PathBuf {
    absolutize(root)
        .join(".casa")
        .join("locks")
        .join(format!("{name}.lock"))
}

fn absolutize(p: &Path) -> PathBuf {
    let base = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::new();
    for c in base.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// §2 — the owner record.
// ─────────────────────────────────────────────────────────────────────────────

/// One line of JSON plus a trailing newline, in the twin's field order. `pid` and
/// `host` are EVIDENCE for a report; identity is `token`, and only `token`.
fn owner_record(token: &str, pid: u32, host: &str, acquired_ms: i64) -> String {
    format!(
        "{{\"v\":1,\"token\":\"{token}\",\"pid\":{pid},\"host\":{},\"acquiredMs\":{acquired_ms}}}\n",
        serde_json::Value::String(host.to_string())
    )
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        })
}

/// 16 crypto-random bytes as 32 lowercase hex. NEVER derived from pid, time or
/// hostname: a successor must not be able to reconstruct the token of the holder
/// it replaced (§2).
fn mint_token() -> String {
    let mut buf = [0u8; 16];
    if getrandom::getrandom(&mut buf).is_err() {
        // A uuid v4 is OS entropy too. A time/pid fallback would silently make
        // the token forgeable, so there is no such fallback.
        buf.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    hex::encode(buf)
}

/// The parsed owner record, or `None` for anything ownerless, truncated or of a
/// foreign protocol version. A `None` is never equal to ours, so it is never
/// removed. Mirrors the twin's `parseLock` validation exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerRecord {
    token: String,
    pid: i64,
    host: String,
    acquired_ms: i64,
}

fn parse_record(text: &str) -> Option<OwnerRecord> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    if v.get("v")? != &serde_json::Value::from(1) {
        return None;
    }
    let token = v.get("token")?.as_str()?.to_string();
    if token.len() != 32 || !token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return None;
    }
    let pid = v.get("pid")?.as_i64()?;
    if pid <= 0 {
        return None;
    }
    let host = v.get("host")?.as_str()?.to_string();
    if host.is_empty() {
        return None;
    }
    let acquired_ms = v.get("acquiredMs").and_then(|m| m.as_i64()).unwrap_or(0);
    Some(OwnerRecord {
        token,
        pid,
        host,
        acquired_ms,
    })
}

/// Is `pid` a live process ON THIS MACHINE? **`ESRCH` — and only `ESRCH` — proves
/// absence** (§5). `EPERM` means alive under another user; every other errno
/// means "we do not know", and "we do not know" must never be reported as death.
/// This is a REPORTING input only: nothing acts on the answer.
fn pid_alive(pid: i64, kill: &dyn Fn(i64) -> Result<(), i32>) -> bool {
    if pid <= 0 {
        return true;
    }
    match kill(pid) {
        Ok(()) => true,
        Err(errno) => errno != libc::ESRCH,
    }
}

#[cfg(unix)]
fn real_kill(pid: i64) -> Result<(), i32> {
    // SAFETY: signal 0 performs the permission/existence check and delivers
    // nothing. The only inputs are a pid we read out of a lock record and 0.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL))
    }
}

#[cfg(not(unix))]
fn real_kill(_pid: i64) -> Result<(), i32> {
    // No portable liveness check: "we do not know" is never death (§5).
    Err(libc::EINVAL)
}

// ─────────────────────────────────────────────────────────────────────────────
// Test seams. Compiled ONLY under `cfg(test)`: the boundary negatives docs/42
// §9(2) requires (an injected fsync EIO, and a third process started from INSIDE
// the injected rename) cannot be produced from outside the module, and a
// production build must not carry a switch that can turn the protocol off.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod inject {
    use std::sync::atomic::AtomicBool;
    /// Make the RECORD `fsync` fail with EIO (§2).
    pub static FAIL_RECORD_FSYNC: AtomicBool = AtomicBool::new(false);
    /// Make the CONTAINING DIRECTORY `fsync` fail with EIO (§2).
    pub static FAIL_DIR_FSYNC: AtomicBool = AtomicBool::new(false);
    /// Skip §4a's ownership pre-check — the REJECTED build, kept only so the
    /// §4a negative has a control that proves it has teeth.
    pub static SKIP_OWNERSHIP_PRECHECK: AtomicBool = AtomicBool::new(false);

    type Hook = Box<dyn Fn() + Send + Sync>;
    /// Fired at the exact instant the authority pathname is EMPTY — after
    /// `rename(lockPath → quarantine)` and before anything is put back.
    pub static AFTER_DETACH_RENAME: std::sync::Mutex<Option<Hook>> = std::sync::Mutex::new(None);

    pub fn reset() {
        FAIL_RECORD_FSYNC.store(false, std::sync::atomic::Ordering::SeqCst);
        FAIL_DIR_FSYNC.store(false, std::sync::atomic::Ordering::SeqCst);
        SKIP_OWNERSHIP_PRECHECK.store(false, std::sync::atomic::Ordering::SeqCst);
        *AFTER_DETACH_RENAME.lock().unwrap() = None;
    }

    pub fn armed(flag: &AtomicBool) -> bool {
        flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn record_fsync(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(test)]
    if inject::armed(&inject::FAIL_RECORD_FSYNC) {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
    file.sync_all()
}

/// `fsync` a DIRECTORY, so a `link(2)` that has been made is a link that survives
/// the power cut. Returns the error rather than panicking so the caller can fail
/// CLOSED on it.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if inject::armed(&inject::FAIL_DIR_FSYNC) {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
    let fd = std::fs::File::open(dir)?;
    fd.sync_all()
}

fn after_detach_rename() {
    #[cfg(test)]
    {
        let hook = inject::AFTER_DETACH_RENAME.lock().unwrap();
        if let Some(h) = hook.as_ref() {
            h();
        }
    }
}

fn precheck_enabled() -> bool {
    #[cfg(test)]
    {
        !inject::armed(&inject::SKIP_OWNERSHIP_PRECHECK)
    }
    #[cfg(not(test))]
    {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §2 — publish.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn ids_of(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn ids_of(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// What `try_publish` proved.
enum Published {
    /// We hold the lock; carries the identity release checks against (§4a).
    Took { dev: u64, ino: u64 },
    /// `EEXIST` at the link — somebody holds it. Classify and wait (§5).
    Held,
    /// A LOCAL failure. Retrying cannot help; fail closed with the typed reason.
    Failed { detail: &'static str, error: String },
}

fn try_publish(lock_path: &Path, token: &str, pid: u32, host: &str, now_ms: i64) -> Published {
    let body = owner_record(token, pid, host, now_ms);
    let staging = with_suffix(lock_path, &format!("new.{}", mint_token()));
    let scrub = || {
        let _ = std::fs::remove_file(&staging);
    };

    // (a) a PRIVATE staging file, not the lock path.
    let mut file = match open_exclusive(&staging) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `.casa/locks/` does not exist yet (a fresh house). A failure to
            // create it is a TYPED error, not contention: reporting contention
            // here would spin to the full deadline on what is really an EACCES.
            if let Some(parent) = lock_path.parent() {
                if let Err(e1) = std::fs::create_dir_all(parent) {
                    return Published::Failed {
                        detail: "create-failed",
                        error: e1.to_string(),
                    };
                }
            }
            match open_exclusive(&staging) {
                Ok(f) => f,
                Err(e2) => {
                    return Published::Failed {
                        detail: "create-failed",
                        error: e2.to_string(),
                    }
                }
            }
        }
        Err(e) => {
            return Published::Failed {
                detail: "create-failed",
                error: e.to_string(),
            }
        }
    };

    // (b) write IN FULL — `write_all` is the short-write loop; a short write does
    //     not raise an error, it silently publishes a truncated record.
    // (c) fsync (A REFUSED FSYNC IS A REFUSED LOCK), capture the inode we are
    //     about to publish, close, read back, compare byte for byte.
    let staged = file
        .write_all(body.as_bytes())
        .and_then(|()| record_fsync(&file))
        .and_then(|()| file.metadata().map(|m| ids_of(&m)));
    let identity = match staged {
        Ok(ids) => ids,
        Err(e) => {
            scrub();
            return Published::Failed {
                detail: "record-write-failed",
                error: e.to_string(),
            };
        }
    };
    drop(file);

    let mut back = Vec::new();
    if let Err(e) = std::fs::File::open(&staging).and_then(|mut f| f.read_to_end(&mut back)) {
        scrub();
        return Published::Failed {
            detail: "record-write-failed",
            error: e.to_string(),
        };
    }
    if back != body.as_bytes() {
        scrub();
        return Published::Failed {
            detail: "record-write-failed",
            error: format!(
                "the staged record read back as {} of {} bytes",
                back.len(),
                body.len()
            ),
        };
    }

    // (d) THE ACQUISITION. `link(2)` fails EEXIST rather than clobbering, so it is
    //     exactly as exclusive as O_EXCL while making the visible transition go
    //     straight from "no lock" to "a whole, parseable lock".
    if let Err(e) = std::fs::hard_link(&staging, lock_path) {
        scrub();
        return if e.kind() == std::io::ErrorKind::AlreadyExists {
            Published::Held
        } else {
            Published::Failed {
                detail: "create-failed",
                error: e.to_string(),
            }
        };
    }

    // (e) the ENTRY, not just the bytes. Failing here means we must give the lock
    //     straight back — and we can do that safely, because we have just
    //     established, atomically, that this pathname is OUR inode. This is the
    //     ONE moment a failed acquisition may touch the lock path (§2).
    if let Some(parent) = lock_path.parent() {
        if let Err(e) = fsync_dir(parent) {
            if let Ok(meta) = std::fs::symlink_metadata(lock_path) {
                if ids_of(&meta) == identity {
                    let _ = std::fs::remove_file(lock_path);
                }
            }
            scrub();
            return Published::Failed {
                detail: "record-write-failed",
                error: e.to_string(),
            };
        }
    }
    scrub();
    Published::Took {
        dev: identity.0,
        ino: identity.1,
    }
}

fn open_exclusive(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// `<lockPath>.<suffix>` — appended, never a component swap. `Path::with_extension`
/// would eat the `.lock`.
fn with_suffix(lock_path: &Path, suffix: &str) -> PathBuf {
    let mut s = lock_path.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

// ─────────────────────────────────────────────────────────────────────────────
// §4 — detach then decide.
// ─────────────────────────────────────────────────────────────────────────────

enum Detached {
    /// The inode we judged is gone; the lock path is free.
    Removed,
    /// There was nothing to detach.
    Gone,
    /// Not ours — the record is back at the lock path, untouched.
    Restored,
    /// Not ours, and a NEW lock appeared while the path was empty: the record is
    /// left in quarantine as evidence and nothing was destroyed.
    Displaced(PathBuf),
    /// The rename itself failed (transient); nothing moved.
    Failed(String),
}

fn detach_and_decide(lock_path: &Path, our_token: &str) -> Detached {
    let quarantine = with_suffix(lock_path, &format!("reclaim.{}", mint_token()));
    // (a) ONE atomic syscall. The path is now free and we hold a private handle
    //     on exactly one inode.
    if let Err(e) = std::fs::rename(lock_path, &quarantine) {
        return if e.kind() == std::io::ErrorKind::NotFound {
            Detached::Gone
        } else {
            Detached::Failed(e.to_string())
        };
    }
    // THE GAP. Everything between here and the unlink/link below runs with the
    // authority pathname EMPTY. §4a exists so we only ever get here having proved
    // the pathname was ours; the seam lets a test drive a third process into the
    // window and show that it never opens.
    after_detach_rename();

    // (b) inspect ONLY the moved inode.
    let raw = std::fs::read_to_string(&quarantine).unwrap_or_default();
    let ours = parse_record(&raw).map(|r| r.token == our_token).unwrap_or(false);
    if ours {
        // (c) the removal provably hit the inode we judged, and nothing else.
        if std::fs::remove_file(&quarantine).is_ok() {
            return Detached::Removed;
        }
        // fall through and put it back rather than leave the week unlocked
    }
    // (d) we moved somebody else's live lock. Put it back.
    match std::fs::hard_link(&quarantine, lock_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&quarantine);
            Detached::Restored
        }
        // EEXIST: somebody took the free path. KEEP the record — destroying a
        // lock file we could not attribute is precisely what this never does.
        Err(_) => Detached::Displaced(quarantine),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The report a human acts on (§5).
// ─────────────────────────────────────────────────────────────────────────────

fn cure_for(lock_path: &Path, detail: &str) -> String {
    match detail {
        "create-failed" | "record-write-failed" => format!(
            "The filesystem would not hold a lock file there — check that {} is a writable \
             directory with space left, then retry. There is no file to remove.",
            lock_path.parent().unwrap_or(lock_path).display()
        ),
        "release-pending" => format!(
            "This process still owns that lock and could not prove it let go; it retries on \
             every acquire. If it never clears, restart the listener, then: rm {}",
            lock_path.display()
        ),
        "no-project-root" => {
            "There is no project root to lock against, so there is nowhere to rendezvous with \
             the gateway. Pass --root, or run from the household directory."
                .to_string()
        }
        _ => format!(
            "Check that no process is mid-write, then: rm {}",
            lock_path.display()
        ),
    }
}

fn report_unrecoverable(lock_path: &Path, name: &str, detail: &str) {
    let key = format!("{}|{detail}", lock_path.display());
    let first = WARNED.with(|w| w.borrow_mut().insert(key));
    if !first {
        return;
    }
    eprintln!(
        "[project-lock] {}: {detail} — mutations guarded by \"{name}\" are REFUSED. Nothing is \
         being written without the lock: this module never runs a mutation it could not \
         serialise. {}",
        lock_path.display(),
        cure_for(lock_path, detail)
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Acquire / release.
// ─────────────────────────────────────────────────────────────────────────────

/// Tuning + the injectable clock/liveness seams the tests drive. Production
/// callers use [`Options::default`].
#[derive(Clone)]
pub struct Options {
    pub wait_ms: u64,
    pub stale_ms: u64,
    pub sleep_ms: u64,
    pub(crate) now_ms: fn() -> i64,
    pub(crate) kill: fn(i64) -> Result<(), i32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            wait_ms: DEFAULT_WAIT_MS,
            stale_ms: DEFAULT_STALE_MS,
            sleep_ms: SLEEP_MS,
            now_ms: || chrono::Utc::now().timestamp_millis(),
            kill: real_kill,
        }
    }
}

impl Options {
    /// A shorter wait, for a caller that would rather answer "not right now"
    /// than keep the family waiting.
    pub fn waiting(wait_ms: u64) -> Self {
        Options {
            wait_ms,
            ..Options::default()
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("wait_ms", &self.wait_ms)
            .field("stale_ms", &self.stale_ms)
            .field("sleep_ms", &self.sleep_ms)
            .finish()
    }
}

/// A held lock. Dropping it releases; [`ProjectLock::release`] does the same and
/// hands back what was PROVED.
#[derive(Debug)]
pub struct ProjectLock {
    root: PathBuf,
    name: String,
    path: PathBuf,
    token: String,
    reentrant: bool,
    released: bool,
}

impl ProjectLock {
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn is_reentrant(&self) -> bool {
        self.reentrant
    }

    pub fn release(mut self) -> Release {
        let out = release_locked(&self.root, &self.name, &self.token);
        self.released = true;
        out
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = release_locked(&self.root, &self.name, &self.token);
        }
    }
}

/// Acquire `(root, name)`, or fail CLOSED (§2, §3, §5).
pub fn acquire(root: &Path, name: &str, opts: &Options) -> Result<ProjectLock, LockRefusal> {
    let Some(rank) = rank_of(name) else {
        return Err(LockRefusal::Order(format!(
            "unknown lock \"{name}\" — add it to the rank table with a rank"
        )));
    };
    if root.as_os_str().is_empty() {
        let path = PathBuf::from(name);
        return Err(LockRefusal::Unavailable {
            name: name.to_string(),
            detail: "no-project-root".into(),
            cure: cure_for(&path, "no-project-root"),
        });
    }
    let path = lock_path_for(root, name);

    // A PENDING RELEASE IS NOT RE-ENTERABLE (§4/§6). The frame that owned this
    // lock has already finished and tried to release; treating that as
    // re-entrancy is a permanent wedge, because the nested frame's exit only
    // decrements the depth and the FILESYSTEM release is never retried.
    let pending = HELD.with(|h| {
        h.borrow()
            .get(&path)
            .filter(|x| x.pending)
            .map(|x| x.token.clone())
    });
    if let Some(tok) = pending {
        let finished = release_locked(root, name, &tok);
        if let Release::Retained(_) = finished {
            report_unrecoverable(&path, name, "release-pending");
            return Err(LockRefusal::Unavailable {
                name: name.to_string(),
                detail: "release-pending".into(),
                cure: cure_for(&path, "release-pending"),
            });
        }
    }

    // RE-ENTRANCY, keyed on the RESOLVED PATH — i.e. on (root, name), so house B's
    // callback is never handed a lock taken for house A (§6).
    let mine = HELD.with(|h| h.borrow().get(&path).map(|x| x.token.clone()));
    if let Some(token) = mine {
        HELD.with(|h| {
            if let Some(x) = h.borrow_mut().get_mut(&path) {
                x.depth += 1;
            }
        });
        return Ok(ProjectLock {
            root: root.to_path_buf(),
            name: name.to_string(),
            path,
            token,
            reentrant: true,
            released: false,
        });
    }

    // THE ORDERING INVARIANT (§1), against the innermost lock held by this
    // thread. STRICTLY greater, so equal-rank nesting is an error too — two
    // processes nesting two rank-10 locks in opposite orders deadlock exactly as
    // readily. Re-entering the SAME lock, handled above, is not a nesting.
    if let Some(inner) = STACK.with(|s| s.borrow().last().cloned()) {
        if rank <= inner.rank {
            return Err(LockRefusal::Order(format!(
                "\"{}\" (rank {}, {}) is held; \"{name}\" (rank {rank}, {}) must be taken BEFORE \
                 it, never inside it",
                inner.name,
                inner.rank,
                inner.path.display(),
                path.display()
            )));
        }
    }

    if let Some(parent) = path.parent() {
        // Typed by the create below — never fail-open.
        let _ = std::fs::create_dir_all(parent);
    }

    let host = hostname();
    let pid = std::process::id();
    let deadline = (opts.now_ms)() + opts.wait_ms as i64;
    // The loop yields the classification it exits on: a taker never reports a
    // reason it did not actually observe.
    let last_detail = loop {
        let token = mint_token();
        match try_publish(&path, &token, pid, &host, (opts.now_ms)()) {
            Published::Took { dev, ino } => {
                HELD.with(|h| {
                    h.borrow_mut().insert(
                        path.clone(),
                        Holder {
                            token: token.clone(),
                            depth: 1,
                            pending: false,
                            dev,
                            ino,
                        },
                    )
                });
                STACK.with(|s| {
                    s.borrow_mut().push(Frame {
                        name: name.to_string(),
                        rank,
                        path: path.clone(),
                    })
                });
                return Ok(ProjectLock {
                    root: root.to_path_buf(),
                    name: name.to_string(),
                    path,
                    token,
                    reentrant: false,
                    released: false,
                });
            }
            Published::Failed { detail, error } => {
                // A LOCAL failure, not contention. Retrying cannot help, and
                // RUNNING THE MUTATION ANYWAY is the fail-open this protocol
                // exists to remove (§3).
                report_unrecoverable(&path, name, detail);
                let _ = error;
                return Err(LockRefusal::Unavailable {
                    name: name.to_string(),
                    detail: detail.to_string(),
                    cure: cure_for(&path, detail),
                });
            }
            Published::Held => {}
        }

        // EEXIST: somebody holds it. Classify WHY — for the caller and for a
        // human — then wait, or give up. NOTHING below removes a lock file (§5).
        let detail = classify_held(&path, &host, opts).to_string();
        // The deadline is checked on EVERY loop edge, before sleeping: a caller
        // that asked for 120ms gets at most 120ms, whichever branch it landed in.
        if (opts.now_ms)() >= deadline {
            break detail;
        }
        std::thread::sleep(std::time::Duration::from_millis(opts.sleep_ms));
        if (opts.now_ms)() >= deadline {
            break detail;
        }
    };

    if last_detail == "held" || last_detail == "timeout" {
        Err(LockRefusal::Busy {
            name: name.to_string(),
            detail: last_detail,
        })
    } else {
        report_unrecoverable(&path, name, &last_detail);
        let cure = cure_for(&path, &last_detail);
        Err(LockRefusal::Unavailable {
            name: name.to_string(),
            detail: last_detail,
            cure,
        })
    }
}

/// Why the lock at `path` is not available. **Classification only** — this
/// function removes nothing and nothing downstream acts on its verdict (§5).
fn classify_held(path: &Path, host: &str, opts: &Options) -> &'static str {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        // Vanished between the EEXIST and the read — race for it again.
        Err(_) => return "held",
    };
    let Some(rec) = parse_record(&raw) else {
        return "unattributable"; // ownerless / corrupt / foreign-version
    };
    if rec.host != host {
        return "foreign-host"; // a pid on another machine is unknowable here
    }
    let age = (opts.now_ms)() - rec.acquired_ms;
    // OLD **AND** PROVABLY GONE. Age alone is never enough for anything, and even
    // both together only change the REPORT: nothing is removed (§5).
    if age >= opts.stale_ms as i64 && !pid_alive(rec.pid, &|p| (opts.kill)(p)) {
        return "stale-unrecovered";
    }
    "held"
}

/// Release, per §4/§4a. Ownership is dropped only on proof.
fn release_locked(root: &Path, name: &str, token: &str) -> Release {
    let path = lock_path_for(root, name);
    let mine = HELD.with(|h| h.borrow().get(&path).cloned());

    // THE TOKEN IS CHECKED FIRST, AGAINST OUR OWN RECORD, BEFORE ANY DISK OR
    // STATE CHANGE. If this thread holds the lock, the only token that may
    // release it is the one we wrote.
    if let Some(m) = &mine {
        if m.token != token {
            return Release::Retained("token-mismatch".into());
        }
        if !m.pending {
            let depth = HELD.with(|h| {
                let mut b = h.borrow_mut();
                let x = b.get_mut(&path).expect("held");
                x.depth = x.depth.saturating_sub(1);
                x.depth
            });
            if depth > 0 {
                return Release::Reentrant;
            }
        }
    } else if token.len() != 32 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Release::Gone;
    }

    let drop_held = || {
        HELD.with(|h| h.borrow_mut().remove(&path));
        STACK.with(|s| {
            let mut b = s.borrow_mut();
            if let Some(i) = b.iter().rposition(|f| f.path == path) {
                b.remove(i);
            }
        });
    };
    let retain = |reason: String| {
        HELD.with(|h| {
            if let Some(x) = h.borrow_mut().get_mut(&path) {
                x.depth = x.depth.max(1);
                x.pending = true;
            }
        });
        Release::Retained(reason)
    };

    let mut last = "gone".to_string();
    for attempt in 0..RELEASE_ATTEMPTS {
        // ── §4a: OWNERSHIP BEFORE ANY PATHNAME CHANGE ────────────────────────
        // Going straight to `rename(lockPath, quarantine)` and judging the record
        // afterwards moves a LIVE SUCCESSOR's authority pathname out of the way,
        // and the pathname then sits EMPTY for the length of a read: a third
        // process's create succeeds there, the restoring link fails EEXIST, and
        // two processes are left each certain they hold the week. The fix is not
        // a better judgement AFTER the move — it is refusing to move a pathname
        // we have not proved is ours.
        if precheck_enabled() {
            let st = std::fs::symlink_metadata(&path).ok();
            let Some(st) = st else {
                // Nothing at the pathname at all: nobody's authority is at risk.
                last = "gone".into();
                if attempt + 1 < RELEASE_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                drop_held();
                return Release::Gone;
            };
            match &mine {
                // `(dev, ino)` from the fstat of the inode we published is the
                // kernel's own answer — unforgeable, and not reusable by another
                // process while our link lives.
                Some(m) => {
                    if ids_of(&st) != (m.dev, m.ino) {
                        drop_held();
                        return Release::Stolen;
                    }
                }
                // No kernel-grounded identity (a release driven by token alone).
                // Fall back to the RECORD, and still refuse to move a pathname
                // whose token is not ours — weaker evidence than an inode, but
                // never as weak as "rename first and judge afterwards".
                None => {
                    let rec = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|r| parse_record(&r));
                    if rec.map(|r| r.token != token).unwrap_or(true) {
                        drop_held();
                        return Release::Stolen;
                    }
                }
            }
        }

        match detach_and_decide(&path, token) {
            Detached::Removed => {
                drop_held();
                return Release::Released;
            }
            // Provably not ours any more: a successor legitimately owns the lock.
            Detached::Restored => {
                drop_held();
                return Release::Stolen;
            }
            Detached::Displaced(q) => {
                eprintln!(
                    "[project-lock] {}: a foreign lock record was moved to {} and could not be \
                     restored — it is LEFT ON DISK as evidence. A lock record is never destroyed \
                     on a guess.",
                    path.display(),
                    q.display()
                );
                drop_held();
                return Release::Stolen;
            }
            Detached::Gone => {
                last = "gone".into();
                if attempt + 1 < RELEASE_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                drop_held();
                return Release::Gone;
            }
            Detached::Failed(e) => {
                last = e;
                if attempt + 1 < RELEASE_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }
    retain(last)
}

// ─────────────────────────────────────────────────────────────────────────────
// The API the writers call.
// ─────────────────────────────────────────────────────────────────────────────

/// Run `f` while holding `(root, name)`, or refuse.
///
/// **FAIL CLOSED (§3).** There is no path on which `f` runs without the lock: not
/// a read-only mount, not an `ENOTDIR` root, not a full disk, not a wedged lock
/// file. The refusal is typed so the caller can turn it into the family's
/// grounded "not right now" — never into a silent unserialised write.
pub fn with_project_lock<T>(
    root: &Path,
    name: &str,
    opts: &Options,
    f: impl FnOnce() -> T,
) -> Result<T, LockRefusal> {
    let lock = acquire(root, name, opts)?;
    let path = lock.path().to_path_buf();
    let out = f();
    // §7: nothing is "finished up afterwards" — the caller's whole transaction is
    // inside `f`, and the lock is let go only once it has returned.
    if let Release::Retained(reason) = lock.release() {
        // We could not PROVE we let go. Ownership stays with us (§4) so the next
        // acquire in this thread retries the release rather than deadlocking
        // against our own file — and a human is told, because a release that never
        // clears is a wedged week.
        eprintln!(
            "[project-lock] release of {} could not be verified ({reason}) — this process still \
             owns it and will retry on the next acquire. The mutation itself completed.",
            path.display()
        );
    }
    Ok(out)
}

/// The week lock, at the default wait. Every engine path that rewrites a plan
/// file, the shopping overlay, the carry or a parked dinner goes through here.
pub fn with_week_mutation_lock<T>(
    root: &Path,
    f: impl FnOnce() -> T,
) -> Result<T, LockRefusal> {
    with_project_lock(root, WEEK_MUTATION, &Options::default(), f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn opts(wait_ms: u64) -> Options {
        Options {
            wait_ms,
            sleep_ms: 2,
            ..Options::default()
        }
    }

    /// Every test that arms an injection seam or reads the thread-local held map
    /// must start from a clean slate — the seams are process-global statics.
    fn clean() {
        inject::reset();
        HELD.with(|h| h.borrow_mut().clear());
        STACK.with(|s| s.borrow_mut().clear());
    }

    /// A SECOND WRITER, and not a nested frame of this one. Re-entrancy is keyed
    /// per (thread, resolved path) (§6), so a "second taker" driven from the same
    /// call stack would be recognised as the SAME writer re-entering — which is
    /// correct behaviour and a useless test. Every contention test therefore takes
    /// its second role on a fresh thread, where the only thing the two writers
    /// share is the file.
    fn acquire_elsewhere(root: &Path, o: Options) -> Result<Release, LockRefusal> {
        let root = root.to_path_buf();
        std::thread::spawn(move || acquire(&root, WEEK_MUTATION, &o).map(|l| l.release()))
            .join()
            .unwrap()
    }

    /// A second writer that KEEPS holding the lock until it is told to let go.
    struct Elsewhere {
        token: String,
        go: std::sync::mpsc::Sender<()>,
        join: std::thread::JoinHandle<Release>,
    }

    impl Elsewhere {
        fn release(self) -> Release {
            let _ = self.go.send(());
            self.join.join().unwrap()
        }
    }

    fn hold_elsewhere(root: &Path, o: Options) -> Elsewhere {
        let root = root.to_path_buf();
        let (tok_tx, tok_rx) = std::sync::mpsc::channel::<String>();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let lock = acquire(&root, WEEK_MUTATION, &o).expect("the other writer must acquire");
            tok_tx.send(lock.token().to_string()).unwrap();
            let _ = wait.recv();
            lock.release()
        });
        let token = tok_rx.recv().expect("the other writer must acquire");
        Elsewhere { token, go, join }
    }

    // ── §1: identity and ranks ──────────────────────────────────────────────

    #[test]
    #[serial(project_lock)]
    fn the_lock_path_and_ranks_match_the_node_twin() {
        let dir = scratch();
        assert_eq!(
            lock_path_for(dir.path(), WEEK_MUTATION),
            dir.path().join(".casa/locks/week-mutation.lock")
        );
        assert_eq!(rank_of(WEEK_MUTATION), Some(10));
        assert_eq!(rank_of(FEED_ROTATION), Some(20));
        assert_eq!(rank_of("invented"), None);
        assert_eq!(DEFAULT_WAIT_MS, 5000);
        assert_eq!(DEFAULT_STALE_MS, 15000);
        // `/house`, `/house/` and `/house/./x/..` are ONE lock, not three.
        let a = lock_path_for(Path::new("/house"), WEEK_MUTATION);
        let b = lock_path_for(Path::new("/house/"), WEEK_MUTATION);
        let c = lock_path_for(Path::new("/house/./x/.."), WEEK_MUTATION);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// §1: the identity of a lock is (root, name). Two households never share one.
    #[test]
    #[serial(project_lock)]
    fn two_roots_are_two_locks() {
        clean();
        let a = scratch();
        let b = scratch();
        let held_a = acquire(a.path(), WEEK_MUTATION, &opts(200)).unwrap();
        // House B's lock is a DIFFERENT lock: taking it while A's is held must not
        // block, and must not be mistaken for re-entering A's.
        assert_eq!(acquire_elsewhere(b.path(), opts(200)), Ok(Release::Released));
        assert!(lock_path_for(a.path(), WEEK_MUTATION).exists());
        assert!(!lock_path_for(b.path(), WEEK_MUTATION).exists());
        assert_ne!(
            lock_path_for(a.path(), WEEK_MUTATION),
            lock_path_for(b.path(), WEEK_MUTATION)
        );
        assert_eq!(held_a.release(), Release::Released);
    }

    // ── §2: the record ──────────────────────────────────────────────────────

    #[test]
    #[serial(project_lock)]
    fn the_owner_record_is_the_documented_line_with_a_trailing_newline() {
        clean();
        let dir = scratch();
        let lock = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let body = std::fs::read_to_string(lock.path()).unwrap();
        assert!(body.ends_with('\n'), "trailing newline: {body:?}");
        assert_eq!(body.matches('\n').count(), 1, "ONE line: {body:?}");
        // Field ORDER is fixed, not just field presence.
        assert!(
            body.starts_with("{\"v\":1,\"token\":\""),
            "field order is fixed: {body:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["v"], 1);
        let token = parsed["token"].as_str().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(token, lock.token());
        assert_eq!(parsed["pid"].as_i64().unwrap(), std::process::id() as i64);
        assert!(parsed["host"].is_string());
        assert!(parsed["acquiredMs"].is_i64());
        lock.release();
    }

    #[test]
    #[serial(project_lock)]
    fn tokens_are_crypto_random_and_never_derived_from_pid_time_or_host() {
        let seen: std::collections::HashSet<String> = (0..256).map(|_| mint_token()).collect();
        assert_eq!(seen.len(), 256, "two acquisitions never share one token");
        let pid = std::process::id().to_string();
        for t in &seen {
            assert!(!t.contains(&pid));
        }
    }

    /// §2: the visible transition is "no lock" → "a whole, parseable lock". A
    /// reader must never see a truncated record at the lock path.
    #[test]
    #[serial(project_lock)]
    fn the_published_lock_is_never_a_partial_record() {
        clean();
        let dir = scratch();
        let path = lock_path_for(dir.path(), WEEK_MUTATION);
        let watched = path.clone();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_r = stop.clone();
        let watcher = std::thread::spawn(move || {
            while !stop_r.load(Ordering::Relaxed) {
                if let Ok(body) = std::fs::read_to_string(&watched) {
                    if !body.is_empty() {
                        assert!(
                            parse_record(&body).is_some(),
                            "a partial record was visible at the lock path: {body:?}"
                        );
                    }
                }
            }
        });
        for _ in 0..300 {
            acquire(dir.path(), WEEK_MUTATION, &opts(1000))
                .unwrap()
                .release();
        }
        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap();
    }

    // ── The four named negatives from the task's validation block ───────────

    /// **test_lock_never_broken_by_age** — §5. A LIVE holder that outlasts the
    /// stale horizon is NEVER displaced. The rejected build unlinked it and let a
    /// second writer in; here the second taker fails closed and the holder's
    /// record is still on disk, byte for byte.
    #[test]
    #[serial(project_lock)]
    fn test_lock_never_broken_by_age() {
        clean();
        let dir = scratch();
        // A record that is FIFTY TIMES the stale horizon old, whose owner is this
        // very process — provably, unambiguously alive.
        let holder = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let path = holder.path().to_path_buf();
        let before = std::fs::read_to_string(&path).unwrap();

        let aged = Options {
            wait_ms: 40,
            sleep_ms: 2,
            stale_ms: 1,
            // "now" is an hour after the record was written.
            now_ms: || chrono::Utc::now().timestamp_millis() + 3_600_000,
            ..Options::default()
        };
        let refused = acquire_elsewhere(dir.path(), aged).unwrap_err();
        assert_eq!(
            refused.detail(),
            "held",
            "a LIVE holder is `held` at any age — never stale, never broken"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the holder's record must still be on disk, untouched"
        );

        // And even when the owner is PROVABLY DEAD and ancient, acquisition
        // classifies and REPORTS — it still does not remove (§5).
        holder.release();
        let dead = format!(
            "{{\"v\":1,\"token\":\"{}\",\"pid\":{},\"host\":{},\"acquiredMs\":1}}\n",
            "00112233445566778899aabbccddeeff",
            424242,
            serde_json::Value::String(hostname())
        );
        std::fs::write(&path, &dead).unwrap();
        let stale_opts = Options {
            wait_ms: 30,
            sleep_ms: 2,
            stale_ms: 1,
            kill: |_| Err(libc::ESRCH),
            ..Options::default()
        };
        let refused = acquire(dir.path(), WEEK_MUTATION, &stale_opts).unwrap_err();
        assert_eq!(refused.detail(), "stale-unrecovered");
        assert!(!refused.retryable(), "it needs a human, not a retry");
        assert!(
            refused.to_string().contains(&path.display().to_string()),
            "the report must name the file a human removes: {refused}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            dead,
            "a provably dead owner's lock is STILL not reclaimed"
        );
        std::fs::remove_file(&path).unwrap();
    }

    /// **test_acquire_error_does_not_run_the_write** — §3. An unlockable lock
    /// ROOT refuses the write, and no bytes move. The rejected Node build printed
    /// a warning and ran the mutation with no exclusion at all.
    #[test]
    #[serial(project_lock)]
    fn test_acquire_error_does_not_run_the_write() {
        clean();
        let dir = scratch();
        // `.casa/locks` is a FILE, so every create under it is ENOTDIR. This is
        // the audit's exact repro shape.
        std::fs::create_dir_all(dir.path().join(".casa")).unwrap();
        std::fs::write(dir.path().join(".casa/locks"), b"not a directory").unwrap();

        let target = dir.path().join("plans/2026-W31-family-plan.md");
        let mut ran = false;
        let refused = with_project_lock(dir.path(), WEEK_MUTATION, &opts(50), || {
            ran = true;
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, b"a week written without the lock").unwrap();
        })
        .unwrap_err();

        assert!(!ran, "the callback MUST NOT run when the lock was not taken");
        assert!(!target.exists(), "no bytes move");
        assert_eq!(refused.detail(), "create-failed");
        assert!(!refused.retryable(), "retrying an ENOTDIR root cannot help");
        // And no debris: not a staging file, not a lock, nothing.
        assert!(!dir.path().join(".casa/locks").is_dir());
    }

    /// **test_release_cannot_remove_successor** — §4/§4a. A predecessor's release
    /// RESTORES, never deletes, a successor's record.
    #[test]
    #[serial(project_lock)]
    fn test_release_cannot_remove_successor() {
        clean();
        let dir = scratch();
        let slow = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let path = slow.path().to_path_buf();

        // A human cleared the wedge (§5's cure) and a successor took the lock
        // while this holder was slow.
        std::fs::remove_file(&path).unwrap();
        let successor = hold_elsewhere(dir.path(), opts(200));
        let successor_record = std::fs::read_to_string(&path).unwrap();
        assert_ne!(successor.token, slow.token());

        assert_eq!(slow.release(), Release::Stolen);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            successor_record,
            "the successor's record survives BYTE FOR BYTE"
        );
        // §4a: the pathname was never even moved, so there is no debris at all.
        assert!(debris(&path).is_empty(), "{:?}", debris(&path));
        assert_eq!(successor.release(), Release::Released);
        clean();
    }

    /// **test_only_esrch_is_death** — §5. An injected EINVAL/EPERM liveness error
    /// never classifies the owner as dead.
    #[test]
    #[serial(project_lock)]
    fn test_only_esrch_is_death() {
        clean();
        let dir = scratch();
        let path = lock_path_for(dir.path(), WEEK_MUTATION);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ancient = format!(
            "{{\"v\":1,\"token\":\"{}\",\"pid\":{},\"host\":{},\"acquiredMs\":1}}\n",
            "00112233445566778899aabbccddeeff",
            999_001,
            serde_json::Value::String(hostname())
        );
        std::fs::write(&path, &ancient).unwrap();

        for (errno, label) in [
            (libc::EINVAL, "EINVAL"),
            (libc::EPERM, "EPERM"),
            (libc::EAGAIN, "EAGAIN"),
        ] {
            let o = Options {
                wait_ms: 20,
                sleep_ms: 2,
                stale_ms: 0,
                kill: match errno {
                    libc::EINVAL => |_| Err(libc::EINVAL),
                    libc::EPERM => |_| Err(libc::EPERM),
                    _ => |_| Err(libc::EAGAIN),
                },
                ..Options::default()
            };
            let refused = acquire(dir.path(), WEEK_MUTATION, &o).unwrap_err();
            assert_eq!(
                refused.detail(),
                "held",
                "{label} means WE DO NOT KNOW, which is never death"
            );
            assert!(refused.retryable(), "{label} is the retryable answer");
        }
        // Only ESRCH flips the verdict — and even then nothing is removed.
        let o = Options {
            wait_ms: 20,
            sleep_ms: 2,
            stale_ms: 0,
            kill: |_| Err(libc::ESRCH),
            ..Options::default()
        };
        assert_eq!(
            acquire(dir.path(), WEEK_MUTATION, &o).unwrap_err().detail(),
            "stale-unrecovered"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ancient);
        // And the unit under test, directly.
        assert!(pid_alive(1234, &|_| Err(libc::EPERM)));
        assert!(pid_alive(1234, &|_| Err(libc::EINVAL)));
        assert!(pid_alive(1234, &|_| Ok(())));
        assert!(!pid_alive(1234, &|_| Err(libc::ESRCH)));
    }

    // ── The two negatives docs/42 §9(2) requires AT THE BOUNDARY ────────────

    /// **§2, injected at the boundary.** A refused `fsync` — the record's or the
    /// directory's — fails the acquisition CLOSED, publishes nothing, and leaves
    /// no debris. The rejected Node build caught EIO and returned `locked: true`.
    #[test]
    #[serial(project_lock)]
    fn injected_fsync_eio_publishes_nothing_and_runs_nothing() {
        for (flag, label) in [
            (&inject::FAIL_RECORD_FSYNC, "record fsync"),
            (&inject::FAIL_DIR_FSYNC, "directory fsync"),
        ] {
            clean();
            let dir = scratch();
            let path = lock_path_for(dir.path(), WEEK_MUTATION);
            flag.store(true, Ordering::SeqCst);

            let mut ran = false;
            let refused = with_project_lock(dir.path(), WEEK_MUTATION, &opts(30), || ran = true)
                .unwrap_err();

            assert!(!ran, "{label} EIO must not run the mutation");
            assert_eq!(refused.detail(), "record-write-failed", "{label}");
            assert!(!refused.retryable(), "{label}");
            assert!(
                !path.exists(),
                "{label}: NOTHING is published — a lock whose bytes are not on the device is a \
                 lock whose owner may never have existed"
            );
            assert!(debris(&path).is_empty(), "{label}: {:?}", debris(&path));
            // In-process state is clean too: the next acquire is not wedged.
            inject::reset();
            let ok = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
            assert_eq!(ok.release(), Release::Released);
        }
        clean();
    }

    /// **§4a, injected at the boundary — the successor gap.** A third process
    /// started from INSIDE the injected rename must never acquire.
    ///
    /// The conforming build never gets as far as the rename when a successor owns
    /// the pathname, so the hook never fires and the third process never runs.
    /// That would be a vacuous pass on its own, so the same scenario runs a second
    /// time with §4a switched OFF: there the hook fires, the third process DOES
    /// acquire on the empty pathname, and two holders exist at once — the exact
    /// audit outcome (`cLocked: true`, `currentIsC: true`) this gate forbids.
    #[test]
    #[serial(project_lock)]
    fn a_third_process_started_inside_the_rename_never_acquires() {
        // ── the CONFORMING build ────────────────────────────────────────────
        clean();
        let dir = scratch();
        let path = lock_path_for(dir.path(), WEEK_MUTATION);
        let predecessor = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        std::fs::remove_file(&path).unwrap(); // a human cleared the wedge
        let successor = hold_elsewhere(dir.path(), opts(200));
        let successor_record = std::fs::read_to_string(&path).unwrap();

        let fired = std::sync::Arc::new(AtomicBool::new(false));
        arm_third_process_hook(dir.path(), &fired);
        let outcome = predecessor.release();
        let third = read_third_process_verdict(dir.path());
        inject::reset();

        assert_eq!(outcome, Release::Stolen);
        assert!(
            !fired.load(Ordering::SeqCst),
            "§4a: the pathname must never be renamed once a successor owns it — no rename, no \
             gap, no third holder"
        );
        assert_eq!(third, None, "no third process ran, so none could acquire");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            successor_record,
            "the successor still holds the week"
        );
        successor.release();

        // ── the CONTROL: §4a off is the REJECTED build ──────────────────────
        clean();
        let dir2 = scratch();
        let path2 = lock_path_for(dir2.path(), WEEK_MUTATION);
        let predecessor = acquire(dir2.path(), WEEK_MUTATION, &opts(200)).unwrap();
        std::fs::remove_file(&path2).unwrap();
        let successor = hold_elsewhere(dir2.path(), opts(200));

        let fired2 = std::sync::Arc::new(AtomicBool::new(false));
        arm_third_process_hook(dir2.path(), &fired2);
        inject::SKIP_OWNERSHIP_PRECHECK.store(true, Ordering::SeqCst);
        let _ = predecessor.release();
        let third2 = read_third_process_verdict(dir2.path());
        inject::reset();

        assert!(
            fired2.load(Ordering::SeqCst),
            "the control must actually reach the rename, or it proves nothing"
        );
        assert_eq!(
            third2.as_deref(),
            Some("acquired"),
            "CONTROL: without §4a a third process takes the week inside the gap — this is the \
             failure the conforming half above must not reproduce"
        );
        let _ = successor.release();
        clean();
    }

    /// Arm the seam so that, at the instant the authority pathname is empty, a
    /// REAL second OS process tries to take the lock and writes its verdict where
    /// the test can read it.
    fn arm_third_process_hook(root: &Path, fired: &std::sync::Arc<AtomicBool>) {
        let root = root.to_path_buf();
        let fired = fired.clone();
        *inject::AFTER_DETACH_RENAME.lock().unwrap() = Some(Box::new(move || {
            fired.store(true, Ordering::SeqCst);
            child_acquire(&root);
        }));
    }

    fn read_third_process_verdict(root: &Path) -> Option<String> {
        std::fs::read_to_string(root.join("third-verdict.txt"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// Re-runs THIS test binary as a separate process, in the child mode below.
    /// The verdict comes back through a FILE, not through stdout: a harness that
    /// swallows or reorders the child's output would turn "the third process took
    /// the week" into a silent pass, which is the one thing this gate may not do.
    fn child_acquire(root: &Path) {
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(exe)
            .args([
                "notify::project_lock::tests::child_process_lock_probe",
                "--exact",
                "--test-threads",
                "1",
            ])
            .env("WG_LOCK_CHILD_ROOT", root)
            .env("WG_LOCK_CHILD_VERDICT", root.join("third-verdict.txt"))
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "the third process did not run: {}{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// The child half of [`child_acquire`]: a REAL second process taking the real
    /// lock through the real code path. Inert unless `WG_LOCK_CHILD_ROOT` is set,
    /// so it costs nothing in a normal run.
    #[test]
    #[serial(project_lock)]
    fn child_process_lock_probe() {
        let Ok(root) = std::env::var("WG_LOCK_CHILD_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let verdict = match acquire(&root, WEEK_MUTATION, &opts(0)) {
            Ok(lock) => {
                // Hold it, so the parent's restoring link fails EEXIST exactly as
                // the audit's C did, and the two-holder state is observable.
                std::mem::forget(lock);
                "acquired".to_string()
            }
            Err(e) => format!("refused:{}", e.detail()),
        };
        std::fs::write(std::env::var("WG_LOCK_CHILD_VERDICT").unwrap(), verdict).unwrap();
    }

    // ── §3/§4/§6/§7: the rest of the checklist ──────────────────────────────

    #[test]
    #[serial(project_lock)]
    fn a_second_taker_fails_closed_while_the_lock_is_held() {
        clean();
        let dir = scratch();
        let first = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let refused = acquire_elsewhere(dir.path(), opts(30)).unwrap_err();
        assert_eq!(refused.detail(), "held");
        assert!(refused.retryable());
        assert_eq!(first.release(), Release::Released);
        assert_eq!(
            acquire_elsewhere(dir.path(), opts(200)),
            Ok(Release::Released),
            "and it succeeds the moment the holder lets go"
        );
    }

    /// §5: ownerless, truncated and foreign-version records all fail CLOSED, and
    /// all of them are still on disk afterwards.
    #[test]
    #[serial(project_lock)]
    fn an_unattributable_lock_fails_closed_and_is_never_removed() {
        for record in [
            r#"{"v":1,"pid":10,"host":"h","acquiredMs":1}"#,
            r#"{"v":1,"token":"0011223344"#,
            r#"{"v":2,"token":"00112233445566778899aabbccddeeff","pid":10,"host":"h"}"#,
            "",
        ] {
            clean();
            let dir = scratch();
            let path = lock_path_for(dir.path(), WEEK_MUTATION);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, record).unwrap();
            let refused = acquire(dir.path(), WEEK_MUTATION, &opts(20)).unwrap_err();
            assert_eq!(refused.detail(), "unattributable", "{record:?}");
            assert!(!refused.retryable());
            assert!(
                refused.to_string().contains(&path.display().to_string()),
                "the report names the file a human must remove"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), record);
        }
    }

    /// §5: a record naming another machine is unknowable here — reported, never
    /// broken.
    #[test]
    #[serial(project_lock)]
    fn a_foreign_host_lock_fails_closed() {
        clean();
        let dir = scratch();
        let path = lock_path_for(dir.path(), WEEK_MUTATION);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rec = "{\"v\":1,\"token\":\"00112233445566778899aabbccddeeff\",\"pid\":2,\
                   \"host\":\"some-other-machine\",\"acquiredMs\":1}\n";
        std::fs::write(&path, rec).unwrap();
        let refused = acquire(dir.path(), WEEK_MUTATION, &opts(20)).unwrap_err();
        assert_eq!(refused.detail(), "foreign-host");
        assert!(!refused.retryable());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rec);
    }

    /// §1: strictly increasing ranks. Equal-rank nesting is refused too.
    #[test]
    #[serial(project_lock)]
    fn lock_order_inversions_are_a_loud_immediate_error() {
        clean();
        let dir = scratch();
        let feed = acquire(dir.path(), FEED_ROTATION, &opts(200)).unwrap();
        let inverted = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap_err();
        assert!(matches!(inverted, LockRefusal::Order(_)), "{inverted:?}");
        assert_eq!(inverted.detail(), "order");
        feed.release();

        // Equal rank, two different houses — the textbook deadlock (§6).
        clean();
        let a = scratch();
        let b = scratch();
        let held_a = acquire(a.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let same_rank = acquire(b.path(), WEEK_MUTATION, &opts(200)).unwrap_err();
        assert!(matches!(same_rank, LockRefusal::Order(_)), "{same_rank:?}");
        held_a.release();

        // And the correct order nests fine.
        clean();
        let dir = scratch();
        let week = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let feed = acquire(dir.path(), FEED_ROTATION, &opts(200)).unwrap();
        feed.release();
        week.release();
        clean();
    }

    /// §6: re-entrancy is keyed on the RESOLVED PATH. The inner frame does not
    /// release, and a lock taken for house A is never handed to house B.
    #[test]
    #[serial(project_lock)]
    fn reentrancy_is_keyed_on_the_resolved_lock_path() {
        clean();
        let dir = scratch();
        let outer = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        let inner = acquire(dir.path(), WEEK_MUTATION, &opts(200)).unwrap();
        assert!(inner.is_reentrant());
        assert_eq!(inner.token(), outer.token());
        assert_eq!(inner.release(), Release::Reentrant);
        assert!(
            outer.path().exists(),
            "only the OUTERMOST holder releases (§6)"
        );
        assert_eq!(outer.release(), Release::Released);
        clean();
    }

    /// §3 at the API the writers actually call.
    #[test]
    #[serial(project_lock)]
    fn with_project_lock_never_runs_the_body_unserialised() {
        clean();
        let dir = scratch();
        let held = hold_elsewhere(dir.path(), opts(200));
        let mut ran = false;
        let refused =
            with_project_lock(dir.path(), WEEK_MUTATION, &opts(30), || ran = true).unwrap_err();
        assert!(!ran);
        assert!(refused.retryable());
        assert_eq!(held.release(), Release::Released);
        // And the happy path does run it, and lets go afterwards.
        let out = with_project_lock(dir.path(), WEEK_MUTATION, &opts(200), || 7).unwrap();
        assert_eq!(out, 7);
        assert!(!lock_path_for(dir.path(), WEEK_MUTATION).exists());
    }

    /// The property every serialised read-modify-write depends on: real
    /// exclusion, with no lost update.
    #[test]
    #[serial(project_lock)]
    fn concurrent_writers_never_lose_an_update() {
        clean();
        let dir = scratch();
        let counter = dir.path().join("counter.txt");
        std::fs::write(&counter, "0").unwrap();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let root = dir.path().to_path_buf();
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    with_project_lock(&root, WEEK_MUTATION, &opts(10_000), || {
                        // read → pause → write: without real exclusion this loses
                        // updates every time.
                        let n: u64 = std::fs::read_to_string(&counter)
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        std::thread::yield_now();
                        std::fs::write(&counter, (n + 1).to_string()).unwrap();
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "100");
        clean();
    }

    fn debris(lock_path: &Path) -> Vec<String> {
        let Some(dir) = lock_path.parent() else {
            return Vec::new();
        };
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.contains(".reclaim.") || n.contains(".new."))
                    .collect()
            })
            .unwrap_or_default()
    }
}
