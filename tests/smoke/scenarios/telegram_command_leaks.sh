#!/usr/bin/env bash
# Smoke: punctuation is not a command; a mention owns its message; the operator
# WG reference never leaks into a family chat (fix-command-leaks).
#
# THE LIVE FAILURE (Luca's screenshot + listener log, 02:28:15): he sent
# "@nora_casapinello_bot ?" in the group. The bare "?" was parsed as a HELP
# command and Otto dumped the RAW WG command reference (claim/done/fail/ready/
# status — coordinator content) into the family chat, OVERRIDING the mention
# election that had already targeted nora (log: rule=mention target=nora AND the
# command firing).
#
# This drives the REAL binary through `wg telegram decide` — the same
# `decode_update` (which reads the Telegram entities) + `command_gate` +
# `elect_responders` decision the `wg telegram listen` listener makes on an
# inbound update — against crafted `getUpdates` elements. It proves:
#   1. "@nora ?"  → conversation, elected=nora, ZERO commands (the reported bug).
#   2. bare "?"   → conversation, ZERO commands (punctuation is never a command).
#   3. bareword "help" → conversation, ZERO commands (no non-slash operator trigger).
#   4. "/help"    → the FAMILY command (family-voiced), never the operator ref.
#   5. "@otto /shopping" → conversation (the mention election owns it; a command
#      after a mention is not a leading slash command).
# Credential-free: `decide` is a pure decision — no live group, nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# The Casa Pinello voices, shaped like the LIVE config (agent_id + username).
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

CHAT='"chat":{"id":-1000000000001,"type":"supergroup"}'
FROM='"from":{"id":8905220378,"username":"luca"}'

# Write a raw getUpdates element to $scratch/<name>.json.
mk_update() {
    local name="$1" text="$2" entities="$3"
    local ent=""
    [ -n "$entities" ] && ent=",\"entities\":$entities"
    cat >"$scratch/$name.json" <<JSON
{"update_id":1,"message":{"message_id":10,$FROM,$CHAT,"text":$text$ent}}
JSON
}

# decide <name> → the one-line decision from the real binary.
decide() {
    (cd "$scratch" && WG_DIR= wg telegram decide "@$scratch/$1.json" 2>&1)
}

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    echo "  $desc → $out"
    echo "$out" | grep -q "$needle" \
        || loud_fail "$desc: expected '$needle', got: $out"
}

expect_not_grep() {
    local desc="$1" out="$2" needle="$3"
    echo "$out" | grep -q "$needle" \
        && loud_fail "$desc: did NOT expect '$needle', got: $out" || true
}

# 1. THE REPORTED BUG: "@nora_casapinello_bot ?" — a mention, NO bot_command.
mk_update mention_q '"@nora_casapinello_bot ?"' '[{"type":"mention","offset":0,"length":21}]'
out="$(decide mention_q)"
expect_grep "mention+? → converses, no command" "$out" "conversation"
expect_grep "mention+? → elected nora" "$out" "elected=nora"
expect_grep "mention+? → not a command" "$out" "has_bot_command=false"
expect_not_grep "mention+? → NO family command" "$out" "family_command"
expect_not_grep "mention+? → NO operator command" "$out" "operator_command"

# 2. A bare "?" alone — no entities at all. Punctuation is never a command.
mk_update bare_q '"?"' ''
out="$(decide bare_q)"
expect_grep "bare ? → conversation" "$out" "conversation"
expect_grep "bare ? → not a command" "$out" "has_bot_command=false"

# 3. The bareword "help" — the OLD non-slash operator trigger. Must be killed.
mk_update help_word '"help"' ''
out="$(decide help_word)"
expect_grep "bareword help → conversation" "$out" "conversation"
expect_not_grep "bareword help → NOT operator_command" "$out" "operator_command"
expect_not_grep "bareword help → NOT family_command" "$out" "family_command"

# 4. A genuine "/help" — bot_command entity at offset 0 → the FAMILY command.
mk_update slash_help '"/help"' '[{"type":"bot_command","offset":0,"length":5}]'
out="$(decide slash_help)"
expect_grep "/help → family command" "$out" "family_command"
expect_grep "/help → the family /help" "$out" "/help"
expect_not_grep "/help → NOT the operator reference" "$out" "operator_command"

# 5. "@otto /shopping" — a slash command AFTER a mention. By Fix (2) the mention
#    election owns the message (the command is not a leading slash command), so
#    otto converses rather than the command racing the election.
mk_update mention_cmd '"@otto_casapinello_bot /shopping"' \
    '[{"type":"mention","offset":0,"length":21},{"type":"bot_command","offset":22,"length":9}]'
out="$(decide mention_cmd)"
expect_grep "@otto /shopping → conversation (mention owns)" "$out" "conversation"
expect_grep "@otto /shopping → elected otto" "$out" "elected=otto"

# 6. No bot token ever leaks into a decision line.
if decide mention_q | grep -q "dummy-token"; then
    loud_fail "bot token leaked into decide output"
fi

echo "PASS: punctuation-is-not-a-command / mention-owns-message / operator-reference-never-in-family-chat (fix-command-leaks)"
