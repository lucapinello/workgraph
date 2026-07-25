#!/usr/bin/env bash
# Smoke: photo → shopping-list vision pipeline (task photo-to-shopping).
#
# "Snap the fridge, Bruno adjusts the pickup list." Pins the credential-free,
# network-free core of the pipeline by driving the REAL binary through
# `wg telegram photo-plan` (the scripted-test seam, sibling of `wg telegram
# decide` / `elect`):
#
#   0. ENGINE BINDING    — the binary exercised is the one the caller REQUESTED
#                          ($WG_BIN, honoured strictly, absolute path), proven
#                          by poisoning PATH with a `wg` that refuses to run:
#                          every assertion below is therefore about the build
#                          under review, not about whatever `wg` PATH holds.
#
#   1. PHOTO ROUTING     — a captioned fridge photo ("bruno what do we still
#                          need?") decodes as a photo and ELECTS Bruno by name,
#                          exactly like a text message with the same caption.
#   2. ALBUM COALESCING  — three photos sharing a media_group_id collapse to ONE
#                          vision turn (no loops on albums), gathering all frames.
#   3. VISION → MUTATION — the model's `SHOPPING_UPDATE: have=[…]; need=[…]` tail
#                          maps deterministically to gateway endpoint calls:
#                          cross off what we already have, add what's missing,
#                          leave already-listed items alone.
#   4. ROUTING RULES     — a non-photo update yields no photo turn; an
#                          uncaptioned (unaddressed) group photo falls to the
#                          concierge (Otto) under the SAME election rules as text
#                          — only a NAMED caption routes to Bruno.
#
# The fixture image (fridge_photo.png) is the payload a live vision turn would
# download-and-attach; here we assert it is a real, valid image so the fixture
# never silently rots. Nothing is downloaded and nothing is sent — this is a
# pure decision, so no live bot, no real tokens, no gateway.

set -euo pipefail
# Remember where we were invoked from BEFORE the cd, so a relative WG_BIN
# ("WG_BIN=target/debug/wg bash tests/smoke/...") still names the binary the
# caller meant and not something under tests/smoke/scenarios/.
smoke_cwd="$PWD"
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

# ── The engine under test ───────────────────────────────────────────
# This scenario drives the REAL binary, so WHICH binary runs is part of the
# assertion. It used to call bare `wg`, i.e. whatever PATH happened to hold:
#
#   * with WG_BIN set but no `wg` on PATH (`env PATH=/usr/bin:/bin WG_BIN=…`,
#     the shape task validation uses), `require_wg` emitted a loud SKIP —
#     exit 77, which does NOT block the gate. The build under review was
#     never exercised and the scenario could not go red: a false green.
#   * with a STALE `wg` on PATH (installed weeks ago, before `telegram
#     photo-plan` existed), the run exercised that stale binary instead and
#     died with an opaque non-zero — a false failure blamed on this change.
#
# So resolve the requested executable ONCE, explicitly, and invoke it by
# absolute path. WG_BIN wins and is honoured strictly: if it is set and not
# usable we FAIL loudly rather than quietly substituting another binary,
# because a silent substitution is precisely the bug above.
WG=""
if [ -n "${WG_BIN:-}" ]; then
    case "$WG_BIN" in
        /*) WG="$WG_BIN" ;;
        *)  WG="$smoke_cwd/$WG_BIN" ;;
    esac
    [ -x "$WG" ] || loud_fail "WG_BIN=$WG_BIN resolves to '$WG', which is not an executable engine — refusing to fall back to PATH, because a silent fallback is how a stale binary stands in for the build under test"
else
    # No explicit request: prefer the installed engine (what the smoke gate
    # has always exercised), then this repo's own build so a fresh clone that
    # ran `cargo build` but not `cargo install` still runs instead of skipping.
    repo_root="$(cd ../../.. && pwd)"
    for cand in \
        "$(command -v wg 2>/dev/null || true)" \
        "$repo_root/target/debug/wg" \
        "$repo_root/target/release/wg"
    do
        if [ -n "$cand" ] && [ -x "$cand" ]; then WG="$cand"; break; fi
    done
    [ -n "$WG" ] || loud_skip "MISSING WG BINARY" "no wg engine found: nothing on PATH and no target/{debug,release}/wg — run 'cargo build --bin wg' or set WG_BIN to the binary under test"
fi
echo "engine under test: $WG"

# A binary that predates task photo-to-shopping has no `telegram photo-plan`
# at all. Say so in one line instead of letting every assertion below fail on
# a usage error, which is how "stale binary" masqueraded as "broken pipeline".
"$WG" telegram photo-plan --help >/dev/null 2>&1 \
    || loud_fail "engine at $WG has no 'telegram photo-plan' subcommand — this build predates task photo-to-shopping; rebuild it (cargo build --bin wg) or point WG_BIN at a current engine"

fixtures="../fixtures"
img="$fixtures/fridge_photo.png"

# The fixture image must exist and be a real PNG (magic bytes) — a live vision
# turn attaches exactly this kind of file.
[ -f "$img" ] || loud_fail "fixture image missing: $img"
head -c 8 "$img" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$' \
    || loud_fail "fixture image is not a valid PNG: $img"

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg"

# Fixture: the Casa Pinello voices (Bruno is the food-savvy one).
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
username  = "bruno_casapinello_bot"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"
TOML

# A captioned fridge photo (largest size last, smaller first — like the wire).
cat >"$scratch/update.json" <<'JSON'
{"update_id":1,"message":{"message_id":58,"date":1720000000,"chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"caption":"bruno what do we still need?","photo":[{"file_id":"thumb","width":90,"height":60,"file_size":900},{"file_id":"biggest","width":1280,"height":720,"file_size":90000}]}}
JSON

# An album: three frames sharing one media_group_id; caption only on the first.
cat >"$scratch/album.json" <<'JSON'
[{"update_id":1,"message":{"message_id":58,"date":1720000000,"media_group_id":"AG9","chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"caption":"bruno what do we still need?","photo":[{"file_id":"f1","width":1280,"height":720,"file_size":90000}]}},
{"update_id":2,"message":{"message_id":59,"date":1720000001,"media_group_id":"AG9","chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"photo":[{"file_id":"f2","width":1280,"height":720,"file_size":90000}]}},
{"update_id":3,"message":{"message_id":60,"date":1720000002,"media_group_id":"AG9","chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"photo":[{"file_id":"f3","width":1280,"height":720,"file_size":90000}]}}]
JSON

# An uncaptioned group photo (no mention) — must not summon anyone.
cat >"$scratch/uncaptioned.json" <<'JSON'
{"update_id":1,"message":{"message_id":61,"date":1720000000,"chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"photo":[{"file_id":"x","width":1280,"height":720,"file_size":90000}]}}
JSON

# A plain text update — not a photo at all.
cat >"$scratch/text.json" <<'JSON'
{"update_id":1,"message":{"message_id":62,"date":1720000000,"chat":{"id":-1000000000001,"type":"supergroup"},"from":{"id":8905220378,"is_bot":false,"username":"luca"},"text":"hey bruno"}}
JSON

# The current shopping list, as GET /shopping.json would return it.
cat >"$scratch/list.json" <<'JSON'
{"ok":true,"groups":[{"store":"Market","items":[{"key":"p:market|chickpeas","text":"Chickpeas","checked":false},{"key":"p:market|lemons","text":"Lemons","checked":false}]}]}
JSON

# Absolute path, never `wg` — see the engine-resolution block above.
plan() { (cd "$scratch" && WG_DIR= "$WG" telegram photo-plan "$@" 2>&1); }

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    echo "  $desc"
    echo "$out" | grep -q "$needle" \
        || loud_fail "$desc: expected '$needle', got: $out"
}

echo "0. engine binding — the routing proof below runs on the REQUESTED binary:"
# Poison PATH with a `wg` that refuses to work. Every engine call in this
# scenario goes through "$WG" by absolute path, so the poison must never run.
# If it ever does, the routing/album/mutation assertions were about some other
# binary — the exact false-green/false-failure this scenario now pins.
mkdir -p "$scratch/poison"
cat >"$scratch/poison/wg" <<'SH'
#!/bin/sh
echo "PATH-RESOLVED-WG: the scenario must not resolve the engine from PATH" >&2
exit 66
SH
chmod +x "$scratch/poison/wg"
PATH="$scratch/poison:$PATH"
export PATH
[ "$(command -v wg 2>/dev/null || true)" = "$scratch/poison/wg" ] \
    || loud_fail "could not poison PATH — 'command -v wg' is '$(command -v wg 2>/dev/null || true)', so this differential would not prove anything"
# Identity-free on purpose: this step proves WHICH binary answers, not who is
# elected. PATH stays poisoned for the rest of the script, so every routing,
# album and mutation assertion below is likewise proven to run on "$WG".
bind_out="$(plan @update.json)"
expect_grep "PATH wg is poisoned yet the engine still answers" "$bind_out" "^photo — "
if echo "$bind_out" | grep -q "PATH-RESOLVED-WG"; then
    loud_fail "the scenario resolved the engine from PATH instead of '$WG': $bind_out"
fi

echo "1. photo routing — captioned fridge photo elects Bruno:"
out="$(plan @update.json)"
expect_grep "is a photo" "$out" "^photo — "
expect_grep "largest frame chosen" "$out" "1 image"
expect_grep "routes to bruno" "$out" "routes to: bruno"

echo "2. album coalescing — 3 frames → ONE turn, all frames gathered:"
out="$(plan @album.json)"
expect_grep "one turn" "$out" "1 turn(s), 3 image(s)"
expect_grep "album group" "$out" "album group AG9"
expect_grep "album caption carried" "$out" "bruno what do we still need?"

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

echo "4. routing rules — non-photo, and uncaptioned falls to the concierge:"
expect_grep "text is not a photo" "$(plan @text.json)" "not a photo"
# An unaddressed photo (no caption naming a voice) routes like unaddressed text:
# to Otto, the group concierge — NOT to Bruno (who is only summoned by name).
uncap_out="$(plan @uncaptioned.json)"
expect_grep "uncaptioned → concierge (otto)" "$uncap_out" "routes to: otto"
if echo "$uncap_out" | grep -q "routes to: bruno"; then
    loud_fail "an uncaptioned photo must not be name-routed to Bruno: $uncap_out"
fi

echo "5. no token ever leaks into the diagnostic output:"
if plan @update.json --reply "$reply" --list @list.json | grep -q "dummy-token"; then
    loud_fail "bot token leaked into photo-plan output"
fi

echo "PASS: photo→shopping (routing / album-coalesce / vision→toggle+add / restore / limits)"
