//! The RUST TWIN of the gateway's cross-process feed mutex
//! (`claw3d-bridge/src/feedLock.mjs`, task `receipt-s1-lock`).
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
//! Two processes can only serialise against each other through ONE protocol, so
//! this module implements the gateway's, step for step, and deliberately adds
//! nothing. Where the two could drift they are pinned by
//! [`tests::protocol_constants_match_the_node_twin`] and by the shape assertions
//! on the owner record: a lock this module writes must be readable, and
//! respected, by `feedLock.mjs`, and vice versa.
//!
//! THE PROTOCOL, in the twin's own numbering:
//!
//! 1. LOCK PATH — `<dirname(feed)>/.conversation.lock`. One lock per feed
//!    directory, covering the live feed, `.casa/archive/*`, the manifest and the
//!    rotation marker. There is deliberately no second lock for the archive: the
//!    transaction spans both.
//!
//! 2. ACQUIRE — STAGE, VERIFY, THEN PUBLISH WITH `link(2)`. The owner record is
//!    one line of JSON carrying a 32-hex `token` of 16 crypto-random bytes (the
//!    holder's unforgeable identity — never derived from pid/time/host, or a
//!    successor could reconstruct the token of the holder it replaced), plus
//!    `pid`/`host` as EVIDENCE ONLY, never identity.
//!
//!    We create a PRIVATE staging file with `O_CREAT|O_EXCL`, write the record
//!    honouring every short write, fsync, close, read it back and compare it byte
//!    for byte — only a complete record may become a lock — and only then
//!    `link(staging, lockPath)`. That link is exactly as exclusive as `O_EXCL`
//!    (EEXIST when held) while making the visible transition go straight from
//!    "no lock" to "a whole, parseable lock". Creating the lock file first and
//!    writing into it afterwards cannot give that property: a reader between the
//!    two syscalls sees a truncated record, which §4 forbids anyone from ever
//!    breaking — a permanent wedge produced by a syscall that "succeeded".
//!
//!    Any failure before the link fails the ACQUIRE and removes only our own
//!    staging file. No failure path in acquisition ever touches the lock path.
//!
//! 3. FAIL CLOSED. Not acquired within `wait` ⇒ the caller's work does NOT run.
//!    There is no fail-open path here. An unserialised write is a silently
//!    corrupted feed; a refused write is a visible, retryable failure, and we
//!    take the visible one every time.
//!
//! 4. REMOVING A LOCK IS ALWAYS "DETACH THEN DECIDE". `read` → `compare` →
//!    `unlink` looks like a compare-and-swap and is not one: between the compare
//!    and the unlink another process can remove that lock and create its own, and
//!    the unlink then deletes A SUCCESSOR'S LOCK, handing a third writer the feed
//!    mid-transaction. The two operations touch a PATH; only one of them touches
//!    the inode we inspected.
//!
//!    So: `rename(lock, lock.reclaim.<hex>)` — one atomic syscall, the path is
//!    free and we hold a private handle on exactly one inode — then inspect ONLY
//!    that inode. Ours ⇒ unlink it (the removal provably hit the inode we
//!    judged). Not ours ⇒ we moved somebody else's live lock, so PUT IT BACK with
//!    `link` + `unlink` (`link` fails EEXIST rather than clobbering, so we can
//!    never overwrite a lock taken during the microsecond the path was empty) and
//!    report [`Release::Stolen`]. A slow holder can lose the lock; it can never
//!    take its successor's lock away.
//!
//!    Release RETAINS OWNERSHIP UNTIL RELEASE IS VERIFIED: a transient failure
//!    returns [`Release::Retained`] and the holder still owns the lock and may
//!    call release again. Dropping in-process state on a failed unlink would
//!    surrender the authority to retry and leave a lock file nobody will claim.
//!
//! 5. NO RECLAIM, AT ANY AGE. A stale, ownerless, truncated, foreign-host or
//!    provably-dead lock makes every writer FAIL CLOSED with a specific reason;
//!    the cure is a human deleting the file. This is the gateway's decision and
//!    the twin may not soften it — "reclaim it if the owner is dead" does not
//!    survive a third contender: freeing the authoritative pathname before
//!    knowing which inode you took has already published a lock nobody holds.
//!
//! 6. NO WORK OUTSIDE THE SECTION. Reading the feed, allocating the global id,
//!    appending, rotating — all of it happens inside the closure. Nothing is
//!    "finished up afterwards".
//!
//! The lock is advisory: it binds only writers that take it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The lock file name, shared with the Node twin's `LOCK_NAME`.
pub const LOCK_NAME: &str = ".conversation.lock";

/// How long a taker waits before failing CLOSED, shared with the Node twin's
/// `DEFAULT_WAIT_MS`. Every honest critical section is a handful of synchronous
/// fs calls, so a full second is many times the worst honest hold.
pub const DEFAULT_WAIT_MS: u64 = 1000;

/// How long to sleep between acquisition attempts (the twin's ~5 ms retry).
const RETRY_SLEEP_MS: u64 = 5;

/// How many times a release retries a TRANSIENT failure before handing the
/// problem back with ownership still retained (protocol §4).
const RELEASE_ATTEMPTS: u32 = 5;

/// `<dirname(feed)>/.conversation.lock` (protocol §1).
pub fn lock_path_for(feed_path: &Path) -> PathBuf {
    feed_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LOCK_NAME)
}

/// Why an acquisition did not happen. Every variant means the caller's work did
/// NOT run and the feed was not touched (protocol §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockRefusal {
    /// The lock was held for the whole wait. The honest, retryable outcome.
    Timeout,
    /// We could not stage a COMPLETE owner record (EIO, ENOSPC, a stalled short
    /// write). Only our own staging file was touched; the lock path was not.
    RecordWriteFailed(String),
    /// The lock directory could not be created or read.
    Io(String),
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
        }
    }
}

impl std::error::Error for LockRefusal {}

/// What a release PROVED. Ownership is dropped only on `Released` or `Stolen`
/// (protocol §4) — those are the two outcomes in which we can prove we no longer
/// hold the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    /// The unlink provably hit the inode carrying our token.
    Released,
    /// The record on the moved inode was NOT ours — a successor reclaimed the
    /// lock while we were slow. We put it back untouched.
    Stolen,
    /// The lock path was already empty and stayed empty.
    Gone,
    /// A transient failure. We STILL OWN the lock and may call release again.
    Retained(String),
    /// We moved a foreign record and could not put it back (the path was retaken
    /// in the gap). The quarantined file is left on disk as evidence — a lock
    /// record is never destroyed on a guess.
    Displaced(String),
}

/// A held lock. Carries the unforgeable token that release compares for EXACT
/// equality against the record on the moved inode.
#[derive(Debug)]
pub struct FeedLock {
    lock_path: PathBuf,
    token: String,
    released: bool,
}

impl FeedLock {
    /// The holder's token — 32 lowercase hex characters.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Release, per protocol §4: detach then decide.
    pub fn release(mut self) -> Release {
        let outcome = self.release_inner();
        // Ownership is dropped ONLY when release is VERIFIED. `Retained` keeps
        // `released == false` so the Drop guard tries again.
        if matches!(outcome, Release::Released | Release::Stolen | Release::Gone) {
            self.released = true;
        }
        outcome
    }

    fn release_inner(&self) -> Release {
        let mut last = String::new();
        for _ in 0..RELEASE_ATTEMPTS {
            match detach_and_decide(&self.lock_path, &self.token) {
                Release::Retained(reason) => last = reason,
                verdict => return verdict,
            }
            std::thread::sleep(std::time::Duration::from_millis(RETRY_SLEEP_MS));
        }
        Release::Retained(last)
    }
}

impl Drop for FeedLock {
    fn drop(&mut self) {
        // A lock leaked by a panic would wedge the family conversation until a
        // human cleared it (§5 forbids anyone else from clearing it). The same
        // detach-then-decide rule applies: we can only ever remove OUR inode.
        if !self.released {
            let _ = self.release_inner();
        }
    }
}

/// The owner record, one line of JSON. `pid`/`host` are EVIDENCE — they let a
/// later taker ask "is that process still alive on this machine?" — and are
/// never identity. Identity is `token`, and only `token`.
fn owner_record(token: &str) -> String {
    let host = hostname();
    let pid = std::process::id();
    let acquired_ms = chrono::Utc::now().timestamp_millis();
    // Key order matches the Node twin's literal so a human diffing two records
    // written by the two processes sees the same shape.
    format!(
        "{{\"v\":1,\"token\":\"{token}\",\"pid\":{pid},\"host\":{},\"acquiredMs\":{acquired_ms}}}",
        serde_json::Value::String(host)
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
/// it replaced (protocol §2).
fn mint_token() -> String {
    let mut buf = [0u8; 16];
    if getrandom::getrandom(&mut buf).is_err() {
        // A uuid v4 is OS entropy too, and fails the same way or not at all. A
        // time/pid fallback would silently make the token forgeable.
        buf.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    hex::encode(buf)
}

/// Read the `token` field out of an owner record. A record that does not parse,
/// or carries no token, reads as `None` — and a `None` token is never equal to
/// ours, so it is never removed.
fn token_of(record: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(record.trim()).ok()?;
    value
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// Protocol §4's primitive. Returns what was PROVED about the lock path.
fn detach_and_decide(lock_path: &Path, our_token: &str) -> Release {
    let quarantine = lock_path.with_extension(format!("reclaim.{}", mint_token()));
    // (a) ONE atomic syscall. The path is now free and we hold a private handle
    //     on exactly one inode.
    if let Err(e) = std::fs::rename(lock_path, &quarantine) {
        return if e.kind() == std::io::ErrorKind::NotFound {
            // Someone else got there first, or we never had it. We changed
            // nothing, and the lock is provably not ours to hold.
            Release::Gone
        } else {
            Release::Retained(e.to_string())
        };
    }
    // (b) Inspect ONLY the moved inode.
    let record = std::fs::read_to_string(&quarantine).unwrap_or_default();
    if token_of(&record).as_deref() == Some(our_token) {
        // (c) The removal provably hits the inode we judged, and nothing else.
        return match std::fs::remove_file(&quarantine) {
            Ok(()) => Release::Released,
            Err(e) => Release::Retained(e.to_string()),
        };
    }
    // (d) We moved somebody else's live lock. Put it back.
    match std::fs::hard_link(&quarantine, lock_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&quarantine);
            Release::Stolen
        }
        Err(e) => Release::Displaced(format!(
            "a foreign lock record was moved to {} and could not be restored: {e}",
            quarantine.display()
        )),
    }
}

/// Stage a complete owner record and PUBLISH it with `link(2)` (protocol §2).
/// `Ok(true)` = acquired. `Ok(false)` = the lock is held by someone else.
fn try_publish(lock_path: &Path, token: &str) -> Result<bool, LockRefusal> {
    let record = owner_record(token);
    // (a) a PRIVATE staging file, not the lock path.
    let staging = lock_path.with_extension(format!("new.{}", mint_token()));
    let staged = stage_record(&staging, record.as_bytes());
    let result = match staged {
        Ok(()) => {
            // (d) the acquisition itself: as exclusive as O_EXCL, but the
            //     visible transition is "no lock" → "a whole, parseable lock".
            match std::fs::hard_link(&staging, lock_path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(LockRefusal::Io(e.to_string())),
            }
        }
        Err(e) => Err(e),
    };
    // Unlink the staging file either way. This is the ONLY path in acquisition
    // that removes anything, and it removes only our own file.
    let _ = std::fs::remove_file(&staging);
    result
}

/// (b) write IN FULL honouring every short write, (c) fsync, close, read back
/// and compare byte for byte. Only a complete record may become a lock.
fn stage_record(staging: &Path, record: &[u8]) -> Result<(), LockRefusal> {
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LockRefusal::Io(e.to_string()))?;
    }
    let mut file = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(staging)
            .map_err(|e| LockRefusal::RecordWriteFailed(e.to_string()))?
    };
    // `write_all` is the short-write loop: it retries until every byte lands. A
    // short write does not raise an error; it silently publishes a truncation.
    file.write_all(record)
        .and_then(|_| file.sync_all())
        .map_err(|e| LockRefusal::RecordWriteFailed(e.to_string()))?;
    drop(file);

    let mut back = Vec::new();
    std::fs::File::open(staging)
        .and_then(|mut f| f.read_to_end(&mut back))
        .map_err(|e| LockRefusal::RecordWriteFailed(e.to_string()))?;
    if back != record {
        return Err(LockRefusal::RecordWriteFailed(format!(
            "the staged record read back as {} of {} bytes",
            back.len(),
            record.len()
        )));
    }
    Ok(())
}

/// Acquire the feed lock, or fail CLOSED after `wait_ms` (protocol §2, §3).
///
/// Never reclaims: a lock held by a dead process, an ownerless file and a
/// truncated record all produce [`LockRefusal::Timeout`], and the cure is a
/// human deleting the file (protocol §5).
pub fn acquire(feed_path: &Path, wait_ms: u64) -> Result<FeedLock, LockRefusal> {
    let lock_path = lock_path_for(feed_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| LockRefusal::Io(e.to_string()))?;
    }
    let token = mint_token();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    loop {
        if try_publish(&lock_path, &token)? {
            return Ok(FeedLock {
                lock_path,
                token,
                released: false,
            });
        }
        if std::time::Instant::now() >= deadline {
            return Err(LockRefusal::Timeout);
        }
        std::thread::sleep(std::time::Duration::from_millis(RETRY_SLEEP_MS));
    }
}

/// Run `f` while holding the lock (protocol §6: nothing is finished up
/// afterwards). The lock is released before the result is handed back.
pub fn with_feed_lock<T>(
    feed_path: &Path,
    wait_ms: u64,
    f: impl FnOnce() -> T,
) -> Result<T, LockRefusal> {
    let lock = acquire(feed_path, wait_ms)?;
    let out = f();
    lock.release();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn feed(dir: &tempfile::TempDir) -> PathBuf {
        let casa = dir.path().join(".casa");
        std::fs::create_dir_all(&casa).unwrap();
        casa.join("group-feed.jsonl")
    }

    /// The two processes can only serialise through ONE protocol. If either of
    /// these drifts from `feedLock.mjs`, the gateway and the engine are taking
    /// two different locks and neither of them is a mutex.
    #[test]
    fn protocol_constants_match_the_node_twin() {
        assert_eq!(LOCK_NAME, ".conversation.lock");
        assert_eq!(DEFAULT_WAIT_MS, 1000);
        let dir = scratch();
        let feed = feed(&dir);
        assert_eq!(lock_path_for(&feed), feed.parent().unwrap().join(LOCK_NAME));
    }

    #[test]
    fn the_owner_record_is_one_line_of_json_with_a_32_hex_token() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        let body = std::fs::read_to_string(lock_path_for(&feed)).unwrap();

        assert!(!body.contains('\n'), "the record is ONE line: {body:?}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["v"], 1);
        assert!(parsed["pid"].is_number());
        assert!(parsed["host"].is_string());
        assert!(parsed["acquiredMs"].is_i64());
        let token = parsed["token"].as_str().unwrap();
        assert_eq!(token.len(), 32, "16 crypto-random bytes as hex");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_eq!(token, lock.token());
        lock.release();
    }

    /// The token is the holder's UNFORGEABLE identity. If it were derived from
    /// pid/host/time, a successor could reconstruct the token of the holder it
    /// replaced and then release a lock it does not hold.
    #[test]
    fn tokens_are_never_derived_from_pid_time_or_host() {
        let seen: std::collections::HashSet<String> = (0..64).map(|_| mint_token()).collect();
        assert_eq!(seen.len(), 64, "every token is distinct");
        let pid = std::process::id().to_string();
        for token in &seen {
            assert!(!token.contains(&pid), "a token must not embed the pid");
        }
    }

    #[test]
    fn a_second_taker_is_refused_while_the_lock_is_held_and_succeeds_after_release() {
        let dir = scratch();
        let feed = feed(&dir);
        let first = acquire(&feed, DEFAULT_WAIT_MS).unwrap();

        // FAIL CLOSED (§3): the second taker does not get the lock, and its
        // caller's work therefore does not run.
        let refused = acquire(&feed, 20).unwrap_err();
        assert_eq!(refused, LockRefusal::Timeout);

        assert_eq!(first.release(), Release::Released);
        let second = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        assert_eq!(second.release(), Release::Released);
    }

    /// Protocol §5, the decision the twin may not soften. A lock whose owner is
    /// long dead is STILL not reclaimed — not after any delay, and not from an
    /// ownerless or truncated file. Every one of these fails closed.
    #[test]
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

            assert_eq!(
                acquire(&feed, 20).unwrap_err(),
                LockRefusal::Timeout,
                "record {record:?} must fail closed, never be reclaimed"
            );
            assert_eq!(
                std::fs::read_to_string(&lock_path).unwrap(),
                record,
                "acquisition CLASSIFIES and REPORTS; it never removes"
            );
        }
    }

    /// The single rule that makes a slow holder safe (§4). A holder whose lock
    /// was reclaimed by a successor must NOT delete the successor's lock.
    #[test]
    fn a_slow_holder_never_takes_its_successors_lock() {
        let dir = scratch();
        let feed = feed(&dir);
        let lock_path = lock_path_for(&feed);
        let slow = acquire(&feed, DEFAULT_WAIT_MS).unwrap();

        // A human cleared the wedge and a successor took the lock while this
        // holder was slow.
        std::fs::remove_file(&lock_path).unwrap();
        let successor = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        let successor_record = std::fs::read_to_string(&lock_path).unwrap();

        assert_eq!(slow.release(), Release::Stolen);
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            successor_record,
            "the successor's record was put back BYTE FOR BYTE"
        );
        assert_eq!(successor.release(), Release::Released);
        // And no quarantine debris is left behind on the happy restore path.
        let debris: Vec<_> = std::fs::read_dir(lock_path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("reclaim."))
            .collect();
        assert!(debris.is_empty(), "unexpected quarantine debris: {debris:?}");
    }

    /// A release that cannot be VERIFIED retains ownership so the holder can try
    /// again — it does not silently declare success.
    #[test]
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
            let mut sightings = 0usize;
            while !stop_reader.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(body) = std::fs::read_to_string(&observer) {
                    if !body.is_empty() {
                        assert!(
                            token_of(&body).is_some(),
                            "a partial record was visible at the lock path: {body:?}"
                        );
                        sightings += 1;
                    }
                }
            }
            sightings
        });
        for _ in 0..200 {
            let lock = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
            lock.release();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        watcher.join().unwrap();
    }

    #[test]
    fn with_feed_lock_runs_the_body_and_releases() {
        let dir = scratch();
        let feed = feed(&dir);
        let out = with_feed_lock(&feed, DEFAULT_WAIT_MS, || 42).unwrap();
        assert_eq!(out, 42);
        assert!(
            !lock_path_for(&feed).exists(),
            "the lock is released before the result is handed back"
        );
    }

    /// §3 again, at the API the writers actually call: a held lock means the
    /// closure NEVER RUNS. A fail-open twin would run it and corrupt the feed.
    #[test]
    fn with_feed_lock_does_not_run_the_body_when_it_cannot_serialise() {
        let dir = scratch();
        let feed = feed(&dir);
        let held = acquire(&feed, DEFAULT_WAIT_MS).unwrap();
        let mut ran = false;
        let refused = with_feed_lock(&feed, 20, || ran = true).unwrap_err();
        assert_eq!(refused, LockRefusal::Timeout);
        assert!(!ran, "the body must NOT run when the lock was not acquired");
        held.release();
    }

    /// Two threads, one lock, many appends: the mutex actually excludes. This is
    /// the property every derived global feed id depends on.
    #[test]
    fn concurrent_writers_never_interleave_inside_the_section() {
        let dir = scratch();
        let feed = feed(&dir);
        let counter = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));
        let mut handles = Vec::new();
        for worker in 0..4 {
            let feed = feed.clone();
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    with_feed_lock(&feed, 5_000, || {
                        // Inside the section: read, pause, write. Without real
                        // exclusion this read-modify-write loses updates.
                        let mut seen = counter.lock().unwrap();
                        let next = seen.len() as i32;
                        std::thread::yield_now();
                        seen.push(next + worker * 0);
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
}
