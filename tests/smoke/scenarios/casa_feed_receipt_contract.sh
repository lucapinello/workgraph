#!/usr/bin/env bash
# Smoke: the v8 RECEIPT CONTRACT on the ENGINE path, through the deployed binary.
#
# WHY THIS EXISTS. A feed row is what the pane SHOWS. It was never evidence that
# anything reached the family: the row is written locally, the relay's
# `{ok, message_id}` was discarded, and the listener drops the bot's own echo by
# design — so "the helper replied" was, end to end, a claim the writer made about
# itself. This scenario pins the parts that make a row PROVABLE, driving the real
# `casa_feed` + `relay_receipt` writers the listener uses (`wg telegram
# feed-write`, the scripted seam). Credential-free: nothing is sent to Telegram.
#
# What is gated here, and the failure each leg names:
#
#   1. THE GLOBAL FEED ID. Every row is allocated one under the cross-process
#      feed lock, and the receipt joins on it. TWO ROWS IN THE SAME MILLISECOND
#      get DIFFERENT ids and each receipt names its own row — the case where an
#      ordinal or timestamp join picks the WRONG row.
#   2. RAW vs HASHED TURN ID. `web_physical_turn_key()` hashes the turn into
#      `web-turn-<64hex>` for the engine's INTERNAL delivery digest. A hashed id,
#      an all-hyphen placeholder and a wrong-version uuid each produce NO ROW and
#      NO RECEIPT — never a row carrying an id no gateway row could ever join.
#   3. THE INBOUND STAMP. A human's group message is `kind:group` +
#      `nonRelayType:telegram-inbound`: it was never relayed, so it says why no
#      receipt could prove it instead of merely lacking one.
#   4. THE SEALED RUN. An UNBOUND agent row is refused outright, while a normal
#      household group message during that same sealed run still lands and
#      creates NO unbound fatal row.
#   5. THE REPLAY GUARD. One Telegram message can only be delivered once, so a
#      second receipt claiming the same (scope, message id) is refused AT WRITE.
#   6. PRIVACY. No token, chat id or user id reaches the feed or the ledger, and
#      the transport-scope id is not a raw sha of the bot id (a bot roster is a
#      handful of short stable strings — an unkeyed digest is reversible by
#      dictionary in milliseconds).
#
# A binary without the seam FAILS rather than skipping: a stale install is
# exactly the state this pins.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING python3" "python3 needed to validate the receipt shapes"

scratch="$(make_scratch)"
feed="$scratch/.casa/group-feed.jsonl"
ledger="$scratch/.casa/relay-receipts.jsonl"

cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "otto"
name = "Otto"
emoji = "\U0001FA90"
TOML

# The secrets a live listener handles and must never write anywhere.
SECRET_TOKEN="0000000000:otto-dummy-token"
SECRET_CHATID="-1000000000001"
SECRET_USERID="987654321"

# A RAW accepted turn id, exactly the shape the gateway mints.
TURN_A="web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3301"
TURN_B="web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3302"

feed_write() {
    (cd "$scratch" && WG_DIR= wg telegram feed-write --root "$scratch" "$@")
}

# The seam must EXIST. A binary predating it would otherwise sail through every
# negative leg below by failing them for the wrong reason.
if ! feed_write --help 2>&1 | grep -q -- "--turn-id"; then
    loud_fail "the installed wg has no 'feed-write --turn-id' seam — stale install (this is the state this scenario pins)"
fi

echo "1. two engine replies in the SAME turn get DISTINCT global feed ids, each receipt naming its own row:"
out_a=$(feed_write --kind agent --agent-id otto --bot-id otto \
    --text "The FIRST answer." --turn-id "$TURN_A" --reply-phase final --message-id 501)
out_b=$(feed_write --kind agent --agent-id otto --bot-id otto \
    --text "The SECOND answer." --turn-id "$TURN_B" --reply-phase final --message-id 502)
id_a=$(echo "$out_a" | sed -n 's/^feedId=//p')
id_b=$(echo "$out_b" | sed -n 's/^feedId=//p')
[ -n "$id_a" ] && [ -n "$id_b" ] || loud_fail "feed-write did not report the allocated feedId"
[ "$id_a" != "$id_b" ] || loud_fail "two rows were allocated the SAME global feed id ($id_a)"
echo "$out_a" | grep -q '^receipt=written' || loud_fail "no engine receipt for the first row"
echo "$out_b" | grep -q '^receipt=written' || loud_fail "no engine receipt for the second row"
echo "   → feedId $id_a and $id_b, one receipt each"

echo "2. a HASHED / all-hyphen / wrong-version turn id writes NO row and NO receipt:"
rows_before=$(grep -c . "$feed")
receipts_before=$(grep -c . "$ledger")
for bad in \
    "web-turn-1e4d3c2b1a09f8e7d6c5b4a3928170695e4d3c2b1a09f8e7d6c5b4a392817069" \
    "web-turn-00000000-0000-0000-0000-000000000000" \
    "web-turn-3f2504e0-4f89-11d3-9a0c-0305e82c3301" ; do
    if feed_write --kind agent --agent-id otto --bot-id otto \
        --text "should never land" --turn-id "$bad" --message-id 999 >/dev/null 2>&1; then
        loud_fail "an unjoinable turn id was ACCEPTED: $bad"
    fi
done
rows_after=$(grep -c . "$feed")
receipts_after=$(grep -c . "$ledger")
[ "$rows_before" -eq "$rows_after" ] \
    || loud_fail "a refused turn id still wrote a row ($rows_before → $rows_after)"
[ "$receipts_before" -eq "$receipts_after" ] \
    || loud_fail "a refused turn id still wrote a receipt ($receipts_before → $receipts_after)"
echo "   → all three refused; feed and ledger byte-count unchanged"

echo "3. the REPLAY guard: a second claim of the same (scope, message id) is refused at write:"
if feed_write --kind agent --agent-id otto --bot-id otto \
    --text "A replayed claim." \
    --turn-id "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3309" \
    --message-id 501 >/dev/null 2>&1; then
    loud_fail "a replayed claim of message 501 was accepted"
fi
echo "   → refused"

echo "4. an inbound human message is writer-stamped kind:group + nonRelayType:telegram-inbound:"
feed_write --kind group --sender nadin --text "what's for dinner tonight?" \
    --non-relay-type telegram-inbound >/dev/null

echo "5. a SEALED run refuses an unbound agent row but never a household message:"
printf '{"sealed":true}\n' >"$scratch/.casa/feed-seal.json"
rows_before=$(grep -c . "$feed")
if feed_write --kind agent --agent-id otto --text "An untraceable report-back." >/dev/null 2>&1; then
    loud_fail "a sealed run accepted an UNBOUND agent row"
fi
[ "$(grep -c . "$feed")" -eq "$rows_before" ] \
    || loud_fail "the sealed refusal still wrote a row"
# The family keeps talking. A message someone actually said must never be lost
# to a certification gate — that would be the gate causing the harm it audits.
feed_write --kind group --sender nadin --text "we're out of milk" \
    --non-relay-type telegram-inbound >/dev/null
[ "$(grep -c . "$feed")" -eq "$((rows_before + 1))" ] \
    || loud_fail "a household group message was dropped during a sealed run"
rm -f "$scratch/.casa/feed-seal.json"
echo "   → unbound agent row refused; the household message landed"

echo "6. shapes + privacy over the whole feed and ledger:"
python3 - "$feed" "$ledger" "$TURN_A" "$TURN_B" "$SECRET_TOKEN" "$SECRET_CHATID" "$SECRET_USERID" <<'PY'
import hashlib, json, sys

feed_path, ledger_path, turn_a, turn_b, *secrets = sys.argv[1:]
feed_raw = open(feed_path).read()
ledger_raw = open(ledger_path).read()

# PRIVACY: neither file may carry a secret substring, nor the field names a leak
# would ride in on.
for blob, what in ((feed_raw, "feed"), (ledger_raw, "ledger")):
    for secret in secrets + ["bot_token", "chat_id", "user_id"]:
        assert secret not in blob, f"{what} leaked secret substring: {secret!r}"

rows = [json.loads(l) for l in feed_raw.splitlines() if l.strip()]
receipts = [json.loads(l) for l in ledger_raw.splitlines() if l.strip()]

# Rows are one-based and dense: the receipt's feedId indexes them directly.
assert len(receipts) == 2, f"expected exactly 2 receipts, got {len(receipts)}"

by_turn = {r["turnId"]: r for r in receipts}
assert set(by_turn) == {turn_a, turn_b}, f"receipt turns {set(by_turn)}"

a, b = by_turn[turn_a], by_turn[turn_b]
assert a["feedId"] != b["feedId"], "two receipts named the same row"
assert rows[a["feedId"] - 1]["text"] == "The FIRST answer.", "receipt A joined the WRONG row"
assert rows[b["feedId"] - 1]["text"] == "The SECOND answer.", "receipt B joined the WRONG row"

for r in receipts:
    # RAW turn id, verbatim — never the engine's internal hashed key.
    assert r["turnId"].startswith("web-turn-") and len(r["turnId"]) == 45, r["turnId"]
    assert r["provenance"] == "engine", f"written BY the engine, got {r['provenance']!r}"
    assert r["status"] == "delivered", r["status"]
    assert isinstance(r["messageId"], int) and r["messageId"] > 0, r["messageId"]
    assert r["receiptId"].startswith("rcpt_"), r["receiptId"]
    assert r["replyPhase"] == "final", r["replyPhase"]
    # The transport scope id names the ACTUAL SENDING BOT through a KEYED digest.
    scope = r["transportScopeId"]
    assert scope.startswith("ts_") and len(scope) == 67, scope
    raw_sha = hashlib.sha256(b"otto").hexdigest()
    assert scope != f"ts_{raw_sha}" and scope != raw_sha, \
        "the transport scope id is a RAW sha of the bot id — reversible by dictionary"

# The rows carry the causal trio; the human's rows are BOUND by nonRelayType,
# never left unbound.
for row in rows:
    for key in ("turnId", "replyPhase", "nonRelayType"):
        assert key in row, f"row missing {key}: {row}"
    bound = row["turnId"] is not None or row["nonRelayType"] is not None
    assert bound, f"an UNBOUND row reached the certifying feed: {row}"

humans = [r for r in rows if r["kind"] == "group"]
assert humans, "no inbound group rows were written"
for h in humans:
    assert h["nonRelayType"] == "telegram-inbound", h
    assert h["turnId"] is None, "a human's own message has no causal turn"
assert any(h["text"] == "we're out of milk" for h in humans), \
    "the household message sent during the sealed run is missing from the record"

print(f"   → {len(rows)} rows, {len(receipts)} receipts, every row bound, no secrets, keyed scope ids")
PY

echo "PASS: engine receipt contract — exact-row join by global feed id, raw turn ids only, inbound stamped, seal holds, replay refused"
