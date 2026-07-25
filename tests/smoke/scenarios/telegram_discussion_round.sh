#!/usr/bin/env bash
# Smoke: discussion-round — 'find consensus' asks run a multi-voice round.
#
# Pins task group-chat-discussion: when a confirmed-human group message is
# elected COLLECTIVE (all four) AND reads as an opinion/discussion ask
# ('discuss', 'find consensus', 'what do you all think', 'thoughts?'), the
# listener runs a DISCUSSION ROUND — each persona a short in-voice take in
# roster order, then Otto synthesizes — instead of four independent replies.
# A plain collective GREETING keeps today's behavior (brief independent hellos),
# and a single-voice/concierge ask never triggers a round.
#
# Proven through the REAL binary via `wg telegram discuss --json`, which runs the
# exact `elect_responders` + `is_discussion_ask` gate the live `Election::All`
# handler uses. Credential-free — nothing is ever sent to Telegram.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"

# A self-contained project with a confirmed human (single-human Casa Pinello
# group) so the membership-aware election answers rather than staying silent.
(
    cd "$scratch"
    export WG_DIR=
    wg init >/dev/null 2>&1
    wg agency human add "Luca" --telegram 8905220378 >/dev/null 2>&1
    wg agency human confirm 8905220378 >/dev/null 2>&1
)

# The project-local source of roster order, presentation, and coordination
# ownership. These are test-fixture names; production has no compiled roster.
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

# Matching dummy bots. Roster order still comes from household.toml.
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

discuss() { (cd "$scratch" && WG_DIR= wg --json telegram discuss "$1" 2>&1); }

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

# --- Discussion asks → a discussion round ---------------------------------
for msg in \
    "can you guys discuss this and find consensus" \
    "tell me what you all think" \
    "what do you all think?" \
    "thoughts on the holiday plan everyone?"
do
    out="$(discuss "$msg")"
    expect_grep "discussion ask runs a round: $msg" "$out" '"category": "discussion-round"'
    expect_grep "round is a discussion ask: $msg" "$out" '"is_discussion_ask": true'
    expect_grep "round synthesizer is otto: $msg" "$out" '"synthesizer": "otto"'
done

# The round voices are the whole roster in order.
out="$(discuss "can you guys discuss dinner and find consensus")"
for voice in nora bruno mira otto; do
    expect_grep "round includes $voice" "$out" "\"$voice\""
done

# --- Plain collective greeting → NO round (today's independent hellos) -----
for msg in \
    "hey guys are you around?" \
    "hi everyone!" \
    "goodnight all"
do
    out="$(discuss "$msg")"
    expect_grep "collective greeting is not a round: $msg" "$out" '"category": "collective-greeting"'
    expect_grep "greeting is not a discussion ask: $msg" "$out" '"is_discussion_ask": false'
done

# --- Single-voice / concierge ask → NO round -------------------------------
out="$(discuss "what is on the menu tomorrow?")"
expect_grep "concierge ask is a single voice, no round" "$out" '"category": "single-voice"'

out="$(discuss "nora, can you help?")"
expect_grep "named address is a single voice, no round" "$out" '"category": "single-voice"'

echo "PASS: telegram_discussion_round"
