#!/usr/bin/env bash
# Smoke: the pr51-auth conversational SWALLOW regression.
#
# Pins the live bug Luca hit after the pr51-auth deploy (fix-conversation-swallow):
# EVERY group message from a confirmed human — including "otto, are you there?"
# and collective greetings — was consumed, produced NO reply and NO log line.
# Root cause class: the hardened awaiting-human-task router (`route_inbound_reply`,
# PR #51) authorizes the inbound sender against their CONFIRMED Telegram binding.
# In Casa the human speaks THROUGH an AI-persona bot, so the router's
# defense-in-depth check ("a per-agent bot must front the same human the sender
# is bound to") REJECTS the turn. That hardening is correct for recording a
# reply onto a TASK — but a confirmed human's ordinary chat turn must never be
# silently swallowed by it: it must fall through to the conversational composer.
#
# This drives the REAL binary via `wg telegram classify` — the dry-run of the
# exact `classify_inbound_message` the live `wg telegram listen` loop invokes
# once a message survives dedupe + election (the pure, filesystem-only core of
# the inbound branch). Credential-free: nothing is ever sent.
#
# What it locks:
#   - confirmed human + persona bot + hardened REJECTION → classifies `unmatched`
#     (⇒ the conversational composer answers), never `confirmed`/`routed`
#     (which would mean the turn was swallowed onto a task/onboarding)
#   - the rejection really is the pr51 hardened-auth path (the server-side log
#     names it), proving we exercise the swallow, not a trivial no-op
#   - an unknown sender is also `unmatched` (the composer onboards them) — the
#     classifier never mis-records a stranger as a human's reply
#
# Companion coverage: the composer itself is pinned by telegram_conversation.sh
# (`wg telegram conversation`), and the classify precedence/fallthrough is unit-
# tested in commands::telegram (awaiting_human_task_reply_wins_over_conversation,
# hardened_auth_rejection_falls_through_to_conversation).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"

# A self-contained project: init seeds the graph + default agents; `human add`
# + `confirm` creates a REAL confirmed human (human-luca) bound to sender luca-1.
(
    cd "$scratch"
    export WG_DIR=
    wg init >/dev/null 2>&1
    wg agency human add "Luca" --telegram luca-1 >/dev/null 2>&1
    wg agency human confirm luca-1 >/dev/null 2>&1
)

# Point the persona bot "otto" at a NON-human agent (any seed agent that is not
# human-luca) so the hardened router's bot-fronting check rejects luca-1's turn
# exactly as it does live — bound to human-luca, arriving on otto's bot.
persona_id=""
for f in "$scratch"/.wg/agency/cache/agents/*.yaml; do
    id="$(awk -F': ' '/^id:/ {print $2; exit}' "$f")"
    [ "$id" = "human-luca" ] && continue
    persona_id="$id"
    break
done
if [ -z "$persona_id" ]; then
    loud_fail "setup: no non-human seed agent found to front the persona bot"
fi

cat >"$scratch/.wg/notify.toml" <<TOML
[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "$persona_id"
username  = "otto_casapinello_bot"
TOML

# classify <flags...> → the listener's classification decision (+ its log line).
classify() {
    (cd "$scratch" && WG_DIR= wg --json telegram classify "$@" 2>&1)
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

echo "confirmed human, hardened-auth REJECTION → falls through to conversation (not swallowed):"
out="$(classify --channel telegram:otto --sender luca-1 --message 'otto, are you there?')"
expect_grep   "the turn is UNMATCHED (⇒ conversational composer)" "$out" '"kind": "unmatched"'
expect_grep   "the rejection is the pr51 hardened-auth path"      "$out" 'Rejected reply from luca-1'
expect_absent "the turn was NOT swallowed onto a task"            "$out" '"kind": "routed"'
expect_absent "the turn was NOT swallowed as an onboarding"       "$out" '"kind": "confirmed"'

echo "unknown sender → unmatched (composer onboards), never mis-recorded as a human's reply:"
out="$(classify --channel telegram:otto --sender stranger-9 --message 'hi there')"
expect_grep   "unknown sender is UNMATCHED"          "$out" '"kind": "unmatched"'
expect_absent "unknown sender is NOT routed"         "$out" '"kind": "routed"'
expect_absent "unknown sender is NOT confirmed"      "$out" '"kind": "confirmed"'

echo "no bot token ever leaks into the classification output:"
expect_absent "no token leak" "$out" "dummy-token"

echo "PASS: a confirmed human's chat turn that pr51-auth rejects falls through to the conversational composer — the swallow can never regress silently"
