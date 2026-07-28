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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyPhase {
    Ack,
    Final,
    Watchdog,
    Failure,
}

impl ReplyPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyPhase::Ack => "ack",
            ReplyPhase::Final => "final",
            ReplyPhase::Watchdog => "watchdog",
            ReplyPhase::Failure => "failure",
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
    #[serde(rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<i64>,
    #[serde(rename = "acceptedAtMs")]
    pub accepted_at_ms: i64,
    pub status: RelayStatus,
    /// Always `engine` from this writer.
    pub provenance: String,
    /// Which transport call this was.
    pub outcome: RelayOutcome,
    /// Which phase of the turn the proven row is.
    #[serde(rename = "replyPhase")]
    pub reply_phase: ReplyPhase,
    /// `(turn, attempt)` — present when the caller supplied `WG_ATTEMPT_ID`.
    #[serde(rename = "attemptId", skip_serializing_if = "Option::is_none")]
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
    /// a replayed claim of an old delivery.
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
/// A missing directory handle is not fatal on platforms that refuse to open one.
fn sync_dir(dir: Option<&Path>) -> std::io::Result<()> {
    let Some(dir) = dir else { return Ok(()) };
    match std::fs::File::open(dir) {
        Ok(handle) => handle.sync_all().or(Ok(())),
        Err(_) => Ok(()),
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

/// `(turn, attempt)` from `WG_ATTEMPT_ID`, or the turn alone when the caller
/// did not supply one. A retry after a genuine failure must not be suppressed
/// as if it were a refire, which is what keying on the turn alone would do.
pub fn attempt_key(turn_id: &str, attempt_id: Option<&str>) -> String {
    match attempt_id.map(str::trim).filter(|a| !a.is_empty()) {
        Some(attempt) => format!("{turn_id}\u{1f}{attempt}"),
        None => format!("{turn_id}\u{1f}1"),
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
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Receipt>(l).ok())
        .collect()
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
///      guard, so a re-read of an old response cannot re-certify it;
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
pub fn append(project_root: &Path, receipt: &Receipt) -> Result<(), ReceiptError> {
    let ledger = ledger_path_for(project_root);
    if let Some(parent) = ledger.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReceiptError::Io(e.to_string()))?;
    }
    let lock = super::feed_lock::acquire(&ledger, super::feed_lock::DEFAULT_WAIT_MS)
        .map_err(|refusal| ReceiptError::NotSerialised(refusal.to_string()))?;
    let outcome = append_locked(project_root, receipt, &lock);
    lock.release();
    outcome
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
        if let Some(prior) = existing.iter().find(|r| {
            r.message_id
                .filter(|id| *id > 0)
                .is_some_and(|prior_mid| replay_key(&r.transport_scope_id, prior_mid) == key)
        }) {
            return Err(ReceiptError::Replay {
                replay_key: key,
                by: prior.receipt_id.clone(),
            });
        }
    }
    let key = attempt_key(&receipt.turn_id, receipt.attempt_id.as_deref());
    if let Some(prior) = existing
        .iter()
        .find(|r| attempt_key(&r.turn_id, r.attempt_id.as_deref()) == key)
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
    // ONE record, ONE write. The line and its newline used to be two calls, and
    // two `O_APPEND` writes from two writers interleave: the audit's race left a
    // ledger of concatenated JSON objects in which the very claims that had just
    // "succeeded" were no longer readable. A single buffer is a single atomic
    // append for any record short enough to fit the pipe/file atomicity window,
    // and the lock above covers the rest.
    let mut line = serde_json::to_string(receipt).map_err(|e| ReceiptError::Io(e.to_string()))?;
    line.push('\n');
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ReceiptError::Io(e.to_string()))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| ReceiptError::Io(e.to_string()))?;
    // fsync the DIRECTORY too, or a ledger created by this very append can be
    // absent after a power loss while the row it proves is durable — evidence
    // that vanishes is worse than evidence that was never written.
    sync_dir(path.parent()).map_err(|e| ReceiptError::Io(e.to_string()))
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
pub fn evidence_for_attempt(
    project_root: &Path,
    turn_id: &str,
    attempt_id: Option<&str>,
) -> Option<Receipt> {
    let key = attempt_key(turn_id, attempt_id);
    read_all(project_root)
        .into_iter()
        .find(|r| attempt_key(&r.turn_id, r.attempt_id.as_deref()) == key)
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
        append(dir.path(), &r).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 7, Some(11))).unwrap();
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
        append(dir.path(), &first).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(4242))).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(4242))).unwrap();
        let mut other_bot = receipt(dir.path(), TURN2, 2, Some(4242));
        other_bot.transport_scope_id = scope_id_for_bot(dir.path(), "bot-two").unwrap();
        append(dir.path(), &other_bot).expect("a genuine second bot's receipt was suppressed");
        assert_eq!(read_all(dir.path()).len(), 2);
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
        append(dir.path(), &first).unwrap();

        let mut retry = receipt(dir.path(), TURN, 2, Some(99));
        retry.attempt_id = Some(ATTEMPT_TWO.into());
        append(dir.path(), &retry).expect("the self-heal retry's receipt was suppressed");

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
        append(dir.path(), &first).unwrap();

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
        assert_eq!(attempt_key(TURN, None), attempt_key(TURN, Some("1")));
        assert_eq!(attempt_key(TURN, Some("  ")), attempt_key(TURN, None));
        assert_ne!(attempt_key(TURN, Some("2")), attempt_key(TURN, None));
    }

    // ── what the ledger holds ───────────────────────────────────────────────

    #[test]
    fn a_receipt_round_trips_with_the_schemas_field_names() {
        let dir = scratch();
        let r = receipt(dir.path(), TURN, 5, Some(11));
        append(dir.path(), &r).unwrap();
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        for key in [
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
            "replyPhase",
        ] {
            assert!(
                body.contains(&format!("\"{key}\"")),
                "missing {key}: {body}"
            );
        }
        // The raw turn id is on the wire VERBATIM — a hashed one cannot join.
        assert!(body.contains(TURN), "{body}");
        assert!(body.contains("\"provenance\":\"engine\""), "{body}");
        assert_eq!(read_all(dir.path()), vec![r]);
    }

    /// A torn tail must not satisfy a join. A half-written line is skipped, not
    /// read as a receipt.
    #[test]
    fn a_truncated_line_is_never_read_as_a_receipt() {
        let dir = scratch();
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
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
        append(dir.path(), &first).unwrap();

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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
        append(dir.path(), &receipt(dir.path(), TURN2, 2, Some(12))).unwrap();
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
            append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
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
        append(dir.path(), &receipt(dir.path(), TURN, 1, Some(11))).unwrap();
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
        append(dir.path(), &good).unwrap();
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
            append(dir.path(), &r).unwrap();
        }
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        for token in [
            "\"send\"",
            "\"edit\"",
            "\"fallback\"",
            "\"ack\"",
            "\"final\"",
            "\"watchdog\"",
            "\"failure\"",
        ] {
            assert!(body.contains(token), "missing {token}: {body}");
        }
        assert_eq!(read_all(dir.path()).len(), 4);
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
        append(root, &first).unwrap();

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
        append(root, &dead).unwrap();

        assert!(
            evidence_for_attempt(root, TURN, Some(ATTEMPT_TWO)).is_none(),
            "a NEW attempt on the same turn is not a refire and must not be suppressed"
        );

        // So it relays, and writes its own receipt for the delivery that worked.
        let mut healed = receipt(root, TURN, 2, Some(9001));
        healed.attempt_id = Some(ATTEMPT_TWO.to_string());
        append(root, &healed).unwrap();
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
