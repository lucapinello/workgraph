#!/usr/bin/env bash
# Smoke: A WEEK-CODED FILE UNDER plans/ IS NOT A PLAN OF RECORD.
#
# Pins task sidecar-is-not.
#
# THE INCIDENT, TWICE. `plans/<week>-dinner-suggestions.md` — the side channel the
# family's dinner choices are parked in — carries a week code in its filename, so the
# engine read it AS a plan and drafted a week titled "Dinner suggestions for the week
# of …" (task week-start-engine, pinned by telegram_week_start.sh). That fix landed at
# two points. The SAME corruption class was still open one lane over, in the glob every
# OTHER engine reader goes through: `family_plan::load_plans` had never heard of the
# gateway's richer rule, and `current_plan()` falls back to "the most recent plan by
# week code" — so a REVIEW or a COMPANION file for the current week came back as the
# household's plan of record with no dinners in it.
#
# WHAT THIS PROVES, THROUGH THE DEPLOYED BINARY, ON BYTES:
#
#   · a `<week>-nora-review.md` (prose "## Dinners", the week's date range in its
#     header, zero meal rows) is NOT this week's plan: asked to start the week, the
#     house DRAFTS it, instead of answering "already planned" and pointing at a review;
#   · the same for every other editorial role — `.draft`, `-notes`, `-summary`,
#     `-scratch`, `-wip`, `-check-in` — and for the parked note;
#   · a `-workouts` COMPANION is not excluded (flow 35 depends on it contributing its
#     section) but it is not a plan of record either: the week still drafts, and the
#     companion file is byte-identical afterwards;
#   · and none of this weakened the real guard — a genuine `<week>-family-plan.md` with
#     dinners in it still answers "already planned" and is never overwritten.
#
# Every non-plan file is asserted BYTE-IDENTICAL after the run: the fix must keep these
# files out of the plan lane, not start rewriting them.
#
# A binary without the fix FAILS here rather than skipping — "the installed binary
# predates the fix" is exactly the state this scenario exists to catch. SKIP (77) only
# when no wg binary can be found at all.
#
# No token, no network, no bot, no model, no live plan.

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
    || loud_skip "MISSING WG BINARY" "set WG_BIN to an executable freshly built wg binary"
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING PYTHON3" "python3 is needed for the JSON assertions"

# Monday of an un-planned week — the window the week-start ask is made in.
now="2026-07-27"
this_code="2026-W31"
ask="Please draft this week's family plan — start the week."

# DOES THE DEPLOYED BINARY CARRY THE SEAM AT ALL? Probed by RUNNING it, never by
# `--help`: clap answers `--help` for an unknown subcommand by printing its parent's
# help and exiting 0, so a help probe passes on a binary that predates the lane.
if ! "$wg_bin" --json telegram week-start "$ask" --now "$now" >/dev/null 2>&1; then
    loud_fail "the deployed binary has no working 'wg telegram week-start' seam — it predates the week-start engine lane (cargo install --path . --force --locked)"
fi

sha_of() {
    python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

assert_outcome="$(mktemp)"
cat >"$assert_outcome" <<'PY'
import json, sys
want = sys.argv[1]
d = json.loads(sys.stdin.read())
a = d.get("applied") or {}
if a.get("outcome") != want:
    print(f"expected outcome={want!r}, got {a!r}", file=sys.stderr)
    sys.exit(1)
if want == "applied" and not a.get("plan_exists"):
    print(f"the lane claimed success with no plan file: {a!r}", file=sys.stderr)
    sys.exit(1)
PY

# ── the impostors ────────────────────────────────────────────────────────────
# Each is a file whose NAME carries this week's code. The review is the live
# incident shape: a "## Dinners" section that is prose, not a table, and a week
# header that makes it "cover" today.
review_body() {
    cat <<'NOTE'
# Review of the week

**Week of Monday 2026-07-27 to Sunday 2026-08-02**

## Dinners

The fish night landed twice this week; worth moving one to Thursday.
- Tuesday felt rushed
- Nobody finished the lentils
NOTE
}

# A companion that carries REAL week content of another kind — and no dinners.
workouts_body() {
    cat <<'NOTE'
# Moving · 2026-W31

**Week of Monday 2026-07-27 to Sunday 2026-08-02**

## 2. Workouts

### River Guest

| Day | Session |
| --- | ------- |
| Mon | Lower (strength) |
| Wed | Intervals |
NOTE
}

drafts_over() {
    local name="$1" body_fn="$2" why="$3"
    local dir impostor drafted before out
    dir="$(make_scratch)"
    mkdir -p "$dir/plans" "$dir/.wg"
    impostor="$dir/plans/$name"
    "$body_fn" >"$impostor"
    before="$(sha_of "$impostor")"
    drafted="$dir/plans/$this_code-family-plan.md"

    if ! out="$("$wg_bin" --json telegram week-start "$ask" --root "$dir" \
        --now "$now" --apply --turn-id "turn-$RANDOM$RANDOM" 2>/dev/null)"; then
        loud_fail "the week-start seam exited nonzero over a project holding $name"
    fi
    printf '%s' "$out" | python3 "$assert_outcome" applied \
        || loud_fail "$name was read as this week's plan of record — $why"
    [[ -f "$drafted" ]] \
        || loud_fail "no plan was drafted beside $name — $why"
    grep -q "## 1. Dinners" "$drafted" \
        || loud_fail "the week drafted beside $name has no dinners section"
    # The impostor supplied neither the plan nor its SHAPE, and was not rewritten.
    if grep -qi "Nobody finished the lentils\|Lower (strength)" "$drafted"; then
        loud_fail "the drafted week inherited content from $name"
    fi
    [[ "$(sha_of "$impostor")" == "$before" ]] \
        || loud_fail "$name was REWRITTEN by the plan lane — it must be left alone entirely"
}

# ── THE READ HAZARD: none of these is this week's plan ───────────────────────
drafts_over "$this_code-nora-review.md" review_body \
    "an editorial review answered 'this week is already planned' for a week with no dinners in it"
drafts_over "$this_code-review.md" review_body "any persona's review, and none"
drafts_over "$this_code-otto-notes.md" review_body "scratch notes are not a plan"
drafts_over "$this_code-summary.md" review_body "a summary is about the week, not the week"
drafts_over "$this_code-scratch.md" review_body "scratch is not a plan"
drafts_over "$this_code-wip.md" review_body "a work-in-progress file is not a plan"
drafts_over "$this_code-check-in.md" review_body "a check-in note is not a plan"
drafts_over "$this_code-family-plan.draft.md" review_body \
    "the atomic-publish half-written file is read mid-composition"
drafts_over "$this_code-dinner-suggestions.md" review_body \
    "the parked side channel — the original incident, now through the read lane"

# ── THE COMPANION: contributes elsewhere, but is not a plan of record ────────
drafts_over "$this_code-mira-workouts.md" workouts_body \
    "a workouts companion has no dinners; it must not answer for the week"

# ── AND THE REAL GUARD STILL HOLDS ──────────────────────────────────────────
# A genuine plan of record for this week is never overwritten by an accepted
# offer. If the fix had simply loosened the glob, this leg goes red.
real="$(make_scratch)"
mkdir -p "$real/plans" "$real/.wg"
plan="$real/plans/$this_code-family-plan.md"
cat >"$plan" <<'PLAN'
# Family week · 2026-W31

**Week of Monday 2026-07-27 to Sunday 2026-08-02**
**Status:** PUBLISHED

## 1. Dinners

| Day | Slot | Dinner | Prep |
| --- | --- | --- | --- |
| Mon 07-27 | Vegetarian | rice bowl | ~30 min |
| Tue 07-28 | Fish | baked trout | ~25 min |
PLAN
# …with a review sitting right beside it, which must change nothing.
review_body >"$real/plans/$this_code-nora-review.md"
plan_before="$(sha_of "$plan")"
out="$("$wg_bin" --json telegram week-start "$ask" --root "$real" \
    --now "$now" --apply --turn-id "turn-real-plan" 2>/dev/null)" \
    || loud_fail "the week-start seam exited nonzero over a project with a real plan"
printf '%s' "$out" | python3 "$assert_outcome" answered \
    || loud_fail "an accepted offer over an ALREADY PLANNED week must answer, not draft"
[[ "$(sha_of "$plan")" == "$plan_before" ]] \
    || loud_fail "the plan of record was rewritten by an accepted offer"
grep -q "baked trout" "$plan" || loud_fail "the plan of record lost its dinners"

rm -f "$assert_outcome"
echo "PASS: a week-coded review / draft / note / companion is not a plan of record"
