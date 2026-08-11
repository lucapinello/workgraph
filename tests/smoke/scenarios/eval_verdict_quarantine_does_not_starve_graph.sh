#!/usr/bin/env bash
# Scenario: eval_verdict_quarantine_does_not_starve_graph
#
# Regression (fix-the-graph, 2026-08-10): `load_durable_verdicts` was store-wide
# fail-closed — it bailed on the FIRST verdict file that failed verification, so
# ONE verdict whose evaluation evidence had been reaped by the daily gc returned
# `Err` for the whole store. The coordinator's `eval_evidence_usable` gate then
# skipped `reconcile_durable_verdicts` on every tick, and NO task could leave
# `pending-eval`. Measured live: 98 of 747 verdicts orphaned, 91 tasks parked,
# `wg ready` empty, the fail-closed line printed 16,764 times, and a family lost
# a whole week of planning to it.
#
# Two guards, both pinned here against the REAL binary (a unit test on the
# reconcile alone passes on the broken build — the defect was one directory over,
# in the loader, so this has to go through the actual `wg service tick` seam):
#   1. A starved tick is LOUD: 0 ready + a large cohort parked in one
#      non-dispatchable status prints a STARVE line naming the cohort and what it
#      holds down. The pre-existing DISPATCH WATCHDOG only fires when
#      tasks_ready > 0, so this exact shape was its blind spot for 1102 ticks.
#   2. The threshold is real: a SMALL parked cohort must NOT raise it. A detector
#      that always fires is the same as no detector.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch" || loud_fail "cannot cd to scratch $scratch"

# Spawning is disabled so this scenario can never launch a real agent: it is
# asserting a DIAGNOSTIC line, and an eval satellite scaffolded mid-run would
# both cost tokens and make the graph dispatchable, hiding the starve.
mkdir -p .wg
cat > .wg/config.toml <<'CFG'
[agency]
auto_evaluate = false
auto_assign = false
auto_place = false
auto_create = false
auto_triage = false
CFG

# ── Fixture: a big pending-eval cohort with open tasks stranded behind it.
# Written directly because no CLI verb parks a task in pending-eval, which is
# exactly the state the wedge lived in.
write_graph() {
    local cohort="$1"
    local children="$2"
    : > .wg/graph.jsonl
    local i
    for ((i = 0; i < cohort; i++)); do
        printf '{"kind":"task","id":"parked-%d","title":"parked %d","status":"pending-eval","priority":10,"created_at":"2026-08-01T00:00:00+00:00","log":[]}\n' \
            "$i" "$i" >> .wg/graph.jsonl
    done
    for ((i = 0; i < children; i++)); do
        printf '{"kind":"task","id":"child-%d","title":"child %d","status":"open","priority":10,"after":["parked-%d"],"created_at":"2026-08-01T00:00:00+00:00","log":[]}\n' \
            "$i" "$i" "$i" >> .wg/graph.jsonl
    done
}

tick() {
    wg --dir "$scratch/.wg" service tick --max-agents 8 2>&1
}

# ── Guard 1: a large parked cohort with nothing ready must be reported. ──────
write_graph 12 3
loud_out=$(tick) || loud_fail "tick failed on the starved graph:\n$loud_out"

grep -q "STARVE:" <<<"$loud_out" \
    || loud_fail "a tick with 0 ready, 15 unfinished and 12 tasks parked in one status MUST print a STARVE line — this silence is what cost a family a week:\n$loud_out"
grep -q "pending-eval" <<<"$loud_out" \
    || loud_fail "the STARVE line must NAME the parked cohort's status:\n$loud_out"
# The count of stranded dependents is the concrete cost of the starve; a line
# that omits it does not tell an operator whether to care.
grep -qE "3 open task\(s\) have ALL their predecessors" <<<"$loud_out" \
    || loud_fail "the STARVE line must count the open tasks held down by the cohort (expected 3):\n$loud_out"

# Nothing may have been spawned: this scenario asserts a log line, not dispatch.
grep -q "0 spawned" <<<"$loud_out" \
    || loud_fail "the starved tick must spawn nothing:\n$loud_out"

# ── Guard 2 (negative control): a SMALL cohort must stay quiet. Without this,
#    a detector hardcoded to always warn would pass Guard 1. ─────────────────
write_graph 3 1
quiet_out=$(tick) || loud_fail "tick failed on the small-cohort graph:\n$quiet_out"

grep -q "No ready tasks" <<<"$quiet_out" \
    || loud_fail "the small-cohort tick should still report that nothing is ready:\n$quiet_out"
if grep -q "STARVE:" <<<"$quiet_out"; then
    loud_fail "a 3-task parked cohort must NOT raise a STARVE alert — the threshold is the whole point of the detector:\n$quiet_out"
fi

echo "PASS: starved tick names its parked cohort (12 parked / 3 stranded); a 3-task cohort stays quiet"
exit 0
