//! Photo → shopping-list vision pipeline (task **photo-to-shopping**).
//!
//! "Snap the fridge, Bruno adjusts the pickup list." A family member sends a
//! photo to a persona (1:1, or a group message that names/@mentions them). The
//! listener downloads the image, hands it — together with the current week's
//! shopping list — to a one-shot vision turn, and applies whatever the reply
//! implies back through the SAME gateway shopping endpoints the kiosk taps, so
//! every surface (kiosk, Week view, kitchen board) stays in sync.
//!
//! This module holds the **pure, testable core** plus the small I/O seams:
//!   - [`largest_photo_file_id`] / [`photo_meta`] — parse a Telegram `photo`
//!     array (Bot API sends several rendered sizes; the last is the largest).
//!   - [`check_photo_within_limits`] — a max dimension / file-size guard so a
//!     hostile or accidental giant image never reaches the download or the CLI.
//!   - [`coalesce_album`] — collapse an **album** (several photos sharing a
//!     `media_group_id`, delivered as separate updates) into ONE vision turn,
//!     so we never loop the model once per frame.
//!   - [`build_vision_prompt`] — frame the compare-against-the-list task in
//!     family voice, embedding the image references and the current list.
//!   - [`parse_vision_verdict`] — read the model's structured tail
//!     (`SHOPPING_UPDATE: have=[…]; need=[…]`) and strip it from the reply the
//!     human sees.
//!   - [`plan_shopping_actions`] — turn the verdict + the current list into a
//!     concrete set of endpoint mutations (cross off what we already have,
//!     keep / add what we still need), with typo-tolerant name matching.
//!   - [`ShoppingGateway`] / [`PhotoDownloader`] / [`VisionComposer`] — the
//!     three async seams (fake in tests, real reqwest / CLI in production) and
//!     [`run_photo_shopping_turn`], the orchestrator that wires them.
//!
//! **Security:** images are only processed for **confirmed** humans (the
//! caller gates on `telegram_sender::resolve_inbound(...).confirmed`); the
//! download URL embeds the bot token, so it is NEVER logged (errors are run
//! through [`crate::notify::telegram::redact_bot_token`]); the on-disk temp
//! file is created under a caller-provided scratch dir and cleaned up by the
//! caller. The `file_id` itself is opaque and safe to log.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::IncomingMessage;
use super::grounding::{self, FamilyVoiceRoster};

// ---------------------------------------------------------------------------
// Photo parsing
// ---------------------------------------------------------------------------

/// Parsed metadata for the largest rendered size of a Telegram photo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoMeta {
    /// Opaque download handle (`getFile`). Safe to log.
    pub file_id: String,
    /// Rendered width in pixels, when the update reports it (`0` if absent).
    pub width: u64,
    /// Rendered height in pixels, when the update reports it (`0` if absent).
    pub height: u64,
    /// File size in bytes, when the update reports it (`None` if absent — the
    /// Bot API omits it for some sizes).
    pub file_size: Option<u64>,
}

/// The `file_id` of the largest rendered size in a message's `photo` array, if
/// the message carries one. Telegram orders the array smallest-first, so the
/// last element is the largest (best for vision). `None` when there is no
/// `photo` array (text/other) or it is empty/malformed.
pub fn largest_photo_file_id(message: &serde_json::Value) -> Option<String> {
    photo_meta(message).map(|m| m.file_id)
}

/// Full [`PhotoMeta`] for the largest size of a message's `photo` array.
pub fn photo_meta(message: &serde_json::Value) -> Option<PhotoMeta> {
    let sizes = message.get("photo")?.as_array()?;
    // Pick the element with the greatest area; Telegram sorts ascending but we
    // don't rely on order — a defensive max keeps us correct if that changes.
    let largest = sizes
        .iter()
        .filter_map(|s| {
            let file_id = s.get("file_id").and_then(|v| v.as_str())?.to_string();
            let width = s.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
            let height = s.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
            let file_size = s.get("file_size").and_then(|v| v.as_u64());
            Some(PhotoMeta {
                file_id,
                width,
                height,
                file_size,
            })
        })
        .max_by_key(|m| (m.width.saturating_mul(m.height), m.file_size.unwrap_or(0)))?;
    Some(largest)
}

/// Ceilings a photo must respect before we download it or hand it to the model.
#[derive(Debug, Clone, Copy)]
pub struct PhotoLimits {
    /// Largest allowed width OR height, in pixels.
    pub max_dimension: u64,
    /// Largest allowed file size, in bytes.
    pub max_file_size: u64,
    /// Most images we will feed a single (album) turn — an album with more
    /// frames than this is truncated to the first `max_images` (and the drop is
    /// surfaced by the caller, never silent).
    pub max_images: usize,
}

impl Default for PhotoLimits {
    fn default() -> Self {
        // Telegram itself caps bot downloads at 20 MB, so that is the hard file
        // ceiling; the dimension cap rejects absurd/decompression-bomb inputs
        // while comfortably clearing any phone photo. Eight frames is plenty for
        // "a few shelves of the fridge".
        Self {
            max_dimension: 12_000,
            max_file_size: 20 * 1024 * 1024,
            max_images: 8,
        }
    }
}

/// `Ok(())` if the photo is within `limits`, else an `Err` naming which ceiling
/// it broke (the caller declines the image with a gentle note; the model is
/// never invoked on an over-limit input).
pub fn check_photo_within_limits(meta: &PhotoMeta, limits: &PhotoLimits) -> Result<()> {
    if meta.width > limits.max_dimension || meta.height > limits.max_dimension {
        anyhow::bail!(
            "photo {}x{} exceeds max dimension {}",
            meta.width,
            meta.height,
            limits.max_dimension
        );
    }
    if let Some(size) = meta.file_size {
        if size > limits.max_file_size {
            anyhow::bail!(
                "photo {} bytes exceeds max file size {}",
                size,
                limits.max_file_size
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Album coalescing
// ---------------------------------------------------------------------------

/// One vision turn's worth of photo(s): the `file_id`s to download, the caption
/// (the words the human typed, used both for routing and as the vision prompt's
/// instruction), and the routing context copied from the triggering message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedPhotoTurn {
    /// The `media_group_id` this turn represents, or `None` for a lone photo.
    pub media_group_id: Option<String>,
    /// Every photo `file_id` in the turn, in arrival order.
    pub file_ids: Vec<String>,
    /// The human's caption (first non-empty across the album's frames — an
    /// album shows the caption only on one frame). Empty when none was typed.
    pub caption: String,
    /// The chat the turn arrived in (reply target).
    pub chat_id: Option<String>,
    /// The chat kind (`private` / `group` / …), for the routing decision.
    pub chat_type: Option<String>,
    /// The sending human's display label.
    pub sender: String,
}

/// Collapse a buffered batch of photo messages into per-turn units.
///
/// Lone photos (no `media_group_id`) each become their own turn. Every frame
/// sharing a `media_group_id` collapses into ONE turn: their `file_id`s are
/// gathered in order and the first non-empty caption wins (Telegram attaches
/// the album caption to a single frame). Non-photo messages are ignored. Turns
/// are returned in first-seen order so a deterministic caption/route is chosen.
///
/// This is what stops the "no loops on albums" failure: without it, six fridge
/// photos would fire six vision turns and six replies.
pub fn coalesce_album(messages: &[IncomingMessage]) -> Vec<CoalescedPhotoTurn> {
    let mut turns: Vec<CoalescedPhotoTurn> = Vec::new();
    // media_group_id -> index into `turns` (only for grouped frames).
    let mut group_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for msg in messages {
        let Some(file_id) = msg.photo_file_id.clone() else {
            continue; // not a photo
        };
        let caption = msg.body.trim().to_string();
        match &msg.media_group_id {
            Some(gid) => {
                if let Some(&idx) = group_index.get(gid) {
                    let turn = &mut turns[idx];
                    turn.file_ids.push(file_id);
                    // First non-empty caption across the album wins.
                    if turn.caption.is_empty() && !caption.is_empty() {
                        turn.caption = caption;
                    }
                } else {
                    group_index.insert(gid.clone(), turns.len());
                    turns.push(CoalescedPhotoTurn {
                        media_group_id: Some(gid.clone()),
                        file_ids: vec![file_id],
                        caption,
                        chat_id: msg.chat_id.clone(),
                        chat_type: msg.chat_type.clone(),
                        sender: msg.sender.clone(),
                    });
                }
            }
            None => {
                turns.push(CoalescedPhotoTurn {
                    media_group_id: None,
                    file_ids: vec![file_id],
                    caption,
                    chat_id: msg.chat_id.clone(),
                    chat_type: msg.chat_type.clone(),
                    sender: msg.sender.clone(),
                });
            }
        }
    }
    turns
}

// ---------------------------------------------------------------------------
// Vision prompt
// ---------------------------------------------------------------------------

/// A shopping-list item as the gateway reports it (`GET /shopping.json`): its
/// stable data-key, its display text, and whether it is already crossed off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShoppingItem {
    pub key: String,
    pub text: String,
    pub checked: bool,
}

/// The marker the vision turn is instructed to end its reply with, so the
/// human-facing family-voice line and the machine directive travel together in
/// one cheap turn yet parse deterministically. Case-insensitive on read.
pub const SHOPPING_UPDATE_MARKER: &str = "SHOPPING_UPDATE:";

/// Assemble the vision prompt: the persona's voice, the current shopping list
/// (so the model compares what it SEES against what we already planned), the
/// human's caption as the instruction, the attached image reference(s), and the
/// dual-output contract (a warm family-voice line + the machine tail).
///
/// `image_refs` are the `@<abs-path>` mentions the `claude` CLI resolves into
/// attached images (see [`VisionComposer`]); pure otherwise.
pub fn build_vision_prompt(
    persona_summary: Option<&str>,
    list: &[ShoppingItem],
    caption: &str,
    image_refs: &[String],
) -> String {
    let mut p = String::new();
    match persona_summary {
        Some(s) if !s.trim().is_empty() => {
            p.push_str("You are answering as this person, in their voice:\n\n");
            p.push_str(s.trim());
            p.push_str("\n\n");
        }
        _ => {
            p.push_str(
                "You are the household helper who received this photo. Reply warmly and \
                 help keep the shared shopping list accurate.\n\n",
            );
        }
    }

    p.push_str(
        "A family member sent a PHOTO (likely the fridge, a shelf, or the pantry). \
         Look at the image and compare what is visibly IN STOCK against the current \
         shopping list below.\n\n",
    );

    p.push_str("Current shopping list for this week:\n");
    if list.is_empty() {
        p.push_str("  (the list is currently empty)\n");
    } else {
        for item in list {
            let mark = if item.checked { "[x]" } else { "[ ]" };
            p.push_str(&format!("  {} {}\n", mark, item.text.trim()));
        }
    }
    p.push('\n');

    if !caption.trim().is_empty() {
        p.push_str(&format!("They wrote: {}\n\n", caption.trim()));
    }

    for r in image_refs {
        p.push_str(&format!("Attached photo: {r}\n"));
    }
    if !image_refs.is_empty() {
        p.push('\n');
    }

    p.push_str(
        "Reply in one or two short, warm sentences — like a person texting family. \
         Say what they still need to buy and what they already have (so it can be \
         crossed off). No jargon, no lists with headings, no task ids.\n\n",
    );
    p.push_str(&format!(
        "Then, on a FINAL separate line, emit a machine directive EXACTLY in this form \
         (this line is stripped before the family sees your reply):\n\
         {SHOPPING_UPDATE_MARKER} have=[items you can SEE they already have]; \
         need=[items still to buy]\n\
         Use plain item names matching the list where possible; leave a side empty \
         like have=[] if it does not apply.\n"
    ));
    p
}

// ---------------------------------------------------------------------------
// Verdict parsing (the structured tail)
// ---------------------------------------------------------------------------

/// The parsed outcome of a vision turn: the family-voice reply (with the
/// machine tail removed) plus the two intent lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisionVerdict {
    /// The reply to actually send the human — the model's text with the
    /// `SHOPPING_UPDATE:` line removed and trailing whitespace trimmed.
    pub reply_text: String,
    /// Items the photo shows we already HAVE → candidates to cross off.
    pub have: Vec<String>,
    /// Items still NEEDED → candidates to keep on / add to the list.
    pub need: Vec<String>,
}

/// Parse the model's reply into a [`VisionVerdict`]. Finds the last line that
/// starts (case-insensitively, after trimming) with [`SHOPPING_UPDATE_MARKER`],
/// pulls its `have=` / `need=` comma lists, and returns the reply with that line
/// stripped. A reply with no marker yields empty `have`/`need` (nothing to
/// apply) and the full text as the reply — so a model that ignores the contract
/// still produces a clean family answer, we just don't mutate the list.
pub fn parse_vision_verdict(reply: &str) -> VisionVerdict {
    let mut kept: Vec<&str> = Vec::new();
    let mut have: Vec<String> = Vec::new();
    let mut need: Vec<String> = Vec::new();
    let mut found = false;

    for line in reply.lines() {
        let trimmed = line.trim_start();
        if trimmed
            .to_ascii_lowercase()
            .starts_with(&SHOPPING_UPDATE_MARKER.to_ascii_lowercase())
        {
            found = true;
            let rest = &trimmed[SHOPPING_UPDATE_MARKER.len()..];
            have = parse_named_list(rest, "have");
            need = parse_named_list(rest, "need");
            // Do NOT keep this line in the family-facing reply.
            continue;
        }
        kept.push(line);
    }

    let _ = found;
    VisionVerdict {
        reply_text: kept.join("\n").trim().to_string(),
        have,
        need,
    }
}

/// Extract the comma-separated items for `field` (`have` / `need`) from a
/// `have=[a, b]; need=[c]`-style tail. Tolerates brackets or bare lists, extra
/// whitespace, and a trailing `;`. Empty items are dropped.
fn parse_named_list(tail: &str, field: &str) -> Vec<String> {
    // Find `field=` case-insensitively.
    let lower = tail.to_ascii_lowercase();
    let needle = format!("{field}=");
    let Some(start) = lower.find(&needle) else {
        return Vec::new();
    };
    let after = &tail[start + needle.len()..];
    // The value runs until the next `;` that separates fields (or end).
    let value_end = after.find(';').unwrap_or(after.len());
    let mut value = after[..value_end].trim();
    // Strip optional surrounding brackets.
    value = value
        .strip_prefix('[')
        .unwrap_or(value)
        .strip_suffix(']')
        .unwrap_or_else(|| value.strip_prefix('[').unwrap_or(value));
    value
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '[' || c == ']').trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Action planning
// ---------------------------------------------------------------------------

/// A single concrete mutation to run against the gateway shopping endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShoppingAction {
    /// Cross an existing item off (`POST /shopping/toggle {key, checked:true}`).
    CrossOff { key: String, text: String },
    /// Restore a crossed-off item (`POST /shopping/toggle {key, checked:false}`).
    Restore { key: String, text: String },
    /// Add a new manual item (`POST /shopping/add {text}`).
    Add { text: String },
}

/// Normalize an item name for typo-tolerant matching: lowercase, collapse
/// whitespace, drop a trailing plural `s` on the last word so "lemons" matches
/// "lemon". Mirrors the gateway's own `norm` intent without importing it.
fn norm_item(s: &str) -> String {
    let collapsed = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // Singularize the final token only (avoid butchering "chickpeas" mid-word).
    if let Some((head, last)) = collapsed.rsplit_once(' ') {
        format!("{head} {}", singular(last))
    } else {
        singular(&collapsed)
    }
}

fn singular(w: &str) -> String {
    // "peas" -> "pea" too readily hurts ("chickpeas"); only strip a plain
    // trailing 's' when the stem is >2 chars and doesn't end in "ss".
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

/// Does `name` refer to `item`? True when the normalized names are equal or one
/// contains the other as a substring (so "lemons" matches "6 lemons" and
/// "chard" matches "swiss chard").
fn names_match(name: &str, item_text: &str) -> bool {
    let a = norm_item(name);
    let b = norm_item(item_text);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.contains(&b) || b.contains(&a)
}

/// Turn a [`VisionVerdict`] into the set of endpoint mutations to apply against
/// the current `list`:
///   - Each `have` item that matches a list item currently ON the list (not yet
///     crossed off) → [`ShoppingAction::CrossOff`].
///   - Each `need` item that matches a crossed-off list item → restore it; that
///     matches nothing on the list → [`ShoppingAction::Add`]. A `need` item
///     already on the list and not crossed off needs no action.
///
/// `need` is resolved AFTER `have`, and a `need` for an item also in `have`
/// wins (we keep what we still need on the list) — so a model that lists an
/// item on both sides never leaves it wrongly crossed off. Duplicate actions on
/// the same key are de-duplicated.
pub fn plan_shopping_actions(
    verdict: &VisionVerdict,
    list: &[ShoppingItem],
) -> Vec<ShoppingAction> {
    use std::collections::HashSet;
    let mut actions: Vec<ShoppingAction> = Vec::new();
    let mut crossed_keys: HashSet<String> = HashSet::new();
    // Which normalized names are on the `need` side — a need overrides a have.
    let need_norms: HashSet<String> = verdict.need.iter().map(|n| norm_item(n)).collect();

    for have in &verdict.have {
        // Skip if the same item is also needed (need wins).
        if need_norms.contains(&norm_item(have)) {
            continue;
        }
        if let Some(item) = list.iter().find(|it| names_match(have, &it.text)) {
            if !item.checked && crossed_keys.insert(item.key.clone()) {
                actions.push(ShoppingAction::CrossOff {
                    key: item.key.clone(),
                    text: item.text.clone(),
                });
            }
        }
        // A `have` item not on the list needs no action — we only had to buy
        // what was on the list; already-owned pantry staples aren't added.
    }

    let mut added_norms: HashSet<String> = HashSet::new();
    let mut restored_keys: HashSet<String> = HashSet::new();
    for need in &verdict.need {
        match list.iter().find(|it| names_match(need, &it.text)) {
            Some(item) if item.checked => {
                if restored_keys.insert(item.key.clone()) {
                    actions.push(ShoppingAction::Restore {
                        key: item.key.clone(),
                        text: item.text.clone(),
                    });
                }
            }
            Some(_) => { /* already on the list, uncrossed — nothing to do */ }
            None => {
                // Not on the list at all → add it (once).
                if added_norms.insert(norm_item(need)) {
                    actions.push(ShoppingAction::Add {
                        text: need.trim().to_string(),
                    });
                }
            }
        }
    }

    actions
}

// ---------------------------------------------------------------------------
// I/O seams: gateway, downloader, composer
// ---------------------------------------------------------------------------

/// The shopping surface the kiosk uses, abstracted so the turn handler applies
/// list changes through the SAME endpoints (never a direct file write). The
/// production impl is [`HttpShoppingGateway`]; tests supply a fake.
#[async_trait]
pub trait ShoppingGateway: Send + Sync {
    /// The current week's merged list (`GET /shopping.json`).
    async fn list(&self) -> Result<Vec<ShoppingItem>>;
    /// Cross an item off / restore it (`POST /shopping/toggle`).
    async fn toggle(&self, key: &str, checked: bool) -> Result<()>;
    /// Add a manual item (`POST /shopping/add`).
    async fn add(&self, text: &str) -> Result<()>;
}

/// Downloads a Telegram photo by `file_id` to `dest`. Production impl calls
/// `getFile` then GETs the file URL (both embed the bot token — kept out of
/// logs); tests supply a fake that writes fixture bytes.
#[async_trait]
pub trait PhotoDownloader: Send + Sync {
    async fn download(&self, file_id: &str, dest: &Path) -> Result<()>;
}

/// A one-shot vision turn: a `claude`-CLI spawn given `prompt` plus the
/// attached `image_paths`. Implemented for [`crate::notify::telegram_conversation::OneshotComposer`].
#[async_trait]
pub trait VisionComposer: Send + Sync {
    async fn compose_vision(&self, prompt: &str, image_paths: &[PathBuf]) -> Result<String>;
}

/// The gateway base URL (`CASA_GATEWAY_URL`, default the fixed kiosk port). No
/// trailing slash.
pub fn gateway_base_url() -> String {
    std::env::var("CASA_GATEWAY_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:7788".to_string())
}

/// Production [`ShoppingGateway`] over HTTP against the casa gateway.
pub struct HttpShoppingGateway {
    client: reqwest::Client,
    base_url: String,
    /// `back` window (0 = current week) — matches the kiosk's default.
    back: i64,
}

impl HttpShoppingGateway {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            back: 0,
        }
    }

    /// Build from the `CASA_GATEWAY_URL` env (or the default port).
    pub fn from_env() -> Self {
        Self::new(gateway_base_url())
    }
}

#[async_trait]
impl ShoppingGateway for HttpShoppingGateway {
    async fn list(&self) -> Result<Vec<ShoppingItem>> {
        let url = format!("{}/shopping.json?back={}", self.base_url, self.back);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET /shopping.json failed")?;
        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse /shopping.json")?;
        Ok(parse_shopping_json(&json))
    }

    async fn toggle(&self, key: &str, checked: bool) -> Result<()> {
        let url = format!("{}/shopping/toggle", self.base_url);
        let body = serde_json::json!({ "back": self.back, "key": key, "checked": checked });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST /shopping/toggle failed")?;
        ensure_ok(resp).await
    }

    async fn add(&self, text: &str) -> Result<()> {
        let url = format!("{}/shopping/add", self.base_url);
        let body = serde_json::json!({ "back": self.back, "text": text });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST /shopping/add failed")?;
        ensure_ok(resp).await
    }
}

async fn ensure_ok(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() || json.get("ok") == Some(&serde_json::Value::Bool(false)) {
        let err = json
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown");
        anyhow::bail!("shopping endpoint returned not-ok ({status}): {err}");
    }
    Ok(())
}

/// Parse `GET /shopping.json`'s `{ ok, groups:[{ items:[{key,text,checked}] }] }`
/// shape into a flat item list. Tolerant of a few field-name variants and of a
/// group being a bare item array.
pub fn parse_shopping_json(json: &serde_json::Value) -> Vec<ShoppingItem> {
    let mut items = Vec::new();
    let groups = json.get("groups").and_then(|g| g.as_array());
    let Some(groups) = groups else {
        return items;
    };
    for group in groups {
        // A group is usually `{ store, items:[...] }`; be tolerant of a bare array.
        let list = group
            .get("items")
            .and_then(|i| i.as_array())
            .or_else(|| group.as_array());
        let Some(list) = list else { continue };
        for it in list {
            let Some(text) = it.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            let key = it
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string();
            let checked = it
                .get("checked")
                .and_then(|c| c.as_bool())
                .or_else(|| it.get("crossed").and_then(|c| c.as_bool()))
                .unwrap_or(false);
            items.push(ShoppingItem {
                key,
                text: text.to_string(),
                checked,
            });
        }
    }
    items
}

/// Production [`PhotoDownloader`]: `getFile` → download the file URL. Both URLs
/// embed the bot token, so any error is scrubbed through
/// [`crate::notify::telegram::redact_bot_token`] before it can be logged.
pub struct TelegramPhotoDownloader {
    client: reqwest::Client,
    bot_token: String,
}

impl TelegramPhotoDownloader {
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            bot_token: bot_token.into(),
        }
    }
}

#[async_trait]
impl PhotoDownloader for TelegramPhotoDownloader {
    async fn download(&self, file_id: &str, dest: &Path) -> Result<()> {
        // 1. Resolve the file path via getFile.
        let get_file_url = format!("https://api.telegram.org/bot{}/getFile", self.bot_token);
        let resp = self
            .client
            .post(&get_file_url)
            .json(&serde_json::json!({ "file_id": file_id }))
            .send()
            .await
            .map_err(|e| scrub("getFile request failed", e))?;
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| scrub("getFile response was not valid JSON", e))?;
        let file_path = json
            .get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|p| p.as_str())
            .context("getFile response missing result.file_path")?;

        // 2. Download the bytes.
        let file_url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot_token, file_path
        );
        let bytes = self
            .client
            .get(&file_url)
            .send()
            .await
            .map_err(|e| scrub("photo download request failed", e))?
            .bytes()
            .await
            .map_err(|e| scrub("reading photo bytes failed", e))?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(dest, &bytes)
            .with_context(|| format!("failed to write photo to {}", dest.display()))?;
        Ok(())
    }
}

/// Consume a transport error from a token-bearing URL into a flat, redacted
/// `anyhow::Error` so no renderer — including anyhow's `Caused by:` chain walk
/// — can reprint the token. Thin alias over the shared choke point
/// [`crate::notify::telegram::redacted_api_error`].
fn scrub<E: std::error::Error>(context: &str, err: E) -> anyhow::Error {
    super::telegram::redacted_api_error(context, err)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// The result of a completed photo turn: the family-voice reply to send and a
/// human-readable summary of the mutations that were applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoTurnResult {
    pub reply_text: String,
    pub actions: Vec<ShoppingAction>,
}

/// Run ONE coalesced photo turn end-to-end, given the three seams:
///   1. download each photo (bounded by `limits.max_images`) into `scratch_dir`,
///   2. read the current list from the gateway,
///   3. build the vision prompt and run the vision turn,
///   4. parse the verdict, plan the list mutations, and apply them via the
///      gateway endpoints,
///   5. return the (marker-stripped) family reply + the applied actions.
///
/// Downloaded temp files are removed before returning (best-effort). The turn
/// fails fast (returning `Err`) on a download / compose / list-read error so the
/// caller can send a gentle "couldn't read that photo" note instead of hanging.
pub async fn run_photo_shopping_turn(
    turn: &CoalescedPhotoTurn,
    persona_summary: Option<&str>,
    limits: &PhotoLimits,
    downloader: &dyn PhotoDownloader,
    composer: &dyn VisionComposer,
    gateway: &dyn ShoppingGateway,
    family_roster: &FamilyVoiceRoster,
    scratch_dir: &Path,
) -> Result<PhotoTurnResult> {
    // 1. Download (cap the album; the cap is the caller's responsibility to
    //    surface — we log-free here and just bound the work).
    let mut image_paths: Vec<PathBuf> = Vec::new();
    for (i, file_id) in turn.file_ids.iter().take(limits.max_images).enumerate() {
        let dest = scratch_dir.join(format!("casa-photo-{i}.jpg"));
        downloader
            .download(file_id, &dest)
            .await
            .context("failed to download photo for vision turn")?;
        image_paths.push(dest);
    }
    if image_paths.is_empty() {
        anyhow::bail!("photo turn had no downloadable images");
    }

    // 2. Current list.
    let list = gateway
        .list()
        .await
        .context("failed to read current shopping list")?;

    // 3. Vision turn. The `@<abs-path>` mention is how the claude CLI attaches
    //    a local image in --print mode.
    let image_refs: Vec<String> = image_paths
        .iter()
        .map(|p| format!("@{}", p.display()))
        .collect();
    let prompt = build_vision_prompt(persona_summary, &list, &turn.caption, &image_refs);
    let raw = composer
        .compose_vision(&prompt, &image_paths)
        .await
        .context("vision compose turn failed");

    // Clean up temp files regardless of the compose outcome.
    for p in &image_paths {
        let _ = std::fs::remove_file(p);
    }
    let raw = raw?;

    // 4. Parse + plan + apply.
    let verdict = parse_vision_verdict(&raw);
    let actions = plan_shopping_actions(&verdict, &list);
    for action in &actions {
        let res = match action {
            ShoppingAction::CrossOff { key, .. } => gateway.toggle(key, true).await,
            ShoppingAction::Restore { key, .. } => gateway.toggle(key, false).await,
            ShoppingAction::Add { text } => gateway.add(text).await,
        };
        // A single mutation failing should not abort the others or the reply —
        // the family still gets the answer; log-free bail is avoided.
        if let Err(e) = res {
            eprintln!(
                "photo-to-shopping: a list update did not apply: {}",
                super::telegram::redact_bot_token(&format!("{e:#}"))
            );
        }
    }

    let reply_text = if verdict.reply_text.is_empty() {
        raw.trim().to_string()
    } else {
        verdict.reply_text
    };
    // Photo replies bypass telegram_conversation's finalizer and go straight to
    // BotReplySink. Guard the model copy here so every caller receives exactly
    // the family-safe text that may be delivered.
    let reply_text = grounding::enforce_family_voice(&reply_text, family_roster);

    Ok(PhotoTurnResult {
        reply_text,
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo_msg(file_id: &str, caption: &str, media_group_id: Option<&str>) -> IncomingMessage {
        IncomingMessage {
            channel: "telegram".to_string(),
            sender: "luca".to_string(),
            sender_id: Some("111".to_string()),
            sender_is_bot: false,
            sent_at: Some(1),
            body: caption.to_string(),
            action_id: None,
            reply_to: None,
            message_id: Some("1".to_string()),
            chat_id: Some("-100".to_string()),
            chat_type: Some("group".to_string()),
            mention_usernames: Vec::new(),
            reply_to_bot: None,
            has_bot_command: false,
            photo_file_id: Some(file_id.to_string()),
            media_group_id: media_group_id.map(|s| s.to_string()),
            voice_file_id: None,
            voice_mime: None,
        }
    }

    fn item(key: &str, text: &str, checked: bool) -> ShoppingItem {
        ShoppingItem {
            key: key.to_string(),
            text: text.to_string(),
            checked,
        }
    }

    // --- photo parsing -----------------------------------------------------

    #[test]
    fn photo_parse_picks_largest_size() {
        let msg = serde_json::json!({
            "photo": [
                { "file_id": "small", "width": 90, "height": 60, "file_size": 900 },
                { "file_id": "big",   "width": 1280, "height": 720, "file_size": 90000 },
                { "file_id": "mid",   "width": 320, "height": 240, "file_size": 8000 },
            ]
        });
        let meta = photo_meta(&msg).expect("has photo");
        assert_eq!(meta.file_id, "big");
        assert_eq!(meta.width, 1280);
        assert_eq!(largest_photo_file_id(&msg).as_deref(), Some("big"));
    }

    #[test]
    fn photo_parse_none_for_text_message() {
        let msg = serde_json::json!({ "text": "hello" });
        assert!(photo_meta(&msg).is_none());
        assert!(largest_photo_file_id(&msg).is_none());
    }

    #[test]
    fn photo_limits_reject_oversize() {
        let limits = PhotoLimits::default();
        let big_dim = PhotoMeta {
            file_id: "x".into(),
            width: 50_000,
            height: 100,
            file_size: Some(100),
        };
        assert!(check_photo_within_limits(&big_dim, &limits).is_err());
        let big_file = PhotoMeta {
            file_id: "x".into(),
            width: 100,
            height: 100,
            file_size: Some(50 * 1024 * 1024),
        };
        assert!(check_photo_within_limits(&big_file, &limits).is_err());
        let ok = PhotoMeta {
            file_id: "x".into(),
            width: 1280,
            height: 720,
            file_size: Some(90_000),
        };
        assert!(check_photo_within_limits(&ok, &limits).is_ok());
    }

    // --- album coalescing --------------------------------------------------

    #[test]
    fn photo_album_coalesces_to_one_turn() {
        let msgs = vec![
            photo_msg("f1", "bruno what do we still need?", Some("grp-A")),
            photo_msg("f2", "", Some("grp-A")),
            photo_msg("f3", "", Some("grp-A")),
        ];
        let turns = coalesce_album(&msgs);
        assert_eq!(turns.len(), 1, "album collapses to one turn");
        assert_eq!(turns[0].file_ids, vec!["f1", "f2", "f3"]);
        assert_eq!(turns[0].caption, "bruno what do we still need?");
    }

    #[test]
    fn photo_lone_photos_are_separate_turns() {
        let msgs = vec![photo_msg("f1", "one", None), photo_msg("f2", "two", None)];
        let turns = coalesce_album(&msgs);
        assert_eq!(turns.len(), 2);
    }

    #[test]
    fn photo_album_first_nonempty_caption_wins_even_out_of_order() {
        let msgs = vec![
            photo_msg("f1", "", Some("g")),
            photo_msg("f2", "the caption", Some("g")),
        ];
        let turns = coalesce_album(&msgs);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].caption, "the caption");
        assert_eq!(turns[0].file_ids, vec!["f1", "f2"]);
    }

    #[test]
    fn photo_coalesce_ignores_non_photo_messages() {
        let mut text = photo_msg("f1", "hi", None);
        text.photo_file_id = None;
        let msgs = vec![text, photo_msg("f2", "pic", None)];
        let turns = coalesce_album(&msgs);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].file_ids, vec!["f2"]);
    }

    #[test]
    fn photo_prompt_uses_configured_voice_or_a_name_free_fallback() {
        let image_refs = vec!["@/tmp/scratch-photo.jpg".to_string()];
        let configured = build_vision_prompt(
            Some("Zephyra is the household's pantry guide."),
            &[],
            "What do we need?",
            &image_refs,
        );
        assert!(
            configured.contains("Zephyra is the household's pantry guide."),
            "the elected voice summary must remain authoritative"
        );

        let fallback = build_vision_prompt(None, &[], "What do we need?", &image_refs);
        assert!(
            fallback.starts_with("You are the household helper who received this photo."),
            "a missing session summary must use a role-neutral prompt"
        );
        assert!(
            !fallback.contains("Zephyra"),
            "the neutral fallback must not invent the configured fixture voice"
        );
    }

    // --- verdict parsing ---------------------------------------------------

    #[test]
    fn photo_verdict_parses_and_strips_marker() {
        let reply = "You still need lemons and chard; you already have chickpeas — crossing them off.\nSHOPPING_UPDATE: have=[chickpeas]; need=[lemons, chard]";
        let v = parse_vision_verdict(reply);
        assert_eq!(v.have, vec!["chickpeas"]);
        assert_eq!(v.need, vec!["lemons", "chard"]);
        assert!(
            !v.reply_text.contains("SHOPPING_UPDATE"),
            "marker stripped from family reply: {:?}",
            v.reply_text
        );
        assert!(v.reply_text.contains("lemons"));
    }

    #[test]
    fn photo_verdict_no_marker_yields_empty_intents() {
        let reply = "Looks like you're all set!";
        let v = parse_vision_verdict(reply);
        assert!(v.have.is_empty() && v.need.is_empty());
        assert_eq!(v.reply_text, "Looks like you're all set!");
    }

    #[test]
    fn photo_verdict_tolerates_bare_lists_and_case() {
        let reply = "ok\nshopping_update: have= chickpeas, rice ; need=lemons";
        let v = parse_vision_verdict(reply);
        assert_eq!(v.have, vec!["chickpeas", "rice"]);
        assert_eq!(v.need, vec!["lemons"]);
    }

    // --- action planning ---------------------------------------------------

    #[test]
    fn photo_plan_crosses_off_have_and_adds_missing_need() {
        let list = vec![
            item("p:store|chickpeas", "Chickpeas", false),
            item("p:store|lemons", "Lemons", false),
        ];
        let verdict = VisionVerdict {
            reply_text: String::new(),
            have: vec!["chickpeas".into()],
            need: vec!["lemons".into(), "chard".into()],
        };
        let actions = plan_shopping_actions(&verdict, &list);
        assert!(actions.contains(&ShoppingAction::CrossOff {
            key: "p:store|chickpeas".into(),
            text: "Chickpeas".into()
        }));
        // "lemons" already on list uncrossed → no action; "chard" missing → add.
        assert!(actions.contains(&ShoppingAction::Add {
            text: "chard".into()
        }));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ShoppingAction::CrossOff { text, .. } if text == "Lemons"))
        );
    }

    #[test]
    fn photo_plan_restores_crossed_item_that_is_needed() {
        let list = vec![item("p:s|milk", "Milk", true)];
        let verdict = VisionVerdict {
            reply_text: String::new(),
            have: vec![],
            need: vec!["milk".into()],
        };
        let actions = plan_shopping_actions(&verdict, &list);
        assert_eq!(
            actions,
            vec![ShoppingAction::Restore {
                key: "p:s|milk".into(),
                text: "Milk".into()
            }]
        );
    }

    #[test]
    fn photo_plan_need_overrides_have_for_same_item() {
        // Model contradicts itself; we must NOT cross off something we still need.
        let list = vec![item("p:s|eggs", "Eggs", false)];
        let verdict = VisionVerdict {
            reply_text: String::new(),
            have: vec!["eggs".into()],
            need: vec!["eggs".into()],
        };
        let actions = plan_shopping_actions(&verdict, &list);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ShoppingAction::CrossOff { .. }))
        );
    }

    #[test]
    fn photo_plan_typo_tolerant_matching() {
        let list = vec![item("p:s|lemon", "6 Lemons", false)];
        let verdict = VisionVerdict {
            reply_text: String::new(),
            have: vec!["lemon".into()],
            need: vec![],
        };
        let actions = plan_shopping_actions(&verdict, &list);
        assert_eq!(
            actions,
            vec![ShoppingAction::CrossOff {
                key: "p:s|lemon".into(),
                text: "6 Lemons".into()
            }]
        );
    }

    // --- shopping.json parsing --------------------------------------------

    #[test]
    fn photo_parse_shopping_json_flattens_groups() {
        let json = serde_json::json!({
            "ok": true,
            "groups": [
                { "store": "Market", "items": [
                    { "key": "p:market|lemons", "text": "Lemons", "checked": false },
                    { "key": "p:market|rice", "text": "Rice", "checked": true },
                ]},
                { "store": "Also getting", "items": [
                    { "key": "add:1", "text": "Sponges", "checked": false },
                ]},
            ]
        });
        let items = parse_shopping_json(&json);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].key, "p:market|lemons");
        assert!(items[1].checked);
        assert_eq!(items[2].text, "Sponges");
    }

    // --- orchestrator (with fakes) ----------------------------------------

    struct FakeDownloader;
    #[async_trait]
    impl PhotoDownloader for FakeDownloader {
        async fn download(&self, _file_id: &str, dest: &Path) -> Result<()> {
            std::fs::write(dest, b"fake-jpeg-bytes")?;
            Ok(())
        }
    }

    struct RecordingComposer {
        reply: String,
        seen_images: std::sync::Mutex<Vec<usize>>,
        seen_prompt: std::sync::Mutex<String>,
    }
    #[async_trait]
    impl VisionComposer for RecordingComposer {
        async fn compose_vision(&self, prompt: &str, image_paths: &[PathBuf]) -> Result<String> {
            *self.seen_prompt.lock().unwrap() = prompt.to_string();
            self.seen_images.lock().unwrap().push(image_paths.len());
            // Every temp image must exist at compose time.
            for p in image_paths {
                assert!(p.exists(), "image not on disk at compose time: {p:?}");
            }
            Ok(self.reply.clone())
        }
    }

    struct FakeGateway {
        items: Vec<ShoppingItem>,
        calls: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ShoppingGateway for FakeGateway {
        async fn list(&self) -> Result<Vec<ShoppingItem>> {
            Ok(self.items.clone())
        }
        async fn toggle(&self, key: &str, checked: bool) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("toggle {key} {checked}"));
            Ok(())
        }
        async fn add(&self, text: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("add {text}"));
            Ok(())
        }
    }

    fn family_roster() -> FamilyVoiceRoster {
        FamilyVoiceRoster::from_names(
            ["Nora", "Bruno", "Coach Mira", "Otto"],
            ["Household Member"],
        )
    }

    #[tokio::test]
    async fn photo_turn_composes_with_image_and_list_then_applies_via_endpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let turn = CoalescedPhotoTurn {
            media_group_id: Some("g".into()),
            file_ids: vec!["f1".into(), "f2".into()],
            caption: "bruno what do we still need?".into(),
            chat_id: Some("-100".into()),
            chat_type: Some("group".into()),
            sender: "luca".into(),
        };
        let composer = RecordingComposer {
            reply: "You still need chard; you already have chickpeas — crossing them off.\nSHOPPING_UPDATE: have=[chickpeas]; need=[chard]".into(),
            seen_images: std::sync::Mutex::new(Vec::new()),
            seen_prompt: std::sync::Mutex::new(String::new()),
        };
        let gateway = FakeGateway {
            items: vec![
                item("p:s|chickpeas", "Chickpeas", false),
                item("p:s|lemons", "Lemons", false),
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let result = run_photo_shopping_turn(
            &turn,
            None,
            &PhotoLimits::default(),
            &FakeDownloader,
            &composer,
            &gateway,
            &family_roster(),
            tmp.path(),
        )
        .await
        .expect("turn ok");

        // Composer saw BOTH images and the list + caption in the prompt.
        assert_eq!(composer.seen_images.lock().unwrap().as_slice(), &[2]);
        let prompt = composer.seen_prompt.lock().unwrap().clone();
        assert!(prompt.contains("Chickpeas"), "list is in the prompt");
        assert!(
            prompt.contains("bruno what do we still need?"),
            "caption in prompt"
        );
        assert!(prompt.contains("@"), "image reference in prompt");

        // Reply is family-voice with the marker stripped.
        assert!(!result.reply_text.contains("SHOPPING_UPDATE"));
        assert!(result.reply_text.contains("chard"));

        // Mutations went through the endpoints: cross off chickpeas, add chard.
        let calls = gateway.calls.lock().unwrap().clone();
        assert!(
            calls.contains(&"toggle p:s|chickpeas true".to_string()),
            "calls={calls:?}"
        );
        assert!(calls.contains(&"add chard".to_string()), "calls={calls:?}");

        // Temp files cleaned up.
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn photo_turn_guards_model_copy_without_losing_shopping_actions() {
        let tmp = tempfile::tempdir().unwrap();
        let turn = CoalescedPhotoTurn {
            media_group_id: None,
            file_ids: vec!["f1".into()],
            caption: "what do we still need?".into(),
            chat_id: Some("-100".into()),
            chat_type: Some("group".into()),
            sender: "member-1".into(),
        };
        let composer = RecordingComposer {
            reply: concat!(
                "Bruno 💬 **Chard** is still needed. ",
                "Zephyra will join us. ",
                "I'll pull that from the live gateway. ",
                "Dispatcher healthy. Otto's got this one.\n",
                "SHOPPING_UPDATE: have=[chickpeas]; need=[chard]",
            )
            .into(),
            seen_images: std::sync::Mutex::new(Vec::new()),
            seen_prompt: std::sync::Mutex::new(String::new()),
        };
        let gateway = FakeGateway {
            items: vec![item("p:s|chickpeas", "Chickpeas", false)],
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let result = run_photo_shopping_turn(
            &turn,
            None,
            &PhotoLimits::default(),
            &FakeDownloader,
            &composer,
            &gateway,
            &family_roster(),
            tmp.path(),
        )
        .await
        .expect("turn ok");

        assert_eq!(result.reply_text, "Chard is still needed.");
        assert_eq!(
            result.actions,
            vec![
                ShoppingAction::CrossOff {
                    key: "p:s|chickpeas".into(),
                    text: "Chickpeas".into(),
                },
                ShoppingAction::Add {
                    text: "chard".into(),
                },
            ],
            "guarding the reply must not discard the already-planned list mutations",
        );
        let calls = gateway.calls.lock().unwrap().clone();
        assert!(calls.contains(&"toggle p:s|chickpeas true".to_string()));
        assert!(calls.contains(&"add chard".to_string()));
    }
}
