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

## What we actually send upstream

**Default is: send nothing.** This is someone else's project and a fork's backlog is not
their inbox. A change earns a PR only by passing all four:

1. the code is **still broken in `gwwg/main` today** — verified, not assumed;
2. it hurts a **wg user who has never heard of Casa**;
3. it is a **bug fix, not a redesign** — no opinion they have to agree with;
4. it is **small enough to review in one sitting**.

Anything failing (3) is at most an issue with evidence, letting them choose the fix.
Anything failing (1) or (2) stays in our fork and is nobody's problem but ours.

### Passes — send

| what | why it qualifies | status |
|---|---|---|
| `pipe2` breaks the macOS build | won't compile on Darwin at all; no judgment involved | **[PR #62](https://github.com/graphwork/wg/pull/62)** — opened 2026-08-12, 1 file +43/−2 |
| verdict store is accidentally quadratic | O(verdicts × evaluations) in their own code; semantics unchanged, their 30 tests still pass. Their agency machinery is what generates the evaluations, so it bites them harder than us | patch written — **held** until #62 lands, so the first thing they see from us is the smallest possible ask |

### Fails (3) — evidence only, if they want it

| what | why not a PR |
|---|---|
| absent agent record makes caches immortal | real bug (1,180 of 1,283 caches unreapable; `considered=1156 reaped=0` every run) but the fix picks a fail-safe posture they may reasonably disagree with. Their call, not ours to land |
| `classify_error` treats exit 0 as `FatalTask` | their comment says this is deliberate. Disagreeing is an opinion, not a defect report |
| one unverifiable verdict starves the whole graph | a genuine trap, but our fix is a design change (per-file quarantine). Too opinionated to arrive as a patch |

### Checked and dropped

| candidate | why |
|---|---|
| `ee9c45d1` heartbeat bash-4 named fd | **0 such sites upstream** — already gone or never there |
| `9b823397` spawn circuit breaker | a feature, not a fix. Features are noise in someone else's roadmap |
| `691965eb` dispatcher resilience | three unrelated fixes in one commit; would need splitting, and each is arguable |
| `f9f92435` protect production crons | "production cron" is a Casa concept |
| `bfed378c`, `16888d31` | entangled with our persona/satellite model |

We have contributed upstream before (`217dc029` references their PR #53), so the channel
exists — which is a reason to spend it carefully, not freely.

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

### Extraction log — `commands/telegram.rs`

Upstream's file was **869 lines** at our fork point; we deleted 677 of them and added
15,349. Every Casa item living there is a merge conflict waiting for the next sync, so
the slices below move ours OUT to `src/casa/`, a path upstream does not have.

| slice | moved | their file | `pub(crate)` exposed |
|---|---|---|---|
| 1 | `casa/telegram_photo.rs` — photo → shopping pipeline | 15,541 → 14,570 | 9 → 6 |
| 2 | `casa/reply_delivery.rs` — family reply delivery | 14,570 → 14,073 | 6 → 5 |
| 3 | `casa/remind.rs` — the `wg telegram remind` tick | 14,073 → 13,536 | 5 → **6** |
| 4 | `casa/one_shot_answers.rs` — `owner`, `parity`, `capability` | 13,536 → 13,317 | 6 → 6 |
| 5 | `casa/digest.rs`, `casa/dryruns.rs`, `casa/elect.rs` | 13,317 → 12,555 | 6 → **5** |

Slice 3 went the wrong way on the marker count, deliberately and once:
`resolve_dm_target` is ours (absent upstream at the fork point and on `gwwg/main`
today) but still has one caller and two tests in their file, so it is exposed rather
than dragged out with its tests. It follows the DM path out when that is extracted.
The marker is the recorded price of a 537-line reduction, not an oversight.

Slice 5 paid off slice 3's debt. `resolve_dm_target` was exposed there so `casa::remind`
could reach it, on the promise that it would "follow the DM path out when that path is
extracted" — `run_digest` was its last non-test caller in that file, so it moved to
`casa::digest` and the exposure went away. Markers are back to 5, the lowest since the split
began, while their file has lost 2,986 lines across five slices.

Two things slice 5 taught about the slice test itself:

- **Count test-module callers.** The exclusivity check excluded them on purpose, so
  `coordination_owner_hint` and `deliver_digest_fire` looked exclusive while having tests in
  their file. They were repointed to the `casa::` path rather than moved, which is fine — but
  a "no shared helpers" verdict that ignores tests is not the verdict it claims to be.
- **Leave a test where its shared dependency is.** `resolve_dm_target`'s two tests also drive
  `try_register_reminder`, which still has seven callers in their file. Moving them would have
  meant widening that helper's visibility — re-creating the exact debt this slice paid off. So
  they stayed and were repointed at the moved function instead.

Also: counting `pub(crate)` occurrences file-wide counts the string in COMMENTS too. A comment
explaining the marker debt read as a marker and made the metric say 6 when the answer was 5.

Slice 4 is what a clean slice looks like: three commands that drag no private helpers,
share none, carry no tests in that file's test module, and — the compiler's verdict, not
mine — import nothing back from it either. Both helpers I added on the strength of a regex
match were flagged unused. Nothing exposed, nothing repointed, no marker change.

It was picked over the bigger candidate on purpose. `run_web_inbound` is 512 lines but
needs three new `pub(crate)` markers (`web_physical_turn_key`, shared with
`run_week_start`; `run_group_collective` and `run_group_discussion`, shared with
`run_listen`) and ~69 test references moved — and it gets cheaper once `run_listen` goes,
since that takes both group handlers with it. Eight of its helpers ARE exclusive to it, so
the slice is real, just not yet.

Also worth recording: `run_ask` looks like a peer of the three moved here and is NOT a
candidate — it is upstream's. Check provenance before assuming a neighbour is ours.

**Choosing a slice.** Prefer a cluster whose helpers are used ONLY by it — then nothing
has to be exposed. Check provenance first (`git show <merge-base>:<path>`): a helper
that exists upstream must stay, and one that does not is ours to move. `run_remind`
qualified on both counts; `run_web_inbound` (512 lines) drags ten private helpers and
two `pub` group handlers, so it wants its own slice.

**Pin by `rev`, never a branch.** Bump monthly while the gap is small — the cost of a bump
grows superlinearly with the gap, which is precisely how this fork got here.
