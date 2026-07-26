#!/usr/bin/env bash
# Smoke: the DEFAULT-CONTACT BOUNDARY — a missing, invalid or ambiguous main point of
# contact elects NOBODY, and the casa gateway's declaration is binding.
#
# Pins task p1-default-contact-engine-boundary.
#
# THE BUG. "Everything nobody owns" — a greeting, a bare thanks, a general question —
# belongs to the household's MAIN POINT OF CONTACT. Two processes decided who that is:
# the casa gateway (which serves the kiosk/phone surfaces) and THIS engine (which runs
# the real election + composer). They derived it INDEPENDENTLY from the same
# household.toml, with different chains:
#
#   gateway : explicit `[household] point_of_contact` → the ONE coordination-marked
#             helper → a one-helper household → else NOBODY (it answers as the house)
#   engine  : `owner_for_domain(Coordination)` — "the FIRST roster member listing
#             `coordination`", and the explicit key was never read at all
#
# So a household whose choice was MISSING (nobody marked), AMBIGUOUS (two claimants) or
# INVALID (a key naming nobody) got TWO different answers depending on where the family
# typed: the house in the kiosk, and an undesignated helper's face on Telegram — a
# designation the family never made, indistinguishable in the feed from one they did.
#
# THE FIX, proven here: this engine resolves the point of contact through the SAME chain,
# fails closed when it cannot (silence with reason `no-default-contact`, never a guess),
# and HONOURS the gateway's explicit `WG_POINT_OF_CONTACT` declaration — including its
# refusal (the `(none)` sentinel).
#
# WHY A SCENARIO AND NOT A GREP. A grep proves a symbol moved; it cannot prove that a
# household the code has never heard of gets the right answer. So every leg runs the REAL
# binary against an OPAQUE roster (ids, authored names and bot handles that ship nowhere,
# and no id containing a domain word), through `--dry-run --json` — the credential-free
# seam that runs the real election and reports WHO would answer while sending NOTHING.
# No token, no network, no message: everything lives in a throwaway scratch dir.
#
# SKIPs (77) when no engine binary carrying this contract can be found or built.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"
repo_root="$(cd "$scenario_dir/../../.." && pwd)"

# --- The composed cast ------------------------------------------------------
# Two helpers, both opaque. `domains` (and the `role` line) are the ONLY ownership
# signals, so nothing below can pass by the engine recognising a shipped id.
COOK_ID="kiln-2"
COOK_NAME="Copper Ladle"
DAY_ID="relay-4"
DAY_NAME="Open Door"

HUMAN_NAME="River Guest"
HUMAN_TELEGRAM="7700415529"
GROUP_CHAT="-1000000000001"

UNOWNED_ASK="can someone take a look at this when you get a chance"
DOMAIN_ASK="what should we cook for dinner tomorrow?"

# Write a scratch project: a confirmed human (so a single-human household group is
# answered rather than protected as chatter), a household.toml built from the two
# arguments, and matching bots for both opaque ids.
#
#   new_household <wg-binary> <cook-domains-toml> <day-domains-toml> [point_of_contact]
#
# Echoes the scratch path.
new_household() {
    local wgbin="$1" cook_domains="$2" day_domains="$3" poc="${4:-}"
    local scratch
    scratch="$(make_scratch)"
    (
        cd "$scratch"
        export WG_DIR=
        "$wgbin" init >/dev/null 2>&1
        "$wgbin" agency human add "$HUMAN_NAME" --telegram "$HUMAN_TELEGRAM" >/dev/null 2>&1
        "$wgbin" agency human confirm "$HUMAN_TELEGRAM" >/dev/null 2>&1
    )
    {
        echo '[household]'
        echo 'name = "Casa Opaque"'
        if [[ -n "$poc" ]]; then
            echo "point_of_contact = \"$poc\""
        fi
        cat <<TOML

[[agent]]
id = "$COOK_ID"
name = "$COOK_NAME"
emoji = "🥄"
role = "the kitchen"
domains = [$cook_domains]

[[agent]]
id = "$DAY_ID"
name = "$DAY_NAME"
emoji = "🏡"
role = "the day"
domains = [$day_domains]
TOML
    } >"$scratch/household.toml"

    cat >"$scratch/.wg/notify.toml" <<TOML
[telegram]
chat_id = "$GROUP_CHAT"

[telegram.bots.$COOK_ID]
bot_token = "0000000000:kiln-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$COOK_ID"
username  = "copper_ladle_house_bot"

[telegram.bots.$DAY_ID]
bot_token = "0000000000:relay-fixture-token"
chat_id   = "$GROUP_CHAT"
agent_id  = "$DAY_ID"
username  = "open_door_house_bot"
TOML
    echo "$scratch"
}

# Run one web-inbound dry-run in an existing scratch. `$2` is the WG_POINT_OF_CONTACT
# declaration: the literal string "-" means the var is NOT set at all (an older gateway,
# or the listener path), which is a DIFFERENT input from declaring "(none)".
web() {
    local scratch="$1" declaration="$2" message="$3"
    if [[ "$declaration" == "-" ]]; then
        (cd "$scratch" && WG_DIR= "$WG_ENGINE" --json telegram web-inbound --dry-run \
            --sender "$HUMAN_NAME" --message "$message" 2>&1)
    else
        (cd "$scratch" && WG_DIR= WG_POINT_OF_CONTACT="$declaration" "$WG_ENGINE" --json \
            telegram web-inbound --dry-run --sender "$HUMAN_NAME" --message "$message" 2>&1)
    fi
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

refute_grep() {
    local desc="$1" out="$2" needle="$3"
    if grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — must NOT contain: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

# --- Engine discovery: a binary that CARRIES this contract -------------------
# Smoke scenarios shell out to `wg`, and the `wg` on PATH is routinely months old — it
# would answer every leg below from the pre-fix code path and report a FAIL that says
# nothing about this working tree. So each candidate is PROBED (does its dry-run JSON
# report the boundary at all?) and, when none does, the tree is built once.
advertises_contract() {
    local candidate="$1" probe out
    [[ -n "$candidate" && -x "$candidate" ]] || return 1
    probe="$(new_household "$candidate" '"cooking", "recipes", "meals"' '"coordination", "calendar"' 2>/dev/null)" || return 1
    out="$(cd "$probe" && WG_DIR= "$candidate" --json telegram web-inbound --dry-run \
        --sender "$HUMAN_NAME" --message "hello" 2>&1 || true)"
    grep -q '"point_of_contact_source"' <<<"$out"
}

WG_ENGINE=""
for candidate in "${WG_BIN:-}" "$repo_root/target/debug/wg" "$repo_root/target/release/wg" \
    "$(command -v wg 2>/dev/null || true)"; do
    [[ -n "$candidate" ]] || continue
    [[ "$candidate" == /* ]] || candidate="$repo_root/$candidate"
    if advertises_contract "$candidate"; then
        WG_ENGINE="$candidate"
        break
    fi
done

if [[ -z "$WG_ENGINE" ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
        loud_skip "NO ENGINE CARRYING THIS CONTRACT" \
            "no wg binary reports the default-contact boundary and cargo is unavailable to build one; set WG_BIN"
    fi
    echo "  building the engine (no candidate binary reports the boundary)…"
    # CARGO_TARGET_DIR is unset deliberately: an inherited target dir (agent worktrees
    # export one) leaves this repo's target/debug/wg STALE while reporting success.
    if ! (cd "$repo_root" && env -u CARGO_TARGET_DIR cargo build --bin wg >/dev/null 2>&1); then
        loud_skip "ENGINE BUILD FAILED" "cargo build --bin wg failed; set WG_BIN to a prebuilt executable"
    fi
    if advertises_contract "$repo_root/target/debug/wg"; then
        WG_ENGINE="$repo_root/target/debug/wg"
    else
        loud_fail "the freshly built engine does not report the default-contact boundary — \
the dry-run JSON is missing point_of_contact_source"
    fi
fi
echo "  engine: $WG_ENGINE"

# ── (A) AMBIGUOUS: two coordination claimants → nobody is elected ────────────
# The regression case. Author order is fixed so a first-wins revert cannot hide: the old
# code elected $COOK_ID here, because it declares `coordination` first.
ambiguous="$(new_household "$WG_ENGINE" '"coordination", "cooking", "recipes", "meals"' '"coordination", "calendar"')"
out="$(web "$ambiguous" "-" "$UNOWNED_ASK")"
expect_grep "an ambiguous household elects nobody for an unowned ask" "$out" '"category": "silence"'
expect_grep "…and says WHY, actionably" "$out" '"silence_reason": "no-default-contact (ambiguous-coordination)"'
expect_grep "…reporting no designated contact" "$out" '"point_of_contact": null'
expect_grep "…and how that was resolved" "$out" '"point_of_contact_source": "unresolved"'
refute_grep "no undesignated helper is named as the answering voice" "$out" "\"who\": \"$COOK_ID\""
refute_grep "…nor the second claimant" "$out" "\"who\": \"$DAY_ID\""

# ── (B) A DOMAIN ask under the SAME household still answers ──────────────────
# Failing closed is scoped to the UNOWNED default. A household that forgot ONE key must
# not be muted: the helper that DECLARED the kitchen still owns a food ask.
out="$(web "$ambiguous" "-" "$DOMAIN_ASK")"
expect_grep "a declared domain still answers under an undesignated household" "$out" '"category": "single-voice"'
expect_grep "…in the voice that declared that domain" "$out" "\"who\": \"$COOK_ID\""

# ── (C) INVALID: an explicit key naming nobody → nobody is elected ───────────
# A typo, or a rename that never landed. The old code ignored this key entirely and fell
# through to the coordination marker, so the mistake was invisible.
typo="$(new_household "$WG_ENGINE" '"cooking", "recipes", "meals"' '"coordination", "calendar"' "ghost-7")"
out="$(web "$typo" "-" "$UNOWNED_ASK")"
expect_grep "a point_of_contact naming nobody elects nobody" "$out" '"category": "silence"'
expect_grep "…named as exactly that mistake" "$out" '"silence_reason": "no-default-contact (explicit-names-nobody)"'
refute_grep "the coordination marker does NOT silently stand in for the typo" "$out" "\"who\": \"$DAY_ID\""

# ── (D) The EXPLICIT key is honoured, and OUTRANKS author order ──────────────
# The split-brain: the gateway obeys this key, the engine used to ignore it. Here the key
# names the COOK while the DAY helper carries the coordination marker — so a pass proves
# the key won, not that the marker did.
explicit="$(new_household "$WG_ENGINE" '"cooking", "recipes", "meals"' '"coordination", "calendar"' "$COOK_ID")"
out="$(web "$explicit" "-" "$UNOWNED_ASK")"
expect_grep "an explicit point_of_contact answers the unowned ask" "$out" '"category": "single-voice"'
expect_grep "…the helper the household NAMED" "$out" "\"who\": \"$COOK_ID\""
expect_grep "…resolved from the explicit key" "$out" '"point_of_contact_source": "explicit"'
refute_grep "the coordination marker does not outrank the explicit key" "$out" "\"who\": \"$DAY_ID\""

# ── (E) UNDESIGNATED: markers on nobody → nobody is elected ─────────────────
undesignated="$(new_household "$WG_ENGINE" '"cooking", "recipes"' '"workouts"')"
out="$(web "$undesignated" "-" "$UNOWNED_ASK")"
expect_grep "a multi-helper household that marked nobody elects nobody" "$out" '"category": "silence"'
expect_grep "…named as the missing designation" "$out" '"silence_reason": "no-default-contact (undesignated)"'

# ── (F) A SOLE coordination marker still designates (the refusal is not universal) ──
designated="$(new_household "$WG_ENGINE" '"cooking", "recipes", "meals"' '"coordination", "calendar"')"
out="$(web "$designated" "-" "$UNOWNED_ASK")"
expect_grep "one marker designates one contact" "$out" '"category": "single-voice"'
expect_grep "…the marked helper" "$out" "\"who\": \"$DAY_ID\""
expect_grep "…resolved from the coordinator marker" "$out" '"point_of_contact_source": "coordinator"'

# ── (G) THE GATEWAY'S DECLARATION IS BINDING — including its refusal ────────
# `(none)` on a household that WOULD designate: the boundary is what fails closed, so a
# gateway that resolved nobody cannot be overruled by this side's own derivation.
out="$(web "$designated" "(none)" "$UNOWNED_ASK")"
expect_grep "a declared refusal silences a household that would otherwise designate" "$out" '"category": "silence"'
expect_grep "…named as the declaration it obeyed" "$out" '"silence_reason": "no-default-contact (declared-nobody)"'
refute_grep "the marked helper is NOT elected against the declaration" "$out" "\"who\": \"$DAY_ID\""

# A declared HELPER wins over author order on the ambiguous household — the two sides can
# no longer disagree about who speaks for the house.
out="$(web "$ambiguous" "$DAY_ID" "$UNOWNED_ASK")"
expect_grep "a declared helper answers the unowned ask" "$out" '"category": "single-voice"'
expect_grep "…the declared one, not the first in author order" "$out" "\"who\": \"$DAY_ID\""
expect_grep "…marked as coming from the boundary" "$out" '"point_of_contact_source": "declared"'
refute_grep "author order does not win over the declaration" "$out" "\"who\": \"$COOK_ID\""

# A declaration naming a helper THIS side's roster does not have is config drift between
# the two processes. Guessing here would hide the drift.
out="$(web "$designated" "someone-else-9" "$UNOWNED_ASK")"
expect_grep "a declaration naming nobody on the roster fails closed" "$out" '"category": "silence"'
expect_grep "…named as drift, not as a missing choice" "$out" '"silence_reason": "no-default-contact (declared-names-nobody)"'

# An UNSET declaration is not a declared refusal: this side derives for itself, so an
# older gateway (and the Telegram listener path) keeps working.
out="$(web "$designated" "" "$UNOWNED_ASK")"
expect_grep "an empty declaration means 'derive for yourself', not 'nobody'" "$out" '"category": "single-voice"'
expect_grep "…so the marked helper still answers" "$out" "\"who\": \"$DAY_ID\""

# ── (H) A helper NAMED by the family is answered whatever the default is ────
# The refusal must never mute a direct address — the family named someone, so no default
# is needed at all.
out="$(web "$ambiguous" "(none)" "$DAY_NAME, can you take that one?")"
expect_grep "an addressed helper answers under a refusing boundary" "$out" '"category": "single-voice"'
expect_grep "…the helper the family named" "$out" "\"who\": \"$DAY_ID\""

echo "PASS: telegram_web_inbound_default_contact"
