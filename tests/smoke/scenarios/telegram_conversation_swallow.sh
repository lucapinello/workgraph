#!/usr/bin/env bash
# Smoke: the hardened-auth conversational SWALLOW regression.
#
# Pins the failure where every ordinary group message from a confirmed household
# member was consumed, produced NO reply, and produced NO useful diagnostic.
# Root cause class: the hardened awaiting-human-task router (`route_inbound_reply`,
# PR #51) authorizes the inbound sender against their CONFIRMED Telegram binding.
# In a household group the member speaks through an AI-persona bot, so the
# router's defense-in-depth check ("a per-agent bot must front the same human
# the sender is bound to") declines the parked-task route. That hardening is
# correct for recording a reply onto a TASK — but a confirmed human's ordinary
# chat turn must never be silently swallowed by it: it must fall through to the
# conversational composer.
#
# This drives the REAL binary via `wg telegram classify` — the dry-run of the
# exact `classify_inbound_message` the live `wg telegram listen` loop invokes
# once a message survives dedupe + election (the pure, filesystem-only core of
# the inbound branch). Credential-free: nothing is ever sent.
#
# What it locks:
#   - confirmed human + persona bot + benign hardened decline → `unmatched`
#     (⇒ the conversational composer answers), never `confirmed`/`routed`
#     (which would mean the turn was swallowed onto a task/onboarding)
#   - the server-side log identifies the benign parked-task decline and says the
#     turn continues, without classifying ordinary conversation as a rejection
#   - an unknown sender is also `unmatched` (the composer onboards them), but
#     still emits the hardened authorization rejection diagnostic
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

# A self-contained project: init seeds the graph + agents; `human add` +
# `confirm` creates a REAL confirmed member bound to an opaque sender identity.
(
    cd "$scratch"
    export WG_DIR=
    wg init >/dev/null 2>&1
    wg agency human add "River Member" --telegram member-41 >/dev/null 2>&1
    wg agency human confirm member-41 >/dev/null 2>&1
)

human_id="$(awk -F': ' '/^[[:space:]]*agent_id:/ {print $2; exit}' \
    "$scratch/.wg/agency/bindings/telegram.yaml")"
if [ -z "$human_id" ]; then
    loud_fail "setup: confirmed member binding has no agent id"
fi

# Point an opaque persona bot at a NON-human agent (any seed agent that is not
# the just-created household member) so the hardened router's bot-fronting check
# takes the ordinary persona-bot decline path.
persona_id=""
for f in "$scratch"/.wg/agency/cache/agents/*.yaml; do
    id="$(awk -F': ' '/^id:/ {print $2; exit}' "$f")"
    [ "$id" = "$human_id" ] && continue
    persona_id="$id"
    break
done
if [ -z "$persona_id" ]; then
    loud_fail "setup: no non-human seed agent found to front the persona bot"
fi

# The project-local presentation is intentionally unrelated to any shipped
# persona. Assertions below do not depend on this display name.
cat >"$scratch/household.toml" <<TOML
[[agent]]
id = "$persona_id"
name = "Amber Relay"
emoji = "🟠"
domains = ["coordination"]
TOML

cat >"$scratch/.wg/notify.toml" <<TOML
[telegram.bots.relay-4]
bot_token = "0000000000:relay-fixture-token"
chat_id   = "-1007000042"
agent_id  = "$persona_id"
username  = "amber_relay_house_bot"
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

echo "confirmed member, persona-bot decline → falls through to conversation (not swallowed):"
out="$(classify --channel telegram:relay-4 --sender member-41 --message 'is anyone there?')"
expect_grep   "machine outcome is UNMATCHED (⇒ conversational composer)" "$out" '"kind": "unmatched"'
expect_grep   "diagnostic identifies the parked-task decline"            "$out" 'not a parked-task reply'
expect_grep   "diagnostic says conversation continues"                   "$out" 'continuing to conversation'
expect_absent "ordinary conversation is NOT a security rejection"        "$out" 'Rejected reply'
expect_absent "the turn was NOT swallowed onto a task"                   "$out" '"kind": "routed"'
expect_absent "the turn was NOT swallowed as onboarding"                 "$out" '"kind": "confirmed"'
expect_absent "the raw persona id is not exposed"                         "$out" "$persona_id"

echo "unknown sender → unmatched, with the hardened rejection still visible:"
out="$(classify --channel telegram:relay-4 --sender visitor-9 --message 'hi there')"
expect_grep   "unknown sender machine outcome is UNMATCHED" "$out" '"kind": "unmatched"'
expect_grep   "unknown sender emits the security diagnostic" "$out" 'Rejected reply from visitor-9'
expect_grep   "authorization reason remains machine-checkable" "$out" 'unrecognized sender'
expect_absent "unknown sender is NOT routed"                  "$out" '"kind": "routed"'
expect_absent "unknown sender is NOT confirmed"               "$out" '"kind": "confirmed"'

echo "no bot token ever leaks into the classification output:"
expect_absent "no token leak" "$out" "fixture-token"

echo "PASS: persona-bot chat falls through while true authorization rejection stays visible"
