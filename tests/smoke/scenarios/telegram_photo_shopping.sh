#!/usr/bin/env bash
# Smoke: photo → shopping-list vision pipeline (task photo-to-shopping).
#
# "Snap the fridge, the configured cooking voice adjusts the pickup list." Pins
# the credential-free, network-free core of the pipeline by driving the REAL
# binary through
# `wg telegram photo-plan` (the scripted-test seam, sibling of `wg telegram
# decide` / `elect`):
#
#   1. PHOTO ROUTING     — a captioned fridge photo naming the configured
#                          cooking voice decodes as a photo and ELECTS that
#                          voice by name, exactly like a text message with the
#                          same caption.
#   2. ALBUM COALESCING  — three photos sharing a media_group_id collapse to ONE
#                          vision turn (no loops on albums), gathering all frames.
#   3. VISION → MUTATION — the model's `SHOPPING_UPDATE: have=[…]; need=[…]` tail
#                          maps deterministically to gateway endpoint calls:
#                          cross off what we already have, add what's missing,
#                          leave already-listed items alone.
#   4. ROUTING RULES     — a non-photo update yields no photo turn; an
#                          uncaptioned (unaddressed) group photo falls to the
#                          configured coordinator under the SAME election rules
#                          as text — only a NAMED caption routes to the cooking
#                          voice.
#
# The fixture image (fridge_photo.png) is the payload a live vision turn would
# download-and-attach; here we assert it is a real, valid image so the fixture
# never silently rots. Nothing is downloaded and nothing is sent — this is a
# pure decision, so no live bot, no real tokens, no gateway.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

repo_root="$(cd ../../.. && pwd)"
wg_bin="${WG_BIN:-}"
if [[ -z "$wg_bin" ]]; then
    wg_bin="$(command -v wg || true)"
elif [[ "$wg_bin" != /* ]]; then
    wg_bin="$repo_root/$wg_bin"
fi
[[ -n "$wg_bin" && -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable locally built wg binary"

fixtures="../fixtures"
img="$fixtures/fridge_photo.png"

# The fixture image must exist and be a real PNG (magic bytes) — a live vision
# turn attaches exactly this kind of file.
[ -f "$img" ] || loud_fail "fixture image missing: $img"
head -c 8 "$img" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$' \
    || loud_fail "fixture image is not a valid PNG: $img"

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# An unfamiliar roster proves both routes come from project configuration, not
# from compiled persona ids. The cooking and coordination domains are explicit.
cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "skillet-7"
name = "Saffron Skillet"
emoji = "🥘"
domains = ["cooking", "recipes"]

[[agent]]
id = "harbor-4"
name = "Harbor Guide"
emoji = "🧭"
domains = ["calendar", "coordination", "shopping"]
TOML

cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.skillet-7]
bot_token = "0000000000:skillet-fixture-token"
chat_id   = "-1007000042"
username  = "saffron_skillet_house_bot"
agent_id  = "skillet-7"

[telegram.bots.harbor-4]
bot_token = "0000000000:harbor-fixture-token"
chat_id   = "-1007000042"
username  = "harbor_guide_house_bot"
agent_id  = "harbor-4"
TOML

# A captioned fridge photo (largest size last, smaller first — like the wire).
cat >"$scratch/update.json" <<'JSON'
{"update_id":1,"message":{"message_id":58,"date":1720000000,"chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"caption":"Saffron Skillet, what do we still need?","photo":[{"file_id":"thumb","width":90,"height":60,"file_size":900},{"file_id":"biggest","width":1280,"height":720,"file_size":90000}]}}
JSON

# An album: three frames sharing one media_group_id; caption only on the first.
cat >"$scratch/album.json" <<'JSON'
[{"update_id":1,"message":{"message_id":58,"date":1720000000,"media_group_id":"AG9","chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"caption":"Saffron Skillet, what do we still need?","photo":[{"file_id":"f1","width":1280,"height":720,"file_size":90000}]}},
{"update_id":2,"message":{"message_id":59,"date":1720000001,"media_group_id":"AG9","chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"photo":[{"file_id":"f2","width":1280,"height":720,"file_size":90000}]}},
{"update_id":3,"message":{"message_id":60,"date":1720000002,"media_group_id":"AG9","chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"photo":[{"file_id":"f3","width":1280,"height":720,"file_size":90000}]}}]
JSON

# An uncaptioned group photo (no mention) — the coordinator handles it.
cat >"$scratch/uncaptioned.json" <<'JSON'
{"update_id":1,"message":{"message_id":61,"date":1720000000,"chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"photo":[{"file_id":"x","width":1280,"height":720,"file_size":90000}]}}
JSON

# A plain text update — not a photo at all.
cat >"$scratch/text.json" <<'JSON'
{"update_id":1,"message":{"message_id":62,"date":1720000000,"chat":{"id":-1007000042,"type":"supergroup"},"from":{"id":741020,"is_bot":false,"username":"member_41"},"text":"hello Saffron Skillet"}}
JSON

# The current shopping list, as GET /shopping.json would return it.
cat >"$scratch/list.json" <<'JSON'
{"ok":true,"groups":[{"store":"Market","items":[{"key":"p:market|chickpeas","text":"Chickpeas","checked":false},{"key":"p:market|lemons","text":"Lemons","checked":false}]}]}
JSON

plan() { (cd "$scratch" && WG_DIR= "$wg_bin" telegram photo-plan "$@" 2>&1); }

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    echo "  $desc"
    echo "$out" | grep -q "$needle" \
        || loud_fail "$desc: expected '$needle', got: $out"
}

echo "1. photo routing — captioned fridge photo elects the configured cooking voice:"
out="$(plan @update.json)"
expect_grep "is a photo" "$out" "^photo — "
expect_grep "largest frame chosen" "$out" "1 image"
expect_grep "routes to the cooking voice" "$out" "routes to: skillet-7"

echo "2. album coalescing — 3 frames → ONE turn, all frames gathered:"
out="$(plan @album.json)"
expect_grep "one turn" "$out" "1 turn(s), 3 image(s)"
expect_grep "album group" "$out" "album group AG9"
expect_grep "album caption carried" "$out" "Saffron Skillet, what do we still need?"

echo "3. vision → mutation — SHOPPING_UPDATE tail maps to endpoint calls:"
reply="You still need lemons and chard; you already have chickpeas — crossing them off.
SHOPPING_UPDATE: have=[chickpeas]; need=[lemons, chard]"
out="$(plan @update.json --reply "$reply" --list @list.json)"
expect_grep "cross off what we have" "$out" "cross off 'Chickpeas'"
expect_grep "add the missing item" "$out" "add 'chard'"
# 'lemons' is already on the list, uncrossed → NO redundant action.
if echo "$out" | grep -qi "add 'lemons'"; then
    loud_fail "must not re-add an item already on the list: $out"
fi
# The family-facing reply must NOT contain the machine marker.
if echo "$out" | grep -q "SHOPPING_UPDATE"; then
    loud_fail "machine marker leaked into the family reply: $out"
fi

echo "3b. restore a crossed-off item that is needed again:"
cat >"$scratch/list_crossed.json" <<'JSON'
{"ok":true,"groups":[{"store":"Market","items":[{"key":"p:market|milk","text":"Milk","checked":true}]}]}
JSON
out="$(plan @update.json --reply $'ok\nSHOPPING_UPDATE: have=[]; need=[milk]' --list @list_crossed.json)"
expect_grep "restore crossed item" "$out" "restore 'Milk'"

echo "4. routing rules — non-photo, and uncaptioned falls to the configured coordinator:"
expect_grep "text is not a photo" "$(plan @text.json)" "not a photo"
# An unaddressed photo (no caption naming a voice) routes like unaddressed text:
# to the configured coordinator — NOT to the cooking voice (which is summoned
# only by name).
uncap_out="$(plan @uncaptioned.json)"
expect_grep "uncaptioned → configured coordinator" "$uncap_out" "routes to: harbor-4"
if echo "$uncap_out" | grep -q "routes to: skillet-7"; then
    loud_fail "an uncaptioned photo must not be name-routed to the cooking voice: $uncap_out"
fi

echo "5. no token ever leaks into the diagnostic output:"
if plan @update.json --reply "$reply" --list @list.json | grep -q "fixture-token"; then
    loud_fail "bot token leaked into photo-plan output"
fi

echo "PASS: photo→shopping (routing / album-coalesce / vision→toggle+add / restore / limits)"
