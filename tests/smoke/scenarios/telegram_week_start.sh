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
#   · dedupe keys on (turn, ATTEMPT): the same pair is a refire and replays, a
#     NEW attempt on the same turn is the gateway self-healing a delivery that
#     died before the family got an answer and must NOT be suppressed — while
#     still overwriting nothing when the week is already there;
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

# ── NEGATIVE: A REFUSAL IS NOT AN INSTRUCTION ────────────────────────────────
# The start phrases are matched as SUBSTRINGS, and "start the week" is a
# substring of "Don't start the week." — so telling the house NOT to start the
# week STARTED it, and wrote a plan of record the family had explicitly refused.
# The loudest possible way to be ignored. Every phrasing below must write nothing.
while IFS= read -r refusal; do
    [[ -n "$refusal" ]] || continue
    neg_dir="$(make_scratch)"
    mkdir -p "$neg_dir/plans" "$neg_dir/.wg"
    cp "$ended" "$neg_dir/plans/$ended_code-family-plan.md"
    # Exit status is not the assertion — the ARTIFACT is. A refusal may exit
    # either way (it is simply not this lane's message); what it may never do is
    # leave a week on disk.
    "$wg_bin" --json telegram week-start "$refusal" --root "$neg_dir" \
        --now "$now" --apply --turn-id "turn-refusal" >/dev/null 2>&1 || true
    if [[ -e "$neg_dir/plans/$this_code-family-plan.md" ]]; then
        loud_fail "a REFUSAL created a plan of record: $refusal"
    fi
    recognize_is "$refusal" no
done <<'REFUSALS'
Don't start the week.
Do not start the week yet.
Never start the week without asking me.
Not yet — don't set up this week.
Stop — do not plan this week.
Please cancel that, don't draft this week's family plan.
Hold off, no need to start the week.
REFUSALS

# …and the negation gate did not eat the asks it sits next to. A "don't" inside
# the QUOTED carriage is the family's EDIT, not a refusal of the ask carrying it.
recognize_is "Please draft this week's family plan — start the week." yes
recognize_is "Non-stop week ahead — start the week." yes
recognize_is 'Please draft this week'"'"'s family plan — start the week. Keep what I just asked for: "Don'"'"'t put fish on Tuesday."' \
    yes "Don't put fish on Tuesday."

# ── SIDECAR: a parked-suggestions note is not a plan, and its dinners land ────
# `plans/<week>-dinner-suggestions.md` is where the family's dinner choices are
# parked for a week that has no plan yet. Its filename carries a week code, so
# shape discovery read it AS the household's most recent plan: the drafted week
# came out titled "Dinner suggestions for the week of …", with the household's
# real headings gone AND the parked dinners dropped — the family's own choices
# lost by the very draft that was supposed to honour them (docs/11 §0c).
side="$(make_scratch)"
mkdir -p "$side/plans" "$side/.wg"
cp "$ended" "$side/plans/$ended_code-family-plan.md"
cat >"$side/plans/$this_code-dinner-suggestions.md" <<'NOTE'
# Dinner suggestions for the week of 2026-W31

These are ideas the family added before the plan was drafted.

- **Thursday** (2026-07-30) — Fish tacos · suggested by the household
- **Friday** (2026-07-31) — Dining out
NOTE
note="$side/plans/$this_code-dinner-suggestions.md"
side_drafted="$side/plans/$this_code-family-plan.md"

if ! out="$("$wg_bin" --json telegram week-start \
    "Please draft this week's family plan — start the week." --root "$side" \
    --now "$now" --apply --turn-id "turn-sidecar" 2>/dev/null)"; then
    loud_fail "the week-start seam exited nonzero over a project with a parked note"
fi
printf '%s' "$out" | python3 "$scratch/assert_applied.py" applied "$this_code" "Thu" "Fish tacos" \
    || loud_fail "the parked Thursday dinner is not in the drafted week"
[[ -f "$side_drafted" ]] || loud_fail "no plan was drafted alongside a parked note"

# THE SHAPE came from the household's own plan, never from the note.
if grep -q "Dinner suggestions for the week" "$side_drafted"; then
    loud_fail "the drafted plan inherited the SIDECAR's shape — a note was read as a plan"
fi
grep -q "## 1. Dinners" "$side_drafted" \
    || loud_fail "the drafted week has no dinners section — the shape source was not a plan"
# THE CHOICES landed, on their nights, verbatim — and their provenance did not.
grep -qi "Thu 07-30 .*Fish tacos" "$side_drafted" \
    || loud_fail "the parked Thursday dinner was dropped from the draft"
grep -qi "Fri 07-31 .*Dining out" "$side_drafted" \
    || loud_fail "the parked non-cook Friday was dropped from the draft"
if grep -q "suggested by" "$side_drafted"; then
    loud_fail "the note's provenance leaked into the family's plan"
fi

# RETIREMENT IS IDEMPOTENT and destroys nothing: the family's own words stay in
# the note, and a second draft over the same note re-applies nothing.
grep -q "Fish tacos" "$note" || loud_fail "retiring the note deleted the family's own words"
grep -q "folded into" "$note" || loud_fail "the folded note was not retired"
side_sum="$(sha_of "$side_drafted")"
note_sum="$(sha_of "$note")"
"$wg_bin" --json telegram week-start "Please draft this week's family plan — start the week." \
    --root "$side" --now "$now" --apply --turn-id "turn-sidecar-2" >/dev/null 2>&1 || true
[[ "$(sha_of "$side_drafted")" == "$side_sum" ]] \
    || loud_fail "a second draft over a folded note rewrote the week"
[[ "$(sha_of "$note")" == "$note_sum" ]] \
    || loud_fail "folding the note a second time was not a no-op"

# ── LIVE SHAPE: a section companion is not a week of dinners ──────────────────
# A `-workouts` / `-recipes` companion is a legitimate file for its week, but a
# week drafted in ITS shape has no meals table at all — so nothing the family
# asked for could land, and the report would be confident about an empty week.
comp="$(make_scratch)"
mkdir -p "$comp/plans" "$comp/.wg"
cp "$ended" "$comp/plans/$ended_code-family-plan.md"
cat >"$comp/plans/$this_code-workouts.md" <<'COMP'
# Movement for the week

## Moving

| Day | Session |
| --- | --- |
| Mon | easy run |
COMP
if ! out="$("$wg_bin" --json telegram week-start "$dispatched" --root "$comp" \
    --now "$now" --apply --turn-id "turn-companion" 2>/dev/null)"; then
    loud_fail "the week-start seam exited nonzero over a project with a companion file"
fi
printf '%s' "$out" | python3 "$scratch/assert_applied.py" applied "$this_code" "Tue" "homemade pizza" \
    || loud_fail "a companion file took over the drafted week's shape"
if grep -q "Movement for the week" "$comp/plans/$this_code-family-plan.md"; then
    loud_fail "the drafted plan inherited a COMPANION's shape"
fi

# ── A REPLAY MUST BE TRUE ────────────────────────────────────────────────────
# The turn ledger says this week was drafted; the plan file is gone. Replaying a
# stored "this week's plan is started" against an empty disk is the
# dead-pipeline-claims-success failure arriving THROUGH the idempotency guard
# rather than around it. The honest answer is to draft the week.
rm -f "$drafted"
apply_is "$dispatched" "turn-week-start-1" applied "$this_code" "Tue" "homemade pizza"
[[ -f "$drafted" ]] \
    || loud_fail "a replay whose plan had vanished reported success and wrote nothing"

# …and two DIFFERENT projects never share one turn ledger: the same turn id in a
# fresh project must draft that project's week, not replay another project's.
twin="$(make_scratch)"
mkdir -p "$twin/plans"
cp "$ended" "$twin/plans/$ended_code-family-plan.md"
if ! out="$("$wg_bin" --json telegram week-start "$dispatched" --root "$twin" \
    --now "$now" --apply --turn-id "turn-week-start-1" 2>/dev/null)"; then
    loud_fail "the week-start seam exited nonzero over a second project"
fi
printf '%s' "$out" | python3 "$scratch/assert_applied.py" applied "$this_code" "Tue" "homemade pizza" \
    || loud_fail "a second project replayed the first project's outcome"
[[ -f "$twin/plans/$this_code-family-plan.md" ]] \
    || loud_fail "a second project inherited another project's turn ledger and got no week"

# ═════════════════════════════════════════════════════════════════════════════
# ATTEMPT KEYING — a refire is suppressed, a SELF-HEAL RETRY is not
# (task week-start-attempt)
#
# The gateway keeps the occurrence id STABLE across a retry of the same accepted
# turn: that is what makes a dispatcher redelivery suppressible. It also retries
# a turn whose first delivery died before the family got an answer — and under a
# turn-only key that retry matches the dead attempt's ledger entry and is dropped
# as "already answered", so the self-heal heals nothing. The canonical ATTEMPT id
# separates the two: the same (turn, attempt) is one physical delivery, a new
# attempt on the same turn is a fresh chance to answer it.
#
# Every leg below drives the same deployed seam and asserts the PLAN BYTES, not
# the verdict alone — a re-key that quietly redrafts over a week the family
# already has is the failure this section is here to prevent as much as the
# suppression is.
# ═════════════════════════════════════════════════════════════════════════════

# One JSON field out of the last run, so a leg can compare the KEYS the dedupe
# actually uses and not only the verdict it printed.
cat >"$scratch/field.py" <<'PY'
import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2].split("."):
    value = (value or {}).get(key)
print("" if value is None else value if isinstance(value, str) else json.dumps(value))
PY

att_run() { # att_run <root> <flag...>  — the accepted ask, once, JSON to last.json
    local root="$1"
    shift
    if ! "$wg_bin" --json telegram week-start "$dispatched" --root "$root" --now "$now" \
        --apply "$@" >"$scratch/last.json" 2>/dev/null; then
        loud_fail "the week-start seam exited nonzero for: $* (does the installed binary carry --attempt-id?)"
    fi
}
att_field() { python3 "$scratch/field.py" "$scratch/last.json" "$1"; }
att_outcome_is() {
    python3 "$scratch/assert_applied.py" "$@" <"$scratch/last.json" \
        || loud_fail "wrong week-start apply outcome for attempt leg: $*"
}

att="$(make_scratch)"
mkdir -p "$att/plans" "$att/.wg"
cp "$ended" "$att/plans/$ended_code-family-plan.md"
att_drafted="$att/plans/$this_code-family-plan.md"

# ── the first attempt of an accepted turn drafts the week ────────────────────
att_run "$att" --turn-id turn-attempt-1 --attempt-id attempt-1
att_outcome_is applied "$this_code" "Tue" "homemade pizza"
[[ "$(att_field attempt_id)" == "attempt-1" ]] \
    || loud_fail "--attempt-id never reached the seam: $(att_field attempt_id)"
[[ -f "$att_drafted" ]] || loud_fail "the first attempt reported success with no plan file"
att_key_first="$(att_field turn_key)"
att_sum="$(sha_of "$att_drafted")"
# The correlation fingerprint stays OPAQUE: it is written into a durable ledger
# whose directory listing must not spell out a household's turn or attempt ids.
case "$att_key_first" in
    *attempt-1* | *turn-attempt-1*)
        loud_fail "the turn fingerprint leaked the gateway's ids: $att_key_first"
        ;;
esac

# ── a TRUE refire — same turn, SAME attempt — replays and drafts nothing ─────
att_run "$att" --turn-id turn-attempt-1 --attempt-id attempt-1
att_outcome_is replayed
[[ "$(att_field turn_key)" == "$att_key_first" ]] \
    || loud_fail "one physical delivery produced two different keys"
[[ "$(sha_of "$att_drafted")" == "$att_sum" ]] \
    || loud_fail "a refire of the same (turn, attempt) rewrote the week"

# ── WG_ATTEMPT_ID is the same transport as the flag, and the flag wins ───────
# Same shape as WG_TURN_ID / WG_OWNER_PIN: the gateway may pass either, and an
# explicit flag beats the ambient environment.
if ! env WG_ATTEMPT_ID=attempt-1 "$wg_bin" --json telegram week-start "$dispatched" \
    --root "$att" --now "$now" --apply --turn-id turn-attempt-1 >"$scratch/last.json" 2>/dev/null; then
    loud_fail "the week-start seam exited nonzero with WG_ATTEMPT_ID set"
fi
att_outcome_is replayed
[[ "$(att_field turn_key)" == "$att_key_first" ]] \
    || loud_fail "WG_ATTEMPT_ID keyed a different occurrence than --attempt-id"
if ! env WG_ATTEMPT_ID=attempt-from-the-environment "$wg_bin" --json telegram week-start \
    "$dispatched" --root "$att" --now "$now" --apply --turn-id turn-attempt-1 \
    --attempt-id attempt-1 >"$scratch/last.json" 2>/dev/null; then
    loud_fail "the week-start seam exited nonzero with both attempt transports set"
fi
[[ "$(att_field turn_key)" == "$att_key_first" ]] \
    || loud_fail "the ambient WG_ATTEMPT_ID overrode the explicit --attempt-id"

# ── a NEW attempt on the same turn is NOT suppressed — and overwrites nothing ─
att_run "$att" --turn-id turn-attempt-1 --attempt-id attempt-2
att_key_retry="$(att_field turn_key)"
[[ "$att_key_retry" != "$att_key_first" ]] \
    || loud_fail "a new attempt on the same turn reused the first attempt's key"
[[ "$(att_field applied.already_delivered)" == "false" ]] \
    || loud_fail "a self-heal retry was suppressed as already-answered: $(att_field applied)"
# Not suppressed does NOT mean drafted twice: the artifact layer answers honestly
# about the week that is already there. (This is the idempotence that had to
# survive the re-key — the dedupe layer is no longer what protects the file.)
att_outcome_is answered
[[ "$(sha_of "$att_drafted")" == "$att_sum" ]] \
    || loud_fail "a self-heal retry redrafted over the week the family already has"
grep -qi "homemade pizza" "$att_drafted" \
    || loud_fail "the retry lost the request the first attempt had honoured"

# ── the LEGACY turn-only key is untouched ────────────────────────────────────
# An older gateway sends no attempt id at all. Its key must stay exactly what it
# was — its own occurrence, still replaying against itself — or every ledger
# entry a running gateway already wrote is orphaned by this change.
att_run "$att" --turn-id turn-attempt-1
[[ "$(att_field attempt_id)" == "" ]] || loud_fail "an attempt id was invented for a legacy caller"
att_key_legacy="$(att_field turn_key)"
[[ "$att_key_legacy" != "$att_key_first" && "$att_key_legacy" != "$att_key_retry" ]] \
    || loud_fail "a legacy turn-only call collided with an attempt-bearing occurrence"
att_outcome_is answered
att_run "$att" --turn-id turn-attempt-1
att_outcome_is replayed
[[ "$(sha_of "$att_drafted")" == "$att_sum" ]] \
    || loud_fail "the legacy turn-only path rewrote the week"

# ── THE WEDGE: a delivery that DIED before the week landed ───────────────────
# This is what the attempt id is for. The first delivery of the accepted turn
# cannot write (here: an unwritable `plans/` — a full disk, a permission change,
# a killed process mid-flight all land in the same place), so the turn is
# journaled with an outcome that is NOT a drafted week. Under a turn-only key
# every later retry of that turn reads that entry back and writes nothing: the
# family accepted the offer, the gateway retried, and the week never exists.
if [[ "$(id -u)" == "0" ]]; then
    echo "note: running as root — the unwritable-plans wedge cannot be staged, skipping that leg"
else
    heal="$(make_scratch)"
    mkdir -p "$heal/plans" "$heal/.wg"
    cp "$ended" "$heal/plans/$ended_code-family-plan.md"
    heal_drafted="$heal/plans/$this_code-family-plan.md"
    chmod 500 "$heal/plans"
    # The dying attempt: exit status is not the assertion, the ARTIFACT is.
    "$wg_bin" --json telegram week-start "$dispatched" --root "$heal" --now "$now" \
        --apply --turn-id turn-heal --attempt-id attempt-1 >/dev/null 2>&1 || true
    chmod 755 "$heal/plans"
    [[ ! -e "$heal_drafted" ]] \
        || loud_fail "fixture precondition: the first attempt was supposed to fail to write"
    # The SAME (turn, attempt) arriving again still writes nothing — that record
    # is about THIS delivery, and re-running a mutation whose fate is unknown is
    # exactly what the journal exists to prevent.
    att_run "$heal" --turn-id turn-heal --attempt-id attempt-1
    [[ "$(att_field applied.outcome)" != "applied" ]] \
        || loud_fail "a refire of the dead attempt re-ran the mutation"
    [[ ! -e "$heal_drafted" ]] \
        || loud_fail "a refire of the dead attempt drafted the week behind the journal"
    # …and the gateway's NEW attempt on that same turn finally produces the week.
    att_run "$heal" --turn-id turn-heal --attempt-id attempt-2
    att_outcome_is applied "$this_code" "Tue" "homemade pizza"
    [[ -f "$heal_drafted" ]] \
        || loud_fail "the self-heal retry of a turn whose delivery died produced NO week"
    grep -qi "homemade pizza" "$heal_drafted" \
        || loud_fail "the healed week dropped the request the family carried into it"
    grep -q "2026-07-27" "$heal_drafted" || loud_fail "the healed week is not dated to this week"
fi

echo "PASS: telegram_week_start"
