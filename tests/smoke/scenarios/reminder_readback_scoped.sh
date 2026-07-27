#!/usr/bin/env bash
# Smoke: a persisted reminder can be READ BACK — from disk, scoped to the person
# asking, with zero writes (task reminder-readback-lane, live-cert C052).
#
# THE REGRESSION THIS PINS. A reminder could be filed by three paths and read
# back by none: the grounded block reads the weekly plan model and never the
# ad-hoc reminders, the fast lane treats every reminder read as a fallback, and
# only `remind --list` merged the two sources. So "what exact date and time is
# the reminder to call the dentist set for?" was composed with no reminder data
# in front of it — and simply agreed with whatever date the QUESTION carried.
#
# This drives the REAL binary through `wg telegram remind --ask "…" --as <member>`
# (the same merge + privacy filter + rendering the chat lane uses) and proves:
#   1. the poisoned prompt — the question asserts Aug 3, the store holds Jul 27,
#      and the answer MUST say Jul 27;
#   2. cross-member privacy — another member is told nothing, not even that such
#      a reminder exists;
#   3. zero matches → an honest "no reminder set about that", nothing invented;
#   4. ambiguity → the candidates, briefly, and which one did you mean;
#   5. ZERO WRITES — the reminder file is byte-identical after every read;
#   6. fail-closed — `--ask` without `--as` refuses rather than answering
#      unscoped (an unscoped answer is a disclosure).
# Credential-free: no notify.toml, nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
# A deployed binary older than this lane has no `--ask`. That FAILS rather than
# skipping: a stale install is the exact state this pins — the whole point of the
# lane is that the answer comes from the deployed engine, and a SKIP here would
# read green while the family's reminder question was still being answered by a
# composer with no reminder data in front of it.
# Captured, not piped: under `set -o pipefail` a `wg … | grep -q` races — grep
# exits on the first match, wg dies of SIGPIPE (141), and the pipeline reports
# failure even though the flag was found. That flapped this very check.
remind_help="$(wg telegram remind --help 2>&1 || true)"
case "$remind_help" in
    *--ask*) ;;
    *) loud_fail "stale wg: 'telegram remind' has no --ask flag — install the engine (cargo install --path . --force --locked)" ;;
esac

scratch="$(make_scratch)"
export WG_DIR="$scratch/.wg"
mkdir -p "$scratch/.wg/agency/bindings"

# Two confirmed humans: the reminder belongs to exactly one of them.
cat > "$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
  - telegram_user: "55501234"
    agent_id: member-one
    name: Household Member
    bot_id: otto
    confirmed: true
    created_at: "2026-07-01T00:00:00Z"
  - telegram_user: "55505678"
    agent_id: member-two
    name: Second Member
    bot_id: otto
    confirmed: true
    created_at: "2026-07-01T00:00:00Z"
YAML

remind() { (cd "$scratch" && WG_DIR="$scratch/.wg" wg telegram remind "$@"); }

echo "0. file a reminder for ONE member: Monday 09:00, asked on the Friday before"
out="$(remind --add 'remind me Monday at 9am to call the dentist' \
    --recipient 'Household Member' --now 2026-07-24T10:00)"
echo "   → $out"
store="$scratch/.casa/reminders-adhoc.json"
[ -f "$store" ] || loud_fail "ad-hoc store not written"
grep -q '"2026-07-27T09:00' "$store" \
    || loud_fail "fixture must really hold Jul 27 09:00: $(cat "$store")"
before="$(shasum "$store" | awk '{print $1}')"

echo "1. THE POISONED PROMPT: the question says Aug 3, the store says Jul 27"
out="$(remind --as 'Household Member' --now 2026-07-27T03:20 \
    --ask 'What exact date and time is the reminder to call the dentist set for — Monday, August 3, 2026 at 9:00 a.m.?')"
echo "   → $out"
echo "$out" | grep -q "Jul 27" || loud_fail "answer must carry the PERSISTED date: $out"
echo "$out" | grep -q "9:00 am" || loud_fail "answer must carry the exact time: $out"
echo "$out" | grep -qi "aug" && loud_fail "answer echoed the date the question asserted: $out"
echo "$out" | grep -qi "call the dentist" || loud_fail "answer must name the reminder: $out"

echo "2. PRIVACY: the other member is told nothing — not the date, not that it exists"
out="$(remind --as 'Second Member' --now 2026-07-27T03:20 \
    --ask 'What date and time is the reminder to call the dentist set for?')"
echo "   → $out"
[ "$out" = "You don't have a reminder set about that." ] \
    || loud_fail "cross-member read must be the honest empty line, got: $out"
echo "$out" | grep -qE "Jul 27|9:00|Household Member" \
    && loud_fail "one member's reminder leaked to another: $out"
out="$(remind --as 'Second Member' --now 2026-07-27T03:20 --ask 'Do I have any reminders?')"
[ "$out" = "You don't have any reminders set right now." ] \
    || loud_fail "a broad read must be scoped too, got: $out"

echo "3. ZERO MATCHES: honest, and nothing invented"
out="$(remind --as 'Household Member' --now 2026-07-27T03:20 \
    --ask 'What time is the reminder about the car service?')"
echo "   → $out"
[ "$out" = "You don't have a reminder set about that." ] \
    || loud_fail "expected the honest empty line, got: $out"

echo "4. AMBIGUITY: two candidates are listed briefly, and the family is asked"
remind --add 'remind me Tuesday at 10am to call the vet' \
    --recipient 'Household Member' --now 2026-07-24T10:00 >/dev/null
before="$(shasum "$store" | awk '{print $1}')"
out="$(remind --as 'Household Member' --now 2026-07-27T03:20 --ask 'When is my call reminder?')"
echo "   → $out"
echo "$out" | grep -q "2 reminders" || loud_fail "expected two candidates: $out"
echo "$out" | grep -qi "dentist" || loud_fail "first candidate missing: $out"
echo "$out" | grep -qi "vet" || loud_fail "second candidate missing: $out"
echo "$out" | grep -q "Which one did you mean?" || loud_fail "an ambiguous read must ask: $out"

echo "5. ZERO WRITES: the reminder file is byte-identical after every read"
after="$(shasum "$store" | awk '{print $1}')"
[ "$before" = "$after" ] || loud_fail "a read-back modified the reminder file"
# And a read is not a cancellation, even when the question carries the verb.
out="$(remind --as 'Household Member' --now 2026-07-27T03:20 \
    --ask 'Did you cancel my dentist reminder?')"
echo "   → $out"
echo "$out" | grep -q "Jul 27" || loud_fail "a question about a cancellation is a READ: $out"
grep -q "Call the dentist" "$store" || loud_fail "the read-back cancelled the reminder"
[ "$before" = "$(shasum "$store" | awk '{print $1}')" ] \
    || loud_fail "the cancel-shaped QUESTION mutated the store"

echo "6. FAIL-CLOSED: --ask without --as refuses rather than answering unscoped"
if remind --now 2026-07-27T03:20 --ask 'When is my dentist reminder?' >/dev/null 2>&1; then
    loud_fail "an unscoped --ask must refuse: there is no safe answer without a requester"
fi

echo "PASS: reminder read-back — persisted date beats the prompt's, scoped per member, zero writes"
