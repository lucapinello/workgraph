#!/usr/bin/env bash
# Smoke: `wg telegram send` resolves a real token from a bots-map-only config.
#
# Regression (task `listener-reconnect`): when notify.toml has ONLY the
# multi-bot `[telegram.bots.*]` map and no legacy top-level `[telegram]`
# bot_token, `wg telegram send` used to build the channel from the empty
# top-level token — producing the URL `https://api.telegram.org/bot/sendMessage`
# and a bare 404 for every send. The fix falls back to the first configured bot.
#
# This drives the REAL binary via `wg telegram send --dry-run` (resolution-only,
# nothing sent), so it exercises the user-visible send path — not just the
# library resolver — and asserts the resolved API URL carries a real numeric bot
# id, never the tell-tale empty-token `bot:REDACTED` 404 shape.
#
# Credential-free: --dry-run resolves and prints; no live Telegram, no real
# tokens, nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# ── Case 1: bots-map-only (the regression config) ──────────────────────────
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.nora]
bot_token = "7654321:nora-dummy-token-body"
chat_id   = "-1000000000001"
username  = "nora_casapinello_bot"

[telegram.bots.bruno]
bot_token = "1234567:bruno-dummy-token-body"
chat_id   = "-1000000000002"
username  = "bruno_casapinello_bot"
TOML

out="$(cd "$scratch" && WG_DIR= wg telegram send "hello family" --dry-run 2>&1)" \
    || loud_fail "bots-map-only send --dry-run exited non-zero: $out"
echo "bots-map-only →"
echo "$out" | sed 's/^/    /'

# Resolution is deterministic (lexicographically-first bot id): "bruno" < "nora".
# The URL must carry bruno's real numeric id, NOT the empty-token 404 shape.
echo "$out" | grep -q "api url: https://api.telegram.org/bot1234567:REDACTED/sendMessage" \
    || loud_fail "expected resolved URL with real bot id 1234567 (bruno), got: $out"
# The empty-token 404 shape (`bot/sendMessage` or a token starting with `:`)
# must be absent — that was the pre-fix bug.
if echo "$out" | grep -Eq "bot/sendMessage|bot:REDACTED"; then
    loud_fail "empty-token 404 URL shape leaked — the bots-map-only bug is back: $out"
fi
# It resolved to the first bot (deterministically) and defaulted to its own chat.
echo "$out" | grep -q "would send via bot 'bruno'" \
    || loud_fail "expected deterministic resolution to bot 'bruno', got: $out"
echo "$out" | grep -q "target chat: -1000000000002" \
    || loud_fail "expected default to bruno's own chat, got: $out"

# ── Case 2: legacy top-level bot present → it wins, explicit chat honored ───
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram]
bot_token = "111222:legacy-dummy-token-body"
chat_id   = "500"

[telegram.bots.nora]
bot_token = "7654321:nora-dummy-token-body"
chat_id   = "-1000000000001"
username  = "nora_casapinello_bot"
TOML

out="$(cd "$scratch" && WG_DIR= wg telegram send "hi" --chat-id 777 --dry-run 2>&1)" \
    || loud_fail "legacy send --dry-run exited non-zero: $out"
echo "legacy-present →"
echo "$out" | sed 's/^/    /'
echo "$out" | grep -q "would send via bot 'default'" \
    || loud_fail "legacy top-level bot must win, got: $out"
echo "$out" | grep -q "api url: https://api.telegram.org/bot111222:REDACTED/sendMessage" \
    || loud_fail "expected legacy bot id 111222 in URL, got: $out"
echo "$out" | grep -q "target chat: 777" \
    || loud_fail "explicit --chat-id must override the default, got: $out"

# No token body ever leaks into --dry-run output (only the numeric id + REDACTED).
if echo "$out" | grep -q "dummy-token-body"; then
    loud_fail "bot token body leaked into --dry-run output: $out"
fi

echo "PASS: wg telegram send resolves a real token from bots-map-only config (no empty-token 404)"
