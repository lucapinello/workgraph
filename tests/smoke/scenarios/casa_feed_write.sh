#!/usr/bin/env bash
# Smoke: the casa conversation-pane feed WRITER (docs/15 §chat-split, WRITE side).
#
# The constellation split view's conversation pane is a live mirror of the
# family Telegram group. The `wg telegram listen` process is the only one holding
# the Telegram sockets, so it owns the WRITE side: it appends one JSONL line to
# `<project-root>/.casa/group-feed.jsonl` for every inbound GROUP message and
# every AGENT reply it relays into the group. The casa gateway tails that file
# and serves it at `GET /conversation`.
#
# This drives the REAL binary end-to-end through the EXACT `casa_feed` writer the
# listener uses (`wg telegram feed-write`, the scripted seam — same pattern as
# `wg telegram elect` exposing the listener's election decision without a live
# socket). Credential-free: nothing is sent to Telegram.
#
# Pins three things:
#   1. an inbound group message lands as a well-formed `group` line (agentId
#      null — a human — emoji empty);
#   2. a relayed agent reply lands as a well-formed `agent` line (agentId mapped
#      from the roster, lower-cased; emoji from the roster);
#   3. PRIVACY: the feed file contains NO bot token, chat id, or user id — only
#      the six display-safe fields.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING python3" "python3 needed to validate feed JSON shape"

scratch="$(make_scratch)"           # the project root; `.casa/` is created under it
feed="$scratch/.casa/group-feed.jsonl"

# The secrets a live listener handles but must NEVER write to the feed. These are
# the real shapes from a Casa notify.toml: a bot token, the group chat id, and a
# Telegram user id.
SECRET_TOKEN="0000000000:otto-dummy-token"
SECRET_CHATID="-1000000000001"
SECRET_USERID="987654321"

feed_write() {
    (cd "$scratch" && WG_DIR= wg telegram feed-write --root "$scratch" "$@")
}

echo "1. inbound group message → a 'group' line lands:"
feed_write --kind group --sender nadin --text "otto, what's for dinner tonight?"
[ -f "$feed" ] || loud_fail "feed file was not created at $feed"
n=$(grep -c . "$feed")
[ "$n" -eq 1 ] || loud_fail "expected 1 feed line after group write, got $n"
echo "   → $(cat "$feed")"

echo "2. relayed agent reply → an 'agent' line lands:"
# Mixed-case + a newline in the reply: agentId must lower-case, text must fold to
# one line. Roster mapping gives sender='Otto', emoji from the roster.
feed_write --kind agent --agent-id Otto --text $'pasta tonight\nwith fresh basil'
n=$(grep -c . "$feed")
[ "$n" -eq 2 ] || loud_fail "expected 2 feed lines after agent write, got $n"
echo "   → $(tail -n1 "$feed")"

echo "3+4. PRIVACY (no token/chat id/user id) + both lines well-formed with the six fields:"
# Both checks run in one python pass — reliable across hosts (some `grep` builds
# on this fleet are SIGKILL-happy, and a killed grep would silently read as
# "no leak", so the privacy gate must not lean on it).
python3 - "$feed" "$SECRET_TOKEN" "$SECRET_CHATID" "$SECRET_USERID" <<'PY'
import json, sys

feed_path, *secrets = sys.argv[1:]
with open(feed_path) as fh:
    raw = fh.read()

# PRIVACY gate first: the raw bytes must not contain any secret substring, nor
# the field names a leak would ride in on.
for secret in secrets + ["bot_token", "chat_id", "user_id"]:
    assert secret not in raw, f"feed leaked secret substring: {secret!r}"

want = {"ts", "sender", "agentId", "emoji", "kind", "text"}
lines = [ln for ln in raw.splitlines() if ln.strip()]

assert len(lines) == 2, f"expected 2 lines, got {len(lines)}"

rows = [json.loads(ln) for ln in lines]
for i, row in enumerate(rows):
    assert set(row.keys()) == want, f"line {i}: keys {set(row.keys())} != {want}"
    assert isinstance(row["ts"], int) and row["ts"] > 0, f"line {i}: bad ts {row['ts']!r}"

g, a = rows
assert g["kind"] == "group", f"line 0 kind={g['kind']!r}"
assert g["agentId"] is None, f"human sender must have agentId null, got {g['agentId']!r}"
assert g["emoji"] == "", f"human sender must have empty emoji, got {g['emoji']!r}"
assert g["sender"] == "nadin", f"sender={g['sender']!r}"

assert a["kind"] == "agent", f"line 1 kind={a['kind']!r}"
assert a["agentId"] == "otto", f"agentId must be lower-cased roster id, got {a['agentId']!r}"
assert a["sender"] == "Otto", f"agent sender from roster, got {a['sender']!r}"
assert a["emoji"], f"agent line must carry a roster emoji, got {a['emoji']!r}"
assert a["text"] == "pasta tonight with fresh basil", f"newlines must fold, got {a['text']!r}"

print("   → clean (no secrets); both lines valid: group(agentId=null) + agent(agentId=otto)")
PY

echo "PASS: casa feed writer — inbound group + relayed agent reply land, roster-mapped, no secrets on disk"
