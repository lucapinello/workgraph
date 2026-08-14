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
use worksgood::notify::telegram_group::{
    Election, elect_responders_with_owner_map, is_discussion_ask, parse_at_mention_tokens,
    resolve_mentioned_bot,
};

use crate::commands::telegram::{human_agent_id_set, load_telegram_config, project_root};

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

// `wg telegram discuss --dry-run` joined its siblings here in slice 6: it answers the same
// shape of question — "what WOULD this turn do" — without sending anything.
/// `wg telegram discuss --dry-run` — show whether a group message would run a
/// DISCUSSION ROUND, and the planned round, without sending anything.
///
/// Runs the exact [`elect_responders_with_owner_map`] decision the listener uses, then applies
/// the same [`is_discussion_ask`] gate the live `Election::All` handler uses to
/// split a collective election into a discussion round vs independent roster
/// replies. Prints the category and, for a round, the household-authored voices
/// in contribution order plus the configured coordination-owner synthesizer.
/// This is the scripted-test seam (sibling of `wg telegram elect`): a discussion
/// ask → `discussion-round`; a plain collective greeting →
/// `collective-greeting`; a named/concierge ask → `single-voice`; small talk →
/// `silence`. Nothing is sent.
pub fn run_discuss(workgraph_dir: &Path, message: &str, json: bool) -> Result<()> {
    use worksgood::notify::telegram_discussion as discussion;
    use worksgood::notify::telegram_standup as standup;

    let config = load_telegram_config()?;
    let mention_usernames: Vec<String> = parse_at_mention_tokens(message);
    let human_count = human_agent_id_set(workgraph_dir).len();

    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let election = elect_responders_with_owner_map(
        Some("supergroup"),
        Some("-1000000000001"),
        message,
        &mention_usernames,
        None,
        // The diagnostic is always run by a human operator, never a bot.
        false,
        human_count,
        &config,
        &owner_map,
    );

    let is_discussion = is_discussion_ask(message);
    let roster_ids: Vec<String> =
        standup::load_project_roster(&project_root(workgraph_dir), &config)?
            .into_iter()
            .map(|m| m.bot_id)
            .collect();
    let synthesizer_bot = owner_map
        .owner_for_domain(ownership::Domain::Coordination)
        .and_then(|owner| resolve_mentioned_bot(owner, &config))
        .map(|bot| bot.bot_id);

    // (category, plan) — plan is Some only for a discussion round.
    let (category, plan): (&str, Option<discussion::DiscussionPlan>) = match &election {
        Election::Private => ("private", None),
        Election::Silence(_) => ("silence", None),
        Election::One { .. } => ("single-voice", None),
        Election::All { .. } => {
            if is_discussion {
                (
                    "discussion-round",
                    Some(discussion::plan_round(
                        &roster_ids,
                        synthesizer_bot.as_deref(),
                    )),
                )
            } else {
                ("collective-greeting", None)
            }
        }
    };

    if json {
        let out = serde_json::json!({
            "category": category,
            "is_discussion_ask": is_discussion,
            "voices": plan.as_ref().map(|p| p.take_voices.clone()),
            "synthesizer": plan.as_ref().and_then(|p| p.synthesizer.clone()),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    match category {
        "discussion-round" => {
            let plan = plan.unwrap();
            println!(
                "discussion round — each voice gives a short take, in order: {}",
                if plan.take_voices.is_empty() {
                    "(none configured)".to_string()
                } else {
                    plan.take_voices.join(" → ")
                },
            );
            match plan.synthesizer {
                Some(s) => {
                    println!("then {s} closes with a synthesis (only if ≥2 other voices weigh in)")
                }
                None => println!("no synthesizer configured — no closing wrap-up"),
            }
        }
        "collective-greeting" => println!(
            "collective greeting — the whole roster answers with brief independent hellos \
             (no discussion round)"
        ),
        "single-voice" => {
            println!("single voice — one bot answers (named/mention/reply/concierge); no round")
        }
        "silence" => println!("silence — no one responds; no round"),
        _ => println!("private chat — 1:1 passthrough; no round"),
    }
    Ok(())
}

// ── slice 9 ──────────────────────────────────────────────────────────────────────────
// `compose-prompt` prints the assembled prompt and spawns no model, which is the same
// question the other seams here answer: what WOULD this turn do.

/// `wg telegram compose-prompt` — print the assembled compose prompt for a
/// message, WITHOUT spawning a model or sending anything.
///
/// The credential-free scripted-test seam for the composer's CONTEXT (sibling of
/// `run_discuss` / `run_decide`). It runs the real production assembly
/// (`telegram_conversation::compose_prompt_preview` → `build_compose_prompt`), so
/// it reads the family's live grounding from disk AND the three gateway-forwarded
/// env blocks — `WG_THREAD_CONTEXT`, `WG_WEEK_CONTEXT`, `WG_MEMORY_CONTEXT`.
/// Composition itself is stubbed by stopping at the prompt, which is exactly what
/// makes this a token-free proof of what the model is handed: a scratch project
/// plus one env var shows whether durable family memory really reaches the
/// composer, and whether it is ranked below live state (docs/39 §5.3, §6).
///
/// `--json` reports which context blocks landed alongside the prompt, so a script
/// can assert on the blocks without pattern-matching prose.
pub fn run_compose_prompt(
    workgraph_dir: &Path,
    message: &str,
    agent: Option<&str>,
    session: Option<&str>,
    json: bool,
) -> Result<()> {
    use worksgood::notify::telegram_conversation;

    let owner_map = ownership::OwnerMap::load(&project_root(workgraph_dir));
    let agent_id = agent
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .or_else(|| owner_map.owner_for_domain(ownership::Domain::Coordination))
        .context("--agent is required when household.toml has no configured coordination owner")?;
    // Default the session ref to the persona id: a bound agent name resolves to
    // its session, and an unknown ref simply yields no summary/history (the
    // fresh-session prompt) rather than an error — so a scratch project works.
    let session_ref = session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(agent_id);

    let prompt = telegram_conversation::compose_prompt_preview(
        workgraph_dir,
        session_ref,
        agent_id,
        message,
    );

    if json {
        // Which forwarded blocks are present in the ASSEMBLED prompt (not merely
        // set in the environment) — that distinction is the whole point: an env
        // var the binary never reads would show `false` here.
        let out = serde_json::json!({
            "agent": agent_id,
            "session": session_ref,
            "message": message,
            "blocks": {
                "thread": prompt.contains("Recent messages in this conversation"),
                // "MEALS" since task meal-read-lane — the forwarded block carries every
                // slot the plan knows (dinners, lunches, no-cook nights), not just the
                // Dinners table. A stale needle here would report `week: false` on a
                // prompt that DOES carry the week — the exact "green stub over an unread
                // var" shape this diagnostic exists to prevent.
                "week": prompt.contains("THIS WEEK'S MEALS"),
                "memory": prompt.contains("FAMILY MEMORY"),
                "corrections": prompt.contains("correction"),
            },
            "prompt": prompt,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{prompt}");
    }
    Ok(())
}
