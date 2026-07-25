#!/usr/bin/env bash
# Real-binary regression for `wg session list --json`: stdout is JSON and
# stderr must not claim that the flag is unsupported or ignored.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HERE/_helpers.sh"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"

wg_bin="${WG_BIN:-$REPO_ROOT/target/debug/wg}"
if [[ "$wg_bin" != /* ]]; then
    wg_bin="$REPO_ROOT/$wg_bin"
fi
[[ -x "$wg_bin" ]] \
    || loud_skip "MISSING REVIEW BINARY" "set WG_BIN to an executable locally built wg binary"
command -v python3 >/dev/null 2>&1 \
    || loud_skip "MISSING PYTHON3" "python3 is needed for the JSON assertion"

scratch="$(make_scratch)"
wg_dir="$scratch/.wg"
"$wg_bin" --dir "$wg_dir" init --no-agency >/dev/null
"$wg_bin" --dir "$wg_dir" session new opaque-fixture --label "Opaque fixture" \
    >/dev/null 2>/dev/null

stderr_path="$scratch/session-list.stderr"
if ! stdout="$("$wg_bin" --dir "$wg_dir" session list --json 2>"$stderr_path")"; then
    loud_fail "wg session list --json exited non-zero:
$(cat "$stderr_path")"
fi

SESSION_LIST_JSON="$stdout" python3 - <<'PY'
import json
import os

rows = json.loads(os.environ["SESSION_LIST_JSON"])
assert isinstance(rows, list), rows
assert len(rows) == 1, rows
assert rows[0]["aliases"] == ["opaque-fixture"], rows[0]
assert rows[0]["label"] == "Opaque fixture", rows[0]
PY

if grep -Eiq -- "--json flag is not supported|will be ignored" "$stderr_path"; then
    loud_fail "valid session JSON was accompanied by a contradictory warning:
$(cat "$stderr_path")"
fi

echo "PASS: session list emitted valid JSON without an unsupported/ignored warning"
