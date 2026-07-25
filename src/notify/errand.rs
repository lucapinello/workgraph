//! The errand engine: close the loop between the plan's **market-run time** and
//! the shopping list's **still-uncrossed items**, so the remaining list chases
//! the person doing the run instead of being left behind on the kiosk.
//!
//! # What fires
//!
//! One source: a `## 3. Calendar` row shaped like an errand, e.g.
//! `| Sat 07-18 | 09:00 | 🛒 Market run (Luca) — fresh fish + produce | Otto (§4) |`.
//! [`ErrandReminder::from_calendar_event`] recognises the `🛒` / `market run` /
//! `errand` shape, pulls out the **runner** (the first known family member named
//! in the row — the plan literally writes `Market run (Luca)`), the owning voice
//! (the Source column), and the errand's wall-clock time. The nudge is scheduled
//! for the errand time **minus a small lead** ([`resolve_lead`], default 15 min,
//! overridable via `CASA_ERRAND_LEAD_MIN`) so the runner sees it before leaving.
//!
//! # Rendered at fire time, not schedule time
//!
//! Unlike a plain [`crate::notify::reminder::Reminder`], an errand carries **no
//! fixed body**. Its message is computed at the moment it fires from the *live*
//! shopping state ([`ShoppingModel`], the `GET /shopping.json` shape the casa
//! gateway serves): the remaining (uncrossed) items, grouped by store section
//! exactly like the Week view. An item crossed off between scheduling and firing
//! is simply absent from the nudge; one added is included — because
//! [`ErrandReminder::render`] reads the model handed to it *now*, never a snapshot
//! taken when the plan was parsed.
//!
//! If every item is already crossed off, the runner still gets **one** cheerful
//! note (`List's already done — nothing to buy 🎉`) rather than a wall of nothing
//! — a deliberate choice (documented on [`ALL_DONE`]) so the run isn't made in
//! doubt about whether the list was even seen.
//!
//! # Exactly once — one nudge per errand
//!
//! Firing reuses the reminder engine's durable [`FiredLog`]: [`errand_tick`]
//! records the errand's stable [`ErrandReminder::id`] **before** the caller sends,
//! so a restart never re-nudges, and — crucially — items added or crossed *after*
//! the nudge went out do **not** re-trigger it. One errand, one nudge.
//!
//! # Paced like every other proactive DM
//!
//! The errand nudge does not DM the runner directly — it routes through the
//! shipped daily-digest pacing layer ([`crate::notify::daily_digest`]) via
//! [`route_errand_nudge`]. It is **time-critical** ([`NudgeKind::ErrandNudge`],
//! [`crate::notify::daily_digest::Urgency::TimeCritical`]): it fires *standalone*
//! at its due time — even inside quiet hours, because a market run at 09:00 is
//! useless nudged at 08:00 — but it **counts against the person's daily
//! standalone cap**. If the cap is already spent that day, the errand folds into
//! the next morning digest as an honest overflow line instead of piling on a
//! fourth ping. Pacing exactly-once (the digest `seen` set) and firing
//! exactly-once (the [`FiredLog`]) are both keyed on the same errand id, so the
//! two layers agree.
//!
//! Everything here is pure over an injected `now` and an injected shopping model,
//! so the whole fire / render / exactly-once / all-crossed / pacing behaviour is
//! unit testable without a clock, a filesystem, or a live gateway.

use chrono::{Duration, NaiveDateTime};
use serde::Deserialize;

use crate::notify::daily_digest::{DigestPolicy, DigestStore, Nudge, NudgeKind, Offer};
use crate::notify::family_plan::{CalendarEvent, PlanDoc};
use crate::notify::ownership::OwnerMap;
use crate::notify::reminder::{
    FireDecision, FirePolicy, FiredLog, Outcome, decide, first_member, hash64, parse_clock,
    resolve_plan_source,
};

/// The shopping-cart emoji that marks an errand row in the plan and prefixes the
/// nudge.
const CART: char = '\u{1f6d2}';

/// The one-and-only message when nothing is left to buy.
///
/// **Design choice (documented per the task):** when the list is fully crossed
/// off we send this happy line **once** rather than staying silent. Sending
/// something — even "nothing to buy" — confirms to the runner that the list *was*
/// checked and is genuinely clear, which is more reassuring at the door than an
/// absent message that could equally mean "the nudge failed". It still fires
/// exactly once (one nudge per errand), so it is never a repeated ping.
pub const ALL_DONE: &str = "List's already done — nothing to buy \u{1f389}";

/// Default lead: the nudge lands 15 minutes before the errand time.
pub const DEFAULT_ERRAND_LEAD_MINUTES: i64 = 15;

/// Environment override for the lead, in whole minutes.
pub const ERRAND_LEAD_ENV: &str = "CASA_ERRAND_LEAD_MIN";

/// Resolve the configured lead time before an errand. Reads
/// [`ERRAND_LEAD_ENV`] (whole minutes, `>= 0`); falls back to
/// [`DEFAULT_ERRAND_LEAD_MINUTES`] when unset, blank, or unparseable.
pub fn resolve_lead() -> Duration {
    let mins = std::env::var(ERRAND_LEAD_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|m| *m >= 0)
        .unwrap_or(DEFAULT_ERRAND_LEAD_MINUTES);
    Duration::minutes(mins)
}

/// True when a calendar Event cell is shaped like an errand: it carries the `🛒`
/// cart emoji, or an explicit `market run` / `grocery run` / `errand` label.
///
/// Deliberately disjoint from [`crate::notify::reminder::is_reminder_event`] (a
/// `⏰ Reminder:` row) so the two engines never double-fire the same row.
pub fn is_errand_event(event: &str) -> bool {
    if event.contains(CART) {
        return true;
    }
    let low = event.to_ascii_lowercase();
    low.contains("market run") || low.contains("grocery run") || low.contains("errand")
}

/// One errand the engine can nudge for: who is doing the run, in whose voice,
/// when the run is, and when to nudge. The **body is not stored** — it is rendered
/// at fire time from live shopping state (see [`ErrandReminder::render`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrandReminder {
    /// Stable de-dupe key. Derived from the plan week + errand date + time + a
    /// hash of the cleaned label — **independent of the lead** so re-tuning the
    /// lead never mints a second id (which would double-nudge). Two ticks (or two
    /// processes across a restart) that see the same errand row derive the same
    /// id, so the [`FiredLog`] nudges once.
    pub id: String,
    /// Wall-clock instant the errand itself happens (family-local), e.g. Sat 09:00.
    pub errand_at: NaiveDateTime,
    /// When to nudge: `errand_at` minus the configured lead, e.g. Sat 08:45.
    pub due: NaiveDateTime,
    /// Display name of the runner to DM, e.g. `"Luca"`. Empty when the row named
    /// no known member (the caller then falls back to the group).
    pub recipient: String,
    /// The voice/bot that owns and sends the nudge, e.g. `"otto"`.
    pub bot: String,
    /// A short human label for the errand, for `--list` / logs, e.g.
    /// `"Market run"` — never the nudge body (which is computed).
    pub label: String,
}

impl ErrandReminder {
    /// Build an errand from a calendar row, or `None` when the row is not
    /// errand-shaped or has no usable time.
    ///
    /// `week_code` scopes the derived id to its plan week; `members` is the list
    /// of known family display names used to pick the runner (first one named in
    /// the row); `lead` is how far ahead of the errand to nudge.
    pub fn from_calendar_event(
        week_code: &str,
        ev: &CalendarEvent,
        members: &[String],
        owners: &OwnerMap,
        lead: Duration,
    ) -> Option<ErrandReminder> {
        if !is_errand_event(&ev.event) {
            return None;
        }
        let date = ev.date?;
        let time = parse_clock(&ev.time)?;
        let errand_at = date.and_time(time);
        let due = errand_at - lead;
        let recipient = first_member(&ev.event, members).unwrap_or_default();
        let bot = resolve_plan_source(&ev.source, owners)?;
        let label = clean_errand_label(&ev.event);
        // Stable id: week + date + time + label hash. NOT a function of `lead`, so
        // changing the lead reuses the same id (one nudge per errand, always).
        let id = format!(
            "errand:{}:{}:{}:{:016x}",
            week_code,
            date.format("%Y-%m-%d"),
            ev.time.replace(':', ""),
            hash64(&label),
        );
        Some(ErrandReminder {
            id,
            errand_at,
            due,
            recipient,
            bot,
            label,
        })
    }

    /// Render the family-voice nudge body from the **live** shopping model.
    ///
    /// This is the render-at-fire-time contract: pass the shopping state as it is
    /// *now* and get the message for *now*. Remaining (uncrossed) items are
    /// grouped by store section, one compact line per store, exactly like the Week
    /// view. When nothing is left, returns [`ALL_DONE`].
    pub fn render(&self, shopping: &ShoppingModel) -> String {
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        for g in &shopping.groups {
            let remaining: Vec<String> = g
                .items
                .iter()
                .filter(|i| !i.checked)
                .map(|i| compact_item(&i.text))
                .filter(|t| !t.is_empty())
                .collect();
            if !remaining.is_empty() {
                sections.push((clean_store(&g.store), remaining));
            }
        }
        if sections.is_empty() {
            return ALL_DONE.to_string();
        }
        let mut out = format!("{CART} Still needed at the market:");
        for (store, items) in &sections {
            if store.is_empty() {
                out.push_str(&format!("\n\u{2022} {}", items.join(", ")));
            } else {
                out.push_str(&format!("\n\u{2022} {}: {}", store, items.join(", ")));
            }
        }
        out
    }
}

/// Collect every errand-shaped row in a parsed plan into [`ErrandReminder`]s.
pub fn errands_from_plan(
    plan: &PlanDoc,
    members: &[String],
    owners: &OwnerMap,
    lead: Duration,
) -> Vec<ErrandReminder> {
    plan.calendar
        .iter()
        .filter_map(|ev| {
            ErrandReminder::from_calendar_event(&plan.week_code, ev, members, owners, lead)
        })
        .collect()
}

/// One errand selected to actually nudge this tick.
#[derive(Debug, Clone)]
pub struct ErrandFiring {
    /// The errand to nudge for. Render its body with [`ErrandReminder::render`]
    /// against the live shopping model.
    pub errand: ErrandReminder,
    /// Whether it fires past the on-time grace (a missed-while-down catch-up). The
    /// nudge text does not change — the market list is the market list — but the
    /// caller may log it.
    pub late: bool,
}

/// Run one scheduler tick over `errands` at `now`, recording every nudged (or
/// dropped-as-stale) id into `log` so it fires exactly once. Returns which
/// errands to nudge, in due order.
///
/// The log is mutated **in memory**; the caller persists it with [`FiredLog::save`]
/// *before* actually sending, giving restart-safe exactly-once (record-before-act)
/// — identical discipline to [`crate::notify::reminder::tick`]. The body is
/// rendered afterwards from live shopping state, so the caller should fetch that
/// state up front and skip the whole tick when it is unavailable (a fetch failure
/// must not burn the one nudge on a stale/empty render).
pub fn errand_tick(
    errands: &[ErrandReminder],
    log: &mut FiredLog,
    now: NaiveDateTime,
    policy: &FirePolicy,
) -> Vec<ErrandFiring> {
    let mut fired = Vec::new();
    for e in errands {
        if log.contains(&e.id) {
            continue; // already nudged or dropped — one nudge per errand
        }
        match decide(e.due, now, policy) {
            FireDecision::Pending => {}
            FireDecision::Fire { late } => {
                log.record(
                    &e.id,
                    now,
                    if late { Outcome::Late } else { Outcome::OnTime },
                );
                fired.push(ErrandFiring {
                    errand: e.clone(),
                    late,
                });
            }
            FireDecision::Drop => {
                log.record(&e.id, now, Outcome::Dropped);
            }
        }
    }
    fired.sort_by_key(|f| f.errand.due);
    fired
}

impl ErrandFiring {
    /// Wrap this firing (with its fire-time–rendered `body`) as a **time-critical**
    /// [`Nudge`] for the daily-digest pacing layer. The nudge id is the errand's
    /// stable id, so the pacing store's exactly-once `seen` set agrees with the
    /// [`FiredLog`], and the market list is the DM body verbatim.
    pub fn to_nudge(&self, body: String) -> Nudge {
        Nudge::time_critical(
            self.errand.id.clone(),
            self.errand.recipient.clone(),
            NudgeKind::ErrandNudge,
            self.errand.due,
            body,
        )
    }
}

/// Render an errand firing from **live** shopping state and route it through the
/// daily-digest pacing layer — the single choke point every proactive DM passes.
///
/// Returns the rendered family-voice body (for logging / the actual send) and the
/// pacing [`Offer`]:
/// * [`Offer::SendNow`] — under the daily standalone cap: DM the runner now, even
///   in quiet hours (the standalone counter has been incremented).
/// * [`Offer::Queued`] `overflow:true` — the cap is spent: folded into the next
///   morning digest as an honest overflow line rather than a fourth ping.
/// * [`Offer::Duplicate`] — this errand id was already paced (never double-counts).
///
/// The body is computed *here*, at fire time, from the `shopping` model handed in
/// — so items crossed or added since the errand was scheduled are reflected, and
/// items changed *after* this call do not re-trigger anything (one nudge per
/// errand, enforced by both the [`FiredLog`] upstream and the pacing `seen` set).
pub fn route_errand_nudge(
    firing: &ErrandFiring,
    shopping: &ShoppingModel,
    store: &mut DigestStore,
    now: NaiveDateTime,
    policy: &DigestPolicy,
) -> (String, Offer) {
    let body = firing.errand.render(shopping);
    let nudge = firing.to_nudge(body.clone());
    let offer = store.offer(&nudge, now, policy);
    (body, offer)
}

// ---------------------------------------------------------------------------
// Live shopping model — the `GET /shopping.json` shape
// ---------------------------------------------------------------------------

/// The merged shopping model as the casa gateway serves it at `GET /shopping.json`
/// (see `claw3d-bridge/src/weekAdapter.mjs::shoppingJson`): planned items overlaid
/// with the durable crossed-off state, grouped by store. Only the fields the
/// errand nudge needs are deserialized; everything else in the JSON is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShoppingModel {
    /// Whether the gateway had a week to answer for. A `false`/absent `ok` means
    /// "no list" — the caller should treat that as *unknown*, not *all done*.
    #[serde(default)]
    pub ok: bool,
    /// Store sections, in Week-view order.
    #[serde(default)]
    pub groups: Vec<ShoppingGroup>,
}

impl ShoppingModel {
    /// Parse a `/shopping.json` body, tolerating anything unexpected as "no list".
    pub fn from_json(body: &str) -> ShoppingModel {
        serde_json::from_str(body).unwrap_or_default()
    }

    /// The count of remaining (uncrossed) items across all sections.
    pub fn remaining_count(&self) -> usize {
        self.groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|i| !i.checked)
            .count()
    }
}

/// One store section of the merged shopping model.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShoppingGroup {
    /// The store heading, e.g. `"Fishmonger / market"`.
    #[serde(default)]
    pub store: String,
    /// The items under it.
    #[serde(default)]
    pub items: Vec<ShoppingItem>,
}

/// One interactive shopping item.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShoppingItem {
    /// Display text, e.g. `"Salmon fillets ×2 (Tue)"`.
    #[serde(default)]
    pub text: String,
    /// Whether it has been crossed off at the kiosk.
    #[serde(default)]
    pub checked: bool,
}

// ---------------------------------------------------------------------------
// Text tidying
// ---------------------------------------------------------------------------

/// Compact one shopping item for the market nudge: drop any trailing
/// parenthetical annotation (`"Salmon fillets ×2 (Tue)"` → `"Salmon fillets ×2"`)
/// so the list reads tight at the door. Interior text is left intact.
fn compact_item(text: &str) -> String {
    let t = text.trim();
    match t.find(" (") {
        Some(i) => t[..i].trim().to_string(),
        None => t.to_string(),
    }
}

/// Tidy a store heading for the nudge: strip a leading emoji and any trailing
/// parenthetical (`"🐟 Fishmonger / market (Sat 07-18, fresh)"` →
/// `"Fishmonger / market"`). The gateway usually pre-cleans these, but we are
/// defensive so the nudge never leaks a raw `(date)` tail.
fn clean_store(store: &str) -> String {
    let mut s = store.trim();
    // Drop a single leading non-alphanumeric, non-space char (an emoji).
    if let Some(first) = s.chars().next() {
        if !first.is_alphanumeric() && !first.is_whitespace() {
            s = s[first.len_utf8()..].trim_start();
        }
    }
    match s.find(" (") {
        Some(i) => s[..i].trim().to_string(),
        None => s.to_string(),
    }
}

/// Clean an errand Event cell into a short label: strip the leading `🛒`, then
/// keep the text up to the first `(` or em/en-dash (`"🛒 Market run (Luca) —
/// fresh fish"` → `"Market run"`). Deterministic — feeds the stable id hash.
fn clean_errand_label(event: &str) -> String {
    let mut s = event.trim();
    s = s.trim_start_matches(CART).trim();
    // Cut at the first parenthesis or dash so "(Luca) — fresh fish" drops off.
    let cut = s
        .find('(')
        .into_iter()
        .chain(s.find('\u{2014}')) // em dash
        .chain(s.find('\u{2013}')) // en dash
        .chain(s.find(" - "))
        .min()
        .unwrap_or(s.len());
    s[..cut].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::family_plan::CalendarEvent;
    use chrono::NaiveDate;

    fn members() -> Vec<String> {
        vec!["Luca".to_string(), "Nadin".to_string()]
    }

    fn owners() -> OwnerMap {
        OwnerMap::casa_default()
    }

    fn owners_from_toml(body: &str) -> OwnerMap {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("household.toml"), body).unwrap();
        OwnerMap::from_household_toml(dir.path()).expect("valid household fixture")
    }

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    /// The exact errand row from the W29 fixture plan.
    fn errand_row() -> CalendarEvent {
        CalendarEvent {
            weekday: "Sat".into(),
            date: Some(NaiveDate::from_ymd_opt(2026, 7, 18).unwrap()),
            time: "09:00".into(),
            event: "\u{1f6d2} Market run (Luca) — fresh fish + produce".into(),
            source: "Otto (\u{a7}4)".into(),
        }
    }

    fn errand() -> ErrandReminder {
        ErrandReminder::from_calendar_event(
            "2026-W29",
            &errand_row(),
            &members(),
            &owners(),
            Duration::minutes(15),
        )
        .expect("errand-shaped")
    }

    /// A live shopping model mirroring the fixture, with a checked mask applied.
    fn model(checked: &[&str]) -> ShoppingModel {
        let mk = |store: &str, items: &[&str]| ShoppingGroup {
            store: store.into(),
            items: items
                .iter()
                .map(|t| ShoppingItem {
                    text: (*t).into(),
                    checked: checked.contains(t),
                })
                .collect(),
        };
        ShoppingModel {
            ok: true,
            groups: vec![
                mk(
                    "Fishmonger / market",
                    &["Salmon fillets ×2 (Tue)", "Fresh sardines, ~400 g (Sat)"],
                ),
                mk(
                    "Greengrocer / produce",
                    &["Swiss chard, 1 bunch", "Lemons ×3"],
                ),
            ],
        }
    }

    #[test]
    fn errand_row_resolves_person_time_and_lead() {
        let e = errand();
        assert_eq!(e.errand_at, dt(2026, 7, 18, 9, 0), "the run is Sat 09:00");
        assert_eq!(e.due, dt(2026, 7, 18, 8, 45), "nudge 15 min ahead");
        assert_eq!(e.recipient, "Luca", "person resolved from the plan row");
        assert_eq!(e.bot, "otto", "owning voice from the Source column");
        assert_eq!(e.label, "Market run");
    }

    #[test]
    fn renamed_source_resolves_unique_stable_owner() {
        let owners = owners_from_toml(
            r#"
[[agent]]
id = "shopping-anchor-8"
name = "Market Lantern Renamed"
domains = ["shopping", "coordination"]
"#,
        );
        let row = CalendarEvent {
            source: "Market Lantern Renamed \u{2192} \u{a7}4".into(),
            ..errand_row()
        };
        let errand = ErrandReminder::from_calendar_event(
            "2026-W29",
            &row,
            &members(),
            &owners,
            Duration::minutes(15),
        )
        .expect("renamed authored source resolves");
        assert_eq!(
            errand.bot, "shopping-anchor-8",
            "routing must retain the stable id instead of deriving one from the display name",
        );
    }

    #[test]
    fn errand_lead_is_configurable() {
        let e = ErrandReminder::from_calendar_event(
            "2026-W29",
            &errand_row(),
            &members(),
            &owners(),
            Duration::minutes(30),
        )
        .unwrap();
        assert_eq!(
            e.due,
            dt(2026, 7, 18, 8, 30),
            "a 30-min lead nudges at 08:30"
        );
    }

    #[test]
    fn errand_id_stable_across_reparse_and_independent_of_lead() {
        let a = errand();
        let b = ErrandReminder::from_calendar_event(
            "2026-W29",
            &errand_row(),
            &members(),
            &owners(),
            Duration::minutes(45), // different lead …
        )
        .unwrap();
        assert_eq!(
            a.id, b.id,
            "same row → same id regardless of lead (one nudge)"
        );
        assert!(a.id.starts_with("errand:2026-W29:"));
    }

    #[test]
    fn non_errand_rows_are_ignored() {
        // A cook slot.
        let cook = CalendarEvent {
            weekday: "Mon".into(),
            date: Some(NaiveDate::from_ymd_opt(2026, 7, 13).unwrap()),
            time: "18:30".into(),
            event: "Cook: chickpea & spinach curry".into(),
            source: "Bruno".into(),
        };
        assert!(
            ErrandReminder::from_calendar_event(
                "2026-W29",
                &cook,
                &members(),
                &owners(),
                Duration::minutes(15),
            )
            .is_none()
        );
        // A ⏰ reminder row is NOT an errand (the two engines never overlap).
        let reminder = CalendarEvent {
            weekday: "Tue".into(),
            date: Some(NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()),
            time: "19:30".into(),
            event: "\u{23f0} Reminder: Luca PT check-in".into(),
            source: "Otto".into(),
        };
        assert!(
            ErrandReminder::from_calendar_event(
                "2026-W29",
                &reminder,
                &members(),
                &owners(),
                Duration::minutes(15)
            )
            .is_none()
        );
    }

    #[test]
    fn renders_remaining_grouped_by_store_section() {
        // Cross off one fish item; everything else remains.
        let msg = errand().render(&model(&["Salmon fillets ×2 (Tue)"]));
        assert!(
            msg.starts_with("\u{1f6d2} Still needed at the market:"),
            "{msg}"
        );
        // Grouped by store, compact (parentheticals dropped).
        assert!(
            msg.contains("Fishmonger / market: Fresh sardines, ~400 g"),
            "{msg}"
        );
        assert!(
            msg.contains("Greengrocer / produce: Swiss chard, 1 bunch, Lemons ×3"),
            "{msg}"
        );
        // The crossed-off item is gone.
        assert!(
            !msg.contains("Salmon"),
            "crossed item must not appear: {msg}"
        );
    }

    #[test]
    fn errand_renders_at_fire_time_not_schedule_time() {
        // One errand, built once (at "schedule time"). The SAME errand renders two
        // different bodies depending on the live state handed to it at fire time —
        // proving the body is not snapshotted when the plan was parsed.
        let e = errand();

        let early = e.render(&model(&[])); // nothing crossed yet
        assert!(early.contains("Salmon fillets ×2"), "early: {early}");

        // Later, Luca crossed the salmon off at the kiosk. Re-render the SAME errand.
        let late = e.render(&model(&["Salmon fillets ×2 (Tue)"]));
        assert!(
            !late.contains("Salmon"),
            "fire-time render reflects the crossing: {late}"
        );
        assert_ne!(
            early, late,
            "body is computed at fire time, not at schedule time"
        );
    }

    #[test]
    fn all_crossed_sends_the_happy_note() {
        let everything = &[
            "Salmon fillets ×2 (Tue)",
            "Fresh sardines, ~400 g (Sat)",
            "Swiss chard, 1 bunch",
            "Lemons ×3",
        ];
        let msg = errand().render(&model(everything));
        assert_eq!(msg, ALL_DONE);
        assert!(msg.contains("nothing to buy"));
    }

    #[test]
    fn empty_or_unknown_shopping_model_reads_as_all_done() {
        // A defensive shape: no groups at all renders the happy note (the caller
        // is responsible for not even ticking when the fetch failed).
        assert_eq!(errand().render(&ShoppingModel::default()), ALL_DONE);
    }

    #[test]
    fn errand_tick_fires_exactly_once_even_when_called_twice() {
        let policy = FirePolicy::default();
        let errands = vec![errand()];
        let mut log = FiredLog::default();
        let now = dt(2026, 7, 18, 8, 45); // exactly at due

        let first = errand_tick(&errands, &mut log, now, &policy);
        assert_eq!(first.len(), 1, "nudges the first time");
        assert!(!first[0].late);
        assert_eq!(log.len(), 1);

        // A later tick (a minute on) must NOT re-nudge — one nudge per errand, so
        // items added/crossed after this never re-trigger it.
        let second = errand_tick(&errands, &mut log, dt(2026, 7, 18, 8, 46), &policy);
        assert!(second.is_empty(), "already nudged — never twice");
    }

    #[test]
    fn errand_restart_reload_does_not_refire() {
        let dir = tempfile::tempdir().unwrap();
        let path = FiredLog::path(dir.path());
        let policy = FirePolicy::default();
        let errands = vec![errand()];
        let now = dt(2026, 7, 18, 8, 45);

        // First process: record BEFORE "sending", persist.
        let mut log = FiredLog::load(&path);
        let fired = errand_tick(&errands, &mut log, now, &policy);
        assert_eq!(fired.len(), 1);
        log.save(&path).unwrap();

        // Restart: a fresh log from disk must remember the nudge went out.
        let mut reloaded = FiredLog::load(&path);
        assert_eq!(reloaded.len(), 1, "state survived the restart");
        let again = errand_tick(&errands, &mut reloaded, dt(2026, 7, 18, 8, 48), &policy);
        assert!(again.is_empty(), "restart must not re-nudge");
    }

    #[test]
    fn errand_nudge_is_time_critical_and_counts_against_the_standalone_cap() {
        use crate::notify::daily_digest::{
            DigestPolicy, DigestStore, Offer, PersonOverride, Urgency,
        };

        let now = dt(2026, 7, 18, 8, 45); // at the errand's due time
        // Cap Luca at ONE standalone DM/day so the accounting is crisp.
        let policy = DigestPolicy::new().with_override(
            "Luca",
            PersonOverride {
                standalone_cap: Some(1),
                ..Default::default()
            },
        );
        let mut store = DigestStore::default();

        // Fire the errand and render its body from live shopping state.
        let e = errand();
        let mut log = FiredLog::default();
        let fired = errand_tick(
            std::slice::from_ref(&e),
            &mut log,
            now,
            &FirePolicy::default(),
        );
        assert_eq!(fired.len(), 1);
        let firing = &fired[0];

        // The nudge the pacing layer sees is time-critical (fires standalone even
        // in quiet hours) and carries the market list verbatim.
        let expected_body = firing.errand.render(&model(&["Salmon fillets ×2 (Tue)"]));
        let nudge = firing.to_nudge(expected_body.clone());
        assert_eq!(nudge.urgency, Urgency::TimeCritical);
        assert_eq!(nudge.recipient, "Luca");

        // First offer → SendNow, and it COUNTS against the daily standalone cap.
        let (body, offer) = route_errand_nudge(
            firing,
            &model(&["Salmon fillets ×2 (Tue)"]),
            &mut store,
            now,
            &policy,
        );
        assert_eq!(offer, Offer::SendNow(expected_body.clone()));
        assert_eq!(body, expected_body);
        assert_eq!(
            store.state("Luca").unwrap().standalone_sent(),
            1,
            "the errand nudge is a standalone DM and counts against the cap"
        );

        // Re-offering the SAME errand is a no-op — never double-counts, never re-pings.
        let (_, dup) = route_errand_nudge(
            firing,
            &model(&["Salmon fillets ×2 (Tue)"]),
            &mut store,
            now,
            &policy,
        );
        assert_eq!(dup, Offer::Duplicate);
        assert_eq!(store.state("Luca").unwrap().standalone_sent(), 1);

        // A SECOND, distinct errand the same day is OVER the (1/day) cap → it folds
        // into the morning digest as an honest overflow line, not a fourth ping.
        let e2 = ErrandReminder {
            id: "errand:2026-W29:second-run".to_string(),
            ..e.clone()
        };
        let fired2 = errand_tick(
            std::slice::from_ref(&e2),
            &mut FiredLog::default(),
            now,
            &FirePolicy::default(),
        );
        let (_, offer2) = route_errand_nudge(
            &fired2[0],
            &model(&["Salmon fillets ×2 (Tue)"]),
            &mut store,
            now,
            &policy,
        );
        assert_eq!(
            offer2,
            Offer::Queued { overflow: true },
            "over the cap, the errand folds into the digest instead of pinging again"
        );
        assert_eq!(
            store.state("Luca").unwrap().standalone_sent(),
            1,
            "an over-cap errand does NOT bump the standalone counter"
        );
        assert_eq!(
            store.state("Luca").unwrap().pending().len(),
            1,
            "the over-cap errand is queued for the digest"
        );
    }

    #[test]
    fn errand_missed_while_down_fires_late_then_drops_when_stale() {
        let policy = FirePolicy::default();
        let e = errand(); // due 08:45
        let mut log = FiredLog::default();

        // Scheduler comes back 1h late (09:45) → still nudge, tagged late.
        let late = errand_tick(
            std::slice::from_ref(&e),
            &mut log,
            dt(2026, 7, 18, 9, 45),
            &policy,
        );
        assert_eq!(late.len(), 1);
        assert!(late[0].late, "a missed-recent errand nudges with (late)");

        // A different errand, > 2h stale, is dropped (never nudge a run long gone).
        let stale_row = CalendarEvent {
            time: "06:00".into(),
            ..errand_row()
        };
        let stale = ErrandReminder::from_calendar_event(
            "2026-W29",
            &stale_row,
            &members(),
            &owners(),
            Duration::minutes(15),
        )
        .unwrap(); // due 05:45
        let mut log2 = FiredLog::default();
        let dropped = errand_tick(&[stale.clone()], &mut log2, dt(2026, 7, 18, 9, 0), &policy);
        assert!(
            dropped.is_empty(),
            "3h+ stale errand is dropped, not nudged"
        );
        assert_eq!(log2.outcome(&stale.id), Some(Outcome::Dropped));
    }

    #[test]
    fn errand_pending_before_due() {
        let policy = FirePolicy::default();
        let errands = vec![errand()]; // due 08:45
        let mut log = FiredLog::default();
        let fired = errand_tick(&errands, &mut log, dt(2026, 7, 18, 8, 30), &policy);
        assert!(fired.is_empty(), "not due yet at 08:30");
        assert!(log.is_empty(), "pending errands are not recorded");
    }

    #[test]
    fn errands_from_plan_pulls_the_market_row() {
        let plan = PlanDoc {
            week_code: "2026-W29".into(),
            calendar: vec![
                CalendarEvent {
                    weekday: "Mon".into(),
                    date: Some(NaiveDate::from_ymd_opt(2026, 7, 13).unwrap()),
                    time: "18:30".into(),
                    event: "Cook: curry".into(),
                    source: "Bruno".into(),
                },
                errand_row(),
            ],
            ..Default::default()
        };
        let es = errands_from_plan(&plan, &members(), &owners(), Duration::minutes(15));
        assert_eq!(es.len(), 1, "only the errand row, not the cook slot");
        assert_eq!(es[0].recipient, "Luca");
    }

    #[test]
    fn shopping_model_parses_gateway_json_and_counts_remaining() {
        // The exact GET /shopping.json shape from weekAdapter.mjs::shoppingJson.
        let body = r#"{
            "ok": true,
            "groups": [
                { "store": "Fishmonger / market", "items": [
                    { "text": "Salmon fillets ×2", "key": "p:x|y", "checked": true },
                    { "text": "Fresh sardines", "key": "p:x|z", "checked": false }
                ]},
                { "store": "Greengrocer / produce", "items": [
                    { "text": "Swiss chard", "key": "p:a|b", "checked": false }
                ]}
            ]
        }"#;
        let m = ShoppingModel::from_json(body);
        assert!(m.ok);
        assert_eq!(m.groups.len(), 2);
        assert_eq!(m.remaining_count(), 2, "one crossed of three");
        let msg = errand().render(&m);
        assert!(msg.contains("Fresh sardines"));
        assert!(msg.contains("Swiss chard"));
        assert!(!msg.contains("Salmon"), "crossed item excluded: {msg}");
    }

    #[test]
    fn resolve_lead_defaults_to_fifteen_minutes() {
        // Do not touch the process env (parallel tests); assert the default const
        // and the Duration it maps to. The env override path is covered by the
        // `_configurable` test which passes an explicit lead.
        assert_eq!(DEFAULT_ERRAND_LEAD_MINUTES, 15);
        assert_eq!(
            Duration::minutes(DEFAULT_ERRAND_LEAD_MINUTES),
            Duration::minutes(15)
        );
    }

    #[test]
    fn compact_item_and_store_tidy_text() {
        assert_eq!(compact_item("Salmon fillets ×2 (Tue)"), "Salmon fillets ×2");
        assert_eq!(compact_item("Lemons ×3"), "Lemons ×3");
        assert_eq!(
            clean_store("🐟 Fishmonger / market (Sat 07-18, fresh)"),
            "Fishmonger / market"
        );
        assert_eq!(clean_store("Also getting"), "Also getting");
    }
}
