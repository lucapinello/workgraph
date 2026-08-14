//! Reply audience — the ENGINE half of the casa audience ledger.
//!
//! ## Why this file exists
//!
//! The gateway (`claw3d-bridge/src/audienceLedger.mjs`) answers ONE question:
//! *who saw the answer to this turn?* It answers it with one append-only line
//! per `(turn, audience, chat)` in `<root>/.casa/reply-audience.jsonl`, so a
//! family member who reports "otto replied to me PRIVATELY when I asked in the
//! kitchen" can be believed or corrected from disk instead of from memory.
//!
//! That ledger was wired at every reply seam **in the gateway** — and the
//! gateway is not the only writer of the family conversation. This process is
//! the other one: `wg telegram listen` owns the Telegram sockets, so every
//! reply it relays into the family group is appended by [`super::casa_feed`]
//! here, in Rust, with the gateway nowhere in the loop. Those rows already
//! carry the gateway-minted `turnId` (the join key), so before this module they
//! produced exactly the hole the ledger exists to close: a reply the family
//! saw, joinable to a turn, with NO record of the audience it reached. An
//! auditor reading the ledger would have answered "no reply recorded for that
//! turn", which is indistinguishable from "the reply never happened".
//!
//! So this is deliberately NOT a second design. It is the SAME six fields, the
//! SAME file, the SAME dedupe key, the SAME refusals, and the same bytes on the
//! line — a record written here is read back by `audienceForTurn` in the
//! gateway with no translation. Where the two could drift, the gateway is the
//! authority and this file is the port; the cross-implementation twin test
//! (`claw3d-bridge/test/audienceEngineTwin.test.mjs`) drives the real binary
//! and reads the result through the gateway's own reader, so a drift is red.
//!
//! ## The six fields (verbatim from the gateway's header)
//!
//! * `turnId` — the turn this reply answers, RAW and verbatim, the same id the
//!   feed row and the receipt carry. THE JOIN KEY.
//! * `responderId` — who replied, as the stable roster id. Never blank; a line
//!   spoken in the house's own name records [`HOUSE_RESPONDER`].
//! * `audience` — [`AUDIENCE_GROUP`] or [`AUDIENCE_PRIVATE`]. Never inferred:
//!   an audience this code had to guess would be a fabricated answer to the one
//!   question the file is asked, so an unknown value is REFUSED.
//! * `chat` — a LOCAL scope label ([`GROUP_CHAT`], or `dm:<human>__<agent>`
//!   naming the thread file). A chat-id-shaped label is REFUSED, never
//!   truncated: this file names audiences in the house's vocabulary, and the
//!   moment it carries transport ids it is a second, quieter copy of who talks
//!   to whom.
//! * `via` — `family-chat` / `thread` / `private-dm`: HOW the audience was
//!   reached, which is what makes "otto replied to me privately" checkable.
//! * `at` — accepted-at, epoch ms.
//!
//! ## One record per audience, not per send
//!
//! A turn that speaks twice into the same chat (an ack, then the answer) is ONE
//! audience record: the first write wins and later writes for the same
//! `(turn, audience, chat)` are idempotent no-ops. Whether each individual send
//! landed is a DELIVERY question and [`super::relay_receipt`] already answers
//! it against the same `turnId`.
//!
//! ## What is NOT recorded here, and why that is not a hole
//!
//! * An INBOUND human line (`casa_feed::group_entry`, the listener's two
//!   mirrors) is not a reply. Recording it would read as a helper answering,
//!   which is the false positive this file must never manufacture.
//! * A reply with NO canonical turn id — a listener-initiated line, a proactive
//!   digest, a legacy path — answers no turn and has nothing to join on.
//!   Inventing an id would put rows in the file that no conversation can be
//!   read against. Such a reply is skipped, and the skip is REPORTED to the
//!   caller ([`AudienceSkipped`]) rather than silently swallowed.
//! * A PRIVATE reply the engine sends (`ReplyScope::Private`) touches no feed
//!   file at all, and in production it carries no gateway turn id either (the
//!   gateway mints one per accepted web/kiosk turn; a person DMing a bot
//!   directly is a conversation the gateway never saw). When such a path does
//!   acquire a canonical turn, it records here with `audience = "1:1"` and
//!   `via = "private-dm"` — the constants and the writer are already the right
//!   shape for it; what is missing is a turn to join on, not a record.
//!
//! ## Bounded, like the gateway's copy
//!
//! Past [`MAX_RECORDS`] the file is rewritten keeping the newest
//! [`KEEP_RECORDS`]. Nothing keys off a line ordinal (the turn id is the key),
//! so the trim is lossless for every consumer. Both implementations use the
//! same numbers, so whichever one happens to cross the threshold trims to the
//! same horizon.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The whole family.
pub const AUDIENCE_GROUP: &str = "group";
/// One person.
pub const AUDIENCE_PRIVATE: &str = "1:1";
/// The group conversation (the pane plus the family Telegram group behind it).
pub const VIA_FAMILY_CHAT: &str = "family-chat";
/// A durable 1:1 conversation on any surface (office, kiosk, web).
pub const VIA_THREAD: &str = "thread";
/// A direct message on a person's phone.
pub const VIA_PRIVATE_DM: &str = "private-dm";
/// The scope label of the one family chat. A house has exactly one.
pub const GROUP_CHAT: &str = "group";
/// The responder id for a line the house speaks in its OWN name. A real id
/// rather than a blank on purpose: the reader's question is "who replied to
/// me", and an empty field makes an unattributable reply look like a missing
/// record.
pub const HOUSE_RESPONDER: &str = "house";

const MAX_TURN_ID: usize = 80;
const MAX_ID: usize = 64;
const MAX_CHAT: usize = 120;
/// The file is rewritten once it passes this many records…
pub const MAX_RECORDS: usize = 8000;
/// …keeping this many of the newest. Same numbers as the gateway's.
pub const KEEP_RECORDS: usize = 4000;

/// One audience record — the six fields, in the order they are written.
///
/// Serialised with the gateway's own key spelling (`turnId`, `responderId`, …)
/// so a line written here is a line the gateway's reader parses; a re-spelled
/// key would join to nothing and read as no record at all.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudienceRecord {
    pub turn_id: String,
    pub responder_id: String,
    pub audience: String,
    pub chat: String,
    pub via: String,
    pub at: i64,
}

/// Why a record was NOT written. `Duplicate` is a SUCCESS — the audience is
/// already on the record, which is the property the caller wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudienceOutcome {
    Recorded(AudienceRecord),
    Duplicate(AudienceRecord),
}

impl AudienceOutcome {
    /// The wire word for a caller that prints its verdict (`feed-write`).
    pub fn as_str(&self) -> &'static str {
        match self {
            AudienceOutcome::Recorded(_) => "recorded",
            AudienceOutcome::Duplicate(_) => "duplicate",
        }
    }

    pub fn record(&self) -> &AudienceRecord {
        match self {
            AudienceOutcome::Recorded(r) | AudienceOutcome::Duplicate(r) => r,
        }
    }
}

/// Why a record was REFUSED. A refusal means this reply went out with no
/// durable audience record — the exact hole the ledger closes — so callers
/// SURFACE it (the listener prints it beside the row it wrote) instead of
/// swallowing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudienceError {
    /// The record could not be made readable as evidence; nothing was written.
    Malformed(String),
    /// The conversation lock could not be taken. FAIL CLOSED, like every other
    /// writer beside the feed: an unserialised append can land inside a
    /// rotation and be present in neither file.
    Locked(String),
    Io(String),
}

impl std::fmt::Display for AudienceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudienceError::Malformed(d) => write!(f, "malformed audience record: {d}"),
            AudienceError::Locked(d) => write!(f, "feed lock unavailable ({d})"),
            AudienceError::Io(d) => write!(f, "audience ledger write failed: {d}"),
        }
    }
}

/// A reply that legitimately records nothing, named so the caller can say which
/// case it was rather than reporting a silent success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudienceSkipped {
    /// No canonical turn id: nothing to join a record to.
    NoTurn,
}

impl AudienceSkipped {
    pub fn as_str(self) -> &'static str {
        match self {
            AudienceSkipped::NoTurn => "no-turn",
        }
    }
}

/// Beside the feed — `<dir of feed>/reply-audience.jsonl`. The same rule every
/// artifact in this program follows: a record filed next to a different
/// conversation is not evidence about this one.
pub fn audience_path_for(feed_path: &Path) -> PathBuf {
    match feed_path.extension().and_then(|e| e.to_str()) {
        Some("jsonl") => feed_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("reply-audience.jsonl"),
        // A directory was passed (the gateway's reader accepts both).
        _ => feed_path.join("reply-audience.jsonl"),
    }
}

/// Control characters are STRIPPED, not escaped: a newline in a field would
/// split one record into two unreadable lines, and a NUL would break the dedupe
/// key's separator. Then trimmed and capped, exactly as the gateway's `str()`.
fn scrub(v: &str, max: usize) -> String {
    let cleaned: String = v
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .collect();
    cleaned.trim().chars().take(max).collect()
}

/// A chat id, in every shape Telegram uses: a bare positive user id or a
/// negative group/supergroup id, six digits being the floor for both. The
/// gateway's `CHAT_ID_SHAPED` regex, without a regex.
fn chat_id_shaped(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    digits.len() >= 6 && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// The dedupe/read key. NUL-separated so a turn id ending in the first
/// character of an audience cannot collide with another pair.
pub fn audience_key(r: &AudienceRecord) -> String {
    format!("{}\u{0}{}\u{0}{}", r.turn_id, r.audience, r.chat)
}

/// The ONLY fields allowed into a record. Anything that would make the record
/// unreadable as evidence is REFUSED rather than normalised: a record that
/// cannot be joined, or cannot name its responder or its audience, is worse
/// than no record — it looks like proof.
pub fn sanitize(raw: &AudienceRecord) -> Result<AudienceRecord, String> {
    let turn_id = scrub(&raw.turn_id, MAX_TURN_ID);
    if turn_id.is_empty() {
        return Err("a record must name the turn it answers".into());
    }
    let responder_id = scrub(&raw.responder_id, MAX_ID);
    if responder_id.is_empty() {
        return Err("a record must name the responder".into());
    }
    if raw.audience != AUDIENCE_GROUP && raw.audience != AUDIENCE_PRIVATE {
        return Err(format!(
            "audience must be {AUDIENCE_GROUP}|{AUDIENCE_PRIVATE}"
        ));
    }
    if raw.via != VIA_FAMILY_CHAT && raw.via != VIA_THREAD && raw.via != VIA_PRIVATE_DM {
        return Err(format!(
            "via must be {VIA_FAMILY_CHAT}|{VIA_THREAD}|{VIA_PRIVATE_DM}"
        ));
    }
    let chat = scrub(&raw.chat, MAX_CHAT);
    if chat.is_empty() {
        return Err("a record must name the chat it went to".into());
    }
    if chat_id_shaped(&chat) {
        // Deliberately NOT echoed back: a refusal that quotes the value writes
        // the id into every log the refusal reaches.
        return Err("chat must be a local scope label, never a transport id".into());
    }
    Ok(AudienceRecord {
        turn_id,
        responder_id,
        audience: raw.audience.clone(),
        chat,
        via: raw.via.clone(),
        at: if raw.at > 0 { raw.at } else { 0 },
    })
}

/// The 1:1 scope label for a (human, agent) thread — the same
/// `dm:<human>__<agent>` spelling the gateway gives the thread file under
/// `.casa/threads/`, so a record points at a conversation an operator can open.
pub fn dm_chat(human: &str, agent: &str) -> String {
    let tok = |v: &str| -> String {
        scrub(v, 40)
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
            .take(40)
            .collect()
    };
    let agent = tok(agent);
    format!(
        "dm:{}__{}",
        tok(human),
        if agent.is_empty() { "unknown" } else { &agent }
    )
}

/// One record's line, byte-for-byte as the gateway writes it.
fn to_json_line(r: &AudienceRecord) -> String {
    // The six fields are plain scalars with no control characters left in them,
    // so this cannot fail; a `serde_json` error here would still not be worth
    // losing the record over, hence the explicit fallback rather than unwrap.
    serde_json::to_string(r).unwrap_or_else(|_| String::new())
}

/// Every record on disk, oldest first, with the unreadable lines COUNTED rather
/// than silently skipped: a damaged ledger is a reason to say so, not to
/// quietly answer "no private reply was ever recorded".
pub fn read_audience(feed_path: &Path) -> (Vec<AudienceRecord>, Vec<(usize, String)>) {
    let path = audience_path_for(feed_path);
    let body = fs::read_to_string(&path).unwrap_or_default();
    let mut records = Vec::new();
    let mut malformed = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => {
                malformed.push((i + 1, "not json".to_string()));
                continue;
            }
        };
        let raw = AudienceRecord {
            turn_id: parsed
                .get("turnId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            responder_id: parsed
                .get("responderId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            audience: parsed
                .get("audience")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            chat: parsed
                .get("chat")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            via: parsed
                .get("via")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            at: parsed.get("at").and_then(|v| v.as_i64()).unwrap_or(0),
        };
        match sanitize(&raw) {
            Ok(r) => records.push(r),
            Err(why) => malformed.push((i + 1, why)),
        }
    }
    (records, malformed)
}

/// Every audience one turn reached, oldest first — the whole diagnosis of an
/// audience complaint, and the engine-side twin of the gateway's
/// `audienceForTurn`.
pub fn audience_for_turn(feed_path: &Path, turn_id: &str) -> Vec<AudienceRecord> {
    let want = scrub(turn_id, MAX_TURN_ID);
    if want.is_empty() {
        return Vec::new();
    }
    let (records, _) = read_audience(feed_path);
    records.into_iter().filter(|r| r.turn_id == want).collect()
}

/// Record that a reply reached an audience — TAKING the conversation lock.
///
/// The whole thing runs inside the cross-process conversation lock — the SAME
/// lock the feed row was written under, and the same one the gateway's writer
/// takes — so a gateway record and an engine record can never interleave into
/// one another's bytes. A lock refusal writes NOTHING and says so.
///
/// The append happens BEFORE the trim, so a record is never lost to make room
/// for itself; and a failed trim is not a lost audience (the record is already
/// on disk), so it is not reported as one.
///
/// **THIS IS THE ENTRY POINT FOR A CALLER THAT HOLDS NOTHING.** A caller already
/// inside the feed transaction that wrote the row calls
/// [`record_audience_locked`] instead, and takes no second section — see there
/// for why that is not merely a saving.
pub fn record_audience(
    feed_path: &Path,
    raw: &AudienceRecord,
) -> Result<AudienceOutcome, AudienceError> {
    let record = sanitize(raw).map_err(AudienceError::Malformed)?;
    // A SECOND ACQUISITION OF THE SAME LOCK, and so the one most likely to arrive
    // after the budget has already been eaten. It retries on the same terms as
    // the row's (docs/42 §9, `feed-lock-retry`): only a typed `Timeout`, around
    // the acquisition only, and a spent budget still refuses. `write_locked` is
    // idempotent on the (turn, audience) key anyway, but it is never given the
    // chance to prove it — the section runs at most once.
    let completed = super::feed_lock::with_feed_lock_retrying(
        feed_path,
        super::feed_lock::DEFAULT_WAIT_MS,
        super::feed_lock::DEFAULT_ATTEMPTS,
        |_lock| write_locked(feed_path, &record, MAX_RECORDS, KEEP_RECORDS),
    )
    .map_err(|refusal| AudienceError::Locked(refusal.to_string()))?;
    completed.regardless_of_release()
}

/// [`record_audience`]'s body, for a caller that ALREADY HOLDS the feed lock —
/// the delivery seam, which writes the row, its receipt and its audience in one
/// section (docs/42 §9, `feed-lock-section`).
///
/// The `_lock` parameter is a witness, not a hint, exactly as it is in
/// [`relay_receipt::append_locked`](super::relay_receipt::append_locked): a
/// [`FeedLock`] can only be obtained by acquiring one, so this cannot be called
/// from outside a transaction and cannot deadlock by taking the lock twice (it
/// is not reentrant across sequential sections, and the protocol's re-entrancy
/// is keyed per thread, so a nested `acquire` here would silently succeed and
/// prove nothing).
///
/// **WHAT THIS IS NOT.** It is not the audience record joining the ROW's
/// transaction. The row and its receipt are one fact and roll back together
/// ([`super::casa_feed::append_entry_proving`]); the audience is a separate fact
/// about a message the family has ALREADY seen, so a caller must not let a
/// refused audience take the row back out, and must still record the audience
/// when the row itself could not be written. Both callers keep that shape: they
/// write the audience inside the section and carry its outcome out, and the
/// listener falls back to [`record_audience`] on the path where there was no
/// section at all. What is shared is the exclusion, not the rollback.
///
/// [`FeedLock`]: super::feed_lock::FeedLock
pub fn record_audience_locked(
    feed_path: &Path,
    raw: &AudienceRecord,
    _lock: &super::feed_lock::FeedLock,
) -> Result<AudienceOutcome, AudienceError> {
    let record = sanitize(raw).map_err(AudienceError::Malformed)?;
    write_locked(feed_path, &record, MAX_RECORDS, KEEP_RECORDS)
}

/// The critical section's body, with the horizon injectable so the trim is
/// testable without writing eight thousand records.
fn write_locked(
    feed_path: &Path,
    record: &AudienceRecord,
    max_records: usize,
    keep_records: usize,
) -> Result<AudienceOutcome, AudienceError> {
    let (existing, _) = read_audience(feed_path);
    let key = audience_key(record);
    if let Some(prior) = existing.iter().find(|r| audience_key(r) == key) {
        return Ok(AudienceOutcome::Duplicate(prior.clone()));
    }
    let path = audience_path_for(feed_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AudienceError::Io(e.to_string()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| AudienceError::Io(e.to_string()))?;
    file.write_all(to_json_line(record).as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|e| AudienceError::Io(e.to_string()))?;

    if existing.len() + 1 > max_records {
        let mut all = existing;
        all.push(record.clone());
        let keep = all.split_off(all.len().saturating_sub(keep_records));
        let body: String = keep
            .iter()
            .map(|r| format!("{}\n", to_json_line(r)))
            .collect();
        // The record IS on disk; a failed trim is not a lost audience.
        let _ = fs::write(&path, body);
    }
    Ok(AudienceOutcome::Recorded(record.clone()))
}

/// The one shape the engine writes today: a persona's reply relayed into the
/// family group. `audience = group`, `chat = group`, `via = family-chat` — the
/// values the gateway's own group seam records, so an auditor cannot tell which
/// process answered, which is the point.
pub fn record_group_reply(
    feed_path: &Path,
    turn_id: &str,
    responder_id: &str,
    at: i64,
) -> Result<AudienceOutcome, AudienceError> {
    record_audience(feed_path, &group_reply_record(turn_id, responder_id, at))
}

/// [`record_group_reply`] for a caller already inside the feed transaction —
/// the same six fields, written in the section that wrote the row rather than in
/// a second one. See [`record_audience_locked`].
pub fn record_group_reply_locked(
    feed_path: &Path,
    turn_id: &str,
    responder_id: &str,
    at: i64,
    lock: &super::feed_lock::FeedLock,
) -> Result<AudienceOutcome, AudienceError> {
    record_audience_locked(
        feed_path,
        &group_reply_record(turn_id, responder_id, at),
        lock,
    )
}

/// The record both group-reply seams write, built once so the locked and
/// unlocked forms cannot drift into recording two different things.
fn group_reply_record(turn_id: &str, responder_id: &str, at: i64) -> AudienceRecord {
    let responder = scrub(responder_id, MAX_ID);
    AudienceRecord {
        turn_id: turn_id.to_string(),
        responder_id: if responder.is_empty() {
            HOUSE_RESPONDER.to_string()
        } else {
            responder
        },
        audience: AUDIENCE_GROUP.to_string(),
        chat: GROUP_CHAT.to_string(),
        via: VIA_FAMILY_CHAT.to_string(),
        at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn feed(dir: &tempfile::TempDir) -> PathBuf {
        let casa = dir.path().join(".casa");
        fs::create_dir_all(&casa).unwrap();
        casa.join("group-feed.jsonl")
    }

    const TURN: &str = "web-turn-4f1d7c2e-9b3a-4c55-8f01-2a6d5e7b9c10";

    #[test]
    fn audience_file_sits_beside_the_feed() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        assert_eq!(
            audience_path_for(&f),
            dir.path().join(".casa").join("reply-audience.jsonl")
        );
        // A directory is accepted too, like the gateway's reader.
        assert_eq!(
            audience_path_for(&dir.path().join(".casa")),
            dir.path().join(".casa").join("reply-audience.jsonl")
        );
    }

    // THE CROSS-IMPLEMENTATION CONTRACT IN ONE ASSERTION. The gateway parses
    // this line; a key re-spelled here joins to nothing and reads as no record.
    #[test]
    fn the_line_is_the_gateways_six_fields_in_order() {
        let r = sanitize(&AudienceRecord {
            turn_id: TURN.into(),
            responder_id: "otto".into(),
            audience: AUDIENCE_GROUP.into(),
            chat: GROUP_CHAT.into(),
            via: VIA_FAMILY_CHAT.into(),
            at: 1_754_500_000_000,
        })
        .unwrap();
        assert_eq!(
            to_json_line(&r),
            format!(
                "{{\"turnId\":\"{TURN}\",\"responderId\":\"otto\",\"audience\":\"group\",\
                 \"chat\":\"group\",\"via\":\"family-chat\",\"at\":1754500000000}}"
            )
        );
    }

    #[test]
    #[serial]
    fn a_group_reply_is_recorded_once_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        let first = record_group_reply(&f, TURN, "otto", 1).unwrap();
        assert!(matches!(first, AudienceOutcome::Recorded(_)));
        let got = audience_for_turn(&f, TURN);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].responder_id, "otto");
        assert_eq!(got[0].audience, AUDIENCE_GROUP);
        assert_eq!(got[0].chat, GROUP_CHAT);
        assert_eq!(got[0].via, VIA_FAMILY_CHAT);

        // ONE RECORD PER AUDIENCE, NOT PER SEND: the ack and the answer of one
        // turn are one record, and the second write is an idempotent no-op.
        let again = record_group_reply(&f, TURN, "otto", 2).unwrap();
        assert!(matches!(again, AudienceOutcome::Duplicate(_)));
        assert_eq!(audience_for_turn(&f, TURN).len(), 1);
        assert_eq!(audience_for_turn(&f, TURN)[0].at, 1, "first write wins");
    }

    #[test]
    #[serial]
    fn a_second_audience_for_the_same_turn_is_a_second_record() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        record_group_reply(&f, TURN, "otto", 1).unwrap();
        record_audience(
            &f,
            &AudienceRecord {
                turn_id: TURN.into(),
                responder_id: "otto".into(),
                audience: AUDIENCE_PRIVATE.into(),
                chat: dm_chat("Luca", "otto"),
                via: VIA_PRIVATE_DM.into(),
                at: 2,
            },
        )
        .unwrap();
        let got = audience_for_turn(&f, TURN);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].chat, "dm:luca__otto");
    }

    #[test]
    fn refusals_leave_no_record_and_name_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        let base = AudienceRecord {
            turn_id: TURN.into(),
            responder_id: "otto".into(),
            audience: AUDIENCE_GROUP.into(),
            chat: GROUP_CHAT.into(),
            via: VIA_FAMILY_CHAT.into(),
            at: 1,
        };
        let cases: Vec<(AudienceRecord, &str)> = vec![
            (
                AudienceRecord {
                    turn_id: "  ".into(),
                    ..base.clone()
                },
                "the turn",
            ),
            (
                AudienceRecord {
                    responder_id: "".into(),
                    ..base.clone()
                },
                "the responder",
            ),
            (
                AudienceRecord {
                    audience: "everyone".into(),
                    ..base.clone()
                },
                "audience must be",
            ),
            (
                AudienceRecord {
                    via: "telegram".into(),
                    ..base.clone()
                },
                "via must be",
            ),
            (
                AudienceRecord {
                    chat: "".into(),
                    ..base.clone()
                },
                "the chat",
            ),
            // A transport id that leaked into a scope label — refused, and the
            // refusal does not quote it back.
            (
                AudienceRecord {
                    chat: "-1001234567".into(),
                    ..base.clone()
                },
                "never a transport id",
            ),
        ];
        for (raw, needle) in cases {
            let err = record_audience(&f, &raw).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(needle), "{text} should mention {needle}");
            assert!(
                !text.contains("1001234567"),
                "a refusal must not echo an id"
            );
        }
        assert!(!audience_path_for(&f).exists(), "a refusal writes nothing");
    }

    // A control character would split one record into two unreadable lines.
    #[test]
    #[serial]
    fn control_characters_are_stripped_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        record_group_reply(&f, TURN, "ot\nto", 1).unwrap();
        let body = fs::read_to_string(audience_path_for(&f)).unwrap();
        assert_eq!(body.trim().lines().count(), 1);
        assert_eq!(audience_for_turn(&f, TURN)[0].responder_id, "otto");
    }

    #[test]
    fn a_blank_responder_records_the_house_rather_than_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        record_group_reply(&f, TURN, "   ", 1).unwrap();
        assert_eq!(audience_for_turn(&f, TURN)[0].responder_id, HOUSE_RESPONDER);
    }

    #[test]
    fn the_trim_keeps_the_newest_and_never_drops_the_record_it_just_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        // Horizon of 3, keeping 2 — the same rule as the gateway's, which only
        // rewrites on the write that CROSSES the horizon: 1, 2, 3, then the
        // fourth append trims back to 2, then a fifth grows to 3 again.
        let mut counts = Vec::new();
        for i in 0..5 {
            let r = AudienceRecord {
                turn_id: format!("web-turn-{i}"),
                responder_id: "otto".into(),
                audience: AUDIENCE_GROUP.into(),
                chat: GROUP_CHAT.into(),
                via: VIA_FAMILY_CHAT.into(),
                at: i,
            };
            write_locked(&f, &sanitize(&r).unwrap(), 3, 2).unwrap();
            counts.push(read_audience(&f).0.len());
        }
        assert_eq!(counts, vec![1, 2, 3, 2, 3]);
        let (records, malformed) = read_audience(&f);
        assert!(malformed.is_empty());
        // The record that triggered the trim is never the one it drops, and the
        // survivors are the NEWEST — nothing keys off a line ordinal, so the
        // rewrite is lossless for every consumer.
        assert_eq!(
            records
                .iter()
                .map(|r| r.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["web-turn-2", "web-turn-3", "web-turn-4"],
        );
    }

    /// Every `.rs` file under the crate's `src/`, so the guard below sweeps the
    /// WHOLE engine rather than the files its author happened to think of.
    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    // ── THE GUARD A THIRD WRITER TRIPS ────────────────────────────────────────
    //
    // The hole this module closed was not a bug in a line of code; it was a
    // WRITER that nobody had told about the ledger. The gateway instrumented its
    // own seams, the listener's were missed, and nothing anywhere would have said
    // so — the ledger simply reported no reply for those turns.
    //
    // So the inventory of engine-side feed writers is PINNED here, beside the
    // ledger they must all call. A new relay, a new composer, a second
    // diagnostic — anything that builds an agent row or appends to the feed —
    // fails this test with the instruction it needs, at the moment it is added,
    // instead of quietly reopening the hole.
    //
    // The needles are assembled from parts on purpose: spelled out verbatim, this
    // guard's own source would match its own sweep and the counts would be a
    // function of how often the test mentions them.
    #[test]
    fn the_set_of_engine_feed_writers_is_exactly_the_known_list() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        assert!(
            files.len() > 100,
            "the sweep found almost no sources — a broken walk would pass this guard vacuously ({} files)",
            files.len()
        );

        let qualified = |name: &str| format!("casa_feed::{name}(");
        // Building an agent row is the shape that needs an audience: it IS a
        // reply. `group_entry` is a human's own inbound line and is exempt (see
        // the module header), so it is deliberately not on this list.
        let reply_builder = qualified("agent_entry");
        // The two transactional appenders. Every production row goes through one
        // of them (`casa_feed::append_entry` itself is private for that reason).
        // They are no longer interchangeable: `append_entry_allocating` is the
        // receipt-free form and now takes a `casa_feed::NonRelayRow`, so an
        // outbound row can only reach `append_entry_proving`. The inventory
        // still counts BOTH — a writer is a writer, and which door it used is
        // the compiler's business, not this guard's.
        let appenders = [
            qualified("append_entry_allocating"),
            qualified("append_entry_proving"),
        ];

        let mut reply_sites: Vec<String> = Vec::new();
        let mut append_sites: Vec<String> = Vec::new();
        for file in &files {
            let body = fs::read_to_string(file).unwrap_or_default();
            let rel = file
                .strip_prefix(&src)
                .unwrap_or(file)
                .to_string_lossy()
                .to_string();
            for (i, line) in body.lines().enumerate() {
                // A LINE OF PROSE IS NOT A WRITER. `///` examples and `//`
                // commentary name these functions to explain them; counting
                // those makes the inventory a function of how the module is
                // DOCUMENTED, and the guard then fires at whoever wrote the
                // sentence rather than at whoever added a writer.
                //
                // Found by guard-an-outbound: documenting the receipt-free
                // append with a `compile_fail` example — the very example that
                // proves an outbound row cannot reach it — registered
                // `casa_feed.rs` as a third reply builder. The teeth are
                // unchanged, because a doc comment compiles to no call: rustdoc
                // runs those examples as their own crates, and neither a
                // documented nor a commented-out line can put a row in the
                // family's feed.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let at = format!("{rel}:{}", i + 1);
                if line.contains(&reply_builder) {
                    reply_sites.push(at.clone());
                }
                if appenders.iter().any(|n| line.contains(n.as_str())) {
                    append_sites.push(at);
                }
            }
        }
        reply_sites.sort();
        append_sites.sort();

        // The two reply seams, both wired to `record_group_reply`:
        //   casa/reply_delivery.rs  ReplySink::mirror  — the live listener relay
        //   casa/feed_write.rs      run_feed_write     — `wg telegram feed-write`
        //
        // BOTH have now moved off upstream's file: `mirror` with Casa's reply-delivery
        // layer, and `run_feed_write` in slice 6 (docs/UPSTREAM-DIVERGENCE.md). Neither
        // one's code changed — each still calls `record_group_reply` beside its append —
        // only its home did, and this guard correctly went RED for the move, which is
        // exactly what an exact list is for. Still an EXACT ordered list, not a set: a
        // set would let a genuinely new writer hide behind a name already on it.
        let reply_files: Vec<&str> = reply_sites
            .iter()
            .map(|s| s.split(':').next().unwrap_or(""))
            .collect();
        assert_eq!(
            reply_files,
            vec!["casa/feed_write.rs", "casa/reply_delivery.rs"],
            "A NEW writer builds an agent (reply) feed row: {reply_sites:?}\n\
             Every reply row must also record WHO SAW IT, or an audience \
             complaint about it cannot be diagnosed. Call \
             `casa_audience::record_group_reply(&feed_path, turn, responder, \
             casa_feed::now_ms())` beside the append (see ReplySink::mirror), \
             then add the new site here.",
        );
        let append_files: Vec<&str> = append_sites
            .iter()
            .map(|s| s.split(':').next().unwrap_or(""))
            .collect();
        // Four sites, verified against the code rather than read off a diff:
        //   casa/feed_write.rs:156      run_feed_write        (reply — records audience)
        //   casa/reply_delivery.rs:256  ReplySink::mirror     (reply — records audience)
        //   commands/telegram.rs:816    inbound human line    (exempt)
        //   commands/telegram.rs:1003   inbound human line    (exempt)
        // Two moved off upstream's file with Casa's layers; the two that remain are the
        // inbound human lines. Sorted, so this remains an exact ordered list.
        assert_eq!(
            append_files,
            vec![
                "casa/feed_write.rs",
                "casa/reply_delivery.rs",
                "commands/telegram.rs",
                "commands/telegram.rs",
            ],
            "The set of engine feed appenders changed: {append_sites:?}\n\
             Two of the four are inbound human lines (exempt); the other two are \
             replies and must record an audience. Read the header of \
             notify/casa_audience.rs before adding to this list.",
        );
    }

    // A damaged ledger is REPORTED, never read as "no reply was recorded" —
    // which is indistinguishable from the defect the file exists to settle.
    #[test]
    fn unreadable_lines_are_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let f = feed(&dir);
        let p = audience_path_for(&f);
        fs::write(
            &p,
            format!(
                "not json\n{{\"turnId\":\"{TURN}\",\"responderId\":\"otto\",\"audience\":\"group\",\"chat\":\"group\",\"via\":\"family-chat\",\"at\":1}}\n{{\"turnId\":\"x\"}}\n"
            ),
        )
        .unwrap();
        let (records, malformed) = read_audience(&f);
        assert_eq!(records.len(), 1);
        assert_eq!(malformed.len(), 2);
    }

    // --- the audience rides in the row's section (feed-lock-section) --------

    /// [`record_audience_locked`] WRITES IN THE CALLER'S SECTION AND TAKES NO
    /// SECOND LOCK.
    ///
    /// This is the whole point of the seam: the delivery writers used to take
    /// `.conversation.lock`, write the row and its receipt, let go, and take it
    /// again for the audience — a second queue position for every other writer,
    /// arriving after the patience budget was already partly spent. The counter
    /// is the assertion; the record landing is the control that stops "one
    /// acquisition" from being achieved by not writing at all.
    #[test]
    #[serial]
    fn record_audience_locked_writes_in_the_callers_section_and_takes_no_second_lock() {
        let dir = tempfile::tempdir().unwrap();
        let feed_path = feed(&dir);
        super::super::feed_lock::reset_acquisition_counters();

        let lock =
            super::super::feed_lock::acquire(&feed_path, 1000).expect("the caller's section");
        let outcome = record_group_reply_locked(&feed_path, TURN, "harbor", 1, &lock)
            .expect("the audience must land inside the caller's section");
        assert!(
            matches!(outcome, AudienceOutcome::Recorded(_)),
            "{outcome:?}"
        );
        assert_eq!(
            super::super::feed_lock::distinct_acquisitions(),
            1,
            "the locked seam took a lock of its own — that is the second section this exists \
             to remove (docs/42 §9, feed-lock-section)"
        );
        assert_eq!(super::super::feed_lock::reentrant_frames(), 0);
        assert_eq!(lock.release(), super::super::feed_lock::Release::Released);

        let (records, bad) = read_audience(&feed_path);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].turn_id, TURN);
    }

    /// THE CONTROL — the unlocked entry point still exists and still takes a
    /// section of its own, because the listener falls back to it on the path
    /// where the row's transaction failed and there is no section to ride in.
    /// Without this, replacing `record_audience` with the locked form would go
    /// unnoticed until an audience was silently dropped.
    #[test]
    #[serial]
    fn the_unlocked_entry_point_still_takes_its_own_section() {
        let dir = tempfile::tempdir().unwrap();
        let feed_path = feed(&dir);
        super::super::feed_lock::reset_acquisition_counters();
        record_group_reply(&feed_path, TURN, "harbor", 1).expect("the standalone writer must land");
        assert_eq!(
            super::super::feed_lock::distinct_acquisitions(),
            1,
            "the standalone writer must still serialise itself"
        );
        assert_eq!(read_audience(&feed_path).0.len(), 1);
    }

    /// THE TWO FORMS RECORD THE SAME SIX FIELDS. They are two entry points to
    /// one record, and an auditor joining on the ledger cannot be made to care
    /// which one wrote a line — so the only difference permitted between them is
    /// the timestamp.
    #[test]
    #[serial]
    fn the_locked_and_unlocked_group_seams_write_the_same_record() {
        let dir = tempfile::tempdir().unwrap();
        let feed_path = feed(&dir);
        record_group_reply(&feed_path, TURN, "harbor", 7).expect("standalone");

        let other = tempfile::tempdir().unwrap();
        let feed_path2 = feed(&other);
        let lock = super::super::feed_lock::acquire(&feed_path2, 1000).unwrap();
        record_group_reply_locked(&feed_path2, TURN, "harbor", 7, &lock).expect("in-section");
        lock.release();

        assert_eq!(
            to_json_line(&read_audience(&feed_path).0[0]),
            to_json_line(&read_audience(&feed_path2).0[0]),
            "the two seams record different things"
        );
    }
}
