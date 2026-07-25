#!/usr/bin/env bash
# Smoke: each family bot's `agent_id` roster binding is read from notify.toml.
#
# Regression (task `dedupe-key-fix`): the group election logged
# `target=otto(unbound)` because the live `.wg/notify.toml` set no per-bot
# `agent_id`, so `TelegramBotConfig.agent_id` defaulted to None and the election
# decision summary rendered every bot as `<bot_id>(unbound)`. Routing itself
# still worked (dispatch falls back to bot_id), so the symptom was a misleading
# "unbound" diagnostic — but the intended, explicit roster binding was simply
# never present. The per-persona chat SESSION bindings (`wg agent session`) were
# intact the whole time; this bot→agent roster surface is separate.
#
# This drives the REAL binary via `wg telegram list-bots` (the same config load
# the listener performs), asserting that:
#   * with `agent_id` set, every bot reports its bound agent (never "no agent
#     binding"), and the election resolves the bound voice; and
#   * with `agent_id` absent, the bot is honestly reported as shared/unbound —
#     so a future regression that silently stops reading `agent_id` is caught.
#
# Credential-free: config load + election are pure decisions; nothing is sent,
# no live group or real tokens are needed.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

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

# ── Case 1: agent_id set for all four bots (the fixed config) ───────────────
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"
username  = "nora_casapinello_bot"
agent_id  = "nora"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
username  = "bruno_casapinello_bot"
agent_id  = "bruno"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"
username  = "mira_casapinello_bot"
agent_id  = "mira"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"
agent_id  = "otto"
TOML

bound_out="$(cd "$scratch" && WG_DIR= wg telegram list-bots 2>&1)" \
    || loud_fail "list-bots (bound) exited non-zero: $bound_out"
echo "bound config →"
echo "$bound_out" | sed 's/^/    /'

for name in nora bruno mira otto; do
    echo "$bound_out" | grep -Eq "agent id:[[:space:]]+$name\b" \
        || loud_fail "bot '$name' should report a bound agent id, got: $bound_out"
done
# The tell-tale pre-fix rendering must NOT appear for any bot.
if echo "$bound_out" | grep -q "no agent binding"; then
    loud_fail "a bot rendered as unbound despite agent_id being set — the otto(unbound) regression is back: $bound_out"
fi

# The election resolves the bound coordinator for an unmatched team ask.
elect_out="$(cd "$scratch" && WG_DIR= wg telegram elect "can someone handle this?" --chat-type supergroup 2>&1)" \
    || loud_fail "elect exited non-zero: $elect_out"
echo "$elect_out" | grep -q "answered by otto" \
    || loud_fail "expected the configured coordinator to answer the unmatched team ask, got: $elect_out"

# ── Case 2: agent_id absent → bot is honestly reported as shared/unbound ────
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"
TOML

unbound_out="$(cd "$scratch" && WG_DIR= wg telegram list-bots 2>&1)" \
    || loud_fail "list-bots (unbound) exited non-zero: $unbound_out"
echo "unbound config →"
echo "$unbound_out" | sed 's/^/    /'
echo "$unbound_out" | grep -q "no agent binding" \
    || loud_fail "a bot with no agent_id should be reported as shared/unbound, got: $unbound_out"

# No token body ever leaks into list-bots output.
if echo "$bound_out $unbound_out" | grep -q "dummy-token"; then
    loud_fail "bot token body leaked into list-bots output"
fi

echo "PASS: per-bot agent_id roster binding is read from notify.toml (no false unbound, honest when absent)"
