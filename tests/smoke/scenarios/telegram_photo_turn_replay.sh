#!/usr/bin/env bash
# Real-binary replay gate for one photo-derived shopping mutation.
#
# Everything is scratch-local. The raw Telegram updates have the live Bot API
# shape, notify.toml uses the production multi-bot shape, and the shopping
# fixture matches GET /shopping.json. The hidden fixture seam runs production
# decoding, election, action planning, family-voice guarding, occurrence
# journaling, and delivery dedupe; only gateway mutations and Telegram sends
# are replaced by JSONL appenders.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"
repo_root="$(cd "$scenario_dir/../../.." && pwd)"

wg_bin="${WG_BIN:-}"
if [[ -z "$wg_bin" ]]; then
    wg_bin="$(command -v wg 2>/dev/null || true)"
elif [[ "$wg_bin" != /* ]]; then
    wg_bin="$repo_root/$wg_bin"
fi
[[ -n "$wg_bin" && -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable freshly built wg binary"

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg/agency/bindings"

helper_id="voice-anchor-731"
bot_id="wire-copper-731"
group_chat="-1007310042"
mutation_log="$scratch/mock-mutations.jsonl"
send_log="$scratch/mock-sends.jsonl"
image_fixture="$scenario_dir/../fixtures/fridge_photo.png"
[[ -f "$image_fixture" ]] || loud_fail "photo fixture is missing: $image_fixture"
head -c 8 "$image_fixture" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$' \
    || loud_fail "photo fixture is not a valid PNG: $image_fixture"

cat >"$scratch/household.toml" <<TOML
[[agent]]
id = "$helper_id"
name = "Copper Comet"
emoji = "🪐"
domains = ["shopping", "coordination"]
TOML

cat >"$scratch/.wg/notify.toml" <<TOML
[telegram]
chat_id = "$group_chat"

[telegram.bots.$bot_id]
bot_token = "0000000000:opaque-photo-fixture-token"
chat_id = "$group_chat"
username = "copper_comet_fixture_bot"
agent_id = "$helper_id"
TOML

cat >"$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
- telegram_user: "member-wire-41"
  agent_id: "human-anchor-41"
  name: "River Guest"
  bot_id: "wire-copper-731"
  confirmed: true
  created_at: "2026-07-25T00:00:00Z"
  confirmed_at: "2026-07-25T00:00:00Z"
YAML

# First frame of a live-shaped album. The caption elects the configured helper.
cat >"$scratch/album-first.json" <<'JSON'
{"update_id":4101,"message":{"message_id":58,"date":1721900000,"media_group_id":"album-occurrence-31","chat":{"id":-1007310042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_wire_41"},"caption":"Copper Comet, what do we still need?","photo":[{"file_id":"small-a","width":90,"height":60,"file_size":900},{"file_id":"large-a","width":1280,"height":720,"file_size":90000}]}}
JSON

# A later frame can be the first update seen after listener restart. Telegram
# omits the album caption here, but media_group_id still identifies the same
# physical photo turn.
cat >"$scratch/album-later-frame.json" <<'JSON'
{"update_id":4102,"message":{"message_id":59,"date":1721900001,"media_group_id":"album-occurrence-31","chat":{"id":-1007310042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_wire_41"},"photo":[{"file_id":"small-b","width":90,"height":60,"file_size":900},{"file_id":"large-b","width":1280,"height":720,"file_size":91000}]}}
JSON

# Identical household words in a later, distinct album must remain admissible.
cat >"$scratch/album-next.json" <<'JSON'
{"update_id":4201,"message":{"message_id":60,"date":1721900060,"media_group_id":"album-occurrence-32","chat":{"id":-1007310042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_wire_41"},"caption":"Copper Comet, what do we still need?","photo":[{"file_id":"small-c","width":90,"height":60,"file_size":900},{"file_id":"large-c","width":1280,"height":720,"file_size":92000}]}}
JSON

# Live gateway response shape: stable keys, display text, and crossed state.
cat >"$scratch/shopping.json" <<'JSON'
{"ok":true,"groups":[{"store":"Market","items":[{"key":"p:market|chickpeas","text":"Chickpeas","checked":false},{"key":"p:market|lemons","text":"Lemons","checked":false}]}]}
JSON

canonical_reply="I found chickpeas, so I crossed those off and added oat milk."
canonical_model_reply="$canonical_reply
SHOPPING_UPDATE: have=[chickpeas]; need=[lemons, oat milk]"
drifted_model_reply="This newly composed reply must never replace the stored one.
SHOPPING_UPDATE: have=[]; need=[black beans]"

run_turn() {
    local update_file="$1"
    local model_reply="$2"
    shift 2
    (
        cd "$scratch"
        WG_DIR= "$wg_bin" --json telegram photo-replay \
            "@$update_file" \
            --reply "$model_reply" \
            --list @shopping.json \
            --mock-image "$image_fixture" \
            --mock-mutation-log "$mutation_log" \
            --mock-send-log "$send_log" \
            "$@"
    )
}

row_count() {
    local path="$1"
    if [[ -f "$path" ]]; then
        wc -l <"$path" | tr -d ' '
    else
        echo 0
    fi
}

state_count() {
    local state="$1"
    local count=0
    local path
    while IFS= read -r path; do
        if grep -q "\"state\": \"$state\"" "$path"; then
            count=$((count + 1))
        fi
    done < <(find "$scratch/.wg/telegram-occurrences" -type f -name '*.json' 2>/dev/null)
    echo "$count"
}

echo "Round 1 — image-derived mutations apply, then the stubbed send fails:"
if first_out="$(run_turn album-first.json "$canonical_model_reply" --fail-send 2>&1)"; then
    loud_fail "first stubbed transport unexpectedly succeeded:
$first_out"
fi
[[ "$(row_count "$mutation_log")" == "2" ]] \
    || loud_fail "first album did not make exactly two planned gateway mutations"
grep -q '"op":"toggle"' "$mutation_log" \
    || loud_fail "photo verdict did not plan the live-shaped cross-off mutation"
grep -q '"checked":true' "$mutation_log" \
    || loud_fail "photo verdict did not cross off the item already in the image"
grep -q '"op":"add"' "$mutation_log" \
    || loud_fail "photo verdict did not call the missing-item add mutation"
grep -q '"text":"oat milk"' "$mutation_log" \
    || loud_fail "photo verdict did not plan the missing-item add mutation"
[[ "$(row_count "$send_log")" == "0" ]] \
    || loud_fail "failed stub transport must not record a delivered send"
[[ "$(state_count applied)" == "1" ]] \
    || loud_fail "failed delivery was not retained as one applied occurrence"
echo "  ok: each planned mutation ran once, zero delivered sends, canonical outcome retained"

echo "Round 2 — a later frame after restart resumes only stored delivery:"
retry_out="$(run_turn album-later-frame.json "$drifted_model_reply" 2>&1)" \
    || loud_fail "same-album delivery retry failed:
$retry_out"
[[ "$(row_count "$mutation_log")" == "2" ]] \
    || loud_fail "same album reapplied image-derived shopping mutations"
[[ "$(row_count "$send_log")" == "1" ]] \
    || loud_fail "same-album retry did not deliver exactly once"
grep -Fq "$canonical_reply" "$send_log" \
    || loud_fail "retry did not deliver the original canonical reply"
if grep -Fq "newly composed" "$send_log"; then
    loud_fail "retry delivered drifted recomposed copy instead of stored canonical bytes"
fi
echo "$retry_out" | grep -q '"resumed_delivery": true' \
    || loud_fail "retry did not report stored delivery resumption: $retry_out"
[[ "$(state_count delivered)" == "1" ]] \
    || loud_fail "same-album retry did not mark the occurrence delivered"
echo "  ok: no second mutation; original reply delivered once"

echo "Round 3 — completed same-album refire is silent:"
completed_out="$(run_turn album-first.json "$drifted_model_reply" 2>&1)" \
    || loud_fail "completed same-album refire failed:
$completed_out"
[[ "$(row_count "$mutation_log")" == "2" ]] \
    || loud_fail "completed refire mutated the list again"
[[ "$(row_count "$send_log")" == "1" ]] \
    || loud_fail "completed refire sent a second family reply"
echo "$completed_out" | grep -q '"already_delivered": true' \
    || loud_fail "completed refire did not report its durable no-op: $completed_out"
echo "  ok: durable no-op"

echo "Round 4 — a different album with identical words is admitted:"
later_out="$(run_turn album-next.json "$canonical_model_reply" 2>&1)" \
    || loud_fail "later distinct album failed:
$later_out"
[[ "$(row_count "$mutation_log")" == "4" ]] \
    || loud_fail "later distinct album was incorrectly suppressed"
[[ "$(row_count "$send_log")" == "2" ]] \
    || loud_fail "later distinct album did not deliver its reply"
[[ "$(state_count delivered)" == "2" ]] \
    || loud_fail "expected two distinct delivered occurrence records"
echo "  ok: second physical occurrence mutated and delivered once"

echo "Journal privacy — filenames are versioned digests, never fixture identity:"
journal_count=0
while IFS= read -r path; do
    journal_count=$((journal_count + 1))
    base="$(basename "$path")"
    [[ "$base" == b3-v1-*.json ]] \
        || loud_fail "occurrence journal filename is not a versioned digest: $base"
    if [[ "$base" == *"$helper_id"* || "$base" == *"album-occurrence"* ]]; then
        loud_fail "occurrence journal filename exposed fixture identity: $base"
    fi
done < <(find "$scratch/.wg/telegram-occurrences" -type f -name '*.json' 2>/dev/null)
[[ "$journal_count" == "2" ]] \
    || loud_fail "expected two opaque occurrence journals, got $journal_count"

if grep -Rqs "opaque-photo-fixture-token" \
    "$mutation_log" "$send_log" "$scratch/.wg/telegram-occurrences"; then
    loud_fail "fixture bot token leaked into replay artifacts"
fi

echo "PASS: photo turn replay safety (album restart / stored delivery / later occurrence)"
