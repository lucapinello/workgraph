#!/usr/bin/env bash
# Live verification for fix-poll-all-bots.
#
# Proves the `wg telegram listen` loop long-polls EVERY configured bot
# concurrently (not just one). Method:
#
#   1. Start the freshly-built listener against the real casa config
#      (./.wg/notify.toml in the casa project root).
#   2. Assert one "polling <bot>" startup line per configured bot.
#   3. For up to 30s, probe each bot's getUpdates from a SECOND process and
#      assert each bot reports either:
#        - 0 pending updates (result == []), OR
#        - HTTP 409 Conflict ("terminated by other getUpdates request").
#      A 409 is positive proof that OUR listener currently owns that bot's
#      long-poll; an empty result proves the queue is drained. Either is an
#      acceptable pass per the task's validation block.
#
# Exit 0 iff ALL configured bots pass. Prints the startup log lines.
# Written for bash 3.2 (macOS default): no mapfile / associative arrays.
#
# Usage: tests/live/poll_all_bots_live.sh [path-to-wg-binary]
set -uo pipefail

WG_BIN="${1:-$HOME/Projects/weekly_planner/.wg-worktrees/agent-288/target/debug/wg}"
PROJECT_ROOT="${CASA_PROJECT_ROOT:-$HOME/Projects/weekly_planner}"
NOTIFY_TOML="$PROJECT_ROOT/.wg/notify.toml"
DEADLINE_SECS=30

log()  { printf '[live] %s\n' "$*"; }
fail() { printf '[live] FAIL: %s\n' "$*" >&2; exit 1; }

[ -x "$WG_BIN" ] || fail "wg binary not found/executable: $WG_BIN"
[ -f "$NOTIFY_TOML" ] || fail "notify.toml not found: $NOTIFY_TOML"

# --- parse bot ids + tokens from the TOML into parallel indexed arrays --------
# python emits "<bot_id> <token>" (space-separated) per configured bot.
BOT_IDS=()
BOT_TOKENS=()
while read -r bid tok; do
  [ -n "$bid" ] || continue
  BOT_IDS+=("$bid")
  BOT_TOKENS+=("$tok")
done < <(python3 - "$NOTIFY_TOML" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    cfg = tomllib.load(f)
tg = cfg.get("telegram", {})
# legacy single-bot form contributes a "default" entry
if tg.get("bot_token") and tg.get("chat_id"):
    print(f"default {tg['bot_token']}")
for bid, b in tg.get("bots", {}).items():
    print(f"{bid} {b['bot_token']}")
PY
)

N=${#BOT_IDS[@]}
[ "$N" -gt 0 ] || fail "no bots parsed from $NOTIFY_TOML"
log "configured bots ($N): ${BOT_IDS[*]}"

# --- start the listener -------------------------------------------------------
LISTEN_LOG="$(mktemp -t poll_all_bots_live.XXXXXX)"
log "starting listener: $WG_BIN telegram listen  (cwd=$PROJECT_ROOT)"
( cd "$PROJECT_ROOT" && exec "$WG_BIN" telegram listen ) >"$LISTEN_LOG" 2>&1 &
LISTEN_PID=$!
cleanup() { kill "$LISTEN_PID" 2>/dev/null; wait "$LISTEN_PID" 2>/dev/null; }
trap cleanup EXIT

# --- 1) assert one "polling <bot>" line per bot -------------------------------
log "waiting for 'polling <bot>' startup lines..."
polling_ok=0
i=0
while [ "$i" -lt 100 ]; do   # up to ~10s
  ok=1
  for bid in "${BOT_IDS[@]}"; do
    grep -qE "^polling ${bid}\$" "$LISTEN_LOG" || ok=0
  done
  if [ "$ok" = 1 ]; then polling_ok=1; break; fi
  kill -0 "$LISTEN_PID" 2>/dev/null || fail "listener exited early; log: $(cat "$LISTEN_LOG")"
  sleep 0.1
  i=$((i + 1))
done
echo "----- listener startup log -----"
sed -n '1,20p' "$LISTEN_LOG"
echo "--------------------------------"
[ "$polling_ok" = 1 ] || fail "did not see a 'polling <bot>' line for every bot"
log "OK: startup shows a polling line for all $N bots"

# --- 2) probe each bot's getUpdates; expect 0-pending or 409 ------------------
# Prints "PASS <reason>" or "PENDING ..." for a single token.
probe_bot() {
  tok="$1"
  body="$(curl -s -w $'\n%{http_code}' -m 8 \
    "https://api.telegram.org/bot${tok}/getUpdates?timeout=0" 2>/dev/null)"
  http="${body##*$'\n'}"
  json="${body%$'\n'*}"
  if [ "$http" = "409" ]; then
    echo "PASS 409-conflict(listener owns long-poll)"; return 0
  fi
  n="$(printf '%s' "$json" | python3 -c 'import json,sys
try:
    d=json.load(sys.stdin); print(len(d.get("result",[])) if d.get("ok") else -1)
except Exception:
    print(-2)' 2>/dev/null)"
  if [ "$n" = "0" ]; then echo "PASS 0-pending"; return 0; fi
  echo "PENDING http=$http n=$n"; return 1
}

log "probing all bots (deadline ${DEADLINE_SECS}s)..."
RESULTS=()
for _ in "${BOT_IDS[@]}"; do RESULTS+=("PENDING"); done

START=$(date +%s)
all_pass=0
while :; do
  all_pass=1
  idx=0
  while [ "$idx" -lt "$N" ]; do
    case "${RESULTS[$idx]}" in
      PASS*) ;;
      *) RESULTS[$idx]="$(probe_bot "${BOT_TOKENS[$idx]}")" ;;
    esac
    case "${RESULTS[$idx]}" in PASS*) ;; *) all_pass=0 ;; esac
    idx=$((idx + 1))
  done
  [ "$all_pass" = 1 ] && break
  now=$(date +%s)
  [ $((now - START)) -ge "$DEADLINE_SECS" ] && break
  sleep 1
done
ELAPSED=$(( $(date +%s) - START ))

echo "----- per-bot probe results (after ${ELAPSED}s) -----"
idx=0
while [ "$idx" -lt "$N" ]; do
  printf '  %-10s %s\n' "${BOT_IDS[$idx]}" "${RESULTS[$idx]}"
  idx=$((idx + 1))
done
echo "-----------------------------------------------------"

if [ "$all_pass" = 1 ]; then
  log "PASS: all $N bots at 0-pending-or-409 within ${ELAPSED}s of listener start"
  exit 0
else
  fail "not all bots reached 0-pending-or-409 within ${DEADLINE_SECS}s"
fi
