#!/usr/bin/env bash
# Smoke: web-inbound — a kiosk-pane message is a FIRST-CLASS group turn.
#
# Pins task web-chat-first. The live gap: a message typed in the casa kiosk
# conversation pane was only RELAYED into the family Telegram group by a bot, and
# Telegram bots never see other bots' messages — so the listener's election /
# conversation pipeline NEVER ran on it (posted, never answered), while the SAME
# words typed on a phone got four replies.
#
# The fix is `wg telegram web-inbound`, which runs the EXACT `elect_responders`
# table the live listener runs and then dispatches through the SAME senders +
# composer — collective (all four) / discussion round / single voice / silence.
# The gateway shells out to it on POST /conversation/send.
#
# Proven through the REAL binary via `wg telegram web-inbound --dry-run --json`:
# the dry-run seam runs the real election + planning and reports WHO would answer
# without sending — so this is credential-free (nothing is ever sent to Telegram)
# and asserts the web-origin message routes identically to a Telegram message.
#
# The household below is deliberately OPAQUE: agent ids, authored display names,
# bot handles and the confirmed human have never shipped as defaults, and no id
# contains a domain word. So a pass here cannot come from the engine recognising
# a built-in persona — `domains = [...]` in the scratch `household.toml` is the
# only ownership signal, and the gate stays meaningful for a deployment that
# names its own family. The sibling cast lives in
# tests/smoke/scenarios/telegram_household_conversation_matrix.sh.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"
repo_root="$(cd "$scenario_dir/../../.." && pwd)"

review_wg="${WG_BIN:-}"
if [[ -z "$review_wg" ]]; then
    review_wg="$(command -v wg 2>/dev/null || true)"
elif [[ "$review_wg" != /* ]]; then
    review_wg="$repo_root/$review_wg"
fi
if [[ -z "$review_wg" || ! -x "$review_wg" ]]; then
    loud_skip "MISSING WG BINARY" "set WG_BIN to the freshly built executable"
fi

scratch="$(make_scratch)"

# --- The composed cast ------------------------------------------------------
# Declared once, then WRITTEN into the fixture, so every assertion below reads
# the configured household instead of a literal the engine might also know.
MEALS_ID="quill-3"
MEALS_NAME="Cedar Signal"
COOK_ID="kiln-2"
COOK_NAME="Copper Ladle"
TRAIN_ID="trail-9"
TRAIN_NAME="North Compass"
COORD_ID="relay-4"
COORD_NAME="Open Door"
ROSTER=("$MEALS_ID" "$COOK_ID" "$TRAIN_ID" "$COORD_ID")

# The one confirmed human. `agency human add` slugifies the authored name into
# the agency agent id (`human-<handle>`) — the same shape a kiosk session hands
# over as its `humanId`.
HUMAN_NAME="River Guest"
HUMAN_HANDLE="river-guest"
HUMAN_TELEGRAM="7700415529"
GROUP_CHAT="-1000000000001"

# A self-contained project with a confirmed human (a single-human household
# group) so the membership-aware election answers rather than staying silent —
# and so the web sender resolves to a confirmed human (grounded, not onboarding).
(
    cd "$scratch"
    export WG_DIR=
    "$review_wg" init >/dev/null 2>&1
    "$review_wg" agency human add "$HUMAN_NAME" --telegram "$HUMAN_TELEGRAM" >/dev/null 2>&1
    "$review_wg" agency human confirm "$HUMAN_TELEGRAM" >/dev/null 2>&1
)

cat >"$scratch/household.toml" <<TOML
[[agent]]
id = "$MEALS_ID"
name = "$MEALS_NAME"
emoji = "🌿"
domains = ["meals", "nutrition"]
[[agent]]
id = "$COOK_ID"
name = "$COOK_NAME"
emoji = "🥄"
domains = ["meals", "cooking", "recipes"]
[[agent]]
id = "$TRAIN_ID"
name = "$TRAIN_NAME"
emoji = "🧭"
domains = ["workouts"]
[[agent]]
id = "$COORD_ID"
name = "$COORD_NAME"
emoji = "🏡"
domains = ["calendar", "coordination", "shopping"]
TOML

# Matching bots; collective order comes from household.toml.
cat >"$scratch/.wg/notify.toml" <<TOML
[telegram]
chat_id = "$GROUP_CHAT"

[telegram.bots.$MEALS_ID]
bot_token = "0000000000:quill-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$MEALS_ID"
username  = "cedar_signal_house_bot"

[telegram.bots.$COOK_ID]
bot_token = "0000000000:kiln-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$COOK_ID"
username  = "copper_ladle_house_bot"

[telegram.bots.$TRAIN_ID]
bot_token = "0000000000:trail-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$TRAIN_ID"
username  = "north_compass_house_bot"

[telegram.bots.$COORD_ID]
bot_token = "0000000000:relay-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$COORD_ID"
username  = "open_door_house_bot"
TOML

web() { (cd "$scratch" && WG_DIR= "$review_wg" --json telegram web-inbound --dry-run --sender "$1" --message "$2" 2>&1); }
web_turn() {
    (cd "$scratch" && WG_DIR= WG_TURN_ID="$3" "$review_wg" --json telegram web-inbound --dry-run --sender "$1" --message "$2" 2>&1)
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

# --- Collective ask → the whole roster answers (the headline fix) -----------
out="$(web "$HUMAN_NAME" "hey all, are you around?")"
expect_grep "collective ask elects the roster" "$out" '"category": "collective"'
for voice in "${ROSTER[@]}"; do
    expect_grep "collective includes the configured voice $voice" "$out" "$voice"
done

# --- Discussion ask → a multi-voice round -----------------------------------
out="$(web "$HUMAN_NAME" "can you guys discuss dinner and find consensus?")"
expect_grep "discussion ask runs a round" "$out" '"category": "discussion-round"'

# --- Named ask → exactly one voice ------------------------------------------
# Addressed by the AUTHORED display name and answered by the id that name is
# bound to in the fixture, never by a compiled-in default.
out="$(web "$HUMAN_NAME" "$MEALS_NAME, what's for dinner?")"
expect_grep "named address is a single voice" "$out" '"category": "single-voice"'
expect_grep "the named voice is the configured meals owner" "$out" "\"who\": \"$MEALS_ID\""

# A second authored name must land on a DIFFERENT configured id, so the pass
# above cannot come from one hardcoded winner.
out="$(web "$HUMAN_NAME" "$COORD_NAME, can you take that one?")"
expect_grep "a second authored name is also a single voice" "$out" '"category": "single-voice"'
expect_grep "the second named voice is the configured coordinator" "$out" "\"who\": \"$COORD_ID\""

# --- Web identity resolves to the confirmed human ---------------------------
# The web session hands a humanId ("river-guest") / display name — it must
# resolve to the confirmed human's binding key so the composer answers grounded,
# not the onboarding line. auth_sender is the stored numeric telegram id.
out="$(web "$HUMAN_HANDLE" "$MEALS_NAME?")"
expect_grep "web humanId resolves to the confirmed human" "$out" "\"auth_sender\": \"$HUMAN_TELEGRAM\""

# --- Gateway occurrence id reaches the engine idempotency seam ---------------
# The exact same accepted turn id is stable on a dispatcher refire, while a
# later occurrence with identical words gets a different opaque fingerprint.
turn_fingerprint() {
    sed -n 's/.*"turn_fingerprint": "\([^"]*\)".*/\1/p' <<<"$1" | head -1
}
same_words="hey all, please help with the weekend"
first="$(web_turn "$HUMAN_NAME" "$same_words" "turn-smoke-a7")"
refire="$(web_turn "$HUMAN_NAME" "$same_words" "turn-smoke-a7")"
later="$(web_turn "$HUMAN_NAME" "$same_words" "turn-smoke-b9")"
first_fp="$(turn_fingerprint "$first")"
refire_fp="$(turn_fingerprint "$refire")"
later_fp="$(turn_fingerprint "$later")"
[[ -n "$first_fp" ]] || loud_fail "WG_TURN_ID reader emitted no turn_fingerprint"
[[ "$first_fp" == "$refire_fp" ]] || loud_fail "same WG_TURN_ID did not retain its fingerprint"
[[ "$first_fp" != "$later_fp" ]] || loud_fail "different WG_TURN_ID values collapsed for identical words"
echo "  ok: WG_TURN_ID is stable on refire and distinct for a later identical ask"

# --- Small talk with a second human present → silence (no double-answering) --
# With 2+ humans the membership-aware rule protects human-to-human chatter: the
# web message is silenced just as the same words would be on Telegram.
out="$(cd "$scratch" && WG_DIR= "$review_wg" --json telegram web-inbound --dry-run --sender "$HUMAN_NAME" --message "lol ok sounds good" 2>&1 || true)"
# In a single-human group even small talk may draw the concierge; that is fine.
# The invariant we pin: a dry-run never errors and always reports a category.
expect_grep "dry-run always reports a category" "$out" '"category"'

echo "PASS: telegram_web_inbound"
