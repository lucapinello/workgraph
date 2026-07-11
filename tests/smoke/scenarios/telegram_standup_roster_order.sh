#!/usr/bin/env bash
# Smoke: `/standup` posts exactly one message per named voice, in roster order.
#
# Pins the group-standup contract (docs/09 §3): a `/standup` in the family group
# must produce EXACTLY four posts — nora, bruno, mira, otto — in that fixed
# order, no duplicates, each in family voice. We exercise the real command path
# (`wg telegram standup --dry-run`, the on-demand equivalent of the listener's
# `/standup` intercept) against a fixture config whose bots are deliberately
# stored OUT of roster order, and assert the emitted posts come back in canonical
# roster order regardless. Dry-run prints the plan instead of hitting Telegram,
# so the scenario needs no network or real tokens.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# Fixture: four named bots (dummy tokens), inserted out of roster order to prove
# the ordering is imposed by the command, not by config/HashMap iteration.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"

[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
TOML

out="$(cd "$scratch" && wg telegram standup --dry-run 2>&1)" || {
    echo "$out" 1>&2
    loud_fail "wg telegram standup --dry-run exited non-zero"
}

echo "$out"

# Exactly four posts.
count="$(echo "$out" | grep -c '^--- \[' || true)"
[ "$count" -eq 4 ] || loud_fail "expected 4 posts, got $count"

# Roster order: nora, bruno, mira, otto — extracted from the per-post headers.
order="$(echo "$out" | sed -n 's/^--- \[[0-9]*\] \([a-z]*\) .*/\1/p' | tr '\n' ',' )"
[ "$order" = "nora,bruno,mira,otto," ] || loud_fail "roster order wrong: got '$order'"

# No token ever leaks into the printed plan.
if echo "$out" | grep -q "dummy-token"; then
    loud_fail "bot token leaked into standup output"
fi

# Each voice's header emoji/name is present (family voice, not bot ids alone).
echo "$out" | grep -q "Nora 🥗"       || loud_fail "Nora header missing"
echo "$out" | grep -q "Coach Mira 💪" || loud_fail "Coach Mira header missing"
echo "$out" | grep -q "Otto 📋"       || loud_fail "Otto header missing"

echo "PASS: /standup produced 4 posts in roster order (nora, bruno, mira, otto)"
