# Engine TODO — known defects, diagnosed, not yet fixed

Fork-only issues in Casa's engine layer. Upstream-relevant ones live in
`docs/UPSTREAM-DIVERGENCE.md` instead; nothing here is a `wg` concern.

Each entry states the defect, the evidence, and the proposed fix — so the next
session starts from a diagnosis rather than a symptom.

---

## 1. SHIPPED (partly) — `worktree_dirty` no longer lies, but disk is still pinned

**Where:** `src/disk_sentinel.rs`. Fork-only — upstream has no such function.

### What shipped (2026-08-14)

Three changes, each with a mutation-proven test:

1. **The reap contract releases a worktree.** If `.wg-reaped-wip.json` is present AND its
   `commit` resolves in that repo AND its `patch` is on disk, the worktree is expendable.
   Every check is positive evidence; anything unparseable, unresolvable or missing means
   NOT preserved, and the conservative path holds.
2. **The reason names what is actually dirty.** `WorktreeDirt` splits `TrackedDirty` from
   `UntrackedOnly(n)`. Seven worktrees were pinning 11 GB while reporting "owning worktree
   has uncommitted source" with **zero** tracked-dirty files and 13 untracked
   `disposables/` directories. The reason was not merely unhelpful, it was false.
3. **Absent is not unknown.** A cache whose owning worktree no longer exists is expendable
   — this guard protects uncommitted source inside a worktree, and there is no worktree.
   That mislabel covered cargo caches for agents reaped days earlier.

Deliberately NOT done, as reasoned before: `--untracked-files=no` (an untracked file can be
new source) and a path allowlist (`disposables/`, `spikes/`) which would encode today's
directory names into a safety check.

### What did NOT happen: the disk is still 11 GB

`wg disk cleanup` still reports **reaped=0**. The false verdict is gone — "uncommitted
source" now appears zero times — but the remaining holders are:

| preserved reason | count |
|---|---|
| one or more recorded owners are active/inconclusive | 28 |
| N untracked path(s) and no tracked-dirty file | 14 |
| path has open files | 1 |

So the next step is a DIFFERENT gate: **owner liveness**, not worktree dirt. 28 paths are
held because their recorded owners resolve as active or inconclusive. Whether those agents
are really alive is the question to answer next — start by resolving each recorded owner id
against the graph and the process table, and expect the same shape as everything else here:
an inconclusive lookup being reported as an active owner.

The 14 untracked-only entries are preserved BY DESIGN and need a policy decision, not a
bug fix: reaping a worktree whose only dirt is untracked evidence means accepting that an
untracked file might have been new source. That is the owner's call to make, and it is now
visible in the report rather than hidden behind the wrong words.

## 2. RETRACTED — there is no week-lock bypass (the gate certified its own mutant)

**This entry previously claimed, as CONFIRMED, that the `shopping-add` fast lane
writes the week plan without taking the week-mutation lock. That was wrong.** The
engine is correct. Kept rather than deleted because the way I got it wrong is the
reusable lesson — and because it took two attempts to find the real mechanism.

**What is actually true**, with `week-mutation` held from a separate process, against
the live release binary:

```
engine ms=5090   plan changed: no
{"lane":"week-lock-busy","outcome":"answered","reply":"Someone else is changing this week's …"}
```

Waits its whole budget, writes nothing, fails closed — exactly what leg 3 asserts.
The same probe shows it creating `<root>/.casa/locks/`: it takes the lock, visibly,
on disk.

**The defect was in the scenario, in two layers.**

*Layer 1 — a fixed install path.* The gate installs to
`$TMPDIR/wg-cross-impl-lock/install-good`, and `cargo install --path` REFUSES to
reinstall a package whose version is already present — this engine is permanently
`0.1.0`. So it served a 10:48 build against a 14:22 HEAD. Fixed with `--force` plus
an `assert_fresh` mtime check.

*Layer 2 — a shared target dir, found only while verifying the fix for layer 1.*
With `--force` in place the gate STILL went red, and the installed "good" binary was
**byte-identical to this scenario's own teeth mutant** — the deliberately lockless
engine. Proven two ways: `cmp` on the two installs, and the mutant's signature
behaviour, which is that it never creates `.casa/locks` at all. One
`CARGO_TARGET_DIR` was shared by the main tree and the mutant worktree, both building
`worksgood v0.1.0`, so the previous run's mutant artifacts satisfied the next good
build — `Finished in 2.07s`, no recompile, mutant installed as the subject.

Fixed with a target dir per source tree, both binaries built before either is judged,
and `assert_distinct`: the subject may not be byte-identical to its own control. That
check is mechanism-independent — it holds whatever cargo does next.

**How I fooled myself, worth remembering:**

- I ran my "independent" separate-process experiment with **the same cached binary
  the scenario uses**, so it confirmed the scenario rather than testing the claim.
  The one variable that mattered was the one I never varied.
- Two facts sat in front of me and I did not weigh them: the binary was stamped 10:48
  while HEAD was 14:22, and *the same binary had passed earlier that day* — which no
  product-bug theory explains. A theory that cannot explain the earlier pass is not
  yet a diagnosis.
- Determinism felt like proof. 6/6 identical failures made me more confident, not
  more suspicious — but a stale artefact is perfectly deterministic too.
- After fixing layer 1 I nearly called the engine broken a second time, because the
  red persisted and the binary was now provably fresh *by mtime*. Freshness of the
  artifact is not identity of the code inside it.

**Before believing any engine finding from a smoke gate: check that the binary under
test is the code you think it is — and that it is not the gate's own control.**
