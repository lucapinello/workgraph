//! Casa per-agent 1:1 conversation ledger writer — the Rust WRITE side of the
//! per-(human, agent) thread the constellation kiosk and the Claw3D office read
//! (docs/15 §ledger).
//!
//! ## What this is
//!
//! One canonical thread per (human, agent) pair at
//! `<project-root>/.casa/threads/<human>__<agent>.jsonl`. A Telegram 1:1 with
//! Nora and an office/kiosk 1:1 with Nora are the SAME conversation, so they
//! land in the SAME durable thread — every surface a view onto it. The Node
//! gateway (`claw3d-bridge/src/ledger.mjs`) already READS these files, mirrors
//! off-Telegram turns to the agent's DM, and replays consumed-not-composed
//! turns on startup. `wg telegram listen` is the only process holding the
//! Telegram sockets, so it is the one that must WRITE a Telegram 1:1 turn here.
//! This module is the Rust producer half of the `ledger.mjs` contract.
//!
//! It is the per-PAIR sibling of [`super::casa_feed`] (the GROUP-scope feed):
//! same append-only-jsonl + line-ordinal-id + privacy-allowlist pattern, one
//! file per (human, agent) instead of one shared group feed.
//!
//! ## PRIVACY — the load-bearing rule of this module
//!
//! A ledger line carries ONLY display-safe fields: `ts`, `human`, `agent`,
//! `role`, `origin`, `sender`, `text`, `srcId`. It NEVER carries a bot token, a
//! Telegram `chat_id`, or a Telegram user id. [`LedgerTurn`] has no field for a
//! secret, and [`LedgerTurn::to_json_line`] emits exactly those eight keys — so
//! a careless caller structurally cannot write a token or chat id onto disk.
//! `srcId` is an OPAQUE dedupe token (a Telegram `message_id` — a per-chat
//! message sequence number the listener already treats as the cross-bot dedupe
//! identity, equivalent in role to a Telegram `update_id`; it is NOT a chat id
//! or a user id) and is never rendered on any surface.
//!
//! ## DURABLE DEDUPE + RESTART REPLAY (the 2026-07-11 lost-reply fix)
//!
//! A message is "handled" only when its turn is DURABLY RECORDED here — never
//! merely when the listener consumes it from the getUpdates queue.
//! [`record_inbound`] is idempotent on `srcId`, so a re-delivered queue copy
//! records at most once. [`pending_replies`] returns turns that were recorded
//! but never answered (consumed-not-composed), so the listener replays them on
//! startup and every message gets a reply exactly once across a restart. This
//! mirrors `recordInbound` / `pendingReplies` in `ledger.mjs`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Longest a ledger line's text may be — matches the `str` cap in `ledger.mjs`
/// (4000 chars, well beyond any real 1:1 message). Caps a pathological line so
/// it can neither bloat the thread file nor a surface payload.
const MAX_TEXT: usize = 4000;

/// Who is speaking in a turn. A thread alternates human↔agent in intent, but we
/// never assume it (a human can send twice before a reply lands).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// An inbound message from the human.
    Human,
    /// The agent's composed reply.
    Agent,
}

impl Role {
    /// The wire token (`"human"` / `"agent"`), matching the `ROLES` set in
    /// `ledger.mjs`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Human => "human",
            Role::Agent => "agent",
        }
    }
}

/// One append-only ledger line. The eight fields here are the ENTIRE gateway
/// contract — there is deliberately no field for a token, chat id, or user id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerTurn {
    /// Epoch milliseconds when the line was produced.
    pub ts: i64,
    /// Human thread key (lower-cased, `human-` prefix stripped, id-token safe).
    pub human: String,
    /// Agent (persona) thread key (lower-cased, id-token safe).
    pub agent: String,
    /// Who is speaking.
    pub role: Role,
    /// The surface this turn originated from (`"telegram"` for the listener).
    pub origin: String,
    /// Display name to show on a surface (the human's name, or the persona's).
    pub sender: String,
    /// The message text (newlines collapsed, trimmed, length-capped).
    pub text: String,
    /// Opaque source-message id for durable dedupe; `None` for a locally
    /// originated turn with no upstream id (e.g. an agent reply).
    pub src_id: Option<String>,
}

impl LedgerTurn {
    /// Serialize to ONE compact JSON line (no trailing newline).
    ///
    /// Built from an explicit object literal — not `#[derive(Serialize)]` on the
    /// struct — so the emitted keys are pinned to exactly the eight display-safe
    /// fields regardless of any future field added to [`LedgerTurn`]. `srcId`
    /// serializes to JSON `null` when absent. This is the privacy gate's teeth:
    /// there is no code path here that can emit a token, chat id, or user id.
    pub fn to_json_line(&self) -> String {
        let value = serde_json::json!({
            "ts": self.ts,
            "human": self.human,
            "agent": self.agent,
            "role": self.role.as_str(),
            "origin": self.origin,
            "sender": self.sender,
            "text": self.text,
            "srcId": self.src_id,
        });
        value.to_string()
    }
}

/// An id-token: lower-cased, restricted to `[a-z0-9_-]`, capped at `max` chars.
/// Matches `idTok` in `ledger.mjs` so the Rust writer and the Node reader agree
/// on EVERY thread key and filename. Human and agent ids are public, not
/// secrets, so this is purely a normalization (not a privacy) step.
fn id_tok(v: &str, max: usize) -> String {
    v.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .take(max)
        .collect()
}

/// The human thread key for a workgraph human agent id (e.g. `"human-luca"` →
/// `"luca"`). Lower-cases, strips the workgraph `human-` prefix, then id-token
/// normalizes — matching `gatewayCore.mjs`'s
/// `id.toLowerCase().replace(/^human-/, "")` before `ledger.mjs`'s `idTok`.
pub fn human_key(agent_or_id: &str) -> String {
    let lowered = agent_or_id.to_lowercase();
    let stripped = lowered.strip_prefix("human-").unwrap_or(&lowered);
    id_tok(stripped, 40)
}

/// Collapse a message to a calm one-line-per-turn form: runs of whitespace
/// around newlines become a single space, the whole thing is trimmed, and the
/// result is capped at [`MAX_TEXT`] characters (char-safe, never mid-codepoint).
/// Matches the reader's `str(text).replace(/\s*\n\s*/g, " ").trim()` in
/// `ledger.mjs`.
pub fn normalize_text(text: &str) -> String {
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

/// Cap a free-form display string to `max` chars, char-safe. Used for `sender`
/// (80) and `srcId` (80), matching `ledger.mjs`'s `str(v, max)`.
fn cap(v: &str, max: usize) -> String {
    if v.chars().count() > max {
        v.chars().take(max).collect()
    } else {
        v.to_string()
    }
}

/// The persona display name for a known roster id, else a title-cased fallback
/// so an unknown agent still gets a readable `sender`. Mirrors the roster in
/// [`super::casa_feed::persona_identity`] (names only — the ledger carries no
/// emoji field).
pub fn persona_label(agent_id: &str) -> String {
    match agent_id.trim().to_ascii_lowercase().as_str() {
        "nora" => "Nora".to_string(),
        "bruno" => "Bruno".to_string(),
        "mira" => "Coach Mira".to_string(),
        "otto" => "Otto".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The threads directory for a project root: `<root>/.casa/threads`. Kept here
/// so the listener writer and the gateway reader agree on ONE location.
pub fn threads_dir_for(root: &Path) -> PathBuf {
    root.join(".casa").join("threads")
}

/// The thread file for a (root, human, agent): `<root>/.casa/threads/<human>__
/// <agent>.jsonl`, both keys id-token normalized. Matches `ledgerPathFor` in
/// `ledger.mjs` (empty human → `luca`, empty agent → `unknown`).
pub fn ledger_path_for(root: &Path, human: &str, agent: &str) -> PathBuf {
    let h = {
        let t = id_tok(human, 40);
        if t.is_empty() { "luca".to_string() } else { t }
    };
    let a = {
        let t = id_tok(agent, 40);
        if t.is_empty() { "unknown".to_string() } else { t }
    };
    threads_dir_for(root).join(format!("{h}__{a}.jsonl"))
}

/// Build an inbound `human` turn. `origin` is `"telegram"` for the listener.
pub fn human_turn(
    human: &str,
    agent: &str,
    sender: &str,
    text: &str,
    src_id: Option<String>,
    ts: i64,
) -> LedgerTurn {
    let src = src_id.and_then(|s| {
        let c = cap(&s, 80);
        if c.is_empty() { None } else { Some(c) }
    });
    LedgerTurn {
        ts,
        human: id_tok(human, 40),
        agent: id_tok(agent, 40),
        role: Role::Human,
        origin: "telegram".to_string(),
        sender: cap(sender, 80),
        text: normalize_text(text),
        src_id: src,
    }
}

/// Build an `agent` reply turn. The `sender` is the persona's display name; a
/// reply has no upstream `srcId`.
pub fn agent_turn(human: &str, agent: &str, text: &str, ts: i64) -> LedgerTurn {
    LedgerTurn {
        ts,
        human: id_tok(human, 40),
        agent: id_tok(agent, 40),
        role: Role::Agent,
        origin: "telegram".to_string(),
        sender: persona_label(agent),
        text: normalize_text(text),
        src_id: None,
    }
}

/// Append one turn to a thread, creating `.casa/threads/` on first write.
///
/// A single `write_all` of one newline-terminated line is one `O_APPEND` write
/// — atomic vs other appenders on POSIX, so concurrent writers never interleave
/// or drop a line. The caller logs and swallows any error — a full disk or a
/// read-only mount must never take the listener down.
pub fn append_turn(ledger_path: &Path, turn: &LedgerTurn) -> std::io::Result<()> {
    if let Some(parent) = ledger_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path)?;
    file.write_all(turn.to_json_line().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// A parsed thread line, tagged with its 1-based `id` = line ordinal (the cursor
/// a surface polls with, stable even when two turns share a millisecond `ts`).
/// Only the fields the listener needs for dedupe + replay are surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTurn {
    pub id: usize,
    pub role: String,
    pub text: String,
    pub src_id: Option<String>,
}

/// Parse a thread file's contents into ordered turns. Malformed lines are
/// skipped (never a crash); the file is append-only so ids never shift. Matches
/// `parseLedger` in `ledger.mjs`.
pub fn parse_ledger(text: &str) -> Vec<ParsedTurn> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // tolerate a torn / partial line mid-append
        };
        if !v.is_object() {
            continue;
        }
        let role = v
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("human")
            .to_string();
        let src_id = v
            .get("srcId")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let turn_text = v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        out.push(ParsedTurn {
            id: out.len() + 1,
            role,
            text: turn_text,
            src_id,
        });
    }
    out
}

/// Read + parse a thread file. Missing file → empty. Matches the reader's
/// graceful cold-start behaviour.
fn read_all(ledger_path: &Path) -> Vec<ParsedTurn> {
    match fs::read_to_string(ledger_path) {
        Ok(s) => parse_ledger(&s),
        Err(_) => Vec::new(),
    }
}

/// Has a turn with this `srcId` already been durably recorded in this thread?
/// The dedupe primitive: a listener consuming a Telegram message checks this
/// BEFORE recording, so re-delivery of the same queue copy is a no-op. An empty
/// `srcId` is never "seen" (locally originated turns have no upstream id).
/// Matches `hasSource` in `ledger.mjs`.
pub fn has_source(ledger_path: &Path, src_id: &str) -> bool {
    if src_id.is_empty() {
        return false;
    }
    let want = cap(src_id, 80);
    read_all(ledger_path)
        .iter()
        .any(|t| t.src_id.as_deref() == Some(want.as_str()))
}

/// The outcome of a [`record_inbound`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordOutcome {
    /// A new line was appended.
    pub recorded: bool,
    /// The turn's `srcId` was already present — nothing was written.
    pub duplicate: bool,
}

/// Record an INBOUND human turn durably, idempotently. This is the "handled"
/// mark — a message is handled ONLY once it lands here. If a turn with the same
/// non-empty `srcId` is already present, this is a no-op (`duplicate: true`) so
/// two deliveries of the same queue copy record it at most once. Matches
/// `recordInbound` in `ledger.mjs`.
pub fn record_inbound(ledger_path: &Path, turn: &LedgerTurn) -> RecordOutcome {
    if let Some(src) = turn.src_id.as_deref() {
        if has_source(ledger_path, src) {
            return RecordOutcome {
                recorded: false,
                duplicate: true,
            };
        }
    }
    match append_turn(ledger_path, turn) {
        Ok(()) => RecordOutcome {
            recorded: true,
            duplicate: false,
        },
        Err(_) => RecordOutcome {
            recorded: false,
            duplicate: false,
        },
    }
}

/// Turns that were recorded but never answered — the "consumed-not-composed"
/// tail. A human turn is UNREPLIED if no agent turn follows it (higher id). On
/// listener startup, replay these so a crash between record-inbound and
/// compose-reply never eats a message: each pending turn gets a reply exactly
/// once. Returns the pending human turns in arrival order. Matches
/// `pendingReplies` in `ledger.mjs`.
pub fn pending_replies(ledger_path: &Path) -> Vec<ParsedTurn> {
    let all = read_all(ledger_path);
    let mut last_agent_id = 0;
    for t in &all {
        if t.role == "agent" {
            last_agent_id = t.id;
        }
    }
    all.into_iter()
        .filter(|t| t.role == "human" && t.id > last_agent_id)
        .collect()
}

/// A (human, agent) thread that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRef {
    pub human: String,
    pub agent: String,
    pub path: PathBuf,
}

/// List the (human, agent) pairs that have a thread file — used by the startup
/// replay sweep to find every thread with pending inbound turns. Missing dir →
/// empty (graceful cold start). Matches `listThreads` in `ledger.mjs`.
pub fn list_threads(root: &Path) -> Vec<ThreadRef> {
    let dir = threads_dir_for(root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        // `<human>__<agent>.jsonl`, both `[a-z0-9_-]+` (the id-token charset).
        let stem = match name.strip_suffix(".jsonl") {
            Some(s) => s,
            None => continue,
        };
        let parts: Vec<&str> = stem.splitn(2, "__").collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            continue;
        }
        let human = id_tok(parts[0], 40);
        let agent = id_tok(parts[1], 40);
        if human != parts[0] || agent != parts[1] {
            continue; // reject anything outside the id-token charset
        }
        out.push(ThreadRef {
            human,
            agent,
            path: entry.path(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Current epoch milliseconds — the `ts` for a freshly recorded line.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn human_key_strips_prefix_and_normalizes() {
        assert_eq!(human_key("human-luca"), "luca");
        assert_eq!(human_key("Human-Luca"), "luca");
        assert_eq!(human_key("human-erik"), "erik");
        // No prefix → just normalized.
        assert_eq!(human_key("Luca"), "luca");
        // Stray chars stripped to the id-token charset.
        assert_eq!(human_key("human-lu ca!"), "luca");
    }

    #[test]
    fn ledger_path_is_human_agent_under_threads() {
        let p = ledger_path_for(Path::new("/tmp/proj"), "luca", "nora");
        assert!(p.ends_with(".casa/threads/luca__nora.jsonl"), "{p:?}");
        // Normalization + empty fallbacks match ledger.mjs.
        let p2 = ledger_path_for(Path::new("/tmp/proj"), "LUCA", "Nora");
        assert!(p2.ends_with("luca__nora.jsonl"), "{p2:?}");
        let p3 = ledger_path_for(Path::new("/tmp/proj"), "", "");
        assert!(p3.ends_with("luca__unknown.jsonl"), "{p3:?}");
    }

    #[test]
    fn json_line_has_exactly_eight_keys() {
        let turn = human_turn("luca", "nora", "Luca", "hi", Some("42".into()), 7);
        let line = turn.to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["agent", "human", "origin", "role", "sender", "srcId", "text", "ts"]
        );
        assert_eq!(obj.get("role").unwrap(), "human");
        assert_eq!(obj.get("origin").unwrap(), "telegram");
        assert_eq!(obj.get("srcId").unwrap(), "42");
        assert_eq!(obj.get("ts").unwrap(), 7);
    }

    #[test]
    fn agent_turn_uses_persona_label_and_null_srcid() {
        let line = agent_turn("luca", "nora", "pasta tonight", 9).to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["role"], "agent");
        assert_eq!(v["sender"], "Nora");
        assert!(v["srcId"].is_null());
        // Coach Mira roster label.
        let m = agent_turn("luca", "mira", "let's move", 0).to_json_line();
        let mv: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(mv["sender"], "Coach Mira");
    }

    #[test]
    fn text_newlines_collapse_and_cap_is_char_safe() {
        let t = human_turn("luca", "nora", "L", "a\n\n  b  \nc", None, 0);
        assert_eq!(t.text, "a b c");
        let long = "é".repeat(MAX_TEXT + 500);
        let capped = human_turn("luca", "nora", "L", &long, None, 0);
        assert_eq!(capped.text.chars().count(), MAX_TEXT);
    }

    #[test]
    fn record_inbound_is_idempotent_on_srcid() {
        let dir = tempdir().unwrap();
        let path = ledger_path_for(dir.path(), "luca", "nora");
        let turn = human_turn("luca", "nora", "Luca", "are we on for dinner?", Some("111".into()), 1);

        // First delivery records.
        let r1 = record_inbound(&path, &turn);
        assert!(r1.recorded && !r1.duplicate);
        // Re-delivery of the SAME srcId is a no-op.
        let r2 = record_inbound(&path, &turn);
        assert!(!r2.recorded && r2.duplicate);

        // Exactly ONE line on disk.
        let lines: Vec<_> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 1, "re-delivery must not duplicate");
    }

    #[test]
    fn record_inbound_without_srcid_always_appends() {
        let dir = tempdir().unwrap();
        let path = ledger_path_for(dir.path(), "luca", "nora");
        // Two locally-originated turns (no srcId) both land.
        record_inbound(&path, &human_turn("luca", "nora", "L", "one", None, 1));
        record_inbound(&path, &human_turn("luca", "nora", "L", "two", None, 2));
        assert_eq!(read_all(&path).len(), 2);
    }

    #[test]
    fn pending_replies_are_consumed_not_composed_and_exactly_once() {
        let dir = tempdir().unwrap();
        let path = ledger_path_for(dir.path(), "luca", "nora");

        // Human turn recorded, never answered → pending.
        record_inbound(
            &path,
            &human_turn("luca", "nora", "L", "you there?", Some("111".into()), 1),
        );
        let pending = pending_replies(&path);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "you there?");

        // Compose the reply (record the agent turn) → no longer pending.
        append_turn(&path, &agent_turn("luca", "nora", "here!", 2)).unwrap();
        assert!(pending_replies(&path).is_empty());

        // A restart that re-delivers the same srcId records nothing new and
        // finds nothing pending → the reply happens EXACTLY ONCE across restart.
        let r = record_inbound(
            &path,
            &human_turn("luca", "nora", "L", "you there?", Some("111".into()), 3),
        );
        assert!(r.duplicate);
        assert!(pending_replies(&path).is_empty());
    }

    #[test]
    fn list_threads_finds_only_wellformed_thread_files() {
        let dir = tempdir().unwrap();
        append_turn(
            &ledger_path_for(dir.path(), "luca", "nora"),
            &human_turn("luca", "nora", "L", "hi", None, 1),
        )
        .unwrap();
        append_turn(
            &ledger_path_for(dir.path(), "erik", "bruno"),
            &human_turn("erik", "bruno", "E", "hi", None, 1),
        )
        .unwrap();
        // A non-thread file in the dir is ignored.
        fs::write(threads_dir_for(dir.path()).join("notes.txt"), b"x").unwrap();

        let mut threads = list_threads(dir.path());
        threads.sort_by(|a, b| (a.human.clone(), a.agent.clone()).cmp(&(b.human.clone(), b.agent.clone())));
        assert_eq!(threads.len(), 2);
        assert_eq!((threads[0].human.as_str(), threads[0].agent.as_str()), ("erik", "bruno"));
        assert_eq!((threads[1].human.as_str(), threads[1].agent.as_str()), ("luca", "nora"));
    }

    /// The privacy acceptance test: a full 1:1 round-trip (human turn + agent
    /// reply) lands, and NO token / chat_id / user_id substring is anywhere in
    /// the file. This is the writer half of the docs/15 §ledger contract.
    #[test]
    fn round_trip_lines_carry_no_secret_substrings() {
        let dir = tempdir().unwrap();
        let path = ledger_path_for(dir.path(), "luca", "nora");

        // The same two writes the listener performs for one 1:1 round-trip. The
        // srcId is a Telegram message_id (opaque, not a chat/user id).
        record_inbound(
            &path,
            &human_turn("luca", "nora", "Luca", "nora, what's for dinner?", Some("5521".into()), 1),
        );
        append_turn(&path, &agent_turn("luca", "nora", "pasta tonight 🍝", 2)).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "exactly two ledger lines");
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v.as_object().unwrap().len(), 8, "exactly eight fields: {line}");
        }
        let l1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(l1["role"], "human");
        assert_eq!(l1["srcId"], "5521");
        let l2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l2["role"], "agent");

        // PRIVACY: none of the secrets a live listener handles may appear. Same
        // shapes as a real Casa config (mirrors casa_feed's privacy test).
        for secret in [
            "0000000000:nora-dummy-token", // bot token
            "bot_token",
            "-1000000000001", // chat_id
            "chat_id",
            "123456789", // telegram user id
            "user_id",
        ] {
            assert!(
                !contents.contains(secret),
                "ledger leaked secret substring {secret:?}: {contents}"
            );
        }
    }
}
