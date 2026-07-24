#!/usr/bin/env bash
# Scenario: lifecycle_voice_only_promises_what_happens
#
# Regression (lifecycle-voice-honesty, Luca 2026-07-24). Three rapid messages
# about Saturday's pizza minted three tasks (14:27/14:28/14:29). The dedupe
# correctly abandoned two as duplicates of the survivor — and each abandon
# reached the family as LifecycleEvent::Failed:
#
#     "Ran into a snag on that one — I'll take another crack at it.
#      Sorry for the wait!"
#
# twice, from two personas. Nothing had failed (the survivor landed the pizza)
# and no machinery was going to retry anything. This scenario drives the REAL
# engine tick through the REAL CLI (`wg telegram lifecycle --dry-run`, the same
# code path the live listener fires) over a scratch graph built from the live
# task shapes, and pins four rules:
#
#   1. A duplicate-abandon produces NO family line at all.
#   2. A final failure (retries exhausted) never promises a retry, and raises
#      an operator alert so the ask is not a dead end.
#   3. A failure with a real re-attempt behind it DOES keep the retry promise.
#   4. A genuinely dropped (non-duplicate) abandon is still reported, honestly.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

scratch=$(make_scratch)
cd "$scratch" || loud_fail "cannot cd to scratch $scratch"

wg init >/dev/null 2>&1 || loud_fail "wg init failed in $scratch"

graph="$scratch/.wg/graph.jsonl"
[[ -f "$graph" ]] || : >"$graph"

# The live exchange, verbatim in shape: one survivor + two duplicate-abandons,
# plus a final failure, a retrying failure, and a genuine drop.
origin() { printf '{"channel":"telegram-group","chat_id":"8905220378","requester":"Luca","persona":"%s","bot_id":"%s"}' "$1" "$2"; }

{
  printf '{"kind":"task","id":"update-saturday-dinner-plan-to","title":"Update Saturday dinner plan to pizza","status":"done","created_at":"2026-07-24T14:27:00","origin":%s,"log":[{"timestamp":"2026-07-24T14:38:00+00:00","message":"LIFECYCLE_SUMMARY: Saturday is pizza margherita, homemade dough"}]}\n' "$(origin nora nora)"
  printf '{"kind":"task","id":"change-saturday-dinner-from-pasta","title":"Change Saturday dinner from Pasta to Mozzarella Pizza","status":"abandoned","created_at":"2026-07-24T14:28:00","origin":%s,"failure_reason":"Duplicate of update-saturday-dinner-plan-to — the Telegram listener created three tasks from one request. Consolidated into one owner."}\n' "$(origin otto nora)"
  printf '{"kind":"task","id":"prep-margherita-pizza-for-saturday","title":"prep margherita pizza for saturday dinner","status":"abandoned","created_at":"2026-07-24T14:29:00","origin":%s,"failure_reason":"Duplicate of update-saturday-dinner-plan-to — consolidated into one owner."}\n' "$(origin bruno bruno)"
  printf '{"kind":"task","id":"book-the-saturday-table","title":"book the Saturday table at the trattoria","status":"failed","created_at":"2026-07-24T14:20:00","origin":%s,"failure_reason":"Retry exhausted (3/3 attempts). Last incomplete reason: no answer"}\n' "$(origin otto otto)"
  printf '{"kind":"task","id":"swap-friday-dinner-to-trout","title":"swap Friday dinner to trout","status":"failed","created_at":"2026-07-24T14:21:00","origin":%s,"superseded_by":["rescue-swap-friday-dinner-to-trout"]}\n' "$(origin nora nora)"
  printf '{"kind":"task","id":"order-the-birthday-cake","title":"order the birthday cake","status":"abandoned","created_at":"2026-07-24T14:22:00","origin":%s,"failure_reason":"no longer needed — the bakery closed for the week"}\n' "$(origin otto otto)"
} >"$graph"

wg --dir "$scratch/.wg" list >/dev/null 2>&1 \
    || loud_fail "scratch graph did not load — fixture is malformed"

out=$(wg --dir "$scratch/.wg" telegram lifecycle --dry-run --now 2026-07-24T15:10 2>&1) \
    || loud_fail "wg telegram lifecycle --dry-run errored:\n$out"

# ── Rule 1: the duplicate-abandons are SILENT. ──────────────────────────────
for dup in change-saturday-dinner-from-pasta prep-margherita-pizza-for-saturday; do
    if grep -q "$dup" <<<"$out"; then
        loud_fail "duplicate-abandon '$dup' produced a family line — the exact pizza regression:\n$out"
    fi
done
# Exactly ONE 'failed' line may mention a retry promise (the genuinely retrying
# task) — before the fix there were three.
crack_count=$(grep -c "another crack at it" <<<"$out" || true)
[[ "$crack_count" == "1" ]] \
    || loud_fail "expected exactly 1 retry promise (the task that really retries), got $crack_count:\n$out"

# The survivor's Done line IS the family's whole story about the pizza.
grep -q "Done! Saturday is pizza margherita" <<<"$out" \
    || loud_fail "the survivor's Done line must still reach the family:\n$out"

# ── Rule 2: a final failure is honest, and escalates. ───────────────────────
grep -q "That didn't work out — I've flagged it so it isn't forgotten" <<<"$out" \
    || loud_fail "a retries-exhausted failure must use the honest final copy:\n$out"
grep -q "operator-alert for book-the-saturday-table" <<<"$out" \
    || loud_fail "a dead-end family ask must raise an operator alert:\n$out"

# ── Rule 3: a genuinely scheduled retry keeps the promise. ──────────────────
# (A family line names no task id, so it is identified by its speaking bot:
#  the rescued trout swap is Nora's, the two silenced duplicates were Otto's
#  and Bruno's.)
grep -q "via bot 'nora'.*another crack at it" <<<"$out" \
    || loud_fail "a failure with a rescue behind it must keep the retry promise:\n$out"

# ── Rule 4: a genuine (non-duplicate) drop is reported, without a promise. ──
grep -q "I couldn't finish that one — ask me again if you still want it" <<<"$out" \
    || loud_fail "a non-duplicate abandon must be reported honestly:\n$out"

# No operator alert for anything that is not a dead end.
alert_count=$(grep -c "operator-alert for" <<<"$out" || true)
[[ "$alert_count" == "1" ]] \
    || loud_fail "exactly one dead end here, got $alert_count operator alerts:\n$out"

echo "PASS: duplicate-abandon is silent; failed copy matches what actually happens; dead ends escalate"
