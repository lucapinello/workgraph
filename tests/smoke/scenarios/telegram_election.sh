#!/usr/bin/env bash
# Smoke: all-bots-privacy-off responder election.
#
# Pins the election contract from docs/09 §natural-group: when ALL four bots run
# with BotFather privacy OFF and a group message survives cross-bot dedupe,
# `wg telegram elect` (the exact `elect_responders` decision the listener runs,
# minus the network send) resolves it via Luca's ordered table:
#   - @mention                       → that bot (mention beats name-in-text)
#   - explicit addressed name        → that agent
#   - reply to a bot's message       → that agent (reply-chain)
#   - COLLECTIVE address ("hey guys") → ALL FOUR answer, roster order
#   - team-directed unaddressed ask  → OTTO (group coordinator)
#   - pure human-to-human small talk → SILENCE
#
# Drives the REAL binary against a fixture Casa-Pinello notify.toml, so it
# exercises the user-visible flow (type in the group → the right voice(s)
# answer), not just the library. Credential-free: election is a pure decision —
# no live group, no real tokens, nothing is sent.

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

# elect <message> [extra args...] → the one-line decision from the real binary.
elect() {
    local msg="$1"
    shift
    (cd "$scratch" && WG_DIR= wg telegram elect "$msg" "$@" 2>&1)
}

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    echo "  $desc → $out"
    echo "$out" | grep -q "$needle" \
        || loud_fail "$desc: expected '$needle', got: $out"
}

echo "a. explicit name → that voice:"
expect_grep "name/nora" "$(elect "nora, what's for dinner?")" "answered by nora (by name)"
expect_grep "name/bruno" "$(elect "tell Bruno the curry was great")" "answered by bruno (by name)"

echo "b. @mention beats name-in-text:"
out="$(elect "nora can you ask @bruno_casapinello_bot about it")"
expect_grep "mention-beats-name" "$out" "answered by bruno (by @mention)"

echo "c. reply-chain → the replied-to voice:"
out="$(elect "yes that works" --reply-to-bot mira_casapinello_bot)"
expect_grep "reply-chain/mira" "$out" "answered by mira (by reply-chain)"

echo "d. collective address → ALL FOUR in roster order:"
out="$(elect "hey guys, how's it going?")"
expect_grep "collective" "$out" "collective address — the whole roster answers in order: nora, bruno, mira, otto"

echo "d2. typo-tolerant summon (fuzzy-summon) → ALL FOUR:"
# THE LIVE CASE: Luca wrote "hey guyd are you aroind?" — the typos ("guyd",
# "aroind") missed exact-phrase matching and it wrongly elected small-talk
# silence. To a human it is an unambiguous group summon → the roster answers.
out="$(elect "hey guyd are you aroind?")"
expect_grep "fuzzy-summon/luca-typos" "$out" "collective address — the whole roster answers in order: nora, bruno, mira, otto"
# Fuzzy trigger phrase ("hi guyz" ~ "hi guys") also summons the roster.
out="$(elect "hi guyz where is everyone?")"
expect_grep "fuzzy-summon/hi-guyz" "$out" "collective address — the whole roster answers in order: nora, bruno, mira, otto"
# Counter-case: a greeting mentioned mid-sentence is narration, NOT a summon —
# the silence preference for non-greeting-shaped chatter must hold. Two humans
# present (`--humans 2`) so the conservative silence rule is in force.
expect_grep "fuzzy-summon/narration-not-summon" "$(elect "he said hey to me yesterday?" --humans 2)" "silence (small-talk)"

echo "e. team-directed unaddressed ask → otto coordinates:"
expect_grep "ask/someone" "$(elect "can someone plan Saturday dinner?")" "answered by otto (by concierge)"
expect_grep "ask/domain-q" "$(elect "what's the plan for dinner tonight?")" "answered by otto (by concierge)"

echo "f. pure small talk with 2+ humans → SILENCE (family chatter protected):"
# `--humans 2` forces the conservative rule: with more than one human in the
# group these are human-to-human lines the bots deliberately stay out of.
expect_grep "silence/laughter" "$(elect "haha that was so funny" --humans 2)" "silence (small-talk)"
expect_grep "silence/human-q" "$(elect "did you have a good day?" --humans 2)" "silence (small-talk)"
# Tricky: a name talking ABOUT a human must not summon the bot (→ small talk).
expect_grep "silence/name-about-human" "$(elect "nora from work said hi today" --humans 2)" "silence (small-talk)"

echo "g. membership-aware silence — a SINGLE-human group answers greetings:"
# THE LIVE CASE (02:45): Luca posted a bare "hello" in a group with one human
# and four bots. The small-talk rule protects human-to-human chatter, but there
# is none to protect here — every message is for the team — so a bare greeting
# now earns a brief warm roster greeting instead of silence.
expect_grep "solo/bare-hello-collective" "$(elect "hello" --humans 1)" "collective address — the whole roster answers in order: nora, bruno, mira, otto"
# The SAME "hello" with a second human present stays silent (chatter returns).
expect_grep "two-human/bare-hello-silence" "$(elect "hello" --humans 2)" "silence (small-talk)"
# A greeting that names the whole group ("goodnight guys") is a broadcast in
# ANY membership — collective with one human AND with two.
expect_grep "goodnight-guys/solo" "$(elect "goodnight guys" --humans 1)" "collective address — the whole roster answers in order: nora, bruno, mira, otto"
expect_grep "goodnight-guys/two-human" "$(elect "goodnight guys" --humans 2)" "collective address — the whole roster answers in order: nora, bruno, mira, otto"
# In a single-human group, non-greeting unaddressed text leans on otto rather
# than falling silent — the human is never left talking to an empty room.
expect_grep "solo/nongreeting-otto" "$(elect "just got home" --humans 1)" "answered by otto (by concierge)"

echo "Private chat is a 1:1 passthrough (privacy preserved):"
expect_grep "private" "$(elect "hey guys" --chat-type private)" "1:1 passthrough"

# No token ever leaks into any election output.
if elect "otto, status?" | grep -q "dummy-token"; then
    loud_fail "bot token leaked into election output"
fi

# h. REGRESSION (fix-mention-precedence): an explicit @mention must ALWAYS win
#    over the small-talk/silence classifier, EVEN when the config omits the
#    optional `username` field — which is exactly how the LIVE .wg/notify.toml
#    was shaped when `elect "@nora_casapinello_bot what about you?"` wrongly
#    returned `silence(small-talk)`. Here the fixture binds only agent_id + the
#    bot-id key (no username), mirroring production; resolution must still route
#    the real @handle to its agent by @mention.
echo "h. @mention resolves & beats silence with NO username configured (live-config shape):"
nou_scratch="$(make_scratch)"
mkdir -p "$nou_scratch/.wg"
cat >"$nou_scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "nora"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "bruno"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "mira"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "otto"
TOML
elect_nou() {
    local msg="$1"
    shift
    (cd "$nou_scratch" && WG_DIR= wg telegram elect "$msg" "$@" 2>&1)
}
# The exact live failure: mention + small-talk-shaped tail → the mentioned agent.
expect_grep "no-username/mention-beats-small-talk" \
    "$(elect_nou "@nora_casapinello_bot what about you?")" \
    "answered by nora (by @mention)"
# Trailing punctuation on the handle must not defeat the mention path.
expect_grep "no-username/bruno-question" \
    "$(elect_nou "@bruno_casapinello_bot?")" \
    "answered by bruno (by @mention)"
# The rest of the ladder still holds without usernames (reply-chain resolves the
# real handle too; a team ask still coordinates through otto; chatter is silent).
expect_grep "no-username/reply-chain" \
    "$(elect_nou "sounds good" --reply-to-bot mira_casapinello_bot)" \
    "answered by mira (by reply-chain)"
expect_grep "no-username/ask-otto" \
    "$(elect_nou "can someone plan dinner?")" \
    "answered by otto (by concierge)"
# Two humans present so the conservative silence rule holds (this fixture has no
# agency/, i.e. zero onboarded humans, which would otherwise answer greetings).
expect_grep "no-username/small-talk-silent" \
    "$(elect_nou "haha yeah that was fun" --humans 2)" \
    "silence (small-talk)"

echo "PASS: responder election (name / mention-beats-name / reply-chain / collective→4 / ask→otto / small-talk→silence / @mention-without-username→that-agent)"
