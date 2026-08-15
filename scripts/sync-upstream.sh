#!/usr/bin/env bash
# Measure — and only measure — what merging current upstream would cost us.
#
# WHY THIS EXISTS. The fork's sync cost was believed to be "100k lines, unmergeable in practice".
# Phase 1 measured it instead: 11% of our churn sits in files upstream also touched, and the other
# 89% cannot conflict at all. A number that decides a strategy should be re-derivable on demand
# rather than re-argued, so this script re-derives it, and does the trial merge for real.
#
# SAFETY. It never touches your working tree, your branch, or your index. Everything happens in a
# throwaway worktree under a temp dir, which is removed on exit even if a step fails. It makes no
# commits and pushes nothing. Read-only from the caller's point of view.
#
# USAGE
#   scripts/sync-upstream.sh              # measure + trial merge + report
#   scripts/sync-upstream.sh --with-tests # also build and run the suites if the merge is clean
#
# EXIT CODES
#   0  the trial merge is clean (with --with-tests: and the suites pass)
#   1  the trial merge conflicts — the report names every file
#   2  a precondition failed (no remote, dirty tree in a way that blocks worktree creation)
set -uo pipefail

REMOTE="${UPSTREAM_REMOTE:-gwwg}"
BRANCH="${UPSTREAM_BRANCH:-main}"
WITH_TESTS=0
[ "${1:-}" = "--with-tests" ] && WITH_TESTS=1

say() { printf '%s\n' "$*"; }
die() { printf 'sync-upstream: %s\n' "$*" >&2; exit 2; }

git rev-parse --git-dir >/dev/null 2>&1 || die "not a git checkout"
git remote get-url "$REMOTE" >/dev/null 2>&1 || die "no '$REMOTE' remote (set UPSTREAM_REMOTE)"

HERE_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
say "fetching ${REMOTE}/${BRANCH}…"
git fetch --quiet "$REMOTE" "$BRANCH" || die "fetch failed"

BASE="$(git merge-base HEAD "$REMOTE/$BRANCH")" || die "no common ancestor with $REMOTE/$BRANCH"
OURS_N="$(git rev-list --count "$BASE"..HEAD)"
THEIRS_N="$(git rev-list --count "$BASE".."$REMOTE/$BRANCH")"

say ""
say "  base            $(git log -1 --format='%h %ad' --date=short "$BASE")"
say "  ours            $HERE_BRANCH, $OURS_N commits since"
say "  theirs          $REMOTE/$BRANCH, $THEIRS_N commits since"

# ── the conflict surface: files BOTH sides touched ────────────────────────────
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"; [ -n "${wt:-}" ] && git worktree remove --force "$wt" >/dev/null 2>&1' EXIT
git diff --name-only "$BASE"..HEAD -- src/ > "$tmp/ours"
git diff --name-only "$BASE".."$REMOTE/$BRANCH" -- src/ > "$tmp/theirs"
comm -12 <(sort "$tmp/ours") <(sort "$tmp/theirs") > "$tmp/both"

churn_of() { while read -r f; do git diff --numstat "$BASE"..HEAD -- "$f"; done < "$1" | awk '{s+=$1+$2} END{print s+0}'; }
ALL_CHURN="$(churn_of "$tmp/ours")"
BOTH_CHURN="$(churn_of "$tmp/both")"
PCT=0
[ "$ALL_CHURN" -gt 0 ] && PCT=$(( BOTH_CHURN * 100 / ALL_CHURN ))

say ""
say "  our src churn   $ALL_CHURN lines across $(wc -l < "$tmp/ours" | tr -d ' ') files"
say "  conflict surface $BOTH_CHURN lines across $(wc -l < "$tmp/both" | tr -d ' ') files (${PCT}%)"
say "  cannot conflict  $(( ALL_CHURN - BOTH_CHURN )) lines"
say ""
say "  the ten files that hold the cost:"
while read -r f; do printf '%s %s\n' "$(git diff --numstat "$BASE"..HEAD -- "$f" | awk '{print $1+$2}')" "$f"; done < "$tmp/both" \
  | sort -rn | head -10 | awk '{printf "    %6s  %s\n", $1, $2}'

# ── the trial merge, in a throwaway worktree ──────────────────────────────────
wt="$tmp/trial"
say ""
say "trial merge in a throwaway worktree (your tree is untouched)…"
git worktree add --quiet --detach "$wt" HEAD || die "could not create the trial worktree"
rc=0
if git -C "$wt" merge --no-commit --no-ff "$REMOTE/$BRANCH" >/dev/null 2>&1; then
  say "  MERGE CLEAN — no conflicts"
else
  rc=1
  say "  CONFLICTS:"
  git -C "$wt" diff --name-only --diff-filter=U | sed 's/^/    /'
fi

if [ "$WITH_TESTS" = "1" ] && [ "$rc" = "0" ]; then
  say ""
  say "building and testing the merged tree (this takes a few minutes)…"
  if git -C "$wt" diff --cached --quiet && git -C "$wt" diff --quiet; then
    say "  nothing to test: the merge was a no-op"
  else
    ( cd "$wt" && cargo build --quiet 2>&1 | tail -5 ) || { say "  BUILD FAILED"; rc=1; }
    if [ "$rc" = "0" ]; then
      ( cd "$wt" && cargo test --lib 2>&1 | grep -E 'test result:' | sed 's/^/  lib /' )
      ( cd "$wt" && cargo test --bin wg 2>&1 | grep -E 'test result:' | sed 's/^/  bin /' )
    fi
  fi
fi

say ""
if [ "$rc" = "0" ]; then say "sync-upstream: a bump is mechanically clean today."
else say "sync-upstream: a bump needs hand work — see the conflicts above."; fi
exit "$rc"
