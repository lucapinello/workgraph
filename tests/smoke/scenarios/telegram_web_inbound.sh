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
ALIAS_BOT_KEY="wire-z8"
ALIAS_AGENT_ID="orbit-z8"
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

[telegram.bots.$ALIAS_BOT_KEY]
bot_token = "0000000000:alias-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$ALIAS_AGENT_ID"
username  = "mutable_alias_house_bot"
TOML

web() {
    (
        cd "$scratch"
        WG_DIR= "$review_wg" --json telegram web-inbound --dry-run \
            --default-owner "$COORD_ID" --sender "$1" --message "$2" 2>&1
    )
}
web_turn() {
    (
        cd "$scratch"
        WG_DIR= WG_TURN_ID="$3" "$review_wg" --json telegram web-inbound --dry-run \
            --default-owner "$COORD_ID" --sender "$1" --message "$2" 2>&1
    )
}
web_choice() {
    local choice_flag="$1"
    shift
    (
        cd "$scratch"
        WG_DIR= "$review_wg" --json telegram web-inbound --dry-run \
            "$choice_flag" "$@"
    )
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

turn_fingerprint() {
    sed -n 's/.*"turn_fingerprint": "\([^"]*\)".*/\1/p' <<<"$1" | head -1
}

# --- Collective ask → the whole roster answers (the headline fix) -----------
# The ask KEEPS its greeting opener on purpose. `social-closers-single`
# (1360b2a6) made a line that reduces to PURE courtesy elect one voice, and the
# old fixture here ("hey all, are you around?") is exactly that shape — so this
# leg started failing silently and took every later leg down with it (the file
# was last touched at 0c34c3a5, before that fix). The boundary the roster fan-out
# actually needs is "greeting + real content", which is what this asks; the pure
# courtesy side is pinned right below so neither can drift again unnoticed.
out="$(web "$HUMAN_NAME" "hey everyone, can you all take a look at the front door today?")"
expect_grep "collective ask elects the roster" "$out" '"category": "collective"'
for voice in "${ROSTER[@]}"; do
    expect_grep "collective includes the configured voice $voice" "$out" "$voice"
done

# --- The other side of that boundary: pure courtesy never fans out ----------
# `category` carries the whole claim. The `who` check below is a sanity check
# only: `web()` declares $COORD_ID as the default contact and $COORD_ID is also
# the coordination-marked helper, so it cannot tell those two chains apart — the
# listener-seam derivation is pinned separately (task engine-honour-household).
out="$(web "$HUMAN_NAME" "hey all, are you around?")"
expect_grep "a purely social collective line stays one voice" "$out" '"category": "single-voice"'
expect_grep "…and that voice is the household default contact" "$out" "\"who\": \"$COORD_ID\""

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

# --- Explicit default-contact contract --------------------------------------
# An otherwise-general ask follows the gateway's declared opaque default, not
# the engine-local coordination owner. The two ids deliberately differ so this
# assertion cannot pass vacuously.
general_ask="can someone help with the front door?"
out="$(web_choice --default-owner "$TRAIN_ID" --sender "$HUMAN_NAME" --message "$general_ask" 2>&1)"
expect_grep "general ask follows the explicit default contact" "$out" "\"who\": \"$TRAIN_ID\""

# Non-vacuity for key-vs-binding identity: the same extra binding whose table
# key is rejected below must route successfully through its canonical agent id.
out="$(web_choice --default-owner "$ALIAS_AGENT_ID" --sender "$HUMAN_NAME" --message "$general_ask" 2>&1)"
expect_grep "canonical id routes when its bot-table key differs" "$out" "\"who\": \"$ALIAS_AGENT_ID\""

# No designated contact and an unknown declared id both fail closed as a
# distinct successful outcome. `who` stays null: the engine must not invent a
# voice and the caller can render the contact-needed state honestly.
out="$(web_choice --no-default-owner --sender "$HUMAN_NAME" --message "$general_ask" 2>&1)"
expect_grep "no default contact returns needs-contact" "$out" '"category": "needs-contact"'
expect_grep "needs-contact has no responder" "$out" '"who": null'
expect_grep "no-default reason is machine readable" "$out" '"reason": "no-default-owner"'

# Drive the same no-default outcome without `--dry-run`. The deliberately
# closed loopback endpoint makes any accidental send fail, while the one-second
# composer timeout keeps a misplaced compose from hanging the gate. Success and
# an unchanged feed prove the terminal outcome occurs before compose/delivery.
feed_path="$scratch/.casa/group-feed.jsonl"
feed_size_before=0
if [[ -f "$feed_path" ]]; then
    feed_size_before="$(wc -c <"$feed_path" | tr -d ' ')"
fi
out="$(
    cd "$scratch"
    WG_DIR= \
    WG_TELEGRAM_API_BASE="http://127.0.0.1:9" \
    WG_TELEGRAM_COMPOSE_TIMEOUT_SECS=1 \
        "$review_wg" --json telegram web-inbound \
            --no-default-owner --sender "$HUMAN_NAME" --message "$general_ask" 2>&1
)"
expect_grep "live no-default path terminates as needs-contact" "$out" '"category": "needs-contact"'
feed_size_after=0
if [[ -f "$feed_path" ]]; then
    feed_size_after="$(wc -c <"$feed_path" | tr -d ' ')"
fi
[[ "$feed_size_before" == "$feed_size_after" ]] \
    || loud_fail "needs-contact wrote a family feed reply before a contact was designated"
echo "  ok: needs-contact performs no compose, send, or feed delivery"

out="$(web_choice --default-owner "not-in-this-house" --sender "$HUMAN_NAME" --message "$general_ask" 2>&1)"
expect_grep "unknown default contact fails closed" "$out" '"category": "needs-contact"'
expect_grep "unknown default has no responder" "$out" '"who": null'
expect_grep "unknown-default reason is machine readable" "$out" '"reason": "unknown-default-owner"'

# A bot-table key is transport identity only once an explicit stable agent id
# is configured. It must not remain an alternate persona id.
out="$(web_choice --default-owner "$ALIAS_BOT_KEY" --sender "$HUMAN_NAME" --message "$general_ask" 2>&1)"
expect_grep "bot-table alias is not a default-contact identity" "$out" '"category": "needs-contact"'
expect_grep "bot-table alias cannot choose a responder" "$out" '"who": null'

# The default declaration applies ONLY to general/concierge routing. Explicit
# names, domains, collective asks, and a positive owner pin all stay stronger.
out="$(web_choice --no-default-owner --sender "$HUMAN_NAME" --message "$MEALS_NAME, can you help?" 2>&1)"
expect_grep "named voice outranks no-default" "$out" "\"who\": \"$MEALS_ID\""

out="$(web_choice --no-default-owner --sender "$HUMAN_NAME" --message "what should we eat for dinner tonight?" 2>&1)"
expect_grep "domain owner outranks no-default" "$out" "\"who\": \"$MEALS_ID\""

# Same stale-fixture correction as the headline leg above: a line that reduces to
# pure courtesy is one voice now, and one voice with no designated contact is
# needs-contact — which would have "proved" the opposite of what this leg means.
out="$(web_choice --no-default-owner --sender "$HUMAN_NAME" \
    --message "hey everyone, can you all take a look at the front door today?" 2>&1)"
expect_grep "collective ask outranks no-default" "$out" '"category": "collective"'

out="$(
    cd "$scratch"
    WG_DIR= "$review_wg" --json telegram web-inbound --dry-run \
        --no-default-owner --owner "$COOK_ID" \
        --sender "$HUMAN_NAME" --message "$general_ask" 2>&1
)"
expect_grep "positive owner pin outranks no-default" "$out" "\"who\": \"$COOK_ID\""

# A supplied owner pin is binding, so a mutable handle or bot-table alias must
# fail before compose, feed mutation, or send instead of silently re-electing a
# different voice. Run the real non-dry seam against a closed endpoint; the
# specific owner-pin diagnostic and unchanged feed prove the early exit.
feed_size_before=0
if [[ -f "$feed_path" ]]; then
    feed_size_before="$(wc -c <"$feed_path" | tr -d ' ')"
fi
if out="$(
    cd "$scratch"
    WG_DIR= \
    WG_TELEGRAM_API_BASE="http://127.0.0.1:9" \
    WG_TELEGRAM_COMPOSE_TIMEOUT_SECS=1 \
        "$review_wg" --json telegram web-inbound \
            --no-default-owner --owner "$ALIAS_BOT_KEY" \
            --sender "$HUMAN_NAME" --message "$general_ask" 2>&1
)"; then
    loud_fail "web-inbound accepted a bot-table alias as an owner pin"
fi
expect_grep "invalid owner pin fails at the identity seam" "$out" "web-inbound owner pin"
feed_size_after=0
if [[ -f "$feed_path" ]]; then
    feed_size_after="$(wc -c <"$feed_path" | tr -d ' ')"
fi
[[ "$feed_size_before" == "$feed_size_after" ]] \
    || loud_fail "invalid owner pin wrote a family feed reply before failing"
echo "  ok: invalid owner pin performs no compose, send, or feed delivery"

# An explicitly blank CLI value must not disappear and fall through to a valid
# compatibility env pin. The explicit argument wins and is rejected as invalid.
if out="$(
    cd "$scratch"
    WG_DIR= WG_OWNER_PIN="$COOK_ID" \
        "$review_wg" --json telegram web-inbound --dry-run \
            --no-default-owner --owner "" \
            --sender "$HUMAN_NAME" --message "$general_ask" 2>&1
)"; then
    loud_fail "web-inbound erased an explicit blank owner pin"
fi
expect_grep "explicit blank owner pin cannot fall back to the env pin" "$out" "nonblank canonical agent id"

# --- Persisted clarification voice is exact machine identity ----------------
# A bare confirmation must return to the voice that asked, carrying the
# original ask as a ReplyChain even when the bot-table key differs. Compare its
# fingerprint with the same original ask to prove the word "yes" was not routed
# as a new turn.
clarify_ask="could somebody help plan the weekend?"
original_out="$(web_choice --default-owner "$ALIAS_AGENT_ID" --sender "$HUMAN_NAME" --message "$clarify_ask" 2>&1)"
original_fp="$(turn_fingerprint "$original_out")"
[[ -n "$original_fp" ]] || loud_fail "original clarification ask emitted no fingerprint"

mkdir -p "$scratch/.casa"
clarify_now="$(date +%s)"
cat >>"$scratch/.casa/clarify.jsonl" <<JSON
{"ts":$clarify_now,"chat_id":"$GROUP_CHAT","human":"$HUMAN_TELEGRAM","voice":"$ALIAS_AGENT_ID","original_ask":"$clarify_ask"}
JSON

continued_out="$(web_choice --no-default-owner --sender "$HUMAN_NAME" --message "yes" 2>&1)"
continued_fp="$(turn_fingerprint "$continued_out")"
expect_grep "clarification returns to the canonical persisted voice" "$continued_out" "\"who\": \"$ALIAS_AGENT_ID\""
expect_grep "clarification remains a reply chain" "$continued_out" "rule=reply"
[[ "$continued_fp" == "$original_fp" ]] \
    || loud_fail "clarification confirmation did not preserve the original ask fingerprint"
echo "  ok: clarification replays the original ask"

# Append a newer exchange carrying the shadowed transport key. This must fail
# nonzero before fresh default routing, compose, feed mutation, or send.
clarify_now="$(date +%s)"
cat >>"$scratch/.casa/clarify.jsonl" <<JSON
{"ts":$clarify_now,"chat_id":"$GROUP_CHAT","human":"$HUMAN_TELEGRAM","voice":"$ALIAS_BOT_KEY","original_ask":"$clarify_ask"}
JSON
feed_size_before=0
if [[ -f "$feed_path" ]]; then
    feed_size_before="$(wc -c <"$feed_path" | tr -d ' ')"
fi
if out="$(
    cd "$scratch"
    WG_DIR= \
    WG_TELEGRAM_API_BASE="http://127.0.0.1:9" \
    WG_TELEGRAM_COMPOSE_TIMEOUT_SECS=1 \
        "$review_wg" --json telegram web-inbound \
            --no-default-owner --sender "$HUMAN_NAME" --message "yes" 2>&1
)"; then
    loud_fail "web-inbound fell through from an invalid persisted clarification voice"
fi
expect_grep "invalid clarification voice fails at the identity seam" "$out" "web-inbound clarification voice"
feed_size_after=0
if [[ -f "$feed_path" ]]; then
    feed_size_after="$(wc -c <"$feed_path" | tr -d ' ')"
fi
[[ "$feed_size_before" == "$feed_size_after" ]] \
    || loud_fail "invalid clarification voice wrote a family feed reply before failing"
echo "  ok: invalid clarification voice performs no compose, send, or feed delivery"

# The choice is required and mutually exclusive at the real CLI boundary. This
# also pins the cross-version handshake: these are ordinary flags, so an older
# engine that does not know them rejects the new gateway invocation.
if (
    cd "$scratch"
    WG_DIR= "$review_wg" telegram web-inbound --dry-run \
        --sender "$HUMAN_NAME" --message "$general_ask" >/dev/null 2>&1
); then
    loud_fail "web-inbound accepted an ambiguous invocation with no default-owner choice"
fi
echo "  ok: web-inbound requires an explicit default-owner choice"

if (
    cd "$scratch"
    WG_DIR= "$review_wg" telegram web-inbound --dry-run \
        --default-owner "$COORD_ID" --no-default-owner \
        --sender "$HUMAN_NAME" --message "$general_ask" >/dev/null 2>&1
); then
    loud_fail "web-inbound accepted both default-owner choices"
fi
echo "  ok: web-inbound rejects conflicting default-owner choices"

# --- Gateway occurrence id reaches the engine idempotency seam ---------------
# The exact same accepted turn id is stable on a dispatcher refire, while a
# later occurrence with identical words gets a different opaque fingerprint.
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

# --- Small talk remains a valid dry-run decision ----------------------------
out="$(
    cd "$scratch"
    WG_DIR= "$review_wg" --json telegram web-inbound --dry-run \
        --default-owner "$COORD_ID" --sender "$HUMAN_NAME" \
        --message "lol ok sounds good" 2>&1 || true
)"
# This fixture has one confirmed human, so either a concierge response or
# silence is valid. The invariant here is only that dry-run reports a category.
expect_grep "dry-run always reports a category" "$out" '"category"'

echo "PASS: telegram_web_inbound"
