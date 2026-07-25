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
//! `emoji`, `kind`, `text`, plus the provenance pair `srcId` / `origin`
//! (docs/20 §2). It NEVER carries a bot token, a Telegram `chat_id`, or a
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

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::telegram_standup::{HouseholdPersona, load_household_personas};

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
}

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
        });
        value.to_string()
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
    }
}

/// The feed path for a project root: `<root>/.casa/group-feed.jsonl`. Kept here
/// so the listener writer and the gateway reader agree on ONE location.
pub fn feed_path_for(project_root: &Path) -> PathBuf {
    project_root.join(".casa").join("group-feed.jsonl")
}

/// Append one entry to the feed, creating `.casa/` on first write.
///
/// Append-only: one compact JSON object per line plus a trailing newline. The
/// caller (the listener) logs and swallows any error — a full disk or a
/// read-only mount must never take the listener down; the pane simply stays
/// where it was.
pub fn append_entry(feed_path: &Path, entry: &FeedEntry) -> std::io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
                "agentId", "emoji", "kind", "origin", "sender", "srcId", "text", "ts"
            ]
        );
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

        // Both lines parse and carry exactly the eight contract fields (the six
        // display fields plus the provenance pair srcId/origin — docs/20 §2).
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 8, "exactly eight fields: {line}");
            for key in [
                "ts", "sender", "agentId", "emoji", "kind", "text", "srcId", "origin",
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
}
