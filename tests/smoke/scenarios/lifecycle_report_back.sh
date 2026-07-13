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

echo "PASS: the conversational loop reports start + done back to the origin chat, exactly once."
