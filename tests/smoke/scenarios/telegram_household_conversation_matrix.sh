#!/usr/bin/env bash
# Smoke: several rounds of ordinary household conversation with an opaque roster.
#
# This scenario deliberately uses ids and display names that have never shipped as
# defaults. It drives the real engine CLI over scratch-only plans and bindings:
# routing decisions, grounded family commands, and session-backed replies. No
# network call is made and no live household state is read.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"
repo_root="$(cd "$scenario_dir/../../.." && pwd)"

wg_bin="${WG_BIN:-}"
if [[ -z "$wg_bin" ]]; then
    wg_bin="$(command -v wg || true)"
elif [[ "$wg_bin" != /* ]]; then
    wg_bin="$repo_root/$wg_bin"
fi
[[ -n "$wg_bin" && -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable locally built wg binary"

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg/agency/bindings" "$scratch/plans"

cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "prism-7"
name = "Cedar Keeper"
emoji = "🌿"
domains = ["meals", "nutrition"]

[[agent]]
id = "kiln-2"
name = "Copper Ladle"
emoji = "🥄"
domains = ["cooking", "recipes"]

[[agent]]
id = "trail-9"
name = "North Compass"
emoji = "🧭"
domains = ["workouts"]

[[agent]]
id = "relay-4"
name = "Open Door"
emoji = "🏡"
domains = ["calendar", "coordination", "shopping"]
TOML

cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.trail-9]
bot_token = "0000000000:trail-fixture-token"
chat_id = "-100700"
username = "north_compass_house_bot"
agent_id = "trail-9"

[telegram.bots.relay-4]
bot_token = "0000000000:relay-fixture-token"
chat_id = "-100700"
username = "open_door_house_bot"
agent_id = "relay-4"

[telegram.bots.prism-7]
bot_token = "0000000000:prism-fixture-token"
chat_id = "-100700"
username = "cedar_keeper_house_bot"
agent_id = "prism-7"

[telegram.bots.kiln-2]
bot_token = "0000000000:kiln-fixture-token"
chat_id = "-100700"
username = "copper_ladle_house_bot"
agent_id = "kiln-2"
TOML

cat >"$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
- telegram_user: "member-41"
  agent_id: "human-41"
  name: "River Guest"
  bot_id: "relay-4"
  confirmed: true
  created_at: "2026-07-20T00:00:00Z"
  confirmed_at: "2026-07-20T00:00:00Z"
YAML

# The heading mirrors the live plan shape: numbered `Dinners` plus an authored
# parenthetical, rather than the prettier `Meals` shortcut old fixtures used.
cat >"$scratch/plans/2026-W30-family-plan.md" <<'MARKDOWN'
# Household weekly plan · 2026-W30

**Week of Monday 2026-07-20 → Sunday 2026-07-26**

## 1. Dinners (Cedar Keeper → Copper Ladle)

| Day | Slot type | Dinner | Prep | Note |
|-----|-----------|--------|------|------|
| Mon 07-20 | Vegetarian | Chickpea lemon bowls | ~25 min | quick |
| Tue 07-21 | Fish | Baked trout and potatoes | ~35 min | tray bake |
| Wed 07-22 | Vegetarian | Miso aubergine noodles | ~30 min | pantry |
| Thu 07-23 | Fish | Salmon rice bowls | ~30 min | leftovers |
| Fri 07-24 | Flex | Tomato bean soup | ~25 min | freezer |
| Sat 07-25 | Vegetarian | Mushroom tacos | ~30 min | family |
| Sun 07-26 | Leftovers | Clear-the-fridge plates | ~15 min | flex |

## 2. Workouts (North Compass)

### River Guest — gentle movement

| Day | Session | Structure |
|-----|---------|-----------|
| Wed 07-22 | Easy walk | 30 minutes at a conversational pace |

## 3. Calendar (Open Door)

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Wed 07-22 | 18:30 | Cook: miso aubergine noodles | Copper Ladle |

## 4. Shopping list (Open Door)

### Produce
- Aubergines ×2
- Lemons ×3

### Pantry
- White miso
- Noodles

## 5. Human confirmations required

Nothing pending.
MARKDOWN

run_wg() {
    (cd "$scratch" && WG_DIR= "$wg_bin" "$@")
}

expect_has() {
    local label="$1" output="$2" needle="$3"
    if ! grep -qF -- "$needle" <<<"$output"; then
        loud_fail "$label — expected '$needle', got:
$output"
    fi
    echo "  ok: $label"
}

expect_lacks() {
    local label="$1" output="$2" needle="$3"
    if grep -qF -- "$needle" <<<"$output"; then
        loud_fail "$label — unexpected '$needle' in:
$output"
    fi
    echo "  ok: $label"
}

echo "Round 1 — ordinary group routing:"
expect_has "display-name address" \
    "$(run_wg telegram elect "Copper Ladle, can you suggest a sauce?")" \
    "answered by kiln-2 (by name)"
expect_has "configured handle address" \
    "$(run_wg telegram elect "@north_compass_house_bot can we walk tomorrow?")" \
    "answered by trail-9 (by @mention)"
expect_has "reply-chain follow-up" \
    "$(run_wg telegram elect "yes, that works" --reply-to-bot cedar_keeper_house_bot)" \
    "answered by prism-7 (by reply-chain)"
expect_has "meal-plan owner" \
    "$(run_wg telegram elect "what is the dinner plan tonight?")" \
    "answered by prism-7 (by domain:meal-planning)"
expect_has "recipe owner" \
    "$(run_wg telegram elect "how do I cook the aubergines?")" \
    "answered by kiln-2 (by domain:cooking)"
expect_has "workout owner" \
    "$(run_wg telegram elect "can you plan a gentle workout tomorrow?")" \
    "answered by trail-9 (by domain:workouts)"
expect_has "unmatched household coordination" \
    "$(run_wg telegram elect "can someone take care of this?")" \
    "answered by relay-4 (by concierge)"
expect_has "two-person chatter stays private" \
    "$(run_wg telegram elect "that made me laugh" --humans 2)" \
    "silence (small-talk)"
expect_has "one-person greeting reaches the roster" \
    "$(run_wg telegram elect "hello" --humans 1)" \
    "prism-7, kiln-2, trail-9, relay-4"
expect_has "private chat is a passthrough" \
    "$(run_wg telegram elect "hello everyone" --chat-type private)" \
    "1:1 passthrough"

echo "Round 2 — grounded reads from the live-shaped plan:"
dinner="$(run_wg telegram command dinner --today 2026-07-22)"
expect_has "dinner reads the numbered Dinners section" "$dinner" "Miso aubergine noodles"
expect_has "dinner includes grounded prep" "$dinner" "30 min"
expect_lacks "dinner does not invent an empty-plan fallback" "$dinner" "don't have"

week="$(run_wg telegram command week --today 2026-07-22)"
expect_has "week reads dinner rows" "$week" "Miso aubergine noodles"
expect_has "week reads workout rows" "$week" "Workouts:"

shopping="$(run_wg telegram command shopping --today 2026-07-22)"
expect_has "shopping reads a real section" "$shopping" "Produce"
expect_has "shopping reads a real item" "$shopping" "Aubergines"
expect_lacks "shopping strips markdown" "$shopping" "**"

expect_has "dinner owner comes from domains" \
    "$(run_wg --json telegram command dinner --today 2026-07-22)" \
    '"owner": "kiln-2"'
expect_has "shopping owner comes from domains" \
    "$(run_wg --json telegram command shopping --today 2026-07-22)" \
    '"owner": "relay-4"'

echo "Round 3 — private and group session delivery:"
unknown="$(run_wg --json telegram conversation \
    --channel telegram:relay-4 --chat 441 --sender visitor-8 --message "hello")"
expect_has "unknown member gets generic recovery" "$unknown" \
    "Ask River Guest to add you to the family"
for persona_name in "Cedar Keeper" "Copper Ladle" "North Compass" "Open Door"; do
    expect_lacks "unknown recovery does not depend on an agent display name" \
        "$unknown" "$persona_name"
done

private="$(run_wg --json telegram conversation \
    --channel telegram:relay-4 --chat 441 --sender member-41 \
    --message "what time is dinner?" --session-reply "Dinner starts at half past six.")"
expect_has "private reply uses the messaged opaque bot" "$private" '"bot": "relay-4"'
expect_has "private reply stays in the private chat" "$private" '"chat": "441"'
expect_has "private session text relays exactly" "$private" "Dinner starts at half past six."

group="$(run_wg --json telegram conversation \
    --channel telegram:kiln-2 --chat=-100700 --sender member-41 --group \
    --message "what sauce works?" --session-reply "Miso, ginger and a little lime.")"
expect_has "group reply uses the elected opaque bot" "$group" '"bot": "kiln-2"'
expect_has "group reply stays in the family group" "$group" '"chat": "-100700"'
expect_has "group session text relays exactly" "$group" "Miso, ginger and a little lime."

all_output="$dinner
$week
$shopping
$unknown
$private
$group"
expect_lacks "no fixture token leaks" "$all_output" "fixture-token"

echo "PASS: three opaque-roster household conversation rounds (routing, grounded reads, private/group delivery)"
