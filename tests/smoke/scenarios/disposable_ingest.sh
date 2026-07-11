#!/usr/bin/env bash
# Scenario: disposable_ingest
#
# Regression for the disposable ingest step (docs/14-disposable-lifecycle.md
# §ingest): when a named agent spawns a disposable, the disposable's durable
# outputs — its artifact(s) + `wg log` breadcrumb(s), guaranteed present by the
# enforcement gate — are folded into the SPAWNING agent's persistent
# `session-summary.md` on `wg done`, riding the #50 agent↔session binding. So a
# disposable stays ephemeral but its *value* persists into its spawner's memory
# and is injected into the spawner's next task via `{{bound_session_summary}}`.
#
# The link disposable → spawner is a `spawned-by:<agent>` tag written at
# `wg add` time (explicit `--spawned-by`, or auto-derived from the parent
# task's agent inside a task context).
#
# Asserts:
#   (a) After a disposable spawned by a bound agent reaches `wg done`, that
#       agent's `session-summary.md` contains the disposable's artifact string
#       AND its breadcrumb finding.
#   (b) The ingest APPENDS — pre-existing memory (a prior-week decision) is
#       preserved, not clobbered.
#   (c) The ingest does NOT leak the "Task marked as done" system log entry
#       (only genuine agent breadcrumbs are folded in).
#   (d) Ingest is idempotent: re-running `wg done` does not double-append.
#   (e) A disposable whose spawner has NO bound session is a benign no-op
#       (`wg done` still succeeds; nothing to ingest into).
#
# The pure ingest function is additionally pinned by the
# test_disposable_artifact_ingested_into_spawner_session unit test. This
# scenario drives the real `wg agent` / `wg add --spawned-by` / `wg artifact` /
# `wg log` / `wg done` CLI paths end-to-end.
#
# Requires: python3 (to read graph rows + the session registry) and wg.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v python3 >/dev/null 2>&1; then
    loud_skip "MISSING PYTHON3" "python3 needed to read the graph row + session registry"
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

# ── Create a named agent "bruno" and bind it to a persistent session ──
role=$(wg --dir "$wg_dir" role list 2>/dev/null | grep -oE '^  [0-9a-f]{8}' | head -1 | tr -d ' ')
tradeoff=$(wg --dir "$wg_dir" tradeoff list 2>/dev/null | grep -oE '^  [0-9a-f]{8}' | head -1 | tr -d ' ')
[[ -n "$role" ]] || loud_fail "could not resolve a default role id from 'wg role list'"
[[ -n "$tradeoff" ]] || loud_fail "could not resolve a default tradeoff id from 'wg tradeoff list'"

wg --dir "$wg_dir" agent create bruno --role "$role" --tradeoff "$tradeoff" >/dev/null 2>&1 \
    || loud_fail "wg agent create bruno failed"
bruno_short=$(wg --dir "$wg_dir" agent list 2>/dev/null | grep -i 'bruno' | grep -oE '^  [0-9a-f]{8}' | head -1 | tr -d ' ')
[[ -n "$bruno_short" ]] || loud_fail "could not resolve bruno's agent id"

wg --dir "$wg_dir" agent session "$bruno_short" >/dev/null 2>&1 \
    || loud_fail "wg agent session (bind) failed for bruno"

# Read bruno's full agent hash + bound session uuid from the registry.
read -r bruno_hash bruno_uuid < <(python3 - "$wg_dir/chat/sessions.json" <<'PY'
import json, sys
reg = json.load(open(sys.argv[1]))
for uuid, meta in reg.get("sessions", {}).items():
    if meta.get("label", "").endswith("bruno") or "bruno" in "".join(meta.get("aliases", [])):
        print(meta["agent_id"], uuid)
        break
PY
)
[[ -n "${bruno_hash:-}" && -n "${bruno_uuid:-}" ]] \
    || loud_fail "could not read bruno's agent hash / session uuid from the registry"

summary="$wg_dir/chat/$bruno_uuid/session-summary.md"
printf '## Prior work\nWe chose a 50-50 pasta split; Sara dislikes chickpeas.\n' > "$summary"

# ── Helpers ──────────────────────────────────────────────────────────
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

run_done() {
    local tid="$1"
    unset WG_AGENT_ID
    unset WG_SMOKE_AGENT_OVERRIDE
    WG_WORKTREE_PATH="$project_root" \
    WG_BRANCH="wg/smoke/$tid" \
    WG_PROJECT_ROOT="$project_root" \
        wg --dir "$wg_dir" done "$tid" --skip-smoke
}

# ── Spawn a disposable that hands a recipe result back to bruno ───────
wg --dir "$wg_dir" add "Scrape 3 chicken recipes" \
    --id disp1 -t disposable --spawned-by "$bruno_hash" >/dev/null 2>&1 \
    || loud_fail "wg add of disp1 (disposable) failed"

# The spawned-by tag must have been recorded on the row.
if ! grep -q "spawned-by:$bruno_hash" "$wg_dir/graph.jsonl"; then
    loud_fail "disp1 did not record the spawned-by:<bruno> tag"
fi

wg --dir "$wg_dir" artifact disp1 docs/artifacts/chicken-recipes.md >/dev/null 2>&1 \
    || loud_fail "wg artifact disp1 failed"
wg --dir "$wg_dir" log disp1 "Found 3 recipes; lemon-garlic is the family favourite." >/dev/null 2>&1 \
    || loud_fail "wg log disp1 failed"

set_status_in_progress disp1

log_done="$scratch/done-disp1.log"
set +e
run_done disp1 >"$log_done" 2>&1
exit_done=$?
set -e
if [[ $exit_done -ne 0 ]]; then
    loud_fail "wg done disp1 failed — a contract-complete disposable must complete.
done.log:
$(cat "$log_done")"
fi

# ── (a) artifact + breadcrumb folded into bruno's session memory ─────
if ! grep -q "docs/artifacts/chicken-recipes.md" "$summary"; then
    loud_fail "spawner summary is missing the disposable's artifact string.
summary:
$(cat "$summary")"
fi
if ! grep -q "lemon-garlic is the family favourite" "$summary"; then
    loud_fail "spawner summary is missing the disposable's breadcrumb finding.
summary:
$(cat "$summary")"
fi

# ── (b) append, not clobber ──────────────────────────────────────────
if ! grep -q "50-50 pasta split" "$summary"; then
    loud_fail "ingest clobbered pre-existing memory (the prior-week decision is gone)."
fi

# ── (c) no system-log leak ───────────────────────────────────────────
if grep -q "Task marked as done" "$summary"; then
    loud_fail "ingest leaked the 'Task marked as done' system log entry into memory."
fi

# ── (d) idempotent — a second wg done must not double-append ──────────
set_status_in_progress disp1
before_hash=$(python3 -c "import hashlib;print(hashlib.sha256(open('$summary','rb').read()).hexdigest())")
set +e
run_done disp1 >/dev/null 2>&1
set -e
after_hash=$(python3 -c "import hashlib;print(hashlib.sha256(open('$summary','rb').read()).hexdigest())")
if [[ "$before_hash" != "$after_hash" ]]; then
    loud_fail "re-running wg done double-appended the ingest block (not idempotent)."
fi
# Exactly one ingest marker for disp1.
marker_count=$(grep -c "disposable-ingest:disp1" "$summary")
if [[ "$marker_count" != "1" ]]; then
    loud_fail "expected exactly 1 ingest marker for disp1, found $marker_count"
fi

# ── (e) disposable with an unbound spawner → benign no-op ─────────────
wg --dir "$wg_dir" add "Probe an unbound spawner" \
    --id disp2 -t disposable --spawned-by "deadbeefdeadbeef" >/dev/null 2>&1 \
    || loud_fail "wg add of disp2 failed"
wg --dir "$wg_dir" artifact disp2 out.txt >/dev/null 2>&1 || loud_fail "wg artifact disp2 failed"
wg --dir "$wg_dir" log disp2 "nothing to ingest here" >/dev/null 2>&1 || loud_fail "wg log disp2 failed"
set_status_in_progress disp2
log_disp2="$scratch/done-disp2.log"
set +e
run_done disp2 >"$log_disp2" 2>&1
exit_disp2=$?
set -e
if [[ $exit_disp2 -ne 0 ]]; then
    loud_fail "wg done disp2 failed — an unbound-spawner disposable must still complete.
done.log:
$(cat "$log_disp2")"
fi

echo "PASS: disposable_ingest"
exit 0
