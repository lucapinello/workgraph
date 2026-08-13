# Engine TODO — known defects, diagnosed, not yet fixed

Fork-only issues in Casa's engine layer. Upstream-relevant ones live in
`docs/UPSTREAM-DIVERGENCE.md` instead; nothing here is a `wg` concern.

Each entry states the defect, the evidence, and the proposed fix — so the next
session starts from a diagnosis rather than a symptom.

---

## 1. `worktree_dirty` treats disposable evidence as uncommitted source

**Where:** `src/disk_sentinel.rs:708`, called at `:846` (the `safe_remove_owned_path`
guard) and `:1112` (the dry-run branch). Fork-only — upstream has no such function.

```rust
fn worktree_dirty(path: &Path) -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        // …
        .map(|o| !o.stdout.is_empty() || !o.status.success())
        .unwrap_or(true)
}
```

**The defect.** `--untracked-files=normal` counts UNTRACKED files as dirty, so any
build or test byproduct that is neither committed nor gitignored pins its worktree
forever. On a long-running family box disk therefore grows without bound and the
cleanup that exists to stop it never fires. `unwrap_or(true)` also means a git
invocation that fails for any reason preserves — correct as a default, but it
compounds the same direction.

**Measured on the live box, 2026-08-13:**

- 58 agent worktrees, **26 GB**; `wg disk cleanup` reaped 0 of them every run.
- 44 are clean AND their branch is already merged into `main` — 15 GB with nothing
  to lose.
- Across ALL 58, the only uncommitted content is untracked output under
  `disposables/` (UI screenshot evidence — the directory is named for being
  disposable) plus, in four worktrees, `.wg-reaped-wip.json`.
- Those four sidecars each name a WIP **commit** and an out-of-tree **patch**.
  All four commits are present in the repo and all four patch files exist
  (17 MB, 17 MB, 10 KB, 17 MB; 34 files each). The work provably survives
  `git worktree remove`.

So the guard was protecting screenshots, while the one thing that *was* real work
had already been preserved somewhere the guard does not look.

**Proposed fix — use the preservation contract, not a path allowlist.**

The reap path already writes `.wg-reaped-wip.json` + a named WIP commit + an
out-of-tree patch whenever there is work worth keeping (`wg_preserve_reaped_wip`,
`src/commands/spawn/execution.rs`). That is a stronger signal than dirtiness:

- if a valid sidecar exists and BOTH its `commit` resolves and its `patch` file is
  present, the worktree is expendable — say so, and let the reap proceed;
- otherwise keep the current conservative behaviour.

Deliberately NOT proposed: switching to `--untracked-files=no`. An untracked file
can be genuinely new source, and silently dropping it is the one failure this
guard exists to prevent. A path allowlist (`disposables/`, `spikes/`) is also
rejected — it encodes today's directory names into a safety check, and the next
evidence directory would leak straight through.

**Guard it with teeth.** Two cases, both mutation-checkable: a worktree whose
sidecar's patch is MISSING must still be preserved; a worktree with untracked
evidence and a complete sidecar must be reaped. The second is the one that fails
today.

**Not urgent, but it does not shrink on its own** — every reaped agent adds a
worktree, and none are ever reclaimed.

---

## 2. `week_mutation_cross_impl` leg 3 — the engine does not fail closed on a held week lock

**Status: RED and deterministic. Do not paper over it.** `feedlock-and-postverify-reds-are-not-flakes`
applies: an intermittent-looking red in the cross-impl lock seam is also what a real
concurrency bug looks like, and widening a budget or adding a retry deletes the only
evidence.

**What leg 3 asserts** (`claw3d-bridge/test/crossImplWeekLock.mjs:194`): the gateway
holds `week-mutation`, and inside that hold the engine is asked to
`add cardamom to the shopping list`. The engine should wait its ~5s budget, then
refuse with `week-lock-busy`, leaving the plan byte-identical.

**Observed, 6 runs out of 6 (2026-08-13):**

```
lane:   shopping-add
reply:  Done — cardamom on the shopping list 🛒
apply:  {"outcome":"applied","report":"…","week":"2026-W33"}
```

Three sub-assertions fail: no `week-lock-busy`, the plan file is NOT byte-identical
("the engine wrote anyway"), and `result.ms < 4000` — so it never waited its budget.
It acquired immediately and wrote.

**Established facts, not inference:**

- Deterministic: 6/6, 7–10s each (the engine install is cached, so runs are cheap —
  loop it, do not reason from one occurrence).
- Legs 1, 2, 4 and 6 all PASS. So the lock works in general, and leg 6's control
  (lockless engine ⇒ updates ARE lost) still has teeth.
- **Leg 2 vs leg 3 is the only structural difference.** Leg 2 uses `engineAsync` —
  spawn, verify nothing lands during the hold, release, verify it lands. It passes,
  which proves the engine genuinely blocks on this lock. Leg 3 uses `engineSync`,
  running the engine synchronously INSIDE the hold and thereby blocking the JS event
  loop.
- The command now routes through a dedicated **`shopping-add` fast lane**
  (`fast_lane.rs:429`, `:1489`, `:1663`), which mutates plan content and should sit
  inside the `with_week_mutation_lock` closure at `fast_lane.rs:2440`.
- `projectLock.mjs` documents that age-based stealing was REMOVED (a holder's lock was
  once unlinked when mtime age exceeded a 15s default even though the holder was alive;
  "there is now NO age-based break"). And the engine returned in <4s, so age-based
  stealing does not explain it either.
- It PASSED earlier the same day (rc=0, ~20min, full build) with the SAME cached
  binary that now fails. So the change is environmental, not the engine build.

**Competing hypotheses, none confirmed:**

1. **A real lock bypass on the `shopping-add` lane** — the lane writes plan content
   through a path that does not take (or takes a different) week lock. If so this is
   family-data loss: two writers, no serialisation. Most serious; check first.
2. **The Rust twin still steals where the JS side stopped** — the JS lock removed
   age-based breaking; if `project_lock.rs` did not, the engine could break in on a
   rule the gateway no longer plays by. The <4s timing argues against age-based, but
   not against some other break rule.
3. **Leg 3 is an invalid construction** — you may not be able to hold this lock while
   blocking the event loop synchronously, in which case the test is asserting something
   the design never promised, and leg 2 already covers the real property.

**Next step:** decide between (1) and (3) first, because they point opposite ways —
(1) is an engine fix, (3) is deleting a test that proves nothing. The cheap experiment
is to hold the lock from a SEPARATE process (not the same event loop) and re-run the
same engine command: if it then refuses with `week-lock-busy`, leg 3 is the artefact;
if it still writes, hypothesis 1 is live and urgent.

**Do not mark the smoke suite green while this is red.**
