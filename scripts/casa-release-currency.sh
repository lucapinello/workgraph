#!/usr/bin/env bash
# Is the PUBLISHED `casa-prebuilt` release still equivalent to this branch's tip?
#
# WHY THIS EXISTS (audit finding N1). The release went stale twice, and the second time it went
# stale SEVEN HOURS after being fixed: a correct publish landed, then a commit touching
# `wg feedback ask --send` landed after it and nothing re-published. A family's ask-how-dinner
# cron then ran that flag against a binary that did not have it and exited 2. Nothing noticed,
# because nothing could — the only check was a human probing a downloaded binary by hand.
#
# The gateway repo already answers this STRICTLY on every local run
# (weekly_planner/tests/release-is-current.mjs: any published target not built from the release
# tip is a hard red). That gate is deliberately left as it is. This is the CI half, and it is
# deliberately SOFTER in one specific way, for one measured reason:
#
#   On 2026-10-07 the release was behind by `fb732414`, which touched `scripts/release-wg.sh`
#   and nothing else. The published bytes were functionally identical to the branch tip. A job
#   that hard-failed on any SHA mismatch would have reddened the engine's CI over a commit that
#   could not change the binary — and a gate that cries on a no-op is one people learn to
#   ignore. So: fail only when the drift could actually change what a family installs.
#
# WHAT COUNTS AS "could change the binary". Not a hand-kept list. `src/**`, the cargo manifests
# and the toolchain pin, PLUS every file the binary EMBEDS, derived at run time from the
# `include_str!` / `include_bytes!` call sites in HEAD's own source. That derivation matters:
# a change to `templates/executors/claude.toml.example` or `.claude/skills/wg/SKILL.md` is
# compiled INTO `wg`, so it changes what `wg init` hands a family, while living nowhere near
# `src/`. A hardcoded `src/|Cargo.toml|Cargo.lock` pattern calls that drift harmless. It is not.
#
# The derivation is intentionally OVER-inclusive: it does not try to work out which embeds sit
# behind `#[cfg(test)]` and so never ship. Deciding that by grep would be guessing, and guessing
# wrong in the direction that lets a stale binary through is the exact failure N1 is about. A
# false "republish needed" costs one build; a missed stale release costs a family's dinner cron.
#
# EXIT CONTRACT
#   0  CURRENT, or behind only by changes that cannot change the binary (loud note either way)
#   1  STALE: behind by a change that WOULD change the binary — republish
#   3  CANNOT DETERMINE — fail closed, and say what would make it answerable
#
# Every "cannot determine" is returned as status 3 and checked by its caller. It is NOT raised by
# `exit` inside a function, because these functions run inside command substitutions: an `exit`
# there leaves only the subshell, and `set -e` is suspended inside an `if` condition, so the
# failure would be swallowed and the delta would classify as HARMLESS. That is a fail-OPEN bug in
# a guard whose whole job is to fail closed, and it was in this file's first draft.
#
# `--selftest` runs the classifier against pinned historical commit pairs using git alone, no
# network. CI runs the selftest BEFORE the real check, so the teeth are proven where they are used.
set -euo pipefail

# bash 3.2 (macOS /bin/bash) is a supported target: no associative arrays, no ${arr[-1]},
# no mapfile, no ${var,,}.

ME=$(basename "$0")
REPO=${CASA_RELEASE_REPO:-lucapinello/workgraph}
TAG=${CASA_RELEASE_TAG:-casa-prebuilt}
RELEASE_BRANCH=${CASA_RELEASE_BRANCH:-casa-integration-synced}

ROOT=""
PUBLISHED=""
HEAD_REF=""
SELFTEST=0

usage() {
  cat <<USAGE
$ME — is the published casa-prebuilt release still equivalent to this branch's tip?

  --root <dir>        checkout to inspect (default: this script's repo)
  --published <sha>   classify against this commit instead of reading the release (offline)
  --head <sha>        the tip to compare (default: HEAD of --root)
  --repo <owner/name> release repo (default: $REPO)
  --tag <tag>         release tag (default: $TAG)
  --selftest          run the pinned offline classification cases and exit

exit: 0 current or harmless drift · 1 stale, republish · 3 cannot determine
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --root)      ROOT=${2:?--root needs a path}; shift 2 ;;
    --published) PUBLISHED=${2:?--published needs a commit}; shift 2 ;;
    --head)      HEAD_REF=${2:?--head needs a commit}; shift 2 ;;
    --repo)      REPO=${2:?--repo needs owner/name}; shift 2 ;;
    --tag)       TAG=${2:?--tag needs a tag}; shift 2 ;;
    --selftest)  SELFTEST=1; shift ;;
    -h|--help)   usage; exit 0 ;;
    *) echo "$ME: unknown argument '$1'" >&2; usage >&2; exit 3 ;;
  esac
done

say() { printf '%s\n' "$*"; }
# Printed by whichever layer decided it could not answer; the caller turns it into exit 3.
cannot() { printf '❌ %s: CANNOT DETERMINE — %s\n' "$ME" "$1" >&2; shift; for l in "$@"; do printf '   %s\n' "$l" >&2; done; return 3; }

if [ -z "$ROOT" ]; then ROOT=$(cd "$(dirname "$0")/.." && pwd); fi
# Every path below derives from $ROOT, so a test can point this at a scratch checkout and know
# nothing reached past it.
git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1 || {
  cannot "'$ROOT' is not a git checkout — nothing can be compared." "Repair: pass --root <checkout>." || exit 3; }

# ── the embedded-asset derivation ────────────────────────────────────────────
# Collapse `.` and `..` textually; the path need not exist (a deleted embed still has a name).
normalize_path() {
  printf '%s\n' "$1/$2" | awk -F/ '{
    n = 0
    for (i = 1; i <= NF; i++) {
      s = $i
      if (s == "" || s == ".") continue
      if (s == "..") { if (n > 0) n--; continue }
      st[++n] = s
    }
    out = ""
    for (i = 1; i <= n; i++) out = (i == 1 ? st[i] : out "/" st[i])
    print out
  }'
}

# Is every embed readable by the line-based derivation? Run at top level so its refusal is a real
# exit, never a swallowed subshell status.
assert_embeds_derivable() {
  local root=$1 invocations resolvable
  invocations=$(cd "$root" && grep -rEo 'include_(str|bytes)!\(' src/ 2>/dev/null | wc -l | tr -d ' ')
  resolvable=$(cd "$root" && grep -rEo 'include_(str|bytes)!\("[^"]*"' src/ 2>/dev/null | wc -l | tr -d ' ')
  if [ "$invocations" != "$resolvable" ]; then
    cannot "cannot derive the binary's embedded assets." \
      "$invocations include_str!/include_bytes! invocation(s) in src/, but only $resolvable carry" \
      "a string literal on the same line. The rest use a computed path or span lines, so this" \
      "script cannot tell whether they are build inputs — and guessing wrong in that direction" \
      "is exactly how a stale release gets through." \
      "Repair: give those call sites same-line literals, or teach this derivation to read them."
    return 3
  fi
  return 0
}

# Paths outside src/ that HEAD's source compiles into the binary, one per line.
embedded_paths() {
  local root=$1 hit file lit dir
  (cd "$root" && grep -rEno 'include_(str|bytes)!\("[^"]*"' src/ 2>/dev/null) | while IFS= read -r hit; do
    file=${hit%%:*}
    lit=$(printf '%s' "$hit" | sed -n 's/.*!("\(.*\)"$/\1/p')
    [ -n "$lit" ] || continue
    dir=$(dirname "$file")
    normalize_path "$dir" "$lit"
  done | grep -v '^src/' | sort -u || true
}

assert_embeds_derivable "$ROOT" || exit 3
EMBEDS=$(embedded_paths "$ROOT")
EMBED_LIST=$(mktemp "${TMPDIR:-/tmp}/casa-relcur-embeds.XXXXXX")
# Blank lines are stripped deliberately. `grep -Fxf` treats an empty pattern line as matching
# EVERY input line, so an empty embed list would quietly classify all drift as engine-affecting
# — the gate would be stuck on STALE and nobody would know why. Found by a mutation control.
printf '%s\n' "$EMBEDS" | sed '/^[[:space:]]*$/d' > "$EMBED_LIST"
trap 'rm -f "$EMBED_LIST"; [ -n "${DL_DIR:-}" ] && rm -rf "$DL_DIR"' EXIT

# Fixed build inputs. build.rs does not exist today; it is listed so that adding one is covered
# on the day it lands rather than the day someone remembers this file.
BUILD_INPUTS='^src/|^Cargo\.toml$|^Cargo\.lock$|^rust-toolchain\.toml$|^build\.rs$'

# Echo the changed paths that could change the binary; status 0 if any, 1 if none.
engine_affecting() {
  local files=$1
  {
    printf '%s\n' "$files" | grep -E "$BUILD_INPUTS" || true
    if [ -s "$EMBED_LIST" ]; then printf '%s\n' "$files" | grep -Fxf "$EMBED_LIST" || true; fi
  } | sort -u | grep . || return 1
}

# ── the classifier ───────────────────────────────────────────────────────────
# `git diff A B` compares the two TREES, not the history between them. That is the right
# question: "would rebuilding at B produce a different binary than the one published at A?"
# A rebase, a force-push or a diverged line all still get a truthful answer, where a
# merge-base-relative `A...B` would quietly compare against something else.
# Prints a verdict word on line 1 and the paths behind it after; status 0, or 3 = cannot answer.
classify() {
  local from=$1 to=$2 files hits rev
  for rev in "$from" "$to"; do
    git -C "$ROOT" rev-parse --verify --quiet "${rev}^{commit}" >/dev/null 2>&1 || {
      cannot "commit ${rev} is not in this checkout." \
        "A shallow clone cannot answer a currency question — the published commit is absent." \
        "Repair: actions/checkout with 'fetch-depth: 0', or git fetch --unshallow."
      return 3; }
  done
  from=$(git -C "$ROOT" rev-parse "${from}^{commit}")
  to=$(git -C "$ROOT" rev-parse "${to}^{commit}")
  if [ "$from" = "$to" ]; then printf 'CURRENT\n'; return 0; fi
  files=$(git -C "$ROOT" diff --name-only "$from" "$to" 2>/dev/null) || {
    cannot "git could not diff ${from:0:8} against ${to:0:8}."; return 3; }
  # Different commits, identical trees: a rebase or an empty commit. Nothing to rebuild.
  if [ -z "$files" ]; then printf 'EQUIVALENT\n'; return 0; fi
  if hits=$(engine_affecting "$files"); then printf 'STALE\n%s\n' "$hits"; return 0; fi
  printf 'HARMLESS\n%s\n' "$files"; return 0
}

# ── selftest ─────────────────────────────────────────────────────────────────
# Pinned commit pairs, each a state this repo was actually in. Offline: git only.
selftest() {
  local failures=0 pass=0
  check() { # name expected from to
    local name=$1 want=$2 from=$3 to=$4 got rc
    if ! git -C "$ROOT" rev-parse --verify --quiet "${from}^{commit}" >/dev/null 2>&1 \
      || ! git -C "$ROOT" rev-parse --verify --quiet "${to}^{commit}" >/dev/null 2>&1; then
      say "  ✗ $name — pinned commits absent from this checkout"
      say "      a selftest that cannot run is not a pass; fetch full history"
      failures=$((failures + 1)); return
    fi
    set +e; got=$(classify "$from" "$to"); rc=$?; set -e
    got=$(printf '%s' "$got" | head -1)
    if [ "$rc" -ne 0 ]; then
      say "  ✗ $name — classifier could not answer (status $rc)"; failures=$((failures + 1)); return
    fi
    if [ "$got" = "$want" ]; then say "  ✓ $name — $got"; pass=$((pass + 1))
    else say "  ✗ $name — expected $want, got ${got:-<nothing>}"; failures=$((failures + 1)); fi
  }

  say "$ME --selftest (offline; git only)"
  # The 2026-10-07 state that motivated the SOFT rule: scripts/release-wg.sh alone.
  check "script-only drift is harmless"      HARMLESS  18b19d8e fb732414
  # The 2026-10-07 state that motivated the JOB: src/commands/{feedback,telegram}.rs.
  check "a src/ change is stale"             STALE     18b19d8e 10a939ad
  # So that CURRENT cannot be the answer to everything.
  check "a commit against itself is current" CURRENT   10a939ad 10a939ad

  # The derivation is the ONLY thing covering out-of-src build inputs. If it silently returned
  # nothing, every embedded-asset change would classify HARMLESS and the two checks above would
  # still pass — so it gets its own assertion. templates/executors/*.toml.example is compiled
  # into `wg` by src/commands/init.rs.
  if [ "$(printf '%s\n' "$EMBEDS" | grep -c '^templates/executors/' || true)" -eq 0 ]; then
    say "  ✗ embedded-asset derivation — found no templates/executors/* embeds"
    failures=$((failures + 1))
  else
    say "  ✓ embedded-asset derivation — finds the shipped templates/executors/* embeds"
    pass=$((pass + 1))
  fi
  # And that such a path actually drives the verdict, not just appears in a list.
  if engine_affecting "templates/executors/claude.toml.example" >/dev/null; then
    say "  ✓ an embedded asset alone is engine-affecting"
    pass=$((pass + 1))
  else
    say "  ✗ an embedded asset alone was classified harmless"
    failures=$((failures + 1))
  fi
  # Negative control: a path that is neither a build input nor an embed must NOT count, or
  # 'STALE' is the answer to everything and the soft rule is dead.
  if engine_affecting "docs/05-deployment-guide.md" >/dev/null; then
    say "  ✗ a docs path was classified engine-affecting"
    failures=$((failures + 1))
  else
    say "  ✓ a docs path is not engine-affecting"
    pass=$((pass + 1))
  fi

  say ""
  if [ "$failures" -ne 0 ]; then say "❌ selftest: $pass passed, $failures FAILED"; return 1; fi
  say "✅ selftest: $pass passed"
  return 0
}

if [ "$SELFTEST" = 1 ]; then
  selftest || exit 1
  if [ -z "$PUBLISHED" ] && [ -z "$HEAD_REF" ]; then exit 0; fi
  say ""
fi

# ── the real check ───────────────────────────────────────────────────────────
[ -n "$HEAD_REF" ] || HEAD_REF=$(git -C "$ROOT" rev-parse HEAD)

RESULT=""
DETAIL=""

if [ -z "$PUBLISHED" ]; then
  command -v gh >/dev/null 2>&1 || { cannot \
    "gh is not on PATH, so the published release cannot be read." \
    "Repair: install gh, or pass --published <sha> to classify a known pair offline." || exit 3; }
  DL_DIR=$(mktemp -d "${TMPDIR:-/tmp}/casa-relcur.XXXXXX")
  gh release download "$TAG" -R "$REPO" -p RELEASE-INFO -D "$DL_DIR" >/dev/null 2>&1 || { cannot \
    "release '$TAG' in $REPO carries no RELEASE-INFO asset." \
    "Without provenance, 'is this current?' is not an answerable question — which is the state" \
    "that let the release go stale unnoticed in the first place." \
    "Repair: publish once with scripts/release-wg.sh (it emits RELEASE-INFO)." || exit 3; }
  rows=$(grep -c . "$DL_DIR/RELEASE-INFO" 2>/dev/null || true)
  [ "${rows:-0}" -gt 0 ] || { cannot \
    "RELEASE-INFO on '$TAG' is present but empty — nothing to verify." || exit 3; }

  say "published provenance on '$TAG' — $rows target(s), compared against ${HEAD_REF:0:8}:"
  RESULT=CURRENT
  while IFS= read -r row; do
    [ -n "$row" ] || continue
    target=$(printf '%s' "$row" | awk '{print $1}')
    commit=$(printf '%s' "$row" | awk '{print $2}')
    set +e; verdict=$(classify "$commit" "$HEAD_REF"); rc=$?; set -e
    [ "$rc" -eq 0 ] || exit 3
    head_line=$(printf '%s\n' "$verdict" | head -1)
    rest=$(printf '%s\n' "$verdict" | tail -n +2 | tr '\n' ' ')
    say "  · $target @ ${commit:0:8} → $head_line"
    case "$head_line" in
      STALE)    RESULT=STALE
                DETAIL="$DETAIL
$target @ ${commit:0:8} differs in: $rest" ;;
      HARMLESS) [ "$RESULT" = STALE ] || RESULT=HARMLESS
                DETAIL="$DETAIL
$target @ ${commit:0:8} differs only in: $rest" ;;
    esac
  done < "$DL_DIR/RELEASE-INFO"
else
  set +e; verdict=$(classify "$PUBLISHED" "$HEAD_REF"); rc=$?; set -e
  [ "$rc" -eq 0 ] || exit 3
  RESULT=$(printf '%s\n' "$verdict" | head -1)
  DETAIL=$(printf '%s\n' "$verdict" | tail -n +2)
fi

say ""
case "$RESULT" in
  CURRENT|EQUIVALENT)
    say "✅ release-currency: the published release builds the same binary as ${HEAD_REF:0:8}."
    exit 0 ;;
  HARMLESS)
    say "✅ release-currency: the published release is BEHIND, but only by changes that cannot"
    say "   change the binary a family installs. No republish needed."
    printf '%s\n' "$DETAIL" | sed '/^[[:space:]]*$/d; s/^/   /'
    say "   (If you expected an engine change in this delta, it is not here — check you pushed it.)"
    exit 0 ;;
  STALE)
    say "❌ release-currency: the published release is STALE — it is behind by a change that"
    say "   WOULD change the binary. A family installing now gets the old behaviour."
    printf '%s\n' "$DETAIL" | sed '/^[[:space:]]*$/d; s/^/   /'
    say ""
    say "   Repair (both halves, or the release stays half-old):"
    say "     macOS:  scripts/release-wg.sh --target aarch64-apple-darwin --publish"
    say "     Linux:  gh workflow run casa-prebuilt.yml -R $REPO \\"
    say "               --ref $RELEASE_BRANCH -f release_tag=$TAG -f publish=true"
    exit 1 ;;
  *)
    cannot "the classifier returned '$RESULT', which is not a verdict it defines." || exit 3 ;;
esac
