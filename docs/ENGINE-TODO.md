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

## 2. RETRACTED — there is no week-lock bypass (it was a stale test binary)

**This entry previously claimed, as CONFIRMED, that the `shopping-add` fast lane
writes the week plan without taking the week-mutation lock. That was wrong.** The
engine is correct. Kept rather than deleted because the way I got it wrong is the
reusable lesson.

**What is actually true.** Against a binary freshly built from HEAD, holding
`week-mutation` from a separate process:

```
waited 5s   plan file unchanged
{"lane":"week-lock-busy","outcome":"answered","reply":"Someone else is changing this week's …"}
```

Fails closed, waits its whole budget, writes nothing. Exactly what leg 3 asserts.

**The real defect was in the scenario.** `week_mutation_cross_impl` installs its
engine to a FIXED path (`$TMPDIR/wg-cross-impl-lock/install-good`), and
`cargo install --path` REFUSES to reinstall a package whose version is already
present — this engine is permanently `0.1.0`. So the gate served a build from
**10:48** against a **14:22** HEAD, from source predating the lock being wired into
that lane. Leg 3 went red and reproduced 6/6, which read exactly like a
deterministic product bug.

Fixed by adding `--force` to both install paths, plus an independent
`assert_fresh` that fails loudly if the installed binary is older than any tracked
engine source. A gate pinned to a stale binary does not merely miss regressions —
it invents them.

**How I fooled myself, worth remembering:**

- I ran my "independent" separate-process experiment with **the same cached binary
  the scenario uses**, so it confirmed the scenario rather than testing the claim.
  The one variable that mattered was the one I never varied.
- Two facts sat in front of me and I did not weigh them: the binary was stamped
  10:48 while HEAD was 14:22, and *the same binary had passed earlier that day* —
  which no product-bug theory explains. A theory that cannot explain the earlier
  pass is not yet a diagnosis.
- Determinism felt like proof. 6/6 identical failures made me more confident, not
  more suspicious — but a stale artefact is perfectly deterministic too.

**Before believing any engine finding from a smoke gate: check that the binary
under test is the code you think it is.**

