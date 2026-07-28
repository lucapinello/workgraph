#!/usr/bin/env bash
# Smoke: the reminder contract at the EDGES — an elapsed civil instant, an
# impossible civil date, a dayless elapsed clock, a negated request, and the
# confirmation that names the date. Both parsers, real binary, one clock pin.
#
# The exact-tree audit of `wg/weekday-strict` (C052, 2026-07-27) confirmed the
# three-way weekday split was real and then reproduced five contract breaks that
# the committed gate could not see, because every semantic call in it was
# `wg telegram remind --add` — the DM parser, never the fast lane:
#
#   1. "on Monday, July 27, 2026 at 2:00 a.m.", typed at 03:20 that Monday,
#      registered a reminder eighty minutes IN THE PAST (both parsers checked
#      `date < today`, never the resolved instant);
#   2. "on Monday, February 30, 2027" — a date no calendar has — fell through to
#      the "Monday" beside it and filed for a day nobody named;
#   3. the fast lane rolled an elapsed clock only when a weekday was named, so a
#      DAYLESS "at 2:00 a.m." stayed on today while the DM path rolled it;
#   4. "do not remind me …" and "…this isn't a reminder request" both CREATED
#      reminders (the cancel vocabulary had "don't remind" but not the spelled
#      out form, and the trigger was the bare substring "remind");
#   5. the fast-lane confirmation dropped the resolved date, so three asks
#      meaning two different days read back identically as "Monday at 09:00".
#
# So this scenario drives the FULL set against the installed binary: the set
# forms, the readback (`--ask` / `--list`), the cancel, and every negative
# control — through BOTH seams, each pinned at `--now 2026-07-27T03:20`.
# Credential-free: `--add` writes only `.casa/reminders-adhoc.json`, and the fast
# lane writes only the scratch project's own plan file. Nothing is sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
if ! wg telegram remind --help >/dev/null 2>&1; then
    loud_skip "STALE WG BINARY" "wg has no 'telegram remind' subcommand; rebuild from the fork"
fi
# The fast-lane seam needs a FULL wall-clock pin (`--today` alone cannot express
# "03:20 on that Monday") and a calendar owner to file a row under. A binary
# without them predates this contract; say so loudly rather than passing vacuously.
# Captured into a variable first: `wg --help | grep -q` under `pipefail` dies of
# SIGPIPE on a match and reads as a stale binary when it is not.
shopping_help="$(wg telegram shopping --help 2>&1 || true)"
case "$shopping_help" in
    *"--calendar-owner"*) ;;
    *) loud_skip "STALE WG BINARY" \
        "'wg telegram shopping' has no --now/--calendar-owner; rebuild from the fork" ;;
esac

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

# The week the pin falls in: Monday 2026-07-27 → Sunday 2026-08-02.
cat > "$scratch/plans/2026-W31-family-plan.md" <<'MD'
# Family plan — 2026-W31

**Week of Monday 2026-07-27 → Sunday 2026-08-02**
**Status:** DRAFT

## 3. Calendar (Otto) — combined projection

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Mon 07-27 | 18:30 | Cook: chickpea & spinach curry | Bruno |
MD

# Monday 2026-07-27 at 03:20 — the exact pin from the audit.
NOW="2026-07-27T03:20"

dm() {
    (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg --json telegram remind --add "$1" --recipient Luca --now "${2:-$NOW}")
}
due_for() { dm "$1" "${2:-$NOW}" | tr ',' '\n' | grep '"due"' | sed 's/.*"due":"//; s/".*//'; }
refuses() {
    local out
    out="$(dm "$1" "${2:-$NOW}")"
    case "$out" in
        *'"registered":false'*) ;;
        *) loud_fail "the DM parser filed a request it must refuse: $1 → $out" ;;
    esac
    case "$out" in
        *'"due"'*) loud_fail "a refused request still resolved a due time: $1 → $out" ;;
    esac
}
fl() {
    (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg telegram shopping "$1" --now "${2:-$NOW}")
}

# ── 1. SET forms: the DM parser files each shape on its own date ─────────────

echo "1a. an explicit civil date is the date the family typed:"
got="$(due_for 'Remind me to call the dentist on Monday, August 3, 2026 at 9:00 a.m.')"
[ "$got" = "2026-08-03T09:00" ] || loud_fail "explicit date → $got, expected 2026-08-03T09:00"
echo "    → $got"

echo "1b. 'next Monday' is the STRICTLY following Monday:"
got="$(due_for 'Remind me to call the vet next Monday at 9:00 a.m.')"
[ "$got" = "2026-08-03T09:00" ] || loud_fail "'next Monday' → $got, expected 2026-08-03T09:00"
echo "    → $got"

echo "1c. CONTROL — a bare weekday still means today while its clock is ahead:"
got="$(due_for 'Remind me to water the plants Monday at 9:00 a.m.')"
[ "$got" = "2026-07-27T09:00" ] || loud_fail "bare weekday → $got, expected 2026-07-27T09:00"
echo "    → $got"

echo "1d. a DAYLESS elapsed clock rolls to tomorrow, never files at 02:00 today:"
got="$(due_for 'Remind me to move the car at 2:00 a.m.')"
[ "$got" = "2026-07-28T02:00" ] || loud_fail "dayless elapsed clock → $got, expected 2026-07-28T02:00"
echo "    → $got"

# ── 2. The audit's negative controls, DM parser ──────────────────────────────

echo "2a. a same-day civil date whose clock has ELAPSED is asked about, not filed:"
refuses 'Remind me to call the dentist on Monday, July 27, 2026 at 2:00 a.m.'
echo "    → refused"

echo "2b. an IMPOSSIBLE civil date never falls through to the weekday beside it:"
for bad in 'Remind me to call the vet on Monday, February 30, 2027 at 9:00 a.m.' \
           'Remind me to call the vet on 2027-02-30 at 9:00 a.m.' \
           'Remind me to call the vet on 2/30/2027 at 9:00 a.m.'; do
    refuses "$bad"
done
echo "    → all three refused"

echo "2c. a NEGATED request, and prose that merely mentions reminders, file nothing:"
refuses 'Do not remind me to call the dentist next Monday at 9:00 a.m.'
refuses "Next Monday at 9:00 a.m. is unrelated; this isn't a reminder request."
refuses 'Cancel the reminder about the dentist'
echo "    → all three refused"

echo "2d. CONTROL — the leap day is a REAL date and is still filed:"
got="$(due_for 'Remind me to renew the passport on February 29 at 9:00 a.m.')"
[ "$got" = "2028-02-29T09:00" ] || loud_fail "29 February → $got, expected 2028-02-29T09:00"
echo "    → $got"

# ── 3. READBACK: the store, and the answer the family gets back ──────────────

echo "3a. the four filed reminders are all readable, on four distinct dates:"
list="$( (cd "$scratch" && WG_DIR="$scratch/.wg" wg --json telegram remind --list --now "$NOW") )"
for want in 2026-08-03T09:00 2026-07-27T09:00 2026-07-28T02:00 2028-02-29T09:00; do
    case "$list" in
        *"$want"*) ;;
        *) loud_fail "readback is missing $want: $list" ;;
    esac
done
[ -f "$scratch/.casa/reminders-adhoc.json" ] || loud_fail "ad-hoc store not written"
echo "    → four dates present"

echo "3b. asking WHEN one is set for answers with its resolved date:"
answer="$( (cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg telegram remind --ask 'what date and time is the reminder to water the plants set for?' \
       --as Luca --now "$NOW") )"
case "$answer" in
    *"Jul 27"*|*"July 27"*) ;;
    *) loud_fail "the readback did not name the resolved date: $answer" ;;
esac
echo "    → $answer"

# ── 4. The SAME contract on the fast lane, through the real binary ───────────

echo "4a. the fast lane names the resolved DATE, so three asks do not collapse:"
explicit="$(fl 'remind me to call the dentist on Monday, August 3, 2026 at 9:00 a.m.')"
next="$(fl 'remind me to call the dentist next Monday at 9:00 a.m.')"
bare="$(fl 'remind me to call the dentist Monday at 9:00 a.m.')"
for out in "$explicit" "$next"; do
    case "$out" in
        *"Monday, August 3 at 09:00"*) ;;
        *) loud_fail "a fast-lane confirmation did not name 3 August: $out" ;;
    esac
done
case "$bare" in
    *"Monday, July 27 at 09:00"*) ;;
    *) loud_fail "the bare-weekday confirmation did not name today's date: $bare" ;;
esac
echo "    → the week-after asks and today's ask read back differently"

echo "4b. a dayless elapsed clock rolls on the fast lane too (parser parity):"
case "$(fl 'remind me to move the car at 2:00 a.m.')" in
    *"Tuesday, July 28 at 02:00"*) ;;
    *) loud_fail "the fast lane kept a dayless elapsed clock on today: $(fl 'remind me to move the car at 2:00 a.m.')" ;;
esac
echo "    → Tuesday, July 28 at 02:00"

echo "4c. every negative control falls back on the fast lane as well:"
for bad in 'remind me to call the dentist on Monday, July 27, 2026 at 2:00 a.m.' \
           'remind me to call the vet on Monday, February 30, 2027 at 9:00 a.m.' \
           'remind me to call the vet on 2027-02-30 at 9:00 a.m.' \
           "next Monday at 9:00 a.m. is unrelated; this isn't a reminder request."; do
    out="$(fl "$bad")"
    case "$out" in
        *"reminder-set"*) loud_fail "the fast lane wrote a reminder it must refuse: $bad → $out" ;;
    esac
done
# A negation is handled EXPLICITLY — never as a creation.
case "$(fl 'do not remind me to call the dentist next Monday at 9:00 a.m.')" in
    *"reminder-set"*) loud_fail "'do not remind me …' created a reminder on the fast lane" ;;
esac
echo "    → all refused"

# ── 5. The fast lane's real WRITE and its cancel, against the scratch plan ───

plan="$scratch/plans/2026-W31-family-plan.md"

echo "5a. the fast lane writes the resolved date into the plan's calendar:"
(cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg telegram shopping 'remind me to call the dentist next Monday at 9:00 a.m.' \
       --now "$NOW" --apply --root "$scratch" --calendar-owner Otto) >/dev/null
grep -q 'Mon 08-03 | 09:00 | ⏰ Reminder: call the dentist' "$plan" \
    || loud_fail "no 2026-08-03 reminder row in the plan:
$(cat "$plan")"
echo "    → | Mon 08-03 | 09:00 | ⏰ Reminder: call the dentist |"

echo "5b. a refused ask leaves that plan BYTE-IDENTICAL:"
before="$(cat "$plan")"
(cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg telegram shopping 'remind me to call the vet on Monday, February 30, 2027 at 9:00 a.m.' \
       --now "$NOW" --apply --root "$scratch" --calendar-owner Otto) >/dev/null
[ "$before" = "$(cat "$plan")" ] || loud_fail "an impossible date changed the plan:
$(diff <(printf '%s' "$before") "$plan" || true)"
echo "    → unchanged"

echo "5c. the cancel really removes the row it names:"
(cd "$scratch" && WG_DIR="$scratch/.wg" \
    wg telegram shopping 'cancel the reminder about the dentist' \
       --now "$NOW" --apply --root "$scratch" --calendar-owner Otto) >/dev/null
if grep -q 'Reminder: call the dentist' "$plan"; then
    loud_fail "the cancelled reminder is still in the plan:
$(cat "$plan")"
fi
echo "    → gone"

echo "PASS: reminder civil-clock + negation contract — an elapsed instant, an impossible date, a dayless clock and a negation are all refused, and both parsers name the date they resolved"
