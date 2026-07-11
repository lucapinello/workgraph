#!/usr/bin/env bash
# Smoke: the casa per-agent 1:1 conversation LEDGER writer (docs/15 §ledger,
# WRITE side — the Rust half of conversation-ledger-sync).
#
# A Telegram 1:1 with an agent and an office/kiosk 1:1 with the same agent are
# the SAME conversation, so they land in ONE durable thread per (human, agent)
# pair at `<project-root>/.casa/threads/<human>__<agent>.jsonl`. The Node gateway
# already READS these files; the `wg telegram listen` process is the only one
# holding the Telegram sockets, so it owns the WRITE side: it records the human
# turn durably BEFORE composing and the agent reply AFTER.
#
# This drives the REAL binary end-to-end through the EXACT `casa_ledger` writer
# the listener uses (`wg telegram ledger-write` / `ledger-pending`, the scripted
# seam — same pattern as `wg telegram feed-write`). Credential-free: nothing is
# sent to Telegram.
#
# Pins the full durable contract from the task:
#   1. an inbound 1:1 message lands as a well-formed `role=human` line, with
#      origin=telegram and srcId=<message id>;
#   2. re-delivering the SAME srcId records the human turn EXACTLY ONCE
#      (hasSource dedupe);
#   3. that consumed-not-composed turn is reported as 1 pending reply (the
#      startup-replay primitive) — then 0 once the agent turn is recorded, so a
#      restart replays it exactly once;
#   4. the agent reply lands as a well-formed `role=agent` line;
#   5. PRIVACY: the thread file contains NO bot token, chat id, or user id —
#      only the eight display-safe fields.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING python3" "python3 needed to validate ledger JSON shape"

scratch="$(make_scratch)"   # the project root; `.casa/threads/` is created under it
thread="$scratch/.casa/threads/luca__nora.jsonl"

# The secrets a live listener handles but must NEVER write to a thread. Real
# shapes from a Casa notify.toml: a bot token, a chat id, a Telegram user id.
SECRET_TOKEN="0000000000:nora-dummy-token"
SECRET_CHATID="-1000000000001"
SECRET_USERID="987654321"

ledger_write() { (cd "$scratch" && WG_DIR= wg telegram ledger-write --root "$scratch" "$@"); }
ledger_pending() { (cd "$scratch" && WG_DIR= wg telegram ledger-pending --root "$scratch" --human luca --agent nora); }

echo "1. inbound 1:1 message → a 'role=human' line lands (origin=telegram, srcId set):"
ledger_write --role human --human human-luca --agent nora --sender "Luca" \
    --text "nora, what's for dinner tonight?" --src-id 5521
[ -f "$thread" ] || loud_fail "thread file was not created at $thread"
n=$(grep -c . "$thread")
[ "$n" -eq 1 ] || loud_fail "expected 1 thread line after human write, got $n"
echo "   → $(cat "$thread")"

echo "2. re-delivering the SAME srcId records EXACTLY ONCE (hasSource dedupe):"
out=$(ledger_write --role human --human human-luca --agent nora --sender "Luca" \
    --text "nora, what's for dinner tonight?" --src-id 5521)
echo "   → $out"
case "$out" in
    *duplicate*) : ;;
    *) loud_fail "re-delivery must report 'duplicate', got: $out" ;;
esac
n=$(grep -c . "$thread")
[ "$n" -eq 1 ] || loud_fail "re-delivery must not duplicate: expected 1 line, got $n"

echo "3. consumed-not-composed turn → 1 pending reply (startup-replay primitive):"
p=$(ledger_pending | head -n1)
echo "   → $p"
[ "$p" = "pending 1" ] || loud_fail "expected 'pending 1' after human-only, got: $p"

echo "4. relayed agent reply → a 'role=agent' line lands:"
# A newline in the reply: text must fold to one line; sender maps to the roster.
ledger_write --role agent --human human-luca --agent nora --text $'pasta tonight\nwith fresh basil'
n=$(grep -c . "$thread")
[ "$n" -eq 2 ] || loud_fail "expected 2 thread lines after agent write, got $n"
echo "   → $(tail -n1 "$thread")"

echo "5. pending → 0 after the reply is recorded (so a restart replays exactly once):"
p=$(ledger_pending | head -n1)
echo "   → $p"
[ "$p" = "pending 0" ] || loud_fail "expected 'pending 0' after agent reply, got: $p"

echo "6+7. PRIVACY (no token/chat id/user id) + both lines well-formed with the eight fields:"
# One python pass — reliable across hosts (some `grep` builds on this fleet are
# SIGKILL-happy, and a killed grep would silently read as "no leak").
python3 - "$thread" "$SECRET_TOKEN" "$SECRET_CHATID" "$SECRET_USERID" <<'PY'
import json, sys

thread_path, *secrets = sys.argv[1:]
with open(thread_path) as fh:
    raw = fh.read()

# PRIVACY gate first: the raw bytes must not contain any secret substring, nor
# the field names a leak would ride in on.
for secret in secrets + ["bot_token", "chat_id", "user_id"]:
    assert secret not in raw, f"thread leaked secret substring: {secret!r}"

want = {"ts", "human", "agent", "role", "origin", "sender", "text", "srcId"}
lines = [ln for ln in raw.splitlines() if ln.strip()]
assert len(lines) == 2, f"expected 2 lines, got {len(lines)}"

rows = [json.loads(ln) for ln in lines]
for i, row in enumerate(rows):
    assert set(row.keys()) == want, f"line {i}: keys {set(row.keys())} != {want}"
    assert isinstance(row["ts"], int) and row["ts"] > 0, f"line {i}: bad ts {row['ts']!r}"
    assert row["human"] == "luca", f"line {i}: human={row['human']!r} (human- prefix must strip)"
    assert row["agent"] == "nora", f"line {i}: agent={row['agent']!r}"
    assert row["origin"] == "telegram", f"line {i}: origin={row['origin']!r}"

h, a = rows
assert h["role"] == "human", f"line 0 role={h['role']!r}"
assert h["srcId"] == "5521", f"human line must carry the srcId, got {h['srcId']!r}"
assert h["sender"] == "Luca", f"human sender={h['sender']!r}"

assert a["role"] == "agent", f"line 1 role={a['role']!r}"
assert a["srcId"] is None, f"agent reply has no upstream srcId, got {a['srcId']!r}"
assert a["sender"] == "Nora", f"agent sender from roster, got {a['sender']!r}"
assert a["text"] == "pasta tonight with fresh basil", f"newlines must fold, got {a['text']!r}"

print("   → clean (no secrets); both lines valid: human(srcId=5521) + agent(role=agent)")
PY

echo "PASS: casa ledger writer — inbound 1:1 turn + agent reply land, srcId-deduped, pending replay primitive holds, no secrets on disk"
