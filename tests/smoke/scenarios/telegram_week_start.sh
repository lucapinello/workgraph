#!/usr/bin/env bash
# Smoke: THE ENGINE HALF OF THE WEEK-START PROMISE — an accepted offer really
# produces a week, with the request the family carried into it.
#
# Pins task week-start-engine.
#
# THE PROMISE. On the Monday before the Sunday ritual has run, a write that
# belongs to a plan of record is refused with an OFFER — "This week isn't set up
# yet — want me to start it?" — and a bare "yes" accepts it. The gateway rewrites
# that affirmative into an explicit ask and dispatches it with the refused
# request QUOTED inside the message, because production web inbound drops the
# structured continuation field and the words are all that survive the trip.
#
# THE GAP THIS PINS. Nothing on the ENGINE side consumed that ask. It fell
# through the closed-set classifier to the composer, which cannot create a plan
# file — so accepting the offer produced an encouraging sentence and no week. The
# human flow that "proved" the promise hand-wrote the new plan onto disk itself
# and then asserted a write landed in it: that proves the gateway DISPATCHES and
# proves nothing about FULFILMENT. Every leg below therefore drives the DEPLOYED
# binary's credential-free `wg --json telegram week-start` seam over a throwaway
# scratch project, pinned with --now, and asserts the JSON verdict plus the bytes
# on disk:
#
#   · the dispatched ask is RECOGNIZED and its quoted carriage is read out;
#   · --apply really CREATES this week's plan — seven day rows, covering the
#     pinned week — and the requested Tuesday dinner is IN IT;
#   · the week that has already ENDED is byte-identical afterwards (the carried
#     quote reads exactly like a bare meal swap: applied to "the newest plan on
#     disk" it would rewrite a dinner the family already ate);
#   · the SAME turn id replays the stored outcome instead of drafting twice, and
#     a second acceptance answers honestly and overwrites nothing;
#   · NEGATIVE — a carried request the closed set cannot express drafts NO week
#     at all, rather than a week that silently dropped what was asked for;
#   · NEGATIVE — a QUESTION about the week ("did you start the week?") is a read
#     and creates nothing.
#
# A binary with no week-start seam FAILS rather than skipping: "the installed
# binary predates the fix" is exactly the state this scenario exists to catch.
# SKIPs (77) only when no wg binary can be found at all.
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

# Monday of the un-planned week. The only plan on disk is the week BEFORE it —
# the exact window the offer is made in.
now="2026-07-27"
ended_code="2026-W30"
this_code="2026-W31"

# DOES THE DEPLOYED BINARY REALLY CARRY THE SEAM? Probed by RUNNING it, never by
# `--help`: clap answers `--help` for an unknown subcommand by printing its
# parent's help and exiting 0, so a help probe passes on a binary that predates
# the lane entirely — a vacuous gate on exactly the state this scenario pins.
if ! "$wg_bin" --json telegram week-start \
    "Please draft this week's family plan — start the week." --now "$now" >/dev/null 2>&1; then
    loud_fail "the deployed binary has no working 'wg telegram week-start' seam — it predates the week-start engine lane (cargo install --path . --force --locked)"
fi

scratch="$(make_scratch)"
mkdir -p "$scratch/plans" "$scratch/.wg"
ended="$scratch/plans/$ended_code-family-plan.md"
drafted="$scratch/plans/$this_code-family-plan.md"

# An OPAQUE scratch plan: no shipped names, no live data. It is the week that has
# ALREADY ENDED (Jul 20–26), which is what makes the archived-write leg real.
cat >"$ended" <<'PLAN'
# Family week · 2026-W30 · Week of Monday July 20 – Sunday July 26

**Week of Monday 2026-07-20 to Sunday 2026-07-26**
**Status:** PUBLISHED

## 1. Dinners

| Day | Slot | Dinner | Prep |
| --- | --- | --- | --- |
| Mon 07-20 | Vegetarian | rice bowl | ~30 min |
| Tue 07-21 | Fish | baked trout | ~25 min |
| Wed 07-22 | Flex | bean stew | ~20 min |

## 4. Shopping list

### Market
- Olive oil, 1 bottle
PLAN

# The EXACT message the gateway dispatches when the family accepts the offer:
# the explicit instruction, carrying the refused request in quotes.
dispatched='Please draft this week'"'"'s family plan — start the week. Keep what I just asked for: "Set Tuesday'"'"'s dinner to homemade pizza."'

# ── assertion helpers (JSON + bytes, never prose) ─────────────────────────────
cat >"$scratch/assert_recognized.py" <<'PY'
import json, sys
want_recognized = sys.argv[1] == "yes"
want_carried = sys.argv[2] if len(sys.argv) > 2 else ""
d = json.loads(sys.stdin.read())
if bool(d.get("recognized")) != want_recognized:
    print(f"expected recognized={want_recognized}, got {d.get('recognized')!r} "
          f"(lane={d.get('lane')!r})", file=sys.stderr)
    sys.exit(1)
if want_carried:
    carried = d.get("carried") or []
    if want_carried not in carried:
        print(f"the quoted request was not carried through: {carried!r}", file=sys.stderr)
        sys.exit(1)
PY

cat >"$scratch/assert_applied.py" <<'PY'
import json, sys
want_outcome = sys.argv[1]
d = json.loads(sys.stdin.read())
a = d.get("applied") or {}
if a.get("outcome") != want_outcome:
    print(f"expected outcome={want_outcome!r}, got {a!r}", file=sys.stderr)
    sys.exit(1)
if want_outcome == "applied":
    # Success is a claim about BYTES, not about control flow: the seam re-reads
    # the plan it wrote and reports what parsed back out of it.
    if not a.get("plan_exists"):
        print(f"the lane claimed success with no plan file: {a!r}", file=sys.stderr)
        sys.exit(1)
    if a.get("week") != sys.argv[2]:
        print(f"drafted the wrong week: {a!r}", file=sys.stderr)
        sys.exit(1)
    if a.get("day_rows") != 7:
        print(f"a week has seven nights, got {a.get('day_rows')!r}", file=sys.stderr)
        sys.exit(1)
    want_day, want_dish = sys.argv[3], sys.argv[4]
    hit = [m for m in (a.get("dinners") or [])
           if m.get("day", "").lower().startswith(want_day.lower())
           and want_dish.lower() in (m.get("dish") or "").lower()]
    if not hit:
        print(f"the carried request is NOT in the drafted week: {a.get('dinners')!r}",
              file=sys.stderr)
        sys.exit(1)
if want_outcome == "replayed" and not a.get("already_delivered"):
    print(f"a refire of the same turn was not recognized as delivered: {a!r}", file=sys.stderr)
    sys.exit(1)
if want_outcome == "answered":
    reply = (a.get("reply") or "").lower()
    if reply.startswith("done"):
        print(f"an answer that wrote nothing opened like an applied edit: {reply!r}",
              file=sys.stderr)
        sys.exit(1)
PY

sha_of() {
    python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

recognize_is() {
    local text="$1" want="$2" carried="${3:-}" out
    if ! out="$("$wg_bin" --json telegram week-start "$text" --now "$now" 2>/dev/null)"; then
        loud_fail "the week-start seam exited nonzero for: $text"
    fi
    printf '%s' "$out" | python3 "$scratch/assert_recognized.py" "$want" "$carried" \
        || loud_fail "wrong week-start recognition for: $text"
}

apply_is() {
    local text="$1" turn="$2" want_outcome="$3"
    shift 3
    local out
    if ! out="$("$wg_bin" --json telegram week-start "$text" --root "$scratch" \
        --now "$now" --apply --turn-id "$turn" 2>/dev/null)"; then
        loud_fail "the week-start apply seam exited nonzero for: $text"
    fi
    printf '%s' "$out" | python3 "$scratch/assert_applied.py" "$want_outcome" "$@" \
        || loud_fail "wrong week-start apply outcome for: $text"
}

ended_before="$(sha_of "$ended")"

# ── RECOGNITION: the dispatched ask is owned, and its carriage read out ───────
recognize_is "$dispatched" yes "Set Tuesday's dinner to homemade pizza."
# …the bare form too (an offer accepted with nothing outstanding).
recognize_is "Please draft this week's family plan — start the week." yes

# ── A QUESTION ABOUT THE WEEK IS A READ ──────────────────────────────────────
# "Did you start the week?" must not create a plan of record. Recognizing it
# would be the same class of bug as writing on a read.
recognize_is "Did you start the week?" no
recognize_is "Has anyone set up this week yet?" no
recognize_is "swap Friday to tacos" no

# ── APPLY: the accepted offer really produces THIS week, carrying the edit ────
if [[ -e "$drafted" ]]; then loud_fail "fixture precondition: this week must not be planned yet"; fi
apply_is "$dispatched" "turn-week-start-1" applied "$this_code" "Tue" "homemade pizza"
[[ -f "$drafted" ]] || loud_fail "the lane reported success but no plan file exists"
grep -qi "homemade pizza" "$drafted" \
    || loud_fail "the drafted week does not carry the request the family made"
grep -q "2026-07-27" "$drafted" || loud_fail "the drafted week is not dated to this week"

# ── the week that ENDED is byte-identical ────────────────────────────────────
# The carried quote reads exactly like a bare meal swap; applied to "the newest
# plan on disk" it would have rewritten a dinner the family already ate.
[[ "$(sha_of "$ended")" == "$ended_before" ]] \
    || loud_fail "starting this week rewrote the week that had already ended"
grep -q "baked trout" "$ended" || loud_fail "the ended week's Tuesday dinner was overwritten"

# …and this week is not a copy of last week's content.
if grep -q "rice bowl" "$drafted"; then
    loud_fail "the draft copied last week's dinners into this week"
fi
if grep -q "Olive oil" "$drafted"; then
    loud_fail "the draft copied last week's shopping list"
fi

# ── CORRELATION: the same turn id replays, it never drafts twice ─────────────
drafted_sum="$(sha_of "$drafted")"
apply_is "$dispatched" "turn-week-start-1" replayed
[[ "$(sha_of "$drafted")" == "$drafted_sum" ]] \
    || loud_fail "a refire of the same accepted turn rewrote the week"

# ── a SECOND acceptance answers honestly and overwrites nothing ──────────────
apply_is "$dispatched" "turn-week-start-2" answered
[[ "$(sha_of "$drafted")" == "$drafted_sum" ]] \
    || loud_fail "a second acceptance erased the week the first one created"

# ── NEGATIVE: a carry the closed set cannot express drafts NO week ───────────
# The whole point of the lane. A week on disk that quietly dropped the request
# the family carried into it is the failure this ordering exists to prevent.
neg_scratch="$(make_scratch)"
mkdir -p "$neg_scratch/plans" "$neg_scratch/.wg"
cp "$ended" "$neg_scratch/plans/$ended_code-family-plan.md"
unfulfillable='Please draft this week'"'"'s family plan — start the week. Keep what I just asked for: "Rebalance the whole week around the travel."'
if ! out="$("$wg_bin" --json telegram week-start "$unfulfillable" --root "$neg_scratch" \
    --now "$now" --apply --turn-id "turn-week-start-neg" 2>/dev/null)"; then
    loud_fail "the week-start seam exited nonzero on an unfulfillable carry"
fi
printf '%s' "$out" | python3 "$scratch/assert_applied.py" fallback \
    || loud_fail "an unfulfillable carry must defer, not draft"
if [[ -e "$neg_scratch/plans/$this_code-family-plan.md" ]]; then
    loud_fail "a week was drafted WITHOUT the request the family carried into it"
fi

# ── NEGATIVE: a question creates nothing, even with --apply ──────────────────
q_scratch="$(make_scratch)"
mkdir -p "$q_scratch/plans" "$q_scratch/.wg"
cp "$ended" "$q_scratch/plans/$ended_code-family-plan.md"
if "$wg_bin" --json telegram week-start "Did you start the week?" --root "$q_scratch" \
    --now "$now" --apply --turn-id "turn-week-start-q" >/dev/null 2>&1; then
    loud_fail "a QUESTION about the week was accepted as an instruction to draft one"
fi
if [[ -e "$q_scratch/plans/$this_code-family-plan.md" ]]; then
    loud_fail "asking whether the week was started CREATED a week"
fi

echo "PASS: telegram_week_start"
