#!/usr/bin/env bash
# Smoke: cancelling a reminder is ALL-OR-NOTHING ACROSS BOTH SURFACES.
#
# A family reminder lives in one of two places. One the family asked for in a DM
# is a row in `.casa/reminders-adhoc.json`; one that came off the week is a
# `⏰ Reminder` row in the plan's `## 3. Calendar`. "Cancel the reminder about the
# dentist" cannot know which — and might name one of each.
#
# The break this pins (task cross-surface-reminder, from the C052 engine-contract
# audit's "Cancellation caveat"): the fast lane cleared the matching AD-HOC
# reminder first and only afterwards went looking for the plan. So
#
#   * a cancel that matched on both surfaces removed one from EACH — two
#     reminders gone from one sentence, neither of them asked about;
#   * "is this ambiguous?" was answered once per surface instead of once per
#     turn, so two candidates could each look unique;
#   * and when the plan leg then could not run — the week held by another writer,
#     the lock unreadable, the plan file unreadable — the ad-hoc deletion had
#     ALREADY happened: the family was told nothing was saved while a reminder
#     they still wanted was gone.
#
# The contract now: survey both surfaces, remove EXACTLY ONE unambiguous target,
# otherwise remove NOTHING and ask. The surface that does not own the target is
# left byte-identical — not even resaved.
#
# Drives the real binary through both seams at one clock pin: `wg telegram remind
# --add` (the DM/ad-hoc writer) to seed, and `wg --json telegram shopping … --apply
# --root <scratch>` (the fast lane — the surface-spanning cancel itself).
# Credential-free: only the scratch project's own `plans/` and
# `.casa/reminders-adhoc.json` are written; nothing is ever sent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
if ! wg telegram remind --help >/dev/null 2>&1; then
    loud_skip "STALE WG BINARY" "wg has no 'telegram remind' subcommand; rebuild from the fork"
fi
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

# Monday 2026-07-27 at 08:00 — inside the W31 plan week (Mon 07-27 → Sun 08-02).
NOW="2026-07-27T08:00"
plan="$scratch/plans/2026-W31-family-plan.md"
adhoc="$scratch/.casa/reminders-adhoc.json"

# THE PLAN SURFACE: two pending reminder rows of its own. "dentist" is the word
# that also names an ad-hoc reminder below; "passport" belongs to the plan alone.
cat > "$plan" <<'MD'
# Family plan — 2026-W31

**Week of Monday 2026-07-27 → Sunday 2026-08-02**
**Status:** DRAFT

## 3. Calendar (Otto) — combined projection

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Mon 07-27 | 18:30 | Cook: chickpea & spinach curry | Bruno |
| Wed 07-29 | 09:00 | ⏰ Reminder: book the dentist | Otto |
| Thu 07-30 | 11:00 | ⏰ Reminder: renew the passport | Otto |
MD

dm() {
    (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg --json telegram remind --add "$1" --recipient Luca --now "$NOW")
}
# The fast lane, applying for real against the scratch project.
cancel() {
    (cd "$scratch" && WG_DIR="$scratch/.wg" \
        wg --json telegram shopping "$1" --now "$NOW" \
           --apply --root "$scratch" --calendar-owner Otto)
}
# WHAT ACTUALLY HAPPENED lives in the `applied` block. The top-level `lane` and
# `reply` are the classifier's dry-run preview — for a cancel that is the "Done —
# cancelled …" line it WOULD send if the write succeeded — so reading those would
# grade the classification and call every refusal a success.
applied_of() { printf '%s\n' "$1" | sed -n '/"applied":/,$p'; }
field_of() { applied_of "$2" | sed -n "s/.*\"$1\": *\"\\([^\"]*\\)\".*/\\1/p" | head -1; }
outcome_of() { field_of outcome "$1"; }
lane_of() { field_of lane "$1"; }
reply_of() { field_of reply "$1"; }

# THE AD-HOC SURFACE: "book the dentist" (the same words as the plan's Wednesday
# row) and "rotate the tyres" (ad-hoc alone).
dm 'Remind me to book the dentist on Friday at 9:00 a.m.' >/dev/null
dm 'Remind me to rotate the tyres on Saturday at 10:00 a.m.' >/dev/null
[ -f "$adhoc" ] || loud_fail "the ad-hoc store was not seeded"
grep -q 'dentist' "$adhoc" || loud_fail "the ad-hoc dentist reminder is missing: $(cat "$adhoc")"
grep -q 'tyres' "$adhoc" || loud_fail "the ad-hoc tyres reminder is missing: $(cat "$adhoc")"

# ── 1. GLOBAL AMBIGUITY: one match on each surface removes NOTHING ───────────

echo "1. one plan row + one ad-hoc row, both 'the dentist' → nothing goes, and it asks:"
plan_before="$(cat "$plan")"
adhoc_before="$(cat "$adhoc")"
out="$(cancel 'cancel the reminder about the dentist')"
got_outcome="$(outcome_of "$out")"
[ "$got_outcome" = "answered" ] \
    || loud_fail "a cancel matching BOTH surfaces must ask, not act: outcome=$got_outcome
$out"
got_lane="$(lane_of "$out")"
[ "$got_lane" = "reminder-cancel-ambiguous" ] \
    || loud_fail "expected the cross-surface ambiguity lane, got lane=$got_lane
$out"
got_reply="$(reply_of "$out")"
case "$got_reply" in
    *"more than one reminder"*) ;;
    *) loud_fail "the ask must say why nothing was cancelled: $out" ;;
esac
case "$got_reply" in
    Done*) loud_fail "nothing was cancelled, so nothing is 'Done': $out" ;;
esac
[ "$plan_before" = "$(cat "$plan")" ] || loud_fail "an ambiguous cancel changed the plan:
$(diff <(printf '%s\n' "$plan_before") "$plan" || true)"
[ "$adhoc_before" = "$(cat "$adhoc")" ] || loud_fail "an ambiguous cancel changed the ad-hoc reminders:
$(diff <(printf '%s\n' "$adhoc_before") "$adhoc" || true)"
echo "    → answered ($got_lane); both surfaces byte-identical"

# ── 2. EXACTLY ONE TARGET: it goes, and the other surface is untouched ───────

echo "2a. the sole match is a PLAN row → the row goes, the ad-hoc store is byte-identical:"
adhoc_before="$(cat "$adhoc")"
out="$(cancel 'cancel the reminder about the passport')"
got_outcome="$(outcome_of "$out")"
[ "$got_outcome" = "applied" ] || loud_fail "the single plan match must be removed: outcome=$got_outcome
$out"
if grep -q 'renew the passport' "$plan"; then
    loud_fail "the cancelled plan row is still there:
$(cat "$plan")"
fi
grep -q 'book the dentist' "$plan" || loud_fail "only the named row may go:
$(cat "$plan")"
[ "$adhoc_before" = "$(cat "$adhoc")" ] || loud_fail "a plan cancel rewrote the ad-hoc reminders:
$(diff <(printf '%s\n' "$adhoc_before") "$adhoc" || true)"
echo "    → the passport row is gone; the ad-hoc store never moved"

echo "2b. the sole match is an AD-HOC reminder → it goes, the plan is byte-identical:"
plan_before="$(cat "$plan")"
out="$(cancel 'cancel the reminder about the tyres')"
got_outcome="$(outcome_of "$out")"
[ "$got_outcome" = "applied" ] || loud_fail "the single ad-hoc match must be removed: outcome=$got_outcome
$out"
if grep -q 'tyres' "$adhoc"; then
    loud_fail "the cancelled ad-hoc reminder is still on file: $(cat "$adhoc")"
fi
[ "$plan_before" = "$(cat "$plan")" ] || loud_fail "an ad-hoc cancel rewrote the plan:
$(diff <(printf '%s\n' "$plan_before") "$plan" || true)"
echo "    → the tyres reminder is gone; the plan never moved"

# ── 3. A PLAN LEG THAT CANNOT RUN LEAVES THE AD-HOC ROW IN PLACE ─────────────
#
# The week-mutation lock is the plan leg's first step. An unattributable record
# there is refused (docs/42 §3, §5: fail closed, never reclaimed), so the turn
# cannot survey or edit the plan at all. Under the old order the ad-hoc dentist
# reminder had already been deleted by this point and the family was told nothing
# was saved — a half-applied cancel. Nothing may go now.

echo "3. with the week's lock unreadable, a cancel saves nothing on EITHER surface:"
mkdir -p "$scratch/.casa/locks"
printf 'not a lock record\n' > "$scratch/.casa/locks/week-mutation.lock"
plan_before="$(cat "$plan")"
adhoc_before="$(cat "$adhoc")"
out="$(cancel 'cancel the reminder about the dentist')"
got_outcome="$(outcome_of "$out")"
[ "$got_outcome" != "applied" ] \
    || loud_fail "a cancel reported success while the week could not be locked: $out"
grep -q 'dentist' "$adhoc" \
    || loud_fail "the ad-hoc reminder was deleted by a cancel whose plan leg never ran: $(cat "$adhoc")"
[ "$adhoc_before" = "$(cat "$adhoc")" ] || loud_fail "the ad-hoc store moved under a refused cancel:
$(diff <(printf '%s\n' "$adhoc_before") "$adhoc" || true)"
[ "$plan_before" = "$(cat "$plan")" ] || loud_fail "the plan moved under a refused cancel:
$(diff <(printf '%s\n' "$plan_before") "$plan" || true)"
echo "    → outcome=$got_outcome; the dentist reminder is still on file, both files byte-identical"

echo "4. CONTROL — with the lock cleared the same words are ambiguous again, and still nothing goes:"
rm -f "$scratch/.casa/locks/week-mutation.lock"
out="$(cancel 'cancel the reminder about the dentist')"
[ "$(lane_of "$out")" = "reminder-cancel-ambiguous" ] \
    || loud_fail "the refusal was the lock, so clearing it must return the ambiguity ask: $out"
grep -q 'dentist' "$adhoc" || loud_fail "the ad-hoc reminder went on the ambiguous retry: $(cat "$adhoc")"
grep -q 'book the dentist' "$plan" || loud_fail "the plan row went on the ambiguous retry:
$(cat "$plan")"

echo "5. CONTROL — once the plan's twin is gone, that same cancel finally acts:"
# The family takes the plan row out by hand (the editorial path), leaving one
# candidate anywhere. The identical sentence now removes the ad-hoc reminder.
grep -v 'Reminder: book the dentist' "$plan" > "$plan.tmp" && mv "$plan.tmp" "$plan"
out="$(cancel 'cancel the reminder about the dentist')"
[ "$(outcome_of "$out")" = "applied" ] \
    || loud_fail "the sole remaining candidate must be cancelled: $out"
if grep -q 'dentist' "$adhoc"; then
    loud_fail "the ad-hoc dentist reminder survived an unambiguous cancel: $(cat "$adhoc")"
fi
echo "    → applied, and the ad-hoc store is empty of it"

echo "PASS: a reminder cancel is decided across both surfaces before either is touched — one unambiguous target goes, otherwise none, and a plan leg that cannot run leaves the ad-hoc reminder exactly where it was"
