//! The shopping OVERLAY writer — the Rust half of the store the lock protocol already says the
//! engine is supposed to write.
//!
//! # The bug this closes (KNOWN-GAPS #7)
//!
//! A shopping add from the kiosk and the same sentence from Telegram used to land in two different
//! stores, and only one of them survives a rewrite:
//!
//! * **Kiosk / web** — `claw3d-bridge/src/gatewayCore.mjs` calls `this.addShopping`, which is
//!   `shoppingStore.add` (`claw3d-bridge/src/shoppingStore.mjs`): the durable per-week OVERLAY at
//!   `.casa/shopping/<week>.json`, `added[]`.
//! * **Telegram** — the listener's shopping short-circuit ran `fast_lane::run_fast_lane`, which
//!   edits the plan MARKDOWN (`## 4. Shopping list — by store`).
//!
//! The overlay exists precisely so a *manual* item survives a plan regeneration — that is what
//! `shoppingStore`'s own header says the `added[]` list is for. So an item jotted on a phone was
//! living in the artifact a rewrite discards, while the same item from the tablet was safe.
//!
//! This was never a design question. `docs/42-cross-process-lock-protocol.md` §1 names what the
//! `week-mutation` lock guards — *"shopping overlay, carry-forward, plan/meal writes, parked dinner
//! suggestions — from the browser, the kiosk, Telegram, or `wg`"* — and it names the overlay as the
//! store. The engine's Telegram lane simply did not behave like the contract's writer.
//!
//! # Fidelity to the Node store, and why it is written the way it is
//!
//! Two decisions matter more than the rest:
//!
//! 1. **The whole state document is round-tripped through `serde_json::Value`.** Only `seq` and
//!    `added` are touched. If this wrote a freshly-built object containing just the fields it knows,
//!    it would WIPE the family's crossed-off state (`checked`), their bumps, their dismissed rows and
//!    the carry-forward ledger every time somebody added an item from the phone. Preserving unknown
//!    keys is not tidiness; it is the difference between adding a row and destroying the list.
//! 2. **`seq` allocation and the append happen inside ONE section.** `docs/42` §11.1 is explicit that
//!    the id is a function of how many rows exist and the append is what makes one more exist: *split
//!    them and two writers compute the same id — a duplicate join key, permanently, which an auditor
//!    cannot tell from a genuine repeat.* So both live inside `with_week_mutation_lock`, the same
//!    lock the gateway takes (`projectLock`, rank 10).
//!
//! The shape mirrors `shoppingStore.mjs` exactly: `safeSegment` for the filename, the `{id, text,
//! store, key: "add:<seq>"}` item, the empty and 80-character guards, and "missing or corrupt →
//! a fresh empty state".

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::project_lock;

/// `.casa/shopping` — `SHOPPING_DIR_REL` in `shoppingStore.mjs`.
pub const SHOPPING_DIR_REL: &str = ".casa/shopping";
/// The store every manual item lands in when the caller does not name one.
pub const DEFAULT_STORE: &str = "Also getting";
/// `shoppingStore.add` refuses an item longer than this.
pub const MAX_ITEM_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// The item was empty after trimming — the Node store answers "Type an item to add first."
    EmptyItem,
    /// Longer than [`MAX_ITEM_CHARS`].
    TooLong,
    /// The `week-mutation` lock could not be taken. Carries the refusal's own detail.
    LockUnavailable { detail: String },
    /// The overlay could not be read or written.
    Io(String),
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyItem => write!(f, "Type an item to add first."),
            Self::TooLong => write!(f, "That's a bit long for a shopping item."),
            Self::LockUnavailable { detail } => write!(f, "the shopping list is busy: {detail}"),
            Self::Io(why) => write!(f, "could not update the shopping list: {why}"),
        }
    }
}

/// The item that was appended, so the caller can report what actually landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedItem {
    pub id: u64,
    pub text: String,
    pub store: String,
    pub key: String,
}

/// `normalizeWeekKey` — pull the ISO week token out of a name and pad it to two digits, so a
/// hand-typed `2026-W8` addresses the same overlay as discovery's `2026-W08`. `None` when the text
/// carries no such token, which is what makes the caller fall through to the next candidate.
pub fn normalize_week_key(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    // Scan for `YYYY-W<1-2 digits>` (also accepting `YYYY-Www`), lowercase w included.
    for i in 0..bytes.len() {
        if i + 6 > bytes.len() {
            break;
        }
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        let year = &raw[i..i + 4];
        if !year.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if i + 4 >= bytes.len() || bytes[i + 4] != b'-' {
            continue;
        }
        if i + 5 >= bytes.len() || !bytes[i + 5].eq_ignore_ascii_case(&b'w') {
            continue;
        }
        let mut j = i + 6;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
            if j - (i + 6) >= 2 {
                break;
            }
        }
        if j == i + 6 {
            continue; // `W` with no digits
        }
        let n: u32 = raw[i + 6..j].parse().ok()?;
        if n == 0 || n > 53 {
            continue;
        }
        return Some(format!("{year}-W{n:02}"));
    }
    None
}

/// `safeSegment` — a week key becomes a flat, traversal-proof filename segment.
pub fn safe_segment(week_key: &str) -> String {
    let cleaned: String = week_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "current".to_string()
    } else {
        cleaned
    }
}

/// `<root>/.casa/shopping/<safeSegment(weekKey)>.json`.
pub fn overlay_path(root: &Path, week_key: &str) -> PathBuf {
    root.join(SHOPPING_DIR_REL)
        .join(format!("{}.json", safe_segment(week_key)))
}

/// A fresh overlay, byte-shape-compatible with `shoppingStore.mjs`'s `EMPTY`.
fn empty_state() -> Value {
    json!({
        "version": 1,
        "checked": {},
        "added": [],
        "bumps": {},
        "dismissed": {},
        "seq": 0,
        "carriedWeeks": {},
    })
}

/// Read the overlay, or a fresh empty state. Mirroring `shoppingStore.state`: missing OR corrupt
/// both read as empty rather than as an error, because the reader is what the Week view renders
/// from and a corrupt overlay must not take the whole list down.
fn read_state(path: &Path) -> Result<Value, OverlayError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str::<Value>(&text).unwrap_or_else(|_| empty_state())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty_state()),
        Err(e) => Err(OverlayError::Io(e.to_string())),
    }
}

/// Write the overlay atomically: stage beside the target, then rename over it. A partial write to
/// the live path would lose the family's list, which is the exact failure this module exists to
/// remove.
fn write_state(path: &Path, state: &Value) -> Result<(), OverlayError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| OverlayError::Io(e.to_string()))?;
    }
    let body = serde_json::to_string(state).map_err(|e| OverlayError::Io(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| OverlayError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| OverlayError::Io(e.to_string()))
}

/// Append one item to the week's overlay, inside the `week-mutation` lock.
///
/// The caller supplies the week candidates in the SAME precedence the gateway uses
/// (`overlayWeekKey`): the plan FILE name first, then the plan's declared week key, then the ISO
/// week of the current calendar week. Passing them in order keeps the two implementations on one
/// derivation instead of two.
pub fn add_item(
    root: &Path,
    week_candidates: &[&str],
    text: &str,
    store: Option<&str>,
    by: Option<&str>,
) -> Result<AddedItem, OverlayError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(OverlayError::EmptyItem);
    }
    if trimmed.chars().count() > MAX_ITEM_CHARS {
        return Err(OverlayError::TooLong);
    }
    let week_key = week_candidates
        .iter()
        .find_map(|c| normalize_week_key(c))
        .ok_or_else(|| OverlayError::Io("no ISO week token in any candidate".into()))?;
    let week_key = week_key.as_str();
    let store = store
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_STORE)
        .to_string();

    let path = overlay_path(root, week_key);
    let body = || -> Result<AddedItem, OverlayError> {
        let mut state = read_state(&path)?;
        let next = state.get("seq").and_then(Value::as_u64).unwrap_or(0) + 1;
        state["seq"] = json!(next);
        let mut item = json!({
            "id": next,
            "text": trimmed,
            "store": store,
            "key": format!("add:{next}"),
        });
        if let Some(by) = by.map(str::trim).filter(|b| !b.is_empty()) {
            item["by"] = json!(by);
        }
        match state.get_mut("added").and_then(Value::as_array_mut) {
            Some(rows) => rows.push(item),
            None => state["added"] = json!([item]),
        }
        write_state(&path, &state)?;
        Ok(AddedItem {
            id: next,
            text: trimmed.to_string(),
            store: store.clone(),
            key: format!("add:{next}"),
        })
    };

    // ALLOCATION AND APPEND IN ONE SECTION (docs/42 §11.1).
    match project_lock::with_week_mutation_lock(root, body) {
        Ok(completed) => completed.out,
        Err(refusal) => Err(OverlayError::LockUnavailable {
            detail: refusal.detail().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "casa-overlay-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_pads_the_week_and_reads_a_plan_name() {
        assert_eq!(normalize_week_key("2026-W8"), Some("2026-W08".into()));
        assert_eq!(normalize_week_key("2026-W08"), Some("2026-W08".into()));
        assert_eq!(
            normalize_week_key("2026-W40-family-plan.md"),
            Some("2026-W40".into())
        );
        assert_eq!(normalize_week_key("plans/2026-W41-bruno-recipes.md"), Some("2026-W41".into()));
        assert_eq!(normalize_week_key("family-plan.md"), None);
        assert_eq!(normalize_week_key(""), None);
        // A bare year must not be mistaken for a week.
        assert_eq!(normalize_week_key("2026"), None);
        // Week numbers out of range are not weeks.
        assert_eq!(normalize_week_key("2026-W77"), None);
    }

    #[test]
    fn safe_segment_cannot_traverse() {
        assert_eq!(safe_segment("2026-W40"), "2026-W40");
        // `.` is NOT in Node's safe set either (`/[^A-Za-z0-9_-]/`), so it becomes `_`.
        assert_eq!(safe_segment("../../etc/passwd"), "______etc_passwd");
        assert_eq!(safe_segment(""), "current");
        // The path never escapes the overlay directory.
        let p = overlay_path(Path::new("/tmp/root"), "../../escape");
        assert!(p.starts_with("/tmp/root/.casa/shopping"), "got {:?}", p);
    }

    #[test]
    fn a_missing_overlay_starts_empty_and_the_item_matches_the_node_shape() {
        let root = scratch("shape");
        let added = add_item(&root, &["2026-W40"], "olive oil", None, Some("Luca")).unwrap();
        assert_eq!(added.id, 1);
        assert_eq!(added.key, "add:1");
        assert_eq!(added.store, "Also getting");

        let raw = std::fs::read_to_string(overlay_path(&root, "2026-W40")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["seq"], 1);
        let item = &v["added"][0];
        assert_eq!(item["id"], 1);
        assert_eq!(item["text"], "olive oil");
        assert_eq!(item["store"], "Also getting");
        assert_eq!(item["key"], "add:1");
        assert_eq!(item["by"], "Luca");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// THE REASON THIS MODULE ROUND-TRIPS A `Value` RATHER THAN BUILDING A FRESH OBJECT.
    /// If the writer emitted only the fields it knows, adding milk from the phone would silently
    /// un-cross everything the family had ticked off and drop their bumps and carry-forward ledger.
    #[test]
    fn adding_an_item_preserves_everything_else_in_the_overlay() {
        let root = scratch("preserve");
        let path = overlay_path(&root, "2026-W40");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "version": 1,
                "checked": { "plan:Monday:Carrots": true },
                "added": [{ "id": 1, "text": "wine", "store": "Also getting", "key": "add:1" }],
                "bumps": { "plan:Tuesday:Milk": 6 },
                "dismissed": { "plan:Sunday:Basil": true },
                "seq": 1,
                "carriedWeeks": { "2026-W39": ["plan:Monday:Carrots"] },
                "anUnknownFutureKey": { "keep": "me" }
            }))
            .unwrap(),
        )
        .unwrap();

        let added = add_item(&root, &["2026-W40"], "milk", None, None).unwrap();
        assert_eq!(added.id, 2, "seq continues from the stored counter");

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["checked"]["plan:Monday:Carrots"], true, "crossed-off state preserved");
        assert_eq!(v["bumps"]["plan:Tuesday:Milk"], 6, "bumps preserved");
        assert_eq!(v["dismissed"]["plan:Sunday:Basil"], true, "dismissals preserved");
        assert_eq!(v["carriedWeeks"]["2026-W39"][0], "plan:Monday:Carrots");
        assert_eq!(v["anUnknownFutureKey"]["keep"], "me", "unknown keys preserved");
        assert_eq!(v["added"].as_array().unwrap().len(), 2, "the new row is appended, not replacing");
        assert_eq!(v["added"][0]["text"], "wine");
        assert_eq!(v["added"][1]["text"], "milk");
        assert_eq!(v["added"][1]["key"], "add:2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ids_are_distinct_across_adds() {
        let root = scratch("ids");
        for (i, item) in ["a", "b", "c"].iter().enumerate() {
            let added = add_item(&root, &["2026-W40"], item, None, None).unwrap();
            assert_eq!(added.id, i as u64 + 1);
            assert_eq!(added.key, format!("add:{}", i + 1));
        }
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(overlay_path(&root, "2026-W40")).unwrap())
                .unwrap();
        let keys: Vec<String> = v["added"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap().to_string())
            .collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(keys.len(), unique.len(), "duplicate join key: {keys:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_guards_match_the_node_store() {
        let root = scratch("guards");
        assert_eq!(
            add_item(&root, &["2026-W40"], "   ", None, None),
            Err(OverlayError::EmptyItem)
        );
        let long = "x".repeat(81);
        assert_eq!(
            add_item(&root, &["2026-W40"], &long, None, None),
            Err(OverlayError::TooLong)
        );
        // Exactly 80 is accepted — the boundary, not one either side of it.
        assert!(add_item(&root, &["2026-W40"], &"x".repeat(80), None, None).is_ok());
        // A named store is honoured; whitespace falls back to the default.
        let a = add_item(&root, &["2026-W40"], "flour", Some("  Bakery "), None).unwrap();
        assert_eq!(a.store, "Bakery");
        let b = add_item(&root, &["2026-W40"], "salt", Some("   "), None).unwrap();
        assert_eq!(b.store, "Also getting");
        // No week token anywhere → refuse rather than write to a guessed file.
        assert!(add_item(&root, &["plans/", ""], "rice", None, None).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_candidates_are_tried_in_order() {
        let root = scratch("order");
        add_item(
            &root,
            &["not-a-week", "2026-W40-family-plan.md", "2026-W41"],
            "beans",
            None,
            None,
        )
        .unwrap();
        assert!(overlay_path(&root, "2026-W40").exists(), "the plan file name wins");
        assert!(!overlay_path(&root, "2026-W41").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_overlay_reads_as_empty_rather_than_failing() {
        let root = scratch("corrupt");
        let path = overlay_path(&root, "2026-W40");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json at all").unwrap();
        let added = add_item(&root, &["2026-W40"], "rice", None, None).unwrap();
        assert_eq!(added.id, 1, "a corrupt file does not wedge the list");
        let _ = std::fs::remove_dir_all(&root);
    }
}
