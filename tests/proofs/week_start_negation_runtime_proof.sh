#!/usr/bin/env bash
# RUNTIME PROOF for the week-start NEGATION P0 (task week-start-engine-2, set 1).
#
# "Don't start the week." NAMED the lane in order to refuse it, and the closed-set
# detector is a SUBSTRING scan: it saw "start the week" inside the refusal, read
# that as consent, and created the plan of record. The one sentence that could not
# have been clearer about wanting no plan was the one that produced one.
#
# This drives a BINARY — not a unit test — over a fresh scratch project per
# phrasing, and asserts the bytes:
#
#   · every negated phrasing writes NO plan, leaves plans/ byte-identical, and
#     REPORTS the refusal (a crashed binary also writes no plan, so the absence
#     of a file on its own is not evidence of anything);
#   · every positive phrasing still drafts a real seven-row week dated to the
#     pinned week — including the two that contain a negation in their REASON
#     ("this week is not set up yet — please start the week"), which is how a
#     blunter guard would have broken the promise in the other direction.
#
# Run it against the PRE-FIX binary and the negatives FAIL — that is the bug
# reproducing. Against the fixed binary both halves pass.
#
#   ./week_start_negation_runtime_proof.sh /path/to/wg [outdir]
#
# The binary is an argument on purpose: this proof must never reach for the live
# ~/.cargo/bin/wg. Deploying is a separate, later act.
set -uo pipefail

WG="${1:?usage: week_start_negation_runtime_proof.sh <path-to-wg> [outdir]}"
unset WG_DIR    # the journal belongs to each scratch project, never to the live graph
OUT="${2:-${TMPDIR:-/tmp}/week-start-negation-runtime-proof/run}"
PIN=2026-07-27          # the Monday of an un-planned week
WEEK=2026-W31

[ -x "$WG" ] || { echo "FAIL: not an executable binary: $WG"; exit 2; }
rm -rf "$OUT"; mkdir -p "$OUT"

echo "binary:  $WG"
echo "sha256:  $(shasum -a 256 "$WG" | cut -d' ' -f1)"
echo "pinned:  $PIN ($WEEK)"
echo

# A scratch project whose newest plan is the week that has ALREADY ENDED — the
# exact state the offer is made in, and the state in which a spurious draft does
# the damage.
scratch() {
  local dir="$1"
  # Its OWN occurrence journal. Without this the draft is journaled against the
  # ambient .wg, the turn key is the message words, and the SECOND scratch
  # project to see a given sentence replays the first one's outcome instead of
  # drafting — a green run that proved nothing about this binary.
  mkdir -p "$dir/plans" "$dir/.wg"
  cat > "$dir/plans/2026-W30-family-plan.md" <<'PLAN'
# Family week · 2026-W30 · Week of Monday July 20 – Sunday July 26

**Week of Monday 2026-07-20 to Sunday 2026-07-26**
**Status:** PUBLISHED

## 1. Dinners (planner → cook)

| Day | Slot | Dinner | Prep |
|-----|------|--------|------|
| Mon 07-20 | Vegetarian | Chickpea curry | ~35 min |
| Tue 07-21 | Fish | Baked salmon | ~30 min |

## 4. Shopping list — by store

### Greengrocer / produce
- Chard, 1 bunch
PLAN
}

pass=0; fail=0
note() { printf '  %s\n' "$1"; }

# --- the negatives: a REFUSAL must write no week -----------------------------
NEGATIVES=(
  "Don't start the week."
  "Do not start the week."
  "Please don't set up this week."
  "Never start the week without asking me first."
  "Not yet — don't draft this week's plan."
  "Cancel that, don't start the week."
  "Stop — do not plan this week."
  "Hold off, don't get this week started."
  "Not yet, start the week later."
)

i=0
for msg in "${NEGATIVES[@]}"; do
  i=$((i+1))
  dir="$OUT/neg-$i"; scratch "$dir"
  before=$(ls "$dir/plans" | sort)
  out=$("$WG" --json telegram week-start "$msg" --root "$dir" --now "$PIN" --apply 2>&1)
  code=$?
  printf '%s\n' "$out" > "$dir/verdict.json"
  after=$(ls "$dir/plans" | sort)
  plan="$dir/plans/$WEEK-family-plan.md"

  echo "NEG $i: $msg"
  if [ -e "$plan" ]; then
    note "FAIL — a REFUSAL created $WEEK-family-plan.md (exit $code)"
    fail=$((fail+1)); continue
  fi
  if [ "$before" != "$after" ]; then
    note "FAIL — plans/ changed: [$before] -> [$after]"
    fail=$((fail+1)); continue
  fi
  # The verdict must SAY it refused, not merely fail to mention the week: a
  # crashed binary also writes no plan, and would pass a file-absence check.
  if ! printf '%s' "$out" | grep -q 'NEGATED week-start ask'; then
    note "FAIL — no honest refusal in the verdict: $out"
    fail=$((fail+1)); continue
  fi
  note "ok — no plan written, refusal reported"
  pass=$((pass+1))
done

# The same negatives, read-only (no --apply): the verdict must report the lane as
# refused rather than merely unrecognized.
i=0
for msg in "${NEGATIVES[@]}"; do
  i=$((i+1))
  echo "NEG-READ $i: $msg"
  out=$("$WG" --json telegram week-start "$msg" --now "$PIN" 2>&1)
  if printf '%s' "$out" | grep -q '"recognized": true'; then
    note "FAIL — a REFUSAL was recognized as a week-start ask"
    fail=$((fail+1)); continue
  fi
  if ! printf '%s' "$out" | grep -q '"negated": true'; then
    note "FAIL — the refusal was not reported as one: $out"
    fail=$((fail+1)); continue
  fi
  note "ok — recognized:false negated:true"
  pass=$((pass+1))
done

# --- the positives: the promise still works ----------------------------------
POSITIVES=(
  "Please draft this week's family plan — start the week."
  "Please draft this week's family plan — start the week. Keep what I just asked for: \"Set Tuesday's dinner to homemade pizza.\""
  "This week is not set up yet — please start the week."
  "Don't worry about the shopping list; start the week."
)

i=0
for msg in "${POSITIVES[@]}"; do
  i=$((i+1))
  dir="$OUT/pos-$i"; scratch "$dir"
  out=$("$WG" --json telegram week-start "$msg" --root "$dir" --now "$PIN" --apply 2>&1)
  printf '%s\n' "$out" > "$dir/verdict.json"
  plan="$dir/plans/$WEEK-family-plan.md"

  echo "POS $i: $msg"
  if [ ! -e "$plan" ]; then
    note "FAIL — a genuine ask drafted NO week (the guard over-refused)"
    note "  $out"
    fail=$((fail+1)); continue
  fi
  rows=$(grep -cE '^\| (Mon|Tue|Wed|Thu|Fri|Sat|Sun) ' "$plan")
  if [ "$rows" -ne 7 ]; then
    note "FAIL — $rows day rows, expected 7"
    fail=$((fail+1)); continue
  fi
  if ! grep -q '2026-07-27' "$plan"; then
    note "FAIL — the drafted week is not dated to the pinned week"
    fail=$((fail+1)); continue
  fi
  if printf '%s' "$msg" | grep -q 'homemade pizza'; then
    if ! grep -qi 'homemade pizza' "$plan"; then
      note "FAIL — the carried request was dropped from the drafted week"
      fail=$((fail+1)); continue
    fi
    note "ok — week drafted, carried request landed"
  else
    note "ok — week drafted"
  fi
  # …and last week is never copied into it.
  if grep -qi 'chickpea curry' "$plan"; then
    note "FAIL — the drafted week copied the ENDED week's content"
    fail=$((fail+1)); continue
  fi
  pass=$((pass+1))
done

echo
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ] || exit 1
echo "PROOF OK"
