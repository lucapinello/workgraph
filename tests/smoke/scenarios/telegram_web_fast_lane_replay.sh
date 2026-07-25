#!/usr/bin/env bash
# Real-binary replay gate for a kiosk fast-lane mutation.
#
# The fixture is wholly scratch-local: an opaque authored roster, a confirmed
# fixture human, the live numbered Dinners plan shape, and a loopback Telegram
# API stub. The first send fails after the plan edit, then fresh wg processes
# replay the same accepted occurrence. Only delivery may retry; the plan and
# graph must not mutate again. A distinct occurrence id with identical words is
# admitted normally.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"
repo_root="$(cd "$scenario_dir/../../.." && pwd)"

wg_bin="${WG_BIN:-}"
if [[ -z "$wg_bin" ]]; then
    wg_bin="$(command -v wg 2>/dev/null || true)"
elif [[ "$wg_bin" != /* ]]; then
    wg_bin="$repo_root/$wg_bin"
fi
[[ -n "$wg_bin" && -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable freshly built wg binary"
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING PYTHON3" "python3 is needed for the loopback Telegram stub and JSON assertions"

scratch="$(make_scratch)"
mkdir -p "$scratch/plans"

human_name="River Guest"
human_handle="river-guest"
human_telegram="7700415529"
helper_id="relay-4"
group_chat="-1000000000001"
message="add oat milk to the shopping list"

read -r week_id week_start week_end mon_slot tue_slot wed_slot thu_slot fri_slot sat_slot sun_slot < <(
    python3 - <<'PY'
from datetime import date, timedelta

today = date.today()
monday = today - timedelta(days=today.weekday())
days = [monday + timedelta(days=offset) for offset in range(7)]
iso_year, iso_week, _ = today.isocalendar()
print(
    f"{iso_year}-W{iso_week:02d}",
    monday.isoformat(),
    days[-1].isoformat(),
    *(day.strftime("%a_%m-%d") for day in days),
)
PY
)

(
    cd "$scratch"
    WG_DIR= "$wg_bin" init >/dev/null 2>&1
    WG_DIR= "$wg_bin" agency human add "$human_name" --telegram "$human_telegram" >/dev/null 2>&1
    WG_DIR= "$wg_bin" agency human confirm "$human_telegram" >/dev/null 2>&1
)

cat >"$scratch/household.toml" <<TOML
[[agent]]
id = "$helper_id"
name = "Open Door"
emoji = "🏡"
domains = ["calendar", "coordination", "shopping"]
TOML

cat >"$scratch/.wg/notify.toml" <<TOML
[telegram]
chat_id = "$group_chat"

[telegram.bots.$helper_id]
bot_token = "0000000000:fixture-token"
chat_id = "$group_chat"
agent_id = "$helper_id"
username = "open_door_fixture_bot"
TOML

# Match the live parser shape: the first section is numbered Dinners with an
# authored parenthetical. The mutation itself targets the shopping section.
cat >"$scratch/plans/${week_id}-family-plan.md" <<MARKDOWN
# Household weekly plan · ${week_id}

**Week of Monday ${week_start} → Sunday ${week_end}**

## 1. Dinners (Cedar Signal → Copper Ladle)

| Day | Slot type | Dinner | Prep | Note |
|-----|-----------|--------|------|------|
| ${mon_slot/_/ } | Vegetarian | Chickpea lemon bowls | ~25 min | quick |
| ${tue_slot/_/ } | Fish | Baked trout and potatoes | ~35 min | tray bake |
| ${wed_slot/_/ } | Vegetarian | Miso aubergine noodles | ~30 min | pantry |
| ${thu_slot/_/ } | Fish | Salmon rice bowls | ~30 min | leftovers |
| ${fri_slot/_/ } | Flex | Tomato bean soup | ~25 min | freezer |
| ${sat_slot/_/ } | Vegetarian | Mushroom tacos | ~30 min | family |
| ${sun_slot/_/ } | Leftovers | Clear-the-fridge plates | ~15 min | flex |

## 2. Calendar (Open Door)

| Day | Time | Event | Source |
|-----|------|-------|--------|
| ${wed_slot/_/ } | 18:30 | Cook: miso aubergine noodles | Copper Ladle |

## 3. Shopping list (Open Door)

### Produce
- Aubergines ×2

### Pantry
- White miso
MARKDOWN

port_file="$scratch/stub.port"
request_log="$scratch/stub-requests.jsonl"
server_py="$scratch/telegram_stub.py"
cat >"$server_py" <<'PY'
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port_file, request_log = sys.argv[1:3]

class Handler(BaseHTTPRequestHandler):
    calls = 0

    def do_POST(self):
        size = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(size)
        Handler.calls += 1
        with open(request_log, "ab") as log:
            log.write(body + b"\n")
            log.flush()

        if Handler.calls == 1:
            status = 500
            reply = {"ok": False, "description": "fixture transport failure"}
        else:
            status = 200
            reply = {"ok": True, "result": {"message_id": Handler.calls}}
        payload = json.dumps(reply).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format, *_args):
        pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w", encoding="utf-8") as out:
    out.write(str(server.server_address[1]))
    out.flush()
server.serve_forever()
PY

python3 "$server_py" "$port_file" "$request_log" >"$scratch/stub.out" 2>"$scratch/stub.err" &
stub_pid=$!
cleanup_telegram_stub() {
    if kill -0 "$stub_pid" 2>/dev/null; then
        kill "$stub_pid" 2>/dev/null || true
        wait "$stub_pid" 2>/dev/null || true
    fi
}
add_cleanup_hook cleanup_telegram_stub

for _ in $(seq 1 100); do
    [[ -s "$port_file" ]] && break
    sleep 0.05
done
[[ -s "$port_file" ]] || loud_fail "loopback Telegram stub did not publish a port"
api_base="http://127.0.0.1:$(cat "$port_file")"

run_turn() {
    local occurrence="$1"
    (
        cd "$scratch"
        WG_DIR= \
        WG_TURN_ID="$occurrence" \
        WG_TELEGRAM_API_BASE="$api_base" \
            "$wg_bin" --json telegram web-inbound \
                --sender "$human_handle" \
                --message "$message"
    )
}

item_count() {
    grep -cF -- "- oat milk" "$scratch/plans/2026-W30-family-plan.md" || true
}

request_count() {
    if [[ -f "$request_log" ]]; then
        wc -l <"$request_log" | tr -d ' '
    else
        echo 0
    fi
}

state_count() {
    local state="$1"
    python3 - "$scratch/.wg/telegram-occurrences" "$state" <<'PY'
import json
import pathlib
import sys
root, expected = pathlib.Path(sys.argv[1]), sys.argv[2]
print(sum(json.loads(path.read_text())["state"] == expected for path in root.glob("*.json")))
PY
}

echo "Round 1 — mutation succeeds and the stubbed transport fails:"
if first_out="$(run_turn "opaque-occurrence-a7" 2>&1)"; then
    loud_fail "first transport call unexpectedly succeeded:
$first_out"
fi
[[ "$(item_count)" == "1" ]] || loud_fail "first occurrence did not apply exactly one shopping mutation"
[[ "$(request_count)" == "1" ]] || loud_fail "first occurrence did not make exactly one stubbed send attempt"
[[ "$(state_count applied)" == "1" ]] || loud_fail "failed delivery was not retained as one applied occurrence"
echo "  ok: applied once and retained the canonical outcome after send failure"

echo "Round 2 — a fresh process retries only delivery:"
retry_out="$(run_turn "opaque-occurrence-a7" 2>&1)" \
    || loud_fail "stored delivery retry failed:
$retry_out"
[[ "$(item_count)" == "1" ]] || loud_fail "same occurrence mutated the shopping list twice"
[[ "$(request_count)" == "2" ]] || loud_fail "same occurrence did not retry exactly one failed delivery"
[[ "$(state_count delivered)" == "1" ]] || loud_fail "successful retry did not mark the occurrence delivered"
grep -qF '"replayed": true' <<<"$retry_out" \
    || loud_fail "retry output did not identify the stored replay:
$retry_out"

python3 - "$request_log" "$group_chat" <<'PY' \
    || loud_fail "failed delivery did not replay the exact canonical route and reply"
import json
import sys
rows = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
assert len(rows) == 2, rows
assert rows[0] == rows[1], rows
assert str(rows[0]["chat_id"]) == sys.argv[2], rows
assert "oat milk" in rows[0]["text"].lower(), rows
PY
echo "  ok: retry reused the stored bot route, chat, and family-visible bytes"

echo "Round 3 — a completed refire is silent:"
done_out="$(run_turn "opaque-occurrence-a7" 2>&1)" \
    || loud_fail "completed same-turn refire failed:
$done_out"
[[ "$(item_count)" == "1" ]] || loud_fail "completed same-turn refire mutated the plan"
[[ "$(request_count)" == "2" ]] || loud_fail "completed same-turn refire sent again"
grep -qF '"already_delivered": true' <<<"$done_out" \
    || loud_fail "completed refire did not report its durable no-op:
$done_out"
echo "  ok: no second mutation and no second successful send"

echo "Round 4 — identical words in a later occurrence are admitted:"
later_out="$(run_turn "opaque-occurrence-b9" 2>&1)" \
    || loud_fail "later distinct occurrence failed:
$later_out"
[[ "$(item_count)" == "2" ]] || loud_fail "later occurrence did not apply its own mutation"
[[ "$(request_count)" == "3" ]] || loud_fail "later occurrence did not make its own send"
[[ "$(state_count delivered)" == "2" ]] || loud_fail "two accepted occurrences did not produce two delivered journals"

python3 - "$scratch/.wg/telegram-occurrences" "$request_log" <<'PY' \
    || loud_fail "opaque journal or final send assertions failed"
import json
import pathlib
import sys
journal_dir, request_log = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
records = list(journal_dir.glob("*.json"))
assert len(records) == 2, records
assert all(path.name.startswith("b3-v1-") for path in records), records
assert all("opaque-occurrence" not in path.name for path in records), records
rows = [json.loads(line) for line in request_log.read_text().splitlines()]
assert len(rows) == 3, rows
assert rows[2]["text"] == rows[1]["text"], rows
PY

python3 - "$scratch/.wg/graph.jsonl" <<'PY' \
    || loud_fail "fast-lane graph visibility was stamped more than once per occurrence"
import json
import sys
rows = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
fast = [row for row in rows if "fast-lane" in row.get("tags", [])]
assert len(fast) == 2, fast
PY

echo "PASS: web fast-lane replay mutates and delivers at most once per accepted occurrence"
