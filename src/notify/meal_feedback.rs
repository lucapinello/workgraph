//! Meal feedback — the loop that makes the agents *persistent*.
//!
//! This closes the circle from a **planned dinner** to the **next week's plan**:
//!
//! 1. **The ask.** The evening after a cooked dinner, Bruno sends the family one
//!    warm line — *"How was the salmon? 👍👎 or just tell me."* — via
//!    [`compose_ask`]. It is rate-limited so it never nags (see [`AskGate`]).
//! 2. **The routing.** A reaction or reply routes back to Bruno through the
//!    existing election (name / reply → Bruno). [`parse_rating_reply`] turns that
//!    free-text/emoji reply into a structured [`Verdict`] + note — the "intent
//!    routing" step.
//! 3. **The memory.** Ratings persist as append-only JSONL at
//!    `<root>/plans/feedback.jsonl` ([`feedback_path_for`] / [`append_rating`] /
//!    [`load_ratings`]) *and* get summarised into a warm one-liner for Bruno's and
//!    Nora's session summaries ([`render_session_note`]) so their conversational
//!    selves know it ("you all loved the stir-fry; beets are banned").
//! 4. **The loop.** The Sunday plan draft consumes the ratings: [`render_plan_briefing`]
//!    renders the winners-to-repeat / losers-to-drop block that the
//!    `weekly-plan-sunday` prompt reads, so next week's dinners actually reflect
//!    what the family liked ("beef stir-fry is back by popular demand").
//!
//! Everything here is pure or filesystem-append-only and needs no live Telegram
//! socket, so the whole loop is unit-testable from seeded fixtures. The dish name
//! for "the ask" comes from [`crate::notify::family_plan`] (`meal_on(today)`), so
//! this module never has to re-parse the plan markdown itself.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// How the family felt about a dinner, strongest-positive to strongest-negative.
///
/// A [`Verdict`] carries a numeric [`Verdict::score`] so many ratings for one
/// dish aggregate into a single winner/loser signal (see [`summarize`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Enthusiastic — "loved it", "amazing", "best yet", 😋.
    Loved,
    /// Positive — "yes", "good", "nice", 👍.
    Liked,
    /// Indifferent — "meh", "it was fine", "ok".
    Meh,
    /// Negative — "no", "not again", "too bland", 👎.
    Disliked,
}

impl Verdict {
    /// Aggregation weight. Positive = keep it, negative = drop it. `Meh` is a
    /// neutral zero so a lone shrug neither promotes nor retires a dish.
    pub fn score(self) -> i64 {
        match self {
            Verdict::Loved => 2,
            Verdict::Liked => 1,
            Verdict::Meh => 0,
            Verdict::Disliked => -2,
        }
    }

    /// Stable wire token for JSONL persistence.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Loved => "loved",
            Verdict::Liked => "liked",
            Verdict::Meh => "meh",
            Verdict::Disliked => "disliked",
        }
    }

    /// Parse a persisted token back into a [`Verdict`]; `None` for anything else.
    pub fn from_str(s: &str) -> Option<Verdict> {
        match s.trim().to_ascii_lowercase().as_str() {
            "loved" => Some(Verdict::Loved),
            "liked" => Some(Verdict::Liked),
            "meh" => Some(Verdict::Meh),
            "disliked" => Some(Verdict::Disliked),
            _ => None,
        }
    }

    /// The thumb/emoji face for this verdict, for a briefing bullet.
    pub fn emoji(self) -> &'static str {
        match self {
            Verdict::Loved => "😋",
            Verdict::Liked => "👍",
            Verdict::Meh => "😐",
            Verdict::Disliked => "👎",
        }
    }
}

/// One recorded reaction to a dinner — the durable unit of family memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MealRating {
    /// Epoch milliseconds when the rating was recorded.
    pub ts: i64,
    /// The dish that was rated, e.g. `"Beef stir-fry"`. Compared case-folded
    /// and whitespace-collapsed when aggregating (see [`dish_key`]).
    pub dish: String,
    /// Who reacted — the human's display handle (never a numeric user id).
    pub rater: String,
    /// The structured sentiment.
    pub verdict: Verdict,
    /// The free-text note the human added, if any ("too much salt"). Empty when
    /// the reaction was a bare thumb.
    pub note: String,
}

impl MealRating {
    /// Serialize to ONE compact JSON line (no trailing newline). Built from an
    /// explicit object literal so the emitted keys stay pinned regardless of any
    /// future field added to the struct.
    pub fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "dish": self.dish,
            "rater": self.rater,
            "verdict": self.verdict.as_str(),
            "note": self.note,
        })
        .to_string()
    }

    /// Parse one JSONL line back into a [`MealRating`]; `None` on any malformed
    /// line so a half-written file still yields whatever rows are valid.
    pub fn from_json_line(line: &str) -> Option<MealRating> {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        Some(MealRating {
            ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
            dish: v.get("dish")?.as_str()?.to_string(),
            rater: v
                .get("rater")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            verdict: Verdict::from_str(v.get("verdict").and_then(|x| x.as_str())?)?,
            note: v
                .get("note")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// The ask (family voice)
// ---------------------------------------------------------------------------

/// Compose Bruno's warm one-line ask for a just-cooked dish. Family voice
/// (docs/04): natural, one clear ask, no jargon, offers the low-effort 👍👎 path
/// *and* an open door for a sentence. `dish` is tonight's dinner from the plan.
pub fn compose_ask(dish: &str) -> String {
    let dish = dish.trim();
    if dish.is_empty() {
        return "How was dinner tonight? 👍 👎 or just tell me — I'll keep notes for next week. — Bruno".to_string();
    }
    // Lower-case the leading article-free dish so it reads like speech in the
    // sentence ("How was the salmon?"), but keep it verbatim if it looks like a
    // proper name (starts with an already-capitalised multi-word dish is fine as-is).
    format!(
        "How was the {dish} tonight? 👍 👎 or just tell me — I'll remember it for next week. — Bruno",
        dish = speakable_dish(dish)
    )
}

/// Fold a plan dish name into something that reads inside a sentence: trim, and
/// lower-case a leading capital *unless* the whole thing is already multi-word
/// Title Case that would look odd lowered. Keeps it simple and warm.
fn speakable_dish(dish: &str) -> String {
    let dish = dish.trim();
    // Keep the ask crisp and speakable: drop a trailing parenthetical like
    // "(leftovers)" and any comma-separated sides, so a plan slot like
    // "Baked salmon, roasted potatoes, green beans" reads as "the baked salmon".
    let base = dish
        .split(" (")
        .next()
        .unwrap_or(dish)
        .split(',')
        .next()
        .unwrap_or(dish)
        .trim();
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Intent routing: reply / reaction -> Verdict
// ---------------------------------------------------------------------------

/// Turn a human's reply (or bare emoji reaction) to the ask into a structured
/// rating. Returns the [`Verdict`] plus a cleaned note (the original text with
/// leading/trailing whitespace trimmed and pure-emoji-only replies reduced to an
/// empty note). `None` when the reply carries no legible sentiment at all — the
/// caller can then leave it unrecorded rather than invent a rating.
///
/// Precedence: an explicit 👍/👎 (or its word) wins over a merely "meh"; a
/// strong love word promotes 👍 to [`Verdict::Loved`]. This is the "intent
/// routing" seam — the election has already decided the reply belongs to Bruno;
/// this decides *what the reply means*.
pub fn parse_rating_reply(text: &str) -> Option<(Verdict, String)> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();

    // Emoji signals.
    let has_up = raw.contains('\u{1F44D}'); // 👍
    let has_down = raw.contains('\u{1F44E}'); // 👎
    let has_love = raw.contains('\u{2764}') // ❤
        || raw.contains('\u{1F60B}') // 😋
        || raw.contains('\u{1F929}') // 🤩
        || raw.contains('\u{1F60D}'); // 😍
    let has_yuck = raw.contains('\u{1F922}') // 🤢
        || raw.contains('\u{1F92E}'); // 🤮

    // Word signals (checked on a lowercased copy). Kept small and legible; this
    // is family chat, not sentiment analysis.
    let loved_words = [
        "loved", "love it", "amazing", "delicious", "incredible", "best", "so good",
        "fantastic", "yum", "yummy", "perfect", "obsessed", "10/10",
    ];
    let liked_words = [
        "liked", "good", "nice", "tasty", "great", "yes", "yep", "thumbs up", "solid",
        "enjoyed", "please again", "again please", "more of this",
    ];
    let meh_words = [
        "meh", "ok", "okay", "fine", "average", "alright", "so-so", "so so", "not bad",
    ];
    let disliked_words = [
        "no thanks", "not again", "hated", "gross", "bland", "too salty", "yuck", "awful",
        "bad", "disgusting", "won't miss", "wont miss", "skip", "not for me", "dry",
        "overcooked", "banned",
    ];

    let word_hit = |set: &[&str]| set.iter().any(|w| lower.contains(w));

    let loved = has_love || word_hit(&loved_words);
    let disliked = has_down || has_yuck || word_hit(&disliked_words);
    let liked = has_up || word_hit(&liked_words);
    let meh = word_hit(&meh_words);

    // Resolve precedence. Strong signals first; a plain "no" is a dislike even if
    // the message also thanks the cook.
    let verdict = if loved && !disliked {
        Some(Verdict::Loved)
    } else if disliked && !loved {
        Some(Verdict::Disliked)
    } else if liked && !disliked {
        Some(Verdict::Liked)
    } else if meh {
        Some(Verdict::Meh)
    } else if loved && disliked {
        // Genuinely mixed ("loved the sauce, hated the beets") — call it Meh and
        // keep the note, which carries the nuance a human can read.
        Some(Verdict::Meh)
    } else {
        None
    }?;

    Some((verdict, cleaned_note(raw)))
}

/// Reduce a reply to the note we persist: trim, collapse internal newlines to
/// spaces, and drop it entirely if what remains is only emoji/punctuation (a bare
/// 👍 has no note worth keeping). Length-capped so a pathological paste can't
/// bloat the memory file.
fn cleaned_note(raw: &str) -> String {
    const MAX_NOTE: usize = 400;
    let joined = raw
        .split('\n')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    // If it's only symbols/emoji/whitespace, there's no textual note.
    let has_letters = joined.chars().any(|c| c.is_alphanumeric());
    if !has_letters {
        return String::new();
    }
    if joined.chars().count() > MAX_NOTE {
        joined.chars().take(MAX_NOTE).collect()
    } else {
        joined
    }
}

// ---------------------------------------------------------------------------
// Persistence: plans/feedback.jsonl
// ---------------------------------------------------------------------------

/// The ratings file for a project root: `<root>/plans/feedback.jsonl`. Lives
/// under `plans/` beside the weekly plan markdown so the Sunday drafter finds the
/// family's memory right next to the document it is about to rewrite.
pub fn feedback_path_for(project_root: &Path) -> PathBuf {
    project_root.join("plans").join("feedback.jsonl")
}

/// Append one rating, creating `plans/` on first write. Append-only: one compact
/// JSON object per line plus a trailing newline.
pub fn append_rating(path: &Path, rating: &MealRating) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(rating.to_json_line().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Load every valid rating from the file, skipping malformed/blank lines. Missing
/// file yields an empty vec (no ratings yet is not an error).
pub fn load_ratings(path: &Path) -> Vec<MealRating> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(MealRating::from_json_line)
        .collect()
}

// ---------------------------------------------------------------------------
// Rate limiting: the nag gate
// ---------------------------------------------------------------------------

/// A record of one ask Bruno sent, and whether the family answered it. Persisted
/// append-only at `<root>/plans/feedback-asks.jsonl` so the gate survives a
/// restart of the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRecord {
    /// Epoch milliseconds when the ask was sent.
    pub ts: i64,
    /// The dish the ask was about.
    pub dish: String,
    /// Whether at least one family member replied to this ask.
    pub responded: bool,
}

impl AskRecord {
    pub fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "dish": self.dish,
            "responded": self.responded,
        })
        .to_string()
    }

    pub fn from_json_line(line: &str) -> Option<AskRecord> {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        Some(AskRecord {
            ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
            dish: v.get("dish")?.as_str()?.to_string(),
            responded: v.get("responded").and_then(|x| x.as_bool()).unwrap_or(false),
        })
    }
}

/// Why a proposed ask was allowed or suppressed. [`AskDecision::reason`] is a
/// short human-readable string for the log — never sent to the family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    /// Go ahead and send the ask.
    Send,
    /// Suppress the ask; the string says why (for the operator log).
    Skip(String),
}

impl AskDecision {
    pub fn should_send(&self) -> bool {
        matches!(self, AskDecision::Send)
    }
    pub fn reason(&self) -> &str {
        match self {
            AskDecision::Send => "ok",
            AskDecision::Skip(r) => r,
        }
    }
}

/// The path for the ask log: `<root>/plans/feedback-asks.jsonl`.
pub fn ask_log_path_for(project_root: &Path) -> PathBuf {
    project_root.join("plans").join("feedback-asks.jsonl")
}

/// Append one ask record.
pub fn append_ask(path: &Path, ask: &AskRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(ask.to_json_line().as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Load every ask record from the log (oldest first, file order).
pub fn load_asks(path: &Path) -> Vec<AskRecord> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(AskRecord::from_json_line)
        .collect()
}

/// The nag gate — decides whether Bruno may send tonight's ask.
///
/// Two rules, both from the task's "don't nag" requirement:
///
/// * **Max one ask per day.** If any prior ask landed on the same UTC calendar
///   day as `now_ms`, skip — one gentle question per evening, never a barrage.
/// * **Stop after silence.** If the two most recent asks both went unanswered
///   (`responded == false`), skip — the family has signalled they don't want to
///   rate right now, so we back off until they engage again (any future recorded
///   rating flips the latest ask to answered and reopens the gate).
///
/// Pure: takes the loaded ask history and the current time, returns a decision.
pub fn gate(asks: &[AskRecord], now_ms: i64) -> AskDecision {
    // Rule 1: one per day.
    let today = day_bucket(now_ms);
    if asks.iter().any(|a| day_bucket(a.ts) == today) {
        return AskDecision::Skip("already asked today".to_string());
    }

    // Rule 2: don't nag after two silent asks in a row. Look at the two most
    // recent asks by timestamp.
    let mut recent: Vec<&AskRecord> = asks.iter().collect();
    recent.sort_by_key(|a| a.ts);
    let n = recent.len();
    if n >= 2 && !recent[n - 1].responded && !recent[n - 2].responded {
        return AskDecision::Skip("two silent asks in a row — backing off".to_string());
    }

    AskDecision::Send
}

/// UTC day index (days since epoch) for an epoch-ms timestamp. Two timestamps in
/// the same UTC day share a bucket. Used only for the "one ask per day" rule.
fn day_bucket(ts_ms: i64) -> i64 {
    // 86_400_000 ms per day. Floor-divide so negative (pre-epoch, test-only)
    // values still bucket monotonically.
    ts_ms.div_euclid(86_400_000)
}

/// Mark the most recent ask in `path` as answered, if it isn't already. Called
/// when a rating is recorded so the gate knows the family engaged. Rewrites the
/// file (it is tiny — a handful of lines per week). No-op if the file is missing
/// or empty.
pub fn mark_latest_ask_answered(path: &Path) -> std::io::Result<()> {
    let mut asks = load_asks(path);
    if asks.is_empty() {
        return Ok(());
    }
    // Find the index of the newest ask by ts.
    let idx = asks
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.ts)
        .map(|(i, _)| i);
    if let Some(i) = idx {
        if asks[i].responded {
            return Ok(()); // already answered — nothing to rewrite
        }
        asks[i].responded = true;
        let body: String = asks
            .iter()
            .map(|a| a.to_json_line())
            .collect::<Vec<_>>()
            .join("\n");
        crate::atomic_file::write_atomic(path, format!("{body}\n").as_bytes())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Summarisation: ratings -> memory + plan briefing
// ---------------------------------------------------------------------------

/// The aggregated verdict for one dish across every rating it received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DishVerdict {
    /// The dish name (display form of the first rating seen for it).
    pub dish: String,
    /// Sum of verdict scores — positive is a winner, negative a loser.
    pub score: i64,
    /// How many ratings contributed.
    pub count: usize,
    /// Distinct non-empty notes, in first-seen order, for colour in the briefing.
    pub notes: Vec<String>,
}

/// Case/whitespace-folded key so `"Beef Stir-fry"` and `"beef stir-fry "` are the
/// same dish when aggregating.
pub fn dish_key(dish: &str) -> String {
    dish.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Aggregate ratings into one [`DishVerdict`] per dish, sorted best-to-worst
/// (ties broken by most-rated, then by name for stability).
pub fn summarize(ratings: &[MealRating]) -> Vec<DishVerdict> {
    use std::collections::BTreeMap;
    // Preserve first-seen display name + order via an insertion index.
    let mut order: BTreeMap<String, usize> = BTreeMap::new();
    let mut acc: BTreeMap<String, DishVerdict> = BTreeMap::new();
    for (i, r) in ratings.iter().enumerate() {
        let key = dish_key(&r.dish);
        order.entry(key.clone()).or_insert(i);
        let entry = acc.entry(key).or_insert_with(|| DishVerdict {
            dish: r.dish.trim().to_string(),
            score: 0,
            count: 0,
            notes: Vec::new(),
        });
        entry.score += r.verdict.score();
        entry.count += 1;
        let note = r.note.trim();
        if !note.is_empty() && !entry.notes.iter().any(|n| n == note) {
            entry.notes.push(note.to_string());
        }
    }
    let mut out: Vec<DishVerdict> = acc.into_values().collect();
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.count.cmp(&a.count))
            .then(a.dish.to_lowercase().cmp(&b.dish.to_lowercase()))
    });
    out
}

/// The dishes worth repeating (net-positive score).
pub fn winners(ratings: &[MealRating]) -> Vec<DishVerdict> {
    summarize(ratings).into_iter().filter(|d| d.score > 0).collect()
}

/// The dishes worth dropping/reworking (net-negative score).
pub fn losers(ratings: &[MealRating]) -> Vec<DishVerdict> {
    summarize(ratings)
        .into_iter()
        .filter(|d| d.score < 0)
        .collect()
}

/// Render the **plan briefing** block the Sunday drafter reads. This is the text
/// that makes the loop close: it names the winners to repeat and the losers to
/// drop, so the next draft can say "beef stir-fry is back by popular demand".
///
/// Returns an empty string when there are no ratings, so the prompt can guard
/// with a simple `if !briefing.is_empty()`.
pub fn render_plan_briefing(ratings: &[MealRating]) -> String {
    let wins = winners(ratings);
    let losses = losers(ratings);
    if wins.is_empty() && losses.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("Last week's dinner ratings from the family — repeat the wins, retire the flops:\n");
    for d in &wins {
        out.push_str(&format!(
            "- {} {} — a hit ({}). Bring it back.\n",
            Verdict::Liked.emoji(),
            d.dish,
            approving_count(d)
        ));
        if let Some(note) = d.notes.first() {
            out.push_str(&format!("    they said: \"{note}\"\n"));
        }
    }
    for d in &losses {
        out.push_str(&format!(
            "- {} {} — didn't land ({}). Drop it or rework it next week.\n",
            Verdict::Disliked.emoji(),
            d.dish,
            disapproving_count(d)
        ));
        if let Some(note) = d.notes.first() {
            out.push_str(&format!("    they said: \"{note}\"\n"));
        }
    }
    out
}

/// Render the warm one-liner that goes into Bruno's / Nora's **session summary**
/// so their conversational selves remember the family's tastes without reading
/// the file. Empty string when there's nothing to say yet.
pub fn render_session_note(ratings: &[MealRating]) -> String {
    let wins = winners(ratings);
    let losses = losers(ratings);
    if wins.is_empty() && losses.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    if !wins.is_empty() {
        let names = join_natural(&wins.iter().map(|d| d.dish.clone()).collect::<Vec<_>>());
        parts.push(format!("they loved {names}"));
    }
    if !losses.is_empty() {
        let names = join_natural(&losses.iter().map(|d| d.dish.clone()).collect::<Vec<_>>());
        parts.push(format!("{names} fell flat — skip it for now"));
    }
    format!(
        "Cooking notes from the family: {}.",
        join_natural_clauses(&parts)
    )
}

fn approving_count(d: &DishVerdict) -> String {
    if d.count == 1 {
        "1 thumbs-up".to_string()
    } else {
        format!("{} thumbs-up", d.count)
    }
}

fn disapproving_count(d: &DishVerdict) -> String {
    if d.count == 1 {
        "1 thumbs-down".to_string()
    } else {
        format!("{} thumbs-down", d.count)
    }
}

/// `["a"]` → `"a"`, `["a","b"]` → `"a and b"`, `["a","b","c"]` → `"a, b and c"`.
fn join_natural(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let head = items[..items.len() - 1].join(", ");
            format!("{head} and {}", items[items.len() - 1])
        }
    }
}

/// Join independent clauses with "; " so the session note reads as one sentence.
fn join_natural_clauses(parts: &[String]) -> String {
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rating(dish: &str, rater: &str, verdict: Verdict, note: &str, ts: i64) -> MealRating {
        MealRating {
            ts,
            dish: dish.to_string(),
            rater: rater.to_string(),
            verdict,
            note: note.to_string(),
        }
    }

    // ---- the ask (family voice) ----

    #[test]
    fn feedback_ask_is_family_voice_and_names_the_dish() {
        let line = compose_ask("Salmon");
        assert!(line.contains("salmon"), "should name the dish: {line}");
        assert!(line.contains("👍") && line.contains("👎"), "offers the quick path");
        assert!(line.contains("Bruno"), "signed in-persona");
        // No jargon leaks (docs/04).
        assert!(!line.to_lowercase().contains("task"));
        assert!(!line.contains("W29"));
        assert!(!line.contains("plans/"));
    }

    #[test]
    fn feedback_ask_drops_parenthetical_and_handles_empty() {
        assert!(compose_ask("Leftover risotto (flex)").contains("leftover risotto"));
        assert!(!compose_ask("Leftover risotto (flex)").contains("(flex)"));
        // A full plan slot with sides reads as just the headline dish.
        let salmon = compose_ask("Baked salmon, roasted potatoes, green beans");
        assert!(salmon.contains("baked salmon"), "{salmon}");
        assert!(!salmon.contains("potatoes"), "sides trimmed: {salmon}");
        // Empty dish still yields a warm, valid ask.
        let generic = compose_ask("   ");
        assert!(generic.contains("dinner") && generic.contains("Bruno"));
    }

    // ---- intent routing ----

    #[test]
    fn feedback_routing_reads_thumbs_up_and_down() {
        assert_eq!(parse_rating_reply("👍").unwrap().0, Verdict::Liked);
        assert_eq!(parse_rating_reply("👎").unwrap().0, Verdict::Disliked);
        // A bare thumb carries no note.
        assert_eq!(parse_rating_reply("👍").unwrap().1, "");
    }

    #[test]
    fn feedback_routing_reads_words_and_keeps_the_note() {
        assert_eq!(parse_rating_reply("loved it, best yet!").unwrap().0, Verdict::Loved);
        assert_eq!(parse_rating_reply("it was fine, meh").unwrap().0, Verdict::Meh);
        let (v, note) = parse_rating_reply("no thanks, too bland").unwrap();
        assert_eq!(v, Verdict::Disliked);
        assert_eq!(note, "no thanks, too bland");
    }

    #[test]
    fn feedback_routing_promotes_thumb_with_love_word_and_ignores_noise() {
        assert_eq!(
            parse_rating_reply("👍 amazing, so good").unwrap().0,
            Verdict::Loved
        );
        // No sentiment at all -> None, so we don't invent a rating.
        assert!(parse_rating_reply("what time is dinner?").is_none());
        assert!(parse_rating_reply("").is_none());
    }

    #[test]
    fn feedback_routing_mixed_signal_is_meh_with_note() {
        // Loves one part, hates another -> neutral, but the note carries nuance.
        let (v, note) = parse_rating_reply("loved the sauce but hated the beets").unwrap();
        assert_eq!(v, Verdict::Meh);
        assert!(note.contains("beets"));
    }

    // ---- persistence ----

    #[test]
    fn feedback_persistence_round_trips_through_jsonl() {
        let dir = tempdir().unwrap();
        let path = feedback_path_for(dir.path());
        assert_eq!(path.file_name().unwrap(), "feedback.jsonl");
        append_rating(&path, &rating("Beef stir-fry", "nadin", Verdict::Loved, "more please", 10))
            .unwrap();
        append_rating(&path, &rating("Beet salad", "luca", Verdict::Disliked, "", 20)).unwrap();
        let back = load_ratings(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].dish, "Beef stir-fry");
        assert_eq!(back[0].verdict, Verdict::Loved);
        assert_eq!(back[0].note, "more please");
        assert_eq!(back[1].verdict, Verdict::Disliked);
        // Missing file -> empty, not an error.
        assert!(load_ratings(&dir.path().join("nope.jsonl")).is_empty());
    }

    #[test]
    fn feedback_persistence_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = feedback_path_for(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "not json\n{\"dish\":\"Chili\",\"verdict\":\"liked\",\"rater\":\"x\",\"note\":\"\",\"ts\":1}\n\n",
        )
        .unwrap();
        let back = load_ratings(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].dish, "Chili");
    }

    // ---- nag gate / rate limits ----

    const DAY: i64 = 86_400_000;

    #[test]
    fn feedback_gate_allows_first_ask() {
        assert!(gate(&[], 5 * DAY).should_send());
    }

    #[test]
    fn feedback_gate_blocks_second_ask_same_day() {
        let asks = vec![AskRecord { ts: 5 * DAY + 1000, dish: "Salmon".into(), responded: true }];
        let d = gate(&asks, 5 * DAY + 9_000_000);
        assert!(!d.should_send());
        assert!(d.reason().contains("today"));
        // A new day reopens the gate.
        assert!(gate(&asks, 6 * DAY + 1000).should_send());
    }

    #[test]
    fn feedback_gate_backs_off_after_two_silent_asks() {
        // Two prior asks, both unanswered, on earlier days.
        let asks = vec![
            AskRecord { ts: 3 * DAY, dish: "Tofu".into(), responded: false },
            AskRecord { ts: 4 * DAY, dish: "Cod".into(), responded: false },
        ];
        let d = gate(&asks, 5 * DAY);
        assert!(!d.should_send(), "should back off, not nag");
        assert!(d.reason().contains("silent") || d.reason().contains("backing off"));
    }

    #[test]
    fn feedback_gate_resumes_when_family_answered_recently() {
        // One silence then one answer: the two most recent are not both silent.
        let asks = vec![
            AskRecord { ts: 3 * DAY, dish: "Tofu".into(), responded: false },
            AskRecord { ts: 4 * DAY, dish: "Cod".into(), responded: true },
        ];
        assert!(gate(&asks, 5 * DAY).should_send());
    }

    #[test]
    fn feedback_gate_ask_log_round_trips_and_marks_answered() {
        let dir = tempdir().unwrap();
        let path = ask_log_path_for(dir.path());
        append_ask(&path, &AskRecord { ts: 3 * DAY, dish: "Tofu".into(), responded: false })
            .unwrap();
        append_ask(&path, &AskRecord { ts: 4 * DAY, dish: "Cod".into(), responded: false })
            .unwrap();
        // Before answering: gate backs off.
        assert!(!gate(&load_asks(&path), 5 * DAY).should_send());
        // A rating arrives -> mark the latest ask answered.
        mark_latest_ask_answered(&path).unwrap();
        let asks = load_asks(&path);
        assert!(asks.iter().max_by_key(|a| a.ts).unwrap().responded);
        // Gate now reopens (most recent is answered).
        assert!(gate(&asks, 5 * DAY).should_send());
    }

    // ---- summarisation: the memory + the loop ----

    #[test]
    fn feedback_summary_aggregates_and_orders_winners_and_losers() {
        let ratings = vec![
            rating("Beef stir-fry", "nadin", Verdict::Loved, "", 1),
            rating("beef stir-fry", "luca", Verdict::Liked, "so good", 2), // same dish, folded
            rating("Beet salad", "nadin", Verdict::Disliked, "earthy", 3),
            rating("Cod bake", "luca", Verdict::Meh, "", 4),
        ];
        let summary = summarize(&ratings);
        assert_eq!(summary[0].dish, "Beef stir-fry");
        assert_eq!(summary[0].count, 2, "two ratings folded into one dish");
        assert_eq!(summary[0].score, 3);
        let w = winners(&ratings);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].dish, "Beef stir-fry");
        let l = losers(&ratings);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].dish, "Beet salad");
    }

    #[test]
    fn feedback_plan_briefing_names_winners_and_losers_for_the_draft() {
        // The load-bearing test: a seeded fixture -> the draft-facing briefing
        // actually mentions repeating the winner and dropping the loser.
        let ratings = vec![
            rating("Beef stir-fry", "nadin", Verdict::Loved, "more please", 1),
            rating("Beef stir-fry", "luca", Verdict::Liked, "", 2),
            rating("Beet salad", "nadin", Verdict::Disliked, "too earthy", 3),
        ];
        let briefing = render_plan_briefing(&ratings);
        assert!(briefing.contains("Beef stir-fry"), "names the winner");
        assert!(
            briefing.to_lowercase().contains("bring it back"),
            "tells the drafter to repeat it"
        );
        assert!(briefing.contains("Beet salad"), "names the loser");
        assert!(
            briefing.to_lowercase().contains("drop") || briefing.to_lowercase().contains("rework"),
            "tells the drafter to retire it"
        );
        assert!(briefing.contains("more please"), "carries the family's own words");
        // No ratings -> empty briefing (prompt guards on this).
        assert!(render_plan_briefing(&[]).is_empty());
    }

    #[test]
    fn feedback_session_note_is_one_warm_line() {
        let ratings = vec![
            rating("Beef stir-fry", "nadin", Verdict::Loved, "", 1),
            rating("Beet salad", "nadin", Verdict::Disliked, "", 2),
        ];
        let note = render_session_note(&ratings);
        assert!(note.to_lowercase().contains("loved"));
        assert!(note.contains("Beef stir-fry"));
        assert!(note.contains("Beet salad"));
        // Single line — no embedded newline.
        assert!(!note.contains('\n'));
        assert!(render_session_note(&[]).is_empty());
    }
}
