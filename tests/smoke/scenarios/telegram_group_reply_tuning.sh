#!/usr/bin/env bash
# Smoke: group-reply-tuning — sender resolution + bot-loop guard + classifier.
#
# Pins the live failures Luca hit in the family group (task group-reply-tuning):
#
#   Fix #5 (LEAD): the listener resolved every sender to "unknown" because it
#     only read `from.username`, never `from.id`. A confirmed human bound by
#     their NUMERIC Telegram id was rejected ("unrecognized sender 'unknown'").
#     Now resolved at the boundary via `find_by_identity` (id first, then
#     @username) — proven here through the REAL binary's `wg telegram
#     resolve-sender`, which runs the exact extract_sender → find_by_identity
#     path the listener uses.
#
#   Fix #0: a bot-sent message (from.is_bot == true) must never elect/route —
#     the diagnostic surfaces is_bot so the guard's input is visible.
#
#   Fix #4a: a content question with no greeting must NOT elect a four-way
#     collective — it goes to the concierge (otto). A pure greeting still does.
#     Proven through `wg telegram elect --json` (the listener's real election).
#
# Credential-free: both subcommands only read local config/bindings; nothing is
# ever sent to Telegram.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"

# A self-contained project with a REAL confirmed human bound to a NUMERIC
# Telegram id (Luca's live case: no public @username, id 8905220378).
(
    cd "$scratch"
    export WG_DIR=
    wg init >/dev/null 2>&1
    wg agency human add "Luca" --telegram 8905220378 >/dev/null 2>&1
    wg agency human confirm 8905220378 >/dev/null 2>&1
)

# The four family voices, so `wg telegram elect` has a roster to elect from.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "nora"
username  = "nora_casapinello_bot"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "bruno"
username  = "bruno_casapinello_bot"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "mira"
username  = "mira_casapinello_bot"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "otto"
username  = "otto_casapinello_bot"
TOML

resolve() { (cd "$scratch" && WG_DIR= wg --json telegram resolve-sender "$1" 2>&1); }
elect()   { (cd "$scratch" && WG_DIR= wg --json telegram elect "$@" 2>&1); }

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

echo "Fix #5: a raw update carrying only from.id (no @username) resolves to the confirmed human:"
out="$(resolve '{"message":{"from":{"id":8905220378,"is_bot":false},"chat":{"id":8905220378,"type":"private"},"text":"bruno?"}}')"
expect_grep   "numeric id resolves to human-luca"      "$out" '"agent_id": "human-luca"'
expect_grep   "the resolved human is confirmed"        "$out" '"confirmed": true'
expect_absent "sender is NOT dropped as unrecognized"  "$out" '"agent_id": null'

echo "Fix #0: a bot-sent update is flagged is_bot (the guard's input is visible):"
out="$(resolve '{"message":{"from":{"id":7777,"is_bot":true,"username":"nora_casapinello_bot"},"text":"Hey everyone!"}}')"
expect_grep   "is_bot true is surfaced"                "$out" '"is_bot": true'

echo "Fix #5: an unbound sender is unrecognized (not mis-resolved):"
out="$(resolve '{"message":{"from":{"id":999999,"is_bot":false},"text":"hi"}}')"
expect_grep   "unbound sender resolves to no agent"    "$out" '"agent_id": null'

echo "Fix #4a: a content question with no greeting elects the concierge (otto), NOT a collective:"
out="$(elect 'what is on the menu tomorrow?' --chat-type supergroup)"
expect_grep   "elected the single concierge voice"     "$out" '"who": "otto"'
expect_grep   "by the concierge rule"                  "$out" '"addressed_by": "concierge"'
expect_absent "did NOT fan out to the roster"          "$out" '"kind": "collective"'

echo "Fix #4a regression: a PURE greeting still elects the whole roster (collective):"
out="$(elect 'hey everyone, how is it going?' --chat-type supergroup)"
expect_grep   "pure greeting stays collective"         "$out" '"kind": "collective"'

echo "PASS: sender resolution (id→human), bot-sender visibility, and the content-question-vs-collective boundary all hold through the real binary"
