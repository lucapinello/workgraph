//! Group-chat **discussion rounds** (task `group-chat-discussion`).
//!
//! When a confirmed-human group message is elected COLLECTIVE
//! ([`Election::All`](super::telegram_group::Election::All)) **and** reads as an
//! opinion / discussion ask ([`is_discussion_ask`](super::telegram_group::is_discussion_ask))
//! — "can you guys discuss this and find consensus", "what do you all think?",
//! "thoughts?" — the family should actually *talk it through* rather than fire
//! four independent one-liners. This module runs that round:
//!
//! 1. Each bound-session persona contributes ONE short in-character take, in
//!    roster order, **sequenced** so later voices can react to earlier ones (each
//!    take's prompt embeds the takes so far). Each take is sent via ITS OWN bot.
//! 2. The configured coordination owner closes with a 1–3 sentence synthesis
//!    ("so the consensus seems to be…") — but ONLY when at least two *other*
//!    voices actually contributed.
//! 3. It stays tight: a per-voice compose budget, an overall round deadline
//!    (~90s), and a length cap. A voice whose session errors or does not answer
//!    in time is **skipped silently** — never a glitch line, never jargon in the
//!    family group.
//!
//! The runner is deliberately factored around the same mockable seams the 1:1 /
//! collective paths use ([`ReplyComposer`] + [`ReplySink`]) so the whole round —
//! sequencing, dead-session skips, the synthesis threshold — is provable in unit
//! tests without a live model or a live bot.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::grounding::{self, FamilyVoiceRoster};
use super::telegram_conversation::{
    CanonicalDeliveryState, ReplyComposer, ReplySink, canonical_delivery_state,
    send_canonical_reply_once,
};

/// Hard character cap for a single voice's take, applied after compose as a
/// safety net (the prompt already asks for one or two sentences). Cut on a word
/// boundary with an ellipsis so a long reply never floods the group.
const TAKE_CHAR_CAP: usize = 600;

/// Timing budget for a discussion round. Both are env-tunable without a rebuild:
/// * `WG_TELEGRAM_DISCUSS_VOICE_SECS` — per-voice compose budget (default 22)
/// * `WG_TELEGRAM_DISCUSS_ROUND_SECS` — whole-round deadline (default 90)
///
/// Each voice gets `min(per_voice, remaining_round_budget)`; once the round
/// budget is spent, any remaining voices are skipped rather than blocking.
#[derive(Debug, Clone, Copy)]
pub struct DiscussionTiming {
    pub per_voice: Duration,
    pub overall: Duration,
}

impl Default for DiscussionTiming {
    fn default() -> Self {
        Self {
            per_voice: Duration::from_secs(22),
            overall: Duration::from_secs(90),
        }
    }
}

impl DiscussionTiming {
    pub fn from_env() -> Self {
        let d = Self::default();
        let per_voice = std::env::var("WG_TELEGRAM_DISCUSS_VOICE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .unwrap_or(d.per_voice);
        let overall = std::env::var("WG_TELEGRAM_DISCUSS_ROUND_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .unwrap_or(d.overall);
        Self { per_voice, overall }
    }
}

/// One participant in a discussion round: a persona with a bound session that can
/// compose a grounded, in-voice take. Voices without a bound session (or from an
/// unconfirmed sender) are excluded upstream — the round only ever holds voices
/// that can actually speak in character.
#[derive(Debug, Clone)]
pub struct DiscussionVoice {
    /// The `[telegram.bots.<id>]` persona id — the bot that sends this take.
    pub bot_id: String,
    /// Project-local display name from the ordered household roster.
    pub display_name: String,
    /// The graph agent id whose bound session grounds the take.
    pub agent_id: String,
    /// The persona's bound persistent session.
    pub session_ref: String,
}

/// A take that actually landed in the group: which persona said it and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Take {
    pub bot_id: String,
    pub display_name: String,
    pub text: String,
}

/// The result of running a round — what was said, what was skipped, and the
/// configured coordination owner's wrap-up (present only when the synthesis
/// threshold was met). No token or chat id — safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionOutcome {
    /// Takes that landed, in contribution order.
    pub takes: Vec<Take>,
    /// The configured owner's closing synthesis, when it fired (at least two
    /// other voices landed).
    pub synthesis: Option<String>,
    /// Persona bot ids skipped (session errored, timed out, or send failed) —
    /// never surfaced to the group, but recorded for the observability log.
    pub skipped: Vec<String>,
}

/// Clamp a take to [`TAKE_CHAR_CAP`], cutting on a word boundary with an ellipsis
/// so an over-long reply never floods the group. Whitespace-trimmed first.
fn clamp_take(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= TAKE_CHAR_CAP {
        return t.to_string();
    }
    let truncated: String = t.chars().take(TAKE_CHAR_CAP).collect();
    let cut = match truncated.rfind(char::is_whitespace) {
        Some(i) if i > TAKE_CHAR_CAP / 2 => &truncated[..i],
        _ => truncated.as_str(),
    };
    format!("{}…", cut.trim_end())
}

/// Build the framed "message" a voice composes its take against. The persona's
/// own voice/session grounding is added by the composer's own prompt builder —
/// this only supplies the discussion framing and the takes so far, so a later
/// voice can genuinely react to earlier ones. Pure.
pub fn discussion_take_message(topic: &str, prior: &[Take]) -> String {
    let mut m = String::new();
    m.push_str("The family group chat is having a quick discussion. Someone asked: \"");
    m.push_str(topic.trim());
    m.push_str("\".\n\n");
    if prior.is_empty() {
        m.push_str("You're first to weigh in — kick it off with your honest take. ");
    } else {
        m.push_str("Here's what the family have said so far:\n");
        for t in prior {
            m.push_str("- ");
            m.push_str(&t.display_name);
            m.push_str(": ");
            m.push_str(t.text.trim());
            m.push('\n');
        }
        m.push('\n');
        m.push_str(
            "Add your own brief take, and feel free to agree, push back, or build on what the \
             others said. ",
        );
    }
    m.push_str(
        "Keep it to one or two sentences, in your own voice — this is a lively group chat, not \
         an essay.",
    );
    m
}

/// Build the framed message the configured synthesizer composes against. Pure.
pub fn synthesis_message(topic: &str, takes: &[Take]) -> String {
    let mut m = String::new();
    m.push_str("The family just talked through: \"");
    m.push_str(topic.trim());
    m.push_str("\". Here's what everyone said:\n");
    for t in takes {
        m.push_str("- ");
        m.push_str(&t.display_name);
        m.push_str(": ");
        m.push_str(t.text.trim());
        m.push('\n');
    }
    m.push_str(
        "\nAs the one who keeps the family organised, close the discussion in one to three \
         sentences: where did everyone land? Start naturally, e.g. \"So the consensus seems to \
         be…\". No lists, no jargon — just the sense of the room.",
    );
    m
}

/// Compose one voice's contribution, bounded by `budget`. Returns `None` on
/// timeout, compose error, or an empty reply — the caller then skips this voice
/// silently (never a glitch line in the group). A successful reply is
/// length-clamped.
async fn compose_bounded(
    composer: &dyn ReplyComposer,
    workgraph_dir: &Path,
    session_ref: &str,
    agent_id: &str,
    message: &str,
    budget: Duration,
    family_roster: &FamilyVoiceRoster,
) -> Option<String> {
    match tokio::time::timeout(
        budget,
        composer.compose(workgraph_dir, session_ref, agent_id, message),
    )
    .await
    {
        // Discussion turns do not pass through telegram_conversation's finalizer.
        // Guard the clamped draft here, before it enters either the Telegram sink
        // or the mirrored Casa feed.
        Ok(Ok(text)) if !text.trim().is_empty() => Some(grounding::enforce_family_voice(
            &clamp_take(&text),
            family_roster,
        )),
        _ => None,
    }
}

/// Remaining round budget, saturating at zero.
fn remaining(overall: Duration, started: Instant) -> Duration {
    overall.checked_sub(started.elapsed()).unwrap_or_default()
}

fn discussion_delivery_id(physical_turn_key: &str, part: &str, bot_id: &str) -> String {
    if physical_turn_key.trim().is_empty() {
        String::new()
    } else {
        format!("{physical_turn_key}\u{1f}{part}\u{1f}{bot_id}")
    }
}

enum DiscussionPartResolution {
    Delivered(String),
    Skipped,
    Blocked,
}

async fn send_discussion_canonical(
    workgraph_dir: &Path,
    delivery_id: &str,
    bot_id: &str,
    chat_id: &str,
    text: &str,
    sink: &dyn ReplySink,
) -> DiscussionPartResolution {
    match send_canonical_reply_once(workgraph_dir, delivery_id, bot_id, chat_id, text, sink).await {
        Ok(CanonicalDeliveryState::Confirmed(text)) => DiscussionPartResolution::Delivered(text),
        Ok(CanonicalDeliveryState::Skipped) => DiscussionPartResolution::Skipped,
        // A transport error, in-flight claim, or legacy/corrupt state is not a
        // family-visible take. Keep it out of subsequent compose context.
        Ok(
            CanonicalDeliveryState::Missing
            | CanonicalDeliveryState::Ready(_)
            | CanonicalDeliveryState::Pending
            | CanonicalDeliveryState::Unavailable,
        )
        | Err(_) => DiscussionPartResolution::Blocked,
    }
}

/// Run a discussion round: sequenced in-voice takes then an optional synthesis,
/// sending via `sink`.
///
/// `voices` is the roster order the takes are contributed in (each must have a
/// bound session). `synthesizer_bot` is the persona that closes the discussion
/// from project configuration. The synthesis fires only when at least two
/// *other* voices contributed a take. `timing` bounds each voice and the round
/// as a whole; a voice that errors or does not answer within its budget is
/// skipped, never blocking the round and never leaking an error to the group.
/// For replayable rounds, an unconfirmed transport becomes an ordering barrier:
/// no later take or synthesis may land until an exact retry confirms it.
/// `physical_turn_key` is the opaque occurrence key shared by every contribution
/// in this round. Each take and synthesis derives a distinct durable delivery
/// claim from it, so replaying one physical turn sends nothing twice while a
/// later turn containing the same words remains eligible.
pub async fn run_discussion_round(
    workgraph_dir: &Path,
    topic: &str,
    voices: &[DiscussionVoice],
    synthesizer_bot: &str,
    composer: &dyn ReplyComposer,
    family_roster: &FamilyVoiceRoster,
    sink: &dyn ReplySink,
    chat_id: &str,
    physical_turn_key: &str,
    timing: DiscussionTiming,
) -> Result<DiscussionOutcome> {
    let started = Instant::now();
    let mut takes: Vec<Take> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut delivery_barrier = false;

    for voice in voices {
        if delivery_barrier {
            skipped.push(voice.bot_id.clone());
            continue;
        }
        let delivery_id = discussion_delivery_id(physical_turn_key, "take", &voice.bot_id);
        let state = canonical_delivery_state(workgraph_dir, &delivery_id, &voice.bot_id, chat_id)?;
        let resolution = match state {
            CanonicalDeliveryState::Confirmed(text) => DiscussionPartResolution::Delivered(text),
            CanonicalDeliveryState::Skipped => DiscussionPartResolution::Skipped,
            CanonicalDeliveryState::Ready(text) => {
                send_discussion_canonical(
                    workgraph_dir,
                    &delivery_id,
                    &voice.bot_id,
                    chat_id,
                    &text,
                    sink,
                )
                .await
            }
            CanonicalDeliveryState::Pending | CanonicalDeliveryState::Unavailable => {
                DiscussionPartResolution::Blocked
            }
            CanonicalDeliveryState::Missing => {
                let left = remaining(timing.overall, started);
                if left.is_zero() {
                    send_discussion_canonical(
                        workgraph_dir,
                        &delivery_id,
                        &voice.bot_id,
                        chat_id,
                        "",
                        sink,
                    )
                    .await
                } else {
                    let budget = timing.per_voice.min(left);
                    let message = discussion_take_message(topic, &takes);
                    match compose_bounded(
                        composer,
                        workgraph_dir,
                        &voice.session_ref,
                        &voice.agent_id,
                        &message,
                        budget,
                        family_roster,
                    )
                    .await
                    {
                        Some(text) => {
                            send_discussion_canonical(
                                workgraph_dir,
                                &delivery_id,
                                &voice.bot_id,
                                chat_id,
                                &text,
                                sink,
                            )
                            .await
                        }
                        None => {
                            send_discussion_canonical(
                                workgraph_dir,
                                &delivery_id,
                                &voice.bot_id,
                                chat_id,
                                "",
                                sink,
                            )
                            .await
                        }
                    }
                }
            }
        };
        match resolution {
            DiscussionPartResolution::Delivered(text) => takes.push(Take {
                bot_id: voice.bot_id.clone(),
                display_name: voice.display_name.clone(),
                text,
            }),
            DiscussionPartResolution::Skipped => {
                skipped.push(voice.bot_id.clone());
            }
            DiscussionPartResolution::Blocked => {
                skipped.push(voice.bot_id.clone());
                // Without a stable physical key there is no replay to order, so
                // preserve the legacy best-effort skip-and-continue behavior.
                // Durable rounds stop here: a later voice must never land before
                // this canonical part has transport confirmation.
                if !delivery_id.is_empty() {
                    delivery_barrier = true;
                }
            }
        }
    }

    // Synthesis closes the round — but only when the discussion had substance:
    // at least two voices OTHER than the synthesizer actually weighed in. A round
    // where only the synthesizer (or a single peer) spoke needs no
    // "the consensus is…".
    let non_synth_takes = takes.iter().filter(|t| t.bot_id != synthesizer_bot).count();
    let mut synthesis = None;
    if !delivery_barrier && non_synth_takes >= 2 {
        if let Some(voice) = voices.iter().find(|v| v.bot_id == synthesizer_bot) {
            let delivery_id = discussion_delivery_id(physical_turn_key, "synthesis", &voice.bot_id);
            let state =
                canonical_delivery_state(workgraph_dir, &delivery_id, &voice.bot_id, chat_id)?;
            let resolution = match state {
                CanonicalDeliveryState::Confirmed(text) => {
                    DiscussionPartResolution::Delivered(text)
                }
                CanonicalDeliveryState::Skipped => DiscussionPartResolution::Skipped,
                CanonicalDeliveryState::Ready(text) => {
                    send_discussion_canonical(
                        workgraph_dir,
                        &delivery_id,
                        &voice.bot_id,
                        chat_id,
                        &text,
                        sink,
                    )
                    .await
                }
                CanonicalDeliveryState::Pending | CanonicalDeliveryState::Unavailable => {
                    DiscussionPartResolution::Blocked
                }
                CanonicalDeliveryState::Missing => {
                    let left = remaining(timing.overall, started);
                    // Give the wrap-up a full per-voice budget even at the tail
                    // of the round (it is the payload the discussion ask wanted);
                    // only skip it when the round budget is fully spent.
                    let budget = if left.is_zero() {
                        Duration::ZERO
                    } else {
                        timing.per_voice.min(left.max(Duration::from_secs(1)))
                    };
                    if budget.is_zero() {
                        send_discussion_canonical(
                            workgraph_dir,
                            &delivery_id,
                            &voice.bot_id,
                            chat_id,
                            "",
                            sink,
                        )
                        .await
                    } else {
                        let message = synthesis_message(topic, &takes);
                        match compose_bounded(
                            composer,
                            workgraph_dir,
                            &voice.session_ref,
                            &voice.agent_id,
                            &message,
                            budget,
                            family_roster,
                        )
                        .await
                        {
                            Some(text) => {
                                send_discussion_canonical(
                                    workgraph_dir,
                                    &delivery_id,
                                    &voice.bot_id,
                                    chat_id,
                                    &text,
                                    sink,
                                )
                                .await
                            }
                            None => {
                                send_discussion_canonical(
                                    workgraph_dir,
                                    &delivery_id,
                                    &voice.bot_id,
                                    chat_id,
                                    "",
                                    sink,
                                )
                                .await
                            }
                        }
                    }
                }
            };
            synthesis = match resolution {
                DiscussionPartResolution::Delivered(text) => Some(text),
                DiscussionPartResolution::Skipped | DiscussionPartResolution::Blocked => None,
            };
        }
    }

    Ok(DiscussionOutcome {
        takes,
        synthesis,
        skipped,
    })
}

/// The planned shape of a discussion round for a given roster — the pure core of
/// the `wg telegram discuss --dry-run` diagnostic. `take_voices` is the roster
/// order the takes are contributed in; `synthesizer` is the configured persona
/// that closes, when present. This does NOT decide *whether* a round runs
/// (that is [`is_discussion_ask`](super::telegram_group::is_discussion_ask) over
/// a collective election) — only what the round would look like once it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionPlan {
    pub take_voices: Vec<String>,
    pub synthesizer: Option<String>,
}

/// Plan a round over `roster_bot_ids` (roster order). `synthesizer_bot` comes
/// from the project-local coordination owner; an absent or unconfigured owner
/// yields no synthesis rather than inventing a persona.
pub fn plan_round(roster_bot_ids: &[String], synthesizer_bot: Option<&str>) -> DiscussionPlan {
    let synthesizer = synthesizer_bot.and_then(|want| {
        roster_bot_ids
            .iter()
            .find(|id| id.eq_ignore_ascii_case(want))
            .cloned()
    });
    DiscussionPlan {
        take_voices: roster_bot_ids.to_vec(),
        synthesizer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::tempdir;

    /// A composer that returns a canned take per agent id, or errors for agents
    /// mapped to `None` (a "dead session").
    struct FakeComposer {
        replies: HashMap<String, Option<String>>,
    }

    #[async_trait]
    impl ReplyComposer for FakeComposer {
        async fn compose(
            &self,
            _dir: &Path,
            _session_ref: &str,
            agent_id: &str,
            _message: &str,
        ) -> Result<String> {
            match self.replies.get(agent_id) {
                Some(Some(text)) => Ok(text.clone()),
                Some(None) => anyhow::bail!("dead session for {agent_id}"),
                None => anyhow::bail!("no fake reply for {agent_id}"),
            }
        }
    }

    /// A sink that records every (bot_id, text) it is asked to send.
    #[derive(Default)]
    struct RecordingSink {
        sent: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl ReplySink for RecordingSink {
        async fn send(&self, bot_id: &str, _chat_id: &str, text: &str) -> Result<Option<String>> {
            self.sent
                .lock()
                .unwrap()
                .push((bot_id.to_string(), text.to_string()));
            Ok(Some("1".to_string()))
        }
    }

    fn voice(bot: &str) -> DiscussionVoice {
        DiscussionVoice {
            bot_id: bot.to_string(),
            display_name: format!("Display {bot}"),
            agent_id: bot.to_string(),
            session_ref: format!("session-{bot}"),
        }
    }

    fn roster() -> Vec<DiscussionVoice> {
        vec![voice("nora"), voice("bruno"), voice("mira"), voice("otto")]
    }

    fn generous_timing() -> DiscussionTiming {
        // Large budgets so timeouts never interfere — the fake composer resolves
        // instantly, so the Err path (not the clock) drives every skip here.
        DiscussionTiming {
            per_voice: Duration::from_secs(600),
            overall: Duration::from_secs(3600),
        }
    }

    fn family_roster() -> FamilyVoiceRoster {
        FamilyVoiceRoster::from_names(
            ["Nora", "Bruno", "Coach Mira", "Otto"],
            ["Household Member"],
        )
    }

    #[tokio::test]
    async fn same_turn_refire_does_not_recompose() {
        struct CountingComposer {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl ReplyComposer for CountingComposer {
            async fn compose(
                &self,
                _dir: &Path,
                _session_ref: &str,
                agent_id: &str,
                message: &str,
            ) -> Result<String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if message.contains("close the discussion") {
                    Ok("The shared direction is clear.".to_string())
                } else {
                    Ok(format!("A grounded take from {agent_id}."))
                }
            }
        }

        let dir = tempdir().unwrap();
        let voices = vec![voice("voice-a"), voice("voice-b"), voice("voice-c")];
        let family = FamilyVoiceRoster::from_names(
            voices.iter().map(|voice| voice.display_name.as_str()),
            ["Fixture Member"],
        );
        let composer = CountingComposer {
            calls: AtomicUsize::new(0),
        };
        let sink = RecordingSink::default();

        let first = run_discussion_round(
            dir.path(),
            "which option fits best?",
            &voices,
            "voice-c",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-refire",
            generous_timing(),
        )
        .await
        .unwrap();
        assert_eq!(first.takes.len(), 3);
        assert!(first.synthesis.is_some());
        assert_eq!(composer.calls.load(Ordering::SeqCst), 4);
        assert_eq!(sink.sent.lock().unwrap().len(), 4);

        let replay = run_discussion_round(
            dir.path(),
            "which option fits best?",
            &voices,
            "voice-c",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-refire",
            generous_timing(),
        )
        .await
        .unwrap();

        assert_eq!(
            composer.calls.load(Ordering::SeqCst),
            4,
            "a confirmed physical-turn replay must not invoke any composer",
        );
        assert_eq!(
            sink.sent.lock().unwrap().len(),
            4,
            "a confirmed physical-turn replay must stay transport-silent",
        );
        assert_eq!(
            replay, first,
            "the replay outcome must reuse the exact canonical delivered round",
        );
    }

    #[tokio::test]
    async fn failed_send_reuses_canonical_take_bytes() {
        struct ChangingComposer {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl ReplyComposer for ChangingComposer {
            async fn compose(
                &self,
                _dir: &Path,
                _session_ref: &str,
                _agent_id: &str,
                _message: &str,
            ) -> Result<String> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(if call == 0 {
                    "**First canonical take.**".to_string()
                } else {
                    "A different recomposed take.".to_string()
                })
            }
        }

        struct FailFirstSink {
            attempts: AtomicUsize,
            texts: Mutex<Vec<String>>,
            canonical_path: PathBuf,
            canonical_seen_before_transport: AtomicBool,
        }
        #[async_trait]
        impl ReplySink for FailFirstSink {
            async fn send(
                &self,
                _bot_id: &str,
                _chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                let canonical = std::fs::read_to_string(&self.canonical_path)
                    .expect("canonical bytes must be durable before transport");
                assert_eq!(
                    canonical, text,
                    "transport must receive the exact persisted canonical bytes",
                );
                self.canonical_seen_before_transport
                    .store(true, Ordering::SeqCst);
                self.texts.lock().unwrap().push(text.to_string());
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    anyhow::bail!("stub discussion transport failure");
                }
                Ok(Some("confirmed-take".to_string()))
            }
        }

        let dir = tempdir().unwrap();
        let voices = vec![voice("voice-retry")];
        let family = FamilyVoiceRoster::from_names(
            voices.iter().map(|voice| voice.display_name.as_str()),
            ["Fixture Member"],
        );
        let composer = ChangingComposer {
            calls: AtomicUsize::new(0),
        };
        let physical_turn_key = "physical-discussion-send-retry";
        let chat_id = "-100-fixture";
        let delivery_id = discussion_delivery_id(physical_turn_key, "take", "voice-retry");
        let digest = super::super::telegram_conversation::durable_telegram_digest_v1(
            "telegram-delivery-claim",
            &[&delivery_id, "voice-retry", chat_id],
        );
        let sink = FailFirstSink {
            attempts: AtomicUsize::new(0),
            texts: Mutex::new(Vec::new()),
            canonical_path: dir
                .path()
                .join("telegram-deliveries")
                .join(format!("{digest}.canonical")),
            canonical_seen_before_transport: AtomicBool::new(false),
        };

        let first = run_discussion_round(
            dir.path(),
            "what should we choose?",
            &voices,
            "missing-synthesizer",
            &composer,
            &family,
            &sink,
            chat_id,
            physical_turn_key,
            generous_timing(),
        )
        .await
        .unwrap();
        assert!(first.takes.is_empty());
        assert_eq!(first.skipped, vec!["voice-retry".to_string()]);
        assert!(
            sink.canonical_seen_before_transport.load(Ordering::SeqCst),
            "the failed transport must observe canonical guarded bytes already on disk",
        );

        let retry = run_discussion_round(
            dir.path(),
            "what should we choose?",
            &voices,
            "missing-synthesizer",
            &composer,
            &family,
            &sink,
            chat_id,
            physical_turn_key,
            generous_timing(),
        )
        .await
        .unwrap();
        let replay = run_discussion_round(
            dir.path(),
            "what should we choose?",
            &voices,
            "missing-synthesizer",
            &composer,
            &family,
            &sink,
            chat_id,
            physical_turn_key,
            generous_timing(),
        )
        .await
        .unwrap();

        assert_eq!(
            composer.calls.load(Ordering::SeqCst),
            1,
            "retry and confirmed refire must not recompose canonical bytes",
        );
        let texts = sink.texts.lock().unwrap().clone();
        assert_eq!(
            texts,
            vec!["First canonical take.", "First canonical take."]
        );
        assert_eq!(retry.takes[0].text, "First canonical take.");
        assert_eq!(replay.takes, retry.takes);
    }

    #[tokio::test]
    async fn partial_retry_context_contains_only_delivered_canonical_takes() {
        #[derive(Default)]
        struct ContextComposer {
            calls: Mutex<HashMap<String, usize>>,
            prompts: Mutex<Vec<(String, String)>>,
        }
        #[async_trait]
        impl ReplyComposer for ContextComposer {
            async fn compose(
                &self,
                _dir: &Path,
                _session_ref: &str,
                agent_id: &str,
                message: &str,
            ) -> Result<String> {
                self.prompts
                    .lock()
                    .unwrap()
                    .push((agent_id.to_string(), message.to_string()));
                let call = {
                    let mut calls = self.calls.lock().unwrap();
                    let entry = calls.entry(agent_id.to_string()).or_default();
                    let call = *entry;
                    *entry += 1;
                    call
                };
                match (agent_id, call) {
                    ("voice-alpha", 0) => Ok("Alpha-original.".to_string()),
                    ("voice-alpha", _) => Ok("Alpha-recomposed.".to_string()),
                    ("voice-delta", 0) => Ok("Delta-original.".to_string()),
                    ("voice-delta", _) => Ok("Delta-recomposed.".to_string()),
                    ("voice-beta", 0) => Ok("Beta-original.".to_string()),
                    ("voice-beta", _) => Ok("Beta-recomposed.".to_string()),
                    ("voice-gamma", _) => {
                        if message.contains("Alpha-original.")
                            && message.contains("Delta-original.")
                            && message.contains("Beta-original.")
                            && !message.contains("recomposed")
                        {
                            if message.contains("close the discussion") {
                                Ok("Alpha-original, Delta-original, Beta-original, and \
                                     Gamma-original are the visible takes."
                                    .to_string())
                            } else {
                                Ok("Gamma-original builds on Alpha-original, Delta-original, \
                                     and Beta-original."
                                    .to_string())
                            }
                        } else {
                            Ok("I reacted to recomposed or unsent words.".to_string())
                        }
                    }
                    _ => anyhow::bail!("unexpected discussion voice {agent_id}"),
                }
            }
        }

        #[derive(Default)]
        struct FailBetaOnceSink {
            failed_beta: AtomicBool,
            attempts: Mutex<Vec<(String, String)>>,
            delivered: Mutex<Vec<(String, String)>>,
        }
        #[async_trait]
        impl ReplySink for FailBetaOnceSink {
            async fn send(
                &self,
                bot_id: &str,
                _chat_id: &str,
                text: &str,
            ) -> Result<Option<String>> {
                self.attempts
                    .lock()
                    .unwrap()
                    .push((bot_id.to_string(), text.to_string()));
                if bot_id == "voice-beta" && !self.failed_beta.swap(true, Ordering::SeqCst) {
                    anyhow::bail!("stub beta transport failure");
                }
                self.delivered
                    .lock()
                    .unwrap()
                    .push((bot_id.to_string(), text.to_string()));
                Ok(Some(format!("confirmed-{bot_id}")))
            }
        }

        let dir = tempdir().unwrap();
        let voices = vec![
            voice("voice-alpha"),
            voice("voice-delta"),
            voice("voice-beta"),
            voice("voice-gamma"),
        ];
        let family = FamilyVoiceRoster::from_names(
            voices.iter().map(|voice| voice.display_name.as_str()),
            ["Fixture Member"],
        );
        let composer = ContextComposer::default();
        let sink = FailBetaOnceSink::default();

        let first = run_discussion_round(
            dir.path(),
            "how should we combine these ideas?",
            &voices,
            "voice-gamma",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-partial-retry",
            generous_timing(),
        )
        .await
        .unwrap();
        assert_eq!(
            first
                .takes
                .iter()
                .map(|take| take.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha-original.", "Delta-original."],
        );
        assert_eq!(
            first.skipped,
            vec!["voice-beta".to_string(), "voice-gamma".to_string()],
        );
        assert_eq!(
            composer.calls.lock().unwrap().get("voice-gamma").copied(),
            None,
            "a failed middle transport must stop the round before a later composer runs",
        );
        let delivered_before_retry = sink.delivered.lock().unwrap().len();

        let retry = run_discussion_round(
            dir.path(),
            "how should we combine these ideas?",
            &voices,
            "voice-gamma",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-partial-retry",
            generous_timing(),
        )
        .await
        .unwrap();

        let calls = composer.calls.lock().unwrap().clone();
        assert_eq!(calls.get("voice-alpha"), Some(&1));
        assert_eq!(calls.get("voice-delta"), Some(&1));
        assert_eq!(calls.get("voice-beta"), Some(&1));
        assert_eq!(calls.get("voice-gamma"), Some(&2));
        let gamma_retry_prompt = composer
            .prompts
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(agent, prompt)| {
                agent == "voice-gamma" && !prompt.contains("close the discussion")
            })
            .map(|(_, prompt)| prompt.clone())
            .unwrap();
        assert!(gamma_retry_prompt.contains("Alpha-original."));
        assert!(gamma_retry_prompt.contains("Delta-original."));
        assert!(gamma_retry_prompt.contains("Beta-original."));
        assert!(!gamma_retry_prompt.contains("recomposed"));

        let delivered = sink.delivered.lock().unwrap().clone();
        assert_eq!(
            &delivered[delivered_before_retry..],
            &[
                ("voice-beta".to_string(), "Beta-original.".to_string()),
                (
                    "voice-gamma".to_string(),
                    "Gamma-original builds on Alpha-original, Delta-original, and \
                     Beta-original."
                        .to_string(),
                ),
                (
                    "voice-gamma".to_string(),
                    "Alpha-original, Delta-original, Beta-original, and Gamma-original \
                     are the visible takes."
                        .to_string(),
                ),
            ],
            "retry may deliver only canonical bytes and replies grounded in takes already confirmed",
        );
        assert_eq!(
            retry
                .takes
                .iter()
                .map(|take| take.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Alpha-original.",
                "Delta-original.",
                "Beta-original.",
                "Gamma-original builds on Alpha-original, Delta-original, and Beta-original.",
            ],
        );
        assert_eq!(
            retry.synthesis.as_deref(),
            Some(
                "Alpha-original, Delta-original, Beta-original, and Gamma-original \
                 are the visible takes.",
            ),
        );

        let sends_before_replay = sink.attempts.lock().unwrap().len();
        let replay = run_discussion_round(
            dir.path(),
            "how should we combine these ideas?",
            &voices,
            "voice-gamma",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-partial-retry",
            generous_timing(),
        )
        .await
        .unwrap();
        assert_eq!(
            composer.calls.lock().unwrap().clone(),
            calls,
            "a confirmed replay must stay composer-silent",
        );
        assert_eq!(
            sink.attempts.lock().unwrap().len(),
            sends_before_replay,
            "a confirmed replay must stay transport-silent",
        );
        assert_eq!(replay, retry);
    }

    #[tokio::test]
    async fn same_turn_compose_and_timeout_skips_are_durable_without_blocking_later_voices() {
        #[derive(Default)]
        struct SkipThenChangeComposer {
            calls: Mutex<HashMap<String, usize>>,
        }
        #[async_trait]
        impl ReplyComposer for SkipThenChangeComposer {
            async fn compose(
                &self,
                _dir: &Path,
                _session_ref: &str,
                agent_id: &str,
                _message: &str,
            ) -> Result<String> {
                let call = {
                    let mut calls = self.calls.lock().unwrap();
                    let entry = calls.entry(agent_id.to_string()).or_default();
                    let call = *entry;
                    *entry += 1;
                    call
                };
                match (agent_id, call) {
                    ("voice-skipped", 0) => {
                        anyhow::bail!("stub first compose failure")
                    }
                    ("voice-skipped", _) => Ok("A resurrected take must never land.".to_string()),
                    ("voice-timeout", 0) => {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        Ok("A timed-out take must never land.".to_string())
                    }
                    ("voice-timeout", _) => {
                        Ok("A resurrected timeout must never land.".to_string())
                    }
                    ("voice-later", _) => Ok("The later take can still land.".to_string()),
                    _ => anyhow::bail!("unexpected discussion voice {agent_id}"),
                }
            }
        }

        let dir = tempdir().unwrap();
        let voices = vec![
            voice("voice-skipped"),
            voice("voice-timeout"),
            voice("voice-later"),
        ];
        let family = FamilyVoiceRoster::from_names(
            voices.iter().map(|voice| voice.display_name.as_str()),
            ["Fixture Member"],
        );
        let composer = SkipThenChangeComposer::default();
        let sink = RecordingSink::default();

        let first = run_discussion_round(
            dir.path(),
            "which option should we take?",
            &voices,
            "missing-synthesizer",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-durable-skip",
            DiscussionTiming {
                per_voice: Duration::from_millis(5),
                overall: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            first.skipped,
            vec!["voice-skipped".to_string(), "voice-timeout".to_string()],
        );
        assert_eq!(
            first
                .takes
                .iter()
                .map(|take| take.bot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["voice-later"],
        );
        let sends_after_first = sink.sent.lock().unwrap().len();

        let replay = run_discussion_round(
            dir.path(),
            "which option should we take?",
            &voices,
            "missing-synthesizer",
            &composer,
            &family,
            &sink,
            "-100-fixture",
            "physical-discussion-durable-skip",
            DiscussionTiming {
                per_voice: Duration::from_millis(5),
                overall: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            composer.calls.lock().unwrap().get("voice-skipped").copied(),
            Some(1),
            "a durable skip must never re-invoke a changing composer",
        );
        assert_eq!(
            composer.calls.lock().unwrap().get("voice-timeout").copied(),
            Some(1),
            "a durable timeout must never re-invoke a changing composer",
        );
        assert_eq!(sink.sent.lock().unwrap().len(), sends_after_first);
        assert_eq!(replay, first);
    }

    #[tokio::test]
    async fn full_round_sequences_all_voices_then_synthesizes() {
        let replies = [
            ("nora", Some("Pasta sounds great.")),
            ("bruno", Some("Agreed, I'll cook.")),
            ("mira", Some("Add a salad and I'm in.")),
            ("otto", Some("Works for me.")),
        ]
        .iter()
        .map(|(a, r)| (a.to_string(), r.map(|s| s.to_string())))
        .collect();
        let composer = FakeComposer { replies };
        let sink = RecordingSink::default();

        let outcome = run_discussion_round(
            Path::new("."),
            "what should we do for dinner?",
            &roster(),
            "otto",
            &composer,
            &family_roster(),
            &sink,
            "-100",
            "",
            generous_timing(),
        )
        .await
        .unwrap();

        // Four takes landed, in roster order.
        assert_eq!(
            outcome
                .takes
                .iter()
                .map(|t| t.bot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["nora", "bruno", "mira", "otto"],
        );
        assert!(outcome.skipped.is_empty());
        // Synthesis fired (≥2 non-otto takes) and was Otto's 5th send.
        assert!(outcome.synthesis.is_some());
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[4].0, "otto");
    }

    #[tokio::test]
    async fn later_voice_sees_earlier_takes_in_its_prompt() {
        // The message a voice composes against must embed the prior takes so it
        // can react — proven via the pure builder the runner feeds the composer.
        let prior = vec![Take {
            bot_id: "agent-7f3".to_string(),
            display_name: "Morning Compass".to_string(),
            text: "Pasta!".to_string(),
        }];
        let msg = discussion_take_message("dinner?", &prior);
        assert!(msg.contains("Morning Compass: Pasta!"));
        assert!(!msg.contains("Agent-7f3"));
        // The first voice's message has no prior-takes block.
        let first = discussion_take_message("dinner?", &[]);
        assert!(first.contains("first to weigh in"));
        assert!(!first.contains("said so far"));
    }

    #[tokio::test]
    async fn dead_session_is_skipped_not_blocking() {
        // Bruno's session is dead — the round must skip him and continue.
        let replies = [
            ("nora", Some("I say yes.")),
            ("bruno", None), // dead session
            ("mira", Some("Me too.")),
            ("otto", Some("Sounds good.")),
        ]
        .iter()
        .map(|(a, r)| (a.to_string(), r.map(|s| s.to_string())))
        .collect();
        let composer = FakeComposer { replies };
        let sink = RecordingSink::default();

        let outcome = run_discussion_round(
            Path::new("."),
            "beach this weekend?",
            &roster(),
            "otto",
            &composer,
            &family_roster(),
            &sink,
            "-100",
            "",
            generous_timing(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.skipped, vec!["bruno".to_string()]);
        assert_eq!(
            outcome
                .takes
                .iter()
                .map(|t| t.bot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["nora", "mira", "otto"],
        );
        // No glitch/error text was ever sent for bruno.
        let sent = sink.sent.lock().unwrap();
        assert!(sent.iter().all(|(bot, _)| bot != "bruno"));
        // Two non-otto takes (nora, mira) → synthesis still fires.
        assert!(outcome.synthesis.is_some());
    }

    #[tokio::test]
    async fn synthesis_needs_at_least_two_other_voices() {
        // Only Nora answers; Bruno and Mira are dead, Otto answers his peer take.
        let replies = [
            ("nora", Some("Just me here.")),
            ("bruno", None),
            ("mira", None),
            ("otto", Some("Noted.")),
        ]
        .iter()
        .map(|(a, r)| (a.to_string(), r.map(|s| s.to_string())))
        .collect();
        let composer = FakeComposer { replies };
        let sink = RecordingSink::default();

        let outcome = run_discussion_round(
            Path::new("."),
            "thoughts on the plan?",
            &roster(),
            "otto",
            &composer,
            &family_roster(),
            &sink,
            "-100",
            "",
            generous_timing(),
        )
        .await
        .unwrap();

        // Only one non-otto take (nora) → NO synthesis.
        assert!(outcome.synthesis.is_none());
        assert_eq!(
            outcome.skipped,
            vec!["bruno".to_string(), "mira".to_string()]
        );
        // Otto spoke exactly once (his peer take), never a wrap-up.
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.iter().filter(|(bot, _)| bot == "otto").count(), 1);
    }

    #[tokio::test]
    async fn synthesis_counts_non_synthesizer_takes_only() {
        // Two non-otto voices land → synthesis fires even though otto's own peer
        // take also counts toward the total take list.
        let replies = [
            ("nora", Some("A.")),
            ("bruno", Some("B.")),
            ("mira", None),
            ("otto", Some("O.")),
        ]
        .iter()
        .map(|(a, r)| (a.to_string(), r.map(|s| s.to_string())))
        .collect();
        let composer = FakeComposer { replies };
        let sink = RecordingSink::default();

        let outcome = run_discussion_round(
            Path::new("."),
            "movie night?",
            &roster(),
            "otto",
            &composer,
            &family_roster(),
            &sink,
            "-100",
            "",
            generous_timing(),
        )
        .await
        .unwrap();

        assert!(outcome.synthesis.is_some());
        // Otto sent twice: his peer take and the wrap-up.
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.iter().filter(|(bot, _)| bot == "otto").count(), 2);
    }

    #[tokio::test]
    async fn every_take_and_synthesis_is_family_guarded_before_send() {
        let replies = [
            ("nora", Some("Nora 💬 **Pasta** sounds great.")),
            ("bruno", Some("Pasta sounds great.")),
            (
                "mira",
                Some("A salad works. Zephyra will join us. Otto's got this one."),
            ),
            (
                "otto",
                Some(concat!(
                    "I'll pull that from the live gateway. ",
                    "Dispatcher healthy. W29 is still a draft.",
                )),
            ),
        ]
        .iter()
        .map(|(a, r)| (a.to_string(), r.map(|s| s.to_string())))
        .collect();
        let composer = FakeComposer { replies };
        let sink = RecordingSink::default();

        let outcome = run_discussion_round(
            Path::new("."),
            "what should we do for dinner?",
            &roster(),
            "otto",
            &composer,
            &family_roster(),
            &sink,
            "-100",
            "",
            generous_timing(),
        )
        .await
        .unwrap();

        let sent = sink.sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            5,
            "four takes and the synthesis should still land"
        );
        for (_, text) in sent.iter() {
            assert!(
                !text.contains("Nora 💬"),
                "self-attribution leaked: {text:?}"
            );
            assert!(
                !text.contains("Zephyra"),
                "off-roster claim leaked: {text:?}"
            );
            assert!(
                !text.contains("got this one"),
                "persona handoff leaked: {text:?}"
            );
            assert!(
                !["system", "gateway", "pipeline"]
                    .iter()
                    .any(|word| text.to_lowercase().contains(word)),
                "infrastructure narration leaked: {text:?}",
            );
            assert!(!text.contains("W29"), "machine shorthand leaked: {text:?}");
            assert!(!text.contains("**"), "markdown leaked: {text:?}");
        }
        assert_eq!(
            sent[1].1, "Pasta sounds great.",
            "safe copy must stay unchanged"
        );
        assert!(
            outcome
                .takes
                .iter()
                .all(|take| !take.text.contains("Zephyra"))
        );
        assert!(
            !outcome
                .synthesis
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains("gateway"),
            "the stored synthesis must match the guarded delivery",
        );
    }

    #[test]
    fn clamp_take_truncates_on_word_boundary() {
        let long = "word ".repeat(400); // 2000 chars
        let clamped = clamp_take(&long);
        assert!(clamped.chars().count() <= TAKE_CHAR_CAP + 1);
        assert!(clamped.ends_with('…'));
        // Short text is returned untouched (trimmed).
        assert_eq!(clamp_take("  hi there  "), "hi there");
    }

    #[test]
    fn opaque_synthesizer_id_is_configured() {
        let plan = plan_round(
            &[
                "ember".to_string(),
                "quartz".to_string(),
                "harbor".to_string(),
            ],
            Some("harbor"),
        );
        assert_eq!(plan.take_voices.len(), 3);
        assert_eq!(plan.synthesizer.as_deref(), Some("harbor"));

        // No configured coordination owner → no synthesizer.
        let plan = plan_round(&["ember".to_string(), "quartz".to_string()], None);
        assert_eq!(plan.synthesizer, None);
    }
}
