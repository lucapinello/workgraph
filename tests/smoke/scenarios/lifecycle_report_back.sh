#!/usr/bin/env bash
# Smoke: the conversational feedback loop — asks report back.
#
# Luca's north-star interaction is the back-and-forth: he asks a persona in chat
# to change the week, the family does the work, and he HEARS about it — "on it",
# then "done, here's what changed". Before this the pipeline did the work
# invisibly (a task got created but no start/done ever came back), so it looked
# like a bug. `wg telegram lifecycle` is the CLI seam the coordinator's tick
# invokes (same credential-free pattern as `wg telegram remind` / `elect`): it
# reads origin-stamped tasks, derives each one's start/done/fail event from live
# status, and reports back to the exact chat via the composing persona's bot —
# without touching a live bot on `--dry-run`.
#
# This drives the REAL binary end-to-end and pins the behaviour the unit tests
# assert, through the CLI a human/scheduler actually invokes:
#   1. an in-progress origin-stamped task WOULD send an "on it" line, naming the
#      worker, to the ORIGIN chat via the ORIGIN persona's bot;
#   2. a done task WOULD send the payoff with WHAT CHANGED (the family-voice
#      LIFECYCLE_SUMMARY line);
#   3. a task with NO origin (an internal chore) is never reported on;
#   4. a real fire records durable state so a restart NEVER re-reports (exactly
#      once) — the loop is not a nag.
# Credential-free: no notify.toml, so nothing is sent; state files prove intent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
# A stale deployed binary (pre-`close-the-conversational`) has no `lifecycle`
# subcommand — skip loudly rather than FAIL, so this only gates a wg built with it.
if ! wg telegram lifecycle --help >/dev/null 2>&1; then
    loud_skip "STALE WG BINARY" "wg has no 'telegram lifecycle' subcommand; rebuild from the fork"
fi

scratch="$(make_scratch)"
export WG_DIR="$scratch/.wg"
mkdir -p "$scratch/.wg"

# Three tasks: an in-progress ask (assigned to Nora), a done ask carrying a
# family-voice change summary, and an internal chore with NO origin stamp.
cat > "$scratch/.wg/graph.jsonl" <<'JSONL'
{"kind":"task","id":"tweak-w29-meals","title":"tweak this week's meals","status":"in_progress","assigned":"nora","origin":{"channel":"telegram-1:1","chat_id":"555","requester":"Luca","persona":"otto","bot_id":"otto"}}
{"kind":"task","id":"book-dentist","title":"book the dentist","status":"done","assigned":"bruno","log":[{"timestamp":"2026-07-13T10:00:00","message":"LIFECYCLE_SUMMARY: booked for next Tuesday at 3pm"}],"origin":{"channel":"telegram-1:1","chat_id":"555","requester":"Luca","persona":"otto","bot_id":"otto"}}
{"kind":"task","id":"internal-chore","title":"tidy the backlog","status":"in_progress"}
JSONL

life() { WG_DIR="$scratch/.wg" wg telegram lifecycle "$@"; }

echo "1. in-progress ask WOULD send 'on it', naming the worker, to the origin chat/bot:"
out="$(life --dry-run --now 2026-07-13T12:58)"
echo "$out"
echo "$out" | grep -q "started → chat 555 via bot 'otto' as 'otto'" \
    || loud_fail "start must route to the ORIGIN chat via the ORIGIN persona's bot: $out"
echo "$out" | grep -q "Nora is on it" \
    || loud_fail "start line must name the worker: $out"

echo "2. done ask WOULD send the payoff with WHAT CHANGED:"
echo "$out" | grep -q "done → chat 555" || loud_fail "expected a done report: $out"
echo "$out" | grep -q "Done! booked for next Tuesday at 3pm" \
    || loud_fail "done line must carry the family-voice change summary: $out"

echo "3. the un-stamped internal chore is NEVER reported on:"
echo "$out" | grep -q "internal-chore" && loud_fail "un-origin-stamped tasks must not be reported: $out"
echo "$out" | grep -q "tidy the backlog" && loud_fail "internal chore leaked into a report: $out"
echo "   → internal chore correctly silent"

echo "4. a real fire records durable state so a restart never re-reports (exactly once):"
# No notify.toml → the actual send has no bot and fails loudly, but state is
# persisted BEFORE the send attempt (restart-safe), so the ids are now handled.
life --now 2026-07-13T12:58 >/dev/null 2>&1 || true
# `.casa/` lives at the PROJECT ROOT (the parent of `.wg`), same as the reminder
# and errand engines' durable state.
test -f "$scratch/.casa/reminders-state.json" \
    || loud_fail "expected durable lifecycle state after a real fire"
out2="$(life --dry-run --now 2026-07-13T13:30)"
echo "$out2" | grep -q "started → " && loud_fail "a fired start must not re-report: $out2"
echo "$out2" | grep -q "done → " && loud_fail "a fired done must not re-report: $out2"
echo "$out2" | grep -qi "Nothing to report" \
    || loud_fail "after firing, the loop is quiet (not a nag): $out2"
echo "   → $out2"

echo "5. a report-back is a REPLY, so it fires standalone even when the proactive"
echo "   standalone cap is already spent (the pesto-round-2 regression):"
# Live root cause of the 2nd failed test: Luca made a burst of meal-swap asks;
# the day's proactive standalone cap (default 3) was spent, so the "done" reply
# was folded into the NEXT-MORNING digest instead of sent — the loop looked
# broken. A reply to the human's own ask must bypass that cap. Seed the pacing
# store with the cap already spent for Luca, add a fresh done origin task, and
# assert the dry-run reports it standalone — NOT "folds into digest".
cat > "$scratch/.casa/digest-state.json" <<'JSON'
{"people":{"Luca":{"day":"2026-07-13","standalone_sent":3,"digest_sent":false,"pending":[],"seen":[]}}}
JSON
cat >> "$scratch/.wg/graph.jsonl" <<'JSONL'
{"kind":"task","id":"swap-friday-to-pesto","title":"swap Friday dinner to pesto","status":"done","assigned":"nora","log":[{"timestamp":"2026-07-13T12:00:00","message":"LIFECYCLE_SUMMARY: Friday is now pesto pasta"}],"origin":{"channel":"telegram-1:1","chat_id":"555","requester":"Luca","persona":"otto","bot_id":"otto"}}
JSONL
out5="$(life --dry-run --now 2026-07-13T14:00)"
echo "$out5"
echo "$out5" | grep -q "done → chat 555" \
    || loud_fail "the report-back must fire even with the standalone cap spent: $out5"
echo "$out5" | grep -q "Done! Friday is now pesto pasta" \
    || loud_fail "the payoff must carry the family-voice change summary: $out5"
# The pesto done is the only not-yet-fired report-back in this dry-run (steps 1-4
# already fired the others), so ANY "folds into digest" here is the regression.
if echo "$out5" | grep -qi "folds into digest"; then
    loud_fail "a reply must NOT be capped into the digest — the pesto regression: $out5"
fi
echo "   → report-back reached the human standalone despite the spent cap"

echo "6. CROSS-SURFACE (lifecycle-messages-obey): a GROUP report-back lands in BOTH"
echo "   Telegram (verified send, message id logged) AND the pane feed"
echo "   (.casa/group-feed.jsonl the constellation pane reads), exactly once:"
# --mock-send is a hidden flag on a wg built with this fix — run the REAL tick +
# REAL casa-feed mirror while recording (not sending) the Telegram call, so the
# cross-surface contract is exercised credential-free. A stale binary lacks the
# flag and clap rejects it — skip loudly rather than FAIL.
probe="$(life --mock-send --dry-run --now 2026-07-13T15:00 2>&1 || true)"
if echo "$probe" | grep -qiE "unexpected argument|unrecognized|invalid value|no such"; then
    loud_skip "STALE WG BINARY" "wg telegram lifecycle has no --mock-send; rebuild from the fork"
fi

# A fresh GROUP-origin in-progress ask (elected voice: Nora). Scoped by task id
# so this step is isolated from the 1:1 tasks above.
cat >> "$scratch/.wg/graph.jsonl" <<'JSONL'
{"kind":"task","id":"swap-sat-lunch-group","title":"swap Saturday lunch","status":"in_progress","assigned":"nora","origin":{"channel":"telegram-group","chat_id":"-100999","requester":"Luca","persona":"nora","bot_id":"nora"}}
JSONL
feed="$scratch/.casa/group-feed.jsonl"

out6="$(life swap-sat-lunch-group --mock-send --json --now 2026-07-13T15:00)"
echo "$out6"
# TELEGRAM surface: sent exactly once, none undelivered.
echo "$out6" | grep -q '"sent":1' \
    || loud_fail "the group report-back must be sent (delivery verified) exactly once: $out6"
echo "$out6" | grep -q '"undelivered":0' \
    || loud_fail "no undelivered report-backs expected on a confirmed send: $out6"
# LEDGER surface: exactly one 'is on it' agent line landed in the pane feed.
test -f "$feed" || loud_fail "the group report-back never reached the pane feed: $feed missing"
n_isonit=$(grep -c 'is on it' "$feed" || true)
[ "$n_isonit" -eq 1 ] || loud_fail "expected exactly one 'is on it' feed line, got $n_isonit:
$(cat "$feed")"
grep -q '"kind":"agent"' "$feed" \
    || loud_fail "the mirrored line must be an agent line: $(cat "$feed")"
grep -q '"agentId":"nora"' "$feed" \
    || loud_fail "the mirrored line must be attributed to the persona: $(cat "$feed")"
echo "   → the 'is on it' report-back is in the pane feed AND was sent, exactly once"

echo "6b. EXACTLY-ONCE across surfaces: a re-tick re-fires nothing and adds NO 2nd feed line:"
out6b="$(life swap-sat-lunch-group --mock-send --json --now 2026-07-13T15:05)"
echo "$out6b" | grep -q '"fired":0' \
    || loud_fail "an already-reported group transition must not re-fire: $out6b"
n_isonit2=$(grep -c 'is on it' "$feed" || true)
[ "$n_isonit2" -eq 1 ] \
    || loud_fail "the pane feed gained a duplicate 'is on it' line ($n_isonit2):
$(cat "$feed")"
echo "   → re-tick is quiet on both surfaces (FiredLog exactly-once)"

echo "6c. a 1:1 DM report-back is NEVER written into the SHARED group feed (privacy):"
# Only the group report-back may sit in the shared pane feed — the 1:1 origin
# tasks (tweak-w29-meals, book-dentist, pesto) must never leak into it.
lines_in_feed=$(grep -c . "$feed" || true)
[ "$lines_in_feed" -eq 1 ] \
    || loud_fail "the shared feed must hold ONLY the one group line, has $lines_in_feed:
$(cat "$feed")"
echo "   → only the group report-back is in the shared pane; no 1:1 leak"

echo "PASS: the conversational loop reports start + done back to the origin chat, exactly once, a reply is never capped into the digest, and a GROUP report-back lands in BOTH Telegram and the pane feed exactly once (no 1:1 leak)."
