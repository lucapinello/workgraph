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
//! A feed line carries ONLY six display-safe fields: `ts`, `sender`, `agentId`,
//! `emoji`, `kind`, `text`. It NEVER carries a bot token, a Telegram `chat_id`,
//! or a Telegram user id. [`FeedEntry`] has no field for a secret, and
//! [`FeedEntry::to_json_line`] emits exactly those six keys — so a careless
//! caller structurally cannot write a token or chat id onto disk. The gateway
//! re-sanitizes on read as defence in depth, but this writer is the first and
//! primary gate: secrets must never reach the file in the first place.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// One append-only feed line. The six fields here are the ENTIRE gateway
/// contract — there is deliberately no field for a token, chat id, or user id.
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
}

/// Longest a mirrored message may be. Well beyond any real family message; caps
/// a pathological line so it can neither bloat the feed nor the pane payload.
const MAX_TEXT: usize = 2000;

impl FeedEntry {
    /// Serialize to ONE compact JSON line (no trailing newline).
    ///
    /// Built from an explicit object literal — not `#[derive(Serialize)]` on the
    /// struct — so the emitted keys are pinned to exactly the six display-safe
    /// fields regardless of any future field added to [`FeedEntry`]. `agent_id`
    /// serializes to JSON `null` when absent. This is the privacy gate's teeth:
    /// there is no code path here that can emit a token or chat id.
    pub fn to_json_line(&self) -> String {
        let value = serde_json::json!({
            "ts": self.ts,
            "sender": self.sender,
            "agentId": self.agent_id,
            "emoji": self.emoji,
            "kind": self.kind.as_str(),
            "text": self.text,
        });
        value.to_string()
    }
}

/// The persona presentation (display name, identity emoji) for a known roster
/// id, or `None` for any non-persona sender.
///
/// Mirrors `telegram_standup::persona_presentation` so a voice wears the same
/// face in the standup, the group, and the conversation pane. `None` (rather
/// than a title-cased fallback) is what lets [`group_entry`] write `agentId:
/// null` for a human sender — a human is not a persona, so it has no id here.
pub fn persona_identity(id: &str) -> Option<(String, String)> {
    match id.trim().to_ascii_lowercase().as_str() {
        "nora" => Some(("Nora".to_string(), "🥗".to_string())),
        "bruno" => Some(("Bruno".to_string(), "👨\u{200d}🍳".to_string())),
        "mira" => Some(("Coach Mira".to_string(), "💪".to_string())),
        "otto" => Some(("Otto".to_string(), "📋".to_string())),
        _ => None,
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

/// Build a `group` line for an inbound group message.
///
/// `sender` is the human's display handle (the Telegram `@username`, never a
/// numeric user id — see `notify::telegram` where the field is populated). If it
/// happens to match a known persona it is presented as that persona; otherwise
/// it is a human, so `agentId` is `null` and `emoji` is empty. `ts` is epoch ms.
pub fn group_entry(sender: &str, text: &str, ts: i64) -> FeedEntry {
    match persona_identity(sender) {
        Some((name, emoji)) => FeedEntry {
            ts,
            sender: name,
            agent_id: Some(sender.trim().to_ascii_lowercase()),
            emoji,
            kind: FeedKind::Group,
            text: normalize_text(text),
        },
        None => FeedEntry {
            ts,
            sender: sender.to_string(),
            agent_id: None,
            emoji: String::new(),
            kind: FeedKind::Group,
            text: normalize_text(text),
        },
    }
}

/// Build an `agent` line for a persona's relayed reply.
///
/// `agent_id` is the persona id (e.g. `"nora"`). The display name and emoji come
/// from the roster; `agentId` is always set (lower-cased) because this is, by
/// definition, an agent line. An unknown id still yields a line (title-cased
/// name, empty emoji) rather than being dropped. `ts` is epoch ms.
pub fn agent_entry(agent_id: &str, text: &str, ts: i64) -> FeedEntry {
    let id = agent_id.trim().to_ascii_lowercase();
    let (sender, emoji) = match persona_identity(agent_id) {
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

    #[test]
    fn group_entry_human_has_null_agent_id_and_empty_emoji() {
        let e = group_entry("nadin", "what's for dinner?", 1_720_000_000_000);
        assert_eq!(e.sender, "nadin");
        assert_eq!(e.agent_id, None);
        assert_eq!(e.emoji, "");
        assert_eq!(e.kind, FeedKind::Group);
        assert_eq!(e.text, "what's for dinner?");
    }

    #[test]
    fn agent_entry_maps_from_roster_lowercased() {
        // Mixed-case id resolves to the roster face; agentId is lower-cased.
        let e = agent_entry("Nora", "pasta tonight", 1_720_000_005_000);
        assert_eq!(e.sender, "Nora");
        assert_eq!(e.agent_id.as_deref(), Some("nora"));
        assert_eq!(e.emoji, "🥗");
        assert_eq!(e.kind, FeedKind::Agent);

        let mira = agent_entry("mira", "let's move", 0);
        assert_eq!(mira.sender, "Coach Mira");
        assert_eq!(mira.agent_id.as_deref(), Some("mira"));
        assert_eq!(mira.emoji, "💪");
    }

    #[test]
    fn agent_entry_unknown_id_still_writes_a_line() {
        let e = agent_entry("Zed", "hi", 0);
        assert_eq!(e.sender, "Zed");
        assert_eq!(e.agent_id.as_deref(), Some("zed"));
        assert_eq!(e.emoji, "");
    }

    #[test]
    fn json_line_has_exactly_six_keys_and_null_agent_id() {
        let line = group_entry("nadin", "hi", 42).to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["agentId", "emoji", "kind", "sender", "text", "ts"]
        );
        assert!(obj.get("agentId").unwrap().is_null());
        assert_eq!(obj.get("kind").unwrap(), "group");
        assert_eq!(obj.get("ts").unwrap(), 42);
    }

    #[test]
    fn text_newlines_collapse_to_single_space() {
        let e = agent_entry("otto", "line one\n\n  line two  \nline three", 0);
        assert_eq!(e.text, "line one line two line three");
    }

    #[test]
    fn text_is_capped_without_splitting_a_codepoint() {
        let long = "é".repeat(MAX_TEXT + 500);
        let e = group_entry("luca", &long, 0);
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
        append_entry(&feed, &group_entry("nadin", "nora, what's for dinner?", 1)).unwrap();
        append_entry(&feed, &agent_entry("nora", "pasta tonight 🍝", 2)).unwrap();

        let contents = fs::read_to_string(&feed).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "exactly two feed lines");

        // Both lines parse and carry exactly the six contract fields.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 6, "exactly six fields: {line}");
            for key in ["ts", "sender", "agentId", "emoji", "kind", "text"] {
                assert!(obj.contains_key(key), "missing {key} in {line}");
            }
        }

        // Line 1 is the human (agentId null), line 2 is the persona.
        let l1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(l1["kind"], "group");
        assert!(l1["agentId"].is_null());
        let l2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l2["kind"], "agent");
        assert_eq!(l2["agentId"], "nora");

        // PRIVACY: none of the secrets a live listener handles may appear. These
        // are the exact token / chat_id / user_id shapes from a real Casa config.
        for secret in [
            "0000000000:nora-dummy-token", // bot token
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
}
