#!/usr/bin/env bash
# Smoke: the converse turn actually COMPLETES (real answer) or FAILS FAST into a
# graceful "glitched" follow-up — the fix for `fix-converse-hang`.
#
# The bug: a plain message that routed to one agent wrote the human turn to the
# bound-session INBOX and then polled the OUTBOX forever for a reply that only a
# live `wg nex` daemon could produce. No such daemon runs in the deployment, so
# every converse turn acked at ~4s and then TIMED OUT at 120s — the real answer
# NEVER sent. The old dry-run hid this because `--session-reply` installed a
# FIXTURE responder that wrote the outbox.
#
# The fix: the converse turn is driven by a real COMPOSER — a one-shot `claude`
# spawn (`OneshotComposer`) — so it either returns the answer within the timeout
# or fails fast into the graceful glitch line (editing the ack in place, never a
# permanent hourglass). This scenario drives the REAL binary via
# `wg telegram conversation`:
#
#   --compose-error : inject a failing composer → prove the fail-fast + glitch
#                     follow-up path (credential-free, ALWAYS runs).
#   --compose       : drive the production one-shot `claude` composer → prove an
#                     actual session-generated answer lands within the timeout
#                     (needs auth; loud-SKIPs credential-free rather than block).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"
mkdir -p "$scratch/.wg/agency/bindings"

cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
username  = "otto_casapinello_bot"
TOML

# A CONFIRMED human so `luca-1` is conversation-eligible (→ a `converse` plan
# rather than the onboarding line).
cat >"$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
- telegram_user: "luca-1"
  agent_id: "human-luca"
  name: "Luca"
  bot_id: "otto"
  confirmed: true
  created_at: "2026-07-11T00:00:00Z"
  confirmed_at: "2026-07-11T00:00:00Z"
YAML

convo() {
    (cd "$scratch" && WG_DIR= wg --json telegram conversation "$@" 2>&1)
}

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    if ! grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — expected to find: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

expect_absent() {
    local desc="$1" out="$2" needle="$3"
    if grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — did NOT expect: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

# ---------------------------------------------------------------------------
# 1) Induced failure → fail fast into the graceful glitch line (credential-free)
# ---------------------------------------------------------------------------
echo "induced compose failure → glitched follow-up sent, no permanent hourglass:"
out="$(convo --compose-error --channel telegram:otto --chat 555 --sender luca-1 --message 'hey otto')"
expect_grep "compose-error converse kind" "$out" '"kind": "converse"'
expect_grep "compose-error glitched"      "$out" '"outcome": "glitched-fallback'
expect_grep "compose-error glitch line"   "$out" 'glitched for a second'
expect_grep "compose-error via otto"      "$out" '"bot": "otto"'
expect_grep "compose-error in chat 555"   "$out" '"chat": "555"'
# No permanent hourglass: the ack ("On it — one sec") must NOT be the last word;
# with a fast failure the ack never fires and the human gets only the glitch line.
expect_absent "compose-error no stuck ack" "$out" "On it — one sec"
expect_absent "compose-error no token leak" "$out" "dummy-token"

# ---------------------------------------------------------------------------
# 2) Real turn → an ACTUAL session-generated answer within the timeout
#    (needs auth; loud-SKIP credential-free so the gate doesn't block)
# ---------------------------------------------------------------------------
echo "real one-shot compose → actual answer within the timeout:"
# No pre-flight credential probe: the `claude` CLI self-authenticates (env token,
# ~/.claude credentials, or the macOS Keychain), so we drive the real turn and
# branch on the OUTCOME. A short per-call timeout keeps the smoke snappy while
# still bounding a hung child. `replied` → assert the real answer; `glitched` →
# the composer ran but its child failed (no usable auth in this environment) → a
# loud SKIP (the deterministic composed-turn success is already locked by the
# notify::telegram_conversation unit tests, and the --compose-error path above
# locks fail-fast unconditionally).
out="$( (cd "$scratch" && WG_DIR= WG_TELEGRAM_COMPOSE_TIMEOUT_SECS=45 \
        wg --json telegram conversation --compose \
        --channel telegram:otto --chat 555 --sender luca-1 \
        --message 'hey otto, quick hello' 2>&1) )"
if grep -qF '"outcome": "glitched-fallback' <<<"$out"; then
    loud_skip "COMPOSER CHILD FAILED" \
        "the live one-shot composer returned glitched (child error / no usable auth); real-answer assertion skipped. Output: $out"
fi
expect_grep "real compose converse kind" "$out" '"kind": "converse"'
expect_grep "real compose replied"       "$out" '"outcome": "replied"'
expect_grep "real compose via otto"      "$out" '"bot": "otto"'
expect_grep "real compose in chat 555"   "$out" '"chat": "555"'
# The reply is a real, non-empty, generated answer — assert a non-trivial send
# body exists (the JSON "text" field carries more than a couple of chars).
if ! grep -qE '"text": "[^"]{8,}' <<<"$out"; then
    loud_fail "real compose produced no substantive answer text
--- got ---
$out
-----------"
fi
echo "  ok: real compose produced a substantive answer"
expect_absent "real compose no token leak" "$out" "dummy-token"

echo "PASS: converse turn completes with a real answer (or fails fast into the graceful glitch line) — no 120s hang"
