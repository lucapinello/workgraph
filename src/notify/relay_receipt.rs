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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    BadShape { field: &'static str, value: String },
    /// `status: delivered` with no positive message id — the delivery is not
    /// proven, so it may not be recorded as one.
    DeliveredWithoutMessageId,
    /// The caller did not observe a global feed id. A receipt that names no row
    /// proves nothing, and a guessed ordinal names the WRONG row.
    NoFeedId,
    /// Another receipt already proves this row. One row, one receipt.
    RowAlreadyProven { feed_id: i64, by: String },
    /// This exact (transport scope, message id) delivery is already recorded —
    /// a replayed claim of an old delivery.
    Replay { replay_key: String, by: String },
    /// This (turn, attempt) already wrote a receipt. A refire, not a retry.
    AttemptAlreadyRecorded { attempt_id: String, by: String },
    /// A receipt id was reused.
    ReceiptIdReused { receipt_id: String },
    Io(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiptError::BadShape { field, value } => {
                write!(f, "{field} is not a valid typed id: {value:?}")
            }
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
            ReceiptError::Io(m) => write!(f, "receipt ledger io: {m}"),
        }
    }
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
        if part.len() != want || !part.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()) {
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
    s.strip_prefix("ts_")
        .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()))
}

/// Mint a fresh receipt id.
pub fn mint_receipt_id() -> String {
    format!("rcpt_{}", uuid::Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// The transport scope id — WHICH BOT physically sent this
// ---------------------------------------------------------------------------

/// Read (or mint) the per-install scope key. 32 random bytes, `0600`, stored
/// beside the ledger. It never appears in a receipt or a log — exposing it would
/// turn every scope id back into a reversible digest of a short bot id.
fn scope_key(project_root: &Path) -> Result<Vec<u8>, ReceiptError> {
    let path = scope_key_path(project_root);
    if let Ok(existing) = std::fs::read(&path) {
        if existing.len() >= 32 {
            return Ok(existing);
        }
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
    crate::atomic_file::write_atomic(&path, &buf).map_err(|e| ReceiptError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(buf.to_vec())
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

/// Every receipt currently in the ledger. A missing or unreadable ledger reads
/// as empty; a line that does not parse is SKIPPED rather than taken as a
/// receipt, so a truncated tail cannot silently satisfy a join.
pub fn read_all(project_root: &Path) -> Vec<Receipt> {
    let Ok(body) = std::fs::read_to_string(ledger_path_for(project_root)) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Receipt>(l).ok())
        .collect()
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
pub fn append(project_root: &Path, receipt: &Receipt) -> Result<(), ReceiptError> {
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
    if receipt.status == RelayStatus::Delivered
        && !receipt.message_id.is_some_and(|id| id > 0)
    {
        return Err(ReceiptError::DeliveredWithoutMessageId);
    }
    if receipt.feed_id <= 0 {
        return Err(ReceiptError::NoFeedId);
    }

    let existing = read_all(project_root);
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
    let line =
        serde_json::to_string(receipt).map_err(|e| ReceiptError::Io(e.to_string()))?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ReceiptError::Io(e.to_string()))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|e| ReceiptError::Io(e.to_string()))
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
        assert!(!is_valid_turn_id("web-turn---------------------------------"));
        // Wrong version nibble (v1, not v4) and wrong variant.
        assert!(!is_valid_turn_id("web-turn-3f2504e0-4f89-11d3-9a0c-0305e82c3301"));
        assert!(!is_valid_turn_id("web-turn-3f2504e0-4f89-41d3-ca0c-0305e82c3301"));
        // Uppercase hex is not the canonical form.
        assert!(!is_valid_turn_id("web-turn-3F2504E0-4f89-41d3-9a0c-0305e82c3301"));
        // A signed numeric — a chat id wearing a turn id's name.
        assert!(!is_valid_turn_id("-1002233445566"));
        assert!(!is_valid_receipt_id("-1002233445566"));
        assert!(!is_valid_scope_id("-1002233445566"));
        // A token-like and a plain name.
        assert!(!is_valid_scope_id("123456:AA-Ee_ffffffffffffffffffffffffffff"));
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

    #[test]
    fn the_scope_key_never_reaches_the_ledger() {
        let dir = scratch();
        let r = receipt(dir.path(), TURN, 1, Some(11));
        append(dir.path(), &r).unwrap();
        let key = std::fs::read(scope_key_path(dir.path())).unwrap();
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        assert!(!body.contains(&hex::encode(&key)), "the scope key leaked into the ledger");
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
        assert!(read_all(dir.path()).is_empty(), "a refused receipt was written");
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
            Err(ReceiptError::BadShape { field: "turnId", .. })
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
            matches!(append(dir.path(), &replayed), Err(ReceiptError::Replay { .. })),
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
        first.attempt_id = Some("1".into());
        append(dir.path(), &first).unwrap();

        let mut retry = receipt(dir.path(), TURN, 2, Some(99));
        retry.attempt_id = Some("2".into());
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
        first.attempt_id = Some("1".into());
        append(dir.path(), &first).unwrap();

        let mut refire = receipt(dir.path(), TURN, 2, Some(12));
        refire.attempt_id = Some("1".into());
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
            assert!(body.contains(&format!("\"{key}\"")), "missing {key}: {body}");
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
            r.attempt_id = Some(format!("{}", i + 1));
            append(dir.path(), &r).unwrap();
        }
        let body = std::fs::read_to_string(ledger_path_for(dir.path())).unwrap();
        for token in ["\"send\"", "\"edit\"", "\"fallback\"", "\"ack\"", "\"final\"", "\"watchdog\"", "\"failure\""] {
            assert!(body.contains(token), "missing {token}: {body}");
        }
        assert_eq!(read_all(dir.path()).len(), 4);
    }
}
