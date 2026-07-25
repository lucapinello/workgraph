#!/usr/bin/env bash
# Smoke: `/standup` posts exactly one message per named voice, in roster order.
#
# Pins the group-standup contract (docs/09 §3): a `/standup` in the family group
# must produce exactly one post per joined household persona, in the authored
# `[[agent]]` order, no duplicates, each in family voice. We exercise the real command path
# (`wg telegram standup --dry-run`, the on-demand equivalent of the listener's
# `/standup` intercept) against opaque fixture ids whose bots are deliberately
# stored OUT of roster order, and assert the emitted posts come back in household
# order with household-authored names/emoji. Dry-run prints instead of hitting Telegram,
# so the scenario needs no network or real tokens.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# The authored order is deliberately neither alphabetical nor bot-map order.
cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "agent-zeta"
name = "North Star"
emoji = "🌙"

[[agent]]
id = "agent-alpha"
name = "Garden Lantern"
emoji = "🏮"

[[agent]]
id = "agent-kappa"
name = "Quiet Harbor"
emoji = "🧭"
TOML

# Dummy bot secrets, inserted in a different order. The join key is the opaque
# agent id; neither names nor emoji are derived from these keys.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.agent-kappa]
bot_token = "0000000000:kappa-dummy-token"
chat_id   = "-1000000000001"

[telegram.bots.agent-zeta]
bot_token = "0000000000:zeta-dummy-token"
chat_id   = "-1000000000001"

[telegram.bots.agent-alpha]
bot_token = "0000000000:alpha-dummy-token"
chat_id   = "-1000000000001"
TOML

out="$(cd "$scratch" && wg telegram standup --dry-run 2>&1)" || {
    echo "$out" 1>&2
    loud_fail "wg telegram standup --dry-run exited non-zero"
}

echo "$out"

# Exactly three joined household voices.
count="$(echo "$out" | grep -c '^--- \[' || true)"
[ "$count" -eq 3 ] || loud_fail "expected 3 posts, got $count"

# Authored household order, not alphabetical or HashMap iteration order.
order="$(echo "$out" | sed -n 's/^--- \[[0-9]*\] \([^ ]*\) .*/\1/p' | tr '\n' ',' )"
[ "$order" = "agent-zeta,agent-alpha,agent-kappa," ] || loud_fail "roster order wrong: got '$order'"

# No token ever leaks into the printed plan.
if echo "$out" | grep -q "dummy-token"; then
    loud_fail "bot token leaked into standup output"
fi

# Each household-authored name/emoji is present; no presentation was inferred
# from the opaque ids.
echo "$out" | grep -q "North Star 🌙"     || loud_fail "North Star header missing"
echo "$out" | grep -q "Garden Lantern 🏮" || loud_fail "Garden Lantern header missing"
echo "$out" | grep -q "Quiet Harbor 🧭"   || loud_fail "Quiet Harbor header missing"

# A multi-bot config with no authored roster must fail closed. It must never
# recover by sorting or iterating the bot map.
missing="$(make_scratch)"
mkdir -p "$missing/.wg"
cp "$scratch/.wg/notify.toml" "$missing/.wg/notify.toml"
if missing_out="$(cd "$missing" && wg telegram standup --dry-run 2>&1)"; then
    loud_fail "multi-bot standup succeeded without household.toml: $missing_out"
fi
echo "$missing_out" | grep -q "failed to read household roster" \
    || loud_fail "missing-roster failure was not explicit: $missing_out"

# A malformed roster fails in the same direction.
malformed="$(make_scratch)"
mkdir -p "$malformed/.wg"
cp "$scratch/.wg/notify.toml" "$malformed/.wg/notify.toml"
printf 'not = [valid\n' >"$malformed/household.toml"
if malformed_out="$(cd "$malformed" && wg telegram standup --dry-run 2>&1)"; then
    loud_fail "multi-bot standup succeeded with malformed household.toml: $malformed_out"
fi
echo "$malformed_out" | grep -q "invalid household roster" \
    || loud_fail "malformed-roster failure was not explicit: $malformed_out"

echo "PASS: /standup used ordered household identities with opaque agent ids"
