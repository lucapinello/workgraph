# Divergence from graphwork/wg — why, how much, and the way back

This fork's relationship to upstream has until now lived only in people's heads. This file
is the record, so it is never re-derived by archaeology.

Measured **2026-08-12**. Fork point: **2026-07-20** (`29740403`).

## Where we are

| | commits | files | lines |
|---|---|---|---|
| ours since fork | 310 | 159 | +100,845 / −1,640 |
| upstream since fork | 459 | 683 | +195,696 / −30,816 |

Upstream is active — 289 commits in August 2026, most recent 3 days before this was
written. Roughly **150 commits/week of combined divergence**, with **54 files changed on
both sides**, concentrated in the dispatcher core (`coordinator.rs`, `eval_lifecycle.rs`,
`service/mod.rs`, `config.rs`, `disk_sentinel.rs`).

Our diff splits cleanly by intent:

- **80 new files, ~71k lines (69%)** — the Casa layer. 31 of them under `src/notify/`
  (upstream has 12 files there, we have 43); the rest are smoke scenarios and fixtures.
  These cost nothing to sync.
- **81 modified upstream files, ~31k lines (31%)** — the actual sync cost. **16k of that is
  one file**: `src/commands/telegram.rs`, which was 869 lines at the fork point. We deleted
  677 of them (78%) and added 15,349 — it is a rewrite living in their file, not a superset.

## Why a separate crate is possible

Three facts, each verified rather than assumed:

1. **`worksgood` is a library crate.** Upstream's `src/lib.rs` exposes **97 `pub mod`s**,
   including `commands`, `config`, `cron`, `graph`, `notify`, `parser`, `service`. A Casa
   binary can *depend on* wg instead of patching it.
2. **No dependency inversion.** Our additions to `coordinator.rs` call only `worksgood::*`
   and `std` — never a Casa module. Upstream never calls into us, so there is nothing to
   unpick; our dispatcher edits are modifications to *their* policy, not hooks into ours.
3. **Much of our engine diff is general.** A first-cut bucketing of the 223 commits that
   touch upstream files:

   | bucket | commits | |
   |---|---|---|
   | **A** — general engine fix, would benefit wg with no Casa | 71 | 31% |
   | **B** — Casa behaviour, belongs in our crate | 106 | 47% |
   | **C** — merge commits, rustfmt, noise | 46 | 20% |

   **This is a keyword heuristic, not a per-commit review.** It is wrong in both directions
   — `fix(macos): three inert safety systems…` landed in B only because it contains the
   word "family". Treat the shape as real and every individual row as unverified.

## Known bucket-A candidates

Confirmed still present upstream unless noted:

| commit | what | status |
|---|---|---|
| `d72abfa0` | verdict store is accidentally quadratic (96% → 2% of a core) | patch written |
| — | absent agent record makes caches immortal | patch written |
| — | `pipe2` breaks the macOS build | **PR branch pushed** |
| `2db5230c` | one unverifiable verdict starves the whole graph — upstream still bails on the first bad file | to send |
| `bfed378c` | transport-exhausted → per-task quarantine | to send |
| `9b823397` | self-healing spawn circuit breaker | to send |
| `ee9c45d1` | heartbeat guard uses fd 9, not a bash-4 named fd | to send |
| `691965eb` | cron-fanout orphaning, zombie handler locks, poison-task stall | to send |
| `f9f92435` | protect production crons from a cleanup sweep | to send |
| `b949bbe5` | `classify_error` done-spoof hides a real provider outage | to send |
| `16888d31` | unwedge PendingEval | to send |

We have contributed upstream before (`217dc029` references their PR #53), so the channel
exists.

Prepared patches and their verification notes:
`casa-cert-run3/prep/upstream-patches/`.

## Rules for an upstream patch

Learned from writing the first three:

- Rebuild against **current `gwwg/main` in a clean worktree**, to *their* structure. Do not
  port our file's shape — upstream's `load_durable_verdicts` has no quarantine split, so
  our fix had to be re-expressed and came out simpler.
- **Baseline their suite before and after.** Four `disk_sentinel` tests already fail on
  macOS; without the before-run that reads as "our patch broke it".
- Note related-but-unchanged issues in the PR text rather than silently fixing them.

## macOS is second-class upstream

`cargo build` on upstream HEAD fails outright on Darwin (`pipe2`). With that fixed, 8
`commands::service::worktree::tests` still fail — git path resolution, most likely the
`/tmp` → `/private/tmp` symlink compared against a `TempDir` spelling. Expect more of
these; some are worth sending.

## The way back

Target: `casa` becomes its own crate depending on `worksgood` pinned by `rev`, and
`workgraph/` becomes a plain upstream checkout with zero local edits. Sync then costs one
rev bump plus whatever genuinely broke.

Full phased plan, including the Telegram extraction that removes over half the sync cost:
see the approved plan for this work.

**Pin by `rev`, never a branch.** Bump monthly while the gap is small — the cost of a bump
grows superlinearly with the gap, which is precisely how this fork got here.
