#!/usr/bin/env bash
# Smoke: the conversational reply path — 1:1 AND name-addressed group messages.
#
# Pins the fix for the user-visible gap Luca hit live TWICE: a plain message
# from a CONFIRMED human that routes to a SINGLE agent — whether DMed 1:1 or
# name/mention/reply-elected in the GROUP — used to fall through to the silent
# `Unmatched` arm (only collective addresses and /commands had reply composers).
# Now both entry points share ONE conversational composer that runs a chat turn
# against the agent's persistent session and replies WHERE ASKED.
#
# Drives the REAL binary via `wg telegram conversation` — the dry-run of the
# exact composer the live listener invokes on the `Unmatched` arm. With
# `--session-reply` it exercises the FULL persistent-session round-trip against
# an ephemeral fixture session (create → bind → inbox → outbox → relay), proving
# the human's message reaches a real session and the session's reply lands in the
# correct chat via the correct bot. Credential-free: a recording sink captures
# the sends instead of hitting Telegram, so no live group or real tokens needed.
#
# What it locks:
#   - unknown sender          → ONE polite onboarding line, nothing more (no chat)
#   - confirmed 1:1           → round-trips through the session, replies in the
#                               1:1 chat via the MESSAGED bot (otto)
#   - confirmed group-elected → round-trips through the session, replies IN THE
#                               GROUP via the ELECTED bot (bruno)
#   - family voice            → onboarding line is warm & jargon-free (no ids)
#   - no token leak           → the bot token never appears in any reply/output
#
# Also pinned by the notify::telegram_conversation unit/tokio tests (plan,
# round-trip, ack-on-slow, timeout, onboard) and the commands::telegram
# precedence tests (awaiting-human task routing still wins over conversation).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg/agency/bindings"

# The two Casa Pinello voices this scenario addresses (otto = concierge/1:1,
# bruno = an elected group voice). Dummy tokens: nothing is ever sent.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
username  = "bruno_casapinello_bot"
TOML

# A CONFIRMED human binding so `luca-1` is conversation-eligible (an unconfirmed
# or absent binding gets the onboarding line instead).
cat >"$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
- telegram_user: "luca-1"
  agent_id: "human-luca"
  name: "Luca"
  bot_id: "otto"
  confirmed: true
  created_at: "2026-07-11T00:00:00Z"
  confirmed_at: "2026-07-11T00:00:00Z"
YAML

# convo <flags...> → the composer's --json decision + captured sends.
convo() {
    (cd "$scratch" && WG_DIR= wg --json telegram conversation "$@" 2>&1)
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

echo "unknown sender → one polite onboarding line, nothing more:"
out="$(convo --channel telegram:otto --chat 555 --sender stranger-9 --message 'hi there')"
expect_grep "onboard kind"       "$out" '"kind": "onboard"'
expect_grep "onboard entry 1:1"  "$out" '"entry": "1:1"'
expect_grep "onboard via otto"   "$out" '"bot": "otto"'
expect_grep "onboard in 1:1 chat" "$out" '"chat": "555"'
expect_grep "onboard family voice" "$out" "don't recognise you yet"
expect_absent "onboard has no jargon (task ids)" "$out" "task"

echo "confirmed 1:1 → round-trips the session, replies in the 1:1 via the messaged bot:"
out="$(convo --channel telegram:otto --chat 555 --sender luca-1 --message 'are we on for dinner?' --session-reply 'Dinner is at seven!')"
expect_grep "1:1 converse kind"  "$out" '"kind": "converse"'
expect_grep "1:1 entry"          "$out" '"entry": "1:1"'
expect_grep "1:1 replied"        "$out" '"outcome": "replied"'
expect_grep "1:1 reply via otto" "$out" '"bot": "otto"'
expect_grep "1:1 reply in chat 555" "$out" '"chat": "555"'
expect_grep "1:1 session reply relayed" "$out" "Dinner is at seven!"

echo "confirmed group-elected → round-trips the session, replies IN THE GROUP via the elected bot:"
out="$(convo --channel telegram:bruno --chat=-100777 --sender luca-1 --message 'bruno what is for dinner?' --group --session-reply 'Chickpea curry tonight.')"
expect_grep "group converse kind"   "$out" '"kind": "converse"'
expect_grep "group entry"           "$out" '"entry": "group-elected"'
expect_grep "group replied"         "$out" '"outcome": "replied"'
expect_grep "group reply via bruno" "$out" '"bot": "bruno"'
expect_grep "group reply in the group chat" "$out" '"chat": "-100777"'
expect_grep "group session reply relayed" "$out" "Chickpea curry tonight."

echo "no bot token ever leaks into any reply or decision output:"
for out in \
    "$(convo --channel telegram:otto --chat 555 --sender stranger-9 --message 'hi')" \
    "$(convo --channel telegram:otto --chat 555 --sender luca-1 --message 'hi' --session-reply 'hello')" ; do
    expect_absent "no token leak" "$out" "dummy-token"
done

echo "PASS: conversational replies — 1:1 + group-name-addressed round-trip through a real session and reply where asked; unknown sender onboarded; no token leak"
