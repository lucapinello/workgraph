//! Casa's three one-shot Telegram answer commands: `owner`, `parity`, `capability`.
//!
//! Extracted from `commands/telegram.rs` as the fourth slice of the Casa/upstream split
//! (see docs/UPSTREAM-DIVERGENCE.md). None of the three exists upstream — not at our fork
//! point, not on `gwwg/main` today — and none of them is a `wg` concern: each answers one
//! family question and exits.
//!
//! CHOSEN BECAUSE IT IS CLEAN, not because it is big. These three drag no private helpers
//! and share none, so nothing had to be exposed in upstream's file: the marker count there
//! is unchanged. They also carry no tests in that file's test module, so nothing had to be
//! repointed. `run_web_inbound` was the obvious next candidate by size (512 lines) and was
//! deliberately skipped: it needs three new `pub(crate)` markers and ~69 test references
//! moved, and it gets cheaper once `run_listen` goes, taking `run_group_collective` and
//! `run_group_discussion` with it.
//!
//! `run_ask` sits next to these three in that file and is NOT a candidate: it is upstream's.
//!
//! This module borrows NOTHING from `commands::telegram` — the compiler said so, by warning
//! that the two helpers I imported on the strength of a regex match were both unused. So
//! this slice is fully self-contained: no import back into upstream's file, in either
//! direction.

use anyhow::Result;
use std::path::Path;

pub fn run_owner(
    ask: &str,
    persona: Option<&str>,
    root: Option<&Path>,
    _dry_run: bool,
    json: bool,
) -> Result<()> {
    use worksgood::notify::ownership::{self, OwnerDecision, OwnerMap};

    let domain = ownership::classify_domain(ask);
    let map = match root {
        Some(r) => OwnerMap::load(r),
        None => OwnerMap::default(),
    };
    let owner = map.owner_for_ask(ask).map(str::to_string);
    let decision = persona.map(|p| map.decide_owner(p, ask));

    if json {
        let (decision_slug, defer_to) = match &decision {
            Some(OwnerDecision::Owner) => ("owner", None),
            Some(OwnerDecision::Defer { owner }) => ("defer", Some(owner.clone())),
            None => ("n/a", None),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ask": ask,
                "domain": domain.slug(),
                "owner": owner,
                "persona": persona,
                "decision": decision_slug,
                "deferTo": defer_to,
            }))?
        );
        return Ok(());
    }

    println!("ask:     \"{ask}\"");
    println!("domain:  {}", domain.slug());
    match &owner {
        Some(o) => println!("owner:   {o}"),
        None => println!("owner:   (unresolved — no persona lists this domain)"),
    }
    match (persona, &decision) {
        (Some(p), Some(OwnerDecision::Owner)) => {
            println!("verdict: {p} OWNS this ask → it creates the task");
        }
        (Some(p), Some(OwnerDecision::Defer { owner })) => {
            println!(
                "verdict: {p} is OFF-DOMAIN → defers to {owner} (re-routed, never its own copy)"
            );
        }
        _ => {}
    }
    Ok(())
}

/// Audit a composed reply for promise-action parity — the `wg telegram parity`
/// seam (see [`crate::cli::TelegramCommands::Parity`]).
///
/// Runs the exact pattern-based classifier the live conversational turn uses and
/// prints: the promise kind (`action` / `preference` / `none`), whether the
/// reply already carries a `TASK_CREATE:` tail, and whether there is a MISMATCH
/// a live turn would repair (a one-off action promised with no artifact). No
/// side effects — nothing is sent, no task is created.
pub fn run_parity(reply_text: &str, human: Option<&str>, _dry_run: bool, json: bool) -> Result<()> {
    use worksgood::notify::lifecycle;
    use worksgood::notify::parity::{self, PromiseKind};

    // The audit runs over the human-facing reply, with any machine tail stripped
    // — exactly as the live turn sees it. With the human's ask in hand it is the
    // INTENT-AWARE audit the live turn runs (task capability-answer-no-invented-work);
    // with no ask it stays the reply-only classifier.
    let directive = lifecycle::extract_task_directive(reply_text.trim());
    let audit = match human {
        Some(ask) => parity::audit_promise_in_turn(ask, &directive.reply),
        None => parity::audit_promise(&directive.reply),
    };
    // Did the human ask for work at all? A turn that did not can never receive a
    // correction tail, however the reply is worded.
    let asked_for_action = human.map(parity::turn_requests_action);
    let has_artifact = directive.title.is_some();
    // A mismatch is the parity gap: a one-off action promised, no artifact.
    let mismatch = audit.commits_action() && !has_artifact;
    // What the live turn would actually append to the reply.
    let correction = if mismatch && asked_for_action.unwrap_or(true) {
        Some(parity::correction_line())
    } else {
        None
    };
    let fallback_title = if mismatch {
        Some(parity::fallback_task_title(
            human.unwrap_or(""),
            &directive.reply,
        ))
    } else {
        None
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "promised": audit.kind.slug(),
                "commits": audit.commits(),
                "hasArtifact": has_artifact,
                "artifactTitle": directive.title,
                "mismatch": mismatch,
                "matched": audit.matched,
                "fallbackTitle": fallback_title,
                "requestedAction": asked_for_action,
                "correction": correction,
            }))?
        );
        return Ok(());
    }

    println!("promised: {}", audit.kind.slug());
    if let Some(m) = &audit.matched {
        println!("matched:  \"{m}\"");
    }
    if let Some(asked) = asked_for_action {
        println!(
            "asked:    the turn {} ask for work",
            if asked { "DID" } else { "did NOT" }
        );
    }
    match directive.title {
        Some(t) => println!("artifact: TASK_CREATE present → \"{t}\""),
        None => println!("artifact: none"),
    }
    match audit.kind {
        PromiseKind::Preference => {
            println!("verdict:  standing preference → would be written to the durable store");
        }
        PromiseKind::Action if mismatch => {
            println!("verdict:  MISMATCH → promised an action but no artifact");
            println!("          a live turn would retry once, then fall back to task:");
            println!("          \"{}\"", fallback_title.unwrap_or_default());
            match &correction {
                Some(c) => println!("correction: \"{c}\""),
                None => println!("correction: none — the turn requested no action"),
            }
        }
        PromiseKind::Action => {
            println!("verdict:  action promised AND artifact present → parity OK");
        }
        PromiseKind::None => {
            println!("verdict:  no commitment → nothing owed");
        }
    }
    Ok(())
}

/// The single-owner routing test seam — the `wg telegram owner` command (see
/// [`crate::cli::TelegramCommands::Owner`]).
///
/// Runs the exact pure classifier the live conversational turn uses and prints
/// the ask's household domain, the single persona that owns it, and — with
/// `--persona` — whether that voice would create the task or defer to the owner.
/// No side effects: nothing is sent, no task created.
/// The CAPABILITY act seam — the `wg telegram capability` command (see
/// [`crate::cli::TelegramCommands::Capability`]).
///
/// Prints whether `text` is a bare capability ask and, when it is, the answer the
/// configured household gives ([`worksgood::notify::capability`]). Also reports the
/// promise audit of that answer, because the live-cert C011 failure was not the
/// answer's content but the phantom promise appended to it. Side-effect-free.
pub fn run_capability(text: &str, root: Option<&Path>, _dry_run: bool, json: bool) -> Result<()> {
    use worksgood::notify::capability;
    use worksgood::notify::ownership::OwnerMap;
    use worksgood::notify::parity;

    let is_ask = capability::is_capability_ask(text);
    let map = match root {
        Some(r) => OwnerMap::load(r),
        None => OwnerMap::default(),
    };
    let answer = if is_ask {
        capability::capability_answer(&map)
    } else {
        None
    };
    // The answer must never itself commit to work (that is the C011 defect).
    let promised = answer.as_deref().map(|a| {
        parity::audit_promise_in_turn(text, a)
            .kind
            .slug()
            .to_string()
    });

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "text": text,
                "capabilityAsk": is_ask,
                "answer": answer,
                "promised": promised,
                "requestedAction": parity::turn_requests_action(text),
            }))?
        );
        return Ok(());
    }

    println!("capability ask: {}", if is_ask { "yes" } else { "no" });
    match &answer {
        Some(a) => {
            println!("answer:  {a}");
            println!("promised: {}", promised.as_deref().unwrap_or("none"));
        }
        None if is_ask => {
            println!("answer:  none — this household declares no domain ownership");
        }
        None => println!("answer:  none — not a capability ask, the normal path owns it"),
    }
    Ok(())
}
