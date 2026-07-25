//! Durable, opaque state for one accepted Telegram-adjacent occurrence.
//!
//! Some inbound paths perform a local mutation before they send a family-visible
//! reply. A transport retry must not repeat that mutation, and a process crash
//! must not leave a later invocation guessing whether the mutation happened.
//! This journal gives those paths a small record-before-act state machine:
//!
//! `reserved -> applied(outcome) -> delivered(outcome)`
//!
//! A leftover `reserved` record is deliberately incomplete and therefore fails
//! closed. An `applied` record carries the exact canonical outcome needed to
//! retry delivery without re-running the mutation. A `delivered` record makes a
//! completed occurrence a no-op. `passed_through` records that this domain did
//! not own the occurrence, so the caller may continue its normal pipeline on a
//! dispatcher refire.
//!
//! Filenames are versioned BLAKE3 digests over the domain and caller-supplied
//! opaque occurrence key. Neither that key nor household/chat identifiers are
//! exposed in the directory listing. Each occurrence has a sidecar advisory lock
//! held across the caller's entire state transition, and each record transition
//! uses the repository's fsync + atomic-rename writer.

use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::telegram_conversation::durable_telegram_digest_v1;

const OCCURRENCE_SCHEMA_VERSION: u8 = 1;
const OCCURRENCE_DIR: &str = "telegram-occurrences";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OccurrenceState<T> {
    /// This invocation durably reserved a previously unseen occurrence.
    New,
    /// A prior invocation reserved the occurrence but did not durably record
    /// whether its mutation completed. Re-applying would be unsafe.
    Incomplete,
    /// The mutation completed and only delivery may be retried.
    Applied(T),
    /// The mutation and delivery both completed.
    Delivered(T),
    /// This mutation domain declined the occurrence and the caller may continue.
    PassedThrough,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum StoredOccurrence {
    Reserved {
        version: u8,
    },
    Applied {
        version: u8,
        outcome: serde_json::Value,
    },
    Delivered {
        version: u8,
        outcome: serde_json::Value,
    },
    PassedThrough {
        version: u8,
    },
}

impl StoredOccurrence {
    fn version(&self) -> u8 {
        match self {
            Self::Reserved { version }
            | Self::Applied { version, .. }
            | Self::Delivered { version, .. }
            | Self::PassedThrough { version } => *version,
        }
    }
}

/// Exclusive handle for one occurrence record.
///
/// The sidecar lock is intentionally retained for this value's full lifetime,
/// including the caller's mutation and transport call. Concurrent invocations
/// therefore observe a completed transition instead of racing the first writer.
pub struct OccurrenceJournal<T> {
    path: PathBuf,
    _lock: OccurrenceLock,
    _outcome: PhantomData<T>,
}

impl<T> OccurrenceJournal<T>
where
    T: Serialize + DeserializeOwned + Clone,
{
    /// Reserve or reopen an occurrence under `<workgraph>/telegram-occurrences`.
    ///
    /// `domain` separates mutation kinds (for example web fast lane vs. photo
    /// shopping), while `occurrence_key` identifies one accepted physical turn.
    /// Both are hashed into the filename and never persisted in plaintext.
    pub fn claim(
        workgraph_dir: &Path,
        domain: &str,
        occurrence_key: &str,
    ) -> Result<(Self, OccurrenceState<T>)> {
        Self::open(workgraph_dir, domain, occurrence_key, true)?
            .context("occurrence claim unexpectedly returned no journal")
    }

    /// Reopen an already-journaled occurrence without reserving a missing one.
    ///
    /// This lets a caller honor an `applied`/`delivered` outcome even if mutable
    /// request fields drift on a dispatcher retry and no longer classify into
    /// the mutation domain. A never-seen ordinary turn leaves no journal file.
    pub fn reopen(
        workgraph_dir: &Path,
        domain: &str,
        occurrence_key: &str,
    ) -> Result<Option<(Self, OccurrenceState<T>)>> {
        Self::open(workgraph_dir, domain, occurrence_key, false)
    }

    fn open(
        workgraph_dir: &Path,
        domain: &str,
        occurrence_key: &str,
        reserve_missing: bool,
    ) -> Result<Option<(Self, OccurrenceState<T>)>> {
        let domain = domain.trim();
        let occurrence_key = occurrence_key.trim();
        if domain.is_empty() {
            anyhow::bail!("Telegram occurrence domain must not be empty");
        }
        if occurrence_key.is_empty() {
            anyhow::bail!("Telegram occurrence key must not be empty");
        }

        let digest =
            durable_telegram_digest_v1("telegram-occurrence-journal", &[domain, occurrence_key]);
        let dir = workgraph_dir.join(OCCURRENCE_DIR);
        let path = dir.join(format!("{digest}.json"));
        // The common pass-through path should not create a directory or
        // permanent sidecar for every ordinary conversation.
        if !reserve_missing && !path.exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating Telegram occurrence journal {}", dir.display()))?;
        let lock_path = dir.join(format!("{digest}.lock"));
        let lock = OccurrenceLock::acquire(&lock_path)?;
        let journal = Self {
            path,
            _lock: lock,
            _outcome: PhantomData,
        };

        if !journal.path.exists() {
            if !reserve_missing {
                return Ok(None);
            }
            journal.write(&StoredOccurrence::Reserved {
                version: OCCURRENCE_SCHEMA_VERSION,
            })?;
            return Ok(Some((journal, OccurrenceState::New)));
        }

        let bytes = std::fs::read(&journal.path).with_context(|| {
            format!(
                "reading Telegram occurrence journal {}",
                journal.path.display()
            )
        })?;
        let stored: StoredOccurrence = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "invalid Telegram occurrence journal {} (refusing to re-run mutation)",
                journal.path.display()
            )
        })?;
        if stored.version() != OCCURRENCE_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported Telegram occurrence journal version {} in {} (expected {}; refusing to re-run mutation)",
                stored.version(),
                journal.path.display(),
                OCCURRENCE_SCHEMA_VERSION,
            );
        }

        let state = match stored {
            StoredOccurrence::Reserved { .. } => OccurrenceState::Incomplete,
            StoredOccurrence::PassedThrough { .. } => OccurrenceState::PassedThrough,
            StoredOccurrence::Applied { outcome, .. } => OccurrenceState::Applied(
                serde_json::from_value(outcome).with_context(|| {
                    format!(
                        "invalid applied outcome in Telegram occurrence journal {} (refusing to re-run mutation)",
                        journal.path.display()
                    )
                })?,
            ),
            StoredOccurrence::Delivered { outcome, .. } => OccurrenceState::Delivered(
                serde_json::from_value(outcome).with_context(|| {
                    format!(
                        "invalid delivered outcome in Telegram occurrence journal {} (refusing to re-run mutation)",
                        journal.path.display()
                    )
                })?,
            ),
        };
        Ok(Some((journal, state)))
    }

    /// Persist the exact canonical result of the mutation before transport.
    pub fn mark_applied(&self, outcome: &T) -> Result<()> {
        self.write(&StoredOccurrence::Applied {
            version: OCCURRENCE_SCHEMA_VERSION,
            outcome: serde_json::to_value(outcome)
                .context("serializing Telegram occurrence outcome")?,
        })
    }

    /// Persist that transport accepted (or durably deduplicated) the outcome.
    pub fn mark_delivered(&self, outcome: &T) -> Result<()> {
        self.write(&StoredOccurrence::Delivered {
            version: OCCURRENCE_SCHEMA_VERSION,
            outcome: serde_json::to_value(outcome)
                .context("serializing delivered Telegram occurrence outcome")?,
        })
    }

    /// Persist that this mutation domain did not own the occurrence.
    pub fn mark_passed_through(&self) -> Result<()> {
        self.write(&StoredOccurrence::PassedThrough {
            version: OCCURRENCE_SCHEMA_VERSION,
        })
    }

    fn write(&self, stored: &StoredOccurrence) -> Result<()> {
        let mut bytes =
            serde_json::to_vec_pretty(stored).context("serializing Telegram occurrence journal")?;
        bytes.push(b'\n');
        crate::atomic_file::write_atomic(&self.path, bytes).with_context(|| {
            format!(
                "persisting Telegram occurrence journal {}",
                self.path.display()
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// RAII exclusive advisory lock for one occurrence sidecar.
struct OccurrenceLock {
    #[cfg(unix)]
    _file: File,
}

impl OccurrenceLock {
    #[cfg(unix)]
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening Telegram occurrence lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking Telegram occurrence {}", path.display()));
        }
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire(_path: &Path) -> Result<Self> {
        Ok(Self {})
    }
}

#[cfg(unix)]
impl Drop for OccurrenceLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct FixtureOutcome {
        reply: String,
    }

    #[test]
    fn occurrence_paths_are_opaque_and_state_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let workgraph_dir = dir.path().join(".wg");
        let raw_key = "gateway-turn-household-raw-41";
        let outcome = FixtureOutcome {
            reply: "Done — oat milk is on the list.".to_string(),
        };

        assert!(
            OccurrenceJournal::<FixtureOutcome>::reopen(
                &workgraph_dir,
                "fixture",
                "ordinary-unseen-turn",
            )
            .unwrap()
            .is_none()
        );
        assert!(
            !workgraph_dir.join(OCCURRENCE_DIR).exists(),
            "checking an ordinary unseen turn must not create runtime state",
        );

        let (journal, state) =
            OccurrenceJournal::<FixtureOutcome>::claim(&workgraph_dir, "fixture", raw_key).unwrap();
        assert_eq!(state, OccurrenceState::New);
        let path = journal.path().to_path_buf();
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("b3-v1-")
        );
        assert!(!path.to_string_lossy().contains(raw_key));
        journal.mark_applied(&outcome).unwrap();
        drop(journal);

        let (journal, state) =
            OccurrenceJournal::<FixtureOutcome>::reopen(&workgraph_dir, "fixture", raw_key)
                .unwrap()
                .unwrap();
        assert_eq!(state, OccurrenceState::Applied(outcome.clone()));
        journal.mark_delivered(&outcome).unwrap();
        drop(journal);

        let (_journal, state) =
            OccurrenceJournal::<FixtureOutcome>::reopen(&workgraph_dir, "fixture", raw_key)
                .unwrap()
                .unwrap();
        assert_eq!(state, OccurrenceState::Delivered(outcome));
    }

    #[test]
    fn corrupt_or_reserved_occurrence_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workgraph_dir = dir.path().join(".wg");

        let (reserved, state) =
            OccurrenceJournal::<FixtureOutcome>::claim(&workgraph_dir, "fixture", "reserved")
                .unwrap();
        assert_eq!(state, OccurrenceState::New);
        drop(reserved);
        let (_reserved, state) =
            OccurrenceJournal::<FixtureOutcome>::claim(&workgraph_dir, "fixture", "reserved")
                .unwrap();
        assert_eq!(state, OccurrenceState::Incomplete);

        let (corrupt, _) =
            OccurrenceJournal::<FixtureOutcome>::claim(&workgraph_dir, "fixture", "corrupt")
                .unwrap();
        let corrupt_path = corrupt.path().to_path_buf();
        drop(corrupt);
        std::fs::write(&corrupt_path, b"{not-json").unwrap();
        let error = match OccurrenceJournal::<FixtureOutcome>::claim(
            &workgraph_dir,
            "fixture",
            "corrupt",
        ) {
            Ok(_) => panic!("corrupt journal must not be accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("refusing to re-run mutation"));
        assert_eq!(std::fs::read(&corrupt_path).unwrap(), b"{not-json");
    }
}
