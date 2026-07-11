#!/usr/bin/env bash
# Smoke: natural group routing — the "make the family group feel natural" layer.
#
# Pins the routing contract from docs/09 §natural-group: when a bot receives a
# group message, `wg telegram route` (the exact `route_natural` decision the
# `wg telegram listen` listener runs, minus the network send) must resolve it to
# a family voice by, in order: an explicit @mention, the first family name in the
# text, the bot a reply is threaded onto, and finally the concierge (otto) when
# no one is named. `/standup` is intercepted for the whole roster.
#
# This drives the REAL binary against a fixture Casa-Pinello notify.toml, so it
# exercises the user-visible flow (type in the group → the right voice answers),
# not just the library. Credential-free: routing is a pure decision — no live
# group, no real tokens, nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# Fixture: the four Casa Pinello voices, each with its @handle username.
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

# route <message> [extra args...] → the one-line decision from the real binary.
route() {
    local msg="$1"
    shift
    (cd "$scratch" && WG_DIR= wg telegram route "$msg" "$@" 2>&1)
}

expect_agent() {
    local desc="$1" msg="$2" want="$3"
    shift 3
    local out
    out="$(route "$msg" "$@")" || loud_fail "$desc: route exited non-zero: $out"
    echo "  $desc → $out"
    echo "$out" | grep -q "routed to $want " \
        || loud_fail "$desc: expected route to '$want', got: $out"
}

echo "Name-addressed routing (3 cases):"
# Case 1: leading name with comma.
expect_agent "name/nora" "nora, what's for dinner?" "nora"
# Case 2: name mid-sentence, mixed case.
expect_agent "name/bruno" "tell Bruno the curry was great" "bruno"
# Case 3: first of several names wins (left-to-right).
expect_agent "name/mira" "mira can you and otto sort the schedule?" "mira"

echo "No name → concierge (otto):"
out="$(route "hi guys what are you doing")"
echo "  no-name → $out"
echo "$out" | grep -q "routed to otto (by concierge)" \
    || loud_fail "no-name message must route to otto concierge, got: $out"

echo "Reply-chain routing:"
out="$(route "yes that works" --reply-to-bot bruno_casapinello_bot)"
echo "  reply-chain → $out"
echo "$out" | grep -q "routed to bruno (by reply-chain)" \
    || loud_fail "reply to bruno's message must route to bruno, got: $out"

echo "Explicit @mention still wins (R17):"
out="$(route "@mira_casapinello_bot add a run")"
echo "  mention → $out"
echo "$out" | grep -q "routed to mira (by @mention)" \
    || loud_fail "@mention must route to that bot, got: $out"

echo "/standup is intercepted for the whole roster:"
out="$(route "/standup")"
echo "  standup → $out"
echo "$out" | grep -q "intercepted; posts the whole roster" \
    || loud_fail "/standup must be intercepted, got: $out"

# Privacy-by-default is preserved: a 1:1 (private) chat is never group-routed.
out="$(route "hi guys what are you doing" --chat-type private)"
echo "$out" | grep -q "1:1 passthrough" \
    || loud_fail "private chat must be a 1:1 passthrough, got: $out"

# No token ever leaks into any routing output.
if route "otto, status?" | grep -q "dummy-token"; then
    loud_fail "bot token leaked into routing output"
fi

echo "PASS: natural group routing (name / no-name→otto / reply-chain / mention / standup)"
