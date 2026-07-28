#!/usr/bin/env bash
# Smoke: "next Monday" is the FOLLOWING Monday — the same-weekday-before-the-time
# regression, pinned against the real binary.
#
# The live-cert C052 probe, run at `--now 2026-07-27T03:20` (a Monday, with 09:00
# still five hours ahead), got the SAME answer for three phrasings that mean two
# different days:
#
#   "… on Monday, August 3, 2026 at 9:00 a.m."  → due 2026-07-27T09:00  ✗
#   "… next Monday at 9:00 a.m."                → due 2026-07-27T09:00  ✗
#   "… Monday at 9:00 a.m."                     → due 2026-07-27T09:00  ✓
#
# `resolve_day` resolved every named weekday modulo seven (today counts), threw
# "next" away as noise, and never parsed the civil date. Two of those three were
# filed for a morning five hours away instead of the week after.
#
# This drives the REAL binary through the same CLI seam the reviewer used —
# isolated scratch project, `--now` pin, `--json` so the due date is read as data,
# never as prose — and pins all three shapes at once. The bare-weekday case is the
# control: it must STAY on today, because today-if-the-time-is-ahead is correct.
# Credential-free: `--add` only registers to the ad-hoc store; nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
if ! wg telegram remind --help >/dev/null 2>&1; then
    loud_skip "STALE WG BINARY" "wg has no 'telegram remind' subcommand; rebuild from the fork"
fi

scratch="$(make_scratch)"
export WG_DIR="$scratch/.wg"
mkdir -p "$scratch/plans" "$scratch/.wg/agency/bindings"

cat > "$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
  - telegram_user: "55501234"
    agent_id: luca
    name: Luca
    bot_id: otto
    confirmed: true
    created_at: "2026-07-01T00:00:00Z"
YAML

# Monday 2026-07-27 at 03:20 — the exact pin from the proof.
NOW="2026-07-27T03:20"

# The reviewer's probe: register the ask and read the resolved `due` back as JSON.
due_for() {
    (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg --json telegram remind --add "$1" --recipient Luca --now "$NOW") \
        | tr ',' '\n' | grep '"due"' | sed 's/.*"due":"//; s/".*//'
}

echo "1. an explicit civil date is the date the family typed, not the weekday word:"
got="$(due_for 'Remind me to call the dentist on Monday, August 3, 2026 at 9:00 a.m.')"
[ "$got" = "2026-08-03T09:00" ] \
    || loud_fail "explicit date resolved to $got, expected 2026-08-03T09:00"
echo "   → $got"

echo "2. 'next Monday' is the STRICTLY following Monday, never this one:"
got="$(due_for 'Remind me to call the dentist next Monday at 9:00 a.m.')"
[ "$got" = "2026-08-03T09:00" ] \
    || loud_fail "'next Monday' resolved to $got, expected 2026-08-03T09:00"
echo "   → $got"

echo "3. CONTROL — a bare weekday still means today when the time is ahead:"
got="$(due_for 'Remind me to call the dentist Monday at 9:00 a.m.')"
[ "$got" = "2026-07-27T09:00" ] \
    || loud_fail "bare weekday resolved to $got, expected 2026-07-27T09:00 (today)"
echo "   → $got"

echo "4a. the SAME bare weekday, once its clock has passed, rolls a whole week:"
got="$( (cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg --json telegram remind --add 'Remind me to call the dentist Monday at 9:00 a.m.' \
       --recipient Luca --now 2026-07-27T09:30) | tr ',' '\n' | grep '"due"' | sed 's/.*"due":"//; s/".*//')"
[ "$got" = "2026-08-03T09:00" ] \
    || loud_fail "at 09:30 a bare Monday resolved to $got, expected 2026-08-03T09:00"
echo "   → $got"

echo "4b. a bare weekday whose time has PASSED still rolls a whole week:"
got="$( (cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg --json telegram remind --add 'Remind me to move the car Monday at 2:00 a.m.' \
       --recipient Luca --now "$NOW") | tr ',' '\n' | grep '"due"' | sed 's/.*"due":"//; s/".*//')"
[ "$got" = "2026-08-03T02:00" ] \
    || loud_fail "an elapsed bare Monday resolved to $got, expected 2026-08-03T02:00"
echo "   → $got"

echo "5. a detached meridiem belongs to the clock beside it (9:00 p.m. is 21:00):"
got="$(due_for 'Remind me to call the dentist next Monday at 9:00 p.m.')"
[ "$got" = "2026-08-03T21:00" ] \
    || loud_fail "'9:00 p.m.' resolved to $got, expected 2026-08-03T21:00"
echo "   → $got"

echo "6. a typed date the engine cannot honour is ASKED about, never filed:"
for bad in 'Remind me to call the dentist on Tuesday, August 3, 2026 at 9:00 a.m.' \
           'Remind me to call the dentist on July 4, 2026 at 9:00 a.m.'; do
    # August 3 2026 is a MONDAY, so the first contradicts itself; the second is gone.
    out="$( (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg --json telegram remind --add "$bad" --recipient Luca --now "$NOW") )"
    echo "$out" | grep -q '"registered":false' \
        || loud_fail "an unhonourable date was filed anyway: $out"
    echo "$out" | grep -q '"due"' \
        && loud_fail "an unhonourable date resolved to a due time: $out"
done
echo "   → both fell back to be asked about"

echo "7. the three phrasings did not collapse into one date:"
[ -f "$scratch/.casa/reminders-adhoc.json" ] || loud_fail "ad-hoc store not written"
grep -q '2026-08-03T09:00' "$scratch/.casa/reminders-adhoc.json" \
    || loud_fail "no reminder filed for 2026-08-03: $(cat "$scratch/.casa/reminders-adhoc.json")"
grep -q '2026-07-27T09:00' "$scratch/.casa/reminders-adhoc.json" \
    || loud_fail "no reminder filed for 2026-07-27: $(cat "$scratch/.casa/reminders-adhoc.json")"

echo "PASS: next-weekday-strict — 'next Monday' and an explicit date land the week after; a bare weekday still means today"
