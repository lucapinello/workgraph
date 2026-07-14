//! Single-owner routing for conversationally-created tasks.
//!
//! Luca's 2026-07-13 constellation test exposed the bug this module fixes: a
//! single group ask ("swap Thursday dinner to grilled tofu") elected the WHOLE
//! roster, and because [`run_group_collective`] runs one compose turn per voice
//! and every turn's [`finalize_composed_reply`] can create a task, the one ask
//! minted FOUR duplicate tasks in 64 seconds — one per persona, including
//! **Coach Mira** (workouts) taking on a **cooking** task. Three were abandoned;
//! only Nora's — the dietitian, whose domain it was — survived.
//!
//! Three layers close the hole, all pure and file-backed so they are testable
//! without a model call or a live Telegram:
//!
//! 1. **Single-owner rule** ([`decide_owner`]). When a conversational turn
//!    (collective OR single-voice) is about to create a task, creation routes to
//!    exactly ONE persona — the DOMAIN OWNER derived from `household.toml`
//!    domains (meals → Nora / Bruno per their split; workouts → Mira;
//!    calendar / shopping / coordination → Otto). Every other voice in a
//!    collective turn answers conversationally and DEFERS ("Nora's got this one
//!    🥗") — it never creates its own copy.
//!
//! 2. **Intent dedupe** ([`IntentLedger`]) as the safety net. Task creation from
//!    conversation carries an intent fingerprint (normalized ask text + origin
//!    chat, within a ~5-minute window). A second creation matching the
//!    fingerprint is refused — logged as `duplicate intent, task X already
//!    exists` — regardless of which persona tries. This backstops the owner rule
//!    for races and for households where the owner cannot be resolved.
//!
//! 3. **Off-domain guard** ([`decide_owner`] again). A persona resolving to a
//!    task outside its own domains is a [`OwnerDecision::Defer`]: the caller logs
//!    a loud warning and re-routes to the owner. Mira can never *own* a cooking
//!    task; the constellation card "Coach Mira took on add grilled tofu" is made
//!    impossible.
//!
//! [`run_group_collective`]: crate::commands::telegram::run_group_collective
//! [`finalize_composed_reply`]: crate::notify::telegram_conversation
//! [`decide_owner`]: OwnerMap::decide_owner

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

/// The household domain a conversational ask belongs to. Coarser than the raw
/// `household.toml` tags (which split meals across Nora & Bruno); each variant
/// carries an ORDERED preference list of household tags so [`OwnerMap`] can pick
/// the single most-specific owner (e.g. a meal *plan* change prefers the
/// nutrition owner, a *recipe* ask prefers the cooking owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// Deciding *what* the family eats — a dinner swap, a menu change, "what's
    /// for lunch". The planner/dietitian's call.
    MealPlanning,
    /// *How* a dish is made — a recipe, a cooking technique, "how do I make…".
    /// The kitchen's call.
    Cooking,
    /// Exercise, training, the gym.
    Workouts,
    /// The calendar — appointments, bookings, reminders, scheduling.
    Calendar,
    /// The shopping list, groceries, errands, "add X to the list".
    Shopping,
    /// Anything else team-directed — the concierge/coordinator catch-all.
    Coordination,
}

impl Domain {
    /// Stable slug for logs and the `--dry-run` seam.
    pub fn slug(self) -> &'static str {
        match self {
            Domain::MealPlanning => "meal-planning",
            Domain::Cooking => "cooking",
            Domain::Workouts => "workouts",
            Domain::Calendar => "calendar",
            Domain::Shopping => "shopping",
            Domain::Coordination => "coordination",
        }
    }

    /// The ordered `household.toml` domain tags this domain maps to, most
    /// specific first. [`OwnerMap`] returns the first persona that lists any of
    /// them, so a household that splits meals into `nutrition` (Nora) vs
    /// `cooking`/`recipes` (Bruno) routes a plan change to Nora and a recipe ask
    /// to Bruno, while both remain valid meal owners.
    pub fn household_tags(self) -> &'static [&'static str] {
        match self {
            Domain::MealPlanning => &["nutrition", "meals"],
            Domain::Cooking => &["cooking", "recipes", "meals"],
            Domain::Workouts => &["workouts", "exercise", "fitness"],
            Domain::Calendar => &["calendar", "coordination"],
            Domain::Shopping => &["shopping", "groceries", "coordination"],
            Domain::Coordination => &["coordination"],
        }
    }
}

// ---------------------------------------------------------------------------
// Ask classification
// ---------------------------------------------------------------------------

/// Whole-word membership test after light normalization — matches `needle` as a
/// standalone token in `text` (so "plan" does not fire on "airplane").
fn has_word(words: &[String], needle: &str) -> bool {
    words.iter().any(|w| w == needle)
}

/// Any of `needles` present as a standalone token.
fn has_any(words: &[String], needles: &[&str]) -> bool {
    needles.iter().any(|n| has_word(words, n))
}

/// Split an ask into lower-case alphanumeric tokens (apostrophes dropped, so
/// "what's" → "whats"). The shared tokenizer for classification and
/// fingerprinting, so the two never drift.
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

const WORKOUT_WORDS: &[&str] = &[
    "workout", "workouts", "exercise", "exercises", "gym", "training", "train",
    "run", "running", "jog", "jogging", "yoga", "stretch", "cardio", "lift",
    "lifting", "weights", "fitness", "pilates", "reps", "hike",
];

const CALENDAR_WORDS: &[&str] = &[
    "calendar", "appointment", "appointments", "schedule", "scheduling",
    "reschedule", "book", "booking", "reminder", "reminders", "remind",
    "meeting", "event", "rsvp", "reservation",
];

const SHOPPING_WORDS: &[&str] = &[
    "shopping", "grocery", "groceries", "buy", "errand", "errands", "store",
    "supermarket", "pickup", "pantry", "restock",
];

/// Nouns that mark an ask as being about *food / a meal*. Presence of one makes
/// the ask a meals domain (split into planning vs cooking below).
const MEAL_WORDS: &[&str] = &[
    "dinner", "dinners", "lunch", "lunches", "breakfast", "breakfasts", "meal",
    "meals", "menu", "menus", "dish", "dishes", "supper", "snack", "snacks",
    "food", "recipe", "recipes", "eat", "eating",
];

/// Signals that a *meal* ask is about the KITCHEN — a recipe, a technique, "how
/// do I make it". These route the meal to the cooking owner (Bruno) rather than
/// the planner (Nora). Deliberately narrow: a mere cooking-method adjective in a
/// dish name ("grilled tofu") is NOT one of these, so a plan change like "swap
/// dinner to grilled tofu" stays with the planner.
const COOKING_WORDS: &[&str] = &[
    "recipe", "recipes", "cook", "cooking", "bake", "baking", "roast",
    "roasting", "marinate", "marinade", "technique", "saute", "simmer", "knead",
    "prep", "chop",
];

/// Phrases that clinch a recipe / how-to-cook ask even if the individual words
/// are ambiguous.
const COOKING_PHRASES: &[&str] = &[
    "how do i cook", "how to cook", "how do i make", "how to make",
    "recipe for", "how do you cook", "how do you make",
];

/// Specific, prepared DISH names (not raw ingredients). A bare dish mention with
/// no meal noun ("dinner") and no planning verb — "pizza on Friday", "carbonara
/// tonight" — is a KITCHEN signal: it should reach the chef's voice (Bruno), not
/// the concierge. Deliberately excludes generic groceries ("rice", "milk") that
/// belong on the shopping list, and is checked LAST (after shopping) so
/// "add pizza to the shopping list" still routes to shopping, not the kitchen.
const DISH_WORDS: &[&str] = &[
    "pizza", "pasta", "carbonara", "lasagna", "lasagne", "risotto", "ravioli",
    "gnocchi", "sushi", "ramen", "taco", "tacos", "burrito", "burritos",
    "burger", "burgers", "curry", "paella", "pesto", "omelette", "omelet",
    "pancakes", "waffles", "quesadilla", "enchiladas", "stirfry",
];

/// Classify a conversational ask into its household [`Domain`]. Pure keyword
/// heuristics (never a model call) so the routing decision is deterministic and
/// unit-testable. The order encodes precedence: an explicit workout/calendar
/// signal wins, then meals (planning-vs-cooking split), then shopping, then a
/// standalone cooking verb, else the coordination catch-all.
pub fn classify_domain(ask: &str) -> Domain {
    let lower = ask.to_lowercase();
    let words = tokens(ask);

    if has_any(&words, WORKOUT_WORDS) {
        return Domain::Workouts;
    }
    if has_any(&words, CALENDAR_WORDS) {
        return Domain::Calendar;
    }

    let cooking_flavored = has_any(&words, COOKING_WORDS)
        || COOKING_PHRASES.iter().any(|p| lower.contains(p));

    if has_any(&words, MEAL_WORDS) {
        // A meal ask: the kitchen owns a recipe/technique ask; the planner owns
        // a "what are we eating / swap dinner" decision.
        return if cooking_flavored {
            Domain::Cooking
        } else {
            Domain::MealPlanning
        };
    }
    if has_any(&words, SHOPPING_WORDS) {
        return Domain::Shopping;
    }
    // A standalone cooking ask with no meal noun ("can you bake something?").
    if cooking_flavored {
        return Domain::Cooking;
    }
    // A bare named dish ("pizza on Friday") — the kitchen's, so plain food
    // chatter reaches the chef's voice instead of falling through to the
    // concierge. Checked after shopping so "add pizza to the list" stays shopping.
    if has_any(&words, DISH_WORDS) {
        return Domain::Cooking;
    }
    Domain::Coordination
}

// ---------------------------------------------------------------------------
// Owner map
// ---------------------------------------------------------------------------

/// Maps household domains to the persona that OWNS them, derived from
/// `household.toml` `[[agent]]` `domains`. Preserves author order so ties (two
/// personas both listing `meals`) resolve deterministically to the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerMap {
    /// `(persona_id, domain_tags)` in `household.toml` author order.
    entries: Vec<(String, Vec<String>)>,
}

/// The single-owner decision for one (persona, ask) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerDecision {
    /// This persona owns the ask's domain (or no owner could be resolved / the
    /// persona is unknown) — it may create the task.
    Owner,
    /// This persona is OFF-domain. It must NOT create the task; the owner does.
    /// Carries the owner's persona id so the caller can log the re-route and the
    /// deferring voice can name who has it.
    Defer { owner: String },
}

impl OwnerMap {
    /// The shipped Casa Pinello roster, mirroring `household.toml`:
    /// Nora = meals & nutrition, Bruno = the kitchen (cooking/recipes), Coach
    /// Mira = workouts, Otto = calendar / coordination / shopping. Used as the
    /// fallback when no `household.toml` is found.
    pub fn casa_default() -> Self {
        let entry = |id: &str, tags: &[&str]| {
            (
                id.to_string(),
                tags.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            )
        };
        Self {
            entries: vec![
                entry("nora", &["meals", "nutrition"]),
                entry("bruno", &["meals", "cooking", "recipes"]),
                entry("mira", &["workouts"]),
                entry("otto", &["calendar", "coordination", "shopping"]),
            ],
        }
    }

    /// Build directly from `(persona_id, domain_tags)` pairs (author order
    /// preserved). Ids and tags are trimmed/lower-cased; empty ids are dropped.
    pub fn from_pairs<I, S, T>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, Vec<T>)>,
        S: Into<String>,
        T: Into<String>,
    {
        let entries = pairs
            .into_iter()
            .filter_map(|(id, tags)| {
                let id = id.into().trim().to_lowercase();
                if id.is_empty() {
                    return None;
                }
                let tags = tags
                    .into_iter()
                    .map(|t| t.into().trim().to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect();
                Some((id, tags))
            })
            .collect();
        Self { entries }
    }

    /// Parse `<root>/household.toml`'s `[[agent]]` blocks (id + domains). Returns
    /// `None` when the file is absent or has no usable agents, so the caller can
    /// fall back to [`casa_default`](Self::casa_default).
    pub fn from_household_toml(root: &Path) -> Option<Self> {
        let body = std::fs::read_to_string(root.join("household.toml")).ok()?;
        let value: toml::Value = body.parse().ok()?;
        let agents = value.get("agent")?.as_array()?;
        let pairs: Vec<(String, Vec<String>)> = agents
            .iter()
            .filter_map(|a| {
                let id = a.get("id")?.as_str()?.to_string();
                let domains = a
                    .get("domains")
                    .and_then(|d| d.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|d| d.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Some((id, domains))
            })
            .collect();
        if pairs.is_empty() {
            return None;
        }
        Some(Self::from_pairs(pairs))
    }

    /// Load the owner map for a project root: `household.toml` if present, else
    /// the Casa default. Never fails — routing always has a map.
    pub fn load(root: &Path) -> Self {
        Self::from_household_toml(root).unwrap_or_else(Self::casa_default)
    }

    /// The persona id that owns `domain`, by trying the domain's ordered
    /// household tags and returning the first persona (author order) that lists
    /// one. `None` only if no persona lists any of the tags.
    pub fn owner_for_domain(&self, domain: Domain) -> Option<&str> {
        for tag in domain.household_tags() {
            if let Some((id, _)) = self.entries.iter().find(|(_, tags)| {
                tags.iter().any(|t| t == tag)
            }) {
                return Some(id.as_str());
            }
        }
        None
    }

    /// The persona id that owns the ask (classify + resolve).
    pub fn owner_for_ask(&self, ask: &str) -> Option<&str> {
        self.owner_for_domain(classify_domain(ask))
    }

    /// The single-owner decision for `persona` creating a task from `ask`.
    ///
    /// * The resolved owner (case-insensitive id match) → [`OwnerDecision::Owner`].
    /// * A different persona → [`OwnerDecision::Defer`] to the owner (off-domain
    ///   guard: it must not create, and the caller re-routes / the voice defers).
    /// * No resolvable owner, or an empty/unknown persona → [`OwnerDecision::Owner`]
    ///   (fail-open so a real ask is never dropped; the intent ledger still
    ///   dedupes any duplicate that slips through).
    pub fn decide_owner(&self, persona: &str, ask: &str) -> OwnerDecision {
        let persona = persona.trim().to_lowercase();
        let Some(owner) = self.owner_for_ask(ask) else {
            return OwnerDecision::Owner;
        };
        if persona.is_empty() || persona == owner {
            OwnerDecision::Owner
        } else {
            OwnerDecision::Defer {
                owner: owner.to_string(),
            }
        }
    }
}

/// A family-voice one-liner a deferring voice can add so the ask visibly lands
/// with its owner instead of vanishing ("Nora's got this one 🥗"). `owner` is a
/// persona id; the display name is a simple capitalization.
pub fn defer_line(owner: &str, domain: Domain) -> String {
    let name = display_name(owner);
    let emoji = match domain {
        Domain::MealPlanning => " 🥗",
        Domain::Cooking => " 🍳",
        Domain::Workouts => " 💪",
        Domain::Calendar => " 📅",
        Domain::Shopping => " 🛒",
        Domain::Coordination => "",
    };
    format!("{name}'s got this one{emoji}")
}

/// Capitalize a persona id into a display name ("nora" → "Nora"). Good enough for
/// a defer line; the roster's real display name is used where one is available.
fn display_name(id: &str) -> String {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Intent dedupe ledger
// ---------------------------------------------------------------------------

/// Default dedupe window: a second creation of the SAME ask in the SAME chat
/// within this many seconds is a duplicate. ~5 minutes covers the collective
/// burst (Luca's four tasks landed inside 64s) plus a slow retry, without
/// suppressing a genuine repeat of the same ask hours later.
pub const DEFAULT_WINDOW_SECS: i64 = 300;

/// Normalize an ask into a stable fingerprint keyed to its chat: lower-cased
/// alphanumeric tokens joined by single spaces, prefixed with the chat id. Two
/// personas answering the identical group ask produce the identical fingerprint,
/// so the ledger collapses them to one task.
pub fn fingerprint(ask: &str, chat_id: &str) -> String {
    let normalized = tokens(ask).join(" ");
    format!("{}\u{1f}{}", chat_id.trim(), normalized)
}

/// One recorded task-creation intent. Append-only JSONL under
/// `<root>/.casa/intents.jsonl` — the same `.casa` surface the preference store
/// and conversation feed use — so the dedupe survives a process restart (the
/// four duplicate turns can be separate listener invocations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentRecord {
    /// Unix epoch seconds the intent was recorded.
    pub ts: i64,
    /// The chat+ask fingerprint ([`fingerprint`]).
    pub fingerprint: String,
    /// The task id that was created for this intent.
    pub task_id: String,
    /// The persona that created it (for the audit trail).
    pub persona: String,
}

impl IntentRecord {
    fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "fingerprint": self.fingerprint,
            "task_id": self.task_id,
            "persona": self.persona,
        })
        .to_string()
    }

    fn from_json_line(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        Some(Self {
            ts: v.get("ts")?.as_i64()?,
            fingerprint: v.get("fingerprint")?.as_str()?.to_string(),
            task_id: v.get("task_id")?.as_str()?.to_string(),
            persona: v
                .get("persona")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }
}

/// The durable intent-dedupe ledger. Dependency-free file I/O so it is trivially
/// testable and cannot itself lose or fabricate a dedupe to a model call.
pub struct IntentLedger;

impl IntentLedger {
    /// Path to the append-only JSONL under `<root>/.casa/`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("intents.jsonl")
    }

    /// The task id of a still-fresh intent matching `fp` (recorded within
    /// `window_secs` of `now_epoch`), or `None` if this ask is new to the chat.
    /// A duplicate means the caller must REFUSE creation and reuse the returned
    /// task id.
    pub fn find_recent(
        root: &Path,
        fp: &str,
        now_epoch: i64,
        window_secs: i64,
    ) -> Option<String> {
        let path = Self::path(root);
        let body = std::fs::read_to_string(&path).ok()?;
        body.lines()
            .filter_map(IntentRecord::from_json_line)
            .filter(|r| r.fingerprint == fp && (now_epoch - r.ts).abs() <= window_secs)
            .map(|r| r.task_id)
            .last()
    }

    /// Durably record a fresh intent, creating `.casa/` if needed. Append-only:
    /// a new intent never clobbers an earlier one.
    pub fn record(
        root: &Path,
        fp: &str,
        task_id: &str,
        persona: &str,
        now_epoch: i64,
    ) -> std::io::Result<()> {
        let rec = IntentRecord {
            ts: now_epoch,
            fingerprint: fp.to_string(),
            task_id: task_id.to_string(),
            persona: persona.trim().to_string(),
        };
        let path = Self::path(root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{}", rec.to_json_line())?;
        Ok(())
    }

    /// The current dedupe window in seconds — [`DEFAULT_WINDOW_SECS`] unless
    /// `CASA_INTENT_WINDOW_SECS` overrides it (0 disables the window).
    pub fn window_secs() -> i64 {
        std::env::var("CASA_INTENT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_WINDOW_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classification --------------------------------------------------

    #[test]
    fn tofu_swap_is_meal_planning_not_cooking() {
        // The exact regression ask. "grilled" is a dish adjective, not a recipe
        // request — this is a plan change, so it belongs to the planner (Nora).
        assert_eq!(
            classify_domain("swap Thursday dinner to grilled tofu"),
            Domain::MealPlanning
        );
    }

    #[test]
    fn recipe_ask_is_cooking() {
        assert_eq!(
            classify_domain("what's a good recipe for the tofu?"),
            Domain::Cooking
        );
        assert_eq!(
            classify_domain("how do I cook the lentils"),
            Domain::Cooking
        );
    }

    #[test]
    fn bare_dish_name_is_cooking_but_shopping_list_stays_shopping() {
        // A bare dish mention reaches the kitchen (Bruno) — "otto replied for food
        // instead of the chef" (Luca, 2026-07-14).
        assert_eq!(classify_domain("pizza on friday"), Domain::Cooking);
        assert_eq!(classify_domain("carbonara tonight"), Domain::Cooking);
        // But a dish on the SHOPPING list is a shopping ask, not a cooking one.
        assert_eq!(
            classify_domain("add pizza to the shopping list"),
            Domain::Shopping
        );
        // A raw grocery is NOT a dish — it stays coordination/shopping, never the
        // kitchen (guards the "add rice to the list" owner test).
        assert_eq!(classify_domain("add rice to the list"), Domain::Coordination);
    }

    #[test]
    fn workout_calendar_shopping_classify() {
        assert_eq!(classify_domain("can we move my gym session?"), Domain::Workouts);
        assert_eq!(
            classify_domain("book a dentist appointment next week"),
            Domain::Calendar
        );
        assert_eq!(
            classify_domain("add oat milk to the shopping list"),
            Domain::Shopping
        );
        assert_eq!(classify_domain("who is picking up the kids?"), Domain::Coordination);
    }

    // ---- owner resolution ------------------------------------------------

    #[test]
    fn casa_owners_resolve_per_domain() {
        let m = OwnerMap::casa_default();
        assert_eq!(m.owner_for_ask("swap Thursday dinner to grilled tofu"), Some("nora"));
        assert_eq!(m.owner_for_ask("how do I cook the tofu"), Some("bruno"));
        assert_eq!(m.owner_for_ask("reschedule my workout"), Some("mira"));
        assert_eq!(m.owner_for_ask("book a table for Friday"), Some("otto"));
        assert_eq!(m.owner_for_ask("add rice to the list"), Some("otto"));
        assert_eq!(m.owner_for_ask("who feeds the cat tonight"), Some("otto"));
    }

    #[test]
    fn tofu_scenario_single_owner_is_nora_others_defer() {
        // The regression fixture: the one ask, four voices, exactly one owner.
        let m = OwnerMap::casa_default();
        let ask = "swap Thursday dinner to grilled tofu";
        assert_eq!(m.decide_owner("nora", ask), OwnerDecision::Owner);
        // Mira (workouts) must NEVER own a cooking/meals task.
        assert_eq!(
            m.decide_owner("mira", ask),
            OwnerDecision::Defer { owner: "nora".into() }
        );
        assert_eq!(
            m.decide_owner("bruno", ask),
            OwnerDecision::Defer { owner: "nora".into() }
        );
        assert_eq!(
            m.decide_owner("otto", ask),
            OwnerDecision::Defer { owner: "nora".into() }
        );
    }

    #[test]
    fn cooking_owner_bruno_never_defers_on_his_own_ask() {
        // DEFER DISCIPLINE (morning-taco-bugs): the taco ask is the kitchen's, so
        // Bruno OWNS it and must never be told to defer to himself — regardless of
        // casing / trailing space in how the composer stamped the persona.
        let m = OwnerMap::casa_default();
        let ask = "hey I changed my mind on friday I want tacos";
        assert_eq!(m.owner_for_ask(ask), Some("bruno"));
        for persona in ["bruno", "Bruno", "  BRUNO  "] {
            assert_eq!(
                m.decide_owner(persona, ask),
                OwnerDecision::Owner,
                "owner {persona:?} must not defer to itself"
            );
        }
        // A genuinely off-domain voice (Coach Mira) still defers to Bruno.
        assert_eq!(
            m.decide_owner("mira", ask),
            OwnerDecision::Defer { owner: "bruno".into() }
        );
    }

    #[test]
    fn off_domain_persona_defers_to_owner() {
        // Otto trying to own a workout re-routes to Mira.
        let m = OwnerMap::casa_default();
        assert_eq!(
            m.decide_owner("otto", "can you plan my run for tomorrow"),
            OwnerDecision::Defer { owner: "mira".into() }
        );
    }

    #[test]
    fn decide_owner_is_case_insensitive_and_fails_open() {
        let m = OwnerMap::casa_default();
        assert_eq!(m.decide_owner("NORA", "swap dinner to tofu"), OwnerDecision::Owner);
        // Empty persona → fail open (do not drop a real ask).
        assert_eq!(m.decide_owner("", "swap dinner to tofu"), OwnerDecision::Owner);
    }

    #[test]
    fn from_pairs_preserves_author_order_for_meal_tie() {
        // Two personas both list "meals"; MealPlanning prefers whoever lists
        // "nutrition", else author order.
        let m = OwnerMap::from_pairs(vec![
            ("bruno", vec!["meals", "cooking"]),
            ("nora", vec!["meals", "nutrition"]),
        ]);
        // nutrition tag wins for a plan change regardless of author order.
        assert_eq!(m.owner_for_domain(Domain::MealPlanning), Some("nora"));
        // A raw "meals"-only tie falls to author order (bruno first here).
        let m2 = OwnerMap::from_pairs(vec![
            ("bruno", vec!["meals"]),
            ("nora", vec!["meals"]),
        ]);
        assert_eq!(m2.owner_for_domain(Domain::MealPlanning), Some("bruno"));
    }

    // ---- intent ledger ---------------------------------------------------

    #[test]
    fn fingerprint_is_stable_across_whitespace_and_case() {
        assert_eq!(
            fingerprint("Swap  Thursday   DINNER to grilled tofu", "chat-1"),
            fingerprint("swap thursday dinner to grilled tofu", "chat-1")
        );
        // Different chat → different fingerprint.
        assert_ne!(
            fingerprint("swap dinner to tofu", "chat-1"),
            fingerprint("swap dinner to tofu", "chat-2")
        );
    }

    #[test]
    fn ledger_dedupes_within_window_across_personas() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let fp = fingerprint("swap Thursday dinner to grilled tofu", "group-42");
        // Nora creates first.
        assert_eq!(IntentLedger::find_recent(root, &fp, 1000, 300), None);
        IntentLedger::record(root, &fp, "swap-thursday-dinner", "nora", 1000).unwrap();
        // Bruno, 20s later, same ask → duplicate, reuses Nora's task.
        assert_eq!(
            IntentLedger::find_recent(root, &fp, 1020, 300),
            Some("swap-thursday-dinner".to_string())
        );
        // Well outside the window → no longer a duplicate.
        assert_eq!(IntentLedger::find_recent(root, &fp, 1000 + 400, 300), None);
    }

    #[test]
    fn ledger_missing_file_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let fp = fingerprint("anything", "c1");
        assert_eq!(IntentLedger::find_recent(dir.path(), &fp, 0, 300), None);
    }
}
