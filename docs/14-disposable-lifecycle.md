# 14 — Disposable lifecycle

A **disposable** is an ephemeral, spawn-and-discard unit of work. Unlike a
normal task — whose value is the commit it merges to `main` and the graph state
it leaves behind — a disposable exists to answer a question *right now* and then
vanish. Its only durable value is:

1. the **artifact(s)** it records (`wg artifact <id> <path>` — a file, a
   reference, a captured result), and
2. the **`wg log` breadcrumb(s)** it leaves — a short, human-readable summary
   of what it found or did, for its spawner to ingest.

Everything else about a disposable is throwaway: its worktree, its transcript,
its process. If it produces neither an artifact nor a breadcrumb, it has left
nothing behind — completing it would launder a silent no-op into a "success".

A task opts into the disposable lifecycle by carrying the `disposable` tag
(`wg add "…" -t disposable`). This is the single marker the rest of the
lifecycle keys off; see `worksgood::graph::DISPOSABLE_TAG` and
`Task::is_disposable()`.

## Lifecycle

```
spawn ──▶ probe/work ──▶ record artifact + log breadcrumb ──▶ wg done ──▶ ingest ──▶ (maybe) promote
             │                       │                           │
             │                       │                           └─ enforcement gate (this doc §enforcement)
             │                       └─ the disposable's ONLY durable outputs
             └─ ephemeral worktree, transcript, process — all discarded
```

- **Ingest** — after a disposable completes, its artifact + breadcrumbs are
  folded into the spawner's session memory (see the `disposable-ingest` task).
- **Promotion** — a disposable pattern that proves repeatedly useful can be
  promoted into a durable, reusable task/function (see the
  `disposable-promotion` task).

Both downstream steps depend on the disposable actually having left something
behind — which is exactly what the enforcement gate guarantees.

## §enforcement

`wg done` enforces the **disposable contract**: a task tagged `disposable` may
not transition to `Done` until **both** of the following hold:

1. it has recorded **≥1 artifact** (`task.artifacts` is non-empty), and
2. it has left **≥1 agent `wg log` breadcrumb** — a log entry authored by a
   plain `wg log <id> "…"` call (one with no system `actor`;
   `Task::has_agent_log_breadcrumb()`). System-authored log lines (coordinator
   spawn, deliverable-preflight, verify-defer, and this gate's own refusal
   note) all set an `actor` and therefore do **not** satisfy this half.

### Where the gate runs

The gate lives in `wg done` (`src/commands/done.rs`), immediately after the
deliverable preflight and before the smoke gate. It mirrors the
deliverable-preflight refusal exactly:

- If the contract is unmet, `wg done` **refuses** with a non-zero exit and a
  message that names *which half/halves* are missing, e.g.:

  ```
  Cannot mark 'probe-endpoint' as done: this is a disposable and its
  completion contract is unmet. `wg done` will keep refusing until both hold:
    - no artifact recorded — run `wg artifact <id> <path>` for the output it produced
    - no `wg log` breadcrumb — run `wg log <id> "<what you found/did>"` before exit
  ```

- The task **stays in-progress** (it is not promoted to `Done`), and the row
  records the machine-readable failure class
  `FailureClass::DisposableContractUnmet` (`disposable-contract-unmet`) plus a
  `failure_reason`, so a retry/dispatch layer can see *why* it was refused.
- `wg done` keeps refusing on every subsequent attempt until both halves hold.
- Once the disposable records its artifact **and** logs a breadcrumb, `wg done`
  succeeds, the task reaches `Done`, and the `disposable-contract-unmet` marker
  is cleared (alongside the analogous `deliverable-missing` /
  `no-operational-output` cleanup).

### Scope

The contract binds **only** tasks that opt in via the `disposable` tag. An
ordinary task with no artifact and no breadcrumb is unaffected — this gate is
strictly additive to existing `wg done` behaviour (blockers, deliverable
preflight, smoke gate, verify).

There is deliberately **no agent escape hatch**: the whole point of a
disposable is that the artifact + breadcrumb are its reason to exist, so an
agent cannot mark one done empty. (A human can still edit the graph row
directly if a genuine false positive ever arises.)

### Rationale

Disposables feed two downstream consumers — `disposable-ingest` (fold results
into session memory) and `disposable-promotion` (promote repeatedly-useful
patterns). Both are no-ops if the disposable left nothing behind. Enforcing the
contract at completion time means the ingest step is *guaranteed* to have
something to consume, and a disposable that "ran green" but produced nothing is
surfaced as a real, retryable failure instead of a phantom success.

### Test & regression coverage

- Unit: `test_disposable_done_refused_without_artifact` (the named failing test
  written first), plus `test_disposable_done_refused_without_log`,
  `test_disposable_done_allowed_with_artifact_and_log`, and
  `test_non_disposable_done_unaffected_by_contract` in
  `src/commands/done.rs`.
- Smoke: `tests/smoke/scenarios/disposable_artifact_enforcement.sh`, owned by
  `disposable-artifact-enforcement` in `tests/smoke/manifest.toml`. It drives
  the real `wg add` / `wg artifact` / `wg log` / `wg done` CLI paths end-to-end:
  a no-artifact disposable is refused, an artifact-only disposable is still
  refused for the missing breadcrumb, and once both are present `wg done`
  succeeds and clears the marker — while an ordinary task is unaffected.

## §ingest

Enforcement guarantees a completed disposable left an artifact + breadcrumb
behind. **Ingest** is what makes that value *persist*: on `wg done`, the
disposable's durable outputs are folded into the **spawning** named agent's
persistent `session-summary.md`, riding the #50 agent↔session binding. This is
the disposable **middle path** (iteration-2 directive 6b) — the disposable
itself stays ephemeral (worktree, transcript, process all discarded), but the
answer it produced survives in its spawner's memory and is injected into the
spawner's *next* task via `{{bound_session_summary}}` — the same recall path
`bind-named-agents` proved in `docs/09`.

So Bruno's recipe-scrape disposable vanishes, but "found 3 recipes;
lemon-garlic is the family favourite" and the artifact path land in Bruno's
memory and colour how Bruno plans next week.

### The spawner link — `spawned-by:<agent>`

A disposable records who spawned it with a single `spawned-by:<agent>` tag
(`worksgood::graph::SPAWNED_BY_TAG_PREFIX`; read via `Task::spawned_by()`),
where `<agent>` is the spawner's **agent content-hash** — exactly the key
`chat_sessions::session_for_agent` matches against a bound session's
`agent_id`. Storing the link as a tag (rather than a new `Task` field) mirrors
the `disposable` tag idiom and touches no `Task` construction site.

The tag is written at `wg add` time in one of two ways:

- **Explicit:** `wg add "…" -t disposable --spawned-by <agent-hash>`.
- **Auto-derived:** for a `-t disposable` task created *inside* a task context
  (`WG_TASK_ID` set) with no explicit `--spawned-by`, `wg add` resolves the
  spawning task's `agent` (the running named agent's content-hash) and records
  it. So when Bruno — a bound named agent — runs `wg add … -t disposable`
  mid-task, the spawner is captured automatically.

### Where the ingest runs

The ingest lives in `wg done` (`src/commands/done.rs`), immediately after the
task is promoted to `Done` (so the enforcement contract has already
guaranteed the artifact + breadcrumb) and `notify_graph_changed`. It calls
`disposable_ingest::ingest_disposable_into_spawner`, which:

1. no-ops unless the task is a disposable **and** carries a `spawned-by:` tag
   **and** that agent has a bound session (a disposable with no bound spawner
   is a benign no-op — there is simply no memory to fold into);
2. resolves the spawner's session via `session_for_agent` → appends a markdown
   block (title, artifacts, and the agent `wg log` breadcrumbs) to
   `.wg/chat/<uuid>/session-summary.md`, in the first-person "your own memory"
   voice `resolve_bound_session_summary` frames bound memory with;
3. is **idempotent** — the block carries a per-disposable
   `<!-- disposable-ingest:<id> -->` marker, so a re-run of `wg done` detects
   it and leaves the file untouched (no double-append).

The breadcrumbs folded in are snapshotted **before** the `Done` transition, so
the "Task marked as done" system log entry the transition itself appends (which
carries `actor = task.assigned`, i.e. `None` for an unassigned task) is never
mistaken for an agent breadcrumb. Ingest is best-effort: it never fails
`wg done` (the disposable is already Done and its outputs survive on the row
regardless); a failure is surfaced as a warning and logged as a
`disposable-ingest` breadcrumb on the task.

### Test & regression coverage

- Unit: `test_disposable_artifact_ingested_into_spawner_session` (the named
  failing test written first) in `src/disposable_ingest.rs`, plus
  `ingest_is_idempotent`, `non_disposable_is_not_ingested`, and
  `disposable_without_spawner_or_binding_is_noop`. The named test also asserts
  the ingested string is exactly what `chat_sessions::agent_session_summary`
  (the function the spawn path injects) returns — i.e. it lands in the
  spawner's *next-task* memory, not just on disk.
- Smoke: `tests/smoke/scenarios/disposable_ingest.sh`, owned by
  `disposable-ingest` in `tests/smoke/manifest.toml`. It drives the real
  `wg agent` / `wg add --spawned-by` / `wg artifact` / `wg log` / `wg done`
  CLI paths end-to-end: a disposable spawned by a bound "bruno" agent folds its
  artifact + breadcrumb into bruno's session summary, the ingest appends (prior
  memory preserved) without leaking the "Task marked as done" line, is
  idempotent (exactly one marker after two `wg done` calls), and a disposable
  with an unbound spawner still completes as a no-op.
