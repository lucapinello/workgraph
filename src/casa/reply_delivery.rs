//! Casa's family-reply delivery: scope, guard policy, and the sinks a turn's
//! answer travels through.
//!
//! Second slice of the Casa/upstream split (docs/UPSTREAM-DIVERGENCE.md), and the
//! one that pays for itself twice: it moves ~600 lines of Casa behaviour out of
//! upstream's `commands/telegram.rs` AND removes most of the `pub(crate)` markers
//! the first slice had to add there, because `casa::telegram_photo` now imports
//! these types from a sibling instead of reaching back into upstream's file.
//!
//! Nothing here is a `wg` concern. `wg` delivers a reply to a chat; Casa decides
//! WHOSE voice speaks, whether a group or DM scope applies, what may be said
//! without a guard, and mirrors every answer into the household's own feed.

use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use worksgood::notify::casa_audience;
use worksgood::notify::casa_feed;
use worksgood::notify::relay_receipt;
use worksgood::notify::telegram::TelegramConfig;
use worksgood::notify::telegram_conversation::ReplySink;

use crate::commands::telegram::{load_feed_persona_catalog, project_root, surface_mirror_failure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyScope {
    Group,
    Private,
}

impl ReplyScope {
    pub(crate) fn from_chat_type(chat_type: Option<&str>) -> Self {
        if matches!(chat_type, Some("group") | Some("supergroup")) {
            Self::Group
        } else {
            Self::Private
        }
    }
}

/// Shared context for the final engine-owned delivery seam.
///
/// Every family-visible reply produced by this command module is sent through
/// this context. It resolves the configured presentation, applies the
/// engine-side family-voice guard to direct replies, and mirrors only confirmed
/// GROUP deliveries to the casa feed. Private replies never touch that file.
#[derive(Clone)]
pub(crate) struct FamilyReplyDelivery {
    feed_path: PathBuf,
    config: TelegramConfig,
    pub(crate) personas: casa_feed::PersonaCatalog,
    pub(crate) family_roster: worksgood::notify::grounding::FamilyVoiceRoster,
    /// The accepted turn, when a test supplies it directly instead of through
    /// the environment. Production ALWAYS resolves the turn from `WG_TURN_ID`
    /// (the gateway dispatches one process per accepted turn), but `WG_TURN_ID`
    /// is process-global: a test that sets it re-keys the turn reservation for
    /// every other test running at that instant, which silently suppresses their
    /// second reply. This seam lets the receipt tests name their own turn
    /// without touching the environment other tests are reading.
    #[cfg(test)]
    turn_override: Option<String>,
}

impl FamilyReplyDelivery {
    pub(crate) fn load(workgraph_dir: &Path, config: &TelegramConfig) -> Self {
        let root = project_root(workgraph_dir);
        Self::load_at(workgraph_dir, config, casa_feed::feed_path_for(&root))
    }

    pub(crate) fn load_at(
        workgraph_dir: &Path,
        config: &TelegramConfig,
        feed_path: PathBuf,
    ) -> Self {
        let root = project_root(workgraph_dir);
        Self {
            feed_path,
            config: config.clone(),
            personas: load_feed_persona_catalog(&root),
            family_roster: worksgood::notify::grounding::load_family_voice_roster(
                &root,
                workgraph_dir,
            ),
            #[cfg(test)]
            turn_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(
        feed_path: PathBuf,
        config: TelegramConfig,
        personas: casa_feed::PersonaCatalog,
        family_roster: worksgood::notify::grounding::FamilyVoiceRoster,
    ) -> Self {
        Self {
            feed_path,
            config,
            personas,
            family_roster,
            turn_override: None,
        }
    }

    /// Name the accepted turn directly, instead of through the process-global
    /// `WG_TURN_ID` every other test is also reading.
    #[cfg(test)]
    pub(crate) fn with_turn_override(mut self, turn: &str) -> Self {
        self.turn_override = Some(turn.to_string());
        self
    }

    /// The canonical turn this delivery belongs to, RAW and verbatim.
    ///
    /// Only `web-turn-<uuid v4>` qualifies: a request id, a digest or the
    /// engine's own hashed idempotency key is not the turn, and stamping one in
    /// the causal position writes a row no receipt could ever join.
    fn canonical_turn(&self) -> Option<String> {
        #[cfg(test)]
        if let Some(turn) = self.turn_override.as_deref() {
            return worksgood::notify::relay_receipt::is_valid_turn_id(turn)
                .then(|| turn.to_string());
        }
        worksgood::notify::telegram_conversation::canonical_turn_id("")
    }

    pub(crate) fn wrap<S>(
        &self,
        inner: S,
        scope: ReplyScope,
        guard: GuardPolicy,
    ) -> ScopedFamilyReplySink<S> {
        ScopedFamilyReplySink {
            inner,
            delivery: self.clone(),
            scope,
            guard,
        }
    }

    pub(crate) async fn send(
        &self,
        scope: ReplyScope,
        bot_id: &str,
        chat_id: &str,
        text: &str,
    ) -> Result<Option<String>> {
        use worksgood::notify::telegram_conversation as convo;
        let sink = self.wrap(
            convo::BotReplySink::new(self.config.clone()),
            scope,
            GuardPolicy::Enforce,
        );
        convo::ReplySink::send(&sink, bot_id, chat_id, text).await
    }

    /// The project root that owns this feed — `<root>/.casa/group-feed.jsonl`.
    /// The receipt ledger and the transport-scope key live beside the feed, so
    /// they are resolved from the SAME path the rows are written to and cannot
    /// drift into a different project than the row they prove.
    fn project_root(&self) -> PathBuf {
        self.feed_path
            .parent()
            .and_then(|casa| casa.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Mirror the exact guarded bytes that Telegram accepted, excluding the
    /// transient latency acknowledgement that a later edit replaces — and write
    /// the ENGINE's OWN RECEIPT for the delivery that produced them.
    ///
    /// The receipt is written HERE, at the one point where all four facts are in
    /// hand at once: the accepted turn, the row's global feed id, the bot that
    /// physically sent it, and the transport's own answer (`message_id`). It is
    /// an INDEPENDENT record, not an inference: the gateway used to reconstruct
    /// "the helper replied" from the row the writer itself wrote, which is a
    /// claim the writer made about itself.
    ///
    /// A reply with no canonical turn (a legacy or listener-initiated path) is
    /// mirrored as before and gets NO receipt — a receipt needs a turn to join
    /// on, and inventing one would forge the very link it exists to prove. Such
    /// a row is unbound, which is precisely what a sealed run refuses.
    fn mirror(
        &self,
        scope: ReplyScope,
        bot_id: &str,
        text: &str,
        message_id: Option<&str>,
        outcome: relay_receipt::RelayOutcome,
        phase: relay_receipt::ReplyPhase,
    ) -> MirrorOutcome {
        use worksgood::notify::telegram_conversation as convo;
        // THE ACK IS MIRRORED LIKE EVERY OTHER DELIVERED ROW. It used to be
        // dropped here — `is_ack` skipped both the feed row and the receipt —
        // and the exact-tree audit named that a P0: the ack physically reaches
        // the family, so a turn that crashes between ack and final produces a
        // real, family-visible delivery that certification cannot observe at
        // all. It has no global feed id and nothing proving it, and the turn's
        // one-to-many delivery lifecycle cannot be reconstructed.
        //
        // What the ack must NOT do is reserve finality — and it does not: the
        // reservation lives in `telegram_conversation`'s durable delivery guard,
        // which reserves `ReplyPhase::Final` only, so a crash after the ack
        // still leaves the turn answerable. Avoiding the reservation was always
        // the right instinct; deleting the evidence was not the way to get it.
        // v9.1's cardinality rule says the same in one line: "ack optional
        // (heavy lane)... exactly one replyPhase:'final' per accepted turn".
        if scope != ReplyScope::Group {
            return MirrorOutcome::Skipped;
        }
        // A caller that has not been converted to the phased API and hands us
        // the ack TEXT gets the phase corrected here, so the row it writes says
        // `ack` rather than mis-stamping the turn's final answer. The phase is
        // still writer-stamped wherever the writer knows it; this is the
        // fallback, not the rule.
        let phase = if phase == relay_receipt::ReplyPhase::Final && text == convo::ack_line() {
            relay_receipt::ReplyPhase::Ack
        } else {
            phase
        };
        let agent_id = convo::agent_for_bot(&self.config, bot_id);
        let entry = casa_feed::agent_entry(&self.personas, &agent_id, text, casa_feed::now_ms());
        // The turn is stamped RAW and verbatim, exactly as the gateway handed it
        // over. Only `web-turn-<uuid v4>` qualifies, so the engine's internal
        // hashed key can never reach the causal position.
        let turn = self.canonical_turn();
        let entry = match turn.as_deref() {
            // The phase is stamped by the writer, which KNOWS which it is —
            // never derived from the text later.
            Some(turn) => entry.with_turn(turn, phase),
            None => entry,
        };

        // ONE TRANSACTION. The row and the receipt that proves it land inside a
        // single critical section, or neither does. Two transactions would let a
        // refused or corrupt receipt leave behind exactly the unprovable row the
        // whole contract exists to eliminate.
        //
        // WHO SAW IT (`casa_audience`, task audit-does-any) RIDES IN THE SAME
        // SECTION (docs/42 §9, `feed-lock-section`) — sharing the exclusion, NOT
        // the rollback. The reply has already physically reached the family group
        // by the time we mirror it, so the audience is a FACT whichever way the
        // row goes: its result is carried out of the closure rather than
        // returned, so a refused audience can never take the row back out, and
        // the fallback below still records it when there was no section at all.
        // What changes is only that a successful mirror stops paying for a second
        // acquisition of the same lock.
        //
        // Before the audience call existed the gateway instrumented every reply
        // seam it owned and THIS process — the listener, which the gateway is not
        // in the loop for at all — appended agent rows carrying a turnId with no
        // audience record anywhere. An auditor reading the ledger for such a turn
        // got "no reply recorded", which is indistinguishable from "no reply".
        let mut audience_recorded = false;
        let written = casa_feed::append_entry_proving(&self.feed_path, &entry, |feed_id, lock| {
            let Some(turn) = turn.as_deref() else {
                // No canonical turn: a legacy or listener-initiated reply. It
                // gets no receipt — inventing a turn would forge the very link a
                // receipt exists to prove — and the row stands as unbound, which
                // is precisely what a sealed run refuses. It has no audience to
                // record either: there is no turn to join one to.
                return Ok(());
            };
            let receipt = self
                .write_engine_receipt(
                    turn, feed_id, &agent_id, bot_id, message_id, outcome, phase, lock,
                )
                // Written inside THIS transaction, so the receipt frame released
                // nothing; the section's verdict arrives with the row below.
                .map(relay_receipt::Appended::regardless_of_release);
            if receipt.is_ok() {
                // Only on the path where the row SURVIVES. A receipt failure
                // truncates the row back out, and an audience record for a row
                // that no longer exists is worse than the second section.
                audience_recorded = true;
                self.report_audience(casa_audience::record_group_reply_locked(
                    &self.feed_path,
                    turn,
                    &agent_id,
                    casa_feed::now_ms(),
                    lock,
                ));
            }
            receipt
        });

        // THE ROW DID NOT SURVIVE ITS SECTION — but the family still saw the
        // reply, so the audience is still a fact and is recorded in a section of
        // its own, exactly as every mirror used to do.
        if !audience_recorded {
            self.record_reply_audience(turn.as_deref(), &agent_id);
        }

        match written {
            // THE SECTION'S RELEASE VERDICT TRAVELS WITH THE ROW (blocker 2).
            // `Recorded` used to be a plain `feed_id`, so a row written inside a
            // section whose release could not be proven read exactly like a row
            // written inside one that proved it let go.
            Ok(row) => MirrorOutcome::Recorded {
                feed_id: row.feed_id(),
                proven: turn.is_some(),
                release_unverified: match row {
                    casa_feed::ProvenRow::Certified(_) => None,
                    casa_feed::ProvenRow::ReleaseUnverified { reason, .. } => Some(reason),
                },
            },
            Err(failure) => {
                let detail = failure.to_string();
                eprintln!(
                    "[{}] casa feed: the reply was sent but NOT recorded: {detail}",
                    chrono::Utc::now().format("%H:%M:%S"),
                );
                MirrorOutcome::Failed(detail)
            }
        }
    }

    /// Record WHO SAW this reply, in the same ledger and the same six fields the
    /// gateway's `audienceLedger.mjs` writes (see [`casa_audience`]).
    ///
    /// Only reached for a GROUP reply (its one caller returns early for every
    /// other scope), so the audience is `group` reached `via` the family chat.
    ///
    /// TWO OUTCOMES ARE NOT ERRORS AND ONE IS. A reply with no canonical turn
    /// answers no turn and has nothing to join a record to — skipped, silently,
    /// because it is the correct answer rather than a failure. A second write
    /// for a turn that already reached this chat (the ack, then the answer) is a
    /// no-op, because the question is "who saw it", not "how many sends". But a
    /// REFUSAL means this reply went out with NO durable audience record — the
    /// exact hole the ledger closes — so it is printed where the operator reads
    /// the listener's log, never swallowed.
    fn record_reply_audience(&self, turn: Option<&str>, agent_id: &str) {
        let Some(turn) = turn else { return };
        self.report_audience(casa_audience::record_group_reply(
            &self.feed_path,
            turn,
            agent_id,
            casa_feed::now_ms(),
        ));
    }

    /// The one place a mirrored reply's audience outcome is reported, so the
    /// in-section write and the fallback cannot report it two different ways.
    fn report_audience(
        &self,
        outcome: Result<casa_audience::AudienceOutcome, casa_audience::AudienceError>,
    ) {
        if let Err(e) = outcome {
            eprintln!(
                "[{}] casa audience: the reply was sent but its audience was NOT recorded: {e}",
                chrono::Utc::now().format("%H:%M:%S"),
            );
        }
    }

    /// Build and append the engine receipt for one relayed row, inside the feed
    /// transaction that wrote it.
    #[allow(clippy::too_many_arguments)]
    fn write_engine_receipt(
        &self,
        turn: &str,
        feed_id: i64,
        agent_id: &str,
        bot_id: &str,
        message_id: Option<&str>,
        outcome: relay_receipt::RelayOutcome,
        phase: relay_receipt::ReplyPhase,
        lock: &worksgood::notify::feed_lock::FeedLock,
    ) -> Result<relay_receipt::Appended, relay_receipt::ReceiptError> {
        write_engine_receipt_at(
            &self.project_root(),
            turn,
            feed_id,
            agent_id,
            bot_id,
            message_id,
            outcome,
            phase,
            Some(lock),
        )
    }
}

/// What mirroring one delivered reply actually did.
///
/// A delivery that produced NO ROW is not a quiet log line: the family has an
/// answer the house has no record of, and every later read — the pane, the
/// audit, the join — will say it never happened. The caller surfaces this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorOutcome {
    /// Nothing to mirror: a DM, or the transient ack a later edit replaces.
    Skipped,
    /// The row is on disk. `proven` is false for a legacy reply with no
    /// canonical turn, which can carry no receipt. `release_unverified` carries
    /// the reason when the section that wrote the row could not PROVE it let the
    /// feed lock go — the row is real, the exclusion around it is unvouched, and
    /// a consumer that certifies rows must be able to tell the two apart.
    Recorded {
        feed_id: i64,
        proven: bool,
        release_unverified: Option<String>,
    },
    /// The row is NOT on disk, and the message may already be with the family.
    Failed(String),
}

/// Append ONE engine receipt for a row that was just written and relayed.
///
/// Shared by the listener's delivery seam and the scripted `wg telegram
/// feed-write` seam, so a smoke test drives the SAME writer the family's
/// replies go through rather than a look-alike that can drift away from it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_engine_receipt_at(
    root: &Path,
    turn: &str,
    feed_id: i64,
    role_id: &str,
    bot_id: &str,
    message_id: Option<&str>,
    outcome: relay_receipt::RelayOutcome,
    phase: relay_receipt::ReplyPhase,
    // The held feed lock, when this receipt is being written inside the same
    // transaction as the row it proves. `None` means "take the lock yourself" —
    // the scripted `feed-write` seam, which has no surrounding transaction.
    lock: Option<&worksgood::notify::feed_lock::FeedLock>,
) -> Result<relay_receipt::Appended, relay_receipt::ReceiptError> {
    // WHICH BOT PHYSICALLY SENT THIS — not the semantic reply role. One role can
    // be spoken by different bots across a rotation, and "whose token sent it" is
    // the question a delivery dispute turns on. Keyed digest, so a leaked ledger
    // is not a reversible dictionary of the bot roster.
    let scope_id = relay_receipt::scope_id_for_bot(root, bot_id)?;
    // A POSITIVE message id is the only proof of delivery there is. Anything
    // else — absent, unparseable, zero, negative — is UNPROVEN, and is recorded
    // as unproven rather than guessed in either direction.
    let mid = message_id
        .and_then(|m| m.trim().parse::<i64>().ok())
        .filter(|id| *id > 0);
    let status = if mid.is_some() {
        relay_receipt::RelayStatus::Delivered
    } else {
        relay_receipt::RelayStatus::Unproven
    };
    let receipt = relay_receipt::engine_receipt(
        turn,
        feed_id,
        "agent",
        role_id,
        &scope_id,
        mid,
        status,
        outcome,
        phase,
        // ITEM 8 — the attempt the GATEWAY minted, read from the environment it
        // dispatched us with. Without it every attempt of one turn keys the same,
        // and a genuine self-heal retry is suppressed as though it were a refire
        // of the first — erasing the evidence for the send that actually reached
        // the family.
        std::env::var("WG_ATTEMPT_ID").ok().as_deref(),
        casa_feed::now_ms(),
    );
    match lock {
        // Inside the caller's feed transaction: this frame releases nothing, so
        // the release verdict is the enclosing section's to propagate.
        Some(held) => relay_receipt::append_locked(root, &receipt, held)
            .map(|()| relay_receipt::Appended::InCallersSection),
        None => relay_receipt::append(root, &receipt),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardPolicy {
    /// Apply the grounding module's family-visible copy gate here.
    Enforce,
    /// The conversation/discussion engine already applied the richer guard,
    /// including any narrowly authorized handoff suffix.
    AlreadyGuarded,
}

pub(crate) struct ScopedFamilyReplySink<S> {
    pub(crate) inner: S,
    delivery: FamilyReplyDelivery,
    scope: ReplyScope,
    guard: GuardPolicy,
}

pub(crate) struct BorrowedReplySink<'a>(
    pub(crate) &'a dyn worksgood::notify::telegram_conversation::ReplySink,
);

#[async_trait]
impl worksgood::notify::telegram_conversation::ReplySink for BorrowedReplySink<'_> {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        self.0.send(bot_id, chat_id, text).await
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.0.edit(bot_id, chat_id, message_id, text).await
    }
}

impl<S> ScopedFamilyReplySink<S> {
    fn guarded(&self, text: &str) -> String {
        match self.guard {
            GuardPolicy::Enforce => {
                let guarded = worksgood::notify::grounding::enforce_family_voice(
                    text,
                    &self.delivery.family_roster,
                );
                if guarded != text {
                    eprintln!(
                        "[{}] family-voice guard: cleaned a direct reply before delivery",
                        chrono::Utc::now().format("%H:%M:%S"),
                    );
                }
                guarded
            }
            GuardPolicy::AlreadyGuarded => text.to_string(),
        }
    }
}

#[async_trait]
impl<S> worksgood::notify::telegram_conversation::ReplySink for ScopedFamilyReplySink<S>
where
    S: worksgood::notify::telegram_conversation::ReplySink,
{
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        self.send_phase(bot_id, chat_id, text, relay_receipt::ReplyPhase::Final)
            .await
    }

    async fn send_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        text: &str,
        phase: relay_receipt::ReplyPhase,
    ) -> Result<Option<String>> {
        let guarded = self.guarded(text);
        let mid = self
            .inner
            .send_phase(bot_id, chat_id, &guarded, phase)
            .await?;
        // The transport's own answer travels with the row it proves: the receipt
        // is written from `mid`, never inferred from the row afterwards.
        let mirrored = self.delivery.mirror(
            self.scope,
            bot_id,
            &guarded,
            mid.as_deref(),
            relay_receipt::RelayOutcome::Send,
            phase,
        );
        surface_mirror_failure(mirrored)?;
        Ok(mid)
    }

    async fn edit(&self, bot_id: &str, chat_id: &str, message_id: &str, text: &str) -> Result<()> {
        self.edit_phase(
            bot_id,
            chat_id,
            message_id,
            text,
            relay_receipt::ReplyPhase::Final,
        )
        .await
    }

    async fn edit_phase(
        &self,
        bot_id: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        phase: relay_receipt::ReplyPhase,
    ) -> Result<()> {
        let guarded = self.guarded(text);
        self.inner
            .edit_phase(bot_id, chat_id, message_id, &guarded, phase)
            .await?;
        // An EDIT of the ack into the final answer is a different delivery from a
        // fresh SEND, with a different message id story — typed so a fallback
        // send after a refused edit cannot be read as the edit that never
        // applied. The FALLBACK's id is the one that carries the answer.
        let fallback = self.inner.take_fallback_message_id();
        let (delivered_id, outcome) = match fallback.as_deref() {
            Some(id) => (id, relay_receipt::RelayOutcome::Fallback),
            None => (message_id, relay_receipt::RelayOutcome::Edit),
        };
        let mirrored = self.delivery.mirror(
            self.scope,
            bot_id,
            &guarded,
            Some(delivered_id),
            outcome,
            phase,
        );
        surface_mirror_failure(mirrored)?;
        Ok(())
    }

    fn take_fallback_message_id(&self) -> Option<String> {
        self.inner.take_fallback_message_id()
    }
}

/// A network-free [`ReplySink`](worksgood::notify::telegram_conversation::ReplySink)
/// that records each send instead of hitting Telegram — the credential-free seam
/// behind `wg telegram lifecycle --mock-send`. It lets the cross-surface smoke
/// drive the REAL lifecycle tick and the REAL casa-feed mirror end-to-end (only
/// the transport is stubbed): every send is recorded and returns a synthetic
/// message id, so `deliver_lifecycle_fire` treats it as a confirmed delivery and
/// mirrors a group report-back into the pane feed exactly as a live send would.
#[derive(Default)]
pub(crate) struct RecordingSink {
    pub(crate) sends: std::sync::Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl worksgood::notify::telegram_conversation::ReplySink for RecordingSink {
    async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
        let mut sends = self.sends.lock().unwrap();
        let n = sends.len() + 1;
        sends.push((bot_id.to_string(), chat_id.to_string(), text.to_string()));
        Ok(Some(format!("mock-{n}")))
    }
}
