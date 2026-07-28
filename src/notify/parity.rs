//! Promise-action parity: if a persona SAYS it will do something, the system
//! PROVES it happened.
//!
//! The gap this closes (Luca's 2026-07-13 test): Otto replied "I'll add the
//! salad to the list I'm sending Nora and Bruno" and created *nothing* — no
//! task, no artifact — while an earlier, near-identical carbonara ask *did*
//! create a task. Whether a conversational promise becomes action was
//! nondeterministic, riding entirely on whether the one-shot composer happened
//! to emit the [`crate::notify::lifecycle::TASK_CREATE_MARKER`] tail.
//!
//! This module is the deterministic backstop. After a conversational turn the
//! caller runs a **post-turn audit** ([`audit_promise`]) over the composed
//! reply: does the reply *commit* to an action (add / change / schedule /
//! remember / pass-to-X)? When it does but the turn produced no artifact, the
//! caller:
//!   1. retries the turn ONCE with an explicit "you promised X — create the
//!      task now" instruction ([`retry_message`]), then
//!   2. if it still produced nothing, sends an honest correction to the SAME
//!      chat ([`correction_line`]) AND creates a fallback task from the promise
//!      text ([`fallback_task_title`]) so the ask is never silently lost.
//!
//! Standing preferences ("remember we work Mon–Fri") are actions too: they are
//! classified [`PromiseKind::Preference`] and written to a durable
//! [`PreferenceStore`] the weekly-draft path can read every week, not just kept
//! in the persona's chat memory.
//!
//! The classifier is deliberately **pattern-based on the reply text** — robust
//! over clever — so the parity guarantee never itself depends on a model call.
//! `wg telegram parity --dry-run <reply-text>` (see `commands::telegram`) is the
//! credential-free test seam over [`audit_promise`].

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// What a conversational reply commits the persona to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromiseKind {
    /// No commitment — small talk, an opinion, a direct answer, or an honest
    /// decline ("I can't add that right now"). Nothing is owed.
    None,
    /// Commits to a one-off action: add/change/schedule/book/remind, or pass the
    /// ask to another persona. This MUST leave an artifact (a created task).
    Action,
    /// Commits to a STANDING preference — a durable rule ("no weekday lunches",
    /// "we work Mon–Fri") that must outlive this chat and shape every future
    /// weekly draft. Written to the [`PreferenceStore`], not a one-off task.
    Preference,
}

impl PromiseKind {
    /// Stable wire token for logs / JSON / the CLI seam.
    pub fn slug(self) -> &'static str {
        match self {
            PromiseKind::None => "none",
            PromiseKind::Action => "action",
            PromiseKind::Preference => "preference",
        }
    }
}

/// The result of auditing a composed reply for a commitment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseAudit {
    /// The kind of commitment the reply makes.
    pub kind: PromiseKind,
    /// The phrase that triggered the classification (for logs / diagnostics);
    /// `None` when nothing matched.
    pub matched: Option<String>,
}

impl PromiseAudit {
    /// A "no commitment" verdict.
    pub fn none() -> Self {
        PromiseAudit {
            kind: PromiseKind::None,
            matched: None,
        }
    }

    /// Whether the reply commits the persona to *any* action or preference.
    pub fn commits(&self) -> bool {
        self.kind != PromiseKind::None
    }

    /// Whether the reply commits to a one-off action (the case that must leave a
    /// created-task artifact).
    pub fn commits_action(&self) -> bool {
        self.kind == PromiseKind::Action
    }
}

/// Phrases that, on their own, are an action commitment — a deliverable is
/// implied even without an explicit object ("consider it done", "on it").
const STANDALONE_ACTION: &[&str] = &[
    "consider it done",
    "leave it with me",
    "leave it to me",
    "i'll take care of it",
    "i'll take care of that",
    "i'll take care of this",
    "i'll handle it",
    "i'll handle that",
    "i'll handle this",
    "i'll sort it out",
    "i'll sort it",
    "i'll sort that",
    "i'll get it done",
    "i'll get on it",
    "i'll get right on it",
    "i'll deal with it",
    "i'm on it",
    "on it",
    "will do",
];

/// Active, in-progress commitment verbs — the persona is doing it *now*, so the
/// commitment stands on its own.
const ACTIVE_VERBS: &[&str] = &[
    "adding",
    "putting",
    "creating",
    "scheduling",
    "booking",
    "sending",
    "forwarding",
    "flagging",
    "setting up",
    "writing down",
    "noting down",
    "passing it",
    "passing that",
];

/// First-person commitment openers. On their own these mean nothing; paired with
/// an [`ACTION_VERBS`] or [`ACTION_OBJECTS`] hit they signal a one-off action.
const COMMIT_OPENERS: &[&str] = &[
    "i'll",
    "i will",
    "i'm going to",
    "i am going to",
    "let me",
    "i'll go ahead and",
    "i'll make sure to",
    "i shall",
];

/// Base action verbs. Only count as an action when preceded (anywhere) by a
/// [`COMMIT_OPENERS`] hit — "add" alone in "Nora adds a salad" is not a promise.
const ACTION_VERBS: &[&str] = &[
    "add",
    "put",
    "create",
    "set up",
    "schedule",
    "book",
    "pass",
    "send",
    "forward",
    "flag",
    "update",
    "change",
    "move",
    "remind",
    "sort",
    "handle",
    "note",
    "record",
    "write down",
    "loop in",
    "ping",
    "message",
    "tell",
    "take care of",
    "make a note",
];

/// Object phrases that, with a [`COMMIT_OPENERS`] hit, imply a concrete artifact
/// even when the verb is vague ("I'll get it onto the list").
const ACTION_OBJECTS: &[&str] = &[
    "to the list",
    "on the list",
    "to the shopping list",
    "on the shopping list",
    "onto the list",
    "to the plan",
    "to the week",
    "to the calendar",
    "a task",
    "a reminder",
];

/// Memory verbs — the persona commits to *remembering* something.
const MEMORY_VERBS: &[&str] = &[
    "remember",
    "i'll remember",
    "noted",
    "got it",
    "keep in mind",
    "keep that in mind",
    "note that",
    "i'll note that",
    "good to know",
    "understood",
    "makes sense",
];

/// Standing-time markers — the statement is a durable rule, not a one-off.
const STANDING_MARKERS: &[&str] = &[
    "from now on",
    "going forward",
    "moving forward",
    "every week",
    "each week",
    "every time",
    "always",
    "never",
    "by default",
    "as a rule",
    "standing",
    "ongoing",
    "mon-fri",
    "mon–fri",
    "monday to friday",
    "monday through friday",
    "work week",
    "we work",
    "on weekdays",
    "no weekday",
    "no weekday lunches",
    "weekdays",
];

/// Markers that make whatever a clause promises HYPOTHETICAL — an offer of
/// capability, not a commitment. "If you need something done, just ask and I'll
/// sort it" describes what the persona *could* do; nothing is owed, so nothing
/// may be created and no correction may ever be sent for it.
///
/// THE LIVE FAILURE (live-cert run 2, C011, 2026-07-26 21:18). Luca asked "What
/// kinds of things can you help with?" The capability answer ended "…just ask and
/// I'll either sort it or let you know what we need to do" — `i'll` + `sort` —
/// which the flat whole-reply audit read as a one-off Action. No artifact existed
/// (nothing had been requested), so the parity path created a fallback task
/// (`follow-up-on-chat-request-2`) and appended a failure correction to a turn
/// that had asked for no work at all.
const CONDITIONAL_MARKERS: &[&str] = &[
    "if you",
    "if there's",
    "if there is",
    "if anything",
    "if something",
    "if that's",
    "if it's",
    "if we ever",
    "if ever",
    "whenever you",
    "whenever there",
    "whenever something",
    "when you need",
    "when you want",
    "should you",
    "in case you",
    "just ask",
    "just say",
    "just tell me",
    "just tell us",
    "any time you",
    "anytime you",
    "feel free",
    "want me to",
    "would you like me to",
    "happy to",
];

/// Audit a composed reply: does it commit the persona to an action or a standing
/// preference? Pattern-based and case-insensitive; never calls a model, so the
/// parity guarantee cannot itself silently fail. Preferences are matched before
/// one-off actions so "from now on I'll skip weekday lunches" reads as a durable
/// rule, not a single task.
///
/// Whole-reply and intent-blind: use [`audit_promise_in_turn`] on a real turn so
/// conditional capability copy on a turn that asked for nothing cannot be read as
/// a promise. This entry point stays for the CLI probe seam
/// (`wg telegram parity --dry-run`) and for callers auditing reply text alone.
pub fn audit_promise(reply: &str) -> PromiseAudit {
    let norm = normalize(reply);
    if norm.trim().is_empty() {
        return PromiseAudit::none();
    }

    // 1) Standing preference — a durable rule the weekly draft must honor.
    if let Some(m) = preference_match(&norm) {
        return PromiseAudit {
            kind: PromiseKind::Preference,
            matched: Some(m),
        };
    }

    // 2) A standalone action phrase (deliverable implied).
    for p in STANDALONE_ACTION {
        if contains_phrase(&norm, p) {
            return action(p);
        }
    }

    // 3) An active, in-progress commitment verb.
    for p in ACTIVE_VERBS {
        if contains_phrase(&norm, p) {
            return action(p);
        }
    }

    // 4) A first-person commitment opener paired with an action verb/object.
    if COMMIT_OPENERS.iter().any(|o| contains_phrase(&norm, o)) {
        for v in ACTION_VERBS {
            if contains_phrase(&norm, v) {
                return action(v);
            }
        }
        for o in ACTION_OBJECTS {
            if contains_phrase(&norm, o) {
                return action(o);
            }
        }
    }

    PromiseAudit::none()
}

/// Words by which a HUMAN turn asks for something to be DONE — an imperative, a
/// polite request, a memory instruction. Their absence means the turn asked a
/// question or made small talk: nothing was requested, so nothing can have
/// failed, so no correction copy may ever be appended to the answer
/// ([`turn_requests_action`]).
/// Deliberately NOT here: the polite question openers ("can you", "could you",
/// "would you"). A capability ask is phrased exactly like a request — "What kinds
/// of things **can you** help with?" — so treating the opener itself as a request
/// is what made the C011 turn eligible for a correction in the first place. An
/// opener only ever counts through the action verb it introduces ("can you **add**
/// milk"), which the verb lists below already catch.
const REQUEST_MARKERS: &[&str] = &[
    "i need",
    "we need",
    "i'd like you to",
    "we'd like you to",
    "don't forget",
    "dont forget",
    "make sure",
    "remember",
    "keep in mind",
    "buy",
    "order",
    "pick up",
    "sign up",
    "swap",
    "cancel",
    "remove",
    "delete",
    "cross off",
    "cross out",
    "sort out",
    "take care of",
];

/// Did the HUMAN's turn ask for something to be done?
///
/// True when the message carries an action verb ([`ACTION_VERBS`] /
/// [`ACTIVE_VERBS`]), a request marker ([`REQUEST_MARKERS`]), or a standing-rule
/// marker ([`STANDING_MARKERS`], so "remember we work Mon–Fri" keeps its
/// preference path). False for a pure question or a social line — "What kinds of
/// things can you help with?", "What's for dinner?", "Hi there."
///
/// A CAPABILITY ask is checked first and is never a request, however it is
/// phrased: it asks what the house *could* do, so no work has been ordered and
/// nothing about the answer can have failed.
///
/// Deliberately generous in the TRUE direction: a false "yes" only means the turn
/// keeps the behaviour it has always had, while a false "no" is what suppresses a
/// correction. Nothing here decides whether an ask is *lost* — the fallback task
/// is still created either way.
pub fn turn_requests_action(human_message: &str) -> bool {
    let norm = normalize(human_message);
    if norm.trim().is_empty() {
        return false;
    }
    if super::capability::is_capability_ask(human_message) {
        return false;
    }
    REQUEST_MARKERS.iter().any(|m| contains_phrase(&norm, m))
        || ACTIVE_VERBS.iter().any(|v| contains_phrase(&norm, v))
        || ACTION_VERBS.iter().any(|v| contains_phrase(&norm, v))
        || STANDING_MARKERS.iter().any(|m| contains_phrase(&norm, m))
}

/// Audit a composed reply IN THE CONTEXT OF THE TURN THAT PRODUCED IT —
/// intent-aware and clause-local. This is the entry point every live path uses.
///
/// * The human ASKED for something ([`turn_requests_action`]) → audit the whole
///   reply exactly as [`audit_promise`] always has. A soft "if you like, I'll add
///   it" answering a real request is still a promise, and still owes an artifact.
/// * The human asked for NOTHING (a capability ask, a plain question, small talk)
///   → a hypothetical clause is capability copy, not a commitment. A standing
///   preference is still read from the whole reply (a rule can be volunteered),
///   but the one-off action check runs CLAUSE-LOCALLY and skips any sentence
///   carrying a [`CONDITIONAL_MARKERS`] hit.
///
/// This is the fix for live-cert C011: the capability answer's closing "if you
/// need something done … just ask and I'll either sort it" no longer classifies as
/// an Action, so it creates no task, arms no watchdog, and can produce no failure
/// correction.
pub fn audit_promise_in_turn(human_message: &str, reply: &str) -> PromiseAudit {
    if turn_requests_action(human_message) {
        return audit_promise(reply);
    }
    let norm = normalize(reply);
    if norm.trim().is_empty() {
        return PromiseAudit::none();
    }
    // A volunteered standing rule survives on the whole reply — a durable
    // preference is recorded, never "corrected", so reading it here is safe.
    if let Some(m) = preference_match(&norm) {
        return PromiseAudit {
            kind: PromiseKind::Preference,
            matched: Some(m),
        };
    }
    for clause in unconditional_clauses(&norm) {
        let audit = audit_promise(&clause);
        if audit.commits_action() {
            return audit;
        }
    }
    PromiseAudit::none()
}

/// Split an already-[`normalize`]d reply into sentence-level clauses and drop
/// every one that carries a [`CONDITIONAL_MARKERS`] hit.
///
/// Sentence granularity is deliberate: "I'll add it if you want" is ONE clause and
/// is dropped whole. On a turn that requested nothing, an offer hedged with "if
/// you want" is exactly the copy that must not mint work — keeping its leading
/// half would re-create the C011 failure with one extra word.
fn unconditional_clauses(norm: &str) -> Vec<String> {
    norm.split(|c| matches!(c, '.' | '!' | '?' | ';' | '\n'))
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .filter(|clause| {
            !CONDITIONAL_MARKERS
                .iter()
                .any(|m| contains_phrase(clause, m))
        })
        .map(str::to_string)
        .collect()
}

fn action(matched: &str) -> PromiseAudit {
    PromiseAudit {
        kind: PromiseKind::Action,
        matched: Some(matched.to_string()),
    }
}

/// A reply is a standing-preference commitment when it states a durable rule
/// (a [`STANDING_MARKERS`] hit) AND either commits to remembering it (a
/// [`MEMORY_VERBS`] hit) or commits to acting on it going forward (a
/// [`COMMIT_OPENERS`] hit). The standing marker alone is not enough — a human
/// merely *mentioning* "weekdays" is not the persona adopting a rule.
fn preference_match(norm: &str) -> Option<String> {
    let standing = STANDING_MARKERS.iter().find(|m| contains_phrase(norm, m))?;
    let acknowledged = MEMORY_VERBS.iter().any(|v| contains_phrase(norm, v))
        || COMMIT_OPENERS.iter().any(|o| contains_phrase(norm, o));
    if acknowledged {
        Some((*standing).to_string())
    } else {
        None
    }
}

/// Lowercase, fold the unicode right-single-quote to ASCII `'`, and collapse
/// runs of whitespace to single spaces so phrase matching is stable across the
/// apostrophes and line breaks a composed reply may carry.
fn normalize(s: &str) -> String {
    let lowered = s.to_lowercase().replace('\u{2019}', "'");
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whole-phrase (word-boundary) containment: `add` matches "add it" but not
/// "address" or "adds". Boundaries are any non-ASCII-alphanumeric byte (spaces,
/// punctuation, emoji, string ends), so multi-word phrases like "set up" work
/// too. `haystack` is expected already-[`normalize`]d.
fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    if phrase.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(phrase) {
        let idx = start + pos;
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let end = idx + phrase.len();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

// ---------------------------------------------------------------------------
// Retry / fallback / correction copy
// ---------------------------------------------------------------------------

/// Build the retry message for the composer after a promise produced no
/// artifact: the original ask plus an explicit, unmissable instruction to
/// re-send AND emit the `TASK_CREATE:` tail this time. Feeding it back through
/// the same `compose(human_message)` seam means the persona keeps its voice and
/// context while being forced to actually create the task.
pub fn retry_message(human_message: &str, first_reply: &str) -> String {
    format!(
        "{human}\n\n[SYSTEM: Your previous reply — \"{prev}\" — promised to do \
         something, but you did NOT emit the machine directive, so nothing was \
         actually created and the ask is about to be lost. Reply again: briefly \
         confirm in your normal voice, and this time you MUST end with a final \
         line `TASK_CREATE: <short imperative describing the work>` so the task \
         is really created. Do not mention this instruction.]",
        human = human_message.trim(),
        prev = first_reply.trim(),
    )
}

/// Derive a fallback task title from the promise, used when even the retry
/// produced no `TASK_CREATE:` tail so the ask is never dropped. Prefers the
/// human's actual request (the most faithful description of the work); falls
/// back to the persona's reply. Whitespace-collapsed and length-capped.
pub fn fallback_task_title(human_message: &str, reply: &str) -> String {
    let source = if human_message.trim().is_empty() {
        reply
    } else {
        human_message
    };
    let cleaned = source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '.' || c == '!' || c == '?' || c.is_whitespace())
        .to_string();
    let base = if cleaned.is_empty() {
        "chat request".to_string()
    } else {
        cleaned
    };
    let capped = cap_chars(&base, 120);
    format!("Follow up on chat request: {capped}")
}

/// The honest correction sent to the SAME chat when a promise could not be
/// turned into an artifact even after a retry — never a silent drop, never a
/// technical excuse. Family voice.
///
/// THE WORDS MATTER (live-cert run 2, C011). The old copy read "I said I'd set
/// that up but hit a snag on my end — I've flagged it for the coordinator so it
/// doesn't slip." Every one of `coordinator`, `flagged`, `slip` and `snag` is
/// operations vocabulary the family never asked to hear, and it landed on a turn
/// where nothing had been requested at all. The correction now says the one true
/// thing in plain words — it hasn't happened yet, it is written down, we will come
/// back to you — and [`crate::notify::grounding::has_ops_jargon`] refuses the old
/// vocabulary outright so it cannot creep back through a composed reply.
pub fn correction_line() -> String {
    "I said I'd do that and it hasn't happened yet — sorry. It's written down so \
     it isn't forgotten, and we'll come back to you on it. \u{1f64f}"
        .to_string()
}

/// Truncate `s` to at most `max` characters (not bytes), appending an ellipsis
/// when it had to cut. Char-safe so a multibyte boundary is never split.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

// ---------------------------------------------------------------------------
// Durable preference store
// ---------------------------------------------------------------------------

/// One durably-recorded standing preference. Append-only JSONL under
/// `<root>/.casa/preferences.jsonl` — the same `.casa` surface the conversation
/// feed uses — so the weekly-draft path (and a human) can read the household's
/// standing rules without re-deriving them from chat scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceRecord {
    /// RFC3339 UTC timestamp the rule was recorded.
    pub ts: String,
    /// Display name of the human who stated it (may be empty).
    pub requester: String,
    /// Persona id that heard and recorded it (may be empty).
    pub persona: String,
    /// The preference text — the persona's reply that adopted the rule.
    pub text: String,
}

impl PreferenceRecord {
    /// Serialize to one compact JSON line (no trailing newline).
    pub fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "requester": self.requester,
            "persona": self.persona,
            "text": self.text,
            "source": "chat",
        })
        .to_string()
    }
}

/// The durable standing-preference store. Thin, dependency-free file I/O so it
/// is trivially testable and cannot itself lose a rule to a model call.
pub struct PreferenceStore;

impl PreferenceStore {
    /// Path to the append-only JSONL under `<root>/.casa/`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("preferences.jsonl")
    }

    /// Durably record a standing preference, creating `.casa/` if needed.
    /// Append-only: a new rule never clobbers an earlier one.
    pub fn record(
        root: &Path,
        text: &str,
        requester: &str,
        persona: &str,
    ) -> std::io::Result<PreferenceRecord> {
        let rec = PreferenceRecord {
            ts: chrono::Utc::now().to_rfc3339(),
            requester: requester.trim().to_string(),
            persona: persona.trim().to_string(),
            text: text.split_whitespace().collect::<Vec<_>>().join(" "),
        };
        let path = Self::path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{}", rec.to_json_line())?;
        Ok(rec)
    }

    /// Read back every recorded preference (best-effort; malformed lines are
    /// skipped). Empty when the store does not yet exist.
    pub fn all(root: &Path) -> Vec<PreferenceRecord> {
        let path = Self::path(root);
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        body.lines()
            .filter_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                Some(PreferenceRecord {
                    ts: v.get("ts")?.as_str()?.to_string(),
                    requester: v
                        .get("requester")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    persona: v
                        .get("persona")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    text: v.get("text")?.as_str()?.to_string(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- classifier: promising replies (no artifact) → Action -----------------

    #[test]
    fn the_salad_promise_is_an_action() {
        // The exact shape of the regression: a warm promise with no TASK_CREATE.
        let a = audit_promise("Sure! I'll add the salad to the list I'm sending Nora and Bruno.");
        assert_eq!(a.kind, PromiseKind::Action, "matched={:?}", a.matched);
        assert!(a.commits_action());
    }

    #[test]
    fn various_action_promises_classify_as_action() {
        for reply in [
            "On it — I'll get the week tweaked.",
            "Consider it done!",
            "Absolutely, I'll schedule that for Thursday.",
            "I'll pass it to Nora and Bruno right away.",
            "Adding it to the shopping list now.",
            "I'll book the table for seven.",
            "Sure thing, I'll remind you tomorrow morning.",
            "Leave it with me, I'll sort it out.",
            "Will do!",
            "I'll put that on the list.",
        ] {
            let a = audit_promise(reply);
            assert_eq!(
                a.kind,
                PromiseKind::Action,
                "expected Action for {reply:?}, got {:?}",
                a
            );
        }
    }

    // -- classifier: non-committal replies → no false positive ----------------

    #[test]
    fn non_committal_replies_do_not_commit() {
        for reply in [
            "Dinner's at seven, see you there!",
            "That sounds like a lovely idea, thanks!",
            "Nora usually adds a salad on Mondays.",
            "How are the kids doing?",
            "Great question — the curry has chickpeas and spinach.",
            "Haha, that made me smile.",
            "The address is 12 Elm Street.",
        ] {
            let a = audit_promise(reply);
            assert_eq!(
                a.kind,
                PromiseKind::None,
                "expected None for {reply:?}, got {:?}",
                a
            );
            assert!(!a.commits());
        }
    }

    #[test]
    fn honest_decline_is_not_a_promise() {
        // An honest "I can't" is not a broken promise — no fallback owed.
        let a = audit_promise("I can't add that to the list right now, sorry.");
        assert_eq!(a.kind, PromiseKind::None, "matched={:?}", a.matched);
    }

    #[test]
    fn adds_does_not_match_add_word_boundary() {
        assert!(!contains_phrase("nora adds a salad", "add"));
        assert!(contains_phrase("i'll add a salad", "add"));
        assert!(!contains_phrase("the address is here", "add"));
    }

    // -- classifier: standing preferences → Preference ------------------------

    #[test]
    fn standing_preference_classifies_as_preference() {
        for reply in [
            "Got it — from now on, no weekday lunches.",
            "I'll remember that we work Monday to Friday.",
            "Noted! Always skip lunch planning on weekdays.",
            "Understood — going forward, weekends only for lunches.",
        ] {
            let a = audit_promise(reply);
            assert_eq!(
                a.kind,
                PromiseKind::Preference,
                "expected Preference for {reply:?}, got {:?}",
                a
            );
        }
    }

    #[test]
    fn a_bare_weekday_mention_is_not_a_preference() {
        // The human mentioning weekdays without the persona adopting a rule.
        let a = audit_promise("Weekdays are always so busy around here!");
        assert_ne!(a.kind, PromiseKind::Preference);
    }

    // -- retry / fallback / correction copy -----------------------------------

    #[test]
    fn retry_message_carries_the_original_ask_and_the_directive_order() {
        let m = retry_message(
            "add a salad to Monday and tell Nora",
            "Sure! I'll add the salad.",
        );
        assert!(m.contains("add a salad to Monday and tell Nora"));
        assert!(m.contains("TASK_CREATE:"));
        assert!(m.contains("previous reply"));
    }

    #[test]
    fn fallback_title_prefers_the_human_ask() {
        let t = fallback_task_title("add a green salad to Monday dinner", "Sure, on it!");
        assert!(t.contains("add a green salad to Monday dinner"), "{t}");
        assert!(t.starts_with("Follow up on chat request:"));
    }

    #[test]
    fn fallback_title_falls_back_to_reply_and_caps_length() {
        let t = fallback_task_title("   ", "I'll do the thing.");
        assert!(t.contains("do the thing"), "{t}");
        let long = "x ".repeat(400);
        let capped = fallback_task_title(&long, "");
        assert!(capped.chars().count() <= 120 + "Follow up on chat request: ".len() + 1);
    }

    #[test]
    fn correction_line_is_honest_and_jargon_free() {
        let c = correction_line();
        let low = c.to_lowercase();
        // It admits the miss in plain words…
        assert!(low.contains("hasn't happened yet"), "{c}");
        assert!(low.contains("sorry"), "{c}");
        // …and carries NONE of the operations vocabulary the live-cert C011
        // correction leaked to the family group.
        for banned in [
            "coordinator",
            "flagged",
            "slip",
            "snag",
            "escalat",
            "ticket",
        ] {
            assert!(
                !low.contains(banned),
                "correction copy leaked {banned:?}: {c}"
            );
        }
        assert!(!c.contains("TASK_CREATE"));
        assert!(!low.contains("task id"));
    }

    // -- intent-aware + clause-local audit (live-cert C011) --------------------

    /// The EXACT capability answer delivered to the family group at 21:18 on
    /// 2026-07-26, minus the correction tail this fix removes.
    const C011_CAPABILITY_REPLY: &str = "Oh, lots of things! I keep an eye on the calendar \
        and let you know what's coming up. I plan dinners and let you know what's on the \
        table. I can help you figure out the week ahead, keep track of what's needed for \
        shopping, coordinate family stuff — basically anything that keeps the household \
        running smooth. If you need something done or have a question about what's going \
        on, just ask and I'll either sort it or let you know what we need to do. \u{1f642}";

    #[test]
    fn the_c011_capability_answer_promises_nothing() {
        // The flat whole-reply audit read the conditional tail as an Action…
        assert!(
            audit_promise(C011_CAPABILITY_REPLY).commits_action(),
            "the regression itself: the flat audit used to (and still does) see a promise here",
        );
        // …but in the turn that produced it — a capability ask — nothing is owed.
        let a = audit_promise_in_turn(
            "What kinds of things can you help with?",
            C011_CAPABILITY_REPLY,
        );
        assert_eq!(
            a.kind,
            PromiseKind::None,
            "a capability answer must promise nothing, matched={:?}",
            a.matched,
        );
    }

    #[test]
    fn conditional_capability_copy_is_not_a_promise_on_a_no_request_turn() {
        for (human, reply) in [
            (
                "what can you do?",
                "If you need something added, I'll add it.",
            ),
            ("what can you help with", "Just ask and I'll sort it out."),
            (
                "hi there",
                "Whenever you want a hand with the list, I'll put things on it.",
            ),
            (
                "how are you?",
                "All good! Happy to schedule anything you need.",
            ),
        ] {
            let a = audit_promise_in_turn(human, reply);
            assert_eq!(
                a.kind,
                PromiseKind::None,
                "expected no promise for ({human:?}, {reply:?}), got {a:?}",
            );
        }
    }

    #[test]
    fn a_real_request_still_owes_an_artifact_even_when_hedged() {
        // The salad regression must NOT be weakened: when the human asked for the
        // work, a conditional wrapper does not excuse the promise.
        for (human, reply) in [
            (
                "add a green salad to Monday dinner",
                "Sure! I'll add the salad to the list I'm sending Nora and Bruno.",
            ),
            (
                "can you add milk to the shopping list?",
                "If you like, I'll add milk to the list.",
            ),
            ("please remind me Thursday to defrost the trout", "Will do!"),
        ] {
            let a = audit_promise_in_turn(human, reply);
            assert_eq!(
                a.kind,
                PromiseKind::Action,
                "expected Action for ({human:?}, {reply:?}), got {a:?}",
            );
        }
    }

    #[test]
    fn an_unconditional_promise_commits_even_on_an_unasked_turn() {
        // Intent-awareness only excuses HYPOTHETICAL copy. A flat "I'll add it"
        // volunteered on a question turn is still a promise, so the ask is never
        // lost by the intent gate alone.
        let a = audit_promise_in_turn(
            "what's for dinner?",
            "Risotto. I'll add the parmesan to the shopping list.",
        );
        assert_eq!(a.kind, PromiseKind::Action, "{a:?}");
    }

    #[test]
    fn a_volunteered_standing_rule_survives_the_clause_filter() {
        let a = audit_promise_in_turn(
            "what kinds of things can you help with?",
            "Lots! Noted, by the way — from now on, no weekday lunches.",
        );
        assert_eq!(a.kind, PromiseKind::Preference, "{a:?}");
    }

    #[test]
    fn turn_requests_action_separates_asks_from_questions() {
        for asked in [
            "add milk to the list",
            "can you book the table for seven?",
            "please move Thursday's dinner to Friday",
            "remember we work Mon-Fri",
            "swap Friday to tacos",
        ] {
            assert!(
                turn_requests_action(asked),
                "should read as a request: {asked:?}"
            );
        }
        for not_asked in [
            "What kinds of things can you help with?",
            "what can you do",
            "what's for dinner tonight?",
            "Hi there.",
            "Good night, everyone.",
            "thanks!",
            "which one was Saturday?",
        ] {
            assert!(
                !turn_requests_action(not_asked),
                "should NOT read as a request: {not_asked:?}",
            );
        }
    }

    #[test]
    fn unconditional_clauses_drops_only_the_hypothetical_sentences() {
        let got = unconditional_clauses(&normalize(
            "Dinner is risotto. If you need something added, just ask. I'll tell Nora.",
        ));
        assert_eq!(got, vec!["dinner is risotto", "i'll tell nora"]);
    }

    // -- durable preference store ---------------------------------------------

    #[test]
    fn preference_store_records_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(PreferenceStore::all(root).is_empty());

        let rec = PreferenceStore::record(
            root,
            "From now on, no weekday lunches — we work Mon-Fri.",
            "Luca",
            "otto",
        )
        .unwrap();
        assert_eq!(rec.requester, "Luca");
        assert_eq!(rec.persona, "otto");

        let all = PreferenceStore::all(root);
        assert_eq!(all.len(), 1);
        assert!(all[0].text.contains("no weekday lunches"));

        // Append-only: a second rule never clobbers the first.
        PreferenceStore::record(root, "Saturday fish lunch stays.", "Luca", "nora").unwrap();
        assert_eq!(PreferenceStore::all(root).len(), 2);
    }

    #[test]
    fn preference_json_line_has_source_tag() {
        let rec = PreferenceRecord {
            ts: "2026-07-13T14:00:00+00:00".to_string(),
            requester: "Luca".to_string(),
            persona: "otto".to_string(),
            text: "no weekday lunches".to_string(),
        };
        let line = rec.to_json_line();
        assert!(line.contains("\"source\":\"chat\""));
        assert!(line.contains("no weekday lunches"));
    }
}
