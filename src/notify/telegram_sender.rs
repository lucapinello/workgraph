//! Sender-identity resolution at the Telegram listener boundary.
//!
//! **The bug this fixes (task `group-reply-tuning`, Fix #5 — the LEAD fix).**
//! The long-poll listener used to populate an inbound message's `sender` from
//! `message.from.username` *only*, falling back to the literal string
//! `"unknown"` whenever a user had no public @username. Every downstream auth
//! check and binding lookup therefore saw `"unknown"` and rejected the sender —
//! a confirmed human (bound by their numeric Telegram user id, e.g.
//! `8905220378`) was told *"unrecognized sender 'unknown': no confirmed
//! Telegram binding"* on every message. The listener never even read
//! `message.from.id`.
//!
//! This module is the pure, network-free core of the fix. [`extract_sender`]
//! reads the raw `getUpdates` element (private message, group message, reply, or
//! callback query) and returns the [`SenderIdentity`] — the numeric user id, the
//! @username (when present), and whether the sender is a **bot** (`from.is_bot`,
//! which also drives the Fix #0 bot-loop guard). [`SenderIdentity::display`]
//! produces the label used for logs/feed (username, else the numeric id, else
//! `"unknown"`) — never `"unknown"` when an id was present, which is the whole
//! point.
//!
//! Binding resolution proper lives on
//! [`crate::agency::human_binding::TelegramBindingMap::find_by_identity`], which
//! this module composes with in [`SenderIdentity::resolve_auth_sender`] to yield
//! the canonical binding key (so the existing verbatim `find_by_user(sender)`
//! call sites downstream keep working unchanged once the boundary rewrites the
//! sender to the key that matches).

use serde_json::Value;

use crate::agency::human_binding::TelegramBindingMap;

/// The identity of whoever sent an inbound Telegram update, read once at the
/// listener boundary from the raw `from` object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderIdentity {
    /// `from.id` rendered as a string (Telegram user ids are 64-bit integers).
    /// `None` only when the update carried no `from` object at all.
    pub user_id: Option<String>,
    /// `from.username`, lower-cased and stripped of any leading `@`. `None` when
    /// the sender has no public @username (very common for real people).
    pub username: Option<String>,
    /// `from.is_bot`. Telegram sets this on every message; when the field is
    /// absent we treat the sender as human (`false`). The Fix #0 bot-loop guard
    /// keys on this: any inbound message whose sender is a bot — including our
    /// own four family bots when they run as group admins and thus see each
    /// other's posts — must NEVER be elected/routed/composed.
    pub is_bot: bool,
}

impl SenderIdentity {
    /// The display label for logs and the casa conversation feed: the @username
    /// when present, else the numeric user id, else the literal `"unknown"`.
    ///
    /// The load-bearing property is that a sender WITH an id but WITHOUT a
    /// username renders as the id — never `"unknown"` — so the observability log
    /// and the feed both carry a stable, resolvable handle.
    pub fn display(&self) -> String {
        self.username
            .clone()
            .or_else(|| self.user_id.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Read the `from` object of a raw Telegram message value into a
/// [`SenderIdentity`]. Shared by [`extract_sender`] and reusable directly on a
/// bare `message` object (as the decoder already holds one).
pub fn identity_from_message(message: &Value) -> SenderIdentity {
    let from = match message.get("from") {
        Some(f) => f,
        None => return SenderIdentity::default(),
    };
    identity_from_from(from)
}

/// Read a raw Telegram `from` object into a [`SenderIdentity`].
pub fn identity_from_from(from: &Value) -> SenderIdentity {
    let user_id = from
        .get("id")
        .and_then(|i| i.as_i64())
        .map(|i| i.to_string());
    let username = from
        .get("username")
        .and_then(|u| u.as_str())
        .map(|u| u.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|u| !u.is_empty());
    let is_bot = from
        .get("is_bot")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    SenderIdentity {
        user_id,
        username,
        is_bot,
    }
}

/// Extract the [`SenderIdentity`] from a raw `getUpdates` update element,
/// handling every shape the listener consumes: a plain/group/reply
/// `message`, and a `callback_query` (button press) whose sender lives at
/// `callback_query.from`. Returns [`SenderIdentity::default`] (all `None`,
/// `is_bot = false`) for an update with neither — the caller then treats it as
/// an anonymous/unknown sender exactly as before.
pub fn extract_sender(update: &Value) -> SenderIdentity {
    if let Some(cb) = update.get("callback_query") {
        if let Some(from) = cb.get("from") {
            return identity_from_from(from);
        }
    }
    if let Some(message) = update.get("message") {
        return identity_from_message(message);
    }
    SenderIdentity::default()
}

/// An inbound sender resolved against the binding map: the raw identity plus
/// the agency binding it maps to, if any. This is the exact composition the
/// listener boundary performs (`extract_sender` → `find_by_identity`) and the
/// value the `wg telegram resolve-sender` diagnostic prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInbound {
    /// The raw sender identity read from the update.
    pub identity: SenderIdentity,
    /// The agency agent id the sender is bound to (`None` when no binding
    /// claims this id/username — the old "unrecognized sender" case).
    pub agent_id: Option<String>,
    /// The bound human's display name, when resolved.
    pub name: Option<String>,
    /// Whether the resolved binding is confirmed (the `YES` handshake done).
    pub confirmed: bool,
}

/// Resolve a raw Telegram update to its bound identity through the SAME path the
/// listener uses: [`extract_sender`] then
/// [`TelegramBindingMap::find_by_identity`] (numeric id first, then @username).
///
/// This is the Fix #5 diagnostic core. Given a raw update carrying only
/// `from.id` (a real person with no public @username), it resolves to that
/// human's binding — the case that previously decoded to `"unknown"` and was
/// rejected. Pure: no filesystem, no network.
pub fn resolve_inbound(update: &Value, bindings: &TelegramBindingMap) -> ResolvedInbound {
    let identity = extract_sender(update);
    let hit = bindings.find_by_identity(identity.user_id.as_deref(), identity.username.as_deref());
    ResolvedInbound {
        agent_id: hit.map(|b| b.agent_id.clone()),
        name: hit.map(|b| b.name.clone()),
        confirmed: hit.map(|b| b.confirmed).unwrap_or(false),
        identity,
    }
}

/// A one-line, PII-conscious summary of [`resolve_inbound`] for the diagnostic
/// command's stdout. Prints the id, whether a username was present, the bot
/// flag, and the resolved agent/name — or `resolved=unrecognized` when no
/// binding matched, which is precisely the signal the live 20:19 failure needed.
pub fn resolve_inbound_summary(update: &Value, bindings: &TelegramBindingMap) -> String {
    let r = resolve_inbound(update, bindings);
    let resolved = match (&r.agent_id, &r.name) {
        (Some(agent), Some(name)) => {
            format!(
                "{agent}({name}){}",
                if r.confirmed { "" } else { " unconfirmed" }
            )
        }
        _ => "unrecognized".to_string(),
    };
    format!(
        "sender_id={} has_username={} is_bot={} resolved={}",
        r.identity.user_id.as_deref().unwrap_or("none"),
        r.identity.username.is_some(),
        r.identity.is_bot,
        resolved,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agency::human_binding::TelegramBinding;

    fn ts() -> chrono::DateTime<chrono::Utc> {
        "2026-07-11T20:19:00Z".parse().unwrap()
    }

    #[test]
    fn extracts_id_username_and_bot_flag_from_group_message() {
        let update = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 42,
                "from": { "id": 8905220378_i64, "is_bot": false, "username": "LucaPinello" },
                "chat": { "id": -100999, "type": "supergroup" },
                "text": "hi bruno"
            }
        });
        let id = extract_sender(&update);
        assert_eq!(id.user_id.as_deref(), Some("8905220378"));
        assert_eq!(id.username.as_deref(), Some("lucapinello")); // lower-cased
        assert!(!id.is_bot);
        assert_eq!(id.display(), "lucapinello");
    }

    #[test]
    fn sender_with_id_but_no_username_displays_as_id_not_unknown() {
        // The exact live failure: a real person with no public @username. Before
        // the fix this decoded to "unknown"; now it decodes to the numeric id.
        let update = serde_json::json!({
            "message": {
                "from": { "id": 8905220378_i64, "is_bot": false },
                "chat": { "id": 55501234, "type": "private" },
                "text": "bruno are you there?"
            }
        });
        let id = extract_sender(&update);
        assert_eq!(id.user_id.as_deref(), Some("8905220378"));
        assert!(id.username.is_none());
        assert_eq!(id.display(), "8905220378"); // NOT "unknown"
    }

    #[test]
    fn detects_bot_sender_for_loop_guard() {
        // A reply COMPOSED and SENT by one of our own bots, seen as inbound on
        // another bot's poller (the group-admin bots-see-bots case). is_bot must
        // be true so the Fix #0 guard suppresses it.
        let update = serde_json::json!({
            "message": {
                "from": { "id": 7777_i64, "is_bot": true, "username": "nora_casapinello_bot" },
                "chat": { "id": -100999, "type": "supergroup" },
                "text": "Hey everyone! All quiet on my end \u{1f44b}"
            }
        });
        let id = extract_sender(&update);
        assert!(id.is_bot, "bot-sent message must be flagged is_bot");
        assert_eq!(id.username.as_deref(), Some("nora_casapinello_bot"));
    }

    #[test]
    fn extracts_sender_from_callback_query() {
        let update = serde_json::json!({
            "callback_query": {
                "from": { "id": 8905220378_i64, "is_bot": false, "username": "luca" },
                "data": "approve:my-task",
                "message": { "message_id": 9, "chat": { "id": 111, "type": "private" } }
            }
        });
        let id = extract_sender(&update);
        assert_eq!(id.user_id.as_deref(), Some("8905220378"));
        assert_eq!(id.username.as_deref(), Some("luca"));
    }

    #[test]
    fn missing_from_yields_default_unknown() {
        let update = serde_json::json!({ "message": { "text": "no from here" } });
        let id = extract_sender(&update);
        assert!(id.user_id.is_none());
        assert!(id.username.is_none());
        assert!(!id.is_bot);
        assert_eq!(id.display(), "unknown");
    }

    #[test]
    fn absent_is_bot_defaults_to_human() {
        let from = serde_json::json!({ "id": 5, "username": "someone" });
        let id = identity_from_from(&from);
        assert!(!id.is_bot);
    }

    // ---- Fix #5 boundary test: raw update -> binding, end to end ----------

    #[test]
    fn raw_update_with_luca_from_id_resolves_to_human_luca() {
        // The 20:19:47 live failure, reproduced through the FULL boundary path
        // (extract_sender → find_by_identity): Luca's confirmed binding is keyed
        // by his numeric id 8905220378, and his update carries only `from.id`
        // (no public @username). Before the fix this decoded to "unknown" and
        // bruno rejected him; now it resolves to human-luca.
        let mut bindings = TelegramBindingMap::default();
        let mut b = TelegramBinding::new("8905220378", "human-luca", "Luca", None, ts());
        b.confirmed = true;
        bindings.add(b).unwrap();

        let update = serde_json::json!({
            "update_id": 100,
            "message": {
                "message_id": 7,
                "from": { "id": 8905220378_i64, "is_bot": false },
                "chat": { "id": 8905220378_i64, "type": "private" },
                "text": "bruno are you there?"
            }
        });

        let resolved = resolve_inbound(&update, &bindings);
        assert_eq!(resolved.agent_id.as_deref(), Some("human-luca"));
        assert_eq!(resolved.name.as_deref(), Some("Luca"));
        assert!(resolved.confirmed);

        let line = resolve_inbound_summary(&update, &bindings);
        assert_eq!(
            line,
            "sender_id=8905220378 has_username=false is_bot=false resolved=human-luca(Luca)"
        );
    }

    #[test]
    fn raw_update_from_unbound_sender_is_unrecognized() {
        let bindings = TelegramBindingMap::default();
        let update = serde_json::json!({
            "message": { "from": { "id": 999, "is_bot": false }, "text": "hi" }
        });
        let r = resolve_inbound(&update, &bindings);
        assert!(r.agent_id.is_none());
        assert!(resolve_inbound_summary(&update, &bindings).contains("resolved=unrecognized"));
    }

    #[test]
    fn raw_bot_update_resolves_is_bot_true() {
        // The diagnostic surfaces is_bot so an operator can see WHY a bot-sent
        // message produced no reply (the Fix #0 guard).
        let bindings = TelegramBindingMap::default();
        let update = serde_json::json!({
            "message": {
                "from": { "id": 7777, "is_bot": true, "username": "nora_casapinello_bot" },
                "text": "Hey everyone!"
            }
        });
        assert!(resolve_inbound_summary(&update, &bindings).contains("is_bot=true"));
    }
}
