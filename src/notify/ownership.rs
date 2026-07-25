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

    /// Parse a [`slug`](Self::slug) back into a domain — the ledger stores the
    /// slug, and an unrecognized/legacy value is `None` (never a wrong guess).
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug.trim() {
            "meal-planning" => Some(Domain::MealPlanning),
            "cooking" => Some(Domain::Cooking),
            "workouts" => Some(Domain::Workouts),
            "calendar" => Some(Domain::Calendar),
            "shopping" => Some(Domain::Shopping),
            "coordination" => Some(Domain::Coordination),
            _ => None,
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

/// True when strings `a` and `b` are within Damerau-free edit distance 1 (a
/// single insertion, deletion, or substitution). A dependency-free mirror of the
/// [`telegram_group`] fuzzy matcher, kept local so `ownership` stays pure and
/// unit-testable without a model call or the group module.
///
/// [`telegram_group`]: crate::notify::telegram_group
fn edit_distance_le_1(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (longer, shorter) = if a.len() >= b.len() { (&a, &b) } else { (&b, &a) };
    let ldiff = longer.len() - shorter.len();
    if ldiff > 1 {
        return false;
    }
    if ldiff == 0 {
        return longer
            .iter()
            .zip(shorter.iter())
            .filter(|(x, y)| x != y)
            .count()
            <= 1;
    }
    // Length differs by one — `shorter` must embed in `longer` with a single
    // insertion. Walk both, permitting exactly one skip in `longer`.
    let (mut i, mut j) = (0usize, 0usize);
    let mut skipped = false;
    while i < longer.len() && j < shorter.len() {
        if longer[i] == shorter[j] {
            i += 1;
            j += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
            i += 1;
        }
    }
    true
}

/// A message `word` matches a `needle` when identical, or — for needles of 4+
/// chars — within edit distance 1. Short needles ("egg", "sub") require an exact
/// match so a one-letter slip in a common short word can't misfire. This is the
/// SAME tolerance the roster-name matcher uses ("guyd"→"guys"), applied here so a
/// misspelled dish/ingredient ("tacod"→"taco", "fennle"→"fennel") still classifies
/// as food.
fn fuzzy_word_matches(word: &str, needle: &str) -> bool {
    word == needle || (needle.chars().count() >= 4 && edit_distance_le_1(word, needle))
}

/// Any of `needles` present as a token, matched typo-tolerantly
/// ([`fuzzy_word_matches`]).
fn has_any_fuzzy(words: &[String], needles: &[&str]) -> bool {
    words
        .iter()
        .any(|w| needles.iter().any(|n| fuzzy_word_matches(w, n)))
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

/// Raw edible ingredients (not prepared dishes, not the generic meal nouns). A
/// bare ingredient is NOT itself a domain signal — "we're out of fennel" is not a
/// meal-plan ask — but an ingredient on EITHER side of a swap ("swap tacod for
/// grilled **fennel**") clinches the swap as a food-plan change. Kept separate
/// from [`DISH_WORDS`] so the swap-shape rule can treat both as edible while the
/// bare-dish rule stays limited to prepared dishes.
const INGREDIENT_WORDS: &[&str] = &[
    "fennel", "tofu", "tempeh", "seitan", "trout", "salmon", "tuna", "cod",
    "chicken", "beef", "pork", "lamb", "turkey", "sausage", "bacon", "shrimp",
    "prawns", "artichoke", "artichokes", "broccoli", "spinach", "kale",
    "mushroom", "mushrooms", "eggplant", "aubergine", "zucchini", "courgette",
    "cauliflower", "asparagus", "lentils", "chickpeas", "beans", "quinoa",
    "couscous", "polenta", "risotto", "gnocchi", "halloumi", "feta", "avocado",
    "aubergines", "peppers", "squash",
    // Fish & other proteins the family plans meals around. "branzino" is the
    // exact miss from Luca's 2026-07-17 transcript ("plan for branzino for
    // tomorrow night"): unknown to the classifier, it dropped to Coordination →
    // Otto and the whole roster answered. Kept typo-tolerant via `is_edible`.
    "branzino", "seabass", "bass", "halibut", "haddock", "tilapia", "sardines",
    "anchovies", "mackerel", "swordfish", "snapper", "sole", "flounder",
    "mussels", "clams", "scallops", "squid", "calamari", "octopus", "lobster",
    "crab", "duck", "venison", "veal",
];

/// Verbs that mark a "replace A with B" SWAP shape. When one of these appears
/// alongside an edible token (dish, meal noun, or ingredient — [`is_edible`]),
/// the ask is a PLAN change even when the food words are misspelled or otherwise
/// unrecognised, so "swap tacod for grilled fennel" reaches the planner instead
/// of dropping to the concierge. Matched typo-tolerantly for 4+-char verbs.
///
/// Deliberately EXCLUDES generic verbs like "make"/"do": "how do I cook the
/// lentils" is a recipe ask (the kitchen's), not a plan swap, and must not be
/// hijacked by an incidental "do" next to an edible word — the recipe rule
/// (`cooking_flavored`) is checked first for exactly that reason.
const SWAP_VERBS: &[&str] = &["swap", "replace", "change", "switch", "substitute", "sub"];

/// Verbs that mark a "what should we eat / put this on the week" MEAL-PLANNING
/// ask. Paired with an edible token ([`is_edible`]) this is a plan change even
/// when NO meal noun ("dinner") appears — "plan branzino for tomorrow night",
/// "let's do salmon saturday". Without the plan verb a bare ingredient stays
/// neutral (a lone "branzino" or "we're out of fennel" is not a plan ask), so
/// this rule only fires on an explicit planning intent. Matched typo-tolerantly
/// for 4+-char verbs.
const PLAN_VERBS: &[&str] = &["plan", "planning", "plans", "planned"];

/// The connector tokens that turn a swap verb into an explicit "A → B"
/// replacement: "swap tacod **for** fennel", "replace chicken **with** trout",
/// "switch dinner **to** pasta". REQUIRING a connector keeps a bare craving
/// ("I *changed* my mind, I want tacos" — no connector) out of the swap rule, so
/// it still reaches the chef as a dish, exactly as before.
const SWAP_CONNECTORS: &[&str] = &["for", "to", "with", "into"];

/// True if `word` is an edible token — a prepared dish, a meal noun, or a raw
/// ingredient — matched typo-tolerantly so "tacod"→"taco" and "fennle"→"fennel"
/// still count. The edibility test that powers the swap-shape rule.
fn is_edible(word: &str) -> bool {
    DISH_WORDS.iter().any(|d| fuzzy_word_matches(word, d))
        || MEAL_WORDS.iter().any(|m| fuzzy_word_matches(word, m))
        || INGREDIENT_WORDS.iter().any(|i| fuzzy_word_matches(word, i))
}

/// True when the ask is a "replace A with B" SWAP that names something edible on
/// at least one side — the [`SWAP_VERBS`] + [`is_edible`] shape. This is the fix
/// for Luca's fennel transcript: "hey can you swap **tacod** for grilled
/// **fennel**" has no recognised meal noun (the dish is misspelled and the target
/// is a raw vegetable), so the old classifier dropped it to Coordination → Otto;
/// the swap shape now pins it to a food plan change.
fn is_food_swap(words: &[String]) -> bool {
    let has_swap_verb = has_any_fuzzy(words, SWAP_VERBS);
    let has_connector = has_any(words, SWAP_CONNECTORS);
    has_swap_verb && has_connector && words.iter().any(|w| is_edible(w))
}

/// True when the ask is an explicit MEAL-PLANNING request that names something
/// edible — a [`PLAN_VERBS`] verb + an edible token ([`is_edible`]). This is the
/// fix for Luca's branzino transcript: "hey plan for branzino for tomorrow
/// night" has no meal noun and no swap connector, so the older classifier
/// dropped it to Coordination → Otto and the whole roster answered. "Plan" +
/// an edible pins it to a food-plan change (Nora). The plan verb is REQUIRED so
/// a bare ingredient ("we're out of fennel") stays neutral. Checked after the
/// recipe rule so "plan how to cook the lentils" stays a kitchen ask.
fn is_meal_plan_ask(words: &[String]) -> bool {
    has_any_fuzzy(words, PLAN_VERBS) && words.iter().any(|w| is_edible(w))
}

/// Classify a conversational ask into its household [`Domain`]. Pure keyword
/// heuristics (never a model call) so the routing decision is deterministic and
/// unit-testable. The order encodes precedence: an explicit workout/calendar
/// signal wins, then meals (planning-vs-cooking split), then shopping, then a
/// food SWAP shape (typo-tolerant, so a misspelled dish still routes to the
/// planner), then a standalone cooking verb, a bare dish, else the coordination
/// catch-all.
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
    // A standalone cooking ask with no meal noun ("can you bake something?",
    // "how do I cook the lentils"). Checked BEFORE the swap shape so a recipe ask
    // that happens to sit next to a "change"/"swap" verb stays with the kitchen.
    if cooking_flavored {
        return Domain::Cooking;
    }
    // A "replace A with B" swap that names something edible — a PLAN change, so
    // it belongs to the planner (Nora) exactly like "swap dinner to tofu" above,
    // even when the dish is misspelled ("tacod") or the target is a raw
    // ingredient ("fennel") that no meal noun covers. Checked AFTER shopping (so
    // "swap the milk on the shopping list…" stays shopping) and after the recipe
    // rule, but BEFORE the bare-dish rule.
    if is_food_swap(&words) {
        return Domain::MealPlanning;
    }
    // An explicit "plan <edible>" request — a food-plan change even without a
    // meal noun or swap connector ("plan branzino for tomorrow night"). Checked
    // alongside the swap shape (both are PLAN changes → the planner, Nora) and
    // before the bare-dish rule so "plan pizza saturday" reaches the planner,
    // not the chef.
    if is_meal_plan_ask(&words) {
        return Domain::MealPlanning;
    }
    // A bare named dish ("pizza on Friday") — the kitchen's, so plain food
    // chatter reaches the chef's voice instead of falling through to the
    // concierge. Checked after shopping so "add pizza to the list" stays shopping.
    // Typo-tolerant so "carbonaraa tonight" still reaches the chef.
    if has_any_fuzzy(&words, DISH_WORDS) {
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// The legacy four-persona map retained for the group-election caller and
    /// focused classifier tests. It is never a fallback for [`load`](Self::load):
    /// task ownership and family-visible handoffs require project-local config.
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
    /// `None` when the file is absent, malformed, or has no usable agents.
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

    /// Load the owner map for a project root. With no valid project roster the
    /// map is empty, so [`decide_owner`](Self::decide_owner) fails open to the
    /// speaking persona instead of naming or re-routing to a compiled household.
    pub fn load(root: &Path) -> Self {
        Self::from_household_toml(root).unwrap_or_default()
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

/// Default amendment window: a follow-up from the SAME human, in the SAME chat,
/// about the SAME domain, arriving this soon after the first ask AMENDS the task
/// that ask created instead of spawning a sibling.
///
/// 3 minutes, matching [`DEFAULT_CLARIFY_WINDOW_SECS`] — Luca's pizza burst
/// spanned 14:27 → 14:29 ("pizza tomorrow night", "just mozzarella", "sorry just
/// margherita…"), three corrective refinements of one intent that the exact-ask
/// fingerprint could not collapse because each message said something *new*.
/// Long enough for a human to think of the correction, short enough that a
/// genuinely separate ask an hour later starts its own task.
pub const DEFAULT_AMEND_WINDOW_SECS: i64 = 180;

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
    /// The chat the ask arrived in — the amendment key, alongside `requester`
    /// and `domain`. Empty on records written before this field existed.
    pub chat_id: String,
    /// The human who asked. Amendment is same-sender only: two people asking
    /// about dinner in the same minute are two asks, not a correction.
    pub requester: String,
    /// The ask's [`Domain`] slug — "same subject" for amendment purposes.
    pub domain: String,
}

impl IntentRecord {
    fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "fingerprint": self.fingerprint,
            "task_id": self.task_id,
            "persona": self.persona,
            "chat_id": self.chat_id,
            "requester": self.requester,
            "domain": self.domain,
        })
        .to_string()
    }

    fn from_json_line(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let str_field = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        Some(Self {
            ts: v.get("ts")?.as_i64()?,
            fingerprint: v.get("fingerprint")?.as_str()?.to_string(),
            task_id: v.get("task_id")?.as_str()?.to_string(),
            persona: str_field("persona"),
            chat_id: str_field("chat_id"),
            requester: str_field("requester"),
            domain: str_field("domain"),
        })
    }
}

/// What a conversational turn should do with an ask that wants to become a task.
///
/// The three-task pizza spawn is the regression: the exact-ask dedupe
/// ([`IntentLedger::find_recent`]) only collapses *identical* asks, so three
/// rapid refinements of one intent each minted their own task, two of which were
/// then abandoned as duplicates. [`decide_creation`] adds the middle case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateDecision {
    /// A new ask — mint a task.
    Create,
    /// The identical ask already became `task_id` inside the dedupe window —
    /// reuse it, create nothing.
    Duplicate { task_id: String },
    /// A rapid corrective follow-up to `task_id`, which is still open: AMEND it
    /// with the new text (append to its description + log) instead of spawning a
    /// sibling that will only be abandoned later.
    Amend { task_id: String },
}

/// Single words that mark a message as a CORRECTION of something just said,
/// rather than a fresh request. Matched as whole tokens.
const CORRECTION_WORDS: &[&str] = &[
    "actually", "sorry", "just", "instead", "rather", "no", "not", "nope",
    "correction", "nevermind", "scratch", "meant", "oops", "wait", "also",
];

/// Multi-word correction cues, matched as substrings of the normalized text.
const CORRECTION_PHRASES: &[&str] = &[
    "make it",
    "make that",
    "change that",
    "change it to",
    "scratch that",
    "i mean",
    "i meant",
    "on second thought",
    "better yet",
    "or maybe",
    "forget the",
];

/// Whether `text` reads like a correction of an ask already in flight ("just
/// mozzarella", "sorry just margherita…", "actually make it Thursday") rather
/// than a new request.
///
/// This is what lets a fragment that carries too little signal to classify —
/// every one of Luca's refinements landed in the [`Domain::Coordination`]
/// catch-all — be folded into the open ask instead of minting a task of its own,
/// WITHOUT swallowing a genuinely new short request ("call the plumber").
pub fn is_corrective_follow_up(text: &str) -> bool {
    let words = tokens(text);
    if words.is_empty() {
        return false;
    }
    if CORRECTION_WORDS.iter().any(|w| has_word(&words, w)) {
        return true;
    }
    let normalized = words.join(" ");
    CORRECTION_PHRASES.iter().any(|p| normalized.contains(p))
}

/// Whether two domains are the same SUBJECT for amendment purposes.
///
/// Equal domains obviously are. Meal planning and cooking are one subject to a
/// family ("pizza tomorrow night" classifies as cooking, "swap Saturday dinner"
/// as meal planning — the same conversation). And a correction that classifies
/// into the [`Domain::Coordination`] catch-all carries no subject of its own, so
/// it inherits the open ask's.
fn amendable_subject(prev: Domain, next: Domain, next_text: &str) -> bool {
    if prev == next {
        return true;
    }
    let food = |d: Domain| matches!(d, Domain::MealPlanning | Domain::Cooking);
    if food(prev) && food(next) {
        return true;
    }
    next == Domain::Coordination && is_corrective_follow_up(next_text)
}

/// Decide whether an ask should mint a task, reuse one, or amend one.
///
/// Order matters:
/// 1. **Exact duplicate** inside [`IntentLedger::window_secs`] → reuse (the
///    existing single-owner safety net, unchanged).
/// 2. **Rapid corrective follow-up** — same chat + same requester + the same
///    subject ([`amendable_subject`]), inside
///    [`IntentLedger::amend_window_secs`], and the earlier task still open
///    (`is_open`) → amend it. The newest matching task wins.
/// 3. Otherwise → create.
///
/// `is_open` is injected (the caller reads the live graph) so this whole
/// decision is pure over the ledger + a predicate, and testable without a graph.
/// An empty `requester` never amends: without a sender to key on, a follow-up
/// cannot be told from a second person's separate ask.
pub fn decide_creation(
    root: &Path,
    ask: &str,
    chat_id: &str,
    requester: &str,
    now_epoch: i64,
    is_open: impl Fn(&str) -> bool,
) -> CreateDecision {
    let fp = fingerprint(ask, chat_id);
    if let Some(task_id) =
        IntentLedger::find_recent(root, &fp, now_epoch, IntentLedger::window_secs())
    {
        return CreateDecision::Duplicate { task_id };
    }
    let domain = classify_domain(ask);
    let candidate = IntentLedger::recent_for_sender(
        root,
        chat_id,
        requester,
        now_epoch,
        IntentLedger::amend_window_secs(),
    )
    .into_iter()
    .filter(|r| {
        Domain::from_slug(&r.domain)
            .is_some_and(|prev| amendable_subject(prev, domain, ask))
    })
    .filter(|r| is_open(&r.task_id))
    .next_back();
    match candidate {
        Some(r) => CreateDecision::Amend { task_id: r.task_id },
        None => CreateDecision::Create,
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

    /// Every intent from `requester` in `chat_id` still inside `window_secs` of
    /// `now_epoch`, oldest first — the candidates a rapid corrective follow-up
    /// could be amending.
    ///
    /// Records written before the chat/requester/domain fields existed carry
    /// empty strings and simply never match — an old ledger degrades to the
    /// previous behaviour (create a sibling) rather than mis-amending.
    pub fn recent_for_sender(
        root: &Path,
        chat_id: &str,
        requester: &str,
        now_epoch: i64,
        window_secs: i64,
    ) -> Vec<IntentRecord> {
        if window_secs <= 0 {
            return Vec::new();
        }
        let chat = chat_id.trim();
        let who = requester.trim();
        if who.is_empty() || chat.is_empty() {
            return Vec::new();
        }
        let Ok(body) = std::fs::read_to_string(Self::path(root)) else {
            return Vec::new();
        };
        body.lines()
            .filter_map(IntentRecord::from_json_line)
            .filter(|r| {
                r.chat_id == chat
                    && !r.requester.is_empty()
                    && r.requester.eq_ignore_ascii_case(who)
                    && !r.domain.is_empty()
                    && (now_epoch - r.ts) >= 0
                    && (now_epoch - r.ts) <= window_secs
            })
            .collect()
    }

    /// The amendment window in seconds — [`DEFAULT_AMEND_WINDOW_SECS`] unless
    /// `CASA_AMEND_WINDOW_SECS` overrides it (0 or negative disables amendment).
    pub fn amend_window_secs() -> i64 {
        std::env::var("CASA_AMEND_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_AMEND_WINDOW_SECS)
    }

    /// Durably record a fresh intent, creating `.casa/` if needed. Append-only:
    /// a new intent never clobbers an earlier one. `chat_id`/`requester`/`domain`
    /// are the amendment key ([`find_amendable`](Self::find_amendable)).
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        root: &Path,
        fp: &str,
        task_id: &str,
        persona: &str,
        chat_id: &str,
        requester: &str,
        domain: Domain,
        now_epoch: i64,
    ) -> std::io::Result<()> {
        let rec = IntentRecord {
            ts: now_epoch,
            fingerprint: fp.to_string(),
            task_id: task_id.to_string(),
            persona: persona.trim().to_string(),
            chat_id: chat_id.trim().to_string(),
            requester: requester.trim().to_string(),
            domain: domain.slug().to_string(),
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

// ---------------------------------------------------------------------------
// Clarification continuation
// ---------------------------------------------------------------------------

/// Default clarify window: a bare confirmation this many seconds after a voice
/// asked a clarifying question still continues THAT exchange. 3 minutes — long
/// enough for a human to read the question and tap "yes", short enough that an
/// unrelated later "ok" doesn't get glued onto a stale ask. Overridable with
/// `CASA_CLARIFY_WINDOW_SECS`.
pub const DEFAULT_CLARIFY_WINDOW_SECS: i64 = 180;

/// The short, self-contained confirmations that CONTINUE an open clarification
/// rather than start a new ask. Matched against the WHOLE normalized message, so
/// "yes" continues but "yes but make it pasta" (which carries new content) falls
/// through to a fresh election. English + the Italian the family uses.
const CONFIRMATION_PHRASES: &[&str] = &[
    "yes", "yep", "yeah", "yup", "y", "ok", "okay", "k", "sure", "sounds good",
    "yes please", "please do", "do it", "go ahead", "go for it", "perfect",
    "great", "confirmed", "correct", "that works", "works for me", "si", "sì",
    "certo", "va bene", "vabene", "fallo", "perfetto",
];

/// True when `text`, once normalized to space-joined lower-case alphanumeric
/// tokens, is EXACTLY one of the [`CONFIRMATION_PHRASES`]. A bare "yes"/"ok"/"si"
/// is a continuation; anything carrying new words is a new ask.
pub fn is_bare_confirmation(text: &str) -> bool {
    let normalized = tokens(text).join(" ");
    if normalized.is_empty() {
        return false;
    }
    CONFIRMATION_PHRASES.iter().any(|p| *p == normalized)
}

/// One open clarification exchange: a `voice` (persona / agent id) asked `human`
/// a clarifying question about `original_ask` in `chat` at `ts`. A bare
/// confirmation from the same human within the window is routed back to `voice`
/// carrying `original_ask`, so the confirmation is answered by the persona that
/// asked — not re-elected from scratch (the fennel bug, where a "yes" summoned a
/// different voice) — and reuses the original ask's [`fingerprint`] so no
/// duplicate task is minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarifyExchange {
    /// Unix epoch seconds the clarifying question was asked.
    pub ts: i64,
    /// The chat the exchange lives in.
    pub chat_id: String,
    /// The human who was asked.
    pub human: String,
    /// The persona / agent id that asked (the voice a confirmation continues).
    pub voice: String,
    /// The original ask text — replayed as the body of the continued turn so the
    /// composer (and any task it creates) works the ORIGINAL intent.
    pub original_ask: String,
}

impl ClarifyExchange {
    /// The fingerprint of the ORIGINAL ask, so a task minted on confirmation is
    /// deduped against one minted on the first turn ([`IntentLedger`]).
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.original_ask, &self.chat_id)
    }

    fn to_json_line(&self) -> String {
        serde_json::json!({
            "ts": self.ts,
            "chat_id": self.chat_id,
            "human": self.human,
            "voice": self.voice,
            "original_ask": self.original_ask,
        })
        .to_string()
    }

    fn from_json_line(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        Some(Self {
            ts: v.get("ts")?.as_i64()?,
            chat_id: v.get("chat_id")?.as_str()?.to_string(),
            human: v.get("human")?.as_str()?.to_string(),
            voice: v.get("voice")?.as_str()?.to_string(),
            original_ask: v.get("original_ask")?.as_str()?.to_string(),
        })
    }
}

/// Durable, file-backed clarification ledger — the twin of [`IntentLedger`],
/// append-only JSONL under `<root>/.casa/clarify.jsonl` so a confirmation that
/// arrives in a SEPARATE listener/gateway invocation (they always are) still
/// finds the open exchange. Dependency-free file I/O, no model call.
pub struct ClarifyLedger;

impl ClarifyLedger {
    /// Path to the append-only JSONL under `<root>/.casa/`.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".casa").join("clarify.jsonl")
    }

    /// The clarify window in seconds — [`DEFAULT_CLARIFY_WINDOW_SECS`] unless
    /// `CASA_CLARIFY_WINDOW_SECS` overrides it.
    pub fn window_secs() -> i64 {
        std::env::var("CASA_CLARIFY_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_CLARIFY_WINDOW_SECS)
    }

    /// Open a clarification window: record that `voice` asked `human` about
    /// `original_ask` in `chat` at `now_epoch`. Creates `.casa/` if needed.
    pub fn open(
        root: &Path,
        chat_id: &str,
        human: &str,
        voice: &str,
        original_ask: &str,
        now_epoch: i64,
    ) -> std::io::Result<()> {
        let rec = ClarifyExchange {
            ts: now_epoch,
            chat_id: chat_id.trim().to_string(),
            human: human.trim().to_string(),
            voice: voice.trim().to_string(),
            original_ask: original_ask.to_string(),
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

    /// The most recent still-open exchange for `(chat, human)` within
    /// `window_secs` of `now_epoch`, or `None`. The last matching record wins so
    /// a fresh clarifying question supersedes an older one.
    pub fn pending(
        root: &Path,
        chat_id: &str,
        human: &str,
        now_epoch: i64,
        window_secs: i64,
    ) -> Option<ClarifyExchange> {
        let path = Self::path(root);
        let body = std::fs::read_to_string(&path).ok()?;
        let chat = chat_id.trim();
        let who = human.trim();
        body.lines()
            .filter_map(ClarifyExchange::from_json_line)
            .filter(|r| {
                r.chat_id == chat
                    && r.human == who
                    && (now_epoch - r.ts) >= 0
                    && (now_epoch - r.ts) <= window_secs
            })
            .last()
    }
}

/// The clarification-continuation decision: if `text` is a bare confirmation
/// ([`is_bare_confirmation`]) AND `(chat, human)` has an open exchange within the
/// window, return it so the caller can route the confirmation back to the voice
/// that asked — reusing the original ask, WITHOUT a new election. Otherwise
/// `None`, and the caller runs the normal election.
pub fn clarify_continuation(
    root: &Path,
    chat_id: &str,
    human: &str,
    text: &str,
    now_epoch: i64,
    window_secs: i64,
) -> Option<ClarifyExchange> {
    if !is_bare_confirmation(text) {
        return None;
    }
    ClarifyLedger::pending(root, chat_id, human, now_epoch, window_secs)
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
    fn missing_or_malformed_household_never_uses_a_compiled_owner() {
        let dir = tempfile::tempdir().unwrap();
        let missing = OwnerMap::load(dir.path());
        assert_eq!(missing.owner_for_ask("swap Thursday dinner to soup"), None);
        assert_eq!(
            missing.decide_owner("hearth", "swap Thursday dinner to soup"),
            OwnerDecision::Owner,
            "without project-local ownership, the speaking persona keeps the ask",
        );

        std::fs::write(dir.path().join("household.toml"), "not = [valid").unwrap();
        let malformed = OwnerMap::load(dir.path());
        assert_eq!(malformed.owner_for_ask("swap Thursday dinner to soup"), None);
        assert_eq!(
            malformed.decide_owner("hearth", "swap Thursday dinner to soup"),
            OwnerDecision::Owner,
            "malformed project config must not resurrect another household's owner",
        );
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
        IntentLedger::record(
            root,
            &fp,
            "swap-thursday-dinner",
            "nora",
            "group-42",
            "Luca",
            Domain::MealPlanning,
            1000,
        )
        .unwrap();
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

    // ---- rapid corrective follow-ups AMEND, they don't spawn siblings ------
    //
    // Luca, 2026-07-24 14:27/14:28/14:29: "pizza tomorrow night", then "just
    // mozzarella", then "sorry just margherita…". Three tasks were minted; two
    // were abandoned as duplicates minutes later and each abandon spoke a false
    // "I'll take another crack at it" to the family.

    /// Record a first ask exactly as `try_create_origin_task` does.
    fn record_ask(root: &Path, ask: &str, chat: &str, who: &str, task_id: &str, ts: i64) {
        IntentLedger::record(
            root,
            &fingerprint(ask, chat),
            task_id,
            "nora",
            chat,
            who,
            classify_domain(ask),
            ts,
        )
        .unwrap();
    }

    #[test]
    fn rapid_follow_up_amends_the_open_task_instead_of_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        let always_open = |_: &str| true;

        // 14:27 — the first ask mints the task.
        assert_eq!(
            decide_creation(
                root,
                "can we do pizza tomorrow night",
                chat,
                "Luca",
                1000,
                always_open
            ),
            CreateDecision::Create
        );
        record_ask(
            root,
            "can we do pizza tomorrow night",
            chat,
            "Luca",
            "update-saturday-dinner-plan-to",
            1000,
        );

        // 14:28 — a refinement, 60s later. Different words, so the exact-ask
        // dedupe cannot see it; the amendment window can.
        assert_eq!(
            decide_creation(root, "just mozzarella", chat, "Luca", 1060, always_open),
            CreateDecision::Amend {
                task_id: "update-saturday-dinner-plan-to".to_string()
            }
        );

        // 14:29 — a second refinement, 120s in. Still an amendment.
        assert_eq!(
            decide_creation(
                root,
                "sorry just margherita pizza for saturday",
                chat,
                "Luca",
                1120,
                always_open
            ),
            CreateDecision::Amend {
                task_id: "update-saturday-dinner-plan-to".to_string()
            }
        );
    }

    #[test]
    fn amendment_requires_same_sender_same_domain_same_chat_and_a_live_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        let always_open = |_: &str| true;
        record_ask(
            root,
            "can we do pizza tomorrow night",
            chat,
            "Luca",
            "update-saturday-dinner-plan-to",
            1000,
        );

        // A DIFFERENT person asking about dinner is a separate ask, not a
        // correction of Luca's.
        assert_eq!(
            decide_creation(root, "just mozzarella", chat, "Sara", 1060, always_open),
            CreateDecision::Create
        );
        // A different DOMAIN from the same person is a separate ask.
        assert_eq!(
            decide_creation(
                root,
                "book me a dentist appointment on tuesday",
                chat,
                "Luca",
                1060,
                always_open
            ),
            CreateDecision::Create
        );
        // A different CHAT is a separate conversation.
        assert_eq!(
            decide_creation(root, "just mozzarella", "other-chat", "Luca", 1060, always_open),
            CreateDecision::Create
        );
        // Past the window (3 min) it is a genuinely new ask.
        assert_eq!(
            decide_creation(root, "just mozzarella", chat, "Luca", 1000 + 400, always_open),
            CreateDecision::Create
        );
        // An unnamed requester can never amend — no sender to key on.
        assert_eq!(
            decide_creation(root, "just mozzarella", chat, "  ", 1060, always_open),
            CreateDecision::Create
        );
    }

    #[test]
    fn a_new_unrelated_request_is_not_swallowed_by_the_amendment_window() {
        // The catch-all inherit rule is gated on a CORRECTION cue, so a short but
        // genuinely new ask still mints its own task even inside the window.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        record_ask(
            root,
            "can we do pizza tomorrow night",
            chat,
            "Luca",
            "update-saturday-dinner-plan-to",
            1000,
        );
        for fresh in ["call the plumber", "remind everyone about the party"] {
            assert_eq!(
                decide_creation(root, fresh, chat, "Luca", 1060, |_| true),
                CreateDecision::Create,
                "'{fresh}' is a new ask, not a correction"
            );
        }
        // …while the corrective fragments are folded in.
        for correction in ["just mozzarella", "actually no onions", "sorry, make it thin crust"] {
            assert!(
                is_corrective_follow_up(correction),
                "'{correction}' reads as a correction"
            );
            assert_eq!(
                decide_creation(root, correction, chat, "Luca", 1060, |_| true),
                CreateDecision::Amend {
                    task_id: "update-saturday-dinner-plan-to".to_string()
                },
                "'{correction}' must amend, not spawn"
            );
        }
    }

    #[test]
    fn a_closed_task_is_never_amended() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        record_ask(
            root,
            "can we do pizza tomorrow night",
            chat,
            "Luca",
            "update-saturday-dinner-plan-to",
            1000,
        );
        // The work already landed → a follow-up is a NEW ask, not a correction.
        assert_eq!(
            decide_creation(root, "just mozzarella", chat, "Luca", 1060, |_| false),
            CreateDecision::Create
        );
    }

    #[test]
    fn an_identical_repeat_is_still_a_duplicate_not_an_amendment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        let ask = "can we do pizza tomorrow night";
        record_ask(root, ask, chat, "Luca", "update-saturday-dinner-plan-to", 1000);
        assert_eq!(
            decide_creation(root, ask, chat, "Luca", 1030, |_| true),
            CreateDecision::Duplicate {
                task_id: "update-saturday-dinner-plan-to".to_string()
            },
            "the exact-ask safety net still wins — reuse, don't amend"
        );
    }

    #[test]
    fn a_legacy_ledger_without_the_amendment_key_never_mis_amends() {
        // Records written before chat/requester/domain existed carry empty
        // strings; they must degrade to "create", never glue a new ask onto an
        // unrelated old task.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = IntentLedger::path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "ts": 1000,
                    "fingerprint": fingerprint("old ask", "8905220378"),
                    "task_id": "ancient-task",
                    "persona": "nora",
                })
            ),
        )
        .unwrap();
        assert_eq!(
            decide_creation(root, "just mozzarella", "8905220378", "Luca", 1060, |_| true),
            CreateDecision::Create
        );
    }

    // ---- typo-tolerant + swap-shaped classification ----------------------

    #[test]
    fn fennel_swap_inbound_classifies_as_meal_planning_not_coordination() {
        // THE LIVE REGRESSION (Luca's fennel transcript, chat 8905220378,
        // 2026-07-14): "hey can you swap tacod for grilled fennel". The dish is
        // misspelled ("tacod") and the target is a raw vegetable ("fennel") that
        // no meal noun covers, so the OLD classifier dropped it to Coordination →
        // Otto — and Otto (calendar/shopping) is the wrong owner for a food swap.
        // It is a PLAN change and must classify as MealPlanning → Nora.
        assert_eq!(
            classify_domain("hey can you swap tacod for grilled fennel"),
            Domain::MealPlanning
        );
        let m = OwnerMap::casa_default();
        assert_eq!(
            m.owner_for_ask("hey can you swap tacod for grilled fennel"),
            Some("nora"),
            "a food swap must NOT fall to the concierge (otto)"
        );
    }

    #[test]
    fn plan_branzino_ask_classifies_as_meal_planning_not_coordination() {
        // THE LIVE REGRESSION (Luca, 2026-07-17): "hey plan for branzino for
        // tomorrow night" — a fish the classifier had never heard of, with a
        // "plan" verb but no meal noun and no swap connector. The old classifier
        // dropped it to Coordination → Otto, elect_responders fell to a
        // whole-roster answer, and FOUR bots replied. It must classify as a food
        // plan change → Nora.
        assert_eq!(
            classify_domain("hey plan for branzino for tomorrow night"),
            Domain::MealPlanning
        );
        let m = OwnerMap::casa_default();
        assert_eq!(
            m.owner_for_ask("hey plan for branzino for tomorrow night"),
            Some("nora"),
            "a meal-plan ask must NOT fall to the concierge (otto)"
        );
        // The plan verb is required — a bare ingredient mention is still neutral.
        assert_eq!(classify_domain("we're out of branzino"), Domain::Coordination);
        // Other proteins + typo tolerance behave the same.
        assert_eq!(classify_domain("plan salmon for saturday"), Domain::MealPlanning);
        assert_eq!(classify_domain("can you plan branzin for friday"), Domain::MealPlanning);
    }

    #[test]
    fn misspelled_dish_swaps_still_route_to_food() {
        // Typo tolerance mirrors the roster-name matcher: a one-letter slip in the
        // dish/ingredient still classifies as a food plan change.
        assert_eq!(classify_domain("swap tacod for burgers"), Domain::MealPlanning);
        assert_eq!(classify_domain("replace the chiken with trout"), Domain::MealPlanning);
        assert_eq!(classify_domain("switch fridays pizzza for pasta"), Domain::MealPlanning);
        // Correctly-spelled ingredient swap with no meal noun, too.
        assert_eq!(classify_domain("swap chicken for tofu"), Domain::MealPlanning);
    }

    #[test]
    fn swap_shape_does_not_hijack_non_food_or_recipe_asks() {
        // A recipe ask sitting next to a swap-ish verb stays with the kitchen —
        // "change" + "lentils" must NOT become a plan swap.
        assert_eq!(classify_domain("how do I cook the lentils"), Domain::Cooking);
        assert_eq!(classify_domain("change how you roast the chicken"), Domain::Cooking);
        // A workout / calendar swap is still owned by workouts / calendar, not food.
        assert_eq!(classify_domain("swap my gym session to friday"), Domain::Workouts);
        assert_eq!(
            classify_domain("reschedule the dentist appointment"),
            Domain::Calendar
        );
        // A swap with NOTHING edible is not a food swap.
        assert_eq!(classify_domain("swap my seat with yours"), Domain::Coordination);
        // A bare craving with an incidental "changed" but NO connector is a dish
        // for the chef, not a plan swap (morning-taco-bugs must stay green).
        assert_eq!(
            classify_domain("hey I changed my mind on friday I want tacos"),
            Domain::Cooking
        );
    }

    #[test]
    fn pre_existing_classifications_unchanged_by_typo_tolerance() {
        // The typo-tolerant edible matcher must not disturb the shipped cases.
        assert_eq!(
            classify_domain("swap Thursday dinner to grilled tofu"),
            Domain::MealPlanning
        );
        assert_eq!(classify_domain("pizza on friday"), Domain::Cooking);
        assert_eq!(classify_domain("add pizza to the shopping list"), Domain::Shopping);
        assert_eq!(classify_domain("add rice to the list"), Domain::Coordination);
        assert_eq!(classify_domain("who is picking up the kids?"), Domain::Coordination);
    }

    #[test]
    fn edit_distance_le_1_basics() {
        assert!(edit_distance_le_1("tacod", "taco")); // insertion
        assert!(edit_distance_le_1("chiken", "chicken")); // deletion
        assert!(edit_distance_le_1("pizzza", "pizza")); // insertion
        assert!(edit_distance_le_1("tofo", "tofu")); // substitution
        // A transposition ("fennle"→"fennel") is distance 2 — deliberately NOT
        // matched, so we never over-reach on distinct words.
        assert!(!edit_distance_le_1("fennle", "fennel"));
        assert!(!edit_distance_le_1("taco", "sushi"));
        assert!(!edit_distance_le_1("list", "salad")); // 2+ apart
    }

    // ---- clarification continuation --------------------------------------

    #[test]
    fn bare_confirmations_recognised_but_content_replies_are_not() {
        for yes in ["yes", "Yes", "  ok  ", "yep", "sure", "si", "sì", "do it", "sounds good"] {
            assert!(is_bare_confirmation(yes), "{yes:?} should be a confirmation");
        }
        for no in ["yes but make it pasta", "no", "actually pizza", "maybe later", ""] {
            assert!(!is_bare_confirmation(no), "{no:?} should NOT be a confirmation");
        }
    }

    #[test]
    fn clarify_continuation_routes_yes_back_to_the_asking_voice() {
        // The fennel "yes" bug: Nora asked a clarifying question, and a bare "yes"
        // re-elected from scratch (Coach Mira answered). With an open exchange, the
        // "yes" continues with NORA and replays the original ask.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "8905220378";
        let human = "luca";
        let ask = "hey can you swap tacod for grilled fennel";
        // No exchange yet → a "yes" is not a continuation.
        assert_eq!(clarify_continuation(root, chat, human, "yes", 1000, 180), None);
        // Nora asks a clarifying question → window opens.
        ClarifyLedger::open(root, chat, human, "nora", ask, 1000).unwrap();
        let ex = clarify_continuation(root, chat, human, "yes", 1030, 180)
            .expect("a bare yes within the window continues the exchange");
        assert_eq!(ex.voice, "nora");
        assert_eq!(ex.original_ask, ask);
        // The continuation reuses the ORIGINAL ask's fingerprint → no dup task.
        assert_eq!(ex.fingerprint(), fingerprint(ask, chat));
    }

    #[test]
    fn clarify_continuation_respects_window_human_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "grp";
        ClarifyLedger::open(root, chat, "luca", "nora", "swap x for y", 1000).unwrap();
        // Outside the window → not a continuation.
        assert_eq!(clarify_continuation(root, chat, "luca", "yes", 1400, 180), None);
        // A DIFFERENT human's "yes" does not continue Luca's exchange.
        assert_eq!(clarify_continuation(root, chat, "mara", "yes", 1030, 180), None);
        // A content-bearing reply is a fresh ask, not a continuation.
        assert_eq!(
            clarify_continuation(root, chat, "luca", "make it pasta instead", 1030, 180),
            None
        );
    }

    #[test]
    fn clarify_pending_latest_supersedes_older() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let chat = "grp";
        ClarifyLedger::open(root, chat, "luca", "nora", "swap a for b", 1000).unwrap();
        ClarifyLedger::open(root, chat, "luca", "bruno", "how do i cook b", 1050).unwrap();
        let ex = ClarifyLedger::pending(root, chat, "luca", 1060, 180).unwrap();
        assert_eq!(ex.voice, "bruno", "the freshest open exchange wins");
    }
}
