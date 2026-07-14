#!/usr/bin/env bash
# Scenario: protected_cron_survives_cleanup_sweep
#
# Regression (re-arm-the): the production `daily-digest` 12:00 UTC cron was
# abandoned by a cleanup developer as "not real work" — friendly fire that
# silently killed the family's morning digest, and a following `wg gc` would
# have erased its schedule from the graph entirely. Production recurring tasks
# now carry a `protected` tag with two guards:
#   1. `wg abandon` REFUSES a protected task without an explicit `--force`
#      (with `--force` it succeeds and logs the override loudly).
#   2. `wg gc` never garbage-collects a protected task, even when terminal.
# This scenario pins both guards against a scratch graph.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch" || loud_fail "cannot cd to scratch $scratch"

wg init >/dev/null 2>&1 || loud_fail "wg init failed in $scratch"

# A protected production cron and an ordinary abandonable task as the control.
wg add "Daily digest" --id prot-digest --cron "0 0 12 * * *" >/dev/null 2>&1 \
    || loud_fail "wg add (cron) failed"
wg edit prot-digest --add-tag protected >/dev/null 2>&1 \
    || loud_fail "wg edit --add-tag protected failed"
wg add "Ordinary chore" --id plain-chore >/dev/null 2>&1 \
    || loud_fail "wg add (plain) failed"

# ── Guard 1a: abandon WITHOUT --force is refused, loudly, and does NOT change
#    the task's status. ────────────────────────────────────────────────────
if refuse_out=$(wg abandon prot-digest --reason "routine cleanup sweep" 2>&1); then
    loud_fail "abandon of a PROTECTED task without --force must FAIL, but it succeeded:\n$refuse_out"
fi
grep -qi "protected" <<<"$refuse_out" \
    || loud_fail "refusal message must name the PROTECTED guard:\n$refuse_out"

status_after_refusal=$(wg show prot-digest --json 2>/dev/null | grep -o '"status"[^,]*' | head -1)
grep -qiv "abandoned" <<<"$status_after_refusal" \
    || loud_fail "a refused abandon must leave the protected task un-abandoned, got: $status_after_refusal"

# ── Guard 1b: abandon WITH --force succeeds and logs the override. ──────────
force_out=$(wg abandon prot-digest --force --reason "genuinely retiring it" 2>&1) \
    || loud_fail "abandon --force of a protected task must succeed:\n$force_out"

# ── Guard 2: gc must SKIP the protected (now terminal) task while still
#    collecting the ordinary one. First abandon the control so gc has a
#    terminal task to actually collect. ───────────────────────────────────
wg abandon plain-chore --reason "no longer needed" >/dev/null 2>&1 \
    || loud_fail "abandon of the ordinary task failed"

wg gc >/dev/null 2>&1 || loud_fail "wg gc errored"

# The protected task must still be in the graph; the ordinary one must be gone.
wg show prot-digest >/dev/null 2>&1 \
    || loud_fail "PROTECTED task was garbage-collected — the exact regression re-arm-the fixes"
if wg show plain-chore >/dev/null 2>&1; then
    loud_fail "an ordinary terminal task should have been collected by gc, but it survived"
fi

echo "PASS: protected cron refuses abandon without --force, allows --force, and survives gc"
