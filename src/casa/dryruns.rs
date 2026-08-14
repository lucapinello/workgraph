//! Casa's two dry-run diagnostics: `wg telegram conversation --dry-run` and the voice
//! equivalent.
//!
//! Extracted from `commands/telegram.rs` as slice 5 of the Casa/upstream split (see
//! docs/UPSTREAM-DIVERGENCE.md). Neither exists upstream. Both answer "what WOULD this turn
//! do" without sending anything, which is a Casa question about a Casa pipeline.
//!
//! Clean by the slice test: no private helpers dragged, none shared, no tests in their
//! file, and nothing exposed there.

use anyhow::{Context, Result};
use std::path::Path;
use worksgood::notify::ownership;

use crate::commands::telegram::{load_telegram_config, project_root};

pub fn run_conversation_dryrun(
    workgraph_dir: &Path,
    channel: &str,
    chat: &str,
    sender: &str,
    message: &str,
    group: bool,
    session_reply: Option<&str>,
    composed_reply: Option<&str>,
    compose: bool,
    compose_error: bool,
    json: bool,
) -> Result<()> {
    use std::sync::{Arc, Mutex};
    use worksgood::notify::telegram_conversation as convo;

    let config = if composed_reply.is_some() {
        load_telegram_config().context(
            "--composed-reply requires a project-local .wg/notify.toml so the real finalizer is reachable",
        )?
    } else {
        load_telegram_config().unwrap_or_default()
    };
    let entry = if group {
        convo::Entry::GroupElected
    } else {
        convo::Entry::Direct
    };

    // Bind an ephemeral session to the addressed agent so the plan resolves to
    // `converse` and the turn has somewhere to land — needed for the fixture
    // round-trip AND both compose modes.
    if session_reply.is_some() || composed_reply.is_some() || compose || compose_error {
        let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
        let coordination_owner = owner_map.owner_for_domain(ownership::Domain::Coordination);
        if let Some(agent_id) =
            convo::agent_for_channel_with_default(&config, channel, coordination_owner)
        {
            let uuid = worksgood::chat_sessions::create_session(
                workgraph_dir,
                worksgood::chat_sessions::SessionKind::Interactive,
                &[],
                None,
            )?;
            worksgood::chat_sessions::bind_agent(workgraph_dir, &agent_id, &uuid)?;
        }
    }

    let plan = convo::plan_conversation(workgraph_dir, &config, channel, chat, sender, entry);

    // Recording sink: capture every send AND edit instead of hitting the
    // network. Sends return a monotonic fake message id so the ack-edit path
    // works; edits are recorded so the printed output shows the final text.
    #[derive(Clone, Default)]
    struct DryRunSink {
        sent: Arc<Mutex<Vec<(String, String, String)>>>,
        edited: Arc<Mutex<Vec<(String, String, String, String)>>>,
        next_id: Arc<Mutex<u64>>,
    }
    #[async_trait::async_trait]
    impl convo::ReplySink for DryRunSink {
        async fn send(&self, bot_id: &str, chat_id: &str, text: &str) -> Result<Option<String>> {
            self.sent.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                text.to_string(),
            ));
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            Ok(Some(n.to_string()))
        }
        async fn edit(
            &self,
            bot_id: &str,
            chat_id: &str,
            message_id: &str,
            text: &str,
        ) -> Result<()> {
            self.edited.lock().unwrap().push((
                bot_id.to_string(),
                chat_id.to_string(),
                message_id.to_string(),
                text.to_string(),
            ));
            Ok(())
        }
    }
    let sink = DryRunSink::default();

    // Credential-free composed-turn fixture used only by smoke tests. Unlike
    // `--session-reply`, this enters through ReplyComposer and therefore drives
    // every production finalizer before the recording sink sees the reply.
    struct FixtureComposer(String);
    #[async_trait::async_trait]
    impl convo::ReplyComposer for FixtureComposer {
        async fn compose(
            &self,
            _wg: &Path,
            _session_ref: &str,
            _agent_id: &str,
            _message: &str,
        ) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    // Injected failing composer for `--compose-error`.
    struct FailingComposer;
    #[async_trait::async_trait]
    impl convo::ReplyComposer for FailingComposer {
        async fn compose(&self, _wg: &Path, _s: &str, _a: &str, _m: &str) -> Result<String> {
            anyhow::bail!("induced compose failure (--compose-error)")
        }
    }

    // Build the composer for whichever mode is active.
    let real_composer = if compose {
        Some(convo::OneshotComposer::from_config(
            worksgood::config::Config::load_merged(workgraph_dir)
                .context("--compose needs a loadable wg config")?,
        ))
    } else {
        None
    };
    let fixture_composer = composed_reply.map(|reply| FixtureComposer(reply.to_string()));
    let failing_composer = FailingComposer;
    let composer_ref: Option<&dyn convo::ReplyComposer> =
        if let Some(fixture) = fixture_composer.as_ref() {
            Some(fixture)
        } else if compose_error {
            Some(&failing_composer)
        } else {
            real_composer
                .as_ref()
                .map(|c| c as &dyn convo::ReplyComposer)
        };

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let outcome = rt.block_on(async {
        // Legacy fixture responder: only when NOT using a composer — echo the
        // canned reply to the outbox as a live session would.
        if composer_ref.is_none() {
            if let (Some(reply), convo::ConversationPlan::Converse { session_ref, .. }) =
                (session_reply, &plan)
            {
                let dir = workgraph_dir.to_path_buf();
                let session_ref = session_ref.clone();
                let reply = reply.to_string();
                tokio::spawn(async move {
                    for _ in 0..200 {
                        let inbox =
                            worksgood::chat::read_inbox_ref(&dir, &session_ref).unwrap_or_default();
                        if let Some(m) = inbox.iter().find(|m| m.role == "user") {
                            let _ = worksgood::chat::append_outbox_ref(
                                &dir,
                                &session_ref,
                                &reply,
                                &m.request_id,
                            );
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                });
            }
        }
        // Timing: in compose modes keep the ack point far out so a normal reply
        // (or a fast induced failure) lands as a single direct send that the
        // test can capture; the reply timeout bounds a genuinely hung child. In
        // the legacy fixture mode, fast timing so the dry-run doesn't stall.
        let timing = if composer_ref.is_some() {
            convo::AckTiming {
                ack_after: std::time::Duration::from_secs(60),
                reply_timeout: std::time::Duration::from_secs(120),
                poll: std::time::Duration::from_millis(50),
            }
        } else {
            convo::AckTiming {
                ack_after: std::time::Duration::from_millis(50),
                reply_timeout: std::time::Duration::from_secs(5),
                poll: std::time::Duration::from_millis(15),
            }
        };
        convo::run_conversation_turn(
            workgraph_dir,
            &plan,
            message,
            &format!("dryrun-{sender}"),
            timing,
            composer_ref,
            &sink,
        )
        .await
    })?;

    // Fold edits into the send list for output so the final text (when the ack
    // was edited in place) is always visible.
    let mut sends = sink.sent.lock().unwrap().clone();
    for (bot, chat, _mid, text) in sink.edited.lock().unwrap().iter() {
        sends.push((bot.clone(), chat.clone(), text.clone()));
    }
    if json {
        let sends_json: Vec<_> = sends
            .iter()
            .map(|(bot, chat, text)| serde_json::json!({ "bot": bot, "chat": chat, "text": text }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "entry": entry.label(),
                "kind": plan.kind_label(),
                "route": { "bot": plan.route().bot_id, "chat": plan.route().chat_id },
                "outcome": outcome.label(),
                "sends": sends_json,
            }))?
        );
    } else {
        println!(
            "route: {} via {} in {} [{}] — {}",
            entry.label(),
            plan.route().bot_id,
            plan.route().chat_id,
            plan.kind_label(),
            outcome.label(),
        );
        for (bot, chat, text) in &sends {
            println!("  send[{bot} -> {chat}]: {text}");
        }
    }
    Ok(())
}

/// `wg telegram conversation` — dry-run the conversational composer for a plain
/// message (the 1:1 and group-name-addressed path), without a live bot.
///
/// Prints the route decision (which bot answers, in which chat, how it was
/// addressed, and the plan kind) and the outbound replies the listener WOULD
/// send — captured by a recording sink, never sent, so it is credential-free.
///
/// With `--session-reply <text>` it exercises the legacy persistent-session
/// round-trip: an ephemeral session is created and bound to the addressed
/// agent (making the plan `converse`), the human's message is written to that
/// session's inbox, a fixture responder writes `<text>` to the outbox, and the
/// relayed reply is captured.
///
/// With the hidden `--composed-reply <text>` test seam, the supplied draft is
/// injected as a [`ReplyComposer`] result and therefore traverses the real
/// finalize → outbox → delivery path. This is the credential-free scratch proof
/// for post-composition guards; unlike `--session-reply`, it does not bypass the
/// finalizer.
///
/// With `--compose` it exercises the REAL fix end-to-end: the converse turn is
/// driven by the production [`OneshotComposer`] (a live one-shot `claude`
/// spawn), so the captured reply is an actual session-generated answer — no
/// fixture, no mock. This is the credential-bearing "real turn" validation.
///
/// With `--compose-error` a deliberately-failing composer is injected so the
/// fail-fast + graceful "glitched" follow-up path is provable through the built
/// binary without a live model (the induced-failure test).
#[allow(clippy::too_many_arguments)]
/// Drive the voice-note path end-to-end from a recording FILE: detect →
/// transcribe → inject (task `telegram-voice-notes`). The credential-free
/// scripted-test seam behind `wg telegram voice --file`.
///
/// `detect`: read the file and build the same [`telegram_voice::VoiceMeta`] the
/// listener parses from a real update. `transcribe`: POST the bytes to a
/// gateway — a STUB (when `stub_ok`/`stub_reason` is set) so the whole path runs
/// with NO live whisper, else the real `/conversation/transcribe`. `inject`: on
/// a transcript, print it (the body that would be injected) AND how the SAME
/// fast-lane classifier a typed line hits would route it — proving a spoken line
/// == a typed line. On failure, print the honest in-persona line the listener
/// would send.
pub fn run_voice_dryrun(
    file: &Path,
    mime: &str,
    lang: &str,
    gateway: Option<&str>,
    stub_ok: Option<&str>,
    stub_reason: Option<&str>,
    json: bool,
) -> Result<()> {
    use async_trait::async_trait;
    use worksgood::notify::fast_lane;
    use worksgood::notify::telegram_voice as tv;

    // ── detect ────────────────────────────────────────────────────────────
    let bytes = std::fs::read(file)
        .with_context(|| format!("failed to read recording file {}", file.display()))?;
    let meta = tv::VoiceMeta {
        file_id: format!("local:{}", file.display()),
        mime_type: Some(mime.to_string()),
        kind: tv::VoiceKind::Voice,
        file_size: Some(bytes.len() as u64),
    };

    // A downloader that just yields the already-read local bytes — the file IS
    // the "download". The real listener path uses the Telegram getFile impl.
    struct LocalBytes(Vec<u8>);
    #[async_trait]
    impl tv::VoiceDownloader for LocalBytes {
        async fn download_bytes(&self, _file_id: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    // A stub gateway returning a canned response, so the full detect→transcribe
    // →inject path is provable with no live whisper engine.
    struct StubGateway(serde_json::Value);
    #[async_trait]
    impl tv::TranscribeGateway for StubGateway {
        async fn transcribe(
            &self,
            _audio: &[u8],
            _mime_type: &str,
            _lang: &str,
        ) -> Result<serde_json::Value> {
            Ok(self.0.clone())
        }
    }

    let downloader = LocalBytes(bytes.clone());
    let limits = tv::VoiceLimits::default();

    let rt = tokio::runtime::Runtime::new().context("Failed to create async runtime")?;
    let result: tv::TranscribeResult = rt.block_on(async {
        if let Some(text) = stub_ok {
            let gw = StubGateway(serde_json::json!({ "ok": true, "text": text }));
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        } else if let Some(reason) = stub_reason {
            let gw = StubGateway(serde_json::json!({ "ok": false, "reason": reason }));
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        } else {
            let base = gateway
                .map(|g| g.to_string())
                .unwrap_or_else(tv::gateway_base_url);
            let gw = tv::HttpTranscribeGateway::new(base);
            tv::transcribe_voice_note(&downloader, &gw, &meta, &limits, lang).await
        }
    })?;

    // ── inject ────────────────────────────────────────────────────────────
    match result {
        tv::TranscribeResult::Transcript(text) => {
            // Route the transcript through the SAME classifier a typed line hits.
            let today = chrono::Local::now().date_naive();
            let classification = fast_lane::classify(&text, today);
            let route = match &classification {
                fast_lane::Classification::FastLane(op) => {
                    format!("fast-lane:{}", op.kind_label())
                }
                fast_lane::Classification::Ask { reason, .. } => {
                    format!("ask:{}", reason.slug())
                }
                fast_lane::Classification::Fallback(_) => "composer".to_string(),
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "outcome": "transcript",
                        "bytes": bytes.len(),
                        "mime": mime,
                        "transcript": text,
                        "injected_body": text,
                        "route": route,
                    })
                );
            } else {
                println!("detect: {} bytes, mime {}", bytes.len(), mime);
                println!("transcribe: ok");
                println!("inject: message body = {text:?}");
                println!("route (same path as typed): {route}");
            }
        }
        tv::TranscribeResult::Failed(failure) => {
            let reason = match failure {
                tv::TranscribeFailure::Unconfigured => "unconfigured",
                tv::TranscribeFailure::Silence => "silence",
                tv::TranscribeFailure::Unclear => "unclear",
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": false,
                        "outcome": "failed",
                        "reason": reason,
                        "reply": failure.message(),
                    })
                );
            } else {
                println!("detect: {} bytes, mime {}", bytes.len(), mime);
                println!("transcribe: failed ({reason})");
                println!("reply (in-persona): {}", failure.message());
            }
        }
    }
    Ok(())
}
