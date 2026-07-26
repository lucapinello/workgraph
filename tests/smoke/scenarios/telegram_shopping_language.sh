#!/usr/bin/env bash
# Smoke: SHOPPING MUTATION LANGUAGE — what ordinary family wording does to the list.
#
# Pins task shopping-engine-half (the engine half of the live-cert P1 "shopping mutation
# language is not safe enough", docs/reviews/LIVE-CONVO-CERT-2026-07-26.md C056–C064).
#
# THE BUG. The gateway half of that P1 shipped first, and it only runs on the gateway's
# own web/group send path. A line typed into the family TELEGRAM group is elected and
# answered by the ENGINE, which never reached those lanes — so on this side:
#
#   "Add glorptwax to shopping."             → WROTE it and confirmed "Done — … 🛒"
#   "Don't add olive oil yet—ask me first."  → WROTE olive oil (the negation was ignored)
#   "Remove AA batteries again."             → removed nothing (no removal path existed)
#   "We are out of olive oil—add olive oil." → unrecognized, or the duplicated literal
#
# WHY A SCENARIO AND NOT A GREP. A grep proves a symbol moved; it cannot prove what a
# SENTENCE does. Every leg below runs the DEPLOYED binary through the credential-free
# seam `wg --json telegram shopping <text>` — the same classifier, over the same
# vocabulary, a family message hits — and the write legs run `--apply` against a
# throwaway scratch project, so a removal is proven to reach the plan file and an ASK is
# proven to leave it byte-identical. No token, no network, no bot, no live plan.
#
# The `--today` pin makes every leg deterministic; the scratch plan is the only plan the
# lane can see.
#
# SKIPs (77) only when no wg binary can be found. A binary that HAS no shopping seam is a
# FAIL, not a skip: that is precisely the "installed binary predates the fix" state this
# scenario exists to catch.

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

if ! "$wg_bin" telegram shopping --help >/dev/null 2>&1; then
    loud_fail "the deployed binary has no 'wg telegram shopping' seam — it predates the shopping-language lane (cargo install --path . --force --locked)"
fi

today="2026-07-14"
scratch="$(make_scratch)"
mkdir -p "$scratch/plans"
plan="$scratch/plans/2026-W29-family-plan.md"

# An OPAQUE scratch plan: no shipped names, no live data. Only the shopping section
# matters to this lane; the dinners table is here so the plan parses like a real one.
cat >"$plan" <<'PLAN'
# Week 2026-W29 — family plan

Week of 2026-07-13 → 2026-07-19

## 1. Dinners

| Day | Dinner |
| --- | --- |
| Mon 07-13 | rice bowl |
| Tue 07-14 | bean stew |
| Wed 07-15 | baked trout |
| Thu 07-16 | lentil salad |
| Fri 07-17 | stir fry |
| Sat 07-18 | flatbreads |
| Sun 07-19 | flex bowl |

## 4. Shopping list

### Market
- Olive oil, 1 bottle
- Green beans, 300 g (Tue)
- AA batteries ×4
PLAN

# ── assertion helpers (JSON, never prose — a wording change cannot fake a pass) ─
cat >"$scratch/assert_lane.py" <<'PY'
import json, sys
want_lane, want_item, want_reason, text = sys.argv[1:5]
d = json.loads(sys.stdin.read())
def bad(msg):
    print(f"SHOPPING-LANE MISMATCH for {text!r}: {msg}", file=sys.stderr)
    print(f"  got lane={d.get('lane')!r} item={d.get('item')!r} reason={d.get('reason')!r}",
          file=sys.stderr)
    sys.exit(1)
if d.get("lane") != want_lane:
    bad(f"expected lane {want_lane!r}")
if want_item and (d.get("item") or "") != want_item:
    bad(f"expected item {want_item!r}")
if want_reason and (d.get("reason") or "") != want_reason:
    bad(f"expected reason {want_reason!r}")
PY

cat >"$scratch/assert_applied.py" <<'PY'
import json, sys
want_outcome = sys.argv[1]
want_lane = sys.argv[2] if len(sys.argv) > 2 else ""
a = (json.loads(sys.stdin.read()).get("applied") or {})
if a.get("outcome") != want_outcome:
    print(f"expected outcome={want_outcome!r}, got {a!r}", file=sys.stderr)
    sys.exit(1)
if want_lane and a.get("lane") != want_lane:
    print(f"expected lane={want_lane!r}, got {a!r}", file=sys.stderr)
    sys.exit(1)
# An answer that writes nothing must never sound like an applied edit.
if want_outcome == "answered" and (a.get("reply") or "").lower().startswith("done"):
    print(f"an ask must never open like an applied edit: {a.get('reply')!r}", file=sys.stderr)
    sys.exit(1)
PY

sha_of() {
    python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

classify_is() {
    local text="$1" want_lane="$2" want_item="$3" want_reason="$4" out
    if ! out="$("$wg_bin" --json telegram shopping "$text" --today "$today" 2>/dev/null)"; then
        loud_fail "the shopping seam exited nonzero for: $text"
    fi
    if ! printf '%s' "$out" \
        | python3 "$scratch/assert_lane.py" "$want_lane" "$want_item" "$want_reason" "$text"; then
        loud_fail "wrong shopping verdict for: $text"
    fi
}

apply_is() {
    local text="$1" want_outcome="$2" want_lane="${3:-}" out
    if ! out="$("$wg_bin" --json telegram shopping "$text" --root "$scratch" --today "$today" --apply 2>/dev/null)"; then
        loud_fail "the apply seam exited nonzero for: $text"
    fi
    if ! printf '%s' "$out" | python3 "$scratch/assert_applied.py" "$want_outcome" "$want_lane"; then
        loud_fail "wrong apply outcome for: $text"
    fi
}

# ── C058: a nonsense item is ASKED about, never written ───────────────────────
classify_is "Add glorptwax to shopping." ask "" unknown-item
classify_is "Add glorptwax to the shopping list." ask "" unknown-item
# …while a real good the aisle taxonomy has no entry for still lands normally.
classify_is "add freezer bags to the shopping list" shopping-add "freezer bags" ""

# ── C059: a HOLD asks and writes nothing (the corpus typo verbatim) ───────────
classify_is "We are low on baking soda. Don not add it yet—ask me first." ask "" held-ask
classify_is "Don't add olive oil to the shopping list yet—ask me first." ask "" held-ask

# ── C056/C057: the removal phrasings really remove ────────────────────────────
classify_is "Remove AA batteries again." shopping-remove "aa batteries" ""
classify_is "Take dishwasher tablets back off." shopping-remove "dishwasher tablets" ""
classify_is "scratch the olive oil off the shopping list" shopping-remove "olive oil" ""

# ── C060: a CANCEL takes the named item back off ──────────────────────────────
classify_is "no baking soda needed after all" shopping-remove "baking soda" ""

# ── C064: one SENTENCE is one item, never the duplicated literal ──────────────
classify_is "We are out of olive oil—add olive oil." shopping-add "olive oil" ""

# ── crossing off is BOUGHT, not deleted: the row must stay ────────────────────
classify_is "cross the milk off the list" none "" not-a-simple-edit
classify_is "checked off the eggs on the shopping list" none "" not-a-simple-edit

# ── an ACTION sentence is never a silent list write ───────────────────────────
# Each of these named no list and wrote a junk row before the guard landed.
classify_is "put the chicken in the oven" none "" not-a-simple-edit
classify_is "we need to talk about the milk" none "" not-a-simple-edit
classify_is "grab a bottle of wine on the way home" none "" not-a-simple-edit
# The real buy ask still lands, and the verb never survives into the row.
classify_is "we need to get milk" shopping-add "milk" ""

# ── the meal ops are not hijacked by the shared add/remove verbs ──────────────
classify_is "swap Friday to tacos" meal-swap "" ""
classify_is "remove the pasta" none "" not-a-simple-edit

# ── APPLY: an ASK leaves the plan byte-identical ──────────────────────────────
before_sum="$(sha_of "$plan")"
for held in "Add glorptwax to the shopping list." "Don't add olive oil to the shopping list yet—ask me first."; do
    apply_is "$held" answered
    if [[ "$(sha_of "$plan")" != "$before_sum" ]]; then
        loud_fail "the plan file changed after an ASK that must write nothing: $held"
    fi
done

# ── APPLY: a conversational removal really reaches the plan file ──────────────
grep -q "AA batteries" "$plan" || loud_fail "fixture precondition: AA batteries is on the list"
apply_is "Remove AA batteries again." applied
if grep -q "AA batteries" "$plan"; then
    loud_fail "the removal reported success but the row is still in the plan file"
fi
grep -q "Olive oil" "$plan" || loud_fail "the removal disturbed a row it was not asked about"
grep -q "bean stew" "$plan" || loud_fail "the removal disturbed the dinners table"

# ── APPLY: a removal that matches NO row is answered honestly, never "done" ────
apply_is "Remove AA batteries again." answered nothing-to-remove

# ── APPLY: an add lands, and the plan the family reads really carries it ──────
apply_is "add freezer bags to the shopping list" applied
grep -qi "freezer bags" "$plan" || loud_fail "the add reported success but never reached the plan file"

echo "PASS: telegram_shopping_language"
