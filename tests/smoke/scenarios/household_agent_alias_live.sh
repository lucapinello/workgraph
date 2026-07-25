#!/usr/bin/env bash
# Real-binary regression for stable household persona routing.
#
# The configured persona reference is an opaque, case-sensitive session alias,
# not an Agent.name. The alias is bound to a full agent id once, then the
# display metadata is renamed. Both ordinary conversation turns must still
# reach the same persistent session. Everything lives in a scratch project;
# the recording conversation command never contacts Telegram or an LLM.

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
    || loud_skip "MISSING PYTHON3" "python3 is needed for exact JSON and YAML fixture assertions"

scratch="$(make_scratch)"
project="$scratch/project"
fixture_home="$scratch/home"
wg_dir="$project/.wg"
mkdir -p "$project" "$fixture_home"

run_wg() {
    (
        cd "$project"
        HOME="$fixture_home" \
        XDG_CONFIG_HOME="$fixture_home/.config" \
        "$wg_bin" --dir "$wg_dir" "$@"
    )
}

run_wg init --no-agency >/dev/null
run_wg role add "Fixture Household Role" \
    --outcome "Keep scratch-only household routing deterministic" >/dev/null
role_id="$(
    run_wg --json role list |
        python3 -c 'import json, sys; rows=json.load(sys.stdin); assert len(rows)==1; print(rows[0]["id"])'
)"
run_wg tradeoff add "Fixture Household Tradeoff" \
    --accept "Stable local routing" \
    --reject "External side effects" >/dev/null
tradeoff_id="$(
    run_wg --json tradeoff list |
        python3 -c 'import json, sys; rows=json.load(sys.stdin); assert len(rows)==1; print(rows[0]["id"])'
)"

initial_display="Unrelated Display Metadata"
renamed_display="Renamed Display Metadata"
alias="household-slot-a"
bot_id="household-entry"

run_wg agent create "$initial_display" \
    --role "$role_id" \
    --tradeoff "$tradeoff_id" >/dev/null
agent_id="$(
    run_wg --json agent list |
        python3 -c 'import json, sys; rows=json.load(sys.stdin); assert len(rows)==1; print(rows[0]["id"])'
)"
[[ "$agent_id" =~ ^[0-9a-f]{64}$ ]] \
    || loud_fail "agent create did not produce a full agent id: $agent_id"

session_uuid="$(run_wg session new "$alias" 2>/dev/null)"
[[ "$session_uuid" =~ ^[0-9a-f-]{36}$ ]] \
    || loud_fail "session new did not return a full UUID: $session_uuid"
run_wg agent session "$agent_id" --session "$alias" >/dev/null

mkdir -p "$wg_dir/agency/bindings"
cat >"$wg_dir/notify.toml" <<TOML
[telegram.bots.$bot_id]
bot_token = "0000000000:fixture-only-token"
chat_id = "fixture-chat"
username = "fixture_household_entry_bot"
agent_id = "$alias"
TOML
cat >"$wg_dir/agency/bindings/telegram.yaml" <<YAML
bindings:
- telegram_user: "fixture-member-before"
  agent_id: "fixture-human-before"
  name: "Fixture Member Before"
  bot_id: "$bot_id"
  confirmed: true
  created_at: "2026-07-25T00:00:00Z"
  confirmed_at: "2026-07-25T00:00:00Z"
- telegram_user: "fixture-member-after"
  agent_id: "fixture-human-after"
  name: "Fixture Member After"
  bot_id: "$bot_id"
  confirmed: true
  created_at: "2026-07-25T00:00:00Z"
  confirmed_at: "2026-07-25T00:00:00Z"
YAML

assert_alias_binding() {
    local session_json
    session_json="$(run_wg session list --json)"
    SESSION_JSON="$session_json" python3 - "$alias" "$agent_id" "$session_uuid" <<'PY'
import json
import os
import sys

alias, agent_id, session_uuid = sys.argv[1:]
rows = json.loads(os.environ["SESSION_JSON"])
matches = [row for row in rows if alias in row.get("aliases", [])]
assert len(matches) == 1, matches
row = matches[0]
assert row["uuid"] == session_uuid, row
assert row["agent_id"] == agent_id, row
PY
}

run_turn() {
    local sender="$1"
    local marker="$2"
    local out
    out="$(
        run_wg --json telegram conversation \
            --channel "telegram:$bot_id" \
            --chat "fixture-chat" \
            --sender "$sender" \
            --message "$marker"
    )"
    grep -qF '"kind": "converse"' <<<"$out" \
        || loud_fail "alias did not plan a session-backed conversation for $sender:
$out"
}

assert_alias_binding
before_marker="before metadata rename marker"
run_turn "fixture-member-before" "$before_marker"
inbox="$wg_dir/chat/$session_uuid/inbox.jsonl"
[[ -f "$inbox" ]] || loud_fail "conversation did not create the bound session inbox"
grep -qF "$before_marker" "$inbox" \
    || loud_fail "first conversation did not reach the alias-bound session"
grep -qF "dryrun-fixture-member-before" "$inbox" \
    || loud_fail "first request id is absent from the alias-bound session"

agent_file="$wg_dir/agency/cache/agents/$agent_id.yaml"
python3 - "$agent_file" "$renamed_display" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
replacement = sys.argv[2]
lines = path.read_text().splitlines()
matches = [i for i, line in enumerate(lines) if line.startswith("name: ")]
assert len(matches) == 1, matches
lines[matches[0]] = f"name: {replacement}"
path.write_text("\n".join(lines) + "\n")
PY

agent_json="$(run_wg --json agent list)"
AGENT_JSON="$agent_json" python3 - "$agent_id" "$renamed_display" <<'PY'
import json
import os
import sys

agent_id, expected_name = sys.argv[1:]
rows = json.loads(os.environ["AGENT_JSON"])
assert len(rows) == 1, rows
assert rows[0]["id"] == agent_id, rows[0]
assert rows[0]["name"] == expected_name, rows[0]
PY

binding_out="$(run_wg agent session "$agent_id")"
grep -qF "$session_uuid" <<<"$binding_out" \
    || loud_fail "metadata rename changed the full-id session binding:
$binding_out"

after_marker="after metadata rename marker"
run_turn "fixture-member-after" "$after_marker"
grep -qF "$after_marker" "$inbox" \
    || loud_fail "renamed metadata diverted the second conversation"
grep -qF "dryrun-fixture-member-after" "$inbox" \
    || loud_fail "second request id is absent from the original bound session"
assert_alias_binding

echo "PASS: opaque household alias kept one full-id session binding across an unrelated Agent.name rename"
