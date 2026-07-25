#!/usr/bin/env bash
# Smoke: the family command set (/dinner /shopping /week /reminders /standup /help).
#
# Pins the command contract from docs/09 §commands: each command is one row of
# the shared table (keyword · owner · data source · compose fn), and its reply is
# GROUNDED in the real week plan (`plans/*.md`) — never a placeholder — in the
# owning voice, family-voice (docs/04). Drives the REAL binary via
# `wg telegram command <name> --today <date>` (the dry-run composer the live
# listener also uses), so it exercises the user-visible content without a live
# group or real tokens. Credential-free: nothing is sent.
#
# What it locks:
#   - /dinner   → Bruno, tonight's actual dish from the plan covering `today`,
#                 with an honest fallback when no plan covers today.
#   - /shopping → Otto, the current week's shopping list under its store
#                 sections, phone-friendly bullets, no leaked markdown.
#   - /week     → Otto, meals as WEEKDAY NAMES (not raw dates) + a workouts line.
#   - /reminders→ Otto, cheerful "all caught up" on an empty graph.
#   - /help     → lists EVERY command in the table with its description.
#   - table     → each command reports the expected owner in --json.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg" "$scratch/plans"

# Fixture week plan (the real 2026-W29 Casa Pinello plan): Mon 07-13 → Sun 07-19.
cp "$scenario_dir/../../fixtures/family_plan_w29.md" \
    "$scratch/plans/2026-W29-family-plan.md"

cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "nora"
name = "Nora"
emoji = "🥗"
domains = ["meals", "nutrition"]
[[agent]]
id = "bruno"
name = "Bruno"
emoji = "🍳"
domains = ["meals", "cooking", "recipes"]
[[agent]]
id = "mira"
name = "Coach Mira"
emoji = "💪"
domains = ["workouts"]
[[agent]]
id = "otto"
name = "Otto"
emoji = "📋"
domains = ["calendar", "coordination", "shopping"]
TOML

# Matching bots for /standup; order/presentation comes from household.toml.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"
username  = "nora_casapinello_bot"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
username  = "bruno_casapinello_bot"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"
username  = "mira_casapinello_bot"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"
TOML

# cmd <name> [extra args...] → the composed reply from the real binary.
cmd() {
    local name="$1"
    shift
    (cd "$scratch" && WG_DIR= wg telegram command "$name" "$@" 2>&1)
}

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    if ! grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — expected to find: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

expect_absent() {
    local desc="$1" out="$2" needle="$3"
    if grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — did NOT expect: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

echo "/dinner → Bruno, tonight's real dish (Wed 07-15 = lentil & beet salad):"
out="$(cmd dinner --today 2026-07-15)"
expect_grep "dinner grounded" "$out" "Lentil & roasted-beet salad"
expect_grep "dinner prep time" "$out" "30 min"
expect_grep "dinner swap tail" "$out" "swap"

echo "/dinner → honest when no plan covers today (07-12 is the day before W29):"
out="$(cmd dinner --today 2026-07-12)"
expect_grep "dinner honest fallback" "$out" "don't have"

echo "/shopping → Otto, real items under store sections, no markdown leak:"
out="$(cmd shopping --today 2026-07-15)"
expect_grep "shopping store section" "$out" "Fishmonger"
expect_grep "shopping grounded item" "$out" "Salmon fillets"
expect_grep "shopping bullets" "$out" "•"
expect_absent "shopping strips markdown bold" "$out" "**"

echo "/week → Otto, meals as weekday names + workouts, no raw dates:"
out="$(cmd week --today 2026-07-15)"
expect_grep "week weekday name" "$out" "Monday"
expect_grep "week meal grounded" "$out" "Chickpea & spinach curry"
expect_grep "week workouts line" "$out" "Workouts:"
expect_absent "week hides raw dates" "$out" "07-13"

echo "/reminders → Otto, cheerful when the graph has nothing pending:"
out="$(cmd reminders --today 2026-07-15)"
expect_grep "reminders cheerful empty" "$out" "caught up"

echo "/help → lists every command in the table:"
out="$(cmd help --today 2026-07-15)"
for kw in /dinner /shopping /week /reminders /standup /help; do
    expect_grep "help lists $kw" "$out" "$kw"
done

echo "table → each command reports the expected owner (--json):"
expect_grep "dinner owner" "$(cd "$scratch" && WG_DIR= wg --json telegram command dinner --today 2026-07-15)" '"owner": "bruno"'
expect_grep "shopping owner" "$(cd "$scratch" && WG_DIR= wg --json telegram command shopping --today 2026-07-15)" '"owner": "otto"'
expect_grep "standup kind" "$(cd "$scratch" && WG_DIR= wg --json telegram command standup --today 2026-07-15)" '"kind": "Roster"'

# No token ever leaks into any composed reply.
if cmd shopping --today 2026-07-15 | grep -q "dummy-token"; then
    loud_fail "bot token leaked into a command reply"
fi

echo "PASS: family command set (dinner/shopping/week/reminders/help grounded in the plan; owners + help table pinned)"
