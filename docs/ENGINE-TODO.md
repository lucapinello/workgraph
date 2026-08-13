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
