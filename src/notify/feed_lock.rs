//! The feed's cross-process mutex — **a thin adapter over
//! [`project_lock`](super::project_lock), which is THE implementation.**
//!
//! WHY A TWIN AND NOT A LOCK OF OUR OWN. The family conversation feed
//! (`.casa/group-feed.jsonl`) has more than one writer in more than one OS
//! process: the gateway appends kiosk/agent rows and rotates the file, and this
//! Rust listener appends every inbound group message and every mirrored reply.
//! Rotation is a multi-step transaction over the same bytes, so an append that
//! lands between two of its steps is destroyed outright — present in neither the
//! archive nor the live file. A lost family message is not cosmetic: it is a
//! message someone said that the house then denies ever hearing.
//!
//! WHY THIS FILE IS NOW FOUR FACTS AND NO PROTOCOL. It used to carry its own copy
//! of the protocol, and the copy was the weakest of the four implementations:
//! no §4a ownership evidence, no directory `fsync`, and — after
//! `receipt-s1c-atomic` — the REJECTED removal, `rename(lock → reclaim)` followed
//! by a judgement of the moved inode. docs/42 §10 records what that cost on the
//! Node side: **a protocol bug had to be found twice and fixed twice, and it was
//! found twice and fixed once.** `feedLock.mjs` became a thin adapter over
//! `projectLock.mjs` for exactly that reason, and this module is the same collapse
//! on the Rust side (docs/42 §10, "what is still outstanding — on the RUST side
//! only"). A protocol change is made in `project_lock.rs`, once.
//!
//! What is left here is the feed's five facts — four of the protocol's, and one
//! (5) that is the caller's alone:
//!
//! 1. THE PATHNAME — `<dirname(feed)>/.conversation.lock`. It predates the
//!    `.casa/locks/<name>.lock` table and every existing feed writer on both sides
//!    already speaks it, so it is SUPPLIED to `project_lock` as an explicit
//!    `lock_path` rather than migrated (docs/42 §1). One lock per feed directory,
//!    covering the live feed, `.casa/archive/*`, the manifest and the rotation
//!    marker: there is deliberately no second lock for the archive, because the
//!    transaction spans both.
//! 2. THE RANK — `feed-rotation` (20). A week mutation can cause a feed write,
//!    never the reverse, so a holder may take this lock while holding
//!    `week-mutation` (rank 10) and never the other way round. Before the
//!    collapse this module had no rank at all, which meant the ordering invariant
//!    could not see a feed lock: it was proving an abstraction.
//! 3. THE DEFAULTS — a 1000 ms fail-closed wait and a 30 s report horizon.
//! 4. THE RESULT SHAPE its callers branch on — [`FeedLock`], [`Release`] and
//!    [`LockRefusal`], rather than the project lock's typed refusal.
//! 5. THE CALLER'S PATIENCE — [`DEFAULT_ATTEMPTS`]. How many whole budgets a
//!    production feed writer spends before it tells the family "not recorded".
//!    This is the ONE fact here that is NOT part of the protocol and has no Node
//!    twin: see [`acquire_retrying`].
//!
//! Everything else — staging + `link(2)` publication, the `fsync` of the record
//! AND of the containing directory (a refused `fsync` is a refused lock), the
//! §4a authority handle held open for the whole section with `flock(LOCK_EX)` on
//! it, §4b's pin-then-judge removal, and §5's "no reclaim, at any age, not even of
//! a provably dead owner" — is inherited by construction. The engine's feed lock
//! and the engine's week lock are now one protocol, in one file, matching the
//! gateway's one file. See `docs/42-cross-process-lock-protocol.md`.
//!
//! The lock is advisory: it binds only writers that take it.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use super::project_lock;

/// The lock file name, shared with the Node twin's `LOCK_NAME`.
pub const LOCK_NAME: &str = ".conversation.lock";

/// How long a taker waits before failing CLOSED, shared with the Node twin's
/// `DEFAULT_WAIT_MS`. Every honest critical section is a handful of synchronous
/// fs calls, so a full second is many times the worst honest hold.
pub const DEFAULT_WAIT_MS: u64 = 1000;

/// How many whole [`DEFAULT_WAIT_MS`] budgets a production feed writer spends
/// before it fails closed — the CALLER's patience, not the protocol's.
///
/// **THIS IS NOT A PROTOCOL CONSTANT AND MUST NOT BECOME ONE.** `DEFAULT_WAIT_MS`
/// is asserted equal on both sides of the cross-implementation gate
/// (`protocol_constants_match_the_node_twin` here, leg 4 of
/// `claw3d-bridge/test/feedLockRustTwin.test.mjs` there); raising it on the Rust
/// side alone is drift dressed up as a fix, which is exactly what `rust-feed-lock`
/// refused to do. How long ONE acquisition waits is a contract between the two
/// implementations. How many acquisitions a caller is willing to attempt before
/// giving the family a refusal is a product decision each caller makes for itself,
/// and neither side has to agree about it for the two to serialise correctly.
///
/// WHY THREE, MEASURED (`feed-lock-retry`, docs/42 §9). `wg telegram feed-write`
/// used to take this lock TWICE in sequence — the row and its receipt in one
/// section ([`super::casa_feed::append_entry_proving`]), then the audience record
/// in another ([`super::casa_audience::record_audience`]) — and each held section
/// read the whole live feed plus every archive segment. Six concurrent writers of
/// that shape were measured at **986 ms of the 1000 ms budget**: zero refusals in
/// one run and 73 % refusals in another, at the SAME offered load. Three attempts
/// buys ~3 s of patience while still ENDING; past that, "busy" has stopped being
/// the true answer and a refusal is the honest one.
///
/// **THE SHAPE THAT NUMBER WAS TAKEN ON IS GONE (`feed-lock-section`).** The
/// audience now rides in the row's section ([`super::casa_audience::record_audience_locked`])
/// and the history read happens outside the lock, so one `feed-write` enters ONE
/// section — asserted, not asserted-about, by [`distinct_acquisitions`]. The
/// budget stays at three: it is the answer to "how long may a writer wait for a
/// lock that is legitimately busy", which a shorter section makes rarer without
/// making it impossible.
pub const DEFAULT_ATTEMPTS: u32 = 3;

/// How old an attributable lock whose owner is provably dead must be before the
/// REPORT calls it stale rather than held — the Node adapter's 30 s horizon.
/// **Nothing acts on this** (docs/42 §5): no code path breaks a lock because of
/// its age.
pub const DEFAULT_STALE_MS: u64 = 30_000;

/// `<dirname(feed)>/.conversation.lock` (docs/42 §1).
pub fn lock_path_for(feed_path: &Path) -> PathBuf {
    feed_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LOCK_NAME)
}

// ─────────────────────────────────────────────────────────────────────────────
// How many times this process took the lock — the number `feed-lock-section`
// exists to bring down, made countable.
// ─────────────────────────────────────────────────────────────────────────────

thread_local! {
    /// Sections actually ENTERED by this writer: acquisitions that published a
    /// record of their own.
    static DISTINCT_ACQUISITIONS: Cell<u64> = const { Cell::new(0) };
    /// Nested frames that re-entered a lock this thread already held (docs/42
    /// §6). Counted apart because they are not a second section — they cost no
    /// `link(2)`, no `fsync` and no contention, and folding them into the number
    /// above would make a re-entrant helper look like a writer that took the
    /// lock twice.
    static REENTRANT_FRAMES: Cell<u64> = const { Cell::new(0) };
}

/// How many DISTINCT feed-lock sections THIS WRITER has entered so far.
///
/// **WHY IT IS ALWAYS COMPILED AND NOT `cfg(test)`.** The thing worth asserting
/// is what the real `wg telegram feed-write` does, and `src/commands/telegram.rs`
/// lives in the BINARY crate while this module lives in the library: a
/// `cfg(test)` counter here is simply absent when the binary's own tests run, so
/// the only reachable assertion would be an in-library look-alike of the seam
/// rather than the seam itself. It is one non-atomic increment per acquisition —
/// unmeasurable beside the `fsync`s that same acquisition performs — and it buys
/// a claim a refactor cannot quietly undo: one `feed-write` enters ONE section.
///
/// **WHY PER-THREAD AND NOT PER-PROCESS.** A "writer" is a call stack, not an
/// image: the listener runs many of them at once, so a process-wide total would
/// add up unrelated writers' sections and could never answer "how many did THIS
/// one take". It is the same reason the protocol keys re-entrancy per thread
/// (docs/42 §6). For the one-shot `wg telegram feed-write` process the two
/// readings are the same number, which is what the printed
/// `feedLockAcquisitions=` line reports. It also makes the number measurable in
/// a test binary at all — process-global, every concurrently running test's
/// locks land in the same counter and the assertion is a coin flip.
pub fn distinct_acquisitions() -> u64 {
    DISTINCT_ACQUISITIONS.with(Cell::get)
}

/// How many nested re-entrant frames this writer has opened — see
/// [`distinct_acquisitions`] for why the two are counted apart.
pub fn reentrant_frames() -> u64 {
    REENTRANT_FRAMES.with(Cell::get)
}

/// Zero both counters, so a test can measure ONE seam rather than the whole
/// thread's history.
pub fn reset_acquisition_counters() {
    DISTINCT_ACQUISITIONS.with(|c| c.set(0));
    REENTRANT_FRAMES.with(|c| c.set(0));
}

fn count_acquisition(lock: &project_lock::ProjectLock) {
    let counter = if lock.is_reentrant() {
        &REENTRANT_FRAMES
    } else {
        &DISTINCT_ACQUISITIONS
    };
    counter.with(|c| c.set(c.get() + 1));
}

/// Why an acquisition did not happen. Every variant means the caller's work did
/// NOT run and the feed was not touched (docs/42 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockRefusal {
    /// The lock was held for the whole wait. The honest, retryable outcome.
    Timeout,
    /// We could not stage a COMPLETE, DURABLE owner record (EIO on either
    /// `fsync`, ENOSPC, a stalled short write). Only our own staging file was
    /// touched; the lock path was not.
    RecordWriteFailed(String),
    /// The lock directory could not be created or written in.
    Io(String),
    /// The lock file on disk is unbreakable and needs a HUMAN (docs/42 §5):
    /// ownerless, truncated, a foreign protocol version, another machine's, or an
    /// ancient record whose owner is provably dead. Retrying cannot help, and
    /// nothing is ever removed automatically. The message names the file to remove.
    Unavailable(String),
}

impl std::fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockRefusal::Timeout => write!(
                f,
                "the conversation feed lock was held for the whole wait — refusing to write unserialised"
            ),
            LockRefusal::RecordWriteFailed(m) => {
                write!(f, "could not stage a complete lock record: {m}")
            }
            LockRefusal::Io(m) => write!(f, "conversation feed lock io: {m}"),
            LockRefusal::Unavailable(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for LockRefusal {}

fn refusal_of(refusal: project_lock::LockRefusal) -> LockRefusal {
    let text = refusal.to_string();
    match refusal.detail() {
        // Held for the whole budget, at any age and whoever holds it.
        "held" | "timeout" => LockRefusal::Timeout,
        "record-write-failed" => LockRefusal::RecordWriteFailed(text),
        "create-failed" => LockRefusal::Io(text),
        // unattributable / foreign-host / stale-unrecovered / release-pending /
        // an ordering inversion: all of them fail CLOSED and none of them is a
        // "try again in a moment".
        _ => LockRefusal::Unavailable(text),
    }
}

/// What a release PROVED. Ownership is dropped only on proof (docs/42 §4).
///
/// See [`super::project_lock::Completed`]: this verdict is returned to the
/// caller now, not dropped on the floor of the wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    /// The unlink provably hit the inode carrying our token.
    Released,
    /// A successor owns the lock now, and we touched NOTHING: under pin-then-judge
    /// a record that is not ours is left exactly where it is, because nothing of a
    /// successor's is ever moved. A slow holder can lose the lock; it can never
    /// take its successor's lock away.
    Stolen,
    /// The lock path was already empty and stayed empty.
    Gone,
    /// An inner frame of a re-entrant acquisition exited; the outermost holder
    /// still owns the lock (docs/42 §6).
    Reentrant,
    /// A transient failure. We STILL OWN the lock and may call release again.
    Retained(String),
}

impl project_lock::ReleaseVerdict for Release {
    fn is_proven(&self) -> bool {
        !matches!(self, Release::Retained(_))
    }
}

fn release_of(release: project_lock::Release) -> Release {
    match release {
        project_lock::Release::Released => Release::Released,
        project_lock::Release::Stolen => Release::Stolen,
        project_lock::Release::Gone => Release::Gone,
        project_lock::Release::Reentrant => Release::Reentrant,
        project_lock::Release::Retained(r) => Release::Retained(r),
    }
}

/// A held lock. Carries the unforgeable token the shared implementation compares
/// against the record on the PINNED inode.
#[derive(Debug)]
pub struct FeedLock {
    inner: Option<project_lock::ProjectLock>,
}

impl FeedLock {
    /// The holder's token — 32 lowercase hex characters.
    pub fn token(&self) -> &str {
        self.inner.as_ref().map(|l| l.token()).unwrap_or("")
    }

    /// The file this lock lives at.
    pub fn path(&self) -> &Path {
        self.inner
            .as_ref()
            .map(|l| l.path())
            .unwrap_or_else(|| Path::new(""))
    }

    /// Release, per docs/42 §4b: pin then judge.
    pub fn release(mut self) -> Release {
        match self.inner.take() {
            Some(lock) => release_of(lock.release()),
            None => Release::Gone,
        }
    }
}

// A lock leaked by a panic would wedge the family conversation until a human
// cleared it (§5 forbids anyone else from clearing it), so `ProjectLock`'s own
// `Drop` releases it — including the retry and the retained-ownership rule. There
// is nothing for this adapter to add.

/// Acquire the feed lock, or fail CLOSED after `wait_ms` (docs/42 §2, §3).
///
/// Never reclaims: a lock held by a dead process, an ownerless file and a
/// truncated record all fail closed with their own reason, and the cure is a
/// human deleting the file (docs/42 §5).
pub fn acquire(feed_path: &Path, wait_ms: u64) -> Result<FeedLock, LockRefusal> {
    let lock_path = lock_path_for(feed_path);
    // The feed directory is the root this lock is scoped to. It is never used to
    // BUILD the pathname — that is supplied — but it keeps the (root, name)
    // identity honest: two households have two feed directories and therefore two
    // locks, never one.
    let root = feed_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let opts = project_lock::Options {
        wait_ms,
        stale_ms: DEFAULT_STALE_MS,
        lock_path: Some(lock_path),
        ..project_lock::Options::default()
    };
    match project_lock::acquire(&root, project_lock::FEED_ROTATION, &opts) {
        Ok(inner) => {
            count_acquisition(&inner);
            Ok(FeedLock { inner: Some(inner) })
        }
        Err(e) => Err(refusal_of(e)),
    }
}

/// Acquire, spending up to `attempts` whole `wait_ms` budgets on a lock that is
/// merely BUSY — the production feed writers' entry point (docs/42 §9).
///
/// WHAT MAKES THIS SAFE, AND WHY IT IS NOT A WEAKENING OF ANYTHING.
///
/// 1. **Only [`LockRefusal::Timeout`] is retried.** docs/42 §3 defines that
///    variant as "the lock was held for the whole wait": our body never ran and
///    the feed was not touched. There is nothing on disk for a second attempt to
///    duplicate. Every other refusal returns on the FIRST attempt, unchanged and
///    without spending another millisecond of the family's time —
///    [`LockRefusal::Unavailable`] is the "a human must remove this file" answer
///    (§5) and retrying it is a wedge, [`LockRefusal::RecordWriteFailed`] and
///    [`LockRefusal::Io`] are local failures where retrying cannot help.
/// 2. **The section still cannot run twice.** The retry is around the
///    ACQUISITION only; [`with_feed_lock_retrying`] still takes its body as
///    `FnOnce`, so "the row was written twice" is a compile error rather than a
///    discipline. This is deliberately not a loop around the whole write.
/// 3. **A refusal is still a refusal.** When the budget is spent the caller gets
///    back the same `Timeout` it would have got before, and writes nothing. The
///    fail-closed rule is untouched; only the number of times we ask is new.
/// 4. **It adds no contention.** The inner acquire already polls every 4 ms for
///    its whole budget, so a further attempt does not knock harder — it only
///    keeps knocking for longer. There is deliberately no back-off sleep between
///    attempts: the previous attempt was one unbroken second of polling, and
///    sleeping on top of it would be dead time in which a freed lock goes untaken.
///
/// A retry that SUCCEEDS is reported on stderr. A system that only stays up
/// because it retries must not look identical to one that never had to.
pub fn acquire_retrying(
    feed_path: &Path,
    wait_ms: u64,
    attempts: u32,
) -> Result<FeedLock, LockRefusal> {
    let attempts = attempts.max(1);
    let mut spent = 0u32;
    loop {
        spent += 1;
        match acquire(feed_path, wait_ms) {
            Ok(lock) => {
                if spent > 1 {
                    eprintln!(
                        "[{}] casa feed: the conversation lock was busy for {} of {} attempts \
                         ({} ms each) — the write landed on attempt {spent}, nothing was lost.",
                        chrono::Utc::now().format("%H:%M:%S"),
                        spent - 1,
                        attempts,
                        wait_ms,
                    );
                }
                return Ok(lock);
            }
            // BUSY, and we can still afford to ask again. Nothing was written:
            // §3 guarantees the body did not run.
            Err(LockRefusal::Timeout) if spent < attempts => continue,
            // Either the patience is spent — and the family is told the same
            // typed refusal as before — or this is not a "try again in a moment"
            // at all and never was.
            Err(refusal) => return Err(refusal),
        }
    }
}

/// Run `f` while holding the lock (docs/42 §7: nothing is finished up
/// afterwards). The lock is released before the result is handed back.
///
/// `f` receives the held [`FeedLock`] as a WITNESS: a function that must run
/// inside the section (appending the receipt that proves the row we just wrote)
/// takes `&FeedLock` as a parameter, so it cannot be called from outside the
/// transaction.
/// The release verdict is PROPAGATED, not dropped. `lock.release();` on its own
/// line discarded it, so a retained release — the lock path still occupied, no
/// proof we let go — returned `Ok(out)` exactly like a clean one. See
/// [`super::project_lock::Completed`] for why this is a success the caller can
/// interrogate rather than an `Err`.
pub fn with_feed_lock<T>(
    feed_path: &Path,
    wait_ms: u64,
    f: impl FnOnce(&FeedLock) -> T,
) -> Result<super::project_lock::Completed<T, Release>, LockRefusal> {
    let lock = acquire(feed_path, wait_ms)?;
    let out = f(&lock);
    let release = lock.release();
    Ok(super::project_lock::Completed { out, release })
}

/// [`with_feed_lock`], but spending up to `attempts` budgets on a merely BUSY
/// lock — see [`acquire_retrying`] for why that is safe and what it is not.
///
/// `f` is still `FnOnce`. The retry is around the acquisition, never around the
/// section, so this wrapper CANNOT run the caller's write twice: the compiler
/// enforces it, and the family's row cannot be duplicated by a lock policy.
pub fn with_feed_lock_retrying<T>(
    feed_path: &Path,
    wait_ms: u64,
    attempts: u32,
    f: impl FnOnce(&FeedLock) -> T,
) -> Result<super::project_lock::Completed<T, Release>, LockRefusal> {
    let lock = acquire_retrying(feed_path, wait_ms, attempts)?;
    let out = f(&lock);
    let release = lock.release();
    Ok(super::project_lock::Completed { out, release })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn feed(dir: &tempfile::TempDir) -> PathBuf {
        let casa = dir.path().join(".casa");
        std::fs::create_dir_all(&casa).unwrap();
        casa.join("group-feed.jsonl")
    }

    fn token_of(record: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(record.trim()).ok()?;
        value
            .get("token")
            .and_then(|t| t.as_str())
            .map(str::to_string)
    }

    /// A SECOND WRITER, and not a nested frame of this one. Re-entrancy is keyed
    /// per (thread, resolved path) (docs/42 §6), so a "second taker" driven from
    /// this call stack would be recognised as the SAME writer re-entering — which
    /// is correct behaviour and a useless test. Contention is taken on a fresh
    /// thread, where the only thing the two writers share is the file.
    fn acquire_elsewhere(feed_path: &Path, wait_ms: u64) -> Result<Release, LockRefusal> {
        let feed_path = feed_path.to_path_buf();
        std::thread::spawn(move || acquire(&feed_path, wait_ms).map(FeedLock::release))
            .join()
            .unwrap()
    }

    fn hold_elsewhere(
        feed_path: &Path,
        wait_ms: u64,
    ) -> (
        String,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<Release>,
    ) {
        let feed_path = feed_path.to_path_buf();
        let (tok_tx, tok_rx) = std::sync::mpsc::channel::<String>();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let lock = acquire(&feed_path, wait_ms).expect("the other writer must acquire");
            tok_tx.send(lock.token().to_string()).unwrap();
            let _ = wait.recv();
            lock.release()
        });
        let token = tok_rx.recv().expect("the other writer must acquire");
        (token, go, join)
    }

    /// A holder that lets go partway through the caller's patience. The taker
    /// with the PRE-CHANGE shape — `attempts = 1`, which is byte-for-byte what
    /// `acquire` alone did — is refused; the production shape lands.
    ///
    /// THIS IS THE PERMANENT MUTATION CONTROL for `feed-lock-retry`. It is not a
    /// mutation switch that turns a check off: `attempts = 1` is a real, callable,
    /// supported configuration that reproduces the exact build this task changed,
    /// so the gate has a build in which it fails permanently rather than only on
    /// the afternoon it was written. Short budgets, because what is being proved
    /// is the SHAPE of the loop, not the wall clock.
    #[test]
    #[serial(project_lock)]
    fn one_attempt_is_the_pre_change_build_and_it_loses_the_write_that_three_lands() {
        let wait = 120u64;
        let hold = 200u64; // longer than ONE budget, shorter than three.

        for (attempts, expected_landed) in [(1u32, false), (3u32, true)] {
            let dir = scratch();
            let feed = feed(&dir);
            let (ready, held) = std::sync::mpsc::channel::<()>();
            let holder = {
                let feed = feed.clone();
                std::thread::spawn(move || {
                    let lock = acquire(&feed, 1000).expect("the other writer must acquire");
                    ready.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(hold));
                    lock.release()
                })
            };
            held.recv().expect("the other writer must acquire");

            let landed = match acquire_retrying(&feed, wait, attempts) {
                Ok(lock) => {
                    lock.release();
                    true
                }
                Err(LockRefusal::Timeout) => false,
                Err(other) => panic!("expected a busy lock, got {other:?}"),
            };
            holder.join().unwrap();
            assert_eq!(
                landed, expected_landed,
                "attempts={attempts} against a {hold}ms hold on a {wait}ms budget"
            );
        }
    }

    /// A REFUSAL A HUMAN HAS TO CLEAR IS NOT RETRIED, AND COSTS NO TIME.
    ///
    /// docs/42 §5: an ownerless lock file is never removed by anyone but a person,
    /// so asking again is a wedge, not patience — it would multiply the family's
    /// wait by the attempt count and then give the same answer. The refusal must
    /// come back on the FIRST attempt, unchanged.
    #[test]
    #[serial(project_lock)]
    fn only_a_timeout_is_retried_an_unavailable_lock_refuses_on_the_first_attempt() {
        let dir = scratch();
        let feed = feed(&dir);
        // Ownerless: present, but carrying no parseable owner record. The
        // classifier calls this "unattributable", which maps to `Unavailable`.
        std::fs::write(lock_path_for(&feed), "not a lock record\n").unwrap();

        let wait = 300u64;
        let attempts = 4u32;
        let started = std::time::Instant::now();
        let refusal = acquire_retrying(&feed, wait, attempts).unwrap_err();
        let spent = started.elapsed();

        assert!(
            matches!(refusal, LockRefusal::Unavailable(_)),
            "an ownerless lock is a human's job, not a retry: got {refusal:?}"
        );
        assert!(
            spent < std::time::Duration::from_millis(wait * 2),
            "the caller spent {spent:?} on a refusal that retrying cannot help — it retried \
             something it must never retry"
        );
        // And the file was NOT touched: §5 removes nothing, at any attempt count.
        assert_eq!(
            std::fs::read_to_string(lock_path_for(&feed)).unwrap(),
            "not a lock record\n"
        );
    }

    /// The caller's patience is the CALLER's, and the wait is the PROTOCOL's.
    ///
    /// If `DEFAULT_ATTEMPTS` ever migrates into the block above this one, it has
    /// become something the Node twin must agree about — and it is not, because
    /// how many times a writer asks changes nothing about how two processes
    /// serialise. This test exists to make that boundary a thing someone has to
    /// delete on purpose.
    #[test]
    #[serial(project_lock)]
    fn the_retry_budget_is_a_caller_policy_and_the_wait_is_the_protocol() {
        assert_eq!(
            DEFAULT_WAIT_MS, 1000,
            "the PROTOCOL constant, asserted equal by the Node twin — a retry budget is not a \
             licence to raise it"
        );
        assert!(
            DEFAULT_ATTEMPTS >= 2,
            "a single attempt is the pre-change build: see \
             one_attempt_is_the_pre_change_build_and_it_loses_the_write_that_three_lands"
        );
        assert!(
            DEFAULT_ATTEMPTS <= 5,
            "patience that is not bounded is a wedge with a nicer name"
        );
    }

    /// `attempts = 0` is a caller bug, and it is read as "ask once" rather than
    /// "do not take the lock and carry on anyway". Fail-closed has no zero.
    #[test]
    #[serial(project_lock)]
    fn zero_attempts_still_asks_once_it_never_skips_the_lock() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock = acquire_retrying(&feed, 200, 0).expect("a free lock is taken");
        assert!(lock_path_for(&feed).exists(), "the lock was actually taken");
        assert_eq!(lock.release(), Release::Released);
    }

    /// The two processes can only serialise through ONE protocol. If either of
    /// these drifts from `feedLock.mjs`, the gateway and the engine are taking
    /// two different locks and neither of them is a mutex.
    #[test]
    #[serial(project_lock)]
    fn protocol_constants_match_the_node_twin() {
        assert_eq!(LOCK_NAME, ".conversation.lock");
        assert_eq!(DEFAULT_WAIT_MS, 1000);
        assert_eq!(DEFAULT_STALE_MS, 30_000);
        let dir = scratch();
        let feed = feed(&dir);
        assert_eq!(lock_path_for(&feed), feed.parent().unwrap().join(LOCK_NAME));
        // THE RANK, which this module had no way to declare before the collapse:
        // the ordering invariant could not see a feed lock at all.
        assert_eq!(project_lock::rank_of(project_lock::FEED_ROTATION), Some(20));
        assert!(
            project_lock::rank_of(project_lock::FEED_ROTATION)
                > project_lock::rank_of(project_lock::WEEK_MUTATION),
            "a week mutation can cause a feed write, never the reverse"
        );
    }

    /// The record is the shared §2 line, byte-identical to what `projectLock.mjs`
    /// (and therefore `feedLock.mjs`) writes — including the trailing newline and
    /// the fixed field order. Before the collapse this module wrote its own
    /// newline-less variant.
    #[test]
    #[serial(project_lock)]
    fn the_owner_record_is_the_documented_line_with_a_trailing_newline() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        let body = std::fs::read_to_string(lock_path_for(&feed)).unwrap();

        assert!(body.ends_with('\n'), "trailing newline: {body:?}");
        assert_eq!(body.matches('\n').count(), 1, "ONE line: {body:?}");
        assert!(
            body.starts_with("{\"v\":1,\"token\":\""),
            "field order is fixed: {body:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["pid"].as_i64().unwrap(), std::process::id() as i64);
        assert!(parsed["host"].is_string());
        assert!(parsed["acquiredMs"].is_i64());
        let token = parsed["token"].as_str().unwrap();
        assert_eq!(token.len(), 32, "16 crypto-random bytes as hex");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_eq!(token, lock.token());
        lock.release();
    }

    #[test]
    #[serial(project_lock)]
    fn a_second_taker_is_refused_while_the_lock_is_held_and_succeeds_after_release() {
        let dir = scratch();
        let feed = feed(&dir);
        let first = acquire(&feed, DEFAULT_WAIT_MS).unwrap();

        // FAIL CLOSED (§3): the second taker does not get the lock, and its
        // caller's work therefore does not run.
        assert_eq!(
            acquire_elsewhere(&feed, 20).unwrap_err(),
            LockRefusal::Timeout
        );

        assert_eq!(first.release(), Release::Released);
        assert_eq!(
            acquire_elsewhere(&feed, DEFAULT_WAIT_MS),
            Ok(Release::Released)
        );
    }

    /// docs/42 §5, the decision the twin may not soften. A lock whose owner is
    /// long dead is STILL not reclaimed — not after any delay, and not from an
    /// ownerless or truncated file. Every one of these fails closed, is still on
    /// disk afterwards, and names the file a human must remove.
    #[test]
    #[serial(project_lock)]
    fn a_stale_ownerless_or_truncated_lock_is_never_reclaimed() {
        for record in [
            // A provably dead owner: pid 0 is not a live process, and the
            // record is ancient.
            r#"{"v":1,"token":"00112233445566778899aabbccddeeff","pid":0,"host":"gone","acquiredMs":1}"#,
            // Ownerless.
            r#"{"v":1,"pid":0,"host":"gone","acquiredMs":1}"#,
            // Truncated mid-record.
            r#"{"v":1,"token":"0011223344"#,
            // Empty.
            "",
        ] {
            let dir = scratch();
            let feed = feed(&dir);
            let lock_path = lock_path_for(&feed);
            std::fs::write(&lock_path, record).unwrap();

            let refused = acquire(&feed, 20).unwrap_err();
            assert!(
                !matches!(refused, LockRefusal::Timeout),
                "record {record:?} is unbreakable and needs a HUMAN, not a retry: {refused}"
            );
            assert!(
                refused
                    .to_string()
                    .contains(&lock_path.display().to_string()),
                "the report must name the file a human removes: {refused}"
            );
            assert_eq!(
                std::fs::read_to_string(&lock_path).unwrap(),
                record,
                "acquisition CLASSIFIES and REPORTS; it never removes"
            );
        }
    }

    /// The single rule that makes a slow holder safe (docs/42 §4). A holder whose
    /// lock was cleared and retaken by a successor must NOT delete, move or
    /// quarantine the successor's lock.
    #[test]
    #[serial(project_lock)]
    fn a_slow_holder_never_takes_its_successors_lock() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock_path = lock_path_for(&feed);
        let slow = acquire(&feed, DEFAULT_WAIT_MS).unwrap();

        // A human cleared the wedge and a successor took the lock while this
        // holder was slow.
        std::fs::remove_file(&lock_path).unwrap();
        let (successor_token, go, join) = hold_elsewhere(&feed, DEFAULT_WAIT_MS);
        let successor_record = std::fs::read_to_string(&lock_path).unwrap();
        assert_ne!(successor_token, slow.token());

        assert_eq!(slow.release(), Release::Stolen);
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            successor_record,
            "the successor's record was never touched — BYTE FOR BYTE"
        );
        // Nothing was moved, so there is no debris of any kind: no quarantine, no
        // staging file, and no pin left behind.
        let debris: Vec<_> = std::fs::read_dir(lock_path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".reclaim.") || n.contains(".new.") || n.contains(".pin."))
            .collect();
        assert!(debris.is_empty(), "unexpected debris: {debris:?}");

        let _ = go.send(());
        assert_eq!(join.join().unwrap(), Release::Released);
    }

    /// A release that cannot be VERIFIED retains ownership so the holder can try
    /// again — it does not silently declare success.
    #[test]
    #[serial(project_lock)]
    fn releasing_a_lock_that_was_never_ours_reports_gone_not_released() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        std::fs::remove_file(lock_path_for(&feed)).unwrap();
        assert_eq!(lock.release(), Release::Gone);
    }

    /// §2: the visible transition is "no lock" → "a whole, parseable lock".
    /// There is no instant at which the lock path holds a partial record.
    #[test]
    #[serial(project_lock)]
    fn the_published_lock_is_never_a_partial_record() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock_path = lock_path_for(&feed);
        let observer = lock_path.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_reader = stop.clone();
        // Hammer the path while acquisitions come and go; every sighting must be
        // a complete record.
        let watcher = std::thread::spawn(move || {
            while !stop_reader.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(body) = std::fs::read_to_string(&observer) {
                    if !body.is_empty() {
                        assert!(
                            token_of(&body).is_some(),
                            "a partial record was visible at the lock path: {body:?}"
                        );
                    }
                }
            }
        });
        for _ in 0..200 {
            acquire(&feed, DEFAULT_WAIT_MS).unwrap().release();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        watcher.join().unwrap();
    }

    /// Debris of a publication that did not complete: the staging file, a pin, a
    /// detach name, a quarantine. A refused acquisition leaves the directory
    /// exactly as it found it.
    fn debris(lock_path: &Path) -> Vec<String> {
        let Some(dir) = lock_path.parent() else {
            return Vec::new();
        };
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| {
                        n.contains(".reclaim.")
                            || n.contains(".new.")
                            || n.contains(".pin.")
                            || n.contains(".detach.")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// **§2 STEP 5, THROUGH THE FEED'S PUBLIC SURFACE** (task `rust-feed-lock`).
    ///
    /// The Node side has exactly this gate, and it is named for the DEFECT rather
    /// than for the implementation precisely so a refactor cannot quietly drop it:
    /// `test_feedlock_fsync_failure_fails_closed`, `EIO` injected on the record
    /// `fsync` and, separately, on the directory `fsync` only, plus a healthy
    /// control. docs/42 §10 requires it to "keep passing through the Rust one —
    /// whatever is behind `acquireFeedLock` at the time", and until now the engine
    /// had no answer to it: `project_lock`'s own boundary negative
    /// (`injected_fsync_eio_publishes_nothing_and_runs_nothing`) drives the WEEK
    /// pathname through `with_project_lock`, so a feed adapter that stopped
    /// inheriting the barrier — by re-copying the protocol, as this module used to
    /// carry it, or by routing somewhere else — would not redden a single engine
    /// test. "Inherited by construction" is a claim about the code; this is the
    /// assertion.
    ///
    /// A link whose directory entry never reached the platter is a lock the next
    /// boot cannot see and one the holder believes it has, so a refused `fsync` —
    /// the record's or the directory's — is a REFUSED LOCK: the feed writer's body
    /// does not run, nothing is published at the lock path, and no debris is left.
    ///
    /// Its teeth are `the_pre_fix_twin_without_step_5_publishes_the_lock_anyway`
    /// below, which restores the build this task was filed against.
    #[test]
    #[serial(project_lock)]
    fn an_injected_fsync_eio_publishes_no_feed_lock_and_runs_no_body() {
        for (flag, label) in [
            (&project_lock::inject::FAIL_RECORD_FSYNC, "record fsync"),
            (&project_lock::inject::FAIL_DIR_FSYNC, "directory fsync"),
        ] {
            project_lock::inject::reset();
            let dir = scratch();
            let feed = feed(&dir);
            let lock_path = lock_path_for(&feed);
            project_lock::inject::arm(flag);

            let mut ran = false;
            let refused = with_feed_lock(&feed, 30, |_| ran = true).unwrap_err();

            assert!(
                !ran,
                "{label} EIO must not run the feed write — an unserialised append tears the feed"
            );
            assert!(
                matches!(refused, LockRefusal::RecordWriteFailed(_)),
                "{label}: a durability failure is a refusal the caller can SEE, not a timeout \
                 and not a success: {refused:?}"
            );
            assert!(
                !lock_path.exists(),
                "{label}: NOTHING is published — a lock whose directory entry is not on the \
                 device is a lock the next boot cannot see and one the holder believes it has"
            );
            assert!(
                debris(&lock_path).is_empty(),
                "{label}: {:?}",
                debris(&lock_path)
            );

            // THE HEALTHY CONTROL, on the same fixture: the refusal above is the
            // injected EIO and not a fixture that cannot take the lock at all.
            project_lock::inject::reset();
            let mut healthy = false;
            let ok = with_feed_lock(&feed, DEFAULT_WAIT_MS, |_| healthy = true).unwrap();
            assert!(healthy, "{label}: the control body must run");
            assert_eq!(ok.release, Release::Released, "{label}");
            assert!(!lock_path.exists(), "{label}");
        }
        project_lock::inject::reset();
    }

    /// **THE CONTROL — a gate whose control cannot fail is not a gate.**
    ///
    /// `SKIP_DIR_FSYNC` restores the build task `rust-feed-lock` was filed
    /// against: step 5 is not attempted at all, which is what this module did
    /// before the collapse (`stage_record` synced the RECORD and `try_publish`
    /// linked it into place with no `File::open(parent)` + `sync_all()` after).
    /// On that build the directory `fsync` cannot refuse, so the same fixture that
    /// fails CLOSED above ACQUIRES — the assertion is not vacuous, and it is not
    /// passing because of the record `fsync` doing both halves' work.
    #[test]
    #[serial(project_lock)]
    fn the_pre_fix_twin_without_step_5_publishes_the_lock_anyway() {
        project_lock::inject::reset();
        let dir = scratch();
        let feed = feed(&dir);
        let lock_path = lock_path_for(&feed);

        project_lock::inject::arm(&project_lock::inject::SKIP_DIR_FSYNC);
        project_lock::inject::arm(&project_lock::inject::FAIL_DIR_FSYNC);

        let mut ran = false;
        let published = with_feed_lock(&feed, DEFAULT_WAIT_MS, |_| ran = true);
        project_lock::inject::reset();

        assert!(
            published.is_ok() && ran,
            "the pre-fix twin has no barrier to refuse, so it publishes and runs — if this \
             starts failing, the negative above has stopped being a test of step 5"
        );
        assert_eq!(published.unwrap().release, Release::Released);
        assert!(!lock_path.exists(), "released, so nothing is left behind");
    }

    #[test]
    #[serial(project_lock)]
    fn with_feed_lock_runs_the_body_and_releases() {
        let dir = scratch();
        let feed = feed(&dir);
        let out = with_feed_lock(&feed, DEFAULT_WAIT_MS, |_| 42).unwrap();
        assert_eq!(out.verified().unwrap(), 42);
        assert!(
            !lock_path_for(&feed).exists(),
            "the lock is released before the result is handed back"
        );
    }

    /// §3 again, at the API the writers actually call: a held lock means the
    /// closure NEVER RUNS. A fail-open twin would run it and corrupt the feed.
    #[test]
    #[serial(project_lock)]
    fn with_feed_lock_does_not_run_the_body_when_it_cannot_serialise() {
        let dir = scratch();
        let feed = feed(&dir);
        let (_token, go, join) = hold_elsewhere(&feed, DEFAULT_WAIT_MS);
        let mut ran = false;
        let refused = with_feed_lock(&feed, 20, |_| ran = true).unwrap_err();
        assert_eq!(refused, LockRefusal::Timeout);
        assert!(!ran, "the body must NOT run when the lock was not acquired");
        let _ = go.send(());
        join.join().unwrap();
    }

    /// Two threads, one lock, many appends: the mutex actually excludes. This is
    /// the property every derived global feed id depends on.
    #[test]
    #[serial(project_lock)]
    fn concurrent_writers_never_interleave_inside_the_section() {
        let dir = scratch();
        let feed = feed(&dir);
        let counter = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let feed = feed.clone();
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    with_feed_lock(&feed, 10_000, |_| {
                        // Inside the section: read, pause, write. Without real
                        // exclusion this read-modify-write loses updates.
                        let mut seen = counter.lock().unwrap();
                        let next = seen.len() as i32;
                        std::thread::yield_now();
                        seen.push(next);
                    })
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let seen = counter.lock().unwrap();
        assert_eq!(seen.len(), 100);
        // Each entry recorded the length at the instant it ran: under real
        // exclusion that is exactly 0..100 with no repeats.
        assert_eq!(*seen, (0..100).collect::<Vec<i32>>());
    }

    /// THE COLLAPSE, ASSERTED. The feed lock is the same protocol object as the
    /// week lock: a third process taking `.conversation.lock` through
    /// `project_lock` directly is excluded by a holder that took it through this
    /// adapter, and the ordering invariant can finally SEE a held feed lock.
    #[test]
    #[serial(project_lock)]
    fn the_feed_lock_is_the_project_lock_under_another_pathname() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock_path = lock_path_for(&feed);
        let held = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        assert_eq!(held.path(), lock_path);

        // The shared implementation, driven straight at the feed's pathname from
        // another thread, is refused by the adapter's holder.
        let direct = {
            let root = dir.path().to_path_buf();
            let lock_path = lock_path.clone();
            std::thread::spawn(move || {
                project_lock::acquire(
                    &root,
                    project_lock::FEED_ROTATION,
                    &project_lock::Options {
                        wait_ms: 20,
                        lock_path: Some(lock_path),
                        ..project_lock::Options::default()
                    },
                )
                .map(|l| l.release())
            })
            .join()
            .unwrap()
        };
        assert_eq!(
            direct.unwrap_err().detail(),
            "held",
            "one protocol, one file — whichever door a writer comes through"
        );

        // And the ordering invariant now sees it: `week-mutation` (rank 10) inside
        // a held `feed-rotation` (rank 20) is a loud immediate error, where before
        // the collapse this module's lock was invisible to the check.
        let inverted = project_lock::acquire(
            dir.path(),
            project_lock::WEEK_MUTATION,
            &project_lock::Options::waiting(50),
        )
        .unwrap_err();
        assert_eq!(inverted.detail(), "order", "{inverted}");
        assert_eq!(held.release(), Release::Released);
    }
}
