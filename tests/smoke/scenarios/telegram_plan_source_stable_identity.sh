#!/usr/bin/env bash
# Real-binary regression: authored reminder/errand Source labels resolve to one
# stable household id and never fall through to roster order or a recipient bot.
#
# Everything is scratch-only. `--dry-run` prevents Telegram delivery, while a
# loopback shopping stub lets the real errand path render its live-shaped nudge.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/_helpers.sh"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

wg_bin="${WG_BIN:-}"
if [[ -z "$wg_bin" ]]; then
    wg_bin="$(command -v wg || true)"
elif [[ "$wg_bin" != /* ]]; then
    wg_bin="$REPO_ROOT/$wg_bin"
fi
[[ -n "$wg_bin" && -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable locally built wg binary"

command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING PYTHON3" "python3 is needed for the loopback fixture and JSON assertions"

scratch="$(make_scratch)"
fixture_home="$scratch/home"
mkdir -p "$fixture_home" "$scratch/.wg/agency/bindings" "$scratch/plans"

run_wg() {
    (
        cd "$scratch"
        HOME="$fixture_home" \
        XDG_CONFIG_HOME="$fixture_home/.config" \
        "$wg_bin" --dir "$scratch/.wg" "$@"
    )
}

write_roster() {
    local reverse="$1"
    if [[ "$reverse" == "yes" ]]; then
        cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "meal-orbit"
name = "Pantry Lantern"
domains = ["meals"]

[[agent]]
id = "calendar-orbit"
name = "Harbor Keeper"
domains = ["calendar", "coordination", "shopping"]
TOML
    else
        cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "calendar-orbit"
name = "Harbor Keeper"
domains = ["calendar", "coordination", "shopping"]

[[agent]]
id = "meal-orbit"
name = "Pantry Lantern"
domains = ["meals"]
TOML
    fi
}

write_roster "no"
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram.bots.meal-wire]
bot_token = "0000000000:meal-fixture-token"
chat_id = "-100731"
agent_id = "meal-orbit"

[telegram.bots.calendar-wire]
bot_token = "0000000000:calendar-fixture-token"
chat_id = "-100731"
agent_id = "calendar-orbit"
TOML
cat >"$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
- telegram_user: "member-731"
  agent_id: "human-731"
  name: "River Guest"
  bot_id: "meal-wire"
  confirmed: true
  created_at: "2026-07-27T00:00:00Z"
  confirmed_at: "2026-07-27T00:00:00Z"
YAML
cat >"$scratch/plans/2026-W31-family-plan.md" <<'MARKDOWN'
# Household weekly plan · 2026-W31

**Week of Monday 2026-07-27 → Sunday 2026-08-02**

## 1. Dinners (Pantry Lantern)

| Day | Slot type | Dinner | Prep |
|-----|-----------|--------|------|
| Tue 07-28 | Vegetarian | Summer pasta | ~20 min |

## 3. Calendar (Harbor Keeper)

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Tue 07-28 | 08:00 | ⏰ Reminder: River Guest set out the bins | Harbor Keeper |
| Tue 07-28 | 08:15 | 🛒 Market run (River Guest) — produce | Harbor Keeper (§4) |

## 4. Shopping list (Harbor Keeper)

### Market
- Oats
MARKDOWN
cp "$scratch/plans/2026-W31-family-plan.md" "$scratch/plan.before"

port_file="$scratch/stub.port"
python3 - "$port_file" <<'PY' &
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import sys

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({
            "ok": True,
            "groups": [{
                "store": "Market",
                "items": [{"text": "Oats", "key": "fixture:oats", "checked": False}],
            }],
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        pass

server = HTTPServer(("127.0.0.1", 0), Handler)
Path(sys.argv[1]).write_text(str(server.server_port))
server.serve_forever()
PY
stub_pid=$!
cleanup_stub() {
    kill "$stub_pid" 2>/dev/null || true
    wait "$stub_pid" 2>/dev/null || true
}
add_cleanup_hook cleanup_stub

for _ in $(seq 1 100); do
    [[ -s "$port_file" ]] && break
    sleep 0.05
done
[[ -s "$port_file" ]] || loud_fail "loopback shopping stub did not publish a port"
gateway_url="http://127.0.0.1:$(<"$port_file")"

assert_routes() {
    local label="$1"
    local list_json dry_output
    list_json="$(run_wg telegram remind --list --json --now 2026-07-28T08:00)"
    LIST_JSON="$list_json" python3 - <<'PY'
import json
import os

rows = json.loads(os.environ["LIST_JSON"])
assert len(rows) == 1, rows
assert rows[0]["bot"] == "calendar-orbit", rows[0]
assert rows[0]["recipient"] == "River Guest", rows[0]
PY
    dry_output="$(
        CASA_GATEWAY_URL="$gateway_url" \
            run_wg telegram remind --dry-run --now 2026-07-28T08:00
    )"
    grep -qF "WOULD SEND to River Guest via calendar-orbit" <<<"$dry_output" \
        || loud_fail "$label: reminder did not retain the stable Source route:
$dry_output"
    grep -qF "WOULD ERRAND-NUDGE River Guest via calendar-orbit" <<<"$dry_output" \
        || loud_fail "$label: errand did not retain the stable Source route:
$dry_output"
    if grep -qF "via meal-orbit" <<<"$dry_output"; then
        loud_fail "$label: Source routing fell through to the recipient's unrelated bound bot:
$dry_output"
    fi
}

assert_routes "initial roster"
write_roster "yes"
assert_routes "reordered roster"

cmp "$scratch/plan.before" "$scratch/plans/2026-W31-family-plan.md" \
    || loud_fail "the engine rewrote family-visible plan Source text"
grep -qF "| Harbor Keeper (§4) |" "$scratch/plans/2026-W31-family-plan.md" \
    || loud_fail "the authored multiword Source label did not remain in the plan"

echo "PASS: live-shaped reminder and errand Sources route by stable identity across roster reorder"
