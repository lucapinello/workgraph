//! The ENGINE half of the relay-receipt contract
//! (schema `casa-receipt-observe-v9.1`, task `week-start-engine-2` set 3).
//!
//! WHY RECEIPTS EXIST. A feed row is what the pane SHOWS. It is not evidence
//! that anything reached the family: the row is written locally, the relay's
//! `{ok, delivered, message_id}` was discarded, and the listener drops the bot's
//! own echo by design — so "the helper replied" was, end to end, a claim the
//! writer made about itself. A receipt is the independent record: a separate
//! ledger, written from the transport's own answer, joined to the exact row it
//! proves. The feed row's self-description is display-only; the receipt is the
//! evidence.
//!
//! WHAT THIS MODULE IS AND IS NOT.
//!   · It is the ENGINE-side writer: `provenance: "engine"`, for replies the
//!     Rust listener/relay sends. The gateway writes its own `gateway-inline`
//!     and `gateway-human` receipts into the same ledger.
//!   · It OWNS the transport-scope mint, the typed-id validation, the replay
//!     guard and the one-receipt-per-row rule.
//!   · It does NOT allocate the global feed id. That id is assigned inside the
//!     gateway's feed-rotation critical section (task `receipt-s1-lock`), and a
//!     second, independent allocator here would be exactly the double-writer
//!     race that slice exists to remove. The caller supplies the id it observed
//!     under that lock; [`append`] refuses a receipt without one rather than
//!     inventing an ordinal that could name the wrong row.
//!
//! THE PARTS THAT ARE EASY TO GET SUBTLY WRONG, AND WHY THEY ARE HERE:
//!
//! TRANSPORT SCOPE ID. `transportScopeId` identifies the ACTUAL SENDING BOT —
//! not the semantic reply role — because "which bot's token physically sent
//! this" is the question a delivery dispute turns on, and one role can be
//! spoken by different bots across a rotation. It is minted through a KEYED
//! digest over a persisted random per-install key. A raw `sha256(bot id)` is
//! forbidden by the schema and would be worthless: a bot roster is a handful of
//! short stable strings, so an unkeyed digest is reversible by dictionary in
//! milliseconds. [`scope_id_for_bot`] therefore keys the digest, and
//! [`is_dictionary_reversible`] is the negative the tests assert against.
//!
//! REPLAY GUARD. `sha256(transportScopeId + "\0" + messageId)`. One Telegram
//! message can only be delivered once, so two receipts claiming the same
//! (scope, message id) means a receipt was replayed — a second, later claim of
//! an old delivery. Rejected AT WRITE, never first-writer-wins-and-ignore.
//!
//! ATTEMPT ID. A retry after a genuine failure is NOT a replay: the self-heal
//! path re-sends, and suppressing its receipt would erase the only evidence
//! that the second attempt is what actually reached the family.
//! `WG_ATTEMPT_ID` carries `(turn, attempt)`, and the dedupe key is the pair —
//! so attempt 2 of turn T writes its own receipt while a refire of attempt 1
//! does not. The replay guard above still applies: two attempts cannot both
//! claim the same Telegram message id.
//!
//! RAW TURN ID, VERBATIM. `WG_TURN_ID` is written into rows and receipts EXACTLY
//! as it arrived. The engine hashes turn ids internally (the durable delivery
//! digest), and a hashed id in a receipt cannot be joined to a gateway row that
//! carries the raw one — the join silently finds nothing and every row reads as
//! unproven. Hashing stays internal to the delivery ledger; the wire is raw.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the receipts live, beside the feed the gateway reads.
pub fn ledger_path_for(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("relay-receipts.jsonl")
}

/// The never-rotated correlation index, shared with the gateway twin
/// (`receiptLedger.mjs` `receiptIndexPathFor`).
///
/// WHY A SECOND FILE AT ALL. Schema v9.1's `receipt_fields` is an EXHAUSTIVE
/// object, and this writer used to put three keys of its own in the ledger:
/// `outcome`, `replyPhase`, `attemptId`. That is not a naming quibble — the
/// gateway twin validates every ledger line against the exact field set, so an
/// engine receipt read as `malformed` from the other side, and a malformed line
/// makes the twin's own `appendReceipt` refuse to write. The two writers were
/// producing a ledger neither could fully read.
///
/// The correlation those keys carried is still needed (the refire guard keys on
/// `(turn, attempt)`), so it lives HERE, out of the evidence and in the
/// accelerator, exactly where the twin already keeps `replayKey`. The twin's
/// index reader checks its own three fields and ignores the rest, so a line we
/// write is a line it reads.
pub fn index_path_for(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("relay-receipt-index.jsonl")
}

/// One index line: the twin's three fields, plus the correlation this writer
/// needs and the schema will not carry.
///
/// NOT `deny_unknown_fields`, unlike [`Receipt`], and the difference is the
/// point: the ledger is EVIDENCE (a key we do not understand there is a claim we
/// cannot evaluate), while this is an ACCELERATOR another implementation also
/// appends to. A key the twin adds later must not stop us reading its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptIndexEntry {
    #[serde(rename = "receiptId")]
    pub receipt_id: String,
    #[serde(rename = "feedId")]
    pub feed_id: i64,
    /// `sha256(transportScopeId + NUL + messageId)`, or null when the attempt
    /// had no positive message id to replay.
    #[serde(rename = "replayKey")]
    pub replay_key: Option<String>,
    /// The correlation the v9.1 ledger may not carry.
    #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(rename = "attemptId", default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(
        rename = "replyPhase",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reply_phase: Option<ReplyPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RelayOutcome>,
}

/// Where the per-install transport-scope key lives. Never leaves the process;
/// never appears in a receipt, a row, or a log.
fn scope_key_path(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("transport-scope.key")
}

/// Did the relay prove a delivery, prove a failure, or prove nothing?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayStatus {
    /// Bot API `ok` AND a positive `result.message_id`. Nothing else.
    Delivered,
    /// The transport answered, and the answer was a failure.
    Failed,
    /// No usable answer — a timeout, a torn connection, a body that did not
    /// parse. The message MAY have arrived; the honest record says so rather
    /// than guessing in either direction.
    Unproven,
}

impl RelayStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RelayStatus::Delivered => "delivered",
            RelayStatus::Failed => "failed",
            RelayStatus::Unproven => "unproven",
        }
    }
}

/// Which transport call produced this receipt. Typed, because "the ack was
/// EDITED into the final answer" and "a second message was SENT" are different
/// deliveries with different evidence, and a fallback send after a failed edit
/// is the case where reading one as the other loses the real message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayOutcome {
    Send,
    Edit,
    /// A fresh send after an edit could not be applied.
    Fallback,
}

/// The value a deserialised receipt carries BEFORE the index refills it. `Send`
/// is the least surprising placeholder — but nothing should ever read it: every
/// reader in this module joins the index before returning.
impl Default for RelayOutcome {
    fn default() -> Self {
        RelayOutcome::Send
    }
}

impl RelayOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RelayOutcome::Send => "send",
            RelayOutcome::Edit => "edit",
            RelayOutcome::Fallback => "fallback",
        }
    }
}

/// Which phase of the turn this row is. A CLOSED enum, writer-stamped at emit
/// time because the writer KNOWS which it is — deriving it later from the text
/// is text analysis, and text analysis is how a watchdog line gets counted as
/// the turn's final answer.
///
/// FIVE MEMBERS SINCE v9.2, and the fifth is the reason this enum moved at all.
/// `Addendum` is a SECOND VOICE's part of ONE answer — the shipped case is the
/// meal-swap fast lane, where the meal owner reports the swap and the nutrition
/// owner adds a one-line companion take. It is turn-bound, receipted and
/// certified like any other row, and it is NEVER the turn's final: it is not
/// counted as final and it does not consume the finality reservation. Before it
/// existed the companion was written with NO causal turn, which kept it stamped
/// and provable but destroyed the durable join back to the ask
/// (`docs/schemas/receipt-observe-schema-v9.2.json` `multi_voice_answer`, in the
/// gateway tree).
///
/// THE ORDER OF THE MEMBERS IS THE SCHEMA'S — `ack|final|addendum|watchdog|
/// failure`. Nothing derives an ordinal from it (serde writes the lowercase
/// name, and the only ordering anyone reads is [`succession_rank`]), but the two
/// implementations are diffed by eye far more often than by machine and the
/// gateway's `REPLY_PHASES` is in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyPhase {
    Ack,
    Final,
    /// A second voice's part of one answer. Turn-bound, never the final.
    Addendum,
    Watchdog,
    Failure,
}

/// See [`RelayOutcome::default`]. `Final` is the placeholder; the index is the
/// answer.
impl Default for ReplyPhase {
    fn default() -> Self {
        ReplyPhase::Final
    }
}

impl ReplyPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyPhase::Ack => "ack",
            ReplyPhase::Final => "final",
            ReplyPhase::Addendum => "addendum",
            ReplyPhase::Watchdog => "watchdog",
            ReplyPhase::Failure => "failure",
        }
    }

    /// How far through the turn this phase is — the ONLY ordering the replay
    /// guard's edit-succession exemption reads. See [`is_edit_succession`].
    ///
    /// The designed successions are `ack → final`, `ack → watchdog → final`,
    /// `ack → failure` and `ack → watchdog → failure`: the heavy lane posts an
    /// ack and then EDITS that same physical message forward.
    ///
    /// `Final` and `Failure` share rank 2 deliberately. They are both TERMINAL,
    /// so neither succeeds the other, and equal ranks fail the strict `<` below
    /// — a second terminal claim over a message that already carries one is the
    /// replay it looks like, and v9.1's "exactly one replyPhase:'final' per
    /// accepted turn" stays enforced by this guard as well as by the attempt
    /// guard.
    ///
    /// `Addendum` JOINS THEM AT 2, AND THAT NUMBER IS A CROSS-IMPLEMENTATION
    /// CONSTRAINT, not a taste call. The receipt index is appended to by BOTH
    /// writers, and the gateway's port of this function
    /// (`receiptLedger.successionRank`) is written as `ack`=0, `watchdog`=1,
    /// ANYTHING ELSE = 2 — so a reader that does not know the word `addendum`
    /// already ranks it 2 by falling through. Giving it any other rank here
    /// would make the two implementations disagree about one line in a shared
    /// file, which is the one thing the v9.2 decision's `cross_impl_note`
    /// forbids: a rank below 2 would let a later edit be read as SUCCEEDING an
    /// addendum in the gateway's ledger and as a replay in ours. 2 is also the
    /// fail-closed reading on its own terms — an addendum is a distinct Telegram
    /// message with its own message id, so it never joins another row's edit
    /// succession anyway.
    fn succession_rank(self) -> u8 {
        match self {
            ReplyPhase::Ack => 0,
            ReplyPhase::Watchdog => 1,
            ReplyPhase::Final | ReplyPhase::Failure | ReplyPhase::Addendum => 2,
        }
    }
}

/// One receipt: the evidence for exactly ONE feed row.
///
/// THE FIELD SET IS EXHAUSTIVE IN BOTH DIRECTIONS. `deny_unknown_fields` is the
/// reading half of that: schema v9.1's `receipt_fields` is a closed object, and
/// a ledger line carrying a key the schema does not define is a record written
/// by something that was not speaking this contract. Accepting it silently —
/// which is what a plain `Deserialize` did — let the exact-tree control add
/// `"unknownKey"` to a receipt and watch the writer append a second one on top
/// of it, certifying against evidence it had not actually understood. The
/// gateway twin's `validateReceipt` refuses the same shape from the other side
/// (`receiptLedger.mjs` RECEIPT_FIELDS, "no strangers, no absentees").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// `rcpt_<uuid v4>` — unique per relay ATTEMPT, never reused.
    #[serde(rename = "receiptId")]
    pub receipt_id: String,
    /// The accepted turn's id, RAW and verbatim.
    #[serde(rename = "turnId")]
    pub turn_id: String,
    /// The global logical feed id of the exact row this proves.
    #[serde(rename = "feedId")]
    pub feed_id: i64,
    /// `agent` for a helper reply; `notice` for a failure notice.
    #[serde(rename = "feedKind")]
    pub feed_kind: String,
    /// The configured stable role id, exactly as sealed in the roster. Never a
    /// display label — a rename changes labels, ids stay.
    #[serde(rename = "roleId")]
    pub role_id: String,
    /// `ts_<64hex>` — the ACTUAL sending bot.
    #[serde(rename = "transportScopeId")]
    pub transport_scope_id: String,
    /// Positive integer from the Bot API. REQUIRED when status is delivered.
    ///
    /// ALWAYS SERIALISED, `null` when absent. The twin's field set has no
    /// absentees either: a receipt that simply omits the key is one the other
    /// implementation reads as malformed, and the two writers share this file.
    #[serde(rename = "messageId")]
    pub message_id: Option<i64>,
    #[serde(rename = "acceptedAtMs")]
    pub accepted_at_ms: i64,
    pub status: RelayStatus,
    /// Always `engine` from this writer.
    pub provenance: String,
    // ── NOT LEDGER FIELDS ───────────────────────────────────────────────────
    // The three below are `#[serde(skip)]`: they are this writer's correlation,
    // not v9.1 receipt fields, and they now travel in [`ReceiptIndexEntry`].
    // See [`index_path_for`] for why the difference is load-bearing rather than
    // cosmetic. The serialised field order above is the twin's, so the two
    // writers' lines are diffable byte for byte.
    //
    // They stay ON this struct because every caller that HAS a receipt has this
    // correlation in hand at the same moment, and a second parameter threaded
    // through eight call sites is one more place to pass the wrong phase. On the
    // way back out they are refilled from the index by [`read_all`] /
    // [`read_strict`], so a receipt read from disk still knows what it was.
    /// Which transport call this was. Index-carried.
    #[serde(skip)]
    pub outcome: RelayOutcome,
    /// Which phase of the turn the proven row is. Index-carried.
    #[serde(skip)]
    pub reply_phase: ReplyPhase,
    /// `(turn, attempt)` — present when the caller supplied `WG_ATTEMPT_ID`.
    /// Index-carried.
    #[serde(skip)]
    pub attempt_id: Option<String>,
}

/// Why a receipt was REFUSED. Every variant leaves the ledger byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptError {
    /// A typed id field did not match its schema shape.
    BadShape {
        field: &'static str,
        value: String,
    },
    /// `status: delivered` with no positive message id — the delivery is not
    /// proven, so it may not be recorded as one.
    DeliveredWithoutMessageId,
    /// The caller did not observe a global feed id. A receipt that names no row
    /// proves nothing, and a guessed ordinal names the WRONG row.
    NoFeedId,
    /// Another receipt already proves this row. One row, one receipt.
    RowAlreadyProven {
        feed_id: i64,
        by: String,
    },
    /// This exact (transport scope, message id) delivery is already recorded —
    /// a replayed claim of an old delivery. NOT raised for the designed edit
    /// succession, where one physical message is edited forward through a
    /// turn's phases: see [`is_edit_succession`].
    Replay {
        replay_key: String,
        by: String,
    },
    /// This (turn, attempt) already wrote a receipt. A refire, not a retry.
    AttemptAlreadyRecorded {
        attempt_id: String,
        by: String,
    },
    /// A receipt id was reused.
    ReceiptIdReused {
        receipt_id: String,
    },
    /// The ledger on disk is not wholly readable — an unreadable file, a line
    /// that does not parse, a torn tail. The evidence is DAMAGED, which is a
    /// different fact from "there is no evidence", and the difference decides
    /// whether a second claim of the same delivery gets certified.
    LedgerCorrupt {
        line: usize,
        detail: String,
    },
    /// The receipt could not be serialised against the feed transaction it
    /// belongs to, so the row it proves was not written either.
    NotSerialised(String),
    Io(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The VALUE is never rendered. A rejected id can be a token-shaped
            // paste or a raw household identifier, and an operator log is the
            // wrong place to reproduce one verbatim — the field and the shape it
            // failed are what a human needs to fix it.
            ReceiptError::BadShape { field, value } => write!(
                f,
                "{field} is not a valid typed id ({})",
                shape_category(field, value)
            ),
            ReceiptError::DeliveredWithoutMessageId => write!(
                f,
                "a receipt claimed delivered with no positive message id — the delivery is unproven"
            ),
            ReceiptError::NoFeedId => write!(
                f,
                "a receipt must name the global feed id of the row it proves"
            ),
            ReceiptError::RowAlreadyProven { feed_id, by } => {
                write!(f, "feed row {feed_id} is already proven by {by}")
            }
            ReceiptError::Replay { replay_key, by } => {
                write!(f, "delivery {replay_key} is already recorded by {by}")
            }
            ReceiptError::AttemptAlreadyRecorded { attempt_id, by } => {
                write!(f, "attempt {attempt_id} already wrote receipt {by}")
            }
            ReceiptError::ReceiptIdReused { receipt_id } => {
                write!(f, "receipt id {receipt_id} has already been used")
            }
            ReceiptError::LedgerCorrupt { line, detail } => write!(
                f,
                "the receipt ledger is damaged at line {line} ({detail}) — refusing to certify \
                 anything against evidence we cannot read in full"
            ),
            ReceiptError::NotSerialised(m) => write!(
                f,
                "the receipt could not be written inside its feed transaction: {m}"
            ),
            ReceiptError::Io(m) => write!(f, "receipt ledger io: {m}"),
        }
    }
}

/// A safe description of WHY a typed id was rejected: the expected shape, plus a
/// category for the value that never reproduces the value itself.
fn shape_category(field: &str, value: &str) -> String {
    let expected = match field {
        "turnId" => "expected web-turn-<uuid v4>",
        "receiptId" => "expected rcpt_<uuid v4>",
        "transportScopeId" => "expected ts_<64 hex>",
        "attemptId" => "expected attempt-<uuid v4>",
        _ => "expected a typed id",
    };
    let got = if value.trim().is_empty() {
        "got an empty value".to_string()
    } else {
        format!("got {} characters", value.chars().count())
    };
    format!("{expected}, {got}")
}

// ---------------------------------------------------------------------------
// Typed id shapes
// ---------------------------------------------------------------------------

/// Canonical RFC-4122 v4: 8-4-4-4-12 lowercase hex, version nibble `4`,
/// variant `[89ab]`. All-hyphens, wrong version and wrong variant are all
/// rejected — they are the shapes a placeholder takes when a real id was never
/// minted, and a placeholder that validates is a certification that proves
/// nothing.
fn is_uuid_v4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    for (part, want) in parts.iter().zip(lens) {
        if part.len() != want
            || !part
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        {
            return false;
        }
    }
    parts[2].starts_with('4') && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b'))
}

/// `web-turn-<uuid v4>`.
pub fn is_valid_turn_id(s: &str) -> bool {
    s.strip_prefix("web-turn-").is_some_and(is_uuid_v4)
}

/// `rcpt_<uuid v4>`.
pub fn is_valid_receipt_id(s: &str) -> bool {
    s.strip_prefix("rcpt_").is_some_and(is_uuid_v4)
}

/// `ts_<64 lowercase hex>`. Signed numerics, token-likes, names and chat ids all
/// fail this by construction — none of them is 64 hex characters.
pub fn is_valid_scope_id(s: &str) -> bool {
    s.strip_prefix("ts_").is_some_and(|h| {
        h.len() == 64
            && h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    })
}

/// Mint a fresh receipt id.
pub fn mint_receipt_id() -> String {
    format!("rcpt_{}", uuid::Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// The transport scope id — WHICH BOT physically sent this
// ---------------------------------------------------------------------------

/// Read (or mint, EXACTLY ONCE) the per-install scope key. 32 random bytes,
/// `0600`, stored beside the ledger. It never appears in a receipt or a log —
/// exposing it would turn every scope id back into a reversible digest of a
/// short bot id.
///
/// CREATE-ONCE, THEN READ THE WINNER. The obvious "read, else mint, else write"
/// is a race with 64 losers: sixty-four first users each mint their own key,
/// one atomic write wins the file, and every one of them returns the key it
/// minted. The engine then represents ONE sending bot with sixty-four different
/// transport scope ids, and the replay guard — which keys on
/// `(transportScopeId, messageId)` — stops seeing a replayed Telegram message id
/// as a replay at all, because the two claims sit under different scopes.
///
/// So the mint is a PUBLICATION, not a write: stage a private file, publish it
/// with `link(2)` (EEXIST when someone else got there first), and then — win or
/// lose — RE-READ the published file and return THAT. The value a caller gets
/// back is always the persisted one, so concurrent first users converge on a
/// single identity.
fn scope_key(project_root: &Path) -> Result<Vec<u8>, ReceiptError> {
    let path = scope_key_path(project_root);
    if let Some(existing) = read_scope_key(&path)? {
        return Ok(existing);
    }
    let mut buf = [0u8; 32];
    if getrandom::getrandom(&mut buf).is_err() {
        // A predictable fallback would silently un-key the digest. A uuid v4 is
        // OS entropy too, and fails the same way or not at all.
        buf[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        buf[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReceiptError::Io(e.to_string()))?;
    }
    // The staging file is created 0600 BEFORE any byte is written, so the key is
    // never briefly readable at the ambient umask — a crash between publish and
    // a later `chmod` cannot leave the identity secret world-readable.
    let staging = path.with_extension(format!("new.{}", uuid::Uuid::new_v4().simple()));
    let staged = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&staging)?;
        file.write_all(&buf)?;
        file.sync_all()
    })();
    let published = staged.and_then(|()| match std::fs::hard_link(&staging, &path) {
        // Won the mint, or lost it to another first user — either way the
        // authoritative bytes are now on disk and the re-read below decides.
        Ok(()) | Err(_) => sync_dir(path.parent()),
    });
    let _ = std::fs::remove_file(&staging);
    published.map_err(|e| ReceiptError::Io(e.to_string()))?;

    // FAIL CLOSED rather than fall back to our own candidate: returning an
    // unpersisted key is exactly the 64-identity bug in a different costume.
    read_scope_key(&path)?.ok_or_else(|| {
        ReceiptError::Io(format!(
            "the transport scope key at {} could not be read back after minting",
            path.display()
        ))
    })
}

/// The persisted key, or `None` when this install has none yet.
///
/// A file that EXISTS but is too short is not "no key": it is a key we cannot
/// use, and silently minting a second one over it would orphan every scope id
/// already written under the first. That fails CLOSED.
fn read_scope_key(path: &Path) -> Result<Option<Vec<u8>>, ReceiptError> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() >= 32 => Ok(Some(bytes)),
        Ok(bytes) => Err(ReceiptError::Io(format!(
            "the transport scope key at {} is {} bytes — refusing to mint a second identity over a damaged one",
            path.display(),
            bytes.len(),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReceiptError::Io(e.to_string())),
    }
}

/// fsync a directory so a rename/link that "succeeded" survives a power loss.
///
/// THE `.or(Ok(()))` USED TO SWALLOW EVERYTHING. The comment above it promised
/// durability while the code reported success whatever the device said, on both
/// the scope-key publication and the receipt append — so "the receipt is
/// durable" was a claim the writer had no evidence for. Three outcomes now, the
/// gateway twin's taxonomy (`receiptLedger.mjs` §"DURABILITY IS A THREE-WAY
/// ANSWER"):
///
/// * no directory handle at all — nothing here ever made a durability claim;
/// * `ENOTSUP`/`EOPNOTSUPP`/`EINVAL`/`EPERM` — the filesystem saying it does not
///   implement the call. Nothing failed;
/// * anything else (`EIO`, `ENOSPC`, `EBADF`) is a device error on the bytes we
///   are publishing, and it propagates.
fn sync_dir(dir: Option<&Path>) -> std::io::Result<()> {
    let Some(dir) = dir else { return Ok(()) };
    // THE OPEN IS CLASSIFIED THE SAME WAY THE FSYNC IS (reviewer 7da0c79a
    // blocker 3). `let Ok(handle) = … else { return Ok(()) }` mapped EVERY open
    // failure — injected EIO included — to a successful durable append: the
    // ledger line was already on disk and its directory entry was unproved, and
    // the API said yes. The caller hands us the concrete parent it has just
    // written the receipt into, so a refused open is not evidence that
    // directory durability is unsupported here. Only the narrow set below is.
    let handle = match open_dir_for_sync(dir) {
        Ok(handle) => handle,
        Err(e) if dir_fsync_unsupported(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    match handle.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if dir_fsync_unsupported(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// "This object cannot be fsynced here", as opposed to "the device said no".
/// The same ANNOUNCED set the gateway twin draws in `receiptLedger.mjs` and the
/// same one [`super::casa_feed`] uses, so the two ledgers agree about what a
/// durable append means.
fn dir_fsync_unsupported(e: &std::io::Error) -> bool {
    e.raw_os_error().is_some_and(|code| {
        [libc::ENOTSUP, libc::EOPNOTSUPP, libc::EINVAL, libc::EPERM].contains(&code)
    })
}

/// `File::open` on the parent, carrying the reviewer's injection seam on the
/// exact result the classification reads. `cfg(test)` only: no shipped binary
/// contains it, and it changes no production decision.
fn open_dir_for_sync(dir: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(test)]
    {
        let injected = inject::DIR_OPEN_ERRNO.with(|e| e.get());
        if injected != 0 {
            return Err(std::io::Error::from_raw_os_error(injected));
        }
    }
    std::fs::File::open(dir)
}

/// Test-only seams. Not compiled at all in a shipped binary.
///
/// THREAD-LOCAL, not a process-global: `cargo test` runs this module's tests
/// concurrently on many threads, and a global would inject an EIO into whichever
/// unrelated test happened to be appending at the time. Keyed to the arming
/// thread, the seam reaches exactly the call it was armed for and no `#[serial]`
/// is needed to make that true.
#[cfg(test)]
pub(crate) mod inject {
    use std::cell::Cell;

    thread_local! {
        /// Errno `open_dir_for_sync` returns instead of asking the filesystem.
        pub static DIR_OPEN_ERRNO: Cell<i32> = const { Cell::new(0) };
    }

    /// Arm the seam for THIS thread; the guard disarms it however the test ends,
    /// so a panicking assertion cannot leave the seam armed for the next test
    /// that reuses the thread.
    pub struct Armed;

    impl Armed {
        pub fn with(errno: i32) -> Armed {
            DIR_OPEN_ERRNO.with(|e| e.set(errno));
            Armed
        }
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            DIR_OPEN_ERRNO.with(|e| e.set(0));
        }
    }
}

/// The transport scope id for the bot that ACTUALLY sent the message.
///
/// `bot_id` is the sending bot's stable configured id — the token is never
/// passed in and never digested, so a leaked ledger cannot be turned back into
/// a credential even with the key.
pub fn scope_id_for_bot(project_root: &Path, bot_id: &str) -> Result<String, ReceiptError> {
    let key = scope_key(project_root)?;
    Ok(format!("ts_{}", keyed_hex(&key, "transport-scope", bot_id)))
}

fn keyed_hex(key: &[u8], domain: &str, value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key);
    h.update(b"\x1f");
    h.update(domain.as_bytes());
    h.update(b"\x1f");
    h.update(value.as_bytes());
    hex::encode(h.finalize())
}

/// THE FORBIDDEN MINT, kept here so the negative can name it: the raw
/// `sha256(bot id)` a scope id must never equal. A bot roster is a handful of
/// short stable strings; an unkeyed digest of one is reversible by dictionary.
pub fn is_dictionary_reversible(scope_id: &str, bot_id: &str) -> bool {
    use sha2::{Digest, Sha256};
    let raw = hex::encode(Sha256::digest(bot_id.as_bytes()));
    scope_id == format!("ts_{raw}") || scope_id == raw
}

/// The replay key for one proven delivery:
/// `sha256(transportScopeId + "\0" + messageId)`, per the schema.
pub fn replay_key(transport_scope_id: &str, message_id: i64) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(transport_scope_id.as_bytes());
    h.update(b"\x00");
    h.update(message_id.to_string().as_bytes());
    hex::encode(h.finalize())
}

/// `attempt-<uuid v4>` — the shape the gateway mints and passes on
/// `WG_ATTEMPT_ID`.
///
/// A loose attempt id is not a small problem. The dedupe key is
/// `(turn, attempt)`, so `"1"` and `"2"` from two unrelated processes collide by
/// construction: one turn's second attempt is then read as another's refire and
/// its receipt — the evidence for the send that actually reached the family — is
/// suppressed. Only a minted id is unique enough to key on.
pub fn is_valid_attempt_id(s: &str) -> bool {
    s.strip_prefix("attempt-").is_some_and(is_uuid_v4)
}

/// `(turn, attempt, phase)` from `WG_ATTEMPT_ID`, with attempt `1` when the
/// caller did not supply one.
///
/// A retry after a genuine failure must not be suppressed as if it were a
/// refire, which is what keying on the turn alone would do.
///
/// AND THE PHASE IS PART OF THE KEY. It was not, and that was invisible while
/// the ack wrote nothing: the moment a delivered ack started carrying its own
/// receipt, the turn's FINAL was refused as a refire of its own ack — one
/// physical delivery blocking a different one. v9.1 says the same in its join
/// rule: `turnId` is ONE-TO-MANY across a turn's receipts, "gateway-human + each
/// delivered helper ack/watchdog/final". Two deliveries of one attempt are two
/// receipts; a refire is the same phase of the same attempt arriving twice.
pub fn attempt_key(turn_id: &str, attempt_id: Option<&str>, phase: ReplyPhase) -> String {
    format!(
        "{turn_id}\u{1f}{}\u{1f}{}",
        attempt_slot(attempt_id),
        phase.as_str()
    )
}

/// The attempt an id names, with the "caller supplied none" case normalised to
/// `1` exactly once. Extracted so [`attempt_key`] and [`is_edit_succession`]
/// cannot drift into two different answers to "is this the same attempt" — a
/// drift that would show up as one guard exempting what the other suppresses.
fn attempt_slot(attempt_id: Option<&str>) -> &str {
    attempt_id
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("1")
}

/// Is `incoming` the DESIGNED EDIT SUCCESSION of `prior` — the same physical
/// Telegram message advancing phase within one turn/attempt — rather than a
/// replayed claim of `prior`'s delivery?
///
/// THIS IS THE LIVE DEFECT THE REPLAY GUARD HAD. The heavy web-inbound flow
/// posts an ack and then edits THAT SAME MESSAGE into the final answer
/// (`commands/telegram.rs` `edit_phase` → [`RelayOutcome::Edit`]), so the final
/// shares the ack's `(transportScopeId, messageId)` by construction — and the
/// guard refused the final's row+receipt as a replay of its own ack. The
/// delivery had already SUCCEEDED; only the evidence write was refused, which
/// left the turn's reservation held and the turn finalless while the family was
/// looking at the answer on their screen. Reproduced three times in the live
/// certification run (segment 5: ack `rcpt_3a74cecb`, feed 585, replay key
/// `de8d3197…`; gateway.log "the reply was sent but NOT recorded").
///
/// The fix narrows the guard's PREDICATE and leaves the key alone. Putting the
/// phase into [`replay_key`] would have been the smaller diff, but that key is a
/// wire field: `replayKey` is `sha256(transportScopeId + NUL + messageId)` in
/// schema v9.1 and in the gateway twin's `receiptLedger.mjs`, which appends to
/// the same index. Rekeying it here would leave the two writers computing
/// different keys for one delivery, and a replay guard that no longer matches
/// the twin's rows is not a replay guard at all.
///
/// So the exemption is deliberately narrow — all four must hold:
///
///   1. the incoming receipt is an EDIT. A `send` (or the `fallback` send after
///      a refused edit) mints a NEW message id, so a send presenting an id that
///      is already recorded is exactly the replay this guard exists to refuse;
///   2. the SAME turn — a different turn reusing a message id is refused as now;
///   3. the SAME attempt — a self-heal retry re-presenting an earlier attempt's
///      id is refused as now;
///   4. the prior is at a STRICTLY EARLIER phase ([`ReplyPhase::succession_rank`]).
///      A refire — the same phase of the same attempt arriving twice — fails
///      this, and is refused exactly as before.
///
/// It fails CLOSED on unjoinable evidence: a prior with no index line keeps
/// [`ReplyPhase::default`] (`Final`, the top rank) and `attempt_id: None`, so it
/// can never be read as an earlier phase of this attempt.
pub fn is_edit_succession(prior: &Receipt, incoming: &Receipt) -> bool {
    incoming.outcome == RelayOutcome::Edit
        && incoming.turn_id == prior.turn_id
        && attempt_slot(incoming.attempt_id.as_deref()) == attempt_slot(prior.attempt_id.as_deref())
        && prior.reply_phase.succession_rank() < incoming.reply_phase.succession_rank()
}

// ---------------------------------------------------------------------------
// The correlation index
// ---------------------------------------------------------------------------

/// The index, read WHOLE or not at all — the same fail-closed rules the ledger
/// gets, for the same reason: the refire guard is a search over THIS list, and a
/// line silently dropped from it is a suppressed refire becoming a duplicate
/// message on the family's screen.
pub fn read_index_strict(project_root: &Path) -> Result<Vec<ReceiptIndexEntry>, ReceiptError> {
    let path = index_path_for(project_root);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ReceiptError::LedgerCorrupt {
                line: 0,
                detail: format!("the receipt index is unreadable: {e}"),
            });
        }
    };
    if !body.is_empty() && !body.ends_with('\n') {
        return Err(ReceiptError::LedgerCorrupt {
            line: body.lines().count(),
            detail: "the receipt index's final line has no terminating newline".into(),
        });
    }
    let mut entries = Vec::new();
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ReceiptIndexEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                return Err(ReceiptError::LedgerCorrupt {
                    line: index + 1,
                    detail: format!("the receipt index is damaged: {}", e.classify_detail()),
                });
            }
        }
    }
    Ok(entries)
}

/// The lenient index read, for the display path. Same leniency contract as
/// [`read_all`]: never the basis for admitting a receipt.
fn read_index_lenient(project_root: &Path) -> Vec<ReceiptIndexEntry> {
    let Ok(body) = std::fs::read_to_string(index_path_for(project_root)) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ReceiptIndexEntry>(l).ok())
        .collect()
}

/// Refill the `#[serde(skip)]` correlation on receipts read from the ledger.
///
/// A receipt whose index line is missing keeps the placeholder defaults and
/// `attempt_id: None` — which is what a receipt written by the GATEWAY twin
/// legitimately looks like from here (it has no engine phase/outcome), so this
/// is a join, not a validation.
fn join_index(receipts: &mut [Receipt], index: &[ReceiptIndexEntry]) {
    for receipt in receipts.iter_mut() {
        if let Some(entry) = index.iter().find(|e| e.receipt_id == receipt.receipt_id) {
            if let Some(outcome) = entry.outcome {
                receipt.outcome = outcome;
            }
            if let Some(phase) = entry.reply_phase {
                receipt.reply_phase = phase;
            }
            receipt.attempt_id = entry.attempt_id.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

/// Every receipt currently in the ledger, for DISPLAY and OBSERVATION only.
///
/// This reader is deliberately lenient — a caller rendering "what do we know"
/// should show what is readable. It must NEVER be the basis for admitting a new
/// receipt: leniency there turns damaged evidence into permission to certify a
/// delivery twice. [`append`] uses [`read_strict`], which fails closed.
pub fn read_all(project_root: &Path) -> Vec<Receipt> {
    let Ok(body) = std::fs::read_to_string(ledger_path_for(project_root)) else {
        return Vec::new();
    };
    let mut receipts: Vec<Receipt> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Receipt>(l).ok())
        .collect();
    join_index(&mut receipts, &read_index_lenient(project_root));
    receipts
}

/// The ledger, read WHOLE or not at all — the read every uniqueness check runs
/// against.
///
/// FAILING OPEN HERE CERTIFIES DUPLICATES. The audit's reproduction is exact:
/// corrupt the one line proving feed row 7 / message 4242, and an identical
/// second proof is accepted (`visible_before=0 duplicate_append_succeeded=true`).
/// Every uniqueness rule in [`append`] — one row one receipt, the replay guard,
/// the attempt guard — is a search over THIS list, so a line silently dropped
/// from it is a claim silently forgotten, and forgetting a claim is
/// indistinguishable from never having had one.
///
/// So: a missing ledger is empty (an install with no receipts yet is a fact, not
/// damage), and anything else — an unreadable file, a line that does not parse,
/// a torn tail — is [`ReceiptError::LedgerCorrupt`]. The cure is an operator
/// looking at the evidence, not a writer guessing past it.
pub fn read_strict(project_root: &Path) -> Result<Vec<Receipt>, ReceiptError> {
    let path = ledger_path_for(project_root);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ReceiptError::LedgerCorrupt {
                line: 0,
                detail: e.to_string(),
            });
        }
    };
    // A JSONL RECORD IS THE BYTES UP TO AND INCLUDING ITS NEWLINE. A last line
    // without one is a write that was interrupted at the delimiter, and
    // `body.lines()` cannot tell that from a finished record — it yields the
    // same string either way. Two reasons this must be refused rather than
    // parsed, and the exact-tree control demonstrated both at once: the bytes
    // may be a PREFIX of a longer record, so "it parsed" proves nothing about
    // what was meant; and our append adds no leading newline, so the next write
    // WELDS itself to the torn line and destroys both records — the control got
    // `receipt=written` and a ledger of one unparsable 385-column line. Turning
    // detectable uncertainty into fresh corruption while reporting success is
    // the worst of the available outcomes. The gateway twin refuses the same
    // state (`parseLedgerText`, "AN UNTERMINATED FINAL LINE IS NOT A RECEIPT").
    if !body.is_empty() && !body.ends_with('\n') {
        return Err(ReceiptError::LedgerCorrupt {
            line: body.lines().count(),
            detail: "the final record has no terminating newline — the write was interrupted"
                .into(),
        });
    }
    let mut receipts = Vec::new();
    for (index, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Receipt>(line) {
            Ok(receipt) => receipts.push(receipt),
            // The value is NOT echoed: a damaged line can hold anything, and an
            // operator log is the wrong place to reproduce it verbatim.
            Err(e) => {
                return Err(ReceiptError::LedgerCorrupt {
                    line: index + 1,
                    detail: e.classify_detail(),
                });
            }
        }
    }
    // The index is read STRICTLY too — a damaged accelerator is a damaged
    // refire guard, and the refire guard's failure mode is a duplicate message
    // on the family's screen.
    join_index(&mut receipts, &read_index_strict(project_root)?);
    Ok(receipts)
}

/// A safe, non-echoing description of why a ledger line did not parse.
trait ClassifyDetail {
    fn classify_detail(&self) -> String;
}

impl ClassifyDetail for serde_json::Error {
    fn classify_detail(&self) -> String {
        match self.classify() {
            serde_json::error::Category::Eof => "a torn line — the write did not complete".into(),
            serde_json::error::Category::Syntax => "not parseable as JSON".into(),
            serde_json::error::Category::Data => "JSON, but not a receipt".into(),
            serde_json::error::Category::Io => "unreadable".into(),
        }
    }
}

/// Append a receipt, refusing every shape the contract forbids.
///
/// The checks run against the ledger AS IT IS ON DISK, in this order, and any
/// one of them leaves the file byte-identical:
///
///   1. typed id shapes (turn, receipt, transport scope);
///   2. delivered ⇒ a positive message id;
///   3. a global feed id was actually observed;
///   4. receipt id not reused;
///   5. this row is not already proven — one row, one receipt;
///   6. this (scope, message id) delivery is not already recorded — the replay
///      guard, so a re-read of an old response cannot re-certify it, EXCEPT for
///      the designed edit succession in which one physical message is edited
///      forward through a turn's phases ([`is_edit_succession`]);
///   7. this (turn, attempt) has not already written — a refire is suppressed,
///      a genuine retry is NOT.
///
/// The whole check-and-append runs INSIDE the shared feed lock, because the
/// checks are only worth what their atomicity is worth: read-then-append with no
/// lock is a test whose answer is stale by the time it is used. Sixty-four
/// synchronised writers submitting one duplicate claim got TEN successes and a
/// ledger of concatenated JSON that no longer parsed. This entry point takes the
/// lock; [`append_locked`] is the same body for a caller already inside the feed
/// transaction (the lock is NOT reentrant).
/// **THE RELEASE VERDICT IS PART OF THE RESULT** (reviewer 7da0c79a blocker 2).
/// This function used to read `lock.release();` as a statement and discard the
/// typed verdict the wrapper had just been taught to return, so a receipt
/// written inside a section whose release could not be proven was
/// indistinguishable at the boundary from one written inside a section that
/// proved it let go. A receipt IS the certification evidence; a certifier that
/// cannot see an unvouched section certifies it by default.
pub fn append(project_root: &Path, receipt: &Receipt) -> Result<Appended, ReceiptError> {
    let ledger = ledger_path_for(project_root);
    if let Some(parent) = ledger.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReceiptError::Io(e.to_string()))?;
    }
    // THE RETRY IS TAKEN ON THE TYPED REFUSAL, BEFORE IT BECOMES A STRING.
    // `ReceiptError::NotSerialised` carries prose, so by the time a caller sees
    // it the difference between "busy for a second" and "a human must delete
    // this file" is gone. That distinction is exactly what may and may not be
    // retried, so the budget is spent HERE, against
    // `feed_lock::LockRefusal::Timeout` (docs/42 §9, `feed-lock-retry`) — a
    // receipt is written on the same lock as the row it proves, and losing it to
    // a coin flip leaves a row in the family's conversation that nothing can
    // prove. When the patience is spent the error is byte-identical to before.
    let lock = super::feed_lock::acquire_retrying(
        &ledger,
        super::feed_lock::DEFAULT_WAIT_MS,
        super::feed_lock::DEFAULT_ATTEMPTS,
    )
    .map_err(|refusal| ReceiptError::NotSerialised(refusal.to_string()))?;
    let outcome = append_locked(project_root, receipt, &lock);
    let release = lock.release();
    // The receipt half is decided FIRST: a refused receipt is a refused receipt
    // whatever the release did, and reporting the lock instead would hide it.
    outcome?;
    Ok(match release {
        super::feed_lock::Release::Retained(reason) => {
            eprintln!(
                "[relay-receipt] the receipt was written under the lock, but the release could \
                 not be verified ({reason}) — this process still owns it and retries on the next \
                 acquire."
            );
            Appended::ReleaseUnverified(reason)
        }
        _ => Appended::Certified,
    })
}

/// A receipt that LANDED, and how the section that wrote it ended.
///
/// Not an `Err`: the ledger line is on disk and every contract check passed
/// under the held lock, so failing the caller's write would be a different lie.
/// Not a bare `()` either — see [`append`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a written receipt carries its release verdict — certify it or discard it explicitly"]
pub enum Appended {
    /// The receipt landed AND the release proved it let go.
    Certified,
    /// The receipt landed inside a section THIS frame did not release — see
    /// [`append_locked`]. The verdict belongs to whoever holds the lock, and
    /// that caller propagates it from its own transaction; there is nothing
    /// unvouched for this frame to report.
    InCallersSection,
    /// The receipt landed, but the release could not be proven: this process
    /// still owns the feed lock and retries on the next acquire.
    ReleaseUnverified(String),
}

impl Appended {
    /// Did the section that wrote this receipt prove it let the lock go?
    pub fn is_certified(&self) -> bool {
        !matches!(self, Appended::ReleaseUnverified(_))
    }

    /// `Ok(())` unless a release THIS frame performed could not be proven;
    /// `Err` carries the unverified reason.
    pub fn certified(self) -> Result<(), String> {
        match self {
            Appended::Certified | Appended::InCallersSection => Ok(()),
            Appended::ReleaseUnverified(reason) => Err(reason),
        }
    }

    /// The write happened, release verdict deliberately discarded — spelled out
    /// so the discard is a decision in the source rather than the default.
    pub fn regardless_of_release(self) {}
}

/// [`append`]'s body, for a caller that ALREADY HOLDS the feed lock — the
/// delivery seam, which writes the row and its receipt in one transaction.
///
/// The `_lock` parameter is a witness, not a hint: a [`FeedLock`] can only be
/// obtained by acquiring one, so a caller cannot reach this function without
/// holding the mutex, and cannot deadlock by taking it twice.
///
/// [`FeedLock`]: super::feed_lock::FeedLock
pub fn append_locked(
    project_root: &Path,
    receipt: &Receipt,
    _lock: &super::feed_lock::FeedLock,
) -> Result<(), ReceiptError> {
    if !is_valid_turn_id(&receipt.turn_id) {
        return Err(ReceiptError::BadShape {
            field: "turnId",
            value: receipt.turn_id.clone(),
        });
    }
    if !is_valid_receipt_id(&receipt.receipt_id) {
        return Err(ReceiptError::BadShape {
            field: "receiptId",
            value: receipt.receipt_id.clone(),
        });
    }
    if !is_valid_scope_id(&receipt.transport_scope_id) {
        return Err(ReceiptError::BadShape {
            field: "transportScopeId",
            value: receipt.transport_scope_id.clone(),
        });
    }
    if let Some(attempt) = receipt.attempt_id.as_deref()
        && !is_valid_attempt_id(attempt)
    {
        return Err(ReceiptError::BadShape {
            field: "attemptId",
            value: attempt.to_string(),
        });
    }
    if receipt.status == RelayStatus::Delivered && !receipt.message_id.is_some_and(|id| id > 0) {
        return Err(ReceiptError::DeliveredWithoutMessageId);
    }
    if receipt.feed_id <= 0 {
        return Err(ReceiptError::NoFeedId);
    }

    // STRICT: damaged evidence is not absent evidence. See [`read_strict`].
    let existing = read_strict(project_root)?;
    if let Some(prior) = existing.iter().find(|r| r.receipt_id == receipt.receipt_id) {
        return Err(ReceiptError::ReceiptIdReused {
            receipt_id: prior.receipt_id.clone(),
        });
    }
    if let Some(prior) = existing.iter().find(|r| r.feed_id == receipt.feed_id) {
        return Err(ReceiptError::RowAlreadyProven {
            feed_id: receipt.feed_id,
            by: prior.receipt_id.clone(),
        });
    }
    if let Some(mid) = receipt.message_id.filter(|id| *id > 0) {
        let key = replay_key(&receipt.transport_scope_id, mid);
        // EVERY prior holder of this key is checked, not just the first found:
        // `ack → watchdog → final` leaves the final facing two of them, and an
        // exemption that stopped at the first would let a genuine replay hide
        // behind a legitimate predecessor. The refusal names the prior that
        // actually failed the succession test.
        if let Some(prior) = existing
            .iter()
            .filter(|r| {
                r.message_id
                    .filter(|id| *id > 0)
                    .is_some_and(|prior_mid| replay_key(&r.transport_scope_id, prior_mid) == key)
            })
            .find(|prior| !is_edit_succession(prior, receipt))
        {
            return Err(ReceiptError::Replay {
                replay_key: key,
                by: prior.receipt_id.clone(),
            });
        }
    }
    let key = attempt_key(
        &receipt.turn_id,
        receipt.attempt_id.as_deref(),
        receipt.reply_phase,
    );
    if let Some(prior) = existing
        .iter()
        .find(|r| attempt_key(&r.turn_id, r.attempt_id.as_deref(), r.reply_phase) == key)
    {
        return Err(ReceiptError::AttemptAlreadyRecorded {
            attempt_id: key,
            by: prior.receipt_id.clone(),
        });
    }

    let path = ledger_path_for(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReceiptError::Io(e.to_string()))?;
    }
    // THE INDEX LINE GOES FIRST, matching the twin's order (`receiptLedger.mjs`
    // appendReceipt: "the INDEX still goes first within the step"). It is the
    // entry that reserves the feed id and the replay key, so an interruption
    // between the two files leaves a RESERVATION with no receipt — a claim that
    // refuses a duplicate — rather than a receipt no guard remembers.
    let index_entry = ReceiptIndexEntry {
        receipt_id: receipt.receipt_id.clone(),
        feed_id: receipt.feed_id,
        replay_key: receipt
            .message_id
            .filter(|id| *id > 0)
            .map(|mid| replay_key(&receipt.transport_scope_id, mid)),
        turn_id: Some(receipt.turn_id.clone()),
        attempt_id: receipt.attempt_id.clone(),
        reply_phase: Some(receipt.reply_phase),
        outcome: Some(receipt.outcome),
    };
    append_line_durable(
        &index_path_for(project_root),
        &serde_json::to_string(&index_entry).map_err(|e| ReceiptError::Io(e.to_string()))?,
    )?;
    // ONE record, ONE write. The line and its newline used to be two calls, and
    // two `O_APPEND` writes from two writers interleave: the audit's race left a
    // ledger of concatenated JSON objects in which the very claims that had just
    // "succeeded" were no longer readable. A single buffer is a single atomic
    // append for any record short enough to fit the pipe/file atomicity window,
    // and the lock above covers the rest.
    append_line_durable(
        &path,
        &serde_json::to_string(receipt).map_err(|e| ReceiptError::Io(e.to_string()))?,
    )
}

/// Append ONE line, with its delimiter, durably.
///
/// The line and its newline are one buffer because two `O_APPEND` writes from
/// two writers interleave: the audit's race left a ledger of concatenated JSON
/// objects in which the very claims that had just "succeeded" were no longer
/// readable. A single buffer is a single atomic append for any record short
/// enough to fit the file atomicity window, and the feed lock covers the rest.
///
/// The DIRECTORY is fsynced too, or a ledger created by this very append can be
/// absent after a power loss while the row it proves is durable — evidence that
/// vanishes is worse than evidence that was never written.
fn append_line_durable(path: &Path, line: &str) -> Result<(), ReceiptError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReceiptError::Io(e.to_string()))?;
    }
    let mut buffer = String::with_capacity(line.len() + 1);
    buffer.push_str(line);
    buffer.push('\n');
    // The exact length BEFORE our bytes. We are inside the feed lock (both entry
    // points take it, and `append_locked` cannot be reached without the witness),
    // so nobody else can have appended and everything past this offset is ours.
    let before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| ReceiptError::Io(e.to_string()))?;
    let landed = file
        .write_all(buffer.as_bytes())
        .and_then(|_| file.sync_all())
        .and_then(|()| sync_dir(path.parent()));
    // A DURABILITY FAILURE IS A REFUSAL, AND A REFUSAL LEAVES THE LEDGER
    // BYTE-IDENTICAL. Now that a refused parent-directory open propagates
    // (blocker 3), this error can arrive with our line already written, and a
    // ledger line whose directory entry may not survive a power loss is exactly
    // the evidence-that-vanishes case above. Take our own bytes back out.
    if let Err(e) = landed {
        let detail = e.to_string();
        return Err(match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(f) => match f.set_len(before) {
                Ok(()) => ReceiptError::Io(detail),
                Err(t) => ReceiptError::Io(format!(
                    "{detail} — AND the unproven line could not be taken back out: {t}"
                )),
            },
            Err(t) => ReceiptError::Io(format!(
                "{detail} — AND the unproven line could not be taken back out: {t}"
            )),
        });
    }
    Ok(())
}

/// The EVIDENCE a prior attempt already recorded for this `(turn, attempt)`, if
/// any — the durable refire bypass.
///
/// A dispatcher refire is a redelivery of an occurrence that was ALREADY
/// accepted and answered. It must not re-relay: the family has the message, and
/// sending it again is a duplicate on their screen. But "do nothing" is not
/// enough either — the refire still has to be able to say what happened, and the
/// only honest answer is the evidence the original attempt wrote. So a refire
/// looks its own evidence up here and reuses it, rather than minting a second
/// receipt (which [`append`] would refuse as [`ReceiptError::AttemptAlreadyRecorded`])
/// or re-sending to manufacture a fresh one.
///
/// Keyed on `(turn, attempt)`, NOT the turn alone: a self-heal retry after a
/// genuine failure is a DIFFERENT attempt, finds no evidence here, and correctly
/// proceeds to relay. Keying on the turn alone would make the retry look like a
/// refire and leave the household with the silence it was retrying.
///
/// The PHASE is deliberately not a parameter, and the FINAL is preferred when
/// several phases of one attempt were delivered. A refire's question is "what
/// did the family end up with", and the answer to that is the turn's answer —
/// reporting its ack instead would say the household is still waiting for a
/// message they already have.
pub fn evidence_for_attempt(
    project_root: &Path,
    turn_id: &str,
    attempt_id: Option<&str>,
) -> Option<Receipt> {
    let of_this_attempt = |r: &Receipt| {
        attempt_key(&r.turn_id, r.attempt_id.as_deref(), r.reply_phase)
            == attempt_key(turn_id, attempt_id, r.reply_phase)
    };
    let mine: Vec<Receipt> = read_all(project_root)
        .into_iter()
        .filter(of_this_attempt)
        .collect();
    mine.iter()
        .find(|r| r.reply_phase == ReplyPhase::Final)
        .cloned()
        .or_else(|| mine.into_iter().next_back())
}

/// The engine-side builder. `provenance` is fixed: this writer speaks only for
/// the engine, and a writer that could claim `gateway-human` could forge the
/// acting human's identity on a receipt.
#[allow(clippy::too_many_arguments)]
pub fn engine_receipt(
    turn_id: &str,
    feed_id: i64,
    feed_kind: &str,
    role_id: &str,
    transport_scope_id: &str,
    message_id: Option<i64>,
    status: RelayStatus,
    outcome: RelayOutcome,
    reply_phase: ReplyPhase,
    attempt_id: Option<&str>,
    accepted_at_ms: i64,
) -> Receipt {
    Receipt {
        receipt_id: mint_receipt_id(),
        turn_id: turn_id.to_string(),
        feed_id,
        feed_kind: feed_kind.to_string(),
        role_id: role_id.to_string(),
        transport_scope_id: transport_scope_id.to_string(),
        message_id: message_id.filter(|id| *id > 0),
        accepted_at_ms,
        status,
        provenance: "engine".to_string(),
        outcome,
        reply_phase,
        attempt_id: attempt_id
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`append`], asserting the section PROVED it let the feed lock go.
    ///
    /// Every receipt test writes through here, so each of them is also a
    /// standing control on the propagated verdict (reviewer 7da0c79a blocker 2):
    /// if the ordinary path ever degraded to an unverified release, the whole
    /// suite would say so instead of one dedicated test.
    fn append_certified(project_root: &Path, receipt: &Receipt) -> Result<(), ReceiptError> {
        append(project_root, receipt)?
            .certified()
            .expect("an undisturbed append certifies its own release");
        Ok(())
    }

    /// **BLOCKER 3 (reviewer 7da0c79a, 4/4) — A FAILED PARENT OPEN IS A FAILED
    /// SYNC.** `sync_dir` mapped every `File::open(dir)` error to `Ok(())`, so an
    /// injected EIO on the ledger's parent produced:
    ///
    /// ```json
    /// {"appendReturnedOk":true,"fileExists":true,
    ///  "injectedDirectoryOpenEio":true,"negativeControlReturnedOk":true}
    /// ```
    ///
    /// The receipt was on disk, its directory entry was unproved, and the API
    /// said yes. Every field of that control is asserted here, with the first one
    /// INVERTED, and the ledger is checked byte-identical: a refused append is a
    /// refused append, so the unproven line comes back out.
    #[test]
    fn an_injected_eio_on_the_ledgers_parent_is_a_refused_append() {
        let dir = scratch();
        let ledger = ledger_path_for(dir.path());

        // The NEGATIVE CONTROL first, uninjected and on the same fixture: this is
        // what "the seam is off" looks like, so a green assertion below cannot be
        // an accident of the fixture never appending at all.
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11)))
            .expect("the uninjected control must append");
        let unharmed = std::fs::read_to_string(&ledger).unwrap();
        assert_eq!(unharmed.lines().count(), 1, "the control wrote its line");

        let refused = {
            let _armed = inject::Armed::with(libc::EIO);
            append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12)))
        };
        let err = refused.expect_err(
            "a receipt whose directory durability could not be proved must NOT return Ok",
        );
        assert!(
            matches!(&err, ReceiptError::Io(detail) if detail.contains("Input/output error")),
            "the refusal must name the device error it actually got: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&ledger).unwrap(),
            unharmed,
            "a refused append leaves the ledger byte-identical — the unproven line \
             must not survive the refusal that named it"
        );
        assert_eq!(
            read_all(dir.path()).len(),
            1,
            "the refused receipt must not be readable as a recorded one"
        );
    }

    /// The seam is a TEST SEAM, not a behaviour: with it disarmed the very same
    /// append succeeds. Without this, "EIO refuses" would be satisfiable by an
    /// append that never worked.
    #[test]
    fn the_directory_open_seam_is_off_by_default() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11)))
            .expect("no seam is armed, so the append proceeds");
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    const TURN: &str = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const TURN2: &str = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3302";
    /// Attempt ids are MINTED, not counted: `"1"`/`"2"` from two unrelated
    /// processes collide, and a collision suppresses a real retry's receipt.
    const ATTEMPT_ONE: &str = "attempt-6ba7b810-9dad-41d1-80b4-00c04fd430c8";
    const ATTEMPT_TWO: &str = "attempt-6ba7b810-9dad-41d1-80b4-00c04fd430c9";

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn receipt(root: &Path, turn: &str, feed_id: i64, mid: Option<i64>) -> Receipt {
        engine_receipt(
            turn,
            feed_id,
            "agent",
            "the-helper-role",
            &scope_id_for_bot(root, "bot-one").unwrap(),
            mid,
            if mid.is_some() {
                RelayStatus::Delivered
            } else {
                RelayStatus::Failed
            },
            RelayOutcome::Send,
            ReplyPhase::Final,
            None,
            1_785_000_000_000,
        )
    }

    /// A RECEIPT LOST TO A COIN FLIP LEAVES AN UNPROVABLE ROW.
    ///
    /// The receipt is written on the SAME lock as the row it proves, and `wg
    /// telegram feed-write` takes that lock three times in one process (row,
    /// receipt, audience), so the receipt is the acquisition most likely to arrive
    /// after the budget has already been eaten. Losing it does not merely delay
    /// something: it leaves a line in the family's conversation that nothing can
    /// ever prove — the exact shape the ledger exists to eliminate.
    ///
    /// A holder that lets go inside the caller's patience must therefore cost the
    /// receipt nothing, and must not cost it TWO ledger lines: the retry is around
    /// the acquisition, so the contract checks in `append_locked` — including the
    /// one-row-one-receipt rule — run exactly once (docs/42 §9, `feed-lock-retry`).
    #[test]
    fn a_receipt_refused_by_a_momentary_holder_is_retried_and_lands_exactly_once() {
        let dir = scratch();
        let ledger = ledger_path_for(dir.path());
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();

        // A SECOND WRITER on a fresh thread — re-entrancy is keyed per (thread,
        // resolved path) (docs/42 §6), so a holder on this call stack would be
        // this same writer re-entering and would prove nothing about contention.
        let hold_ms = super::super::feed_lock::DEFAULT_WAIT_MS + 300;
        let (ready, held) = std::sync::mpsc::channel::<()>();
        let holder = {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                let lock = super::super::feed_lock::acquire(&ledger, 1000)
                    .expect("the other writer must acquire");
                ready.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                lock.release()
            })
        };
        held.recv().expect("the other writer must acquire");

        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11)))
            .expect("a momentary contention loss must not leave the row unprovable");

        assert_eq!(
            std::fs::read_to_string(&ledger).unwrap().lines().count(),
            1,
            "exactly one ledger line: a retry may not write the receipt twice"
        );
        assert_eq!(read_all(dir.path()).len(), 1);
        holder.join().unwrap();
    }

    /// Held for the WHOLE patience, the receipt still fails CLOSED with the same
    /// refusal and a byte-identical ledger. A retry budget may not launder a
    /// refusal into a success, and it may not become an unbounded wait.
    #[test]
    fn a_receipt_refused_for_the_whole_patience_fails_closed_with_an_untouched_ledger() {
        let dir = scratch();
        let ledger = ledger_path_for(dir.path());
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        // A NEGATIVE CONTROL on the same fixture: uncontended, this receipt lands.
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11)))
            .expect("the uncontended control must append");
        let unharmed = std::fs::read_to_string(&ledger).unwrap();

        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (ready, held) = std::sync::mpsc::channel::<()>();
        let holder = {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                let lock = super::super::feed_lock::acquire(&ledger, 1000)
                    .expect("the other writer must acquire");
                ready.send(()).unwrap();
                let _ = wait.recv();
                lock.release()
            })
        };
        held.recv().expect("the other writer must acquire");

        let started = std::time::Instant::now();
        let err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12)))
            .expect_err("a receipt that could not be serialised must NOT return Ok");
        let spent = started.elapsed();

        assert!(
            matches!(err, ReceiptError::NotSerialised(_)),
            "the refusal is unchanged by the retry budget: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&ledger).unwrap(),
            unharmed,
            "a refused append leaves the ledger byte-identical"
        );
        // LITERAL bounds, not `DEFAULT_WAIT_MS * DEFAULT_ATTEMPTS` — a floor
        // derived from the attempt count collapses to zero at `attempts = 1`,
        // the very build this asserts against. See the twin of this test in
        // `casa_feed`.
        assert!(
            spent >= std::time::Duration::from_millis(2000),
            "gave up after {spent:?} — less than two protocol budgets, so it did not retry"
        );
        assert!(
            spent < std::time::Duration::from_millis(5000),
            "waited {spent:?} — a bounded budget must be spendable, not endless"
        );
        let _ = go.send(());
        holder.join().unwrap();
    }

    // ── typed id shapes ─────────────────────────────────────────────────────

    #[test]
    fn typed_ids_reject_every_shape_the_schema_names() {
        assert!(is_valid_turn_id(TURN));
        assert!(is_valid_receipt_id(&mint_receipt_id()));
        // All-hyphens: the shape a placeholder takes when no id was minted.
        assert!(!is_valid_turn_id(
            "web-turn---------------------------------"
        ));
        // Wrong version nibble (v1, not v4) and wrong variant.
        assert!(!is_valid_turn_id(
            "web-turn-3f2504e0-4f89-11d3-9a0c-0305e82c3301"
        ));
        assert!(!is_valid_turn_id(
            "web-turn-3f2504e0-4f89-41d3-ca0c-0305e82c3301"
        ));
        // Uppercase hex is not the canonical form.
        assert!(!is_valid_turn_id(
            "web-turn-3F2504E0-4f89-41d3-9a0c-0305e82c3301"
        ));
        // A signed numeric — a chat id wearing a turn id's name.
        assert!(!is_valid_turn_id("-1002233445566"));
        assert!(!is_valid_receipt_id("-1002233445566"));
        assert!(!is_valid_scope_id("-1002233445566"));
        // A token-like and a plain name.
        assert!(!is_valid_scope_id(concat!(
            "123456",
            ":",
            "AA-Ee",
            "_ffffffffffffffffffffffffffff"
        )));
        assert!(!is_valid_scope_id("ts_the-helper-bot"));
        assert!(!is_valid_scope_id("the-helper-bot"));
        // The prefix alone is not the id.
        assert!(!is_valid_scope_id("ts_"));
        assert!(!is_valid_turn_id("web-turn-"));
    }

    // ── the transport scope id ──────────────────────────────────────────────

    /// THE DICTIONARY-REVERSAL CONTROL the schema requires by name: the scope id
    /// must not be the raw sha256 of an enumerable bot id.
    #[test]
    fn a_scope_id_is_keyed_and_never_the_raw_digest_of_the_bot_id() {
        let dir = scratch();
        let scope = scope_id_for_bot(dir.path(), "bot-one").unwrap();
        assert!(is_valid_scope_id(&scope), "{scope}");
        assert!(
            !is_dictionary_reversible(&scope, "bot-one"),
            "the scope id is a raw sha256 of the bot id — reversible by dictionary",
        );
        // …and the bot id itself never appears in it.
        assert!(!scope.contains("bot-one"), "{scope}");
    }

    #[test]
    fn a_scope_id_is_stable_per_install_and_distinct_per_bot() {
        let dir = scratch();
        let a1 = scope_id_for_bot(dir.path(), "bot-one").unwrap();
        let a2 = scope_id_for_bot(dir.path(), "bot-one").unwrap();
        let b = scope_id_for_bot(dir.path(), "bot-two").unwrap();
        assert_eq!(a1, a2, "the same bot must scope to the same id");
        assert_ne!(a1, b, "two bots must not share one transport scope");

        // A DIFFERENT install mints a different key, so one household's ledger
        // says nothing about another's.
        let other = scratch();
        assert_ne!(a1, scope_id_for_bot(other.path(), "bot-one").unwrap());
    }

    /// ITEM 3 — THE 64-CALLER / 64-SCOPE RACE, from the audit, as a permanent
    /// gate. Sixty-four synchronised first users of a FRESH install asked for
    /// the scope of one stable bot and got sixty-four DIFFERENT ids back
    /// (`distinct_returned_scopes=64`), because each minted its own key and
    /// returned it even though only one write persisted.
    ///
    /// One bot represented by many scopes defeats the replay guard outright: it
    /// keys on `(transportScopeId, messageId)`, so the SAME physical Telegram
    /// message re-claimed under a second scope is not seen as a replay at all.
    #[test]
    fn sixty_four_concurrent_first_users_all_get_the_one_persisted_scope() {
        let dir = scratch();
        let root = dir.path().to_path_buf();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(64));
        let mut handles = Vec::new();
        for _ in 0..64 {
            let root = root.clone();
            let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                // Synchronised, so they genuinely contend on the empty file.
                gate.wait();
                scope_id_for_bot(&root, "the-one-bot").unwrap()
            }));
        }
        let returned: std::collections::HashSet<String> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            returned.len(),
            1,
            "one bot, one transport scope — got {} distinct scopes",
            returned.len()
        );
        // And the id everyone got is the one the PERSISTED key produces: a
        // caller must never return a candidate that lost the mint.
        let persisted = scope_id_for_bot(&root, "the-one-bot").unwrap();
        assert_eq!(returned.into_iter().next().unwrap(), persisted);
    }

    /// The mint is CREATE-ONCE. A second caller never rewrites the key, because
    /// a rewrite would orphan every scope id already recorded under the first —
    /// the ledger would hold two names for one bot with nothing saying so.
    #[test]
    fn the_scope_key_is_minted_once_and_never_rewritten() {
        let dir = scratch();
        let first = scope_id_for_bot(dir.path(), "bot-one").unwrap();
        let key_after_first = std::fs::read(scope_key_path(dir.path())).unwrap();

        for _ in 0..8 {
            assert_eq!(scope_id_for_bot(dir.path(), "bot-one").unwrap(), first);
        }
        assert_eq!(
            std::fs::read(scope_key_path(dir.path())).unwrap(),
            key_after_first,
            "the key bytes were rewritten by a later caller"
        );
    }

    /// A DAMAGED key file fails CLOSED. Treating a short read as "no key yet"
    /// mints a second identity over the first, and every receipt already written
    /// under the old one silently stops joining.
    #[test]
    fn a_truncated_scope_key_fails_closed_instead_of_minting_a_second_identity() {
        let dir = scratch();
        let good = scope_id_for_bot(dir.path(), "bot-one").unwrap();
        let path = scope_key_path(dir.path());
        let damaged = std::fs::read(&path).unwrap()[..8].to_vec();
        std::fs::write(&path, &damaged).unwrap();

        let err = scope_id_for_bot(dir.path(), "bot-one").unwrap_err();
        assert!(
            matches!(err, ReceiptError::Io(ref m) if m.contains("refusing to mint a second identity")),
            "expected a fail-closed refusal, got {err:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            damaged,
            "the damaged key was overwritten — the old scope is now unjoinable"
        );
        // The cure is restoring the key, not minting a new one: once it is back,
        // the ORIGINAL scope id is what callers get.
        let _ = good;
    }

    /// The key is `0600` from the instant it exists — staged at that mode, not
    /// chmodded after publication. A crash in the window between "published at
    /// the ambient umask" and "chmod" would leave the one secret that keeps
    /// scope ids unreversible readable by anything on the box.
    #[cfg(unix)]
    #[test]
    fn the_scope_key_is_never_briefly_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        scope_id_for_bot(dir.path(), "bot-one").unwrap();
        let mode = std::fs::metadata(scope_key_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the scope key must be owner-only");
    }

    #[test]
    fn the_scope_key_never_reaches_the_ledger() {
        let dir = scratch();
        let r = receipt(dir.path(), TURN, 1, Some(11));
        append_certified(dir.path(), &r).unwrap();
        let key = std::fs::read(scope_key_path(dir.path())).unwrap();
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        assert!(
            !body.contains(&hex::encode(&key)),
            "the scope key leaked into the ledger"
        );
    }

    // ── what a receipt refuses ──────────────────────────────────────────────

    #[test]
    fn a_delivered_receipt_without_a_message_id_is_refused() {
        let dir = scratch();
        let mut r = receipt(dir.path(), TURN, 1, None);
        r.status = RelayStatus::Delivered;
        assert_eq!(
            append(dir.path(), &r),
            Err(ReceiptError::DeliveredWithoutMessageId)
        );
        // …including the success-0 the send path used to invent.
        let mut zero = receipt(dir.path(), TURN, 1, None);
        zero.status = RelayStatus::Delivered;
        zero.message_id = Some(0);
        assert_eq!(
            append(dir.path(), &zero),
            Err(ReceiptError::DeliveredWithoutMessageId)
        );
        assert!(
            read_all(dir.path()).is_empty(),
            "a refused receipt was written"
        );
    }

    /// A receipt that names no row proves nothing, and a guessed ordinal names
    /// the WRONG row — so the id must have been observed, never invented.
    #[test]
    fn a_receipt_without_an_observed_feed_id_is_refused() {
        let dir = scratch();
        let r = receipt(dir.path(), TURN, 0, Some(11));
        assert_eq!(append(dir.path(), &r), Err(ReceiptError::NoFeedId));
    }

    #[test]
    fn a_hashed_turn_id_is_refused_so_the_join_can_never_silently_miss() {
        let dir = scratch();
        // The engine's own durable digest shape — internal only, never the wire.
        let mut r = receipt(dir.path(), TURN, 1, Some(11));
        r.turn_id = crate::notify::telegram_conversation::durable_telegram_digest_v1(
            "telegram-delivery-claim",
            &[TURN, "bot-one", "-100"],
        );
        assert!(matches!(
            append(dir.path(), &r),
            Err(ReceiptError::BadShape {
                field: "turnId",
                ..
            })
        ));
    }

    // ── cardinality: one row, one receipt ───────────────────────────────────

    #[test]
    fn a_second_receipt_for_one_feed_row_is_refused() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 7, Some(11))).unwrap();
        let second = receipt(dir.path(), TURN2, 7, Some(12));
        assert!(matches!(
            append(dir.path(), &second),
            Err(ReceiptError::RowAlreadyProven { feed_id: 7, .. })
        ));
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    #[test]
    fn a_reused_receipt_id_is_refused() {
        let dir = scratch();
        let first = receipt(dir.path(), TURN, 1, Some(11));
        append_certified(dir.path(), &first).unwrap();
        let mut clone = receipt(dir.path(), TURN2, 2, Some(12));
        clone.receipt_id = first.receipt_id.clone();
        assert!(matches!(
            append(dir.path(), &clone),
            Err(ReceiptError::ReceiptIdReused { .. })
        ));
    }

    // ── the replay guard ────────────────────────────────────────────────────

    /// One Telegram message is delivered ONCE. Two receipts claiming the same
    /// (scope, message id) means an old delivery was claimed a second time.
    #[test]
    fn the_same_delivery_cannot_be_certified_twice() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(4242))).unwrap();
        let replayed = receipt(dir.path(), TURN2, 2, Some(4242));
        assert!(
            matches!(
                append(dir.path(), &replayed),
                Err(ReceiptError::Replay { .. })
            ),
            "an old delivery was re-certified under a new turn",
        );
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    /// …but the SAME message id from a DIFFERENT bot is a different delivery.
    /// Telegram message ids are per-chat, not global; keying the guard on the id
    /// alone would suppress a genuine second bot's real receipt.
    #[test]
    fn the_same_message_id_from_another_bot_is_a_different_delivery() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(4242))).unwrap();
        let mut other_bot = receipt(dir.path(), TURN2, 2, Some(4242));
        other_bot.transport_scope_id = scope_id_for_bot(dir.path(), "bot-two").unwrap();
        append_certified(dir.path(), &other_bot)
            .expect("a genuine second bot's receipt was suppressed");
        assert_eq!(read_all(dir.path()).len(), 2);
    }

    // ── the edit succession: one message, several phases ────────────────────

    /// One ack receipt, ready to be edited forward.
    fn ack_at(root: &Path, feed_id: i64, mid: i64, attempt: &str) -> Receipt {
        let mut r = receipt(root, TURN, feed_id, Some(mid));
        r.reply_phase = ReplyPhase::Ack;
        r.outcome = RelayOutcome::Send;
        r.attempt_id = Some(attempt.to_string());
        r
    }

    /// A later phase of the SAME physical message — the edit.
    fn edited_to(
        root: &Path,
        turn: &str,
        feed_id: i64,
        mid: i64,
        phase: ReplyPhase,
        attempt: &str,
    ) -> Receipt {
        let mut r = receipt(root, turn, feed_id, Some(mid));
        r.reply_phase = phase;
        r.outcome = RelayOutcome::Edit;
        r.attempt_id = Some(attempt.to_string());
        r
    }

    /// **THE LIVE DEFECT** (certification run 3, segment 5, reproduced 3×): the
    /// heavy lane posts an ack and EDITS that same message into the final, so
    /// the final carries the ack's `(scope, message id)` — and the replay guard
    /// refused the final's receipt as a replay of its own ack. The delivery had
    /// already reached the family; only the evidence was refused, leaving the
    /// turn's reservation held and the turn finalless.
    #[test]
    fn edit_succession_lets_the_final_land_over_its_own_ack() {
        let dir = scratch();
        let ack = ack_at(dir.path(), 1, 4242, ATTEMPT_ONE);
        append_certified(dir.path(), &ack).unwrap();

        let final_ = edited_to(dir.path(), TURN, 2, 4242, ReplyPhase::Final, ATTEMPT_ONE);
        append_certified(dir.path(), &final_)
            .expect("the final was refused as a replay of the ack it edited");

        let all = read_all(dir.path());
        assert_eq!(all.len(), 2, "both phases of the one message are recorded");
        assert_eq!(
            all.iter()
                .filter(|r| r.reply_phase == ReplyPhase::Final)
                .count(),
            1,
            "exactly one final per accepted turn",
        );
        // …and the succession is still ONE physical delivery: both rows carry
        // the same message id, which is precisely why the guard tripped.
        assert!(all.iter().all(|r| r.message_id == Some(4242)));
        // A refire asking "what did the family end up with" gets the answer.
        let evidence = evidence_for_attempt(dir.path(), TURN, Some(ATTEMPT_ONE)).unwrap();
        assert_eq!(evidence.receipt_id, final_.receipt_id);
        assert_eq!(evidence.reply_phase, ReplyPhase::Final);
    }

    /// The three-phase shape: `ack → watchdog → final`, all on one message. The
    /// final faces TWO prior holders of the key, and both must be recognised.
    #[test]
    fn edit_succession_survives_a_watchdog_between_the_ack_and_the_final() {
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 77, ATTEMPT_ONE)).unwrap();
        append_certified(
            dir.path(),
            &edited_to(dir.path(), TURN, 2, 77, ReplyPhase::Watchdog, ATTEMPT_ONE),
        )
        .expect("the watchdog line edited over the ack was refused");
        append_certified(
            dir.path(),
            &edited_to(dir.path(), TURN, 3, 77, ReplyPhase::Final, ATTEMPT_ONE),
        )
        .expect("the final was refused behind two legitimate predecessors");
        assert_eq!(read_all(dir.path()).len(), 3);
    }

    /// …AND THE GUARD STILL HAS TEETH. The same final arriving twice is a
    /// refire, not a succession: equal phases are not strictly earlier.
    #[test]
    fn edit_succession_still_refuses_the_same_final_arriving_twice() {
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 4242, ATTEMPT_ONE)).unwrap();
        let final_ = edited_to(dir.path(), TURN, 2, 4242, ReplyPhase::Final, ATTEMPT_ONE);
        append_certified(dir.path(), &final_).unwrap();

        // A fresh receipt id and a fresh feed id, so nothing but the replay
        // guard can be what refuses this.
        let again = edited_to(dir.path(), TURN, 3, 4242, ReplyPhase::Final, ATTEMPT_ONE);
        assert!(
            matches!(append(dir.path(), &again), Err(ReceiptError::Replay { .. })),
            "a refire of the final was admitted as an edit succession",
        );
        assert_eq!(read_all(dir.path()).len(), 2);
    }

    /// A DIFFERENT TURN presenting the same `(scope, message id)` is refused
    /// exactly as before — the exemption is scoped to one turn's own message.
    #[test]
    fn edit_succession_refuses_a_different_turn_reusing_the_message_id() {
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 4242, ATTEMPT_ONE)).unwrap();
        let stranger = edited_to(dir.path(), TURN2, 2, 4242, ReplyPhase::Final, ATTEMPT_ONE);
        assert!(
            matches!(
                append(dir.path(), &stranger),
                Err(ReceiptError::Replay { .. })
            ),
            "another turn re-certified this turn's delivery",
        );
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    /// A different ATTEMPT of the same turn is refused too: a self-heal retry
    /// re-presenting the first attempt's message id is claiming a delivery it
    /// did not make.
    #[test]
    fn edit_succession_refuses_another_attempt_of_the_same_turn() {
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 4242, ATTEMPT_ONE)).unwrap();
        let retry = edited_to(dir.path(), TURN, 2, 4242, ReplyPhase::Final, ATTEMPT_TWO);
        assert!(
            matches!(append(dir.path(), &retry), Err(ReceiptError::Replay { .. })),
            "a second attempt re-certified the first attempt's delivery",
        );
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    /// And a SEND is never a succession. A send mints a new message id, so a
    /// send presenting a recorded one is the replay this guard exists for —
    /// including the `fallback` send after an edit that could not be applied,
    /// whose whole point is that it carries a DIFFERENT id.
    #[test]
    fn edit_succession_is_only_for_edits_never_for_a_send_or_fallback() {
        for outcome in [RelayOutcome::Send, RelayOutcome::Fallback] {
            let dir = scratch();
            append_certified(dir.path(), &ack_at(dir.path(), 1, 4242, ATTEMPT_ONE)).unwrap();
            let mut claim = edited_to(dir.path(), TURN, 2, 4242, ReplyPhase::Final, ATTEMPT_ONE);
            claim.outcome = outcome;
            assert!(
                matches!(append(dir.path(), &claim), Err(ReceiptError::Replay { .. })),
                "a {} claiming a recorded message id was admitted",
                outcome.as_str(),
            );
            assert_eq!(read_all(dir.path()).len(), 1);
        }
    }

    /// The phase order the exemption reads, asserted directly: the two TERMINAL
    /// phases share a rank, so neither can succeed the other and a second
    /// terminal claim over one message stays refused.
    #[test]
    fn edit_succession_orders_the_phases_and_ranks_both_terminals_equal() {
        use ReplyPhase::*;
        assert!(Ack.succession_rank() < Watchdog.succession_rank());
        assert!(Watchdog.succession_rank() < Final.succession_rank());
        assert!(Ack.succession_rank() < Failure.succession_rank());
        assert_eq!(Final.succession_rank(), Failure.succession_rank());

        // A failure notice edited over a recorded FINAL is refused: the answer
        // is already the family's, and overwriting its evidence is not a phase
        // advance.
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 9, ATTEMPT_ONE)).unwrap();
        append_certified(
            dir.path(),
            &edited_to(dir.path(), TURN, 2, 9, ReplyPhase::Final, ATTEMPT_ONE),
        )
        .unwrap();
        assert!(matches!(
            append(
                dir.path(),
                &edited_to(dir.path(), TURN, 3, 9, ReplyPhase::Failure, ATTEMPT_ONE)
            ),
            Err(ReceiptError::Replay { .. })
        ));
        assert_eq!(read_all(dir.path()).len(), 2);
    }

    /// THE FIFTH PHASE'S RANK IS A CROSS-REPO CONSTRAINT, NOT A PREFERENCE.
    ///
    /// The receipt index is appended to by both writers, and the gateway's port
    /// of `succession_rank` (`receiptLedger.successionRank`) is spelled `ack`=0,
    /// `watchdog`=1, EVERYTHING ELSE = 2 — so a reader that has never heard the
    /// word `addendum` ranks it 2 by falling through. Knowing the word must not
    /// change the number. If it did, one line in one shared file would be a
    /// succession to one implementation and a replay to the other, and only one
    /// of them would refuse the row.
    #[test]
    fn addendum_ranks_terminal_so_both_implementations_agree_on_one_shared_line() {
        use ReplyPhase::*;
        assert_eq!(
            Addendum.succession_rank(),
            Final.succession_rank(),
            "the gateway's unknown-phase fall-through ranks `addendum` 2; so must we"
        );
        // NON-VACUITY: the equality above would also hold if every phase ranked
        // the same. It does not — the ordering the exemption reads is real.
        assert!(Ack.succession_rank() < Addendum.succession_rank());
        assert!(Watchdog.succession_rank() < Addendum.succession_rank());

        // …and therefore an addendum can never be the earlier phase a later edit
        // succeeds. An addendum is its own Telegram message, so it should never
        // face this test at all; if it somehow does, terminal is the fail-closed
        // answer and the row is refused as the replay it looks like.
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 9, ATTEMPT_ONE)).unwrap();
        append_certified(
            dir.path(),
            &edited_to(dir.path(), TURN, 2, 9, ReplyPhase::Addendum, ATTEMPT_ONE),
        )
        .unwrap();
        assert!(
            matches!(
                append(
                    dir.path(),
                    &edited_to(dir.path(), TURN, 3, 9, ReplyPhase::Final, ATTEMPT_ONE)
                ),
                Err(ReceiptError::Replay { .. })
            ),
            "a final edited over a recorded addendum's message id was accepted"
        );
        assert_eq!(read_all(dir.path()).len(), 2);
    }

    /// THE WORD ON THE WIRE IS `addendum`, in both directions.
    ///
    /// `replyPhase` is an index field two implementations parse. A variant that
    /// serialised as `Addendum` (serde's default) would be written by us and
    /// read as malformed by the gateway's `optEnum(obj.replyPhase, REPLY_PHASES)`
    /// — a whole index line dropped, and the refire guard is a search over that
    /// list.
    #[test]
    fn addendum_round_trips_as_the_lowercase_schema_word() {
        assert_eq!(ReplyPhase::Addendum.as_str(), "addendum");
        assert_eq!(
            serde_json::to_string(&ReplyPhase::Addendum).unwrap(),
            "\"addendum\""
        );
        assert_eq!(
            serde_json::from_str::<ReplyPhase>("\"addendum\"").unwrap(),
            ReplyPhase::Addendum
        );
        // NON-VACUITY: the parser is genuinely closed — it did not simply accept
        // whatever it was handed and hand back a default.
        assert!(serde_json::from_str::<ReplyPhase>("\"Addendum\"").is_err());
        assert!(serde_json::from_str::<ReplyPhase>("\"companion\"").is_err());
    }

    /// FAIL CLOSED on evidence we cannot join. A prior receipt with no index
    /// line — what a GATEWAY-written row looks like from here — keeps the
    /// placeholder phase and no attempt, so it can never be mistaken for an
    /// earlier phase of this attempt and its message id stays protected.
    #[test]
    fn edit_succession_cannot_be_claimed_over_an_unjoinable_prior() {
        let dir = scratch();
        append_certified(dir.path(), &ack_at(dir.path(), 1, 4242, ATTEMPT_ONE)).unwrap();
        // Drop the index — the ack's phase/attempt are no longer knowable.
        std::fs::write(index_path_for(dir.path()), "").unwrap();

        let final_ = edited_to(dir.path(), TURN, 2, 4242, ReplyPhase::Final, ATTEMPT_ONE);
        assert!(
            matches!(
                append(dir.path(), &final_),
                Err(ReceiptError::Replay { .. })
            ),
            "an unjoinable prior was read as an earlier phase of this attempt",
        );
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    #[test]
    fn the_replay_key_is_the_schemas_key() {
        use sha2::{Digest, Sha256};
        let want = hex::encode(Sha256::digest(b"ts_abc\x0042"));
        assert_eq!(replay_key("ts_abc", 42), want);
    }

    // ── attempts: a retry is not a refire ───────────────────────────────────

    /// THE SELF-HEAL CASE. A retry after a genuine failure is what actually
    /// reached the family; suppressing its receipt as a duplicate would erase
    /// the only evidence of the delivery that worked.
    #[test]
    fn a_self_heal_retry_writes_its_own_receipt() {
        let dir = scratch();
        let mut first = receipt(dir.path(), TURN, 1, None);
        first.status = RelayStatus::Failed;
        first.attempt_id = Some(ATTEMPT_ONE.into());
        append_certified(dir.path(), &first).unwrap();

        let mut retry = receipt(dir.path(), TURN, 2, Some(99));
        retry.attempt_id = Some(ATTEMPT_TWO.into());
        append_certified(dir.path(), &retry).expect("the self-heal retry's receipt was suppressed");

        let all = read_all(dir.path());
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].status, RelayStatus::Failed);
        assert_eq!(all[1].status, RelayStatus::Delivered);
        assert_eq!(all[1].message_id, Some(99));
    }

    /// …while a REFIRE of the same attempt is suppressed.
    #[test]
    fn a_refire_of_the_same_attempt_is_refused() {
        let dir = scratch();
        let mut first = receipt(dir.path(), TURN, 1, Some(11));
        first.attempt_id = Some(ATTEMPT_ONE.into());
        append_certified(dir.path(), &first).unwrap();

        let mut refire = receipt(dir.path(), TURN, 2, Some(12));
        refire.attempt_id = Some(ATTEMPT_ONE.into());
        assert!(matches!(
            append(dir.path(), &refire),
            Err(ReceiptError::AttemptAlreadyRecorded { .. })
        ));
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    /// An absent attempt id means attempt 1 — so a caller that supplies none is
    /// not silently exempt from the refire guard.
    #[test]
    fn no_attempt_id_means_attempt_one() {
        let f = ReplyPhase::Final;
        assert_eq!(attempt_key(TURN, None, f), attempt_key(TURN, Some("1"), f));
        assert_eq!(attempt_key(TURN, Some("  "), f), attempt_key(TURN, None, f));
        assert_ne!(attempt_key(TURN, Some("2"), f), attempt_key(TURN, None, f));
    }

    /// …and the PHASE is part of the key. Two phases of one attempt are two
    /// physical deliveries, so they are two receipts — the ack does not make
    /// the turn's own answer look like a refire of itself.
    #[test]
    fn each_delivered_phase_of_one_attempt_is_its_own_receipt() {
        for (a, b) in [
            (ReplyPhase::Ack, ReplyPhase::Final),
            (ReplyPhase::Final, ReplyPhase::Watchdog),
            (ReplyPhase::Watchdog, ReplyPhase::Failure),
            // A two-voice answer is TWO physical messages on one attempt. If the
            // companion shared the answer's key it would be refused as a refire
            // of the very row it accompanies — the second voice silently gone.
            (ReplyPhase::Final, ReplyPhase::Addendum),
        ] {
            assert_ne!(
                attempt_key(TURN, Some(ATTEMPT_ONE), a),
                attempt_key(TURN, Some(ATTEMPT_ONE), b)
            );
        }
        let dir = scratch();
        let mut ack = receipt(dir.path(), TURN, 1, Some(11));
        ack.reply_phase = ReplyPhase::Ack;
        ack.attempt_id = Some(ATTEMPT_ONE.to_string());
        append_certified(dir.path(), &ack).unwrap();

        let mut answer = receipt(dir.path(), TURN, 2, Some(12));
        answer.reply_phase = ReplyPhase::Final;
        answer.attempt_id = Some(ATTEMPT_ONE.to_string());
        append_certified(dir.path(), &answer).expect("the final is not a refire of its own ack");

        // …but the SAME phase twice still is one.
        let mut refire = receipt(dir.path(), TURN, 3, Some(13));
        refire.reply_phase = ReplyPhase::Final;
        refire.attempt_id = Some(ATTEMPT_ONE.to_string());
        assert!(matches!(
            append(dir.path(), &refire),
            Err(ReceiptError::AttemptAlreadyRecorded { .. })
        ));

        // The refire bypass reports the ANSWER, not the ack: what the family
        // ended up with is the question a refire is asking.
        let evidence = evidence_for_attempt(dir.path(), TURN, Some(ATTEMPT_ONE)).unwrap();
        assert_eq!(evidence.reply_phase, ReplyPhase::Final);
        assert_eq!(evidence.receipt_id, answer.receipt_id);
    }

    // ── what the ledger holds ───────────────────────────────────────────────

    /// The keys of one serialised ledger line, in the order the BYTES carry
    /// them. `serde_json::Map` is a `BTreeMap` here, so a re-parse sorts the
    /// keys and cannot answer the ordering question at all — asking it would
    /// have made this assertion quietly weaker than it reads.
    fn wire_keys(line: &str) -> Vec<String> {
        let parsed: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line.trim()).expect("the ledger line parses as an object");
        let mut keys: Vec<String> = parsed.into_iter().map(|(k, _)| k).collect();
        keys.sort_by_key(|k| {
            line.find(&format!("\"{k}\":"))
                .expect("every parsed key appears in the bytes")
        });
        keys
    }

    /// `receipt-observe-schema-v9.1.json` `receipt_fields`, verbatim and in the
    /// order the twin serialises them.
    const V9_1_RECEIPT_FIELDS: [&str; 10] = [
        "receiptId",
        "turnId",
        "feedId",
        "feedKind",
        "roleId",
        "transportScopeId",
        "messageId",
        "acceptedAtMs",
        "status",
        "provenance",
    ];

    /// THE FIELD SET IS EXHAUSTIVE, both directions. This test used to require
    /// `replyPhase` on the line — it was one of the tests the exact-tree audit
    /// named as blessing schema drift rather than closing it. The schema's
    /// `receipt_fields` is a closed object, and the gateway twin validates every
    /// line against exactly this list, so a stranger key here is a line the twin
    /// reads as malformed — which then makes the twin's own writer refuse to
    /// append at all.
    #[test]
    fn the_ledger_line_carries_exactly_the_schemas_receipt_fields() {
        let dir = scratch();
        let r = receipt(dir.path(), TURN, 5, Some(11));
        append_certified(dir.path(), &r).unwrap();
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        let line: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(body.trim()).expect("the ledger line parses as an object");

        assert_eq!(
            wire_keys(&body),
            V9_1_RECEIPT_FIELDS,
            "no strangers, no absentees, and in the twin's order"
        );
        // The three the writer used to add. Named individually so a regression
        // reads as what it is.
        for stranger in ["outcome", "replyPhase", "attemptId"] {
            assert!(
                !line.contains_key(stranger),
                "{stranger} is not a v9.1 receipt field: {body}"
            );
        }
        // …and `messageId` is PRESENT, not omitted, even when there is none.
        let unproven = receipt(dir.path(), TURN2, 6, None);
        append_certified(dir.path(), &unproven).unwrap();
        let whole = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        let last = whole.lines().next_back().unwrap();
        assert_eq!(wire_keys(last), V9_1_RECEIPT_FIELDS);
        let last: serde_json::Map<String, serde_json::Value> = serde_json::from_str(last).unwrap();
        assert!(last["messageId"].is_null());

        // The raw turn id is on the wire VERBATIM — a hashed one cannot join.
        assert!(body.contains(TURN), "{body}");
        assert!(body.contains("\"provenance\":\"engine\""), "{body}");
        // And the round trip still yields the receipt we wrote, correlation and
        // all: the phase/outcome/attempt came back from the index.
        assert_eq!(read_all(dir.path())[0], r);
    }

    /// The correlation the ledger may not carry is IN THE INDEX, and the index
    /// line is one the twin can read: its own three fields, correctly shaped.
    #[test]
    fn the_correlation_moves_to_the_index_in_a_shape_the_twin_reads() {
        let dir = scratch();
        let mut r = receipt(dir.path(), TURN, 5, Some(11));
        r.outcome = RelayOutcome::Fallback;
        r.reply_phase = ReplyPhase::Watchdog;
        r.attempt_id = Some(ATTEMPT_ONE.to_string());
        append_certified(dir.path(), &r).unwrap();

        let body = std::fs::read_to_string(index_path_for(dir.path())).unwrap();
        let entry: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(body.trim()).unwrap();
        // The twin's three, exactly as `readReceiptIndexFile` checks them.
        assert_eq!(entry["receiptId"], serde_json::json!(r.receipt_id));
        assert_eq!(entry["feedId"], serde_json::json!(5));
        assert_eq!(
            entry["replayKey"],
            serde_json::json!(replay_key(&r.transport_scope_id, 11))
        );
        // …and ours.
        assert_eq!(entry["replyPhase"], serde_json::json!("watchdog"));
        assert_eq!(entry["outcome"], serde_json::json!("fallback"));
        assert_eq!(entry["attemptId"], serde_json::json!(ATTEMPT_ONE));

        // The join puts them back on the receipt.
        let back = &read_all(dir.path())[0];
        assert_eq!(back.outcome, RelayOutcome::Fallback);
        assert_eq!(back.reply_phase, ReplyPhase::Watchdog);
        assert_eq!(back.attempt_id.as_deref(), Some(ATTEMPT_ONE));
    }

    /// A damaged INDEX fails closed exactly like a damaged ledger. The refire
    /// guard is a search over the index now, and a line silently dropped from it
    /// is a suppressed refire becoming a second message on the family's screen.
    #[test]
    fn a_damaged_index_authorises_no_receipt() {
        for (label, mutate) in [
            (
                "a torn tail",
                Box::new(|b: String| b.trim_end_matches('\n').to_string())
                    as Box<dyn Fn(String) -> String>,
            ),
            (
                "a line that does not parse",
                Box::new(|b: String| b.replace("\"receiptId\"", "\"receipt")),
            ),
        ] {
            let dir = scratch();
            append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
            let path = index_path_for(dir.path());
            let before = mutate(std::fs::read_to_string(&path).unwrap());
            std::fs::write(&path, &before).unwrap();

            // Retry a LOCK refusal, exactly as a real writer does. Under a
            // loaded suite the shared feed lock's wait budget is sometimes
            // starved, and `NotSerialised` is a correct fail-closed answer that
            // simply has not reached the index read yet — accepting it as the
            // damage verdict would make this gate pass for the wrong reason.
            let mut err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap_err();
            for _ in 0..50 {
                if !matches!(err, ReceiptError::NotSerialised(_)) {
                    break;
                }
                err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap_err();
            }
            assert!(
                matches!(err, ReceiptError::LedgerCorrupt { .. }),
                "{label} in the index still authorised a receipt: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                before,
                "{label}: the refusal left the index byte-identical"
            );
        }
    }

    /// A torn tail must not satisfy a join. A half-written line is skipped, not
    /// read as a receipt.
    #[test]
    fn a_truncated_line_is_never_read_as_a_receipt() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        let path = ledger_path_for(dir.path());
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"receiptId\":\"rcpt_3f2504e0-4f89-41d3-9a0c-030\n");
        std::fs::write(&path, body).unwrap();
        assert_eq!(read_all(dir.path()).len(), 1);
    }

    /// ITEM 4 — THE AUDIT'S CORRUPTION REPRO, as a permanent gate.
    ///
    /// `visible_before=0 duplicate_append_succeeded=true visible_after=1`: after
    /// the one line proving feed row 7 / message 4242 was corrupted, an IDENTICAL
    /// second proof was accepted. Damaged evidence had been read as no evidence,
    /// so the delivery could be certified twice — and the second certificate
    /// looked exactly as authoritative as the first.
    #[test]
    fn a_corrupted_proof_blocks_a_new_claim_instead_of_licensing_a_duplicate() {
        let dir = scratch();
        let first = receipt(dir.path(), TURN, 7, Some(4242));
        append_certified(dir.path(), &first).unwrap();

        // Corrupt the proof exactly as the reproducer did: the row is still
        // there, it just no longer parses.
        let path = ledger_path_for(dir.path());
        let body = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, body.replace("\"receiptId\"", "\"receipt")).unwrap();

        // The lenient reader now sees NOTHING (this is the false-clean read that
        // made the duplicate look legitimate)...
        assert_eq!(
            read_all(dir.path()).len(),
            0,
            "visible_before=0, as audited"
        );

        // ...and the strict one, which is what `append` uses, says DAMAGED.
        assert!(matches!(
            read_strict(dir.path()),
            Err(ReceiptError::LedgerCorrupt { line: 1, .. })
        ));

        // So the second claim of the very same delivery is REFUSED.
        let duplicate = receipt(dir.path(), TURN, 7, Some(4242));
        let err = append(dir.path(), &duplicate).unwrap_err();
        assert!(
            matches!(err, ReceiptError::LedgerCorrupt { .. }),
            "a duplicate was admitted over damaged evidence: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "the refusal left the ledger byte-identical"
        );
    }

    /// THE EXACT-TREE CONTROL, as a permanent gate: a receipt line carrying a
    /// key the schema does not define is DAMAGE, not a receipt with a bonus.
    ///
    /// The control added `"unknownKey":"must-reject"` to a written receipt and
    /// asked the same binary for a second one; it got
    /// `writerAcceptedAndAppendedSecondReceipt: true`. That is a writer
    /// certifying against evidence it did not understand — the unknown key can
    /// be a claim of a different contract, a partial record from another
    /// implementation, or a forgery, and "ignore it" chooses one of those
    /// readings silently.
    #[test]
    fn an_unknown_receipt_key_is_damage_and_authorises_nothing() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        let path = ledger_path_for(dir.path());
        let body = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            body.replace("}\n", ",\"unknownKey\":\"must-reject\"}\n"),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        assert!(
            matches!(
                read_strict(dir.path()),
                Err(ReceiptError::LedgerCorrupt { line: 1, .. })
            ),
            "an unknown key was read as a valid receipt"
        );
        let err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap_err();
        assert!(
            matches!(err, ReceiptError::LedgerCorrupt { .. }),
            "an unknown key still authorised a new receipt: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the refusal left the ledger byte-identical"
        );
    }

    /// …and the same for a record interrupted AT THE DELIMITER: valid JSON, no
    /// terminating newline.
    ///
    /// This is the sharpest of the damaged shapes because the old reader could
    /// not see it at all — `lines()` yields the same string for a finished
    /// record and a torn one — and because appending onto it WELDS two records
    /// into one unparsable line. The exact-tree control got `receipt=written`,
    /// `physicalLines: 1`, `ledgerParses: false`: detectable uncertainty turned
    /// into fresh corruption, reported as success.
    #[test]
    fn a_record_interrupted_at_the_delimiter_is_refused_without_welding_a_second_onto_it() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        let path = ledger_path_for(dir.path());
        let whole = std::fs::read_to_string(&path).unwrap();
        // Interrupt exactly at the delimiter: the record's bytes are all there,
        // its newline never became durable.
        let torn = whole.trim_end_matches('\n').to_string();
        std::fs::write(&path, &torn).unwrap();

        assert!(
            matches!(
                read_strict(dir.path()),
                Err(ReceiptError::LedgerCorrupt { .. })
            ),
            "a record with no terminating newline was read as committed"
        );
        let err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap_err();
        assert!(
            matches!(err, ReceiptError::LedgerCorrupt { .. }),
            "an interrupted delimiter still authorised a new receipt: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            torn,
            "the refusal left the ledger byte-identical — no second record welded on"
        );
    }

    /// The negative half of the two rules above: a ledger the writer itself
    /// produced still reads clean. Fail-closed must not mean fail-always.
    #[test]
    fn a_well_formed_terminated_ledger_still_reads_and_accepts() {
        let dir = scratch();
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        append_certified(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap();
        assert_eq!(read_strict(dir.path()).unwrap().len(), 2);
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        assert!(body.ends_with('\n'), "every record carries its delimiter");
    }

    /// Every damaged shape fails closed, and each names WHERE — an unreadable
    /// file, a torn tail, a line that is JSON but not a receipt, a line of
    /// nonsense. None of them may authorise a new receipt.
    #[test]
    fn every_damaged_ledger_shape_fails_closed_and_none_authorises_a_receipt() {
        for (label, tail) in [
            (
                "a torn tail",
                "{\"receiptId\":\"rcpt_3f2504e0-4f89-41d3-9a0",
            ),
            ("json that is not a receipt", "{\"hello\":\"world\"}"),
            ("not json at all", "<<< a log line landed in the ledger"),
            ("a stray NUL-ish blob", "\u{1}\u{2}\u{3}"),
        ] {
            let dir = scratch();
            append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
            let path = ledger_path_for(dir.path());
            let mut body = std::fs::read_to_string(&path).unwrap();
            body.push_str(tail);
            body.push('\n');
            std::fs::write(&path, &body).unwrap();

            assert!(
                matches!(
                    read_strict(dir.path()),
                    Err(ReceiptError::LedgerCorrupt { line: 2, .. })
                ),
                "{label} was not reported as damage"
            );
            let err = append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap_err();
            assert!(
                matches!(err, ReceiptError::LedgerCorrupt { .. }),
                "{label} still authorised a new receipt: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                body,
                "{label}: the refusal must leave the ledger byte-identical"
            );
        }
    }

    /// A ledger that does not EXIST is not damage — an install with no receipts
    /// yet is an ordinary fact, and refusing there would mean no first receipt
    /// could ever be written.
    #[test]
    fn an_absent_ledger_is_empty_not_damaged() {
        let dir = scratch();
        assert_eq!(read_strict(dir.path()).unwrap().len(), 0);
        append_certified(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        assert_eq!(read_strict(dir.path()).unwrap().len(), 1);
    }

    /// THE AUDIT'S WRITE RACE, as a permanent gate. Sixty-four synchronised
    /// writers submitted the same `feedId`, the same `(scope, messageId)` and the
    /// same `(turn, attempt)` with unique receipt ids. TEN calls returned
    /// success, the file held concatenated JSON that no longer parsed, and both
    /// of the two rows still readable violated all three uniqueness rules.
    ///
    /// Under the shared lock and the single-write append: exactly ONE success,
    /// every line parses, and the duplicate claims are refused.
    #[test]
    fn sixty_four_racing_duplicate_claims_admit_exactly_one() {
        let dir = scratch();
        let root = dir.path().to_path_buf();
        // Mint the scope key first, so the race is over the LEDGER and not over
        // the key (that race has its own test).
        let scope = scope_id_for_bot(&root, "bot-one").unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(64));

        let mut handles = Vec::new();
        for _ in 0..64 {
            let root = root.clone();
            let scope = scope.clone();
            let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                let claim = engine_receipt(
                    TURN,
                    9,
                    "agent",
                    "the-helper-role",
                    &scope,
                    Some(4242),
                    RelayStatus::Delivered,
                    RelayOutcome::Send,
                    ReplyPhase::Final,
                    None,
                    1_785_000_000_000,
                );
                gate.wait();
                append(&root, &claim).is_ok()
            }));
        }
        let successes = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();

        assert_eq!(successes, 1, "one delivery, one receipt — got {successes}");
        // EVERY line parses: no interleaved half-records.
        let parsed = read_strict(&root).expect("the ledger must still be wholly readable");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].feed_id, 9);
        assert_eq!(parsed[0].message_id, Some(4242));
    }

    /// A COUNTED attempt id is refused. `"1"`/`"2"` are what the pre-fix retry
    /// tests used, and they collide across unrelated processes by construction:
    /// one turn's genuine second attempt then keys the same as another's first,
    /// and its receipt — the evidence for the send that actually reached the
    /// family — is suppressed as a refire.
    #[test]
    fn a_counted_or_malformed_attempt_id_is_refused_not_silently_keyed() {
        for bad in [
            "1",
            "2",
            "attempt-1",
            "attempt-00000000-0000-0000-0000-000000000000",
            "attempt-6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "6ba7b810-9dad-41d1-80b4-00c04fd430c8",
        ] {
            let dir = scratch();
            let mut r = receipt(dir.path(), TURN, 1, Some(11));
            r.attempt_id = Some(bad.to_string());
            let err = append(dir.path(), &r).unwrap_err();
            assert!(
                matches!(
                    err,
                    ReceiptError::BadShape {
                        field: "attemptId",
                        ..
                    }
                ),
                "attempt id {bad:?} must be refused, got {err:?}"
            );
            assert!(
                !ledger_path_for(dir.path()).exists(),
                "a refused attempt id must leave NO receipt"
            );
        }
        // The positive control: a MINTED attempt is accepted.
        let dir = scratch();
        let mut good = receipt(dir.path(), TURN, 1, Some(11));
        good.attempt_id = Some(ATTEMPT_ONE.to_string());
        append_certified(dir.path(), &good).unwrap();
        assert_eq!(read_strict(dir.path()).unwrap().len(), 1);
    }

    /// A rejected id is NEVER echoed. The value can be a token-shaped paste or a
    /// raw household identifier, and the refusal is often the thing that ends up
    /// in an operator log — the field and the shape it failed are what a human
    /// needs, and all they should get.
    #[test]
    fn a_refusal_names_the_field_and_the_shape_never_the_value() {
        let secret = "web-turn-A_SECRET_LOOKING_VALUE_1234567890";
        let rendered = ReceiptError::BadShape {
            field: "turnId",
            value: secret.to_string(),
        }
        .to_string();
        assert!(
            !rendered.contains("A_SECRET_LOOKING_VALUE"),
            "the rejected value was reproduced verbatim: {rendered}"
        );
        assert!(rendered.contains("turnId"), "{rendered}");
        assert!(rendered.contains("web-turn-<uuid v4>"), "{rendered}");
        assert!(rendered.contains("characters"), "{rendered}");
    }

    #[test]
    fn typed_outcomes_and_phases_are_closed_and_stable_on_the_wire() {
        let dir = scratch();
        for (i, (outcome, phase)) in [
            (RelayOutcome::Send, ReplyPhase::Ack),
            (RelayOutcome::Edit, ReplyPhase::Final),
            (RelayOutcome::Fallback, ReplyPhase::Watchdog),
            (RelayOutcome::Send, ReplyPhase::Failure),
            // The v9.2 member. A phase the index cannot spell is a correlation
            // line the OTHER writer drops as malformed, and the refire guard is
            // a search over that list.
            (RelayOutcome::Send, ReplyPhase::Addendum),
        ]
        .iter()
        .enumerate()
        {
            let mut r = receipt(dir.path(), TURN, i as i64 + 1, Some(i as i64 + 100));
            r.outcome = *outcome;
            r.reply_phase = *phase;
            // A DISTINCT minted attempt per row: the dedupe key is
            // `(turn, attempt)`, so reusing one here would refuse the later rows
            // for the right reason and prove nothing about the wire shape.
            r.attempt_id = Some(format!("attempt-6ba7b810-9dad-41d1-80b4-00c04fd430c{i}"));
            append_certified(dir.path(), &r).unwrap();
        }
        // The tokens are stable on the wire — in the INDEX, which is where the
        // correlation lives now. The LEDGER must not contain them: `"final"` in
        // a receipt line is the schema drift the audit named.
        let index = std::fs::read_to_string(index_path_for(dir.path())).unwrap();
        for token in [
            "\"send\"",
            "\"edit\"",
            "\"fallback\"",
            "\"ack\"",
            "\"final\"",
            "\"addendum\"",
            "\"watchdog\"",
            "\"failure\"",
        ] {
            assert!(index.contains(token), "missing {token}: {index}");
        }
        let ledger = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        for line in ledger.lines() {
            assert_eq!(wire_keys(line), V9_1_RECEIPT_FIELDS);
        }
        // …and the round trip still knows every phase and outcome it wrote.
        let back = read_all(dir.path());
        assert_eq!(back.len(), 5);
        assert_eq!(back[0].reply_phase, ReplyPhase::Ack);
        assert_eq!(back[3].outcome, RelayOutcome::Send);
        assert_eq!(back[3].reply_phase, ReplyPhase::Failure);
        assert_eq!(back[4].reply_phase, ReplyPhase::Addendum);
    }

    /// ITEM 9 — THE DURABLE REFIRE BYPASS. A dispatcher refire of an ALREADY
    /// ACCEPTED occurrence finds the original attempt's evidence and reuses it:
    /// it does not re-relay, and it does not mint a second receipt.
    #[test]
    fn a_refire_reuses_the_accepted_attempts_evidence_and_does_not_relay_again() {
        let dir = scratch();
        let root = dir.path();

        // The accepted turn is delivered once, by attempt 1.
        let mut first = receipt(root, TURN, 1, Some(4242));
        first.attempt_id = Some(ATTEMPT_ONE.to_string());
        append_certified(root, &first).unwrap();

        // THE REFIRE. Same turn, same attempt — the dispatcher redelivering an
        // occurrence that was already answered.
        let evidence = evidence_for_attempt(root, TURN, Some(ATTEMPT_ONE))
            .expect("a refire must FIND the original attempt's evidence");
        assert_eq!(evidence.receipt_id, first.receipt_id);
        assert_eq!(
            evidence.message_id,
            Some(4242),
            "the refire reports the message the family ACTUALLY got"
        );
        assert_eq!(evidence.status, RelayStatus::Delivered);
        assert_eq!(
            evidence.feed_id, 1,
            "and the row it proves, so the refire needs no new row either"
        );

        // Were the refire to try to relay anyway, the ledger refuses its receipt
        // rather than recording one delivery twice.
        let mut again = receipt(root, TURN, 2, Some(4243));
        again.attempt_id = Some(ATTEMPT_ONE.to_string());
        assert!(matches!(
            append(root, &again),
            Err(ReceiptError::AttemptAlreadyRecorded { .. })
        ));
        assert_eq!(
            read_all(root).len(),
            1,
            "still exactly one delivery on record"
        );
    }

    /// The other side of the same key, and the reason it is a PAIR: a self-heal
    /// retry after a delivery that genuinely died is a NEW attempt. It finds no
    /// evidence, so it relays — rather than being suppressed as a refire and
    /// leaving the household with the silence the retry existed to break.
    #[test]
    fn a_self_heal_retry_finds_no_evidence_and_therefore_relays() {
        let dir = scratch();
        let root = dir.path();

        let mut dead = receipt(root, TURN, 1, None);
        dead.attempt_id = Some(ATTEMPT_ONE.to_string());
        dead.status = RelayStatus::Failed;
        append_certified(root, &dead).unwrap();

        assert!(
            evidence_for_attempt(root, TURN, Some(ATTEMPT_TWO)).is_none(),
            "a NEW attempt on the same turn is not a refire and must not be suppressed"
        );

        // So it relays, and writes its own receipt for the delivery that worked.
        let mut healed = receipt(root, TURN, 2, Some(9001));
        healed.attempt_id = Some(ATTEMPT_TWO.to_string());
        append_certified(root, &healed).unwrap();
        let now = evidence_for_attempt(root, TURN, Some(ATTEMPT_TWO)).unwrap();
        assert_eq!(now.message_id, Some(9001));
        assert_eq!(read_all(root).len(), 2);
    }

    /// Evidence for a turn that was never delivered is absent, not fabricated —
    /// the caller must relay rather than claim a delivery that never happened.
    #[test]
    fn an_unknown_turn_has_no_evidence() {
        let dir = scratch();
        assert!(evidence_for_attempt(dir.path(), TURN, Some(ATTEMPT_ONE)).is_none());
        assert!(evidence_for_attempt(dir.path(), TURN, None).is_none());
    }
}
