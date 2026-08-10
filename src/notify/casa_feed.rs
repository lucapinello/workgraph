//! Casa group-feed writer — the WRITE side of the constellation split view's
//! live conversation pane (docs/15 §chat-split).
//!
//! The `wg telegram listen` process owns the Telegram sockets, so it is the one
//! process that can mirror the family's group conversation to disk. It appends
//! one JSONL line to `<project-root>/.casa/group-feed.jsonl` for:
//!   * every inbound **group** message it receives (a human in the group), and
//!   * every **agent** reply it relays back into the group (an elected persona's
//!     answer).
//!
//! The casa gateway (`claw3d-bridge/src/conversation.mjs`) tails that file and
//! serves it at `GET /conversation` for the kiosk's conversation pane. This
//! module is the producer half of that contract; `conversation.mjs` is the
//! consumer half. The two agree on ONE schema (six fields) and ONE location.
//!
//! ## PRIVACY — the load-bearing rule of this module
//!
//! A feed line carries ONLY display-safe fields: `ts`, `sender`, `agentId`,
//! `emoji`, `kind`, `text`, the provenance pair `srcId` / `origin` (docs/20 §2)
//! and the CAUSAL trio `turnId` / `replyPhase` / `nonRelayType` (see below —
//! `turnId` is an opaque gateway-minted uuid, not a household identifier). It
//! NEVER carries a bot token, a Telegram `chat_id`, or a
//! Telegram user id. [`FeedEntry`] has no field for a secret, and
//! [`FeedEntry::to_json_line`] emits exactly those keys — so a careless caller
//! structurally cannot write a token or chat id onto disk. Crucially `srcId` is
//! an OPAQUE fingerprint (see [`source_id`]): it is a hash of the message's
//! content, so it durably dedupes re-deliveries WITHOUT the chat id or user id
//! ever appearing verbatim in the feed. The gateway re-sanitizes on read as
//! defence in depth, but this writer is the first and primary gate: secrets must
//! never reach the file in the first place.
//!
//! ## EXACTLY-ONCE — why `srcId` exists (docs/20 §2)
//!
//! The gateway reader (`conversation.mjs`) dedupes the feed by `srcId`, but only
//! for a NON-NULL `srcId`; a `null` line is unique by construction. The Telegram
//! listener at-least-once re-delivers on restart (an update whose offset was not
//! advanced past a crash arrives again), and its in-memory cross-bot dedupe
//! (`telegram_dedupe`) is empty on a fresh process — so without a durable id the
//! SAME inbound group message appends twice and the pane shows it twice. Stamping
//! each inbound line with a restart-stable [`source_id`] lets the reader's
//! `dedupeBySrcId` collapse the re-delivery to one. This is the write half of the
//! exactly-once contract; `conversation.mjs` is the read half.
//!
//! ## CAUSAL SETTLEMENT — why a row must say what it is
//!
//! A feed row is what the pane SHOWS; it was never evidence that anything reached
//! the family (see [`super::relay_receipt`]). Making the row PROVABLE needs three
//! things stamped by the writer, which is the only party that knows them:
//!
//!   * `turnId` — the accepted turn this row belongs to, RAW and verbatim, as the
//!     gateway handed it over on `WG_TURN_ID`. The engine hashes turn ids
//!     internally for its durable delivery digest (`web_physical_turn_key`), and
//!     that hash is INTERNAL IDEMPOTENCY IDENTITY ONLY: a hashed id in the causal
//!     position cannot be joined to a gateway row carrying the raw one, so the
//!     join silently finds nothing and every row reads as unproven. A row whose
//!     `turnId` is not a raw `web-turn-<uuid v4>` is REFUSED — no row, no receipt
//!     — rather than written with an id that certifies nothing.
//!   * `replyPhase` — ack / final / addendum / watchdog / failure, stamped at
//!     emit time because the writer KNOWS which it is. Deriving it later from
//!     the text is text analysis, and text analysis is how a watchdog line gets
//!     counted as the turn's final answer. `addendum` is the v9.2 member: a
//!     second voice's part of ONE answer, turn-bound and receipted, never the
//!     turn's final.
//!   * `nonRelayType` — why this row legitimately has NO delivery receipt. An
//!     inbound group message was never relayed anywhere, so demanding a receipt
//!     for it would be nonsense; saying so EXPLICITLY is what keeps it from
//!     reading as an unbound row. A row with neither a receipt nor a
//!     `nonRelayType` is the fatal shape (see [`is_unbound`]).
//!
//! ## THE GLOBAL FEED ID — derived under the lock, never an ordinal guess
//!
//! A receipt names the exact row it proves by GLOBAL FEED ID. That id is not a
//! per-file line number: it continues across rotation, so an id handed out before
//! a rotation still names the same row afterwards. Per the gateway protocol
//! (`claw3d-bridge/src/feedLock.mjs` §9) there is NO persisted counter anywhere —
//! the id is DERIVED:
//!
//! ```text
//! id = <entries in canonical group-feed-YYYY-MM.jsonl archive segments,
//!       counted from the bytes> + <live ordinal>
//! ```
//!
//! one-based, in file order, live file last, allocated INSIDE the critical
//! section immediately after our own bytes land (so our row is the last live line
//! at that instant and no counter can drift). [`append_entry_proving`] is that
//! transaction, and [`append_entry_allocating`] is the same transaction with
//! nothing to prove — reachable ONLY with a [`NonRelayRow`], the type-level
//! declaration that no receipt could ever name this row. An outbound row cannot
//! be handed to it. The archive count is recounted FROM THE BYTES and never read from
//! `manifest.json`: the manifest is a READ accelerator whose per-segment counts
//! are believed on a name-set match even when they are wrong, and a wrong count is
//! how the feed minted the duplicate id `[1, 2, 2]` the gateway slice reproduced.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::telegram_standup::{HouseholdPersona, load_household_personas};

/// The phase of the turn a row is, re-exported so the writers stamp the SAME
/// closed enum the receipt ledger records — one vocabulary, not two that drift.
pub use super::relay_receipt::ReplyPhase;

/// The kind of a feed line: a human's group message, or an agent's reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    /// An inbound message from a human (or non-persona) in the group.
    Group,
    /// A reply relayed back into the group by an elected persona.
    Agent,
}

impl FeedKind {
    /// The wire token for this kind (`"group"` / `"agent"`), matching the
    /// gateway's `KINDS` allowlist in `conversation.mjs`.
    pub fn as_str(self) -> &'static str {
        match self {
            FeedKind::Group => "group",
            FeedKind::Agent => "agent",
        }
    }
}

/// One append-only feed line. These fields are the ENTIRE gateway contract —
/// there is deliberately no field for a token, chat id, or user id. `src_id` and
/// `origin` are the display-safe provenance pair (docs/20 §2): `src_id` is an
/// opaque restart-stable dedupe fingerprint (never a secret — see [`source_id`]),
/// `origin` is where the line came from (always `"telegram"` for this writer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// Epoch milliseconds when the line was produced.
    pub ts: i64,
    /// Display name to show in the pane (persona name, or the human's handle).
    pub sender: String,
    /// Persona id (lower-cased) for an agent line; `None` for a human.
    pub agent_id: Option<String>,
    /// Identity emoji for the sender; empty for a non-persona human.
    pub emoji: String,
    /// Whether this is a `group` (human) or `agent` (persona) line.
    pub kind: FeedKind,
    /// The message text (newlines collapsed, trimmed, length-capped).
    pub text: String,
    /// Opaque, restart-stable dedupe id for the source message (docs/20 §2), or
    /// `None` for a locally-originated line (an agent reply has no upstream id
    /// and is unique by construction). Built by [`source_id`] from the message's
    /// content fingerprint — NEVER a raw `message_id` (per-bot unstable; see
    /// `notify::mod::IncomingMessage`) and NEVER a secret. The gateway reader
    /// collapses duplicate non-null `srcId`s (`dedupeBySrcId` in
    /// `conversation.mjs`), which is what makes a listener re-delivery show once.
    pub src_id: Option<String>,
    /// Where this line physically came from — provenance, orthogonal to `kind`
    /// (who is speaking). Always [`ORIGIN_TELEGRAM`] here: this module is the
    /// Telegram listener/relay writer. Matches the reader's `ORIGINS` allowlist
    /// in `conversation.mjs`. Not a secret.
    pub origin: &'static str,
    /// The accepted turn this row belongs to, RAW and verbatim (`web-turn-<uuid
    /// v4>`), or `None` for a row with no causal turn. NEVER the internal hashed
    /// key — see the module header. An opaque gateway-minted id, not a household
    /// identifier.
    pub turn_id: Option<String>,
    /// Which phase of the turn this row is, stamped by the writer at emit time.
    pub reply_phase: Option<ReplyPhase>,
    /// Why this row legitimately has no delivery receipt, e.g.
    /// [`NON_RELAY_TELEGRAM_INBOUND`]. `None` on a row that IS a relay and must
    /// therefore be proven by one.
    pub non_relay_type: Option<String>,
}

/// `nonRelayType` for an inbound group message the listener mirrored. It was
/// never relayed anywhere, so there is no delivery to prove; this token is the
/// row saying so in machine-readable form instead of just lacking a receipt.
pub const NON_RELAY_TELEGRAM_INBOUND: &str = "telegram-inbound";

/// `nonRelayType` for a notice the pane shows but nothing relayed — a failure
/// notice written for the family's own screen, which has no transport answer to
/// prove because no transport was asked to carry it.
pub const NON_RELAY_SYSTEM_NOTICE_PANE_ONLY: &str = "system-notice-pane-only";

/// `nonRelayType` for a row that predates the sealed cutover cursor. It is
/// preserved verbatim and NEVER certifies anything; the token exists so a
/// historical row says what it is rather than reading as an unproven claim.
pub const NON_RELAY_LEGACY_PREBOUNDARY: &str = "legacy-preboundary";

/// The `nonRelayType` tokens this writer may stamp. A CLOSED set: a free-text
/// reason would let any writer excuse itself from the receipt contract by
/// inventing a category, which is the whole gate defeated by a string literal.
///
/// THE SET IS THE SCHEMA'S, NOT OURS. `receipt-observe-schema-v9.1.json`
/// `non_relay_type_allowlist` is exhaustive and immutable, and this writer's
/// former `engine-lifecycle` / `diagnostic` tokens were not in it. That is not a
/// naming quibble: a token the schema does not know is an exemption from the
/// receipt contract that the auditor cannot evaluate, so an engine row could
/// opt out of being provable by inventing a category — the exact defeat the
/// closed set exists to prevent. A lifecycle or diagnostic row the engine wants
/// in the family's feed now carries its turn and its receipt like every other
/// agent row, or it is not written.
pub const NON_RELAY_TYPES: &[&str] = &[
    NON_RELAY_TELEGRAM_INBOUND,
    NON_RELAY_SYSTEM_NOTICE_PANE_ONLY,
    NON_RELAY_LEGACY_PREBOUNDARY,
];

/// Longest a mirrored message may be. Well beyond any real family message; caps
/// a pathological line so it can neither bloat the feed nor the pane payload.
const MAX_TEXT: usize = 2000;

/// Provenance tag stamped on every line this module writes. The Telegram
/// listener/relay is the sole writer here, so `origin` is always `"telegram"`;
/// it must be one of the reader's `ORIGINS` allowlist in `conversation.mjs`.
pub const ORIGIN_TELEGRAM: &str = "telegram";

impl FeedEntry {
    /// Serialize to ONE compact JSON line (no trailing newline).
    ///
    /// Built from an explicit object literal — not `#[derive(Serialize)]` on the
    /// struct — so the emitted keys are pinned to exactly the display-safe fields
    /// regardless of any future field added to [`FeedEntry`]. `agent_id` and
    /// `src_id` serialize to JSON `null` when absent. This is the privacy gate's
    /// teeth: there is no code path here that can emit a token or chat id — even
    /// `srcId` is an opaque hash (see [`source_id`]), never a raw id.
    pub fn to_json_line(&self) -> String {
        let value = serde_json::json!({
            "ts": self.ts,
            "sender": self.sender,
            "agentId": self.agent_id,
            "emoji": self.emoji,
            "kind": self.kind.as_str(),
            "text": self.text,
            "srcId": self.src_id,
            "origin": self.origin,
            "turnId": self.turn_id,
            "replyPhase": self.reply_phase.map(ReplyPhase::as_str),
            "nonRelayType": self.non_relay_type,
        });
        value.to_string()
    }

    /// Stamp the accepted turn and the phase this row is. Consuming-builder so a
    /// caller that HAS the turn cannot forget to attach it and still typecheck
    /// the same way — the writers that must carry a turn all go through here.
    ///
    /// The id is stored EXACTLY as handed over. It is validated (not repaired,
    /// not re-minted) at write time by [`append_entry_proving`] and
    /// [`append_entry_allocating`] alike: a bad id must
    /// produce NO ROW, and quietly normalising one here would produce a row
    /// carrying an id the gateway never issued.
    pub fn with_turn(mut self, turn_id: &str, phase: ReplyPhase) -> Self {
        self.turn_id = Some(turn_id.to_string());
        self.reply_phase = Some(phase);
        self
    }

    /// Declare why this row legitimately has no delivery receipt.
    ///
    /// Returns a plain [`FeedEntry`], so the row can still go through the
    /// PROVING form (`wg telegram feed-write` builds one entry and takes one
    /// path whatever the caller asked for). To reach the receipt-free append,
    /// use [`FeedEntry::declaring_non_relay`], which returns the witness that
    /// seam demands.
    pub fn with_non_relay_type(mut self, non_relay_type: &str) -> Self {
        self.non_relay_type = Some(non_relay_type.to_string());
        self
    }

    /// Declare why this row legitimately has no delivery receipt AND carry that
    /// declaration in the type — the only way to obtain a [`NonRelayRow`], and
    /// therefore the only way to reach [`append_entry_allocating`].
    ///
    /// Consuming, like the other builders: a caller cannot keep a `&FeedEntry`
    /// alias and quietly append the undeclared version instead.
    pub fn declaring_non_relay(self, non_relay_type: &str) -> NonRelayRow {
        NonRelayRow(self.with_non_relay_type(non_relay_type))
    }

    /// Is this row UNBOUND — neither bound to a causal turn (so a receipt could
    /// name it) nor declaring why it needs no receipt?
    ///
    /// This is the fatal shape the receipt contract exists to eliminate: a row
    /// that appears in the family's conversation claiming a helper spoke, with
    /// nothing anywhere that could ever prove it, and no statement that it is
    /// exempt. A row is bound by EITHER a turn id OR a `nonRelayType` — an
    /// inbound human message is legitimately unprovable-by-receipt and says so,
    /// while a relayed reply must carry the turn its receipt will join on.
    pub fn is_unbound(&self) -> bool {
        self.turn_id.is_none() && self.non_relay_type.is_none()
    }
}

/// Config-derived persona presentation used by every casa-feed writer.
///
/// The catalog deliberately contains no compiled household identities. Loading
/// a missing or malformed roster returns an error; a caller that elects to keep
/// delivery available may use [`PersonaCatalog::default`], which renders an
/// agent with a neutral id-derived label and no emoji rather than inventing a
/// household identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonaCatalog {
    personas: Vec<HouseholdPersona>,
}

impl PersonaCatalog {
    /// Build a catalog from an already-validated ordered household roster.
    pub fn from_personas(personas: Vec<HouseholdPersona>) -> Self {
        Self { personas }
    }

    /// Load the presentation catalog from `<project_root>/household.toml`.
    pub fn load(project_root: &Path) -> Result<Self> {
        Ok(Self::from_personas(load_household_personas(project_root)?))
    }

    /// The configured display name and emoji for `id`, matched case-insensitively.
    pub fn identity(&self, id: &str) -> Option<(String, String)> {
        self.personas
            .iter()
            .find(|persona| persona.id.eq_ignore_ascii_case(id.trim()))
            .map(|persona| (persona.display_name.clone(), persona.emoji.clone()))
    }
}

/// Collapse a message to a calm one-line-per-message form: runs of whitespace
/// around newlines become a single space, the whole thing is trimmed, and the
/// result is capped at [`MAX_TEXT`] characters (char-safe, never mid-codepoint).
/// Matches the reader's normalization in `conversation.mjs`.
fn normalize_text(text: &str) -> String {
    // Split on any newline, trim each fragment, drop empties, re-join with a
    // single space — this folds `"a\n\n  b"` to `"a b"`.
    let joined = text
        .split('\n')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = joined.trim();
    if trimmed.chars().count() > MAX_TEXT {
        trimmed.chars().take(MAX_TEXT).collect()
    } else {
        trimmed.to_string()
    }
}

/// Title-case an ASCII id (`"jane"` → `"Jane"`); leaves non-ASCII intact. Used
/// as the display name for an agent whose id is not in the known roster.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Build an opaque, restart-stable source id for an inbound Telegram message
/// (docs/20 §2), for the gateway reader's `dedupeBySrcId`.
///
/// It must satisfy two hard properties:
///   1. **Identical across every delivery of one physical message** — the four
///      privacy-off bots each stamp a *different* `message_id`/`update_id`
///      (observed 58/36/45/39; see `notify::telegram_dedupe`), and a fresh
///      listener re-delivers on restart. The only thing that stays equal across
///      all of that is the message's *content*: the chat it landed in, the human
///      who sent it, the second it was sent, and the text — exactly the tuple
///      `DedupeKey::from_content` keys on. So we fingerprint that tuple, NOT any
///      transport id. This is why a raw `message_id` would be wrong here.
///   2. **No secret** — the chat id and user id are secrets (this module's
///      load-bearing rule). Folding them through a hash means the resulting token
///      dedupes durably while the raw ids never appear verbatim in the feed.
///
/// The output is `tg-<16 hex>` — compact, opaque, and well under the reader's
/// 80-char `srcId` cap. Uses the std `DefaultHasher` (fixed-key SipHash), which
/// is deterministic across process runs, so a re-delivery after restart hashes
/// to the same token as the original write.
pub fn source_id(chat_id: &str, sender_id: &str, date_secs: i64, text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    chat_id.hash(&mut hasher);
    // A NUL separator so `("ab","c")` and `("a","bc")` can't collide.
    0u8.hash(&mut hasher);
    sender_id.hash(&mut hasher);
    0u8.hash(&mut hasher);
    date_secs.hash(&mut hasher);
    text.hash(&mut hasher);
    format!("tg-{:016x}", hasher.finish())
}

/// Build a `group` line for an inbound group message.
///
/// `sender` is the human's display handle (the Telegram `@username`, never a
/// numeric user id — see `notify::telegram` where the field is populated). If it
/// happens to match a known persona it is presented as that persona; otherwise
/// it is a human, so `agentId` is `null` and `emoji` is empty. `ts` is epoch ms.
/// `src_id` is the opaque dedupe fingerprint (from [`source_id`]) for this
/// inbound message, or `None` when the transport didn't surface enough to build
/// one (a `null` srcId is unique-by-construction on the read side).
pub fn group_entry(
    personas: &PersonaCatalog,
    sender: &str,
    text: &str,
    ts: i64,
    src_id: Option<String>,
) -> FeedEntry {
    match personas.identity(sender) {
        Some((name, emoji)) => FeedEntry {
            ts,
            sender: name,
            agent_id: Some(sender.trim().to_ascii_lowercase()),
            emoji,
            kind: FeedKind::Group,
            text: normalize_text(text),
            src_id,
            origin: ORIGIN_TELEGRAM,
            turn_id: None,
            reply_phase: None,
            non_relay_type: None,
        },
        None => FeedEntry {
            ts,
            sender: sender.to_string(),
            agent_id: None,
            emoji: String::new(),
            kind: FeedKind::Group,
            text: normalize_text(text),
            src_id,
            origin: ORIGIN_TELEGRAM,
            turn_id: None,
            reply_phase: None,
            non_relay_type: None,
        },
    }
}

/// Build an `agent` line for a persona's relayed reply.
///
/// `agent_id` is the persona id (e.g. `"nora"`). The display name and emoji come
/// from the roster; `agentId` is always set (lower-cased) because this is, by
/// definition, an agent line. An unknown id still yields a line (title-cased
/// name, empty emoji) rather than being dropped. `ts` is epoch ms.
///
/// An agent reply is composed locally and relayed out, so it has no upstream
/// source message and its `src_id` is `None` (unique by construction — the read
/// side never collapses a null srcId). `origin` is still `"telegram"`: the line
/// is physically written by the Telegram relay.
pub fn agent_entry(personas: &PersonaCatalog, agent_id: &str, text: &str, ts: i64) -> FeedEntry {
    let id = agent_id.trim().to_ascii_lowercase();
    let (sender, emoji) = match personas.identity(agent_id) {
        Some((name, emoji)) => (name, emoji),
        None => (title_case(&id), String::new()),
    };
    FeedEntry {
        ts,
        sender,
        agent_id: Some(id),
        emoji,
        kind: FeedKind::Agent,
        text: normalize_text(text),
        src_id: None,
        origin: ORIGIN_TELEGRAM,
        turn_id: None,
        reply_phase: None,
        non_relay_type: None,
    }
}

/// The feed path for a project root: `<root>/.casa/group-feed.jsonl`. Kept here
/// so the listener writer and the gateway reader agree on ONE location.
pub fn feed_path_for(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("group-feed.jsonl")
}

/// Append one entry to the feed, creating `.casa/` on first write.
///
/// Append-only: one compact JSON object per line plus a trailing newline.
///
/// PRIVATE ON PURPOSE. This is the raw write: no lock, no causal validation, no
/// global id, no receipt. A production caller reaching it would be an
/// unserialised append that a concurrent rotation can destroy, and an unbound row
/// nothing can ever prove — the two failures this module's transaction exists to
/// remove. Every writer goes through [`append_entry_proving`], or through
/// [`append_entry_allocating`] with a [`NonRelayRow`]; keeping this one private
/// is the structural version of that rule, which a source-sweep guard could only
/// approximate — and it is the same move [`NonRelayRow`] makes for the second
/// half of the rule, so that "which append form" is decided by the type of the
/// row rather than by whoever writes the next caller.
fn append_entry(feed_path: &Path, entry: &FeedEntry) -> std::io::Result<()> {
    if let Some(parent) = feed_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(feed_path)?;
    file.write_all(entry.to_json_line().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Current epoch milliseconds — the `ts` for a freshly mirrored line.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// The write transaction: validate, serialise, append, DERIVE the global id
// ---------------------------------------------------------------------------

/// Why a feed write was REFUSED. Every variant leaves the feed byte-identical:
/// the contract is NO ROW, not a row with a caveat, because a row on disk is a
/// thing the family sees and an auditor counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedWriteError {
    /// The `turnId` in the causal position was not a RAW `web-turn-<uuid v4>` —
    /// a hashed `web-turn-<64hex>`, an all-hyphen placeholder, a wrong-version
    /// uuid. See the module header: an id that cannot join proves nothing, and a
    /// placeholder that validates is a certification of nothing.
    BadTurnId(String),
    /// A `nonRelayType` outside the closed [`NON_RELAY_TYPES`] set.
    BadNonRelayType(String),
    /// A row bound to a turn must say which phase of it it is.
    TurnWithoutPhase,
    /// An `agent` row claimed a `nonRelayType` exemption. Every allowlisted
    /// token belongs to some OTHER row: `telegram-inbound` to an inbound human
    /// row, `system-notice-pane-only` to a notice nothing relayed,
    /// `legacy-preboundary` to history. A post-cutover agent row is a helper
    /// speaking to the family through a transport, so it has a delivery, and a
    /// delivery is proven by a receipt — never excused by a token.
    AgentClaimedExemption(String),
    /// A read this write depends on did not answer — an unreadable archive
    /// segment, an unreadable live feed. TYPED UNKNOWN, which is a different
    /// fact from "there was nothing there", and the difference is a permanent
    /// duplicate global id. See [`archived_count_from_bytes`].
    ReadUnknown {
        what: &'static str,
        detail: String,
    },
    /// The row could not be proven AND could not be taken back out. The feed now
    /// holds a row nothing proves, and that is reported as the hard failure it
    /// is rather than printed past.
    OrphanRow {
        feed_id: i64,
        detail: String,
    },
    /// The certifying feed is SEALED and this writer is not bound to a causal
    /// turn. See [`sealed_reason`].
    Sealed {
        kind: &'static str,
    },
    /// The feed could not be serialised against the gateway. The row did NOT go
    /// in — an unserialised append can be destroyed by a concurrent rotation.
    NotSerialised(super::feed_lock::LockRefusal),
    Io(String),
}

impl std::fmt::Display for FeedWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedWriteError::BadTurnId(v) => write!(
                f,
                "refusing to write a feed row whose causal turn id is not a raw web-turn-<uuid v4>: {v:?}"
            ),
            FeedWriteError::BadNonRelayType(v) => write!(
                f,
                "refusing to write a feed row with an unknown nonRelayType {v:?} (known: {})",
                NON_RELAY_TYPES.join(", ")
            ),
            FeedWriteError::TurnWithoutPhase => write!(
                f,
                "refusing to write a turn-bound feed row that does not say which phase of the turn it is"
            ),
            FeedWriteError::AgentClaimedExemption(v) => write!(
                f,
                "refusing an agent row claiming the nonRelayType exemption {v:?}: a helper row that \
                 reached the family has a delivery, and a delivery is proven by a receipt"
            ),
            FeedWriteError::ReadUnknown { what, detail } => write!(
                f,
                "the {what} could not be read ({detail}) — refusing to allocate a global feed id \
                 against evidence we cannot see in full"
            ),
            FeedWriteError::OrphanRow { feed_id, detail } => write!(
                f,
                "feed row {feed_id} could not be proven AND could not be rolled back ({detail}) — \
                 it is on disk with nothing proving it"
            ),
            FeedWriteError::Sealed { kind } => write!(
                f,
                "the certifying conversation feed is sealed: refusing an unbound {kind} row that no receipt could ever prove"
            ),
            FeedWriteError::NotSerialised(r) => write!(f, "{r}"),
            FeedWriteError::Io(m) => write!(f, "casa feed io: {m}"),
        }
    }
}

impl std::error::Error for FeedWriteError {}

/// The seal marker. Its PRESENCE seals the feed; its contents are for humans.
///
/// A file, not an environment variable, because the writers that must be sealed
/// are in DIFFERENT OS PROCESSES from whoever seals the run — the listener is a
/// long-lived daemon started long before a certification begins, and an env var
/// set by the harness would reach exactly the processes that were not the
/// problem.
pub fn seal_path_for(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("feed-seal.json")
}

/// Is this run SEALED — i.e. is the conversation feed currently being used as
/// the certifying record of what the household was told?
///
/// During a sealed run every agent row must be bound to a causal turn (so a
/// receipt can name it) or declare why it needs none. This is the gate for
/// item 7 of the contract: EVERY post-cutover engine agent writer — the native
/// Telegram mirror, lifecycle report-backs, `wg telegram feed-write --kind
/// agent` — carries the canonical causal turn + receipt, OR is prevented from
/// writing the certifying feed at all. A background report-back landing in the
/// middle of a certification as an unattributable "someone said something"
/// row is precisely the evidence-shaped noise the seal exists to keep out.
pub fn is_sealed(feed_path: &Path) -> bool {
    // The seal sits beside the feed, so it is found from the feed path alone —
    // every writer here has one, and not all of them have a project root.
    feed_path
        .parent()
        .map(|casa| casa.join("feed-seal.json"))
        .is_some_and(|p| p.exists())
}

/// Why the seal refused this row, or `None` if it does not apply.
///
/// A GROUP row is never refused. A human speaking in the household group is not
/// an engine writer, was never relayed, and must never be dropped from the
/// record — losing a family message to a certification gate would be the gate
/// causing exactly the harm it audits. It is stamped
/// [`NON_RELAY_TELEGRAM_INBOUND`] and is therefore bound, not fatal.
fn sealed_reason(feed_path: &Path, entry: &FeedEntry) -> Option<FeedWriteError> {
    if entry.kind == FeedKind::Group || !entry.is_unbound() || !is_sealed(feed_path) {
        return None;
    }
    Some(FeedWriteError::Sealed {
        kind: entry.kind.as_str(),
    })
}

/// Validate the causal fields. Runs BEFORE the lock is taken and before any
/// byte is written, so a refusal costs nothing and changes nothing.
fn validate(entry: &FeedEntry) -> Result<(), FeedWriteError> {
    if let Some(turn) = entry.turn_id.as_deref() {
        // RAW, verbatim, RFC-4122 v4. `web_physical_turn_key`'s hashed
        // `web-turn-<64hex>` fails here by construction (64 hex is not a uuid),
        // which is the point: that hash is internal idempotency identity only.
        if !super::relay_receipt::is_valid_turn_id(turn) {
            return Err(FeedWriteError::BadTurnId(turn.to_string()));
        }
        if entry.reply_phase.is_none() {
            return Err(FeedWriteError::TurnWithoutPhase);
        }
    }
    if let Some(kind) = entry.non_relay_type.as_deref() {
        if !NON_RELAY_TYPES.contains(&kind) {
            return Err(FeedWriteError::BadNonRelayType(kind.to_string()));
        }
        // …and the token has to belong to a row of THIS kind. Checking only
        // membership let an agent row pick any allowlisted string and become
        // exempt; the schema's negatives list "agent row claiming nonRelayType
        // exemption" precisely because membership alone is not the rule.
        if entry.kind == FeedKind::Agent {
            return Err(FeedWriteError::AgentClaimedExemption(kind.to_string()));
        }
    }
    Ok(())
}

/// `.casa/archive` — where rotation publishes the segments whose lines keep
/// their global ids.
fn archive_dir_for(feed_path: &Path) -> PathBuf {
    feed_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("archive")
}

/// Count entries in every canonical `group-feed-YYYY-MM.jsonl` archive segment
/// FROM THE BYTES, in file order.
///
/// Deliberately NOT from `manifest.json`. The manifest is a read accelerator
/// that is believed whenever its name set matches the files on disk — a wrong
/// per-segment COUNT is invisible to a name-set check, and an id is permanent.
/// That is exactly how the gateway slice reproduced the duplicate id `[1, 2, 2]`.
/// The allocator pays for the recount.
///
/// AND IT FAILS CLOSED. A missing archive directory is a FACT — an install that
/// has never rotated has zero archived rows — but an archive we cannot READ is
/// not that fact, it is the absence of an answer. Returning `0` for the two
/// cases alike is how the exact-tree control got `feedId=1` for a row whose
/// global id was 2: one unreadable directory, and a permanent id was reissued to
/// a second row. Every unreadable segment, and the unreadable directory itself,
/// is [`FeedWriteError::ReadUnknown`], and the write does not happen.
///
/// The exact-name predicate is also load-bearing. `.casa/archive` co-locates
/// other JSONL ledgers, including `relay-receipts-YYYY-MM.jsonl`; counting those
/// rows poisons every later feed id. Keep this predicate in lockstep with the
/// gateway's `ARCHIVE_RE` in `conversation.mjs` and `conversationObserve.mjs`.
fn is_canonical_feed_archive_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes.len() == 24
        && bytes.starts_with(b"group-feed-")
        && bytes[11..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'-'
        && bytes[16..18].iter().all(u8::is_ascii_digit)
        && &bytes[18..] == b".jsonl"
}

fn archived_count_from_bytes(feed_path: &Path) -> Result<usize, FeedWriteError> {
    let dir = archive_dir_for(feed_path);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // Nothing has rotated yet. That IS zero archived rows.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(FeedWriteError::ReadUnknown {
                what: "feed archive directory",
                detail: e.to_string(),
            });
        }
    };
    let mut names: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| FeedWriteError::ReadUnknown {
            what: "feed archive directory",
            detail: e.to_string(),
        })?;
        let path = entry.path();
        if path.file_name().is_some_and(is_canonical_feed_archive_name) {
            names.push(path);
        }
    }
    // File order: the segments are named so that lexical order IS rotation
    // order, matching the gateway's `feedSegments`.
    names.sort();
    let mut total = 0usize;
    for path in &names {
        let body = fs::read_to_string(path).map_err(|e| FeedWriteError::ReadUnknown {
            what: "feed archive segment",
            detail: e.to_string(),
        })?;
        total += count_entries(&body);
    }
    Ok(total)
}

/// Non-blank lines — one entry per line, matching the reader's parse.
fn count_entries(body: &str) -> usize {
    body.lines().filter(|l| !l.trim().is_empty()).count()
}

/// A row that has DECLARED, in the type system, why no delivery receipt can
/// ever prove it — the ONLY thing [`append_entry_allocating`] accepts.
///
/// WHY THIS IS A TYPE AND NOT A CONVENTION. `casa_feed` exposes two append
/// forms, and they are not interchangeable:
///
/// * [`append_entry_proving`] — the OUTBOUND form. The receipt is built and
///   appended inside the same critical section as the row, so "row" and "proof
///   of delivery" are one transaction with only two visible outcomes.
/// * [`append_entry_allocating`] — the RECEIPT-FREE form. There is no proof
///   callback at all, because the row is one nothing relayed: a human's own
///   inbound line, a pane-only notice, a pre-cutover historical row.
///
/// Until this witness existed, which form a caller reached was a matter of
/// discipline. Both receipt-free callers happened to be inbound mirrors stamped
/// [`NON_RELAY_TELEGRAM_INBOUND`], which is correct — but NOTHING stopped a
/// future outbound caller from reaching the receipt-free form, and the result
/// would have been precisely the defect the receipt contract exists to remove:
/// a row in the family's conversation that reads as a delivered helper reply
/// with no evidence anywhere behind it. Every existing test drives the correct
/// form, so that caller would have shipped green.
///
/// Now it cannot be written. A `&FeedEntry` — the shape every outbound builder
/// produces — does not typecheck at the receipt-free seam, and the only way to
/// obtain a `NonRelayRow` is [`FeedEntry::declaring_non_relay`], which stamps
/// the declaration it is named for. This is the same structural move that keeps
/// the raw [`append_entry`] private and makes
/// [`relay_receipt::append_locked`](super::relay_receipt::append_locked) demand
/// a `&FeedLock` witness: a rule the compiler enforces instead of a rule a
/// reviewer has to notice.
///
/// The declaration is still CHECKED, not merely claimed. The token must be in
/// the closed [`NON_RELAY_TYPES`] set, and an `agent` row may not claim any of
/// them ([`FeedWriteError::AgentClaimedExemption`]) — so minting this witness on
/// a reply is not an escape hatch, it is a refusal with a different error. The
/// type closes the hole that had no check; `validate` keeps the checks that a
/// type cannot express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonRelayRow(FeedEntry);

impl NonRelayRow {
    /// The row underneath, for a caller that must echo or inspect it. Read-only
    /// on purpose: handing back a `&mut` would let the declaration be removed
    /// after the witness was minted.
    pub fn entry(&self) -> &FeedEntry {
        &self.0
    }
}

/// Append one entry and return the GLOBAL FEED ID it was allocated — the
/// RECEIPT-FREE form, reachable only with a [`NonRelayRow`].
///
/// The whole thing is ONE critical section under the cross-process feed lock
/// (`feed_lock`, the twin of the gateway's): validate, append our bytes, then
/// derive our id while our row is provably the last live line. Nothing is
/// finished up afterwards, and nothing is inferred outside the lock.
///
/// The id is DERIVED, never counted from a persisted number and never guessed
/// from an ordinal within one file:
///
/// ```text
/// id = archived entries (from the bytes) + our one-based live ordinal
/// ```
///
/// Returns [`FeedWriteError::NotSerialised`] and writes NOTHING if the lock
/// cannot be taken: an unserialised append can land inside a rotation and be
/// present in neither the archive nor the live file — a message the household
/// said that the house then denies ever hearing.
///
/// AN OUTBOUND ROW CANNOT REACH THIS FUNCTION. A `kind: agent` row bound to a
/// turn is what [`append_entry_proving`] is for; handed here, it is a type
/// error, not a row written unproven:
///
/// ```compile_fail,E0308
/// use worksgood::notify::casa_feed::{self, PersonaCatalog, ReplyPhase};
/// let personas = PersonaCatalog::default();
/// let outbound = casa_feed::agent_entry(&personas, "harbor", "dinner is pasta", 1)
///     .with_turn("2a2f8f1e-6a2a-4a7e-9f0e-1c9f0b6d4a11", ReplyPhase::Final);
/// // NO receipt could ever name this row, and it does not say it is exempt.
/// let _ = casa_feed::append_entry_allocating(std::path::Path::new("/tmp/f"), &outbound);
/// ```
///
/// Nor can an undeclared INBOUND row — it is the missing declaration that is
/// refused, not the kind:
///
/// ```compile_fail,E0308
/// use worksgood::notify::casa_feed::{self, PersonaCatalog};
/// let personas = PersonaCatalog::default();
/// let undeclared = casa_feed::group_entry(&personas, "ari", "we are out of oats", 1, None);
/// let _ = casa_feed::append_entry_allocating(std::path::Path::new("/tmp/f"), &undeclared);
/// ```
///
/// THE CONTROL — the same call, differing by exactly the one declaration,
/// compiles. Without this, the two refusals above would pass on any typo and
/// prove nothing:
///
/// ```no_run
/// use worksgood::notify::casa_feed::{self, PersonaCatalog};
/// let personas = PersonaCatalog::default();
/// let inbound = casa_feed::group_entry(&personas, "ari", "we are out of oats", 1, None)
///     .declaring_non_relay(casa_feed::NON_RELAY_TELEGRAM_INBOUND);
/// let _ = casa_feed::append_entry_allocating(std::path::Path::new("/tmp/f"), &inbound);
/// ```
pub fn append_entry_allocating(
    feed_path: &Path,
    row: &NonRelayRow,
) -> Result<ProvenRow, FeedWriteError> {
    append_entry_proving(feed_path, &row.0, |_id, _lock| {
        Ok::<(), std::convert::Infallible>(())
    })
    .map_err(|failure| match failure {
        ProveFailure::Feed(e) => e,
        ProveFailure::Proof(never) => match never {},
    })
}

/// Why a proving append did not happen: the FEED half refused, or the PROOF half
/// did. They are kept apart because they mean different things to an operator —
/// "the row was never written" versus "the row could not be proven, so it was
/// taken back out".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProveFailure<E> {
    Feed(FeedWriteError),
    Proof(E),
}

impl<E: std::fmt::Display> std::fmt::Display for ProveFailure<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProveFailure::Feed(e) => write!(f, "{e}"),
            ProveFailure::Proof(e) => write!(f, "{e}"),
        }
    }
}

/// Append one row and PROVE it in the SAME transaction: `prove` runs inside the
/// critical section, with the row's global feed id and the held lock, and if it
/// fails the row is TAKEN BACK OUT before anyone can see it.
///
/// WHY THE TWO HALVES MAY NOT BE TWO TRANSACTIONS. A row and the receipt that
/// proves it are one fact. Written under two separate locks, a crash — or a
/// corrupt ledger, or a refused duplicate claim — between them leaves a row in
/// the family's conversation that nothing can ever prove: precisely the shape
/// the receipt contract exists to eliminate, now produced by the mechanism
/// meant to eliminate it. So the id allocation, the row bytes, and the receipt
/// all land inside ONE section, and the only two outcomes visible on disk are
/// "row and receipt" or "neither".
///
/// The rollback is a truncate back to the pre-append length, which is exact
/// because we hold the lock: no other writer can have appended after us, so the
/// only bytes past that offset are our own.
pub fn append_entry_proving<E>(
    feed_path: &Path,
    entry: &FeedEntry,
    prove: impl FnOnce(i64, &super::feed_lock::FeedLock) -> Result<(), E>,
) -> Result<ProvenRow, ProveFailure<E>> {
    validate(entry).map_err(ProveFailure::Feed)?;
    if let Some(sealed) = sealed_reason(feed_path, entry) {
        return Err(ProveFailure::Feed(sealed));
    }
    if let Some(parent) = feed_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ProveFailure::Feed(FeedWriteError::Io(e.to_string())))?;
    }
    // A MOMENTARY CONTENTION LOSS IS NOT A LOST MESSAGE (docs/42 §9,
    // `feed-lock-retry`). Six concurrent writers of the shape `wg telegram
    // feed-write` actually performs were measured at 986 ms of the 1000 ms
    // budget, so which of "recorded" and "the house never heard you" the family
    // gets was decided by scheduler noise. The retry is around the ACQUISITION
    // only — the closure below is `FnOnce`, so it cannot run twice and this row
    // cannot be duplicated — and only a typed `Timeout`, which docs/42 §3 defines
    // as "the body never ran and the feed was not touched", is ever retried. When
    // the patience is spent the refusal is the SAME `NotSerialised` as before.
    let completed = super::feed_lock::with_feed_lock_retrying(
        feed_path,
        super::feed_lock::DEFAULT_WAIT_MS,
        super::feed_lock::DEFAULT_ATTEMPTS,
        |lock| {
            // THE ARCHIVE IS COUNTED BEFORE OUR BYTES LAND. It cannot change while
            // we hold the lock, and asking first means an unreadable archive costs
            // nothing: no row is written, no id is issued, and the refusal is a
            // typed unknown rather than a silent zero.
            let archived = archived_count_from_bytes(feed_path).map_err(ProveFailure::Feed)? as i64;
            let before = match fs::metadata(feed_path) {
                Ok(m) => m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                Err(e) => {
                    return Err(ProveFailure::Feed(FeedWriteError::ReadUnknown {
                        what: "live feed",
                        detail: e.to_string(),
                    }));
                }
            };
            // A DURABILITY FAILURE IS A REFUSAL, AND A REFUSAL LEAVES NO ROW.
            // Now that `sync_dir` propagates a refused parent-directory open
            // (blocker 3), this error can arrive with our bytes already on disk
            // — and "no row, not a row with a caveat" is the contract the whole
            // module is built on. We hold the lock, so the pre-append length is
            // exact and the truncate can only be taking back our own bytes.
            if let Err(e) = append_entry_durable(feed_path, entry) {
                let failed = FeedWriteError::Io(e.to_string());
                return Err(match roll_back(feed_path, before, 0) {
                    Ok(()) => ProveFailure::Feed(failed),
                    Err(orphan) => ProveFailure::Feed(orphan),
                });
            }
            // Our bytes have landed, so our row is the LAST live line at this
            // instant — no counter is needed and none can drift. A live file we
            // cannot read back is the same unknown as an unreadable archive, so it
            // rolls our row back out instead of counting as an empty feed.
            let live = match fs::read_to_string(feed_path) {
                Ok(body) => count_entries(&body),
                Err(e) => {
                    let unknown = FeedWriteError::ReadUnknown {
                        what: "live feed",
                        detail: e.to_string(),
                    };
                    return Err(match roll_back(feed_path, before, 0) {
                        Ok(()) => ProveFailure::Feed(unknown),
                        Err(orphan) => ProveFailure::Feed(orphan),
                    });
                }
            };
            let feed_id = archived + live as i64;

            match prove(feed_id, lock) {
                Ok(()) => Ok(feed_id),
                Err(proof_error) => {
                    // ROLL BACK. An unprovable row must not survive the attempt to
                    // prove it — and a rollback that did not happen is REPORTED,
                    // not printed past: the caller learns "there is an orphan row on
                    // disk", which is a different instruction to an operator than
                    // "your receipt was refused".
                    match roll_back(feed_path, before, feed_id) {
                        Ok(()) => Err(ProveFailure::Proof(proof_error)),
                        Err(orphan) => Err(ProveFailure::Feed(orphan)),
                    }
                }
            }
        },
    )
    .map_err(|refusal| ProveFailure::Feed(FeedWriteError::NotSerialised(refusal)))?;
    // THE RELEASE VERDICT IS READ *AND RETURNED*, not dropped on the wrapper's
    // floor. The row and its receipt are certification evidence, and a section
    // whose release could not be PROVEN is a section a human has to look at.
    //
    // THE STDERR LINE IS NOT THE PROPAGATION (reviewer 7da0c79a blocker 2).
    // This function used to log the retention and then hand back `completed.out`
    // — an ordinary `Ok(feed_id)`, byte-identical at the boundary to a fully
    // proven append. The reviewer's scratch-feed control read
    // `{"apiReturnedOk":true,"feedId":1,"feedRowExists":true,"releaseWasRetained":true}`
    // 4/4: the caller could not tell. The verdict is now part of the SUCCESS
    // VALUE, so a certifier calls [`ProvenRow::certified`] and refuses, while a
    // caller whose contract really is "the row landed" says so in the source
    // with [`ProvenRow::feed_id`].
    if let super::feed_lock::Release::Retained(reason) = &completed.release {
        eprintln!(
            "[{}] casa feed: the row was written under the lock, but the release could not be \
             verified ({reason}) — this process still owns it and retries on the next acquire.",
            chrono::Utc::now().format("%H:%M:%S"),
        );
    }
    let feed_id = completed.out?;
    Ok(match completed.release {
        super::feed_lock::Release::Retained(reason) => {
            ProvenRow::ReleaseUnverified { feed_id, reason }
        }
        _ => ProvenRow::Certified(feed_id),
    })
}

/// A feed row that LANDED, and how the section that wrote it ended.
///
/// This is deliberately not an `Err`: the bytes are on disk and the global id
/// was allocated under the held lock, so telling the caller its write failed
/// would be a different lie. It is also deliberately not a bare `i64` — see
/// [`append_entry_proving`]. A caller that certifies the row must be able to
/// refuse a section nobody can vouch for, and it cannot refuse a fact it was
/// never told.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a written row carries its release verdict — certify it or take the id explicitly"]
pub enum ProvenRow {
    /// The row landed AND the release proved it let go. Fully certified.
    Certified(i64),
    /// The row landed, but the release could not be proven: this process still
    /// owns the feed lock and retries on the next acquire. The row is real; the
    /// exclusion around it is unvouched.
    ReleaseUnverified { feed_id: i64, reason: String },
}

impl ProvenRow {
    /// The global feed id, release verdict deliberately discarded — for callers
    /// whose contract really is "the row is on disk". Spelled out so the discard
    /// is a decision in the source rather than the default.
    pub fn feed_id(&self) -> i64 {
        match self {
            ProvenRow::Certified(id) => *id,
            ProvenRow::ReleaseUnverified { feed_id, .. } => *feed_id,
        }
    }

    /// The id, but only if the section's release was PROVEN. `Err` carries the
    /// unverified reason rather than the value.
    pub fn certified(self) -> Result<i64, String> {
        match self {
            ProvenRow::Certified(id) => Ok(id),
            ProvenRow::ReleaseUnverified { reason, .. } => Err(reason),
        }
    }

    /// Did the section that wrote this row prove it let the lock go?
    pub fn is_certified(&self) -> bool {
        matches!(self, ProvenRow::Certified(_))
    }
}

/// Append one row and make it DURABLE — one write, then fsync of the file and of
/// the directory. A feed row that a power loss can un-write is not a record.
fn append_entry_durable(feed_path: &Path, entry: &FeedEntry) -> std::io::Result<()> {
    if let Some(parent) = feed_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = entry.to_json_line();
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(feed_path)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    if let Some(parent) = feed_path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// fsync a directory, distinguishing "this filesystem does not implement it"
/// from "the device said no".
///
/// The `let _ = handle.sync_all()` this replaced made the durability comment
/// above it false in exactly the case it was written for: an EIO on the
/// directory entry means the row may not survive a power loss, and we returned
/// success anyway. The gateway twin draws the same three-way line
/// (`receiptLedger.mjs` §"DURABILITY IS A THREE-WAY ANSWER"), so the two
/// implementations agree about what a durable append means:
///
/// * the call is not implemented for this object (`ENOTSUP`/`EOPNOTSUPP`/
///   `EINVAL`/`EPERM`) — some network and virtual mounts, and every platform
///   that cannot open a directory as a file at all. Nothing failed; there was
///   nothing to do.
/// * anything else (`EIO`, `ENOSPC`, `EBADF`) is a device error on the bytes we
///   are publishing. It propagates.
///
/// **AND THAT LINE IS DRAWN ON THE `open` TOO** (reviewer 7da0c79a blocker 3).
/// `Err(_) => return Ok(())` used to swallow EVERY open failure, so an injected
/// EIO on the parent directory produced `appendReturnedOk: true` with the row
/// already on disk and directory durability unproved — the one case the
/// three-way answer above exists to name. The caller always supplies a concrete
/// parent it has just written into, so "I could not open it" is not evidence
/// that directory durability was unsupported: it is an unproved claim, and an
/// unproved durability claim FAILS CLOSED.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let handle = match open_dir_for_sync(dir) {
        Ok(handle) => handle,
        // The NARROW, justified exemption: the platform/filesystem cannot hand
        // out a descriptor for a directory at all. Nothing to fsync, nothing
        // claimed.
        Err(e) if fsync_unsupported(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    match handle.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if fsync_unsupported(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `File::open` on the parent, with the reviewer's injection seam sitting on the
/// exact result the classification above reads. Compiled only under `cfg(test)`
/// so the seam cannot exist in a shipped binary, and it changes no production
/// decision — it supplies the errno the real filesystem refused to.
fn open_dir_for_sync(dir: &Path) -> std::io::Result<fs::File> {
    #[cfg(test)]
    {
        let injected = inject::DIR_OPEN_ERRNO.with(|e| e.get());
        if injected != 0 {
            return Err(std::io::Error::from_raw_os_error(injected));
        }
    }
    fs::File::open(dir)
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

/// Is this fsync error the filesystem saying "not implemented" rather than
/// "the write failed"? Matches the gateway twin's ANNOUNCED set.
fn fsync_unsupported(e: &std::io::Error) -> bool {
    // Compared as VALUES, not matched as patterns: `ENOTSUP` and `EOPNOTSUPP`
    // are the same number on this platform, and a duplicate const pattern is a
    // compile warning rather than the union it reads as.
    e.raw_os_error().is_some_and(|code| {
        [libc::ENOTSUP, libc::EOPNOTSUPP, libc::EINVAL, libc::EPERM].contains(&code)
    })
}

/// Take our row back out, or say we could not.
///
/// The diagnostic print this replaced was the whole recovery story for the one
/// state the transaction exists to make impossible — a row on disk with nothing
/// proving it. A message on stderr is not a return value: the caller went on
/// believing only its receipt had failed.
fn roll_back(feed_path: &Path, before: u64, feed_id: i64) -> Result<(), FeedWriteError> {
    truncate_to(feed_path, before).map_err(|e| FeedWriteError::OrphanRow {
        feed_id,
        detail: e.to_string(),
    })
}

/// Cut the file back to `len` and make the cut durable.
fn truncate_to(feed_path: &Path, len: u64) -> std::io::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(feed_path)?;
    file.set_len(len)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The global feed id of a row whose section PROVED it let the lock go.
    ///
    /// Every existing test goes through here, so each of them is also a standing
    /// control that the ordinary path reports a certified release: if the
    /// verdict ever silently degraded, the whole suite would say so rather than
    /// one dedicated test.
    fn certified_id(row: ProvenRow) -> i64 {
        row.certified()
            .expect("an undisturbed append certifies its own release")
    }

    /// Write a row through the production PROVING transaction with a proof that
    /// always succeeds.
    ///
    /// WHY THE TESTS BELOW DO NOT USE [`append_entry_allocating`]. That form is
    /// the RECEIPT-FREE seam and now takes a [`NonRelayRow`] — a row that has
    /// declared, in the type system, that nothing relayed it (see the type's
    /// docs). Most of these tests are about ids, validation and rotation and
    /// use ordinary agent rows, which is exactly the shape that seam must
    /// refuse; reaching it from here would mean minting the witness on rows
    /// that have no business holding one, and a test helper that manufactures
    /// the very exemption the guard exists to withhold hollows the guard.
    ///
    /// This body is byte-for-byte what `append_entry_allocating` used to be, so
    /// those tests exercise the same transaction they always did — the only
    /// thing that changed is which door they knock on.
    fn write_row(feed_path: &Path, entry: &FeedEntry) -> Result<ProvenRow, FeedWriteError> {
        append_entry_proving(feed_path, entry, |_id, _lock| {
            Ok::<(), std::convert::Infallible>(())
        })
        .map_err(|failure| match failure {
            ProveFailure::Feed(e) => e,
            ProveFailure::Proof(never) => match never {},
        })
    }

    fn catalog() -> PersonaCatalog {
        PersonaCatalog::from_personas(vec![
            HouseholdPersona {
                id: "harbor".to_string(),
                display_name: "Harbor Voice".to_string(),
                emoji: "🌊".to_string(),
            },
            HouseholdPersona {
                id: "cedar".to_string(),
                display_name: "Cedar Voice".to_string(),
                emoji: "🌲".to_string(),
            },
        ])
    }

    #[test]
    fn group_entry_human_has_null_agent_id_and_empty_emoji() {
        let e = group_entry(
            &catalog(),
            "guest",
            "what's for dinner?",
            1_720_000_000_000,
            None,
        );
        assert_eq!(e.sender, "guest");
        assert_eq!(e.agent_id, None);
        assert_eq!(e.emoji, "");
        assert_eq!(e.kind, FeedKind::Group);
        assert_eq!(e.text, "what's for dinner?");
        assert_eq!(e.origin, "telegram");
    }

    #[test]
    fn group_entry_carries_the_passed_src_id() {
        let id = source_id("-100999", "555", 1_700_000_000, "hi");
        let e = group_entry(&catalog(), "guest", "hi", 1, Some(id.clone()));
        assert_eq!(e.src_id.as_deref(), Some(id.as_str()));
        // A persona-named group line carries the id too (both arms of the match).
        let p = group_entry(&catalog(), "harbor", "hi", 1, Some(id.clone()));
        assert_eq!(p.src_id.as_deref(), Some(id.as_str()));
        // No id → null, unique-by-construction on the read side.
        let n = group_entry(&catalog(), "guest", "hi", 1, None);
        assert_eq!(n.src_id, None);
    }

    #[test]
    fn agent_entry_has_null_src_id_and_telegram_origin() {
        // A relayed agent reply is locally composed: no upstream id, unique by
        // construction; still written via the Telegram relay so origin=telegram.
        let e = agent_entry(&catalog(), "harbor", "pasta tonight", 0);
        assert_eq!(e.src_id, None);
        assert_eq!(e.origin, "telegram");
    }

    #[test]
    fn source_id_is_stable_opaque_and_leaks_no_ids() {
        let (chat, user, date, text) = ("-1000000000001", "123456789", 1_700_000_000, "hello");
        let a = source_id(chat, user, date, text);
        let b = source_id(chat, user, date, text);
        // (1) Stable: identical content → identical id (so a listener restart /
        // re-delivery of the same message hashes to the same token → deduped).
        assert_eq!(
            a, b,
            "same content must yield the same source id across calls"
        );
        // Distinct content → distinct id (no accidental over-collapse).
        assert_ne!(a, source_id(chat, user, date, "goodbye"));
        assert_ne!(a, source_id(chat, user, date + 1, text));
        assert_ne!(a, source_id(chat, "999", date, text));
        assert_ne!(a, source_id("-100000", user, date, text));
        // (2) Opaque + no secret: the raw chat id and user id NEVER appear.
        assert!(a.starts_with("tg-"), "opaque token shape: {a}");
        assert!(!a.contains(chat), "source id leaked the chat id: {a}");
        assert!(!a.contains(user), "source id leaked the user id: {a}");
    }

    #[test]
    fn agent_entry_maps_from_configured_catalog_lowercased() {
        // Mixed-case id resolves to the configured face; agentId is lower-cased.
        let e = agent_entry(&catalog(), "HARBOR", "pasta tonight", 1_720_000_005_000);
        assert_eq!(e.sender, "Harbor Voice");
        assert_eq!(e.agent_id.as_deref(), Some("harbor"));
        assert_eq!(e.emoji, "🌊");
        assert_eq!(e.kind, FeedKind::Agent);

        let cedar = agent_entry(&catalog(), "cedar", "let's move", 0);
        assert_eq!(cedar.sender, "Cedar Voice");
        assert_eq!(cedar.agent_id.as_deref(), Some("cedar"));
        assert_eq!(cedar.emoji, "🌲");
    }

    #[test]
    fn agent_entry_unknown_id_still_writes_a_line() {
        let e = agent_entry(&catalog(), "Quartz", "hi", 0);
        assert_eq!(e.sender, "Quartz");
        assert_eq!(e.agent_id.as_deref(), Some("quartz"));
        assert_eq!(e.emoji, "");
    }

    #[test]
    fn catalog_loads_opaque_reordered_household_identities() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("household.toml"),
            r#"
[[agent]]
id = "cedar"
name = "Second Voice"
emoji = "②"

[[agent]]
id = "harbor"
name = "First Voice"
emoji = "①"
"#,
        )
        .unwrap();

        let loaded = PersonaCatalog::load(dir.path()).unwrap();
        assert_eq!(
            loaded.identity("CEDAR"),
            Some(("Second Voice".to_string(), "②".to_string()))
        );
        assert_eq!(
            loaded.identity("harbor"),
            Some(("First Voice".to_string(), "①".to_string()))
        );
        assert_eq!(loaded.identity("unconfigured"), None);
    }

    #[test]
    fn missing_or_malformed_catalog_has_neutral_explicit_fallback() {
        let dir = tempdir().unwrap();
        assert!(PersonaCatalog::load(dir.path()).is_err());
        fs::write(dir.path().join("household.toml"), "not = [valid").unwrap();
        assert!(PersonaCatalog::load(dir.path()).is_err());

        let fallback = PersonaCatalog::default();
        let entry = agent_entry(&fallback, "quartz", "hi", 0);
        assert_eq!(entry.sender, "Quartz");
        assert_eq!(entry.agent_id.as_deref(), Some("quartz"));
        assert_eq!(entry.emoji, "");
    }

    #[test]
    fn json_line_has_exactly_the_contract_keys_and_null_agent_id() {
        let line = group_entry(&catalog(), "guest", "hi", 42, None).to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agentId",
                "emoji",
                "kind",
                "nonRelayType",
                "origin",
                "replyPhase",
                "sender",
                "srcId",
                "text",
                "ts",
                "turnId"
            ]
        );
        // The causal trio is always PRESENT and explicitly null when unset. An
        // absent key and a null key read the same to a permissive consumer, but
        // only the null one lets an auditor tell "this writer knows about the
        // contract and this row has no turn" from "this row predates it".
        assert!(obj.get("turnId").unwrap().is_null());
        assert!(obj.get("replyPhase").unwrap().is_null());
        assert!(obj.get("nonRelayType").unwrap().is_null());
        assert!(obj.get("agentId").unwrap().is_null());
        assert_eq!(obj.get("kind").unwrap(), "group");
        assert_eq!(obj.get("ts").unwrap(), 42);
        // Provenance pair: origin present, srcId null when none was passed.
        assert_eq!(obj.get("origin").unwrap(), "telegram");
        assert!(obj.get("srcId").unwrap().is_null());
    }

    #[test]
    fn json_line_emits_src_id_when_present() {
        let id = source_id("-100999", "555", 1_700_000_000, "hi");
        let line = group_entry(&catalog(), "guest", "hi", 42, Some(id.clone())).to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v.get("srcId").unwrap(), &serde_json::Value::String(id));
        assert_eq!(v.get("origin").unwrap(), "telegram");
    }

    #[test]
    fn text_newlines_collapse_to_single_space() {
        let e = agent_entry(
            &catalog(),
            "harbor",
            "line one\n\n  line two  \nline three",
            0,
        );
        assert_eq!(e.text, "line one line two line three");
    }

    #[test]
    fn text_is_capped_without_splitting_a_codepoint() {
        let long = "é".repeat(MAX_TEXT + 500);
        let e = group_entry(&catalog(), "guest", &long, 0, None);
        assert_eq!(e.text.chars().count(), MAX_TEXT);
    }

    #[test]
    fn feed_path_is_casa_group_feed_under_root() {
        let p = feed_path_for(Path::new("/tmp/proj"));
        assert!(p.ends_with(".casa/group-feed.jsonl"), "{p:?}");
    }

    /// The privacy acceptance test: two well-formed lines (one inbound group,
    /// one relayed agent reply) land in the feed, and NO token / chat_id /
    /// user_id substring is anywhere in the file. This is the writer half of the
    /// docs/15 §chat-split contract.
    #[test]
    fn two_lines_land_and_no_secret_substrings() {
        let dir = tempdir().unwrap();
        let feed = feed_path_for(dir.path());

        // A synthetic inbound group message and a relayed agent reply — the same
        // two writes the listener performs for one round-trip.
        append_entry(
            &feed,
            &group_entry(&catalog(), "guest", "harbor, what's for dinner?", 1, None),
        )
        .unwrap();
        append_entry(
            &feed,
            &agent_entry(&catalog(), "harbor", "pasta tonight 🍝", 2),
        )
        .unwrap();

        let contents = fs::read_to_string(&feed).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "exactly two feed lines");

        // Both lines parse and carry exactly the eleven contract fields (the six
        // display fields, the provenance pair srcId/origin — docs/20 §2 — and the
        // causal trio turnId/replyPhase/nonRelayType).
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 11, "exactly eleven fields: {line}");
            for key in [
                "ts",
                "sender",
                "agentId",
                "emoji",
                "kind",
                "text",
                "srcId",
                "origin",
                "turnId",
                "replyPhase",
                "nonRelayType",
            ] {
                assert!(obj.contains_key(key), "missing {key} in {line}");
            }
            // Provenance is always the Telegram writer's tag.
            assert_eq!(obj.get("origin").unwrap(), "telegram");
        }

        // Line 1 is the human (agentId null), line 2 is the persona.
        let l1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(l1["kind"], "group");
        assert!(l1["agentId"].is_null());
        let l2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l2["kind"], "agent");
        assert_eq!(l2["agentId"], "harbor");

        // PRIVACY: none of the secrets a live listener handles may appear. These
        // are the exact token / chat_id / user_id shapes from a real Casa config.
        for secret in [
            "0000000000:dummy-token", // bot token
            "bot_token",
            "-1000000000001", // group chat_id
            "chat_id",
            "123456789", // telegram user id
            "user_id",
        ] {
            assert!(
                !contents.contains(secret),
                "feed leaked secret substring {secret:?}: {contents}"
            );
        }
    }

    /// THE exactly-once reproducer (docs/20 §2, task bug-chat-rust). The listener
    /// at-least-once re-delivers on restart, so the SAME inbound group message can
    /// be written to the feed twice. Because each write now carries a stable
    /// [`source_id`] fingerprint, both physical lines share ONE non-null `srcId`,
    /// which is exactly what the gateway's `dedupeBySrcId` collapses to a single
    /// pane message. Before this fix both lines were `srcId:null` (unique by
    /// construction), so the reader showed the message twice.
    #[test]
    fn identical_redelivered_inbound_lines_share_one_src_id() {
        let dir = tempdir().unwrap();
        let feed = feed_path_for(dir.path());

        // The stable content fingerprint: identical across bot fan-out AND across
        // a listener restart, because it is derived from the message content, not
        // a per-bot transport id.
        let (chat, user, date, text) = ("-1000000000001", "123456789", 1_700_000_000, "who cooks?");
        let sid = source_id(chat, user, date, text);

        // Two writes of the same physical message (the restart re-delivery).
        append_entry(
            &feed,
            &group_entry(&catalog(), "guest", text, 10, Some(sid.clone())),
        )
        .unwrap();
        append_entry(
            &feed,
            &group_entry(&catalog(), "guest", text, 11, Some(sid.clone())),
        )
        .unwrap();

        let contents = fs::read_to_string(&feed).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "the append-only writer writes both physically"
        );

        // Both carry the SAME non-null srcId → the reader's dedupeBySrcId collapses
        // them to one pane line. (On main both would be srcId:null → shown twice.)
        let s0 = serde_json::from_str::<serde_json::Value>(lines[0]).unwrap();
        let s1 = serde_json::from_str::<serde_json::Value>(lines[1]).unwrap();
        assert!(!s0["srcId"].is_null(), "srcId must be non-null to dedupe");
        assert_eq!(s0["srcId"], s1["srcId"], "re-delivery shares one srcId");

        // PRIVACY still holds even though srcId is DERIVED from the chat/user ids:
        // the fingerprint is a hash, so neither raw id appears verbatim.
        assert!(
            !contents.contains(chat),
            "chat id leaked via srcId: {contents}"
        );
        assert!(
            !contents.contains(user),
            "user id leaked via srcId: {contents}"
        );
    }

    // -----------------------------------------------------------------------
    // The causal contract: the raw turn, the derived id, the seal
    // -----------------------------------------------------------------------

    const TURN: &str = "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3301";

    fn scratch_feed() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let feed = feed_path_for(dir.path());
        fs::create_dir_all(feed.parent().unwrap()).unwrap();
        (dir, feed)
    }

    /// **BLOCKER 3 (reviewer 7da0c79a, 4/4) — A FAILED PARENT OPEN IS A FAILED
    /// SYNC.** `sync_dir` mapped every `File::open(dir)` error to `Ok(())`, so an
    /// injected EIO on the feed's parent produced:
    ///
    /// ```json
    /// {"appendReturnedOk":true,"fileExists":true,
    ///  "injectedDirectoryOpenEio":true,"negativeControlReturnedOk":true}
    /// ```
    ///
    /// The row was on disk, its directory entry was unproved, and the API said
    /// yes. Every field of that control is asserted here with the first one
    /// INVERTED — and the feed is byte-identical afterwards, because "NO ROW, not
    /// a row with a caveat" is the module's contract for every refusal.
    #[test]
    fn an_injected_eio_on_the_feeds_parent_is_a_refused_append() {
        let (_dir, feed) = scratch_feed();

        // The NEGATIVE CONTROL, uninjected and on the same fixture.
        let first = certified_id(
            write_row(&feed, &agent_entry(&catalog(), "harbor", "one", 1))
                .expect("the uninjected control must append"),
        );
        assert_eq!(first, 1);
        let unharmed = fs::read_to_string(&feed).unwrap();

        let refused = {
            let _armed = inject::Armed::with(libc::EIO);
            write_row(&feed, &agent_entry(&catalog(), "harbor", "two", 2))
        };
        let err = refused
            .expect_err("a row whose directory durability could not be proved must NOT return Ok");
        assert!(
            matches!(&err, FeedWriteError::Io(detail) if detail.contains("Input/output error")),
            "the refusal must name the device error it actually got: {err:?}"
        );
        assert_eq!(
            fs::read_to_string(&feed).unwrap(),
            unharmed,
            "a refused append leaves the feed byte-identical — the row the family \
             would have seen must not survive the refusal that named it"
        );
    }

    /// The seam is a TEST SEAM, not a behaviour: with it disarmed the very same
    /// append succeeds.
    #[test]
    fn the_feeds_directory_open_seam_is_off_by_default() {
        let (_dir, feed) = scratch_feed();
        assert_eq!(
            certified_id(
                write_row(&feed, &agent_entry(&catalog(), "harbor", "one", 1))
                    .expect("no seam is armed, so the append proceeds")
            ),
            1
        );
    }

    /// **BLOCKER 2 (reviewer 7da0c79a, 4/4) — A RETAINED RELEASE IS NOT AN
    /// ORDINARY SUCCESS.** `append_entry_proving` logged the retention and
    /// returned `completed.out`, so the certifying API's caller read:
    ///
    /// ```json
    /// {"apiReturnedOk":true,"feedId":1,"feedRowExists":true,"releaseWasRetained":true}
    /// ```
    ///
    /// — an ordinary `Ok(feed_id)`, byte-identical at the boundary to a fully
    /// proven append. The row and the id are still real (they are, and this
    /// asserts both), but the verdict now travels WITH them.
    #[test]
    fn a_retained_release_makes_the_written_row_typed_degraded_not_plain_ok() {
        let (_dir, feed) = scratch_feed();

        // Force the retention the honest way, exactly as the project-lock test
        // does: make the lock RECORD unreadable while we hold it, so
        // `verify_self_ownership` answers Unverifiable, the release cannot prove
        // anything, and §4's rule applies — "I could not read it" never softens
        // into "it is probably still mine".
        let entry = agent_entry(&catalog(), "harbor", "the row that landed", 1);
        let lock_path = super::super::feed_lock::lock_path_for(&feed);
        let written = {
            let _wedge = UnreadableOnce::arm(lock_path.clone());
            append_entry_proving(&feed, &entry, |_id, _lock| {
                // The row is already on disk here; the wedge fires on RELEASE.
                UnreadableOnce::wedge(&lock_path);
                Ok::<(), std::convert::Infallible>(())
            })
        };
        let row = written.expect("the row itself landed — the write did not fail");
        // 1. The row and its id are real...
        assert_eq!(
            row.feed_id(),
            1,
            "feedId=1, exactly as the control recorded"
        );
        assert_eq!(
            count_entries(&fs::read_to_string(&feed).unwrap()),
            1,
            "feedRowExists — the mutation happened and is not rolled back"
        );
        // 2. ...and the caller can SEE that nobody vouched for the section.
        assert!(
            !row.is_certified(),
            "an unverified release must not present as a certified row: {row:?}"
        );
        assert!(
            matches!(&row, ProvenRow::ReleaseUnverified { feed_id: 1, reason }
                     if reason.starts_with("unverifiable")),
            "the reason travels with the row, not only to stderr: {row:?}"
        );
        assert!(
            row.certified().is_err(),
            "a certifier must be able to REFUSE this row"
        );
        let _ = fs::remove_file(&lock_path);
    }

    /// The negative half: an undisturbed proving append certifies its own
    /// release. Without it, "not certified" would be satisfiable by never
    /// certifying anything.
    #[test]
    fn an_undisturbed_proving_append_certifies_its_release() {
        let (_dir, feed) = scratch_feed();
        let row = append_entry_proving(
            &feed,
            &agent_entry(&catalog(), "harbor", "one", 1),
            |_id, _lock| Ok::<(), std::convert::Infallible>(()),
        )
        .unwrap();
        assert_eq!(row, ProvenRow::Certified(1));
        assert!(row.is_certified());
    }

    /// Makes the lock record unreadable for the duration, and restores the
    /// directory's mode however the test ends.
    struct UnreadableOnce(PathBuf);

    impl UnreadableOnce {
        fn arm(lock_path: PathBuf) -> UnreadableOnce {
            UnreadableOnce(lock_path)
        }

        /// Truncate the held lock record to nothing: it is then present but
        /// UNATTRIBUTABLE, which is the `Unverifiable` branch of
        /// `verify_self_ownership` — not `Foreign` (that would be a successor)
        /// and not `Gone`.
        fn wedge(lock_path: &Path) {
            let _ = fs::write(lock_path, b"");
        }
    }

    impl Drop for UnreadableOnce {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// ITEM 8 — RAW vs HASHED. `web_physical_turn_key()` hashes the accepted
    /// turn into `web-turn-<64hex>` for the engine's INTERNAL delivery digest.
    /// That hash in the causal position cannot join a gateway row carrying the
    /// raw id, so the join silently finds nothing and every row reads as
    /// unproven. Each of these produces NO ROW — not a row with a caveat.
    #[test]
    fn a_hashed_all_hyphen_or_wrong_version_turn_id_writes_no_row() {
        for bad in [
            // The internal idempotency key: `web-turn-` + 64 hex.
            "web-turn-1e4d3c2b1a09f8e7d6c5b4a3928170695e4d3c2b1a09f8e7d6c5b4a392817069",
            // The all-hyphen placeholder a never-minted id decays to.
            "web-turn-00000000-0000-0000-0000-000000000000",
            // A v1 uuid — right shape, wrong version nibble.
            "web-turn-3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            // Right version, wrong variant nibble.
            "web-turn-3f2504e0-4f89-41d3-1a0c-0305e82c3301",
            // Bare, unprefixed.
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
            "",
        ] {
            let (_dir, feed) = scratch_feed();
            let entry = agent_entry(&catalog(), "harbor", "dinner is pasta", 1)
                .with_turn(bad, ReplyPhase::Final);

            let err = write_row(&feed, &entry).unwrap_err();
            assert_eq!(
                err,
                FeedWriteError::BadTurnId(bad.to_string()),
                "turn id {bad:?} must be refused"
            );
            assert!(
                !feed.exists() || fs::read_to_string(&feed).unwrap().is_empty(),
                "a refused turn id must leave NO ROW on disk (id {bad:?})"
            );
        }
    }

    /// The positive control for the test above: the RAW accepted id is written
    /// VERBATIM — not normalised, not re-minted, not hashed on the way out.
    #[test]
    fn the_raw_accepted_turn_id_is_written_verbatim() {
        let (_dir, feed) = scratch_feed();
        let entry = agent_entry(&catalog(), "harbor", "dinner is pasta", 1)
            .with_turn(TURN, ReplyPhase::Final);

        let feed_id = certified_id(write_row(&feed, &entry).unwrap());
        assert_eq!(feed_id, 1, "the first row of a fresh feed is global id 1");

        let row: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&feed).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(row["turnId"], TURN);
        assert_eq!(row["replyPhase"], "final");
        // And the id on the row is the RAW one, byte for byte — no hashing snuck
        // in between the stamp and the disk.
        assert_eq!(row["turnId"].as_str().unwrap(), TURN);
    }

    /// A row bound to a turn must say WHICH PHASE it is. Deriving the phase from
    /// the text later is text analysis, and text analysis is how a watchdog line
    /// gets counted as the turn's final answer.
    #[test]
    fn a_turn_bound_row_must_declare_its_phase() {
        let (_dir, feed) = scratch_feed();
        let mut entry = agent_entry(&catalog(), "harbor", "still working on it", 1);
        entry.turn_id = Some(TURN.to_string());

        assert_eq!(
            write_row(&feed, &entry).unwrap_err(),
            FeedWriteError::TurnWithoutPhase
        );
        assert!(!feed.exists() || fs::read_to_string(&feed).unwrap().is_empty());

        // …AND THROUGH THE PROVING SEAM, WHICH IS THE ONE PRODUCTION USES (task
        // receipt-engine-reply). Both engine reply writers — the listener's
        // `ReplySink::mirror` and `feed-write --kind agent` — call
        // `append_entry_proving`, not the allocating wrapper. It delegates today,
        // so asserting only the wrapper is asserting an implementation detail: a
        // future proving path that validated separately would leave the writers
        // this contract is actually about uncovered.
        //
        // The PROOF HALF MUST NOT RUN EITHER. A receipt written for a row that was
        // never appended names a feed id nothing occupies — an orphan claim, which
        // is worse than the unstamped row it came from.
        let mut proof_ran = false;
        let failure = append_entry_proving(&feed, &entry, |_id, _lock| {
            proof_ran = true;
            Ok::<(), std::convert::Infallible>(())
        })
        .unwrap_err();
        assert!(
            matches!(
                failure,
                ProveFailure::Feed(FeedWriteError::TurnWithoutPhase)
            ),
            "{failure:?}"
        );
        assert!(
            !proof_ran,
            "a receipt was minted for a row the validator refused to write"
        );
        assert!(!feed.exists() || fs::read_to_string(&feed).unwrap().is_empty());

        // THE CONTROL: the same row, stamped, lands. So the refusal above is about
        // the missing phase and not about anything else in this entry.
        let stamped = agent_entry(&catalog(), "harbor", "still working on it", 1)
            .with_turn(TURN, ReplyPhase::Watchdog);
        assert_eq!(certified_id(write_row(&feed, &stamped).unwrap()), 1);
        let row: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&feed).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(row["replyPhase"], "watchdog");
    }

    /// ITEM 3 — THE EXACT-ROW JOIN. Two rows written in the SAME MILLISECOND get
    /// DIFFERENT global feed ids, and each id names the row it was allocated for.
    ///
    /// This is the test that kills the ordinal join. A joiner that pairs a
    /// receipt to a row by "the Nth row with this timestamp", or by position in
    /// a same-second bucket, picks between these two by luck: they are
    /// byte-identical except for their text and their id. Only the global id
    /// distinguishes them, which is why a receipt must carry one and may never
    /// infer one.
    #[test]
    fn two_rows_in_the_same_millisecond_get_distinct_ids_that_name_the_right_row() {
        let (_dir, feed) = scratch_feed();
        let same_ms = 1_752_000_000_000i64;

        let first = agent_entry(&catalog(), "harbor", "the FIRST answer", same_ms)
            .with_turn(TURN, ReplyPhase::Ack);
        let second = agent_entry(&catalog(), "harbor", "the SECOND answer", same_ms)
            .with_turn(TURN, ReplyPhase::Final);

        let first_id = certified_id(write_row(&feed, &first).unwrap());
        let second_id = certified_id(write_row(&feed, &second).unwrap());

        assert_ne!(
            first_id, second_id,
            "same-millisecond rows must still be distinguishable"
        );
        assert_eq!((first_id, second_id), (1, 2));

        // The join by GLOBAL ID lands on the right row...
        let rows: Vec<serde_json::Value> = fs::read_to_string(&feed)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by_global_id = |id: i64| -> &serde_json::Value { &rows[(id - 1) as usize] };
        assert_eq!(by_global_id(second_id)["text"], "the SECOND answer");
        assert_eq!(by_global_id(second_id)["replyPhase"], "final");

        // ...while the ORDINAL join everyone reaches for first — "the row with
        // this timestamp" — is AMBIGUOUS here: it matches both, and taking the
        // first match proves the WRONG row. This is the failure the global id
        // exists to prevent, and the reason `ReceiptError::NoFeedId` refuses to
        // invent one.
        let same_ts: Vec<&serde_json::Value> = rows.iter().filter(|r| r["ts"] == same_ms).collect();
        assert_eq!(same_ts.len(), 2, "both rows share the timestamp");
        assert_eq!(
            same_ts[0]["text"], "the FIRST answer",
            "an ordinal/timestamp join picks the FIRST row — the wrong one for the final answer"
        );
    }

    /// The id is GLOBAL: it continues across rotation rather than restarting per
    /// file, so an id handed out before a rotation still names the same row
    /// after it. Counted from the archive BYTES — never from `manifest.json`,
    /// whose per-segment counts are believed on a name-set match even when they
    /// are wrong (the duplicate `[1, 2, 2]` the gateway slice reproduced).
    #[test]
    fn the_global_id_continues_across_rotation_and_ignores_a_lying_manifest() {
        let (dir, feed) = scratch_feed();
        let archive = dir.path().join(".casa").join("archive");
        fs::create_dir_all(&archive).unwrap();
        // Two rotated segments holding 3 and 2 entries.
        fs::write(
            archive.join("group-feed-2026-06.jsonl"),
            "{\"ts\":1}\n{\"ts\":2}\n{\"ts\":3}\n",
        )
        .unwrap();
        fs::write(
            archive.join("group-feed-2026-07.jsonl"),
            "{\"ts\":4}\n{\"ts\":5}\n",
        )
        .unwrap();
        // A manifest that LIES about the counts. The allocator must not believe
        // it: its name set matches the files on disk, so a name-set check —
        // which is all the read fast path does — cannot tell it is wrong.
        fs::write(
            archive.join("manifest.json"),
            serde_json::json!({
                "version": 1,
                "segments": [
                    {"name": "group-feed-2026-06.jsonl", "count": 1},
                    {"name": "group-feed-2026-07.jsonl", "count": 1}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let entry = agent_entry(&catalog(), "harbor", "after the rotation", 9)
            .with_turn(TURN, ReplyPhase::Final);
        let id = certified_id(write_row(&feed, &entry).unwrap());

        assert_eq!(
            id, 6,
            "5 archived entries counted FROM THE BYTES + live ordinal 1 — not the manifest's 2"
        );
    }

    /// This table is the Rust spelling of the gateway's exact
    /// `/^group-feed-(\d{4})-(\d{2})\.jsonl$/` selector. In particular, `\d`
    /// there is ASCII-only in JavaScript without the Unicode flag and the regex
    /// intentionally checks SHAPE, not whether the month is in 01..=12.
    #[test]
    fn canonical_feed_archive_name_matches_the_gateway_archive_regex() {
        for accepted in [
            "group-feed-2026-07.jsonl",
            "group-feed-0000-00.jsonl",
            "group-feed-9999-99.jsonl",
        ] {
            assert!(
                is_canonical_feed_archive_name(OsStr::new(accepted)),
                "gateway accepts {accepted:?}, so the engine must accept it"
            );
        }

        for rejected in [
            "relay-receipts-2026-07.jsonl",
            "arbitrary.jsonl",
            "group-feed-2026-7.jsonl",
            "group-feed-2026-007.jsonl",
            "group-feed-20260-07.jsonl",
            "group-feed-2026-0a.jsonl",
            "group-feed-2026-07.jsonl.bak",
            ".group-feed-2026-07.jsonl",
            "group-feed-２０２６-０７.jsonl",
        ] {
            assert!(
                !is_canonical_feed_archive_name(OsStr::new(rejected)),
                "gateway rejects {rejected:?}, so the engine must reject it"
            );
        }
    }

    /// C034's exact failure shape: feed and receipt archives are deliberately
    /// co-located. Only the canonical feed segment contributes to the global
    /// id; counting the receipt rows shifts the receipt join onto a row that
    /// does not exist in the gateway's view.
    #[test]
    fn co_located_receipt_archive_does_not_shift_the_global_feed_id() {
        let (dir, feed) = scratch_feed();
        let archive = dir.path().join(".casa").join("archive");
        fs::create_dir_all(&archive).unwrap();
        fs::write(
            archive.join("group-feed-2026-07.jsonl"),
            "{\"ts\":1}\n{\"ts\":2}\n{\"ts\":3}\n",
        )
        .unwrap();
        fs::write(
            archive.join("relay-receipts-2026-07.jsonl"),
            "{\"receiptId\":1}\n{\"receiptId\":2}\n",
        )
        .unwrap();
        fs::write(&feed, "{\"ts\":4}\n").unwrap();

        let entry = agent_entry(&catalog(), "harbor", "the C034 reply", 5)
            .with_turn(TURN, ReplyPhase::Final);
        assert_eq!(
            certified_id(write_row(&feed, &entry).unwrap()),
            5,
            "3 archived feed rows + live ordinal 2; the 2 receipt rows are not feed history"
        );
    }

    /// Non-feed JSONL entries are outside the selector and therefore must not
    /// even be opened. A directory with a receipt-archive filename is an
    /// unreadable-as-a-file control: the old broad `*.jsonl` selector refused
    /// this write, while the gateway-compatible selector ignores it.
    #[test]
    fn an_unreadable_non_feed_jsonl_entry_is_ignored() {
        let (dir, feed) = scratch_feed();
        let archive = dir.path().join(".casa").join("archive");
        fs::create_dir_all(archive.join("relay-receipts-2026-07.jsonl")).unwrap();

        let entry = agent_entry(&catalog(), "harbor", "the first feed row", 9)
            .with_turn(TURN, ReplyPhase::Final);
        assert_eq!(certified_id(write_row(&feed, &entry).unwrap()), 1);
    }

    /// AN UNREADABLE ARCHIVE IS NOT AN EMPTY ONE. The exact-tree control:
    /// archive one row, make `.casa/archive` unreadable, append a live row —
    /// and the writer reported `feedId=1` for a row whose global id was 2,
    /// reissuing a permanent id to a second row. That is the very duplicate the
    /// recount-from-bytes exists to prevent, arriving through the `unwrap_or`
    /// underneath it.
    #[test]
    fn an_unreadable_archive_refuses_the_write_instead_of_counting_zero() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, feed) = scratch_feed();
        let archive = dir.path().join(".casa").join("archive");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("group-feed-2026-07.jsonl"), "{\"ts\":1}\n").unwrap();

        let entry =
            agent_entry(&catalog(), "harbor", "the live row", 9).with_turn(TURN, ReplyPhase::Final);
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o000)).unwrap();
        let refused = write_row(&feed, &entry);
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            matches!(
                refused,
                Err(FeedWriteError::ReadUnknown {
                    what: "feed archive directory",
                    ..
                })
            ),
            "an unreadable archive must be typed unknown, got {refused:?}"
        );
        assert!(
            !feed.exists() || fs::read_to_string(&feed).unwrap().is_empty(),
            "the refusal writes NO ROW: the id it would have carried is unknowable"
        );

        // …and once the archive is readable again the same row gets the id it
        // always had: 1 archived + live ordinal 1.
        assert_eq!(certified_id(write_row(&feed, &entry).unwrap()), 2);
    }

    /// The same rule for an unreadable SEGMENT — the `unwrap_or_default()` per
    /// file, which loses a whole segment's worth of ids rather than the whole
    /// directory's.
    #[test]
    fn an_unreadable_archive_segment_refuses_the_write() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, feed) = scratch_feed();
        let archive = dir.path().join(".casa").join("archive");
        fs::create_dir_all(&archive).unwrap();
        let segment = archive.join("group-feed-2026-07.jsonl");
        fs::write(&segment, "{\"ts\":1}\n{\"ts\":2}\n").unwrap();
        fs::set_permissions(&segment, fs::Permissions::from_mode(0o000)).unwrap();

        let entry =
            agent_entry(&catalog(), "harbor", "the live row", 9).with_turn(TURN, ReplyPhase::Final);
        let refused = write_row(&feed, &entry);
        fs::set_permissions(&segment, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(
            matches!(
                refused,
                Err(FeedWriteError::ReadUnknown {
                    what: "feed archive segment",
                    ..
                })
            ),
            "an unreadable segment must be typed unknown, got {refused:?}"
        );
        assert!(!feed.exists() || fs::read_to_string(&feed).unwrap().is_empty());
        assert_eq!(certified_id(write_row(&feed, &entry).unwrap()), 3);
    }

    /// A MISSING archive directory is still the fact it always was: an install
    /// that has never rotated has zero archived rows. Fail-closed must not mean
    /// fail-always — this is the control that stops the fix above from being a
    /// blanket refusal.
    #[test]
    fn a_never_rotated_install_still_allocates_from_zero() {
        let (dir, feed) = scratch_feed();
        assert!(!archive_dir_for(&feed).exists());
        let entry = agent_entry(&catalog(), "harbor", "the first row ever", 9)
            .with_turn(TURN, ReplyPhase::Final);
        assert_eq!(certified_id(write_row(&feed, &entry).unwrap()), 1);
        drop(dir);
    }

    /// ITEM 6 — an inbound Telegram row is writer-stamped `kind:group` +
    /// `nonRelayType:telegram-inbound`, and is therefore BOUND: it says why no
    /// receipt could ever prove it, instead of merely lacking one.
    ///
    /// AND IT STILL LANDS THROUGH THE RECEIPT-FREE SEAM ITSELF. This is the
    /// control on [`NonRelayRow`]: the guard that stops an outbound row from
    /// reaching [`append_entry_allocating`] must not be a blanket refusal, so
    /// the row the listener actually writes goes through that exact call here —
    /// not through the test helper — and lands.
    #[test]
    fn an_inbound_group_row_is_stamped_non_relay_and_is_not_unbound() {
        let (_dir, feed) = scratch_feed();
        let entry = group_entry(&catalog(), "guest", "what's for dinner?", 7, None)
            .declaring_non_relay(NON_RELAY_TELEGRAM_INBOUND);
        assert!(!entry.entry().is_unbound());

        certified_id(append_entry_allocating(&feed, &entry).unwrap());
        let row: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&feed).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(row["kind"], "group");
        assert_eq!(row["nonRelayType"], NON_RELAY_TELEGRAM_INBOUND);
        assert!(
            row["turnId"].is_null(),
            "a human's message has no causal turn"
        );
    }

    /// The `nonRelayType` vocabulary is a CLOSED set. Free text would let any
    /// writer excuse itself from the receipt contract by inventing a category —
    /// the whole gate defeated by a string literal.
    #[test]
    fn an_invented_non_relay_type_writes_no_row() {
        let (_dir, feed) = scratch_feed();
        // Through the RECEIPT-FREE SEAM ITSELF: minting the witness is what a
        // writer inventing a category would do, and the seam must refuse it
        // rather than treat a declaration as proof that one was owed.
        let entry = agent_entry(&catalog(), "harbor", "trust me", 1)
            .declaring_non_relay("no-receipt-needed-honest");
        assert_eq!(
            append_entry_allocating(&feed, &entry).unwrap_err(),
            FeedWriteError::BadNonRelayType("no-receipt-needed-honest".to_string())
        );
        assert!(!feed.exists() || fs::read_to_string(&feed).unwrap().is_empty());
    }

    /// ITEM 7 — during a SEALED run an unbound AGENT row is refused outright.
    /// A background report-back landing mid-certification as an unattributable
    /// "someone said something" row is evidence-shaped noise, and the seal keeps
    /// it out of the certifying record rather than letting an auditor discover it
    /// afterwards.
    #[test]
    fn a_sealed_run_refuses_an_unbound_agent_row() {
        let (dir, feed) = scratch_feed();
        fs::write(seal_path_for(dir.path()), "{\"sealed\":true}").unwrap();
        assert!(is_sealed(&feed));

        let unbound = agent_entry(&catalog(), "harbor", "background report-back", 1);
        assert!(unbound.is_unbound());
        assert_eq!(
            write_row(&feed, &unbound).unwrap_err(),
            FeedWriteError::Sealed { kind: "agent" }
        );
        assert!(
            !feed.exists() || fs::read_to_string(&feed).unwrap().is_empty(),
            "the sealed refusal writes NO ROW"
        );

        // A BOUND agent row still goes in — the seal blocks unprovable rows, not
        // the conversation itself.
        let bound = agent_entry(&catalog(), "harbor", "dinner is pasta", 2)
            .with_turn(TURN, ReplyPhase::Final);
        assert_eq!(certified_id(write_row(&feed, &bound).unwrap()), 1);

        // ...but an agent row that DECLARES ITSELF EXEMPT does not, whatever
        // token it picks. This is the half the exact-tree control caught: the
        // seal's own gate reads "bound EITHER by a turn OR by a nonRelayType",
        // so an agent row only had to name an exemption to walk through a sealed
        // run unprovable. Every allowlisted token belongs to some other row's
        // situation; a helper that spoke to the family has a delivery.
        //
        // Driven through [`append_entry_allocating`] itself, because that IS the
        // remaining way an agent row can reach the receipt-free seam now that a
        // bare `&FeedEntry` does not typecheck there: the witness is mintable on
        // any row, and this is the assertion that minting it buys an agent row
        // nothing.
        let declared = agent_entry(&catalog(), "harbor", "listener restarted", 3)
            .declaring_non_relay(NON_RELAY_TELEGRAM_INBOUND);
        assert_eq!(
            append_entry_allocating(&feed, &declared).unwrap_err(),
            FeedWriteError::AgentClaimedExemption(NON_RELAY_TELEGRAM_INBOUND.to_string())
        );
        assert_eq!(
            fs::read_to_string(&feed).unwrap().lines().count(),
            1,
            "the exemption refusal leaves the one bound row and nothing else"
        );
    }

    /// The same refusal with the SEAL ABSENT. A sealed run is when an auditor is
    /// watching; the contract is not "be provable while observed".
    #[test]
    fn an_agent_row_may_not_claim_a_non_relay_exemption_sealed_or_not() {
        let (dir, feed) = scratch_feed();
        assert!(!is_sealed(&feed));
        for token in NON_RELAY_TYPES {
            let entry = agent_entry(&catalog(), "harbor", "an unprovable helper line", 1)
                .declaring_non_relay(token);
            assert_eq!(
                append_entry_allocating(&feed, &entry).unwrap_err(),
                FeedWriteError::AgentClaimedExemption((*token).to_string()),
                "{token} let an agent row out of the receipt contract"
            );
        }
        assert!(!feed.exists() || fs::read_to_string(&feed).unwrap().is_empty());
        drop(dir);
    }

    /// THE WITNESS HAS EXACTLY ONE MINT, AND THIS IS THE ONLY FILE THAT COULD
    /// ADD A SECOND.
    ///
    /// [`NonRelayRow`]'s field is private, so the compiler already stops every
    /// other module from building one out of an arbitrary row — an outbound
    /// caller cannot reach [`append_entry_allocating`] at all. What the compiler
    /// cannot stop is THIS module growing a second, kinder constructor: one
    /// `pub fn as_non_relay(entry: FeedEntry) -> NonRelayRow` added for
    /// convenience and the type gate is a formality again, with every existing
    /// test still green because they all drive the correct form.
    ///
    /// So the construction sites are counted, here, beside the type. Two: the
    /// declaration itself, and [`FeedEntry::declaring_non_relay`], which stamps
    /// the declaration the witness is named for.
    ///
    /// The needle is assembled from parts on purpose — spelled out verbatim,
    /// this test's own source would be one of the hits it counts.
    #[test]
    fn the_only_way_to_mint_the_receipt_free_witness_is_the_declaration() {
        let me = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("notify")
            .join("casa_feed.rs");
        let body = fs::read_to_string(&me).expect("this module reads its own source");
        // A broken path or a renamed file would otherwise pass this guard with
        // zero hits and zero complaints.
        assert!(
            body.contains("pub fn append_entry_allocating("),
            "the sweep did not read this module ({}): a guard that reads the wrong file counts \
             nothing and passes",
            me.display(),
        );
        let mint = format!("{}{}(", "NonRelay", "Row");
        let sites: Vec<String> = body
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(&mint) && !l.trim_start().starts_with("//"))
            .map(|(i, l)| format!("{}: {}", i + 1, l.trim()))
            .collect();
        assert_eq!(
            sites.len(),
            2,
            "the receipt-free witness gained a construction site: {sites:#?}\n\
             Exactly two are expected — the `struct` declaration and \
             `FeedEntry::declaring_non_relay`. A third means some row can now be \
             called non-relay WITHOUT declaring it, which is the receipt-free \
             append reopened. If the new one really does stamp a declaration, \
             say so here and raise the count; otherwise it belongs on \
             `append_entry_proving`.",
        );
    }

    /// The allowlist IS the schema's. `engine-lifecycle` and `diagnostic` were
    /// this writer's own inventions and are not in v9.1's
    /// `non_relay_type_allowlist`; a token the auditor's schema does not define
    /// is an exemption nobody can evaluate.
    #[test]
    fn the_non_relay_allowlist_is_exactly_the_schema_set() {
        assert_eq!(
            NON_RELAY_TYPES,
            &[
                "telegram-inbound",
                "system-notice-pane-only",
                "legacy-preboundary"
            ]
        );
        let (_dir, feed) = scratch_feed();
        for retired in ["engine-lifecycle", "diagnostic"] {
            let entry =
                group_entry(&catalog(), "someone", "text", 1, None).declaring_non_relay(retired);
            assert_eq!(
                append_entry_allocating(&feed, &entry).unwrap_err(),
                FeedWriteError::BadNonRelayType(retired.to_string())
            );
        }
    }

    /// ITEM 7, the half that matters more. A NORMAL HOUSEHOLD GROUP MESSAGE
    /// during a sealed run must never create an unbound fatal row — and must
    /// never be dropped either. Losing a family message to a certification gate
    /// would be the gate causing exactly the harm it audits.
    #[test]
    fn a_household_group_message_during_a_sealed_run_creates_no_unbound_fatal_row() {
        let (dir, feed) = scratch_feed();
        fs::write(seal_path_for(dir.path()), "{\"sealed\":true}").unwrap();

        let human = group_entry(&catalog(), "guest", "we're out of milk", 5, None)
            .declaring_non_relay(NON_RELAY_TELEGRAM_INBOUND);
        let id = certified_id(
            append_entry_allocating(&feed, &human)
                .expect("a human's message is never refused by the seal"),
        );
        assert_eq!(id, 1);

        let rows: Vec<serde_json::Value> = fs::read_to_string(&feed)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "the message IS in the record");
        assert_eq!(rows[0]["text"], "we're out of milk");
        // Bound: it declares why it has no receipt.
        assert_eq!(rows[0]["nonRelayType"], NON_RELAY_TELEGRAM_INBOUND);
        assert!(
            rows.iter()
                .all(|r| !r["nonRelayType"].is_null() || !r["turnId"].is_null()),
            "no unbound row was created: {rows:?}"
        );
    }

    /// The seal is a FILE, not an environment variable, because the writers it
    /// must bind live in other OS processes that started long before the run was
    /// sealed. Removing the file unseals.
    #[test]
    fn the_seal_is_observed_from_disk_by_any_process() {
        let (dir, feed) = scratch_feed();
        assert!(!is_sealed(&feed));
        fs::write(seal_path_for(dir.path()), "{}").unwrap();
        assert!(
            is_sealed(&feed),
            "a separate process's file seals this writer"
        );
        fs::remove_file(seal_path_for(dir.path())).unwrap();
        assert!(!is_sealed(&feed));
    }

    /// A row that cannot be serialised against the gateway is NOT written. An
    /// append landing between two steps of a rotation is present in neither the
    /// archive nor the live file — a message the family said that the house then
    /// denies ever hearing.
    #[test]
    fn a_row_that_cannot_take_the_lock_is_refused_not_written_unserialised() {
        let (_dir, feed) = scratch_feed();
        // A SECOND WRITER, and not a nested frame of this one. Since the feed lock
        // became an adapter over `project_lock`, re-entrancy is real and keyed per
        // (thread, resolved path) (docs/42 §6): a "holder" taken on THIS call stack
        // would be recognised as the same writer re-entering — correct behaviour,
        // and a fixture that proves nothing about contention. The contender takes
        // its role on a fresh thread, where the only thing the two share is the file.
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (ready, held) = std::sync::mpsc::channel::<()>();
        let contender = {
            let feed = feed.clone();
            std::thread::spawn(move || {
                let lock = super::super::feed_lock::acquire(&feed, 1000)
                    .expect("the other writer must acquire");
                ready.send(()).unwrap();
                let _ = wait.recv();
                lock.release()
            })
        };
        held.recv().expect("the other writer must acquire");

        let entry = agent_entry(&catalog(), "harbor", "dinner is pasta", 1)
            .with_turn(TURN, ReplyPhase::Final);
        let err = write_row(&feed, &entry).unwrap_err();
        assert!(
            matches!(err, FeedWriteError::NotSerialised(_)),
            "expected a serialisation refusal, got {err:?}"
        );
        assert!(
            !feed.exists() || fs::read_to_string(&feed).unwrap().is_empty(),
            "NOTHING was written"
        );
        let _ = go.send(());
        contender.join().unwrap();

        // And once the lock is free the same row goes in.
        assert_eq!(certified_id(write_row(&feed, &entry).unwrap()), 1);
    }

    /// Concurrent writers each get a DISTINCT id, and the ids are exactly
    /// 1..=N with no gap and no repeat. A duplicate id is worse than a gap: an
    /// auditor cannot tell it from a genuine repeat, and two receipts would join
    /// to the same row while one row went unproven.
    #[test]
    fn concurrent_writers_allocate_distinct_dense_ids() {
        let (_dir, feed) = scratch_feed();
        let mut handles = Vec::new();
        for worker in 0..4 {
            let feed = feed.clone();
            handles.push(std::thread::spawn(move || {
                let cat = PersonaCatalog::default();
                (0..10)
                    .map(|i| {
                        let entry = agent_entry(&cat, "harbor", &format!("w{worker} m{i}"), 1)
                            .with_turn(TURN, ReplyPhase::Final);
                        for _ in 0..50 {
                            match write_row(&feed, &entry) {
                                Ok(row) => return certified_id(row),
                                // Refused, not written — retry, exactly as a
                                // real writer does. The lock FAILS CLOSED
                                // (feed_lock §3): under contention a writer is
                                // told "I did not do it" and NOTHING is on
                                // disk, which is a visible retryable failure
                                // rather than a silently corrupted feed.
                                Err(FeedWriteError::NotSerialised(_)) => continue,
                                Err(e) => panic!("unexpected feed write failure: {e}"),
                            }
                        }
                        panic!("the feed lock was never obtainable in 50 attempts");
                    })
                    .collect::<Vec<i64>>()
            }));
        }
        let mut ids: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            (1..=40).collect::<Vec<i64>>(),
            "40 concurrent rows, ids 1..=40, no duplicate and no gap"
        );
    }
    /// A MOMENTARY CONTENTION LOSS IS NOT A LOST MESSAGE.
    ///
    /// The lock's `Timeout` means, by docs/42 §3, that our body NEVER RAN and the
    /// feed was not touched — so a second acquisition cannot double-write. The
    /// holder here keeps the lock for LONGER than one protocol budget and SHORTER
    /// than the caller's patience: attempt 1 must lose, attempt 2 must land, and
    /// the family must end up with exactly ONE row and exactly ONE proof.
    ///
    /// The retry re-acquires; it never re-runs the section. `prove` is `FnOnce`,
    /// so "the row was written twice" is a type error rather than a discipline.
    #[test]
    fn a_row_refused_by_a_momentary_holder_is_retried_and_lands_exactly_once() {
        let (_dir, feed) = scratch_feed();
        // A SECOND WRITER, on a fresh thread: re-entrancy is keyed per (thread,
        // resolved path) (docs/42 §6), so a holder taken on THIS call stack is the
        // same writer re-entering and proves nothing about contention.
        let hold_ms = super::super::feed_lock::DEFAULT_WAIT_MS + 300;
        let (ready, held) = std::sync::mpsc::channel::<()>();
        let holder = {
            let feed = feed.clone();
            std::thread::spawn(move || {
                let lock = super::super::feed_lock::acquire(&feed, 1000)
                    .expect("the other writer must acquire");
                ready.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                lock.release()
            })
        };
        held.recv().expect("the other writer must acquire");

        let entry = agent_entry(&catalog(), "harbor", "dinner is pasta", 1)
            .with_turn(TURN, ReplyPhase::Final);
        let proofs = std::sync::atomic::AtomicUsize::new(0);
        let row = append_entry_proving(&feed, &entry, |_id, _lock| {
            proofs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("a momentary contention loss must not cost the family the message");
        assert_eq!(certified_id(row), 1);
        assert_eq!(
            proofs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the proof half ran ONCE — a retry re-acquires, it never re-runs the section"
        );
        assert_eq!(
            count_entries(&fs::read_to_string(&feed).unwrap()),
            1,
            "exactly one row: a retry may not duplicate the family's message"
        );
        holder.join().unwrap();
    }

    /// THE PATIENCE IS BOUNDED, AND A REFUSAL IS STILL A REFUSAL.
    ///
    /// Held for the WHOLE window, the write fails CLOSED with nothing on disk and
    /// the refusal the family is told is still the typed `Timeout` — a retry
    /// budget may not launder a refusal into a success, and it may not become an
    /// unbounded wait that wedges the caller instead of answering it.
    #[test]
    fn a_row_refused_for_the_whole_patience_budget_fails_closed_with_nothing_written() {
        let (_dir, feed) = scratch_feed();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (ready, held) = std::sync::mpsc::channel::<()>();
        let holder = {
            let feed = feed.clone();
            std::thread::spawn(move || {
                let lock = super::super::feed_lock::acquire(&feed, 1000)
                    .expect("the other writer must acquire");
                ready.send(()).unwrap();
                let _ = wait.recv();
                lock.release()
            })
        };
        held.recv().expect("the other writer must acquire");

        let entry = agent_entry(&catalog(), "harbor", "dinner is pasta", 1)
            .with_turn(TURN, ReplyPhase::Final);
        let started = std::time::Instant::now();
        // Through `write_row` — the same transaction, spelled the way an agent
        // row must now spell it. This test's subject is the RETRY BUDGET, and
        // its row is an outbound reply, which no longer typechecks at the
        // receipt-free seam (see [`NonRelayRow`]); `write_row` is that seam's
        // former body verbatim, so the budget being measured is unchanged.
        let err = write_row(&feed, &entry).unwrap_err();
        let spent = started.elapsed();

        assert!(
            matches!(
                err,
                FeedWriteError::NotSerialised(super::super::feed_lock::LockRefusal::Timeout)
            ),
            "a Timeout is never converted to a success, and never re-typed: got {err:?}"
        );
        assert!(
            !feed.exists() || fs::read_to_string(&feed).unwrap().is_empty(),
            "NOTHING was written"
        );
        // MORE THAN ONE BUDGET WAS SPENT — the caller retried rather than giving
        // the family up on the first contention loss. A non-retrying caller
        // returns at ~one `DEFAULT_WAIT_MS`, which is what this assertion caught
        // on the pre-change build ("gave up after 1.006s").
        //
        // THE BOUNDS ARE LITERAL ON PURPOSE. Written as `DEFAULT_WAIT_MS *
        // (DEFAULT_ATTEMPTS - 1)` they read better and prove less: setting
        // `DEFAULT_ATTEMPTS = 1` — which IS the pre-change build — moves the
        // floor to zero and this test passes a caller that never retried. A
        // guard whose threshold is derived from the value it is guarding is
        // hollowed by the same edit it exists to catch. 2 s is two protocol
        // budgets, so any retrying caller clears it whatever the attempt count;
        // 5 s is the ceiling on how long a feed writer may make the family wait
        // before answering, whatever the attempt count.
        assert!(
            spent >= std::time::Duration::from_millis(2000),
            "the caller gave up after {spent:?} — less than two protocol budgets, so it did not \
             retry (DEFAULT_WAIT_MS={}, DEFAULT_ATTEMPTS={})",
            super::super::feed_lock::DEFAULT_WAIT_MS,
            super::super::feed_lock::DEFAULT_ATTEMPTS,
        );
        // AND THE PATIENCE ENDED. An unbounded retry never returns at all, and a
        // budget that quietly grew past five attempts overruns this too.
        assert!(
            spent < std::time::Duration::from_millis(5000),
            "the caller waited {spent:?} — a bounded budget must be spendable, not endless"
        );
        let _ = go.send(());
        holder.join().unwrap();
    }
}
