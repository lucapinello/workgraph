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

### The one macOS gap we carry as an ignore (2026-08-14)

`tui::pty_pane::tests::pty_pane_unblocks_codex_style_query_burst_end_to_end` is
`#[cfg_attr(target_os = "macos", ignore = …)]` in our fork. It is upstream's test, and our
DA1 logic is byte-identical to theirs, so this is a platform gap and not a divergence:

- `compute_query_replies` (the pure half) passes.
- `[ -t 0 ]` inside the script confirms stdin IS the pty slave.
- The child's `read` still gets EOF rather than the replies, so the gap is delivery from the
  master writer to the slave on Darwin.

Two things were fixed along the way and are worth keeping straight from the gap itself: the
script used `read -N`, which macOS's bash 3.2.57 rejects as a usage error (so it reported an
empty response set within milliseconds and looked like "the emulator answered nothing"), and
three of our own test SKIP notices used the stderr macro, which tripped upstream's
`tui_runtime_never_writes_process_stderr` guard — that one was OUR bug and is fixed in our
code, not by widening their guard.

Left as a red it taught a whole session to read "2 failed" as normal, which is how a real
regression walks in beside a known one. On Linux — upstream's CI — the test still runs. Running
it explicitly with `--ignored` still reproduces the same `expected DA1 reply` failure, so
nothing was neutered.

Engine suite on macOS is now lib 3926 / 0 failed and bin 4049 / 0 failed / 2 ignored.

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
| 6 | `casa/feed_write.rs`; `run_discuss` → `casa/dryruns.rs` | 12,555 → 12,210 | 5 → 5 |
| 7a | `casa/group.rs` — collective + discussion rounds | 12,210 → 11,903 | 5 → 5 |
| 8 | `casa/plan_edits.rs` + 3 more one-shots | 11,903 → 11,507 | 5 → 5 |
| 9 | `casa/lifecycle.rs`, `casa/command_gate.rs`, + 2 extended | 11,507 → 10,500 | 5 → 5 |

Slice 3 went the wrong way on the marker count, deliberately and once:
`resolve_dm_target` is ours (absent upstream at the fork point and on `gwwg/main`
today) but still has one caller and two tests in their file, so it is exposed rather
than dragged out with its tests. It follows the DM path out when that is extracted.
The marker is the recorded price of a 537-line reduction, not an oversight.

### Assert your provenance check actually READ something

A provenance check that reads an empty stream reports "absent", which is indistinguishable
from "not present in upstream" — and "not present upstream" is the verdict that decides whether
an item may move. This bit once, on 2026-08-14: a one-off shell check used `$MB` set in an
EARLIER tool call, shell variables do not persist between calls, so it ran
`git show ":src/commands/telegram.rs"`, got nothing, and pronounced `try_confirm_binding` OURS.
It is upstream's. The Python scoping pass, which holds the fork text in a variable it just
filled, said THEIRS and was right.

So: read the fork-point file once, assert it is non-empty (`len(fork) > 10_000` for this file),
and derive every verdict from that one buffer. Re-checked all eighteen items moved in slices
7a–9 that way — every one is genuinely ours, so the slices stand — and `try_confirm_binding`
is the single upstream item in that neighbourhood.

Practical consequence for the last two candidates: `run_listen` is already `pub fn`, so
importing it costs NOTHING, and `run_decide`'s only real cost is exposing upstream's own
private `try_confirm_binding`. That is a marker on THEIR function for our convenience, which is
the one kind of exposure this split should refuse. `run_decide` stays.

### The slice test, corrected (use this one)

The 7b failure produced a scoping method that actually works. Seed a candidate, then close over
**all item kinds** (`fn`, `struct`, `enum`, `const`, `static`, `type`, `trait`) using
**word-boundary references** rather than call sites, and pull in every reachable item that is
OURS. What remains is the honest cost: items that are genuinely upstream's.

Run against every remaining `pub fn run_*` of ours, that gives:

| seed | lines | items | test refs | needs from upstream |
|---|---|---|---|---|
| `run_register_commands` | 73 | 1 | 0 | nothing |
| `run_route` | 81 | 1 | 0 | nothing |
| `run_compose_prompt` | 93 | 3 | 5 | nothing |
| `run_command` | 106 | 2 | 0 | nothing |
| `run_shopping_language` | 145 | 2 | 0 | nothing |
| `run_week_start` | 281 | 5 | 22 | nothing |
| `run_lifecycle` | 703 | 19 | 26 | nothing |
| `run_decide` | 313 | 7 | 28 | `run_listen`, `try_confirm_binding` |
| `run_web_inbound` | 1,355 | 25 | 127 | `run_listen` |

Slice 8 took the four zero-test-ref rows; slice 9 took the remaining three clean ones
(`run_lifecycle`, `run_week_start`, `run_compose_prompt` — 1,091 lines, 26 items, still zero
exposures). **That exhausts the clean list.** `run_decide` and `run_web_inbound` are the only
candidates left, and both genuinely need upstream's `run_listen`; the table is the reason to
stop reaching for them rather than a queue.

Slice 9 also cheapened whatever comes next: `command_gate` moved, so `run_decide` now needs
only `try_confirm_binding` instead of two things.

**One cost worth naming.** Moving a struct out of the file that reads its fields makes those
fields private across the boundary, so `WebFastLaneOutcome` (4 fields) and
`LifecycleDeliverySummary`/`LifecycleRearmJournal` (4 more) needed `pub(crate)` FIELDS. That is
a different and smaller kind of exposure than a `pub(crate) fn` — the function count stayed at
5 — but it is exposure, and a future slice that moves the readers will take it back.

### Slice 7b was attempted and reverted — read this before trying again

The `run_web_inbound` cluster is NOT a clean slice, and the first two scopings of it were both
wrong. Recorded so the third does not repeat them.

**Scoping error 1: only `fn` was scanned.** The moving set came out as 19 functions / 1,548
lines / 0 exposures, the cut was applied, and the compiler produced 77 errors for items that
were never in the set at all — `WebFastLaneOutcome`, `WebFastLaneDispatch`, `WebDefaultOwner`,
`WebDefaultOwnerResolution`, `NeedsContactReason`, `WEB_FAST_LANE_OCCURRENCE_DOMAIN`. Structs,
enums and consts belong to a cluster exactly as much as its functions do. Same class of
omission as slice 5's test-module callers: the analysis answered a narrower question than the
one that mattered.

**Scoping error 2: call sites, not references.** Matching `name(` finds callers of a function
and misses every other mention — a type in a signature, a const in a match arm. Re-run over
all item kinds with word-boundary references, the honest picture is 17 items that upstream's
`run_listen` also touches, `run_listen` itself among the names.

**What that means.** Unlike slices 1–7a, this cluster is genuinely interleaved with upstream's
listener rather than merely called from it. By the slice-7a rule most of those items are ours
and could still move with imports back, but the result is a ~1,500-line move touching the
family's live inbound path, and the value per unit of risk is much lower than any slice so far.
Reverted rather than forced: `git checkout` of `commands/telegram.rs`, `main.rs`, `casa/mod.rs`
plus deleting the two new modules, verified back to 0 dirty files, 0 build errors, lib 3926/0
and bin 4049/0.

**If you take this on:** scope it over ALL item kinds with reference matching, expect the
`WebFastLane*` types to come along, do it on its own branch, and prove it with the human-flow
suite plus a real message through the web pane — not with a build.

Slice 7a overturned an earlier verdict of my own. The first pass at `run_web_inbound` said it
"needs three new `pub(crate)` markers" because `run_group_collective`, `run_group_discussion`
and `web_physical_turn_key` are also called from upstream's `run_listen`. Checking provenance
instead of assuming: all three are OURS — absent upstream at the fork point and on `gwwg/main`
today — and the `run_listen` call sites are our own added lines. So they MOVE and their file
imports them; exposure would have kept our code there and added a marker. Same for
`collective_request_id`, shared with `run_listen` but ours.

The lesson generalises: "shared with a function that stays" is not the same as "must be
exposed". Ask who owns it first. Exposure is only forced when the helper is genuinely
upstream's.

Slice 6 tripped a guard, correctly. `notify/casa_audience.rs` pins the EXACT set of engine
feed-writer files, and moving `run_feed_write` changed it — so the lib suite went
3926/0 → 3925/1 the moment the file moved. That is the guard doing its job; a set instead of
an exact list would have let the move pass silently. Both of its lists were updated, each
site verified against the code (`casa/feed_write.rs:156` and `casa/reply_delivery.rs:256`
record an audience; `commands/telegram.rs:816` and `:1003` are inbound human lines and
exempt) rather than read off the failure diff, and the rewritten list was re-proven to bite
by planting a synthetic append site in an unrelated Casa module.

**What is left, and why the easy slices are done.** `run_listen` (2,363 lines with helpers),
`run_send` and `run_ask` are UPSTREAM's functions — heavily rewritten by us, but they are
theirs and do not move; their Casa content has to be teased out in place, which is a
different and larger job. Of what remains ours: `run_web_inbound` (~1,076) needs the two
group handlers exposed because upstream's `run_listen` also calls them; `run_week_start` and
`run_shopping_language` need `web_physical_turn_key` / `web_fast_lane_now`, which become
internal if they move WITH the `run_web_inbound` cluster — so those three are one slice, not
three; `run_decide` needs `command_gate`, shared with `run_listen`, so it costs a permanent
exposure.

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

**Before cutting, grep for guards that NAME what you are moving.** Two separate guards have
now gone red at 0s purely because a file changed address — `notify/casa_audience.rs` (an exact
list of feed-writer paths) and `tests/smoke/scenarios/feed_lock_section_length.sh` (a corpus
of named source files, widened twice before being made discovery-based). Neither was wrong;
both were doing their job. The check costs one command:

```
grep -rln '<fn or path you are moving>' tests/smoke/scenarios/ src/ | grep -v '<the file itself>'
```

Still live in this shape, and fine only because the symbols they name stay with upstream's
`run_listen`: `engine_capability_no_invented_work.sh` (greps `try_capability_answer` in
`commands/telegram.rs`) and `reminder_classifier_fails_closed.sh` (greps `try_cancel_reminder`
and `try_register_reminder` there). If a future slice moves any of those three, expect a 0s
red from those two scenarios, and prefer widening them to discovery over renaming a path.

**Choosing a slice.** Prefer a cluster whose helpers are used ONLY by it — then nothing
has to be exposed. Check provenance first (`git show <merge-base>:<path>`): a helper
that exists upstream must stay, and one that does not is ours to move. `run_remind`
qualified on both counts; `run_web_inbound` (512 lines) drags ten private helpers and
two `pub` group handlers, so it wants its own slice.

**Pin by `rev`, never a branch.** Bump monthly while the gap is small — the cost of a bump
grows superlinearly with the gap, which is precisely how this fork got here.

---

## Phase 1 — the triage table (2026-08-15)

**Method.** Merge base with `gwwg/main` is `29740403` (2026-07-20). Since then: 342 commits ours,
459 theirs. Files WE touched under `src/`: 122. Files THEY touched: 351. The intersection — the
only files where a rev bump can actually collide — is **48**.

**The number that reframes the problem.** Our `src/` churn is 95,363 lines, and only **11,033 of
them (11%) sit in those 48 files**. The other 84,330 lines live in files upstream has never
touched, so they merge without a conflict at all. The fork is not "100k lines of divergence to
reconcile"; it is 11k lines across 48 files, and the top ten of those hold most of it. `src/casa/`
(5,722 lines) is already immune by construction — upstream has no such path.

**Buckets.** `A` = an upstream bug fix (a plain `wg` user would want it, no Casa involved) → PR it,
and the file leaves our diff for good. `B` = Casa behaviour → moves into the Casa crate at Phase 3.
`C` = wiring or noise (CLI declarations, dispatch arms, `cargo fmt`, merge commits) → disappears
when Casa declares its own CLI, or is a trivial re-apply.

**This is a first pass from commit subjects and file-level reading, not a per-hunk audit.** Every
row marked `A+B` is genuinely mixed and needs a hunk-level split before it can be sent or moved.
Rows are ordered by our churn, because that is the sync cost.

| lines | file | bucket | why |
|---:|---|:---:|---|
| 1397 | `commands/service/coordinator.rs` | A+B | A: spawn circuit breaker, transport-exhausted quarantine, verdict starvation, dispatcher resilience. B: family-first lane (reserve headroom, family turns first), R18 button→task routing |
| 1150 | `commands/service/mod.rs` | A+B | A: breaker + operator alert. B: the human-task dispatch tail (Casa's human asks) |
| 1003 | `cli.rs` | C | Casa subcommand declarations; 565 of 1003 added lines are comments. Gone when Casa owns its CLI |
| 871 | `eval_lifecycle.rs` | A | PendingEval unwedge, verdict-link transitions, coordinator perf — general lifecycle, no Casa concept |
| 794 | `cron.rs` | A+B | A: cron-fanout orphaning, a distinct instance per firing, protected crons surviving a cleanup sweep. B: origin stamping / report-back |
| 678 | `main.rs` | C | dispatch arms for Casa subcommands |
| 596 | `disk_sentinel.rs` | A | double cache walk per interval, a guard reporting an untrue reason, macOS inertness |
| 485 | `commands/service/human_dispatch.rs` | B | Casa's human-ask dispatch + button routing |
| 479 | `service/provider_health.rs` | A | `classify_error` done-spoof suppressing real provider errors |
| 390 | `session_lock.rs` | A | lock-test flakiness + dispatcher resilience |
| 383 | `commands/done.rs` | A | disposable artifact/log enforcement and result ingestion — engine guardrails (docs/14) |
| 357 | `commands/func_apply.rs` | A+C | fixes plus R18 wiring |
| 308 | `config.rs` | A+B | A: breaker knobs. B: Casa-only settings |
| 291 | `graph.rs` | A | `FailureClass` (incl. today's 400 split), disposable contract — engine vocabulary |
| 208 | `commands/service/ipc.rs` | C | R18 routing surface |
| 204 | `service/mod.rs` | A | inherited test failures |
| 154 | `commands/setup.rs` | C | merge noise |
| 151 | `tui/viz_viewer/state.rs` | A | dev TUI test fixes |
| 149 | `commands/abandon.rs` | A | general |
| 142 | `commands/spawn/execution.rs` | B | the heartbeat guard is OURS, not theirs — see the correction below |
| 128 | `commands/publish.rs` | A+B | R18 routing (B) + an inherited test failure (A) |
| 123 | `profile/named.rs` | A | tests |
| 89 + 50 | `commands/service/worktree.rs`, `commands/spawn/worktree.rs` | A | worktree lifecycle / macOS path resolution |
| 80 + 33 | `commands/spawn/raw_stream_classifier.rs`, `commands/service/triage.rs` | A | the 400 classification split (`1d556fca`) — **verified live upstream** |
| 48 | `service/llm.rs` | C | merge |
| 41 | `commands/chat_cmd.rs` | A+B | mixed |
| 35 + 34 | `tui/pty_pane.rs`, `commands/spawn/mod.rs` | A | inherited test failures |
| ≤28 each | `function.rs`, `commands/add.rs`, `claude_handler.rs`, `commands/mod.rs`, `coordinator_agent.rs`, `query.rs`, `commands/show.rs`, `func_cmd.rs`, `func_extract.rs`, `func_bootstrap.rs`, `commands/edit.rs`, `service/executor.rs`, `commands/notify.rs`, `evolve/deferred.rs`, `critical_path.rs`, `func_make_adaptive.rs`, `plan_validator.rs`, `lib.rs` | C mostly | 18 files, 175 lines between them: CLI/dispatch wiring with a few one-line fixes. Cheap either way |

### The PR queue, with verification status

The plan's rule — "would wg users want this with no Casa?" — is necessary but not sufficient. The
second question is whether upstream is still broken, and that has to be checked against
`gwwg/main`, not assumed from our commit message.

Swept 2026-08-15. Four candidates checked against `gwwg/main` at `29459696`; **two survive, two do
not** — and neither of the two that died was distinguishable from a real candidate by its commit
subject.

| candidate | status |
|---|---|
| macOS `pipe2` build fix | **PR #62** — open, MERGEABLE, zero reviews since 2026-08-13. Also a hard prerequisite: `cargo test` on their `main` does not compile on macOS without it, so no macOS contributor can verify anything upstream today |
| 400-classification split (`1d556fca`) | **PR #63, sent 2026-08-15.** Verified live: `Hard if http_status == Some(400) => ApiError400Document` with no document check. Their shape needed a different patch than ours — see the note below |
| one unverifiable verdict starves the graph (`2db5230c`) | **VERIFIED LIVE — ready to send.** `load_durable_verdicts` (their line 917) still aborts the entire store on the first bad file: `?` on load, `bail!` twice, `?` on `verify_evaluation_digest`. One unverifiable verdict file makes every verdict unreadable. Minimal upstream form is skip-and-warn inside the loop, not our richer store split |
| zombie session lock, from `691965eb` | **VERIFIED LIVE — ready to send.** They handle a RECYCLED pid (`holder.alive && pid_reused_by_foreign` → recover) but a genuinely live handler from a previous daemon generation hits `Some(holder) if holder.alive => Err("session lock held by live handler")` and every later coordinator exits as a cooperative handoff, forever. Since `wg service stop` leaves handlers running BY THEIR OWN DESIGN, this is reachable upstream exactly as it was for us on 2026-07-19. Their comment even anticipates the shape while fixing only the recycled case |
| transport-exhausted → per-task quarantine (`bfed378c`) | **NOT APPLICABLE.** No transport-exhausted concept exists upstream at all (`git grep` finds nothing). Ours is bucket B, not a fix to send |
| self-healing spawn circuit breaker (`9b823397`) | **NOT A FIX — a PARALLEL IMPLEMENTATION.** Upstream has its own per-task spawn breaker (`spawn_breaker_tripped_tasks` in their coordinator, with `test_record_dispatch_clears_breaker_on_success` and `test_spawn_circuit_breaker_reset_on_edit`), plus a provider breaker in `triage.rs`. They have no `spawn_breaker.rs`; we built the same idea in a file they lack. This is a Phase 3 **adopt-theirs** candidate: dropping ours in favour of theirs would delete ~900 of our lines and remove a guaranteed conflict |
| cron-fanout orphaning / poison-task stall (`691965eb`) | still unverified — the commit is a bundle, and only its session-lock half has been checked |

### What the sweep says about method

Two of four candidates evaporated, and in both cases the commit subject read exactly like a
portable bug fix. `bfed378c` describes a quarantine for a failure mode upstream has never modelled;
`9b823397` describes a breaker they already have. Add `ee9c45d1` from the first pass and that is
**three of five** named candidates that do not survive contact with their tree.

The `1d556fca` PR makes the same point from the other side. Our fix added enum variants and a
policy mapping; their tree needed neither, because their parser already carries the vocabulary and
the right patch was to make one match arm ask for evidence. Porting our diff would have been
wrong even though the bug was real.

So: verify against their code before writing anything, and expect the patch to be a different
shape than ours.

### Correction to the plan: `ee9c45d1` is not a bucket-A candidate

The plan lists "heartbeat guard uses fd 9, not a bash-4 named fd" as a known upstream fix to send.
It is not. Upstream does not have the feature: `gwwg/main`'s `commands/spawn/execution.rs` carries
tests asserting its ABSENCE —

```rust
!script.contains("heartbeat-watch") && !script.contains("HEARTBEAT_GUARD_FD")
```

— so there is nothing there to fix, and the fix belongs to a Casa-carried feature (bucket B). One
of the five named candidates evaporates on contact with their tree, which is the whole reason this
table records verification status per row instead of trusting a subject line.

### What this changes about the plan

- **Phase 3 gets a priority order.** The top ten of these 48 files hold most of the 11k conflict
  surface; the 18-file tail holds 175 lines. Extracting or upstreaming the top ten is where the
  sync cost actually falls.
- **Phase 2 is smaller than advertised and needs verification per patch**, not per commit subject.
- **A monthly rev bump is already viable** for the 89% of our churn upstream never touches. The
  thing that makes a bump expensive is those ten files, not the fork's total size.
