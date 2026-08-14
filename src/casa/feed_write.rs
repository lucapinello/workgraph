//! Casa's diagnostic feed writer — `wg telegram feed-write`.
//!
//! Extracted from `commands/telegram.rs` as slice 6 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). It drives the EXACT `casa_feed` writer the listener uses, so
//! a smoke test can prove the feed end-to-end against the real binary without a live group.
//! Upstream has no conversation-pane feed to write to.
//!
//! Its eight tests stayed in `commands/telegram.rs`, repointed at this path. They also drive
//! `try_register_reminder`, which still has seven callers there and is therefore genuinely
//! shared — moving them would mean widening that helper's visibility, which is the debt slice
//! 5 just paid off. Same call as `resolve_dm_target`'s tests: a test follows its subject only
//! when it does not also depend on something that stays.

use crate::casa::reply_delivery::write_engine_receipt_at;
use crate::commands::telegram::load_feed_persona_catalog;
use anyhow::{Context, Result};
use std::path::Path;
use worksgood::notify::casa_audience;
use worksgood::notify::casa_feed;
use worksgood::notify::relay_receipt;

/// Mirror one synthetic line to the casa conversation-pane feed (diagnostic).
///
/// Drives the EXACT `casa_feed` writer the listener uses, so a smoke test can
/// prove the feed writer end-to-end against the real binary without a live
/// group: `--kind group` writes an inbound human line (needs `--sender`),
/// `--kind agent` writes a relayed persona reply (needs `--agent-id`). Only the
/// six display-safe fields are written — never a token or chat id.
pub fn run_feed_write(
    root: &Path,
    kind: &str,
    sender: Option<&str>,
    agent_id: Option<&str>,
    text: &str,
    src_id: Option<&str>,
    turn_id: Option<&str>,
    reply_phase: Option<&str>,
    non_relay_type: Option<&str>,
    message_id: Option<&str>,
    bot_id: Option<&str>,
) -> Result<()> {
    let personas = load_feed_persona_catalog(root);
    let entry = match kind {
        "group" => {
            let sender = sender.context("--kind group requires --sender")?;
            // Thread the caller-supplied opaque source id (docs/20 §2) so a smoke
            // test can drive the real writer with the SAME id twice and prove the
            // reader's srcId dedupe collapses the re-delivery to one pane line.
            casa_feed::group_entry(
                &personas,
                sender,
                text,
                casa_feed::now_ms(),
                src_id.map(str::to_string),
            )
        }
        "agent" => {
            let agent_id = agent_id.context("--kind agent requires --agent-id")?;
            casa_feed::agent_entry(&personas, agent_id, text, casa_feed::now_ms())
        }
        other => anyhow::bail!("--kind must be 'group' or 'agent', got '{other}'"),
    };

    // The causal stamps, from the flags or from the environment the gateway
    // dispatched us with. `WG_TURN_ID` carries the RAW accepted turn; it is
    // passed through UNTOUCHED and validated at write, so a hashed or
    // placeholder id produces no row rather than a row that certifies nothing.
    let turn_id = turn_id
        .map(str::to_string)
        .or_else(|| std::env::var("WG_TURN_ID").ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    // THE PHASE IS PARSED ONCE, AND IT IS NEVER GUESSED (task
    // receipt-engine-reply). Row and receipt take it from the SAME value, so the
    // two can never disagree about what this line was — it used to be parsed
    // twice from the same flag, which is one edit away from disagreeing.
    //
    // AND THERE IS NO DEFAULT. This used to read `.unwrap_or("final")`, so a
    // caller that named a turn and declared no phase had `final` stamped for it:
    // the STRONGEST of the four claims, the one the turn's one-final reservation
    // is keyed on, asserted by a writer that never said it. It also walked
    // straight past `casa_feed::validate`'s `TurnWithoutPhase` — v9.1's required
    // negative, unreachable from this seam because the default filled the exact
    // hole the validator exists to catch. The writer KNOWS which of the five it
    // is emitting; if it did not say, it is asked, not guessed for.
    //
    // THE VOCABULARY IS THE SCHEMA'S, AND IT IS A JOIN BETWEEN TWO REPOS. This
    // arm-per-word list IS the engine's half of the closed enum, and the gateway
    // asserts every member of ITS list is stamped verbatim by this binary
    // (`claw3d-bridge/test/replyPhaseEngineTwin.test.mjs`). `addendum` was in the
    // gateway's `REPLY_PHASES` from v9.2 and missing here, so the twin red-lined
    // on the exact message below: a word one implementation writes and the other
    // refuses is a row the gateway can produce and the engine cannot.
    let phase = match reply_phase.map(str::trim) {
        Some("ack") => Some(relay_receipt::ReplyPhase::Ack),
        Some("final") => Some(relay_receipt::ReplyPhase::Final),
        Some("addendum") => Some(relay_receipt::ReplyPhase::Addendum),
        Some("watchdog") => Some(relay_receipt::ReplyPhase::Watchdog),
        Some("failure") => Some(relay_receipt::ReplyPhase::Failure),
        Some(other) => {
            anyhow::bail!(
                "--reply-phase must be one of ack|final|addendum|watchdog|failure, got '{other}'"
            )
        }
        None => None,
    };
    let entry = match turn_id.as_deref() {
        Some(turn) => {
            // A turn-bound row must say WHICH reply of that turn it is. Refused
            // BEFORE the append, so a refusal leaves no row.
            let Some(phase) = phase else {
                anyhow::bail!(
                    "a row bound to a turn must declare --reply-phase \
                     (ack|final|addendum|watchdog|failure) — the writer knows which it is \
                     emitting, and an unstamped turn-bound agent row is a receipt/observe v9.1 \
                     required negative"
                );
            };
            entry.with_turn(turn, phase)
        }
        // No causal turn: its own single-row occurrence, with no turn to be a
        // phase of. An inbound human mirror lands here.
        None => entry,
    };
    let entry = match non_relay_type {
        Some(kind) => entry.with_non_relay_type(kind.trim()),
        None => entry,
    };

    let feed_path = casa_feed::feed_path_for(root);
    let role = agent_id.unwrap_or("unknown");

    // ONE TRANSACTION, through the very same writer the listener's delivery seam
    // uses — so a smoke test drives the production path rather than a look-alike
    // that can drift away from it. A receipt needs both a causal turn to join on
    // and a transport answer to record, so it is written only when the caller
    // supplies a `--message-id`; without one there is no delivery to prove and
    // inventing a receipt would forge exactly the link the ledger establishes.
    //
    // The BOUND-OR-BLOCKED gate applies to this writer exactly as it does to the
    // listener's: a diagnostic is still a row the family's pane shows and an
    // auditor counts, and `feed-write --kind agent` is precisely the shape that
    // put unattributable helper rows into a certification run.
    //
    // AND IT IS ONE SECTION, NOT TWO (docs/42 §9, `feed-lock-section`). The
    // audience record used to be written after this call returned, in a second
    // acquisition of the same lock — the one most likely to arrive after the
    // patience was already spent, and a whole extra queue position for every
    // other writer. It is written below, inside this section, with the held lock
    // as a witness. What did NOT move is the rollback boundary: the row and its
    // receipt are one fact and fail together, while a refused audience is
    // carried out and REPORTED rather than taking the row back out.
    let mut proved = false;
    let mut audience: Option<Result<casa_audience::AudienceOutcome, casa_audience::AudienceError>> =
        None;
    let written = casa_feed::append_entry_proving(&feed_path, &entry, |feed_id, lock| {
        // The phase is destructured HERE rather than defaulted above: a row with a
        // turn always carries one (the bail above), so pattern-matching it costs
        // nothing and leaves no `unwrap_or_default()` that could quietly stand in
        // for a declaration on some future path.
        if let (Some(turn), Some(mid), Some(phase)) = (turn_id.as_deref(), message_id, phase) {
            proved = true;
            // THE ONLY `?` IN THIS SECTION. A refused receipt rolls the row back
            // out; nothing below may, which is why the audience result is stored
            // instead of propagated.
            write_engine_receipt_at(
                root,
                turn,
                feed_id,
                role,
                bot_id.unwrap_or(role),
                Some(mid),
                relay_receipt::RelayOutcome::Send,
                phase,
                Some(lock),
            )
            .map(relay_receipt::Appended::regardless_of_release)?;
        }
        // WHO SAW IT, in this section. Gated on the turn alone and NOT on the
        // receipt: a row bound to a turn with no `--message-id` has no delivery
        // to prove and still has an audience, and that asymmetry is why this is
        // not folded into the branch above.
        if kind == "agent"
            && let Some(turn) = turn_id.as_deref()
        {
            audience = Some(casa_audience::record_group_reply_locked(
                &feed_path,
                turn,
                role,
                casa_feed::now_ms(),
                lock,
            ));
        }
        // Spelled out: `?` above converts through `From`, so with no annotation
        // the proof error type is ambiguous rather than "obviously the receipt's".
        Ok::<(), relay_receipt::ReceiptError>(())
    })
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("failed to append to feed {}", feed_path.display()))?;

    // The written line is itself display-safe (the field allowlist), so echoing
    // it back cannot leak a secret — handy for the smoke assertion. The global
    // feed id goes with it: a caller that must later prove this row needs the id
    // it was ACTUALLY allocated, never an ordinal it counted for itself.
    println!("{}", entry.to_json_line());
    println!("feedId={}", written.feed_id());
    // THE RELEASE VERDICT IS PART OF THE ANSWER, always stated (blocker 2). A
    // scripted certifier that only ever saw `feedId=` could not distinguish a
    // row written inside a proven section from one whose release nobody can
    // vouch for; it now has to read past the id to find that out.
    match &written {
        casa_feed::ProvenRow::Certified(_) => println!("feedRelease=proven"),
        casa_feed::ProvenRow::ReleaseUnverified { reason, .. } => {
            println!("feedRelease=unverified reason={reason}");
        }
    }
    // WHO SAW IT — the same ledger, the same six fields, through the same
    // `casa_audience` writer the listener's delivery seam now calls (task
    // audit-does-any). Stated on EVERY run, in one machine-readable word, for
    // the reason `feedRelease=` is: a certifier that only ever saw the row could
    // not tell a reply whose audience is on the record from one whose audience
    // nothing anywhere can name.
    //
    //   recorded / duplicate  the audience IS on the record (a duplicate is the
    //                         ack and the answer of one turn — one audience)
    //   skipped   no-turn     nothing to join a record to; the honest answer
    //             inbound     a human's own line is not a reply (see the module
    //                         header) — recording it would read as a helper
    //                         answering privately
    //   refused   <reason>    the row exists and its audience does NOT. The hole
    //                         this ledger closes, reported rather than swallowed.
    match (kind, audience) {
        (_, Some(Ok(outcome))) => println!("audience={}", outcome.as_str()),
        (_, Some(Err(e))) => {
            eprintln!("casa audience: the row was written but its audience was NOT: {e}");
            println!("audience=refused reason={e}");
        }
        ("agent", None) => println!(
            "audience=skipped reason={}",
            casa_audience::AudienceSkipped::NoTurn.as_str()
        ),
        _ => println!("audience=skipped reason=inbound"),
    }
    // HOW MANY TIMES THIS PROCESS TOOK THE CONVERSATION LOCK, stated on every run
    // (docs/42 §9, `feed-lock-section`). The number is the whole finding: every
    // section is a queue position for every other writer, and six concurrent
    // writers of this shape were measured at 986 ms of a 1000 ms budget while it
    // was more than one. A scripted certifier — and the twin gate — can now read
    // it from the real process instead of inferring it from a comment.
    println!(
        "feedLockAcquisitions={}",
        worksgood::notify::feed_lock::distinct_acquisitions()
    );
    if proved {
        println!("receipt=written");
    }
    Ok(())
}
