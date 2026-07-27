//! "What can you help with?" — the CAPABILITY act, answered instantly from the
//! household's own configuration (task `capability-answer-no-invented-work`).
//!
//! THE LIVE FAILURE (live-cert run 2, C011; 2026-07-26 21:18, family group). Luca
//! asked "What kinds of things can you help with?" It took **22.6 seconds** through
//! the full compose pipeline, and the delivered answer ended:
//!
//! > "I said I'd set that up but hit a snag on my end — I've flagged it for the
//! > coordinator so it doesn't slip."
//!
//! Nothing had been requested, nothing had failed, and no such setup existed. The
//! turn ALSO minted a task (`follow-up-on-chat-request-2`) for a question that
//! needs no work at all, and a human ended up answering the actual question by
//! hand. The promise half of that is fixed in [`super::parity`]; this module is
//! the other half: a capability ask is a SOCIAL act with a deterministic answer,
//! so it is answered here — instantly, model-free, creating nothing.
//!
//! RENAME-SAFE BY CONSTRUCTION. The answer is assembled from `household.toml`:
//! every named helper and every lane comes from the configured
//! [`super::ownership::OwnerMap`] (`[[agent]]` `id` / `name` / `domains`), so a
//! household that renames its cast, or splits the lanes differently, gets its own
//! answer with no code change. Nothing about a shipped id or a shipped persona
//! name appears in this file. When the household declares NO domain ownership at
//! all, [`capability_answer`] returns `None` and the turn keeps its old path —
//! better a slow real answer than a confident invented one.
//!
//! The gateway carries the same act for the kiosk/web and 1:1 surfaces
//! (`claw3d-bridge/src/socialResponder.mjs` — `detectCapabilityAsk` /
//! `composeCapabilityReply`); the two twins are kept phrase-compatible on purpose
//! so a family sees one voice whichever surface they ask on.

use super::ownership::{Domain, OwnerMap};

/// Longer than this and the message is a real request that happens to contain a
/// capability phrase, not a bare "what can you do?" — same reasoning as the
/// greeting lane's length cap.
/// (Same value as the gateway twin's `MAX_CAPABILITY_LEN`, so the two surfaces
/// admit the same messages.)
const MAX_CAPABILITY_LEN: usize = 64;

/// The capability question, in the shapes families actually type. Matched as
/// whole phrases over a normalized message.
const CAPABILITY_ASKS: &[&str] = &[
    "what can you do",
    "what can you all do",
    "what can you guys do",
    "what can you help",
    "what can you help with",
    "what can you help me with",
    "what can you help us with",
    "what can you handle",
    "what can you take care of",
    "what can you sort out for us",
    "what else can you do",
    "what else can you help with",
    "what do you do",
    "what do you all do",
    "what do you help with",
    "what are you able to do",
    "what are you able to help with",
    "what are you good at",
    "what are your skills",
    "what things can you do",
    "what all can you do",
    "what kinds of things can you",
    "what kind of things can you",
    "what kinds of stuff can you",
    "what kind of stuff can you",
    "what sorts of things can you",
    "what sort of things can you",
    "what sort of stuff can you",
    "how can you help",
    "how can you all help",
    "how do you help",
    "what help can you give",
    "tell me what you can do",
    "tell us what you can do",
    "remind me what you can do",
];

/// Domain content that means the ask is about ONE lane, not about the house's
/// range: "what can you do about Thursday's dinner?" is a meal ask and must keep
/// its owner and its real answer.
const DOMAIN_NOUNS: &[&str] = &[
    "dinner",
    "dinners",
    "lunch",
    "lunches",
    "breakfast",
    "meal",
    "meals",
    "recipe",
    "recipes",
    "menu",
    "shopping",
    "groceries",
    "list",
    "calendar",
    "schedule",
    "appointment",
    "appointments",
    "workout",
    "workouts",
    "training",
    "gym",
    "reminder",
    "reminders",
    "plan",
    "week",
    "weekend",
    "today",
    "tonight",
    "tomorrow",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// Is this turn a bare CAPABILITY ask — "what kinds of things can you help
/// with?", "what can you do?", "how can you help?"
///
/// Conservative in both directions: it needs one of the [`CAPABILITY_ASKS`]
/// phrases, it bails on any [`DOMAIN_NOUNS`] hit (a lane-specific ask keeps its
/// owner), and it bails on a long message (a real request that merely quotes the
/// phrase).
pub fn is_capability_ask(text: &str) -> bool {
    let norm = normalize(text);
    if norm.is_empty() || norm.chars().count() > MAX_CAPABILITY_LEN {
        return false;
    }
    if DOMAIN_NOUNS.iter().any(|n| contains_phrase(&norm, n)) {
        return false;
    }
    CAPABILITY_ASKS.iter().any(|p| contains_phrase(&norm, p))
}

/// One lane of the house, as the family would name it: the configured domain and
/// the plain words the answer uses for it.
/// Phrases are kept free of inner em dashes and of a trailing "and …" so that
/// grouping two lanes under one helper still reads like a sentence a person wrote
/// ("Tally has the calendar and the shopping list").
const LANES: &[(Domain, &str)] = &[
    (Domain::Cooking, "the meals and what's for dinner"),
    (Domain::Calendar, "the calendar"),
    (Domain::Shopping, "the shopping list"),
    (Domain::Workouts, "workouts"),
];

/// Compose the capability answer from the CONFIGURED household.
///
/// Every lane the household declares an owner for is named with that helper's
/// authored display name (falling back to the configured id — never an invented
/// name), grouped so a helper who owns two lanes is named once. Lanes nobody
/// declares are still named, unattributed, so the answer stays complete without
/// crediting work to a helper who was never given it.
///
/// Returns `None` when the household declares no ownership at all: with nothing
/// configured there is nothing honest to say, so the caller falls through to its
/// normal path instead of answering from a compiled-in cast.
pub fn capability_answer(owner_map: &OwnerMap) -> Option<String> {
    // (owner id, display label, lanes) in lane order, grouped by owner.
    let mut groups: Vec<(String, String, Vec<&str>)> = Vec::new();
    let mut unowned: Vec<&str> = Vec::new();
    for (domain, lane) in LANES {
        match owner_map.owner_for_domain(*domain) {
            Some(owner) => {
                let label = display_label(owner_map, owner);
                match groups.iter_mut().find(|(id, _, _)| id == owner) {
                    Some((_, _, lanes)) => lanes.push(lane),
                    None => groups.push((owner.to_string(), label, vec![lane])),
                }
            }
            None => unowned.push(lane),
        }
    }
    if groups.is_empty() {
        return None;
    }

    let mut out = String::from("Quite a lot of the everyday stuff. ");
    for (_, label, lanes) in &groups {
        out.push_str(label);
        out.push_str(" has ");
        out.push_str(&join_lanes(lanes));
        out.push_str(". ");
    }
    if !unowned.is_empty() {
        out.push_str("You can ask about ");
        out.push_str(&join_lanes(&unowned));
        out.push_str(" too. ");
    }
    out.push_str("Ask in plain words — a normal message is enough. \u{1f642}");
    Some(out)
}

/// The household-authored display name for a persona id, or the id itself when the
/// household authored no name. Never invents one.
fn display_label(owner_map: &OwnerMap, owner: &str) -> String {
    owner_map
        .display_names()
        .find(|(id, _)| id.eq_ignore_ascii_case(owner))
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| owner.to_string())
}

/// "a", "a and b", "a, b and c" — plain family list punctuation.
fn join_lanes(lanes: &[&str]) -> String {
    match lanes {
        [] => String::new(),
        [one] => (*one).to_string(),
        [head @ .., last] => format!("{} and {}", head.join(", "), last),
    }
}

/// Lowercase, fold the unicode right-single-quote (every phone keyboard emits
/// U+2019 in "what's"), and collapse whitespace — the same normalization the
/// promise audit uses, for the same reason.
fn normalize(s: &str) -> String {
    let lowered = s.to_lowercase().replace(['\u{2019}', '\u{02bc}', '\u{2032}'], "'");
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whole-phrase (word-boundary) containment over an already-[`normalize`]d
/// haystack, so "list" does not match "listen" and "plan" does not match
/// "plantain".
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::parity;

    /// A configured household whose ids and names are on no shipped list — the
    /// same premise as the human-flow fixture cast: if a test passes by
    /// recognising a familiar name, it proves nothing about configuration.
    fn configured() -> OwnerMap {
        let mut map = OwnerMap::from_pairs(vec![
            ("wren", vec!["meals", "cooking", "recipes"]),
            ("sage", vec!["nutrition", "diet"]),
            ("brindle", vec!["workouts", "training"]),
            ("tally", vec!["calendar", "coordination", "shopping"]),
        ]);
        map.set_display_names(vec![
            ("wren", "Wren"),
            ("sage", "Sage"),
            ("brindle", "Coach Brindle"),
            ("tally", "Tally"),
        ]);
        map
    }

    #[test]
    fn the_live_c011_wording_is_a_capability_ask() {
        assert!(is_capability_ask("What kinds of things can you help with?"));
    }

    #[test]
    fn capability_ask_variants_are_recognized() {
        for m in [
            "What can you do?",
            "what can you help with",
            "what can you guys do",
            "How can you help?",
            "what are you able to do?",
            "What sort of things can you take on?",
            "tell me what you can do",
            "what else can you do?",
            "What\u{2019}s more — what can you do?",
        ] {
            assert!(is_capability_ask(m), "should be a capability ask: {m:?}");
        }
    }

    #[test]
    fn a_lane_specific_ask_is_not_a_capability_ask() {
        // Each of these carries a domain noun: it has a real owner and a real
        // answer, and must keep both.
        for m in [
            "what can you do about Thursday's dinner?",
            "what can you help with on the shopping list",
            "how can you help me plan the week",
            "what can you do with the calendar today",
            "what's for dinner tonight?",
            "add milk to the list",
            "hi there",
        ] {
            assert!(!is_capability_ask(m), "should NOT be a capability ask: {m:?}");
        }
    }

    #[test]
    fn a_long_message_quoting_the_phrase_is_not_a_bare_capability_ask() {
        let long = "So before I forget, and I know this is a lot to take in at once, \
                    what can you do — anyway, more soon";
        assert!(!is_capability_ask(long));
    }

    #[test]
    fn the_answer_names_every_configured_lane_and_helper() {
        let answer = capability_answer(&configured()).expect("a configured household answers");
        for lane in ["dinner", "calendar", "shopping list", "workouts"] {
            assert!(answer.to_lowercase().contains(lane), "missing {lane:?}: {answer}");
        }
        for name in ["Wren", "Tally", "Coach Brindle"] {
            assert!(answer.contains(name), "missing configured helper {name:?}: {answer}");
        }
        // Grouped, so the helper who owns two lanes is named once.
        assert_eq!(answer.matches("Tally").count(), 1, "{answer}");
        assert!(answer.contains("plain words"), "{answer}");
    }

    #[test]
    fn the_answer_carries_no_promise_and_no_operations_words() {
        let answer = capability_answer(&configured()).unwrap();
        // The C011 failure, made structurally impossible: the answer promises
        // nothing, so no artifact is owed and no correction can follow.
        let audit = parity::audit_promise_in_turn("What kinds of things can you help with?", &answer);
        assert!(!audit.commits(), "the capability answer promised something: {audit:?}");
        // …and it is a promise under NO reading, not merely under the turn-aware one.
        assert!(
            !parity::audit_promise(&answer).commits(),
            "even the flat audit must see no promise: {answer}",
        );
        let low = answer.to_lowercase();
        for banned in ["coordinator", "flagged", "snag", "slip", "task", "agent", "dispatcher"] {
            assert!(!low.contains(banned), "answer leaked {banned:?}: {answer}");
        }
        assert!(!crate::notify::grounding::has_ops_jargon(&answer), "{answer}");
    }

    #[test]
    fn a_renamed_household_gets_its_own_answer() {
        let mut other = OwnerMap::from_pairs(vec![
            ("q1", vec!["cooking"]),
            ("q2", vec!["calendar", "shopping"]),
            ("q3", vec!["workouts"]),
        ]);
        other.set_display_names(vec![("q1", "Pip"), ("q2", "Juniper"), ("q3", "Rowan")]);
        let answer = capability_answer(&other).unwrap();
        for name in ["Pip", "Juniper", "Rowan"] {
            assert!(answer.contains(name), "{answer}");
        }
        for shipped in ["Wren", "Tally", "Brindle"] {
            assert!(!answer.contains(shipped), "a foreign cast leaked in: {answer}");
        }
    }

    #[test]
    fn an_unowned_lane_is_named_but_credited_to_nobody() {
        let mut map = OwnerMap::from_pairs(vec![("solo", vec!["cooking", "calendar"])]);
        map.set_display_names(vec![("solo", "Pim")]);
        let answer = capability_answer(&map).unwrap();
        assert!(answer.contains("Pim has"), "{answer}");
        assert!(answer.to_lowercase().contains("workouts"), "{answer}");
        assert!(answer.contains("You can ask about"), "{answer}");
        // The shopping lane has no owner here, so it is not attributed to Pim.
        let pim_sentence = answer.split(". ").find(|s| s.contains("Pim has")).unwrap();
        assert!(!pim_sentence.contains("shopping"), "{answer}");
    }

    #[test]
    fn no_configured_ownership_declines_to_answer() {
        // Nothing configured → no answer at all, so the caller falls through
        // rather than describing a household that was never declared.
        assert_eq!(capability_answer(&OwnerMap::default()), None);
    }

    #[test]
    fn a_household_with_a_missing_name_falls_back_to_its_id() {
        let map = OwnerMap::from_pairs(vec![("kx7", vec!["cooking", "calendar", "shopping"])]);
        let answer = capability_answer(&map).unwrap();
        assert!(answer.contains("kx7 has"), "{answer}");
    }
}
