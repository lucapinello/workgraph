//! Is an inbound message a command? — the pure gate the listener asks first.
//!
//! Extracted from `commands/telegram.rs` in slice 9 (see docs/UPSTREAM-DIVERGENCE.md). Pure and
//! unit-testable, and upstream's `run_listen` asks it, so their file imports it back.
//!
//! Moving it also makes a future `run_decide` slice cheaper: `command_gate` was one of the two
//! things that candidate needed, and only `try_confirm_binding` is left.

/// Whether an inbound listener message may fire a FAMILY command and/or the
/// OPERATOR command reference.
///
/// This is the single gate that closed `fix-command-leaks`: a bare `?` in the
/// group was parsed as an operator HELP command and dumped the raw WG
/// claim/done reference into the family chat, racing the mention election. The
/// three rules it encodes:
///
/// 1. A message is a command **only** when it opens with a genuine Telegram
///    slash command (`has_bot_command` — a `bot_command` entity at offset 0).
///    Punctuation, a bare `?`, or ordinary chatter is conversation, never a
///    command — so it can never race the election.
/// 2. Because the gate keys off the slash entity (not the text), an addressed
///    conversational turn like `@nora ?` carries no command entity → the
///    election owns it and the agent converses.
/// 3. The OPERATOR reference (claim/done/status/help) is coordinator content
///    that must NEVER surface in a family group — it runs only in a 1:1
///    operator DM, and only for a real slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandGate {
    /// A family-voice command (`/dinner`, `/help`, …) may run in this chat.
    pub family: bool,
    /// The operator WG command reference may run in this chat.
    pub operator: bool,
}

/// Decide the [`CommandGate`] for an inbound message. Pure and unit-testable
/// against real `decode_update` output.
pub(crate) fn command_gate(msg: &worksgood::notify::IncomingMessage) -> CommandGate {
    let is_group = matches!(msg.chat_type.as_deref(), Some("group") | Some("supergroup"));
    let is_command = msg.has_bot_command;
    CommandGate {
        family: is_command,
        operator: is_command && !is_group,
    }
}
