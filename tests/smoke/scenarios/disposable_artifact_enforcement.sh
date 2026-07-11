#!/usr/bin/env bash
# Scenario: disposable_artifact_enforcement
#
# Regression for the disposable contract (docs/14-disposable-lifecycle.md
# §enforcement): a task tagged `disposable` is an ephemeral, spawn-and-discard
# unit of work whose only durable value is what it hands back to its spawner.
# Before such a task may complete, `wg done` MUST refuse it until BOTH halves
# of the contract hold:
#   1. it has recorded ≥1 artifact  (`wg artifact <id> <path>`), and
#   2. it has left ≥1 `wg log` breadcrumb (an agent-authored log entry).
#
# A no-artifact / no-breadcrumb disposable promoted to Done launders a silent
# no-op into a "success" the downstream ingest step then has nothing to consume.
#
# Asserts:
#   (a) A disposable with no artifact and no breadcrumb is REFUSED at wg done
#       (non-zero exit), the message names the disposable + artifact contract,
#       failure_class is `disposable-contract-unmet`, and the task stays
#       in-progress (not promoted to Done).
#   (b) After an artifact is recorded but still no breadcrumb, wg done is STILL
#       refused, now naming the missing `wg log` half.
#   (c) After a `wg log` breadcrumb is added, wg done SUCCEEDS (exit 0), the
#       task reaches `done`, and the failure_class marker is cleared.
#   (d) A NON-disposable task with neither artifact nor breadcrumb is UNAFFECTED
#       — the contract only binds tasks that opt in via the `disposable` tag.
#
# The pure gate + Task helpers are additionally pinned by the
# `test_disposable_done_refused_without_artifact` (and siblings) unit tests.
# This scenario drives the real `wg add` / `wg artifact` / `wg log` / `wg done`
# CLI paths end-to-end.
#
# Requires: python3 (to read/rewrite graph rows) and wg.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v python3 >/dev/null 2>&1; then
    loud_skip "MISSING PYTHON3" "python3 needed to read/set task fields on the graph row"
fi

scratch=$(make_scratch)
export HOME="$scratch/home"
mkdir -p "$HOME"
project_root="$scratch/project"
mkdir -p "$project_root"
wg_dir="$project_root/.wg"

# ── Init a git repo with `main` (wg done touches git state) ──────────
git init -b main "$project_root" >/dev/null 2>&1 \
    || loud_fail "git init failed in $project_root"
(
    cd "$project_root"
    git config user.email "smoke@test" >/dev/null
    git config user.name "Smoke" >/dev/null
    echo "initial" > README.md
    git add README.md >/dev/null
    git commit -m "initial" >/dev/null
) || loud_fail "git initial commit setup failed"

cd "$project_root"
wg --dir "$wg_dir" init >/dev/null 2>&1 \
    || loud_fail "wg init failed in scratch dir"

# A disposable task (tag `disposable`) and an ordinary control task.
wg --dir "$wg_dir" add "probe the endpoint" \
    --id disp1 -t disposable >/dev/null 2>&1 \
    || loud_fail "wg add of disp1 failed"
wg --dir "$wg_dir" add "ordinary work" \
    --id ord1 >/dev/null 2>&1 \
    || loud_fail "wg add of ord1 failed"

# Helper: force a field on a task row (used only to flip status → in-progress).
set_status_in_progress() {
    python3 - "$wg_dir/graph.jsonl" "$1" <<'PY'
import json, sys
path, tid = sys.argv[1], sys.argv[2]
out = []
for line in open(path):
    if not line.strip():
        continue
    obj = json.loads(line)
    if obj.get("kind") == "task" and obj.get("id") == tid:
        obj["status"] = "in-progress"
    out.append(json.dumps(obj))
open(path, "w").write("\n".join(out) + "\n")
PY
}

# Helper: read a single field from a task row.
get_task_field() {
    python3 - "$wg_dir/graph.jsonl" "$1" "$2" <<'PY'
import json, sys
path, tid, key = sys.argv[1], sys.argv[2], sys.argv[3]
for line in open(path):
    if not line.strip():
        continue
    obj = json.loads(line)
    if obj.get("kind") == "task" and obj.get("id") == tid:
        v = obj.get(key, "")
        print(v if v is not None else "")
        break
PY
}

# Run `wg done <id>` on the human (non-agent) path, skipping the smoke gate
# so this scenario does not recurse into the smoke manifest.
run_done() {
    local tid="$1"
    unset WG_AGENT_ID
    unset WG_SMOKE_AGENT_OVERRIDE
    WG_WORKTREE_PATH="$project_root" \
    WG_BRANCH="wg/smoke/$tid" \
    WG_PROJECT_ROOT="$project_root" \
        wg --dir "$wg_dir" done "$tid" --skip-smoke
}

# ── (a): disposable with no artifact + no breadcrumb → refused ────────
set_status_in_progress disp1

log_a="$scratch/done-a.log"
set +e
run_done disp1 >"$log_a" 2>&1
exit_a=$?
set -e

if [[ $exit_a -eq 0 ]]; then
    loud_fail "wg done exited 0 for a no-artifact disposable — enforcement regressed.
done.log:
$(cat "$log_a")"
fi
if ! grep -qi "disposable" "$log_a"; then
    loud_fail "refusal did not mention 'disposable'.
done.log:
$(cat "$log_a")"
fi
if ! grep -qi "artifact" "$log_a"; then
    loud_fail "refusal did not name the missing 'artifact' half.
done.log:
$(cat "$log_a")"
fi

fc=$(get_task_field disp1 failure_class)
st=$(get_task_field disp1 status)
if [[ "$fc" != "disposable-contract-unmet" ]]; then
    loud_fail "failure_class expected 'disposable-contract-unmet', got '$fc'"
fi
if [[ "$st" != "in-progress" ]]; then
    loud_fail "status expected 'in-progress' (not promoted), got '$st'"
fi

# ── (b): artifact recorded but still no breadcrumb → still refused ────
wg --dir "$wg_dir" artifact disp1 result.json >/dev/null 2>&1 \
    || loud_fail "wg artifact disp1 failed"

log_b="$scratch/done-b.log"
set +e
run_done disp1 >"$log_b" 2>&1
exit_b=$?
set -e

if [[ $exit_b -eq 0 ]]; then
    loud_fail "wg done exited 0 for a disposable with an artifact but no breadcrumb.
done.log:
$(cat "$log_b")"
fi
if ! grep -qi "log" "$log_b"; then
    loud_fail "refusal did not name the missing 'wg log' breadcrumb half.
done.log:
$(cat "$log_b")"
fi

# ── (c): breadcrumb added → wg done succeeds, marker cleared ──────────
wg --dir "$wg_dir" log disp1 "probed the endpoint, wrote result.json" >/dev/null 2>&1 \
    || loud_fail "wg log disp1 failed"

log_c="$scratch/done-c.log"
set +e
run_done disp1 >"$log_c" 2>&1
exit_c=$?
set -e

if [[ $exit_c -ne 0 ]]; then
    loud_fail "wg done failed after artifact + breadcrumb were recorded — expected success.
done.log:
$(cat "$log_c")"
fi

st_c=$(get_task_field disp1 status)
fc_c=$(get_task_field disp1 failure_class)
if [[ "$st_c" != "done" ]]; then
    loud_fail "status expected 'done' after contract satisfied, got '$st_c'"
fi
if [[ -n "$fc_c" ]]; then
    loud_fail "failure_class should be cleared on success, got '$fc_c'"
fi

# ── (d): ordinary (non-disposable) task is unaffected ────────────────
set_status_in_progress ord1

log_d="$scratch/done-d.log"
set +e
run_done ord1 >"$log_d" 2>&1
exit_d=$?
set -e

if [[ $exit_d -ne 0 ]]; then
    loud_fail "wg done refused a NON-disposable task with no artifact/breadcrumb — contract must only bind disposables.
done.log:
$(cat "$log_d")"
fi
st_d=$(get_task_field ord1 status)
if [[ "$st_d" != "done" ]]; then
    loud_fail "non-disposable status expected 'done', got '$st_d'"
fi

echo "PASS: disposable_artifact_enforcement"
exit 0
