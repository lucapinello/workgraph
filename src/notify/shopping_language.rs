//! Shopping mutation-language safety — the ENGINE twin of the gateway's detectors.
//!
//! The live-conversation certification (weekly_planner
//! `docs/reviews/LIVE-CONVO-CERT-2026-07-26.md`, P1 "shopping mutation language is not
//! safe enough") found four ways ordinary family wording mis-handled the list. The
//! GATEWAY half was fixed in `claw3d-bridge/src/{shoppingMatch,weekSource,weekAdapter}.mjs`
//! (task shopping-language-safety) — but those lanes only run on the gateway group/web
//! send path. A message arriving in the family **Telegram group** is elected and
//! answered by THIS engine and never reaches them, so the same four bugs lived on here
//! (task engine-shopping-language). Measured, before this module existed:
//!
//! | phrase                                        | engine did                        |
//! |-----------------------------------------------|-----------------------------------|
//! | "Add glorptwax to shopping."                  | WROTE it: "Done — glorptwax …🛒"  |
//! | "Don't add olive oil to the list yet—ask me first." | WROTE olive oil (negation ignored) |
//! | "Remove AA batteries again."                  | nothing — no removal path at all  |
//! | "We are out of olive oil—add olive oil."      | nothing — "out of" form unknown   |
//!
//! The rules here are a deliberate, documented PORT of the gateway vocabulary so the
//! two halves cannot drift (the stub-honored/production-ignored disease, docs/41):
//!
//! * [`plausible_grocery`] ⇄ `shoppingMatch.plausibleGrocery` — is this a thing you can
//!   buy? Deliberately WIDER than the aisle taxonomy, because an unknown aisle is a
//!   taxonomy gap, not a nonsense word ("freezer bags" is real, "glorptwax" is not).
//! * [`first_clause`] ⇄ `weekSource.firstClause` — a SENTENCE is not one item, so
//!   "we are out of olive oil—add olive oil" yields ONE item, not the duplicated literal.
//! * [`detect_negation`] ⇄ `weekSource.detectShoppingNegation` — a HOLD asks first, a
//!   CANCEL takes a matching item back off. Either way the write is suppressed.
//! * [`detect_remove_intent`] ⇄ `weekSource.detectShoppingRemoveIntent` — a real removal
//!   phrasing, with crossing-off (= bought, the row stays) deliberately excluded.
//!
//! Everything is pure: no filesystem, no bot, no model. The plan write itself lives in
//! [`super::fast_lane`], which owns this module's verdicts, and the credential-free
//! test seam is `wg telegram shopping <text>`.

use std::sync::LazyLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// Tokenization (⇄ shoppingMatch.itemTokens / singular / tokenNear)
// ---------------------------------------------------------------------------

/// Units + filler words that describe an amount, not the food itself.
const UNIT_WORDS: &[&str] = &[
    "can", "cans", "tin", "tins", "jar", "jars", "bottle", "bottles", "box", "boxes", "pack",
    "packs", "packet", "packets", "bag", "bags", "bunch", "bunches", "loaf", "loaves", "carton",
    "cartons", "dozen", "piece", "pieces", "pcs", "pc", "slice", "slices", "clove", "cloves",
    "head", "heads", "punnet", "punnets", "tub", "tubs", "g", "kg", "mg", "ml", "l", "cl", "oz",
    "lb", "lbs", "gram", "grams", "kilo", "kilos", "litre", "litres", "liter", "liters", "gallon",
    "gallons", "some", "of", "the", "a", "an", "and", "for", "fresh", "ripe", "large", "small",
    "big", "extra", "more", "few", "couple", "pair", "x",
];

/// A crude singular stem so "eggs"↔"egg" and "tomatoes"↔"tomato" collapse.
/// Mirrors `shoppingMatch.singular` rule for rule.
pub fn singular(word: &str) -> String {
    let s = word.to_lowercase();
    if s.len() <= 3 {
        return s;
    }
    if s.ends_with("ies") && s.len() > 4 {
        return format!("{}y", &s[..s.len() - 3]);
    }
    if s.ends_with("ses")
        || s.ends_with("xes")
        || s.ends_with("zes")
        || s.ends_with("ches")
        || s.ends_with("shes")
    {
        return s[..s.len() - 2].to_string();
    }
    if s.ends_with("oes") && s.len() > 4 {
        return s[..s.len() - 2].to_string();
    }
    if s.ends_with('s') && !s.ends_with("ss") && s.len() > 3 {
        return s[..s.len() - 1].to_string();
    }
    s
}

/// One-edit closeness for length-≥4 tokens (⇄ `shoppingMatch.tokenNear`): absorbs a
/// plural or a typo without letting different nouns collide.
fn token_near(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (ab, bb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if ab.len() < 4 || bb.len() < 4 {
        return false;
    }
    if ab.len().abs_diff(bb.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0usize, 0usize, 0usize);
    while i < ab.len() && j < bb.len() {
        if ab[i] == bb[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if ab.len() > bb.len() {
            i += 1;
        } else if bb.len() > ab.len() {
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    edits + (ab.len() - i) + (bb.len() - j) <= 1
}

static PARENS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^)]*\)").expect("valid parenthetical regex"));
static QTY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[×x*]\s*\d+|\b\d+\s*[×x*]\b|\+\s*\d+").expect("valid quantity regex")
});
static NON_ALNUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("valid non-alnum regex"));
static BARE_NUM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d+\b").expect("valid bare-number regex"));

/// Strip the amount noise off a shopping line, leaving the naming words
/// (⇄ `shoppingMatch.stripAmounts`).
fn strip_amounts(text: &str) -> String {
    let lowered = text.to_lowercase().replace(['\u{2019}', '\''], " ");
    let s = PARENS_RE.replace_all(&lowered, " ");
    let s = QTY_RE.replace_all(&s, " ");
    let s = NON_ALNUM_RE.replace_all(&s, " ");
    let s = BARE_NUM_RE.replace_all(&s, " ");
    s.trim().to_string()
}

/// A shopping line → its ordered distinctive naming tokens (⇄ `shoppingMatch.itemTokens`).
pub fn item_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in strip_amounts(text).split_whitespace() {
        if tok.chars().count() >= 2 && !UNIT_WORDS.contains(&tok) && !out.iter().any(|s| s == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Two tokens name the same thing if they are one-edit close OR share a singular stem
/// (⇄ `shoppingMatch.tokenMatch`).
fn token_match(a: &str, b: &str) -> bool {
    token_near(a, b) || singular(a) == singular(b)
}

/// Do two shopping lines refer to the same item? (⇄ `shoppingMatch.sameItem`.) The head
/// nouns must be close, and for a multi-word item a majority of the terser item's
/// tokens must match — so "green beans" ≠ "black beans" but
/// "green beans" == "Green beans, 300 g (Tue)". This is what lets a conversational
/// "take the batteries off" find the plan's "AA batteries ×4" row.
pub fn same_item(a: &str, b: &str) -> bool {
    let ta = item_tokens(a);
    let tb = item_tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    let (short, long) = if ta.len() <= tb.len() {
        (&ta, &tb)
    } else {
        (&tb, &ta)
    };
    if !long.iter().any(|u| token_match(&short[0], u)) {
        return false;
    }
    let shared = short
        .iter()
        .filter(|t| long.iter().any(|u| token_match(t, u)))
        .count();
    if short.len() == 1 {
        return shared == 1;
    }
    shared as f64 / short.len() as f64 >= 0.6
}

// ---------------------------------------------------------------------------
// The aisle taxonomy (⇄ shoppingMatch.AISLES / categorizeAisle)
// ---------------------------------------------------------------------------

struct Aisle {
    aisle: &'static str,
    foods: &'static [&'static str],
}

const AISLES: &[Aisle] = &[
    Aisle {
        aisle: "produce",
        foods: &[
            "tomato", "banana", "apple", "lemon", "lime", "orange", "spinach", "lettuce", "onion",
            "garlic", "potato", "carrot", "pepper", "cucumber", "broccoli", "cauliflower", "berry",
            "strawberry", "blueberry", "raspberry", "grape", "avocado", "mushroom", "basil",
            "cilantro", "coriander", "parsley", "kale", "celery", "ginger", "mango", "pear",
            "peach", "plum", "zucchini", "courgette", "eggplant", "aubergine", "leek", "scallion",
            "chilli", "chili", "herb", "salad", "cabbage", "corn", "radish", "beet", "squash",
            "pumpkin", "melon", "pineapple", "cherry", "apricot", "fig", "kiwi", "shallot",
            "asparagus", "greenbean", "sprout", "rocket", "arugula", "melanzane", "zucchine",
            "pomodori", "basilico", "funghi", "insalata", "cipolla", "aglio", "patate", "carote",
            "limoni", "fragole", "spinaci", "rucola", "carciofi", "fagiolini", "cavolo", "zucca",
            "finocchio",
        ],
    },
    Aisle {
        aisle: "dairy",
        foods: &[
            "milk",
            "cheese",
            "butter",
            "yogurt",
            "yoghurt",
            "cream",
            "egg",
            "feta",
            "mozzarella",
            "parmesan",
            "cheddar",
            "brie",
            "curd",
            "ghee",
            "margarine",
            "kefir",
            "halloumi",
            "ricotta",
            "parmigiano",
            "pecorino",
            "mascarpone",
            "gorgonzola",
            "provolone",
            "burrata",
            "stracchino",
            "formaggio",
            "latte",
            "burro",
            "uova",
            "panna",
            "yougurt",
        ],
    },
    Aisle {
        aisle: "meat",
        foods: &[
            "chicken",
            "beef",
            "pork",
            "salmon",
            "tuna",
            "shrimp",
            "prawn",
            "bacon",
            "sausage",
            "mince",
            "turkey",
            "lamb",
            "cod",
            "haddock",
            "steak",
            "ham",
            "chorizo",
            "guanciale",
            "pancetta",
            "anchovy",
            "sardine",
            "mussel",
            "crab",
            "duck",
            "veal",
            "mackerel",
            "branzino",
            "bass",
            "seabass",
            "trout",
            "bream",
            "seabream",
            "swordfish",
            "halibut",
            "tilapia",
            "snapper",
            "sole",
            "monkfish",
            "octopus",
            "squid",
            "calamari",
            "clam",
            "scallop",
            "oyster",
            "herring",
            "pollock",
            "catfish",
            "whiting",
            "plaice",
            "sardines",
            "kipper",
            "roe",
            "caviar",
            "lobster",
            "crayfish",
            "langoustine",
            "pepperoni",
            "salami",
            "mortadella",
            "bresaola",
            "speck",
            "pesce",
            "pollo",
            "manzo",
            "maiale",
            "tacchino",
            "agnello",
            "vitello",
            "prosciutto",
            "salsiccia",
            "gamberi",
            "gamberetti",
            "cozze",
            "vongole",
            "polpo",
            "seppia",
            "tonno",
            "acciughe",
            "sarde",
            "sgombro",
            "merluzzo",
            "spigola",
            "orata",
            "trota",
            "salmone",
        ],
    },
    Aisle {
        aisle: "bakery",
        foods: &[
            "bread",
            "bagel",
            "roll",
            "baguette",
            "croissant",
            "tortilla",
            "pita",
            "naan",
            "bun",
            "muffin",
            "brioche",
            "focaccia",
            "ciabatta",
            "crumpet",
            "pane",
            "panino",
            "panini",
            "grissini",
            "schiacciata",
            "cornetto",
        ],
    },
    Aisle {
        aisle: "frozen",
        foods: &["frozen", "icecream", "sorbet", "gelato"],
    },
    Aisle {
        aisle: "household",
        foods: &[
            "soap",
            "detergent",
            "paper",
            "towel",
            "foil",
            "napkin",
            "sponge",
            "battery",
            "bleach",
            "wipe",
            "tissue",
            "shampoo",
            "toothpaste",
            "toilet",
            "cleaner",
            "dishwasher",
            "laundry",
            "clingfilm",
        ],
    },
    Aisle {
        aisle: "pantry",
        foods: &[
            "rice",
            "pasta",
            "flour",
            "sugar",
            "oil",
            "salt",
            "bean",
            "chickpea",
            "lentil",
            "sauce",
            "can",
            "tin",
            "spice",
            "cereal",
            "coffee",
            "tea",
            "stock",
            "broth",
            "vinegar",
            "honey",
            "jam",
            "peanut",
            "nut",
            "oat",
            "noodle",
            "couscous",
            "quinoa",
            "passata",
            "coconut",
            "chocolate",
            "biscuit",
            "cookie",
            "crisp",
            "snack",
            "juice",
            "soda",
            "wine",
            "beer",
            "ketchup",
            "mayo",
            "mustard",
            "curry",
            "raisin",
            "syrup",
            "crackers",
            "paprika",
            "cumin",
            "oregano",
            "cinnamon",
            "nutmeg",
            "turmeric",
            "cardamom",
            "cayenne",
            "saffron",
            "thyme",
            "rosemary",
            "sage",
            "clove",
            "bay",
            "chive",
            "dill",
            "fennel",
            "seasoning",
            "peppercorn",
            "olive",
            "caper",
            "pickle",
            "gherkin",
            "tahini",
            "pesto",
            "tapenade",
            "hummus",
            "soy",
            "miso",
            "sriracha",
            "tabasco",
            "marmite",
            "gravy",
            "cornflour",
            "cornstarch",
            "breadcrumb",
            "molasses",
            "treacle",
            "fagioli",
            "ceci",
            "lenticchie",
            "riso",
            "farina",
            "olio",
            "zucchero",
            "aceto",
            "caffe",
            "origano",
            "polenta",
            "semola",
            "sugo",
            "ragu",
        ],
    },
];

/// Non-food household markers that decide the line ahead of the food loop, so
/// "kitchen roll" cannot be dragged into bakery by its bread "roll"
/// (⇄ `shoppingMatch.HOUSEHOLD_GUARD`).
const HOUSEHOLD_GUARD: &[&str] = &[
    "kitchen",
    "toilet",
    "loo",
    "cling",
    "clingfilm",
    "bin",
    "foil",
    "napkin",
    "tissue",
    "detergent",
    "bleach",
    "sponge",
    "wipe",
    "wipes",
    "toothpaste",
    "shampoo",
    "dishwasher",
    "laundry",
];

/// A food line → its coarse aisle, or `None` when nothing names a known good
/// (⇄ `shoppingMatch.categorizeAisle`, same two passes: exact/stem wins over fuzzy).
pub fn categorize_aisle(text: &str) -> Option<&'static str> {
    let toks = item_tokens(text);
    if toks.is_empty() {
        return None;
    }
    let stems: Vec<String> = toks.iter().map(|t| singular(t)).collect();
    if toks.iter().any(|t| HOUSEHOLD_GUARD.contains(&t.as_str())) {
        return Some("household");
    }
    for (i, tok) in toks.iter().enumerate() {
        for cat in AISLES {
            if cat
                .foods
                .iter()
                .any(|f| *f == tok.as_str() || singular(f) == stems[i])
            {
                return Some(cat.aisle);
            }
        }
    }
    for (i, tok) in toks.iter().enumerate() {
        for cat in AISLES {
            if cat
                .foods
                .iter()
                .any(|f| token_near(f, tok) || token_near(&singular(f), &stems[i]))
            {
                return Some(cat.aisle);
            }
        }
    }
    None
}

/// Real non-food purchases the aisle lexicon has no entry for
/// (⇄ `shoppingMatch.GENERIC_GOODS`).
const GENERIC_GOODS: &[&str] = &[
    "freezer",
    "ziploc",
    "sandwich",
    "storage",
    "liner",
    "liners",
    "wrap",
    "wraps",
    "film",
    "parchment",
    "greaseproof",
    "skewer",
    "skewers",
    "straw",
    "straws",
    "filter",
    "filters",
    "cutlery",
    "plate",
    "plates",
    "cup",
    "cups",
    "match",
    "matches",
    "candle",
    "candles",
    "bulb",
    "bulbs",
    "lightbulb",
    "battery",
    "batteries",
    "charcoal",
    "briquette",
    "briquettes",
    "propane",
    "firelighter",
    "firelighters",
    "glue",
    "tape",
    "string",
    "twine",
    "pen",
    "pens",
    "pencil",
    "notebook",
    "stamp",
    "stamps",
    "flower",
    "flowers",
    "plant",
    "plants",
    "seed",
    "seeds",
    "soil",
    "compost",
    "sunscreen",
    "suncream",
    "deodorant",
    "razor",
    "razors",
    "floss",
    "plaster",
    "plasters",
    "bandage",
    "bandages",
    "vitamin",
    "vitamins",
    "painkiller",
    "paracetamol",
    "ibuprofen",
    "lotion",
    "moisturiser",
    "moisturizer",
    "conditioner",
    "brush",
    "toothbrush",
    "comb",
    "cotton",
    "swab",
    "swabs",
    "sanitiser",
    "sanitizer",
    "mask",
    "masks",
    "diaper",
    "diapers",
    "nappy",
    "nappies",
    "wipe",
    "wipes",
    "pet",
    "dog",
    "cat",
    "kibble",
    "litter",
    "treat",
    "treats",
    "birthday",
    "party",
    "balloon",
    "balloons",
    "gift",
    "card",
    "cards",
    // Beyond the gateway list: household consumables the family names by their
    // package word, which `item_tokens` strips as amount noise ("tablets" is a
    // count word for dishwasher tablets, "capsule"/"pod" for laundry).
    "tablet",
    "tablets",
    "capsule",
    "capsules",
    "pod",
    "pods",
];

static PACKAGING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:bag|bags|box|boxes|can|cans|tin|tins|jar|jars|bottle|bottles|pack|packs|packet|packets|carton|cartons|tub|tubs|roll|rolls|loaf|loaves|bunch|bunches|dozen|punnet|punnets|sachet|sachets|refill|refills)\b",
    )
    .expect("valid packaging regex")
});

/// Is this a REAL shopping item at all? (⇄ `shoppingMatch.plausibleGrocery`.)
///
/// Categorization answers "WHICH aisle"; this answers the prior question "is this a
/// thing you can buy?", so the caller can ASK instead of silently writing junk. It is
/// deliberately WIDER than [`categorize_aisle`]: three ways to be plausible — the aisle
/// taxonomy knows the noun, a generic-goods word names a real aisle-less purchase, or
/// the raw text carries a packaging/measure noun.
pub fn plausible_grocery(text: &str) -> bool {
    let raw = text.trim();
    if raw.is_empty() {
        return false;
    }
    if categorize_aisle(raw).is_some() {
        return true;
    }
    for tok in item_tokens(raw) {
        if GENERIC_GOODS.contains(&tok.as_str()) || GENERIC_GOODS.contains(&singular(&tok).as_str())
        {
            return true;
        }
    }
    PACKAGING_RE.is_match(raw)
}

// ---------------------------------------------------------------------------
// Clause + item tails (⇄ weekSource.firstClause / tidyItemTail)
// ---------------------------------------------------------------------------

static CLAUSE_BREAK_RE: LazyLock<Regex> = LazyLock::new(|| {
    // A clause boundary. Deliberately narrow: a sentence stop only counts when
    // followed by whitespace or end-of-string (so "1.5 l" survives), and a comma only
    // when it opens a fresh imperative ("milk, and add bread").
    Regex::new(
        r"(?i)\s*(?:—|–|;|!|\s-\s)\s*|\.(?:\s|$)|,\s+(?:and\s+|but\s+|then\s+)?(?:add|put|also|don't|dont|do\s+not|remove|take|but|then|so)\b",
    )
    .expect("valid clause-break regex")
});

/// The FIRST clause of a sentence (⇄ `weekSource.firstClause`). A SENTENCE is not one
/// item: "we are out of olive oil—add olive oil" must yield ONE item, not the
/// duplicated literal "olive oil—add olive oil" the live-cert caught.
pub fn first_clause(text: &str) -> String {
    let t = collapse(text);
    if t.is_empty() {
        return String::new();
    }
    match CLAUSE_BREAK_RE.find(&t) {
        Some(m) if m.start() > 0 => t[..m.start()].trim().to_string(),
        _ => t,
    }
}

/// Lowercase + single-space a turn, the shape every detector here expects.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

static LEAD_IMPERATIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // The optional `to ` covers the BUY-CONTEXT infinitive the gateway's add form spells
    // out ("we need to buy a gift" → "a gift"), so the verb never survives into the row.
    Regex::new(
        r"(?i)^(?:please\s+|also\s+|just\s+)*(?:to\s+)?(?:add|put|get|buy|grab|pick\s+up|order|restock)\s+",
    )
    .expect("valid lead-imperative regex")
});
static LEAD_QUANTIFIER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:some|a|an|the|more|of|any)\s+").expect("valid lead-quantifier regex")
});
static LIST_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+(?:to|on|onto|into)\s+(?:the\s+|my\s+|our\s+)?(?:shopping\s+|grocery\s+|groceries\s+)?list\b.*$",
    )
    .expect("valid list-tail regex")
});

/// Peel the husk off a captured item tail (⇄ `weekSource.tidyItemTail`): the list
/// clause, a clause boundary, closing punctuation, a leading article/quantifier, and a
/// leading imperative the split left ("add olive oil" → "olive oil").
pub fn tidy_item_tail(tail: &str) -> String {
    let no_list = LIST_TAIL_RE.replace(&collapse(tail), "").to_string();
    let cut = first_clause(&no_list);
    let cut = cut.trim_end_matches([' ', '.', '!', ',', ';', ':']).trim();
    let cut = LEAD_IMPERATIVE_RE.replace(cut, "");
    let cut = LEAD_QUANTIFIER_RE.replace(&cut, "");
    collapse(&cut).to_lowercase()
}

static VERB_PHRASE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^to\s+\w").expect("valid verb-phrase regex"));

/// A tail that is an INFINITIVE, not an item: "we need to talk about the milk" leaves
/// "to talk about the milk" behind. A buy-context infinitive is already peeled by
/// [`tidy_item_tail`] ("to buy a gift" → "gift"), so anything still opening with `to `
/// names an action the family wants, not a row for the list. Measured before this guard:
/// "we need to talk about the milk" wrote the literal "to talk about the milk" and
/// confirmed it with "Done — … 🛒".
pub fn is_verb_phrase(item: &str) -> bool {
    VERB_PHRASE_RE.is_match(item.trim())
}

static TRAILING_CLAUSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s(?:in|into|on|onto|at|for|from|with|about|by|near|under|over|inside|outside|around|before|after|while|when|because|until|so)\s+\S",
    )
    .expect("valid trailing-clause regex")
});

/// Does this item name carry a trailing prepositional/adverbial clause? Then the
/// sentence is about DOING something, not about a row on the list: "put the chicken in
/// the oven", "grab a bottle of wine on the way home", "we need to talk about the milk".
///
/// Only the UNSCOPED path consults this (see `fast_lane::shopping_turn`) — when the
/// family names the list, "add a gift for the party to the shopping list" is an
/// unmistakable ask and keeps working. With nothing naming the list, the honest answer
/// is to leave the turn to the composer rather than confirm a silent junk row. This is
/// the engine's compensation for its two add forms the gateway does not carry (a bare
/// "add X" and a bare "we need X"); the gateway avoids the same trap by requiring an
/// explicit buy context.
pub fn carries_trailing_clause(item: &str) -> bool {
    TRAILING_CLAUSE_RE.is_match(item.trim())
}

/// A tail that names no item — the ask said "it"/"that"/"some" and nothing more.
pub fn is_pronoun_item(item: &str) -> bool {
    matches!(
        item.trim().to_lowercase().as_str(),
        "it" | "this"
            | "that"
            | "them"
            | "those"
            | "these"
            | "the"
            | "a"
            | "an"
            | "some"
            | "more"
            | "one"
            | "something"
            | "anything"
            | "stuff"
    )
}

// ---------------------------------------------------------------------------
// Negation (⇄ weekSource.detectShoppingNegation)
// ---------------------------------------------------------------------------

/// What a negated shopping ask means. The family means two different things, and the
/// difference decides whether anything is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegationKind {
    /// "Don't add it yet — ask me first", "hold off on that", "not yet": the ask is
    /// real but the WRITE must wait for a yes. Ask; write nothing.
    Hold,
    /// "no baking soda needed after all", "we don't need the batteries anymore",
    /// "never mind the olive oil": the ask is WITHDRAWN. Suppress the add and take a
    /// matching item back off the list.
    Cancel,
}

/// A recognized negation around a shopping ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negation {
    pub kind: NegationKind,
    /// Best effort: a CANCEL usually names what to drop, a HOLD usually says "it".
    pub item: String,
}

static NEGATION_HOLD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        // "don't/do not/dont/never/… add|put|buy|get|order|write" plus the standalone
        // hold markers. The bare "not <verb>" alternative also catches the live-cert
        // corpus's mangled "Don not add it yet".
        r"(?i)\b(?:don't|do\s+not|dont|don\s+not|never|not)\s+(?:add|put|buy|get|order|write)\b|\bhold\s+off\b|\bnot\s+yet\b|\bask\s+me\s+first\b|\bcheck\s+with\s+me\s+first\b|\bwait\s+(?:on|before|until|for)\b|\bbefore\s+you\s+add\b",
    )
    .expect("valid negation-hold regex")
});

static NEGATION_CANCEL_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // "no baking soda needed after all" / "no more milk needed"
        Regex::new(r"(?i)\bno\s+(?:more\s+)?(.+?)\s+(?:needed|required|necessary)\b")
            .expect("valid cancel regex 1"),
        // "we don't need the batteries anymore" / "don't need olive oil after all"
        Regex::new(
            r"(?i)\b(?:we\s+)?(?:don't|do\s+not|dont)\s+need\s+(.+?)(?:\s+(?:any\s*more|anymore|after\s+all|now))?$",
        )
        .expect("valid cancel regex 2"),
        // "never mind the olive oil" / "forget the batteries" / "skip the oats"
        Regex::new(
            r"(?i)\b(?:never\s*mind|nevermind|forget|skip|cancel)\s+(?:about\s+)?(?:the\s+|that\s+|those\s+)?(.+)$",
        )
        .expect("valid cancel regex 3"),
    ]
});

/// Detect a negation around a shopping ask (⇄ `weekSource.detectShoppingNegation`).
/// CANCEL shapes are checked before HOLD, exactly as on the gateway.
pub fn detect_negation(text: &str) -> Option<Negation> {
    let raw = collapse(text);
    if raw.is_empty() {
        return None;
    }
    for re in NEGATION_CANCEL_RES.iter() {
        if let Some(caps) = re.captures(&raw) {
            let item = caps
                .get(1)
                .map(|m| tidy_item_tail(m.as_str()))
                .unwrap_or_default();
            let item = if item.is_empty() || is_pronoun_item(&item) {
                String::new()
            } else {
                item
            };
            return Some(Negation {
                kind: NegationKind::Cancel,
                item,
            });
        }
    }
    if NEGATION_HOLD_RE.is_match(&raw) {
        return Some(Negation {
            kind: NegationKind::Hold,
            item: String::new(),
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Removal intent (⇄ weekSource.detectShoppingRemoveIntent)
// ---------------------------------------------------------------------------

/// A recognized conversational REMOVAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveIntent {
    /// The named item; empty when the ask named none (see `pronoun`).
    pub item: String,
    /// The ask named no item — "remove it now". The caller resolves that against what
    /// it just added / just asked about, and never guesses.
    pub pronoun: bool,
}

static REMOVE_FORMS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // "remove/delete/drop X (from the list)(again)" — the report's dominant shape.
        Regex::new(
            r"(?i)\b(?:remove|delete|erase|drop|scratch|scrap)\s+(?:the\s+|that\s+|those\s+|my\s+|our\s+)?(.+)$",
        )
        .expect("valid remove regex 1"),
        // "take X off/back off (the list)" / "take off X"
        Regex::new(r"(?i)\btake\s+(?:the\s+|that\s+|those\s+)?(.+?)\s+(?:back\s+)?off\b")
            .expect("valid remove regex 2"),
        Regex::new(r"(?i)\btake\s+(?:back\s+)?off\s+(?:the\s+|that\s+)?(.+)$")
            .expect("valid remove regex 3"),
        // "X off the list" — a bare "paper towels off the list please"
        Regex::new(
            r"(?i)^(.+?)\s+off\s+(?:the\s+|my\s+|our\s+)?(?:shopping\s+|grocery\s+|groceries\s+)?list\b",
        )
        .expect("valid remove regex 4"),
    ]
});

static REMOVE_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+(?:from|off|out\s+of)\s+(?:the\s+|my\s+|our\s+)?(?:shopping\s+|grocery\s+|groceries\s+)?list\b.*$|\s+(?:again|now|please|entirely|completely|altogether|back|too|as\s+well|for\s+now|after\s+all)\b[\s.!]*$",
    )
    .expect("valid remove-tail regex")
});

static CROSS_OFF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:cross(?:ed)?|check(?:ed)?|tick(?:ed)?|mark(?:ed)?)\s+(?:\w+\s+){0,3}?off\b|\bcross\s+off\b|\bbought\b|\bgot\s+it\s+already\b",
    )
    .expect("valid cross-off regex")
});

static READ_QUESTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:did|do|does|have|has|can|could|would|will|is|are|was|were|what|which|why)\b")
        .expect("valid read-question regex")
});

/// Detect a conversational SHOPPING-REMOVE (⇄ `weekSource.detectShoppingRemoveIntent`).
///
/// CROSSING OFF IS NOT REMOVING: "cross the milk off" means BOUGHT (the row stays,
/// struck through). Those phrasings are deliberately not matched — they must never
/// silently delete a row the family still wants to see.
pub fn detect_remove_intent(text: &str) -> Option<RemoveIntent> {
    let raw = collapse(text);
    if raw.is_empty() {
        return None;
    }
    if CROSS_OFF_RE.is_match(&raw) {
        return None;
    }
    // A question about the list is a READ, not a write ("did you remove the milk?").
    if READ_QUESTION_RE.is_match(&raw) {
        return None;
    }
    for re in REMOVE_FORMS.iter() {
        let Some(caps) = re.captures(&raw) else {
            continue;
        };
        let Some(m) = caps.get(1) else { continue };
        // Cut trailing clauses FIRST ("remove it now; otherwise never mind" → "it now"),
        // so the qualifier peel below sees a real end-of-ask to work against.
        let mut tail = first_clause(m.as_str());
        loop {
            let peeled = REMOVE_TAIL_RE.replace(&tail, "").trim().to_string();
            if peeled == tail || peeled.is_empty() {
                tail = peeled;
                break;
            }
            tail = peeled;
        }
        let item = tidy_item_tail(&tail);
        if item.is_empty() || is_verb_phrase(&item) {
            continue;
        }
        if is_pronoun_item(&item) {
            return Some(RemoveIntent {
                item: String::new(),
                pronoun: true,
            });
        }
        return Some(RemoveIntent {
            item,
            pronoun: false,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Add extraction (⇄ weekSource.SHOPPING_ADD_FORMS / extractShoppingItem)
// ---------------------------------------------------------------------------

static ADD_FORMS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        // "add/put/get/buy/grab X (to|on) the (shopping) list"
        Regex::new(
            r"(?i)\b(?:add|put|get|buy|grab|pick\s+up|throw|stick|pop)\s+(.+?)\s+(?:to|on|onto|into)\s+(?:the\s+|my\s+|our\s+)?(?:shopping\s+|grocery\s+|groceries\s+)?list\b",
        )
        .expect("valid add regex 1"),
        // "add X to shopping" / "add X to groceries"
        Regex::new(
            r"(?i)\b(?:add|put|get|buy|grab|pick\s+up)\s+(.+?)\s+(?:to|on|onto|into)\s+(?:the\s+)?(?:shopping|grocery|groceries)\b",
        )
        .expect("valid add regex 2"),
        // "we are out of X" / "we ran out of X" — a supplies statement IS an add ask.
        Regex::new(r"(?i)\b(?:we(?:'re| are)?\s+|i(?:'m| am)?\s+)?(?:just\s+)?(?:ran\s+out\s+of|run\s+out\s+of|out\s+of)\s+(.+)$")
            .expect("valid add regex 3"),
        // "we are low on X" / "running low on X"
        Regex::new(r"(?i)\b(?:running\s+low\s+on|low\s+on|almost\s+out\s+of)\s+(.+)$")
            .expect("valid add regex 4"),
        // "we need X (on the list)" — the list clause is peeled by tidy_item_tail.
        Regex::new(r"(?i)\bwe\s+need\s+(?:some\s+|more\s+)?(.+)$").expect("valid add regex 5"),
        // "add X" / "put X on there" when the list is named elsewhere in the turn.
        Regex::new(r"(?i)\b(?:add|put|grab|buy|pick\s+up)\s+(.+)$").expect("valid add regex 6"),
    ]
});

/// The ITEM a shopping-ask names, with NO negation or plausibility judgement — the raw
/// extraction step (⇄ `weekSource.extractShoppingItem`). `unknown` marks an item the
/// plausibility check does not recognize.
pub fn extract_item(text: &str) -> Option<(String, bool)> {
    let raw = collapse(text);
    if raw.is_empty() {
        return None;
    }
    for re in ADD_FORMS.iter() {
        let Some(caps) = re.captures(&raw) else {
            continue;
        };
        let Some(m) = caps.get(1) else { continue };
        let item = tidy_item_tail(m.as_str());
        if item.is_empty() || is_pronoun_item(&item) || is_verb_phrase(&item) {
            continue;
        }
        return Some((item.clone(), !plausible_grocery(&item)));
    }
    None
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Does the turn NAME the shopping list? Only an explicitly list-scoped ask is
/// answered with a clarifying question — an unrecognized item in an unscoped sentence
/// is left to the composer rather than answered with an absurd "should I add
/// 'ideas' to the list?".
pub fn names_the_list(text: &str) -> bool {
    let s = text.to_lowercase();
    s.contains("shopping list")
        || s.contains("grocery list")
        || s.contains("groceries")
        || s.contains("shopping")
        || s.contains(" list")
        || s.starts_with("list ")
}

/// Head nouns that are plan-OR-list (⇄ `weekSource.DISH_AMBIGUOUS`): "remove the pasta"
/// with no list word could mean tonight's dinner just as easily as a row on the list.
/// An UNSCOPED turn about one of these is left to the composer / the meal ops, never
/// silently applied to the shopping list.
const DISH_AMBIGUOUS: &[&str] = &[
    "pasta",
    "pizza",
    "lasagna",
    "lasagne",
    "risotto",
    "curry",
    "soup",
    "salad",
    "stew",
    "taco",
    "tacos",
    "burrito",
    "burritos",
    "sandwich",
    "sandwiches",
    "omelette",
    "omelet",
    "casserole",
    "chili",
    "chilli",
    "ramen",
    "sushi",
    "paella",
    "gnocchi",
    "ravioli",
    "tortellini",
    "quiche",
    "frittata",
    "noodle",
    "noodles",
    "pie",
    "wrap",
    "wraps",
    "bowl",
    "roast",
    "chowder",
    "gumbo",
    "dinner",
    "lunch",
    "supper",
    "breakfast",
];

/// Is this item name plan-OR-list ambiguous? See [`DISH_AMBIGUOUS`].
pub fn dish_ambiguous(item: &str) -> bool {
    item_tokens(item).iter().any(|t| {
        DISH_AMBIGUOUS.contains(&t.as_str()) || DISH_AMBIGUOUS.contains(&singular(t).as_str())
    })
}

/// A supplies cue — "we are out of…", "we're low on…", "we need…". These make a turn
/// shopping-ish even when the list is never named.
pub fn names_supplies(text: &str) -> bool {
    let s = text.to_lowercase();
    s.contains("out of")
        || s.contains("low on")
        || s.contains("we need")
        || s.contains("need some")
        || s.contains("need more")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- plausibility (⇄ shoppingMatch.plausibleGrocery) ------------------

    #[test]
    fn nonsense_is_not_a_plausible_grocery() {
        assert!(!plausible_grocery("glorptwax"));
        assert!(!plausible_grocery("frimbulator"));
        assert!(!plausible_grocery(""));
    }

    #[test]
    fn real_goods_are_plausible_even_without_an_aisle() {
        // 1. the aisle taxonomy knows the noun
        assert!(plausible_grocery("olive oil"));
        assert!(plausible_grocery("baking soda"));
        assert!(plausible_grocery("milk"));
        // 2. a generic good with no aisle of its own
        assert!(plausible_grocery("freezer bags"));
        assert!(plausible_grocery("aa batteries"));
        assert!(plausible_grocery("dishwasher tablets"));
        assert!(plausible_grocery("birthday candles"));
        // 3. a packaging noun in the raw text
        assert!(plausible_grocery("a box of matches"));
    }

    #[test]
    fn plausibility_is_wider_than_the_aisle_taxonomy() {
        // The seam the gateway half documents: an unknown AISLE is a taxonomy gap,
        // not a nonsense word. If these ever start failing because plausibility was
        // narrowed to categorize_aisle, real goods would be questioned instead of
        // written.
        assert!(categorize_aisle("freezer bags").is_none());
        assert!(plausible_grocery("freezer bags"));
    }

    // ---- one sentence is not one item (⇄ weekSource.firstClause) ----------

    #[test]
    fn first_clause_cuts_at_a_clause_boundary() {
        assert_eq!(
            first_clause("we are out of olive oil—add olive oil"),
            "we are out of olive oil"
        );
        assert_eq!(
            first_clause("we are low on baking soda. don not add it yet"),
            "we are low on baking soda"
        );
        // A decimal is NOT a sentence stop.
        assert_eq!(first_clause("olive oil 1.5 l"), "olive oil 1.5 l");
    }

    #[test]
    fn the_olive_oil_sentence_yields_one_item() {
        let (item, unknown) = extract_item("we are out of olive oil—add olive oil")
            .expect("the out-of form is an add ask");
        assert_eq!(item, "olive oil");
        assert!(!unknown);
    }

    // ---- negation (⇄ weekSource.detectShoppingNegation) -------------------

    #[test]
    fn a_hold_is_recognized_including_the_corpus_typo() {
        // The live-cert corpus phrase, verbatim (C059) — "Don not" and all.
        let neg = detect_negation("we are low on baking soda. don not add it yet—ask me first")
            .expect("a hold");
        assert_eq!(neg.kind, NegationKind::Hold);
        for phrase in [
            "don't add olive oil to the shopping list yet",
            "hold off on the batteries",
            "not yet please",
            "ask me first before adding it",
        ] {
            assert_eq!(
                detect_negation(phrase).map(|n| n.kind),
                Some(NegationKind::Hold),
                "expected a hold for {phrase:?}"
            );
        }
    }

    #[test]
    fn a_cancel_names_what_to_drop() {
        let neg = detect_negation("no baking soda needed after all").expect("a cancel");
        assert_eq!(neg.kind, NegationKind::Cancel);
        assert_eq!(neg.item, "baking soda");

        let neg = detect_negation("we don't need the batteries anymore").expect("a cancel");
        assert_eq!(neg.kind, NegationKind::Cancel);
        assert_eq!(neg.item, "batteries");

        let neg = detect_negation("never mind the olive oil").expect("a cancel");
        assert_eq!(neg.kind, NegationKind::Cancel);
        assert_eq!(neg.item, "olive oil");
    }

    // ---- an action sentence is not a list row (task shopping-engine-half) ----
    // Found by probing `wg telegram shopping` over the finished lane: the engine
    // carries two add forms the gateway deliberately does not (a bare "add X" and a
    // bare "we need X"), and without these two guards each one wrote a junk row and
    // confirmed it with "Done — … 🛒".

    #[test]
    fn a_buy_context_infinitive_is_peeled_to_the_item() {
        let (item, unknown) = extract_item("we need to buy a gift for the party").expect("an add");
        assert_eq!(item, "gift for the party", "the verb must not survive");
        assert!(!unknown);
        let (item, _) = extract_item("we need to get milk").expect("an add");
        assert_eq!(item, "milk");
    }

    #[test]
    fn a_non_buy_infinitive_names_no_item() {
        // "to talk about the milk" is an ACTION, not a row. Measured before the guard:
        // it was written verbatim onto the family's shopping list.
        assert!(is_verb_phrase("to talk about the milk"));
        assert!(is_verb_phrase("to plan the week"));
        assert!(!is_verb_phrase("tomatoes"));
        assert_eq!(extract_item("we need to talk about the milk"), None);
    }

    #[test]
    fn a_trailing_clause_marks_an_action_sentence() {
        assert!(carries_trailing_clause("chicken in the oven"));
        assert!(carries_trailing_clause("bottle of wine on the way home"));
        assert!(carries_trailing_clause("gift for the party"));
        // A plain item — however it is spelled — carries none.
        assert!(!carries_trailing_clause("olive oil"));
        assert!(!carries_trailing_clause("aa batteries"));
        assert!(!carries_trailing_clause("bottle of wine"));
        assert!(!carries_trailing_clause("freezer bags"));
    }

    #[test]
    fn a_plain_add_is_not_negated() {
        assert_eq!(detect_negation("add milk to the shopping list"), None);
        assert_eq!(detect_negation("we are out of olive oil"), None);
    }

    // ---- removal (⇄ weekSource.detectShoppingRemoveIntent) ---------------

    #[test]
    fn the_report_removal_phrasings_are_recognized() {
        for (phrase, want) in [
            ("Remove AA batteries again.", "aa batteries"),
            ("Take dishwasher tablets back off.", "dishwasher tablets"),
            ("Now remove paper towels.", "paper towels"),
            ("remove olive oil from the list again", "olive oil"),
            ("scratch the olive oil off the list", "olive oil"),
            ("paper towels off the shopping list please", "paper towels"),
        ] {
            let got = detect_remove_intent(phrase)
                .unwrap_or_else(|| panic!("expected a removal for {phrase:?}"));
            assert_eq!(got.item, want, "item for {phrase:?}");
            assert!(!got.pronoun);
        }
    }

    #[test]
    fn crossing_off_is_never_a_removal() {
        for phrase in [
            "cross the milk off the list",
            "check off the eggs",
            "crossed off the batteries",
            "bought the olive oil",
        ] {
            assert_eq!(
                detect_remove_intent(phrase),
                None,
                "crossing off must not delete: {phrase:?}"
            );
        }
    }

    #[test]
    fn a_question_about_the_list_is_a_read() {
        assert_eq!(detect_remove_intent("did you remove the milk?"), None);
        assert_eq!(detect_remove_intent("can you remove the milk later?"), None);
    }

    #[test]
    fn a_pronoun_only_removal_names_no_item() {
        let got = detect_remove_intent("remove it now").expect("a pronoun removal");
        assert!(got.pronoun);
        assert_eq!(got.item, "");
    }

    // ---- scope -----------------------------------------------------------

    #[test]
    fn same_item_matches_a_plan_row_loosely_but_not_wrongly() {
        assert!(same_item("batteries", "AA batteries ×4"));
        assert!(same_item("green beans", "Green beans, 300 g (Tue)"));
        assert!(same_item("olive oil", "Olive oil, 1 bottle"));
        assert!(!same_item("green beans", "black beans, 2 cans"));
        assert!(!same_item("milk", "Salmon fillets ×2 (Tue)"));
    }

    #[test]
    fn a_dish_word_is_plan_or_list_ambiguous() {
        assert!(dish_ambiguous("pasta"));
        assert!(dish_ambiguous("the risotto"));
        assert!(!dish_ambiguous("aa batteries"));
        assert!(!dish_ambiguous("olive oil"));
    }

    #[test]
    fn scope_helpers_read_the_turn() {
        assert!(names_the_list("add milk to the shopping list"));
        assert!(names_the_list("add glorptwax to shopping"));
        assert!(!names_the_list("we are out of olive oil"));
        assert!(names_supplies("we are out of olive oil"));
        assert!(names_supplies("we are low on baking soda"));
        assert!(!names_supplies("hello there"));
    }
}
