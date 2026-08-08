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
# THE SECOND FILE, AND WHY IT IS NOT A NAMING QUIBBLE. Schema v9.1's
# `receipt_fields` is an EXHAUSTIVE object. This writer used to put three keys of
# its own in the LEDGER — `replyPhase`, `outcome`, `attemptId` — and the gateway
# twin validates every ledger line against the exact field set, so an engine
# receipt read as `malformed` from the other side AND `archiveReceiptsThrough`
# threw ("refusing to rotate"), which meant a house whose engine writes receipts
# could never rotate `.casa/group-feed.jsonl` again. The correlation those keys
# carried is still needed (the refire guard keys on `(turn, attempt)`), so it
# lives in the never-rotated INDEX — out of the evidence, in the accelerator.
# Legs 6 and 8 therefore read PHASE and ATTEMPT from here, and leg 6 asserts the
# ledger does NOT carry them.
index="$scratch/.casa/relay-receipt-index.jsonl"

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
#
# The help text is CAPTURED FIRST rather than piped into `grep -q`. Under
# `set -o pipefail` a `-q` grep exits at the first match, `wg` dies of SIGPIPE
# writing the rest of its help, and the pipeline reports failure — so the probe
# went red against a binary that HAS the seam, and this scenario failed for
# every build, correct or stale, which is a gate that pins nothing.
help_text="$(feed_write --help 2>&1 || true)"
case "$help_text" in
    *--turn-id*) ;;
    *) loud_fail "the installed wg has no 'feed-write --turn-id' seam — stale install (this is the state this scenario pins)" ;;
esac

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
    # `--reply-phase` IS SUPPLIED, and that is load-bearing. A turn-bound row
    # must declare its phase (the writer may not GUESS it), and that guard fires
    # BEFORE the turn-id validation this leg exists to prove — so without the
    # flag all three of these were refused for the wrong reason and the leg
    # passed against a build with the turn-id check removed entirely.
    err=$(feed_write --kind agent --agent-id otto --bot-id otto \
        --text "should never land" --turn-id "$bad" --reply-phase final \
        --message-id 999 2>&1) \
        && loud_fail "an unjoinable turn id was ACCEPTED: $bad"
    # …and refused BY THE TURN-ID RULE, named in the refusal.
    case "$err" in
        *"causal turn id is not a raw web-turn-"*) ;;
        *) loud_fail "turn id $bad was refused for the WRONG reason: $err" ;;
    esac
done
rows_after=$(grep -c . "$feed")
receipts_after=$(grep -c . "$ledger")
[ "$rows_before" -eq "$rows_after" ] \
    || loud_fail "a refused turn id still wrote a row ($rows_before → $rows_after)"
[ "$receipts_before" -eq "$receipts_after" ] \
    || loud_fail "a refused turn id still wrote a receipt ($receipts_before → $receipts_after)"
echo "   → all three refused; feed and ledger byte-count unchanged"

echo "3. the REPLAY guard: a second claim of the same (scope, message id) is refused at write,"
echo "   and the REFUSED RECEIPT TAKES ITS ROW WITH IT (one transaction):"
rows_before=$(grep -c . "$feed")
# `--reply-phase` supplied for the same reason as leg 2: the phase guard fires
# first, so without it this leg was refused before the replay guard was ever
# consulted and would have passed with the replay guard deleted.
err=$(feed_write --kind agent --agent-id otto --bot-id otto \
    --text "A replayed claim." \
    --turn-id "web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c3309" \
    --reply-phase final --message-id 501 2>&1) \
    && loud_fail "a replayed claim of message 501 was accepted"
# Refused BY THE REPLAY GUARD: the refusal names the delivery already on record
# and the receipt that holds it.
case "$err" in
    *"is already recorded by rcpt_"*) ;;
    *) loud_fail "the replayed claim was refused for the WRONG reason: $err" ;;
esac
# The row and the receipt are ONE transaction. Written in two, the refused
# receipt would leave "A replayed claim." in the family's conversation with
# nothing able to prove it — the exact unprovable row the contract removes.
[ "$(grep -c . "$feed")" -eq "$rows_before" ] \
    || loud_fail "the refused receipt left its row behind — the write was not one transaction"
grep -q "A replayed claim." "$feed" \
    && loud_fail "an unprovable row survived its refused receipt"
echo "   → refused, and no row survived it"

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
python3 - "$feed" "$ledger" "$index" "$TURN_A" "$TURN_B" "$SECRET_TOKEN" "$SECRET_CHATID" "$SECRET_USERID" <<'PY'
import hashlib, json, sys

feed_path, ledger_path, index_path, turn_a, turn_b, *secrets = sys.argv[1:]
feed_raw = open(feed_path).read()
ledger_raw = open(ledger_path).read()
index_raw = open(index_path).read()

# PRIVACY: none of the three files may carry a secret substring, nor the field
# names a leak would ride in on.
for blob, what in ((feed_raw, "feed"), (ledger_raw, "ledger"), (index_raw, "index")):
    for secret in secrets + ["bot_token", "chat_id", "user_id"]:
        assert secret not in blob, f"{what} leaked secret substring: {secret!r}"

rows = [json.loads(l) for l in feed_raw.splitlines() if l.strip()]
receipts = [json.loads(l) for l in ledger_raw.splitlines() if l.strip()]
index = [json.loads(l) for l in index_raw.splitlines() if l.strip()]
by_receipt_id = {e["receiptId"]: e for e in index}

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

    # EXHAUSTIVE v9.1 `receipt_fields`, and NOT ONE KEY MORE. This is the
    # assertion whose absence let the unreadable-receipt wedge ship: the gateway
    # twin rejects unknown keys, so a fourth key here is a ledger line the other
    # half reads as `malformed` — and one malformed line makes its
    # `archiveReceiptsThrough` throw, so the feed stops rotating forever.
    assert set(r) == {
        "receiptId", "turnId", "feedId", "feedKind", "roleId",
        "transportScopeId", "messageId", "acceptedAtMs", "status", "provenance",
    }, f"ledger receipt drifted from v9.1 receipt_fields: {sorted(set(r))}"
    # The three keys this writer used to smuggle into the evidence live in the
    # INDEX now. Named individually so a regression says WHICH one came back.
    for moved in ("replyPhase", "outcome", "attemptId"):
        assert moved not in r, \
            f"{moved!r} is back in the LEDGER — v9.1 forbids it and the twin will refuse to rotate"

    # …and the correlation is still recorded, in the accelerator, joined by
    # receiptId. Dropping it from the ledger must not mean losing it.
    entry = by_receipt_id.get(r["receiptId"])
    assert entry is not None, f"no index entry for receipt {r['receiptId']}"
    assert entry["replyPhase"] == "final", entry["replyPhase"]
    assert entry["feedId"] == r["feedId"], "the index names a different row than the receipt"
    assert entry["turnId"] == r["turnId"], "the index names a different turn than the receipt"

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

echo "7. the ATTEMPT is MINTED, not counted — a counted id writes nothing at all:"
TURN_C="web-turn-3f2504e0-4f89-41d3-9a0c-0305e82c330a"
ATTEMPT_ONE="attempt-6ba7b810-9dad-41d1-80b4-00c04fd430c8"
ATTEMPT_TWO="attempt-6ba7b810-9dad-41d1-80b4-00c04fd430c9"
rows_before=$(grep -c . "$feed")
# `1`/`2` collide across unrelated processes by construction: one turn's real
# second attempt then keys the same as another's first, and the receipt for the
# send that actually reached the family is suppressed as a refire.
# `--reply-phase` supplied for the same reason as legs 2 and 3 — otherwise the
# phase guard refuses this before WG_ATTEMPT_ID is ever parsed, and the leg
# passes against a build that accepts counted attempt ids happily.
err=$(cd "$scratch" && WG_DIR= WG_ATTEMPT_ID=1 wg telegram feed-write --root "$scratch" \
        --kind agent --agent-id otto --bot-id otto --text "counted attempt" \
        --turn-id "$TURN_C" --reply-phase final --message-id 601 2>&1) \
    && loud_fail "a COUNTED attempt id was accepted"
# Refused BY THE TYPED-ID RULE, naming attemptId.
case "$err" in
    *"attemptId is not a valid typed id"*) ;;
    *) loud_fail "the counted attempt id was refused for the WRONG reason: $err" ;;
esac
[ "$(grep -c . "$feed")" -eq "$rows_before" ] \
    || loud_fail "a refused attempt id still left a row behind"
echo "   → refused; feed unchanged"

echo "8. a SELF-HEAL RETRY is not a refire — attempt 2 of one turn writes its own receipt:"
attempt_write() {
    local attempt="$1" mid="$2" text="$3"
    (cd "$scratch" && WG_DIR= WG_ATTEMPT_ID="$attempt" wg telegram feed-write --root "$scratch" \
        --kind agent --agent-id otto --bot-id otto --text "$text" \
        --turn-id "$TURN_C" --reply-phase final --message-id "$mid")
}
receipts_before=$(grep -c . "$ledger")
attempt_write "$ATTEMPT_ONE" 601 "attempt one" >/dev/null \
    || loud_fail "a minted attempt id was refused"
# A REFIRE of the same attempt writes nothing...
if attempt_write "$ATTEMPT_ONE" 602 "a refire" >/dev/null 2>&1; then
    loud_fail "a refire of one attempt wrote a second receipt"
fi
# ...while the gateway's self-heal retry — a NEW attempt on the same turn — does.
attempt_write "$ATTEMPT_TWO" 603 "attempt two" >/dev/null \
    || loud_fail "a self-heal retry was suppressed as a refire"
[ "$(grep -c . "$ledger")" -eq "$((receipts_before + 2))" ] \
    || loud_fail "expected exactly two more receipts (attempt one and attempt two)"
# WHICH ATTEMPT reached the family is recorded in the INDEX, not the ledger:
# `attemptId` is not one of v9.1's `receipt_fields` (see the note beside
# $index above). Asserted here rather than merely dropped — the correlation is
# the point of the leg, and the file it lives in is an implementation detail
# the gate must follow, not a reason to stop checking.
grep -q '"attemptId":"'"$ATTEMPT_TWO"'"' "$index" \
    || loud_fail "the retry's receipt does not record WHICH attempt reached the family"
if grep -q '"attemptId":"'"$ATTEMPT_TWO"'"' "$ledger"; then
    loud_fail "attemptId is back in the v9.1 LEDGER — the twin will read it as malformed and refuse to rotate"
fi
echo "   → attempt 1 recorded, its refire refused, attempt 2 recorded"

echo "PASS: engine receipt contract — exact-row join by global feed id, raw turn ids only, inbound stamped, seal holds, replay refused, one transaction, minted attempts"
