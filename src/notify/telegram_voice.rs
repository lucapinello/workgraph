//! Telegram voice notes → transcript → normal message (task **telegram-voice-notes**).
//!
//! A recording sent to any household bot — a `voice` note (OGG/Opus), an
//! `audio` file, or a `video_note` — is transcribed by the SAME whisper engine
//! the kiosk mic uses, then injected as if the human had TYPED the transcript.
//! After injection it flows through the UNCHANGED inbound pipeline (election,
//! single-owner routing, fast lane, conversation composer, ledger, dedupe): a
//! spoken "add milk" adds milk, a spoken "what's the plan today" gets answered.
//!
//! This module is the neutral, transport-agnostic core. It:
//!   - [`voice_meta`] — parse a Telegram `voice` / `audio` / `video_note`
//!     payload out of a raw update `message` into a [`VoiceMeta`] download
//!     handle (mirrors `telegram_photo::photo_meta`).
//!   - [`transcribe_voice_note`] — download the bytes (via the RECEIVING bot's
//!     token — the `file_id` is bot-specific) and POST them to the gateway's
//!     `/conversation/transcribe`, reusing decode (ffmpeg), whisper, the
//!     `[voice]` diagnostics line and voice-debug. Returns a [`TranscribeResult`]:
//!     either the transcript to inject, or a [`TranscribeFailure`] the caller
//!     turns into an honest, in-persona reply (never silent).
//!   - [`classify_transcribe_response`] — the pure reason→outcome mapping,
//!     shared by production and the `wg telegram voice` dry-run so both agree.
//!
//! ## Token discipline
//!
//! The `getFile` + file-download URLs embed the bot token. The production
//! [`VoiceDownloader`] (implemented on `TelegramChannel`) scrubs the token from
//! any transport error through `telegram::redact_bot_token` before it is
//! returned or logged — so a failed download never leaks the token. Only the
//! opaque `file_id` and the failure `reason` are ever logged.

use anyhow::{Context, Result};
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Voice parsing
// ---------------------------------------------------------------------------

/// Which kind of audio a message carried. Only affects the default MIME used
/// when the transport declares none (a `video_note` has no `mime_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    /// A Telegram `voice` note — a microphone recording, OGG/Opus.
    Voice,
    /// An `audio` file (a music/voice file with a duration + optional title).
    Audio,
    /// A round `video_note` — carries an audio track we transcribe.
    VideoNote,
}

impl VoiceKind {
    /// The MIME to send when the message declared none. ffmpeg sniffs the
    /// actual container anyway, so this is only a hint.
    pub fn default_mime(self) -> &'static str {
        match self {
            VoiceKind::Voice => "audio/ogg",
            VoiceKind::Audio => "audio/mpeg",
            VoiceKind::VideoNote => "video/mp4",
        }
    }

    /// Short label for logs.
    pub fn label(self) -> &'static str {
        match self {
            VoiceKind::Voice => "voice",
            VoiceKind::Audio => "audio",
            VoiceKind::VideoNote => "video_note",
        }
    }
}

/// Parsed metadata for an inbound audio recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMeta {
    /// Opaque `getFile` download handle. Bot-specific; safe to log.
    pub file_id: String,
    /// The declared MIME type (`voice.mime_type` / `audio.mime_type`), when the
    /// transport surfaced one. `None` for a `video_note`.
    pub mime_type: Option<String>,
    /// Which media object carried the audio.
    pub kind: VoiceKind,
    /// The transport-declared byte size, when present. Used for an early
    /// too-large guard before any bandwidth is spent.
    pub file_size: Option<u64>,
}

impl VoiceMeta {
    /// The MIME to advertise to the gateway: the declared type, else the
    /// per-kind default.
    pub fn effective_mime(&self) -> String {
        self.mime_type
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| self.kind.default_mime().to_string())
    }
}

/// Parse a `voice` / `audio` / `video_note` recording out of a raw Telegram
/// `message`. A message carries at most one of these; `voice` is preferred over
/// `audio` over `video_note` when (impossibly) several appear. Returns `None`
/// for text/photo/other messages.
pub fn voice_meta(message: &serde_json::Value) -> Option<VoiceMeta> {
    for (key, kind) in [
        ("voice", VoiceKind::Voice),
        ("audio", VoiceKind::Audio),
        ("video_note", VoiceKind::VideoNote),
    ] {
        if let Some(obj) = message.get(key) {
            let file_id = obj.get("file_id").and_then(|f| f.as_str())?;
            if file_id.is_empty() {
                continue;
            }
            return Some(VoiceMeta {
                file_id: file_id.to_string(),
                mime_type: obj
                    .get("mime_type")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string()),
                kind,
                file_size: obj.get("file_size").and_then(|s| s.as_u64()),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Ceiling a recording must respect before we download it. Telegram itself caps
/// bot downloads at 20 MB, so that is the hard file-size limit.
#[derive(Debug, Clone, Copy)]
pub struct VoiceLimits {
    /// Max bytes we will download / hand to the gateway.
    pub max_file_size: u64,
}

impl Default for VoiceLimits {
    fn default() -> Self {
        // 20 MB — Telegram's own bot-download ceiling.
        Self {
            max_file_size: 20 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// Transcription outcome
// ---------------------------------------------------------------------------

/// Why a recording could not become a message. Each maps to one honest,
/// in-persona, jargon-free reply (family-voice rules) the caller sends via the
/// RECEIVING bot in the SAME chat — the family never sees a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeFailure {
    /// No transcription engine is set up on the home computer yet.
    Unconfigured,
    /// The recording decoded fine but there was nothing to hear (silence /
    /// too-short) — or whisper produced an empty transcript.
    Silence,
    /// We couldn't decode / transcribe the recording (decode-failed, too-large,
    /// engine error, or a transport error reaching the gateway).
    Unclear,
}

impl TranscribeFailure {
    /// The honest, plain-language reply for this failure.
    pub fn message(self) -> &'static str {
        match self {
            TranscribeFailure::Unconfigured => {
                "I can't listen to recordings yet — voice needs a one-time setup on the home computer (casa voice-setup)."
            }
            TranscribeFailure::Silence => {
                "That recording came through with nothing I could hear — mind trying again, or just typing it?"
            }
            TranscribeFailure::Unclear => {
                "I couldn't make out that recording — mind typing it?"
            }
        }
    }
}

/// The outcome of transcribing one recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscribeResult {
    /// A usable transcript — becomes the message body and routes like typed text.
    Transcript(String),
    /// A clean failure to report in-persona.
    Failed(TranscribeFailure),
}

/// Map the gateway's `/conversation/transcribe` JSON response into a
/// [`TranscribeResult`]. Pure, so production and the `wg telegram voice`
/// dry-run classify identically.
///
/// - `{ ok:true, text:"..." }` with non-blank text → [`TranscribeResult::Transcript`].
/// - `{ ok:true }` with empty/blank text → treated as silence (nothing heard).
/// - `{ ok:false, reason:"unconfigured" }` → [`TranscribeFailure::Unconfigured`].
/// - `reason` of `silence` / `too-short` → [`TranscribeFailure::Silence`].
/// - anything else (`decode-failed`, `too-large`, `empty`, `error`, unknown) →
///   [`TranscribeFailure::Unclear`].
pub fn classify_transcribe_response(body: &serde_json::Value) -> TranscribeResult {
    let ok = body.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
    if ok {
        let text = body
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return TranscribeResult::Failed(TranscribeFailure::Silence);
        }
        return TranscribeResult::Transcript(text.to_string());
    }
    let reason = body
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let failure = match reason.as_str() {
        "unconfigured" => TranscribeFailure::Unconfigured,
        "silence" | "too-short" => TranscribeFailure::Silence,
        _ => TranscribeFailure::Unclear,
    };
    TranscribeResult::Failed(failure)
}

// ---------------------------------------------------------------------------
// Injectables (download + gateway) — real impls live in `telegram.rs`
// ---------------------------------------------------------------------------

/// Downloads an inbound recording's raw bytes by `file_id`. The production impl
/// (on `TelegramChannel`) calls `getFile` then GETs the file URL — both embed
/// the bot token, kept out of logs; tests supply a fake returning fixture bytes.
#[async_trait]
pub trait VoiceDownloader: Send + Sync {
    async fn download_bytes(&self, file_id: &str) -> Result<Vec<u8>>;
}

/// POSTs audio bytes to the gateway's `/conversation/transcribe` and returns the
/// parsed JSON body. The production impl is [`HttpTranscribeGateway`]; tests
/// supply a stub so the full detect→transcribe→inject path runs without a live
/// whisper engine.
#[async_trait]
pub trait TranscribeGateway: Send + Sync {
    async fn transcribe(
        &self,
        audio: &[u8],
        mime_type: &str,
        lang: &str,
    ) -> Result<serde_json::Value>;
}

/// The gateway base URL (`CASA_GATEWAY_URL`, default the fixed kiosk port),
/// shared with the photo pipeline. No trailing slash.
pub fn gateway_base_url() -> String {
    super::telegram_photo::gateway_base_url()
}

/// The recognition language to pass to the gateway. Read from `CASA_VOICE_LANG`
/// (the listener's configurable seam — the household's `kiosk.voiceLang` isn't
/// parsed listener-side); empty when unset, in which case the gateway applies
/// its own configured default.
pub fn voice_lang() -> String {
    std::env::var("CASA_VOICE_LANG")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// Production [`TranscribeGateway`] over HTTP against the casa gateway.
pub struct HttpTranscribeGateway {
    client: reqwest::Client,
    base_url: String,
}

impl HttpTranscribeGateway {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Build from the `CASA_GATEWAY_URL` env (or the default port).
    pub fn from_env() -> Self {
        Self::new(gateway_base_url())
    }
}

#[async_trait]
impl TranscribeGateway for HttpTranscribeGateway {
    async fn transcribe(
        &self,
        audio: &[u8],
        mime_type: &str,
        lang: &str,
    ) -> Result<serde_json::Value> {
        let mut url = format!("{}/conversation/transcribe", self.base_url);
        if !lang.is_empty() {
            url.push_str(&format!("?lang={}", urlencode(lang)));
        }
        let resp = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, mime_type)
            .body(audio.to_vec())
            .send()
            .await
            .context("POST /conversation/transcribe failed")?;
        // The endpoint answers a JSON body on every status (200 ok, 200/4xx for
        // honest reasons, 502 for a configured-but-failed engine); parse it
        // regardless of code and let `classify_transcribe_response` decide.
        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse /conversation/transcribe response")?;
        Ok(json)
    }
}

/// Minimal percent-encoding for the `lang` query value (alphanumerics, `-` and
/// `_` pass through; everything else is `%`-escaped). Avoids pulling a URL crate
/// for a value that is almost always a plain ISO code like `en` or `it`.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Transcribe one recording end-to-end: enforce the size guard, download the
/// bytes via the RECEIVING bot, POST them to the gateway, and classify the
/// response.
///
/// Returns:
/// - `Ok(TranscribeResult::Transcript(text))` — inject `text` as the body.
/// - `Ok(TranscribeResult::Failed(f))` — send `f.message()` in-persona.
/// - `Err(e)` — the download or the transport failed; the caller logs the
///   (token-scrubbed) error and sends the [`TranscribeFailure::Unclear`] reply.
///   A transport failure is never a silent drop.
pub async fn transcribe_voice_note(
    downloader: &dyn VoiceDownloader,
    gateway: &dyn TranscribeGateway,
    meta: &VoiceMeta,
    limits: &VoiceLimits,
    lang: &str,
) -> Result<TranscribeResult> {
    // Early too-large guard on the transport-declared size — refuse before
    // spending bandwidth. This is a clean, in-persona failure, not an error.
    if let Some(size) = meta.file_size {
        if size > limits.max_file_size {
            return Ok(TranscribeResult::Failed(TranscribeFailure::Unclear));
        }
    }

    let bytes = downloader.download_bytes(&meta.file_id).await?;
    if bytes.is_empty() {
        return Ok(TranscribeResult::Failed(TranscribeFailure::Silence));
    }
    if bytes.len() as u64 > limits.max_file_size {
        return Ok(TranscribeResult::Failed(TranscribeFailure::Unclear));
    }

    let body = gateway
        .transcribe(&bytes, &meta.effective_mime(), lang)
        .await?;
    Ok(classify_transcribe_response(&body))
}

/// The honest group-feed marker for a spoken line: `🎙️ <transcript>`. The
/// mirror renders `sender: 🎙️ <transcript>` so everyone sees it was SPOKEN,
/// while the ROUTED body stays the plain transcript (so "add milk" fast-lanes).
/// See the listener's group-feed mirror.
pub fn spoken_feed_body(transcript: &str) -> String {
    format!("🎙️ {transcript}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // A real Bot API voice-note update's `message` object (task fixture).
    fn voice_message() -> serde_json::Value {
        serde_json::json!({
            "message_id": 42,
            "from": { "id": 111, "is_bot": false, "username": "luca" },
            "chat": { "id": -100, "type": "supergroup" },
            "date": 1_700_000_000,
            "voice": {
                "duration": 3,
                "mime_type": "audio/ogg",
                "file_id": "AwACAgVOICE",
                "file_unique_id": "uniq",
                "file_size": 8192
            }
        })
    }

    #[test]
    fn voice_meta_parses_a_real_voice_note() {
        let msg = voice_message();
        let m = voice_meta(&msg).expect("voice note parses");
        assert_eq!(m.file_id, "AwACAgVOICE");
        assert_eq!(m.mime_type.as_deref(), Some("audio/ogg"));
        assert_eq!(m.kind, VoiceKind::Voice);
        assert_eq!(m.file_size, Some(8192));
        assert_eq!(m.effective_mime(), "audio/ogg");
    }

    #[test]
    fn voice_meta_parses_audio_and_video_note() {
        let audio = serde_json::json!({
            "audio": { "file_id": "AUDIO1", "mime_type": "audio/mpeg", "duration": 5 }
        });
        let m = voice_meta(&audio).expect("audio parses");
        assert_eq!(m.kind, VoiceKind::Audio);
        assert_eq!(m.effective_mime(), "audio/mpeg");

        // A video_note declares NO mime_type → falls back to the per-kind default.
        let vn = serde_json::json!({
            "video_note": { "file_id": "VNOTE1", "duration": 4, "length": 240 }
        });
        let m = voice_meta(&vn).expect("video_note parses");
        assert_eq!(m.kind, VoiceKind::VideoNote);
        assert_eq!(m.mime_type, None);
        assert_eq!(m.effective_mime(), "video/mp4");
    }

    #[test]
    fn voice_meta_none_for_text_and_photo() {
        let text = serde_json::json!({ "text": "hello", "message_id": 1 });
        assert!(voice_meta(&text).is_none());
        let photo = serde_json::json!({ "photo": [{ "file_id": "P", "width": 1, "height": 1 }] });
        assert!(voice_meta(&photo).is_none());
    }

    #[test]
    fn classify_ok_with_text_is_transcript() {
        let body = serde_json::json!({ "ok": true, "text": "  add milk  ", "engine": "whisper" });
        assert_eq!(
            classify_transcribe_response(&body),
            TranscribeResult::Transcript("add milk".to_string()),
        );
    }

    #[test]
    fn classify_ok_empty_text_is_silence() {
        let body = serde_json::json!({ "ok": true, "text": "   " });
        assert_eq!(
            classify_transcribe_response(&body),
            TranscribeResult::Failed(TranscribeFailure::Silence),
        );
    }

    #[test]
    fn classify_each_failure_reason() {
        let cases = [
            ("unconfigured", TranscribeFailure::Unconfigured),
            ("silence", TranscribeFailure::Silence),
            ("too-short", TranscribeFailure::Silence),
            ("decode-failed", TranscribeFailure::Unclear),
            ("too-large", TranscribeFailure::Unclear),
            ("empty", TranscribeFailure::Unclear),
            ("error", TranscribeFailure::Unclear),
            ("something-new", TranscribeFailure::Unclear),
        ];
        for (reason, want) in cases {
            let body = serde_json::json!({ "ok": false, "reason": reason });
            assert_eq!(
                classify_transcribe_response(&body),
                TranscribeResult::Failed(want),
                "reason {reason} → {want:?}",
            );
        }
    }

    #[test]
    fn failure_messages_are_in_persona_and_name_the_fix() {
        assert!(TranscribeFailure::Unconfigured
            .message()
            .contains("casa voice-setup"));
        assert!(TranscribeFailure::Unclear.message().contains("typing"));
        assert!(!TranscribeFailure::Silence.message().is_empty());
    }

    #[test]
    fn spoken_feed_body_carries_the_mic_marker() {
        assert_eq!(spoken_feed_body("add milk"), "🎙️ add milk");
    }

    // --- orchestrator with stub download + gateway ------------------------

    struct StubDownloader {
        bytes: Vec<u8>,
    }
    #[async_trait]
    impl VoiceDownloader for StubDownloader {
        async fn download_bytes(&self, _file_id: &str) -> Result<Vec<u8>> {
            Ok(self.bytes.clone())
        }
    }

    struct StubGateway {
        response: serde_json::Value,
        seen: Mutex<Option<(usize, String, String)>>,
    }
    #[async_trait]
    impl TranscribeGateway for StubGateway {
        async fn transcribe(
            &self,
            audio: &[u8],
            mime_type: &str,
            lang: &str,
        ) -> Result<serde_json::Value> {
            *self.seen.lock().unwrap() =
                Some((audio.len(), mime_type.to_string(), lang.to_string()));
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn orchestrator_returns_transcript_and_forwards_mime_and_lang() {
        let dl = StubDownloader {
            bytes: vec![1, 2, 3, 4],
        };
        let gw = StubGateway {
            response: serde_json::json!({ "ok": true, "text": "what's the plan today" }),
            seen: Mutex::new(None),
        };
        let meta = voice_meta(&voice_message()).unwrap();
        let out = transcribe_voice_note(&dl, &gw, &meta, &VoiceLimits::default(), "it")
            .await
            .unwrap();
        assert_eq!(
            out,
            TranscribeResult::Transcript("what's the plan today".to_string())
        );
        let seen = gw.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.0, 4, "all downloaded bytes POSTed");
        assert_eq!(seen.1, "audio/ogg", "declared mime forwarded");
        assert_eq!(seen.2, "it", "lang forwarded");
    }

    #[tokio::test]
    async fn orchestrator_maps_unconfigured() {
        let dl = StubDownloader { bytes: vec![9; 10] };
        let gw = StubGateway {
            response: serde_json::json!({ "ok": false, "reason": "unconfigured" }),
            seen: Mutex::new(None),
        };
        let meta = voice_meta(&voice_message()).unwrap();
        let out = transcribe_voice_note(&dl, &gw, &meta, &VoiceLimits::default(), "")
            .await
            .unwrap();
        assert_eq!(
            out,
            TranscribeResult::Failed(TranscribeFailure::Unconfigured)
        );
    }

    #[tokio::test]
    async fn orchestrator_refuses_oversize_before_download() {
        let dl = StubDownloader { bytes: vec![] };
        let gw = StubGateway {
            response: serde_json::json!({ "ok": true, "text": "never reached" }),
            seen: Mutex::new(None),
        };
        let mut meta = voice_meta(&voice_message()).unwrap();
        meta.file_size = Some(99 * 1024 * 1024);
        let out = transcribe_voice_note(&dl, &gw, &meta, &VoiceLimits::default(), "")
            .await
            .unwrap();
        assert_eq!(out, TranscribeResult::Failed(TranscribeFailure::Unclear));
        assert!(gw.seen.lock().unwrap().is_none(), "gateway never called");
    }

    // --- inject: a SPOKEN add reaches the plan seam through the SAME routing ---

    // The full detect→transcribe→INJECT contract (task Validation): a voice note
    // whose transcript is "add olive oil to the shopping list" must, once
    // injected as the message body, route through the EXACT same fast lane a
    // TYPED line hits and actually reach the plan file on disk. Proven end to end
    // against a fixture plan dir with the shared W29 fixture.
    #[tokio::test]
    async fn spoken_add_injects_and_reaches_the_plan_seam() {
        use crate::notify::fast_lane::{self, FastLaneOp, FastLaneResult};

        // 1. detect: a real voice-note update → download handle.
        let msg = voice_message();
        let meta = voice_meta(&msg).expect("voice note parses");

        // 2. transcribe: the gateway (stubbed — no live whisper) hears the add.
        let dl = StubDownloader { bytes: vec![1, 2, 3] };
        let gw = StubGateway {
            response: serde_json::json!({
                "ok": true,
                "text": "add olive oil to the shopping list"
            }),
            seen: Mutex::new(None),
        };
        let transcript = match transcribe_voice_note(&dl, &gw, &meta, &VoiceLimits::default(), "")
            .await
            .unwrap()
        {
            TranscribeResult::Transcript(t) => t,
            other => panic!("expected a transcript, got {other:?}"),
        };

        // 3. inject: feed the transcript through the SAME run_fast_lane entry a
        //    typed line uses, against a fixture plan dir — it must APPLY.
        let dir = std::env::temp_dir().join(format!(
            "voice-inject-seam-{}-{}",
            std::process::id(),
            transcript.len()
        ));
        let plans = dir.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(
            plans.join("2026-W29-family-plan.md"),
            include_str!("../../tests/fixtures/family_plan_w29.md"),
        )
        .unwrap();
        // A Tuesday inside the W29 plan week.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();

        let result = fast_lane::run_fast_lane(&dir, &transcript, today);
        match &result {
            FastLaneResult::Applied { op, week_code, .. } => {
                assert!(matches!(op, FastLaneOp::ShoppingAdd { .. }));
                assert_eq!(week_code, "2026-W29");
            }
            other => panic!("spoken add should fast-lane like a typed add, got {other:?}"),
        }
        // The plan on disk actually gained the spoken item.
        let after =
            std::fs::read_to_string(plans.join("2026-W29-family-plan.md")).unwrap();
        assert!(
            after.to_lowercase().contains("olive oil"),
            "the spoken item reached the plan file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
