#!/usr/bin/env bash
# Scenario: cron_template_reregistration_no_reblock
#
# Regression for the cron-re-registration deadlock (tonight's plan-pipeline
# stall): when a recurring cron fired, the COMPLETED run's task id was
# re-registered as next week's open instance, so children created
# `--after <cron-id>` re-blocked on the FUTURE instance — their finished work
# stranded ("blocked by <cron-id>: Open", agents 2285/2286, ~40 min lost).
#
# CONTRACT under test (src/cron.rs::mint_due_cron_instances / mint_cron_instance
# + src/commands/service/coordinator.rs Phase 2.94 + src/query.rs template gate):
#
#  1. TEMPLATE NEVER DISPATCHED: a `--cron-template` task is never itself
#     ready — it only mints instances (is_time_ready → cron_template ⇒ false).
#  2. DISTINCT INSTANCE PER FIRING: each due firing mints a NEW, distinct
#     instance task id (`<template>-<period>`), not a re-registration of the
#     template id.
#  3. NO RE-BLOCK: a child created `--after <instance-1>` and left Open is NOT
#     re-blocked by the cron when the NEXT firing mints instance-2 — its
#     after-edge binds to the finished run, not the recurring definition.
#
# Credential-free: the template's exec is `true`, so the real coordinator mints
# AND dispatches instances that self-complete in shell mode — no LLM. The
# daemon runs with max_agents>=1 (max_agents=0 short-circuits before the
# maintenance/mint phase). python3 drives two distinct fire times.

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/_helpers.sh"

require_wg

if ! command -v python3 >/dev/null 2>&1; then
    loud_skip "MISSING PYTHON3" "python3 required to inspect graph.jsonl"
fi

scratch=$(make_scratch)
cd "$scratch"

fake_home="$scratch/home"
mkdir -p "$fake_home/.config"
export HOME="$fake_home"
export XDG_CONFIG_HOME="$fake_home/.config"

if ! wg init -x shell >init.log 2>&1; then
    loud_fail "wg init failed: $(tail -5 init.log)"
fi

# A weekly Sunday 20:30 UTC cron TEMPLATE. `--exec true` so any minted instance
# self-completes in shell mode (no worker/LLM needed).
add_out=$(wg add "weekly-plan-sunday" --cron "0 30 20 * * 1" --cron-template --exec "true" --no-place 2>&1) || \
    loud_fail "wg add --cron-template failed: $add_out"
template_id=$(echo "$add_out" | grep "^Added task:" | sed -E 's/.*\(([^)]+)\).*/\1/')
[[ -n "$template_id" ]] || loud_fail "could not parse template id from: $add_out"

wg_dir="$scratch/.wg"
graph="$wg_dir/graph.jsonl"

set_next_fire() {  # <graph> <rfc3339-ts>
    python3 - "$1" "$template_id" "$2" <<'PY'
import json, sys, os
path, tid, ts = sys.argv[1], sys.argv[2], sys.argv[3]
tmp = path + ".tmp.%d" % os.getpid()
out = []
with open(path) as f:
    for line in f:
        line = line.rstrip("\n")
        if not line.strip():
            continue
        obj = json.loads(line)
        if obj.get("kind") == "task" and obj.get("id") == tid:
            obj["next_cron_fire"] = ts
        out.append(json.dumps(obj, separators=(",", ":")))
with open(tmp, "w") as f:
    for o in out:
        f.write(o + "\n")
os.replace(tmp, path)
PY
}
instances_of() {  # <graph>  → prints minted instance ids, one per line
    python3 - "$1" "$template_id" <<'PY'
import json, sys
path, tid = sys.argv[1], sys.argv[2]
with open(path) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        obj = json.loads(line)
        if obj.get("kind") == "task" and obj.get("cron_instance_of") == tid:
            print(obj["id"])
PY
}
ago() { python3 -c "import datetime,sys;print((datetime.datetime.now(datetime.timezone.utc)-datetime.timedelta($1)).strftime('%Y-%m-%dT%H:%M:%SZ'))"; }

# ── 1. Template is NEVER itself ready (even when due) ──────────────────────
set_next_fire "$graph" "$(ago 'days=8')"
if wg ready 2>&1 | grep -q "$template_id"; then
    loud_fail "cron TEMPLATE appeared in wg ready — templates must never be dispatched directly"
fi
echo "PASS (1/3): cron template is never itself ready"

# ── 2. First firing mints a distinct instance (via the real coordinator) ──
start_wg_daemon "$scratch" --max-agents 2 --no-chat-agent --interval 2
wg_dir="$WG_SMOKE_DAEMON_DIR"
graph="$wg_dir/graph.jsonl"

inst1=""
for _ in $(seq 1 20); do
    inst1=$(instances_of "$graph" | head -1)
    [[ -n "$inst1" ]] && break
    sleep 1
done
[[ -n "$inst1" ]] || loud_fail "no instance minted from the due template within 20s"
[[ "$inst1" != "$template_id" ]] || \
    loud_fail "minted instance reuses the template id ($inst1) — re-registration bug is back"
echo "PASS (2/3): firing minted a DISTINCT instance '$inst1' (not the template id)"

wg --dir "$wg_dir" service stop --force >/dev/null 2>&1 || true
sleep 1
# The run finished; ensure it is Done, then bind a child --after it, left Open
# (added while the daemon is stopped so it is not dispatched).
wg --dir "$wg_dir" done "$inst1" >/dev/null 2>&1 || true
child_out=$(wg --dir "$wg_dir" add "downstream-plan-work" --id downstream-plan-work --after "$inst1" --no-place 2>&1) || \
    loud_fail "wg add child --after instance failed: $child_out"

# ── 3. Next firing mints a NEW instance; child is NOT re-blocked by the cron ─
set_next_fire "$graph" "$(ago 'minutes=2')"
start_wg_daemon "$scratch" --max-agents 2 --no-chat-agent --interval 2
wg_dir="$WG_SMOKE_DAEMON_DIR"
graph="$wg_dir/graph.jsonl"

inst2=""
for _ in $(seq 1 20); do
    inst2=$(instances_of "$graph" | grep -v "^$inst1\$" | head -1)
    [[ -n "$inst2" ]] && break
    sleep 1
done
[[ -n "$inst2" ]] || loud_fail "next firing did not mint a second, distinct instance within 20s"
wg --dir "$wg_dir" service stop --force >/dev/null 2>&1 || true
sleep 1

# THE FIX: the child's only cron-related blocker is the FINISHED run inst1
# (Done ⇒ satisfied). The re-registration (new id inst2) must NOT appear as an
# unresolved blocker of the child, and the template id must never block it.
# (Agency scaffolding like `.assign-*` is ignored — we assert specifically that
# no CRON template/instance re-blocks the finished-run child.)
python3 - "$graph" "downstream-plan-work" "$template_id" "$inst1" "$inst2" <<'PY'
import json, sys
graph, child_id, template_id, inst1, inst2 = sys.argv[1:6]
tasks = {}
for line in open(graph):
    line = line.strip()
    if not line:
        continue
    o = json.loads(line)
    if o.get("kind") == "task":
        tasks[o["id"]] = o
child = tasks[child_id]
after = child.get("after", [])
assert inst1 in after, f"child should bind --after the finished run {inst1}, got {after}"
assert template_id not in after, f"child must NOT depend on the recurring template {template_id}: {after}"
assert inst2 not in after, f"re-registration re-pointed the child onto the new instance {inst2}: {after}"
# The finished run stays Done — never flipped back to Open by re-registration.
assert tasks[inst1]["status"] == "done", f"finished run {inst1} must stay Done, got {tasks[inst1]['status']}"
# No unresolved CRON blocker remains for the child.
def satisfied(s): return s in ("done", "abandoned")
cron_blockers = [b for b in after
                 if not satisfied(tasks.get(b, {}).get("status", "open"))
                 and (b == template_id or tasks.get(b, {}).get("cron_instance_of"))]
assert not cron_blockers, f"child re-blocked by cron task(s) after re-registration: {cron_blockers}"
print("child not re-blocked by cron; blockers(after)=", after)
PY
rc=$?
[[ $rc -eq 0 ]] || loud_fail "re-registration RE-BLOCKED the finished-run child — the tonight deadlock is back"
echo "PASS (3/3): re-registration minted '$inst2' and did NOT re-block the finished-run child"

echo "PASS: cron template re-registration mints distinct instances and never re-blocks children"
exit 0
