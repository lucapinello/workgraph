# TWIN COPY — the source of truth for this file is the GATEWAY repo's scripts/release-wg.sh.
#
# It lives here as well because .github/workflows/casa-prebuilt.yml runs IN THIS REPO and must
# package the engine without reaching into a private repo: fetching it cross-repo needed a
# repository variable plus a read token, which is configuration that sits unset and turns the
# whole workflow into a fail-fast error message.
#
# DRIFT IS GUARDED, not hoped for: claw3d-bridge/test/releaseScriptTwin.test.mjs asserts these
# two files are byte-identical whenever the engine checkout is present, and it runs in the
# gateway's own suite. Edit the gateway copy, then copy it here — never the other way round.
#
# Everything below this header is that file, verbatim.
#!/usr/bin/env bash
#
# release-wg.sh — build, checksum, and publish a PREBUILT `wg` engine binary so a
# new family never has to compile the engine from source.
#
# WHY THIS EXISTS (panel consensus #2 → BLOCKER before any second family):
#   The first run of the family-team stack must not require a Rust toolchain, a
#   `cargo build`, cmake/BoringSSL, or the `--locked` yanked-crate dance. This
#   script is the reproducible producer side: it builds `wg` from the fork branch
#   `integration/casa-pinello`, computes a SHA-256 checksum, and publishes the
#   archive + checksum as a GitHub Release asset on the fork
#   (lucapinello/workgraph). `bin/casa install-wg` is the consumer side: it
#   downloads that asset, verifies the checksum, and installs `wg` — no compiler.
#
# The asset NAMING here is deliberately identical to the fork's CI
# (.github/workflows/release.yml) — `wg-v<version>-<target>.tar.gz` plus a
# `<archive>.sha256` and a combined `SHA256SUMS` — so `casa install-wg` works
# against BOTH a CI-published release and a locally-published one from this
# script, interchangeably.
#
# USAGE:
#   scripts/release-wg.sh                 # build + package the HOST target, no publish (dry run)
#   scripts/release-wg.sh --all           # package ALL first-class targets (see PLATFORMS)
#   scripts/release-wg.sh --all --publish # ... and create/update the GitHub Release on the fork
#   scripts/release-wg.sh --target x86_64-unknown-linux-gnu --publish
#   scripts/release-wg.sh --tag casa-prebuilt --publish
#
# ENV OVERRIDES:
#   WG_SOURCE_DIR   fork checkout to build from   (default: ~/Projects/workgraph)
#   WG_FORK_BRANCH  branch to build               (default: integration/casa-pinello)
#   WG_RELEASE_REPO GitHub repo to publish to      (default: lucapinello/workgraph)
#   WG_RELEASE_TAG  release tag to publish under   (default: casa-prebuilt, a rolling tag)
#   OUT_DIR         where archives are staged       (default: <repo>/dist)
#   WG_PREBUILT_BIN            package THIS binary for the single --target (no build)
#   WG_LINUX_BIN / WG_LINUX_ARM64_BIN / WG_MACOS_BIN  per-target CI artifact for --all
#                             (linux x86_64 / linux arm64 / macOS arm64)
#   WG_PREBUILT_BIN_<TARGET>     generic per-target artifact (target upper-cased, '-'→'_')
#                   e.g. WG_PREBUILT_BIN_X86_64_UNKNOWN_LINUX_GNU=/path/to/wg
#
# PLATFORMS (all FIRST-CLASS — Luca):
#   * macOS arm64   (aarch64-apple-darwin)      — builds NATIVELY on an Apple-silicon Mac.
#   * linux x86_64  (x86_64-unknown-linux-gnu)  — builds NATIVELY on a linux x86_64 box.
#   * linux arm64   (aarch64-unknown-linux-gnu) — builds NATIVELY on a linux arm64 box.
#   `--all` requests ALL THREE. On a given machine one is the host (built natively); the
#   OTHERS are cross-built ONLY if their rustup std target + a linker are installed, else
#   they are SKIPPED with a plain reason (the run still ships the host target). The HONEST
#   cross path: supply the other targets' binaries from the fork CI (`.github/workflows/
#   release.yml`, all five targets) via WG_LINUX_BIN / WG_LINUX_ARM64_BIN / WG_MACOS_BIN,
#   or the generic WG_PREBUILT_BIN_<TARGET>, or run this script natively on that OS.
#   Nothing is silently dropped — skips are named at the end.
#
set -euo pipefail

# ── config ───────────────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WG_SOURCE_DIR="${WG_SOURCE_DIR:-$HOME/Projects/workgraph}"
WG_FORK_BRANCH="${WG_FORK_BRANCH:-integration/casa-pinello}"
WG_RELEASE_REPO="${WG_RELEASE_REPO:-lucapinello/workgraph}"
WG_RELEASE_TAG="${WG_RELEASE_TAG:-casa-prebuilt}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"

# The FIRST-CLASS targets (Luca: all three platforms are first-class). `--all`
# packages them in one release; cross-target binaries the host can't build come
# from CI or a per-target prebuilt env var (see resolve_bin_for_target below).
# Both Linux targets ship so `casa install-wg` serves an x86_64 *and* an arm64
# container/box with no compiler (the Docker default image fetches these at boot).
FIRST_CLASS_TARGETS=(aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu)

PUBLISH=0
TARGETS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --publish)      PUBLISH=1; shift;;
    --all)          TARGETS+=("${FIRST_CLASS_TARGETS[@]}"); shift;;
    --target)       TARGETS+=("${2:?--target needs a value}"); shift 2;;
    --tag)          WG_RELEASE_TAG="${2:?--tag needs a value}"; shift 2;;
    --source-dir)   WG_SOURCE_DIR="${2:?--source-dir needs a value}"; shift 2;;
    -h|--help)      sed -n '2,50p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "release-wg: unknown argument '$1'" >&2; exit 2;;
  esac
done

c_grn=$'\033[32m'; c_red=$'\033[31m'; c_ylw=$'\033[33m'; c_rst=$'\033[0m'; c_bold=$'\033[1m'
say()  { printf '%s[release-wg]%s %s\n' "$c_bold" "$c_rst" "$*"; }
ok()   { printf '  %s✓%s %s\n' "$c_grn" "$c_rst" "$*"; }
warn() { printf '  %s!%s %s\n' "$c_ylw" "$c_rst" "$*"; }
die()  { printf '  %s✗%s %s\n' "$c_red" "$c_rst" "$*" >&2; exit 1; }

# The one checksum tool the whole pipeline agrees on. Linux ships `sha256sum`;
# macOS ships `shasum -a 256`. Emit "<hex>  <name>" on both (the `-c` format).
sha256_of() { # $1 = file → prints the 64-hex digest only
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ── resolve the host target triple (or honor --target) ───────────────────────
host_target() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64)        echo "aarch64-apple-darwin";;
    Darwin/x86_64)       echo "x86_64-apple-darwin";;
    Linux/x86_64)        echo "x86_64-unknown-linux-gnu";;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu";;
    *) return 1;;
  esac
}

if [ "${#TARGETS[@]}" -eq 0 ]; then
  TARGETS=("$(host_target)") || die "unsupported host $(uname -sm) — pass --target explicitly, use --all, or build on a supported box"
fi
# De-dup (someone could pass --all --target <one-of-them>).
_seen=""; _uniq=()
for t in "${TARGETS[@]}"; do case " $_seen " in *" $t "*) ;; *) _uniq+=("$t"); _seen="$_seen $t";; esac; done
TARGETS=("${_uniq[@]}")

say "producing a prebuilt wg release"
echo "    source:  $WG_SOURCE_DIR (branch $WG_FORK_BRANCH)"
echo "    targets: ${TARGETS[*]}"
echo "    repo:    $WG_RELEASE_REPO   tag: $WG_RELEASE_TAG"
echo "    publish: $([ "$PUBLISH" -eq 1 ] && echo yes || echo 'no (dry run — build + checksum only)')"

[ -d "$WG_SOURCE_DIR" ] || die "fork checkout not found at $WG_SOURCE_DIR — set WG_SOURCE_DIR or clone lucapinello/workgraph"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$WG_SOURCE_DIR/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || die "could not read version from $WG_SOURCE_DIR/Cargo.toml"

# Soft branch check: warn (don't force-switch — the fork checkout is shared) if the
# source isn't on the fork branch we advertise as the release source.
CUR_BRANCH="$(git -C "$WG_SOURCE_DIR" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
[ "$CUR_BRANCH" = "$WG_FORK_BRANCH" ] || warn "source is on '$CUR_BRANCH', not '$WG_FORK_BRANCH' — checkout the fork branch for the canonical artifact"

# ── resolve a wg binary for ONE target ───────────────────────────────────────
# Prints the binary path on stdout and returns 0; returns non-zero (with a reason
# on stderr) when this target can't be produced on this machine — the caller then
# SKIPS it (so `--all` still ships the targets it can). Resolution order:
#   1. a per-target prebuilt binary from env (a CI artifact you downloaded):
#        WG_PREBUILT_BIN_<TARGET>  (target sanitized: '-' → '_', upper-cased), or the
#        friendly aliases WG_LINUX_BIN / WG_LINUX_ARM64_BIN / WG_MACOS_BIN, or plain WG_PREBUILT_BIN
#        when exactly ONE target was requested (back-compat).
#   2. NATIVE cargo build when the target IS the host (the always-works path).
#   3. CROSS cargo build when the rustup target is installed AND a working linker
#        exists — attempted, not assumed; on failure the target is SKIPPED, honestly.
resolve_bin_for_target() { # $1 = target  → echoes bin path or returns non-zero
  local target="$1" host env_name env_val
  host="$(host_target 2>/dev/null || echo none)"

  # 1. env-supplied prebuilt (CI artifact) — the sanctioned cross path.
  env_name="WG_PREBUILT_BIN_$(printf '%s' "$target" | tr 'a-z-' 'A-Z_')"
  env_val="${!env_name:-}"
  [ -z "$env_val" ] && case "$target" in
    x86_64-unknown-linux-gnu)  env_val="${WG_LINUX_BIN:-}";;
    aarch64-unknown-linux-gnu) env_val="${WG_LINUX_ARM64_BIN:-}";;
    aarch64-apple-darwin)      env_val="${WG_MACOS_BIN:-}";;
  esac
  [ -z "$env_val" ] && [ "${#TARGETS[@]}" -eq 1 ] && env_val="${WG_PREBUILT_BIN:-}"
  if [ -n "$env_val" ]; then
    [ -x "$env_val" ] || { echo "supplied binary $env_val ($env_name) is not executable" >&2; return 1; }
    echo "$env_val"; return 0
  fi

  # 2 & 3. build with cargo.
  if ! command -v cargo >/dev/null 2>&1; then
    echo "no cargo, and no prebuilt binary supplied for $target (set $env_name=/path/to/wg)" >&2; return 1
  fi
  local args=() bin="$WG_SOURCE_DIR/target/release/wg"
  if [ "$target" != "$host" ]; then
    # cross: only attempt if the rustup std target is installed.
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
      echo "cross-target $target needs 'rustup target add $target' + a $target linker — use the fork CI or a native box, or set $env_name" >&2
      return 2
    fi
    args=(--target "$target"); bin="$WG_SOURCE_DIR/target/$target/release/wg"
  fi
  say "building wg $VERSION for $target (cargo build --release --locked) …" >&2
  # `--locked` is MANDATORY (yanked rquest versions break a fresh resolve).
  # NB: "${args[@]+"${args[@]}"}" — bash 3.2 (macOS) errors on "${args[@]}" when the
  # array is empty (the native-host build case) under `set -u`; this guard expands to
  # nothing when args is unset/empty and to the flags when it is set.
  if ! ( cd "$WG_SOURCE_DIR" && cargo build --release --locked --bin wg "${args[@]+"${args[@]}"}" ) >&2; then
    echo "cargo build failed for $target (cross builds need the target linker — CI or a native box is the honest path)" >&2; return 3
  fi
  [ -x "$bin" ] || { echo "expected binary missing at $bin after build" >&2; return 3; }
  echo "$bin"
}

# ── stage + archive + checksum ONE target ────────────────────────────────────
package_target() { # $1 = target ; $2 = bin path
  local target="$1" bin="$2"
  local archive_root="wg-v${VERSION}-${target}" archive_name stage
  archive_name="${archive_root}.tar.gz"
  stage="$OUT_DIR/$archive_root"
  rm -rf "$stage"; mkdir -p "$stage"
  cp "$bin" "$stage/wg"
  # nex ships alongside wg in CI; include it when it sits next to the binary.
  local nex; nex="$(dirname "$bin")/nex"
  [ -x "$nex" ] && cp "$nex" "$stage/nex" || true
  [ -f "$WG_SOURCE_DIR/LICENSE" ] && cp "$WG_SOURCE_DIR/LICENSE" "$stage/LICENSE" || true
  cat > "$stage/README-install.txt" <<EOF
Prebuilt wg engine — Casa family-team release
  version: ${VERSION}
  target:  ${target}
  branch:  ${WG_FORK_BRANCH}

Install with the family stack (no compiler needed):
  ./bin/casa install-wg

Or verify + install by hand:
  shasum -a 256 -c ${archive_name}.sha256
  tar -xzf ${archive_name}
  install -m 0755 ${archive_root}/wg ~/.local/bin/wg
EOF
  say "packaging $archive_name …"
  tar -C "$OUT_DIR" -czf "$OUT_DIR/$archive_name" "$archive_root"

  local digest; digest="$(sha256_of "$OUT_DIR/$archive_name")"
  printf '%s  %s\n' "$digest" "$archive_name" > "$OUT_DIR/$archive_name.sha256"
  # Combined SHA256SUMS: replace this target's line, keep the others (so --all and
  # repeated single-target runs accumulate into ONE manifest the CI also emits).
  touch "$OUT_DIR/SHA256SUMS"
  grep -v "  $archive_name\$" "$OUT_DIR/SHA256SUMS" > "$OUT_DIR/SHA256SUMS.tmp" 2>/dev/null || true
  printf '%s  %s\n' "$digest" "$archive_name" >> "$OUT_DIR/SHA256SUMS.tmp"
  sort -k2 "$OUT_DIR/SHA256SUMS.tmp" > "$OUT_DIR/SHA256SUMS"; rm -f "$OUT_DIR/SHA256SUMS.tmp"

  # Self-verify — a release that can't verify itself is worthless.
  ( cd "$OUT_DIR" && ( command -v sha256sum >/dev/null 2>&1 && sha256sum -c "$archive_name.sha256" \
      || shasum -a 256 -c "$archive_name.sha256" ) >/dev/null ) \
    || die "self-verify FAILED for $archive_name — the checksum does not match the archive"
  ok "$target → $archive_name  (sha256 ${digest:0:16}…, self-verified)"
  PRODUCED_ASSETS+=("$OUT_DIR/$archive_name" "$OUT_DIR/$archive_name.sha256")
}

# ── the loop: resolve → package each requested target ────────────────────────
mkdir -p "$OUT_DIR"
PRODUCED_ASSETS=()
PRODUCED_TARGETS=()
SKIPPED_TARGETS=()
for target in "${TARGETS[@]}"; do
  if bin="$(resolve_bin_for_target "$target")"; then
    package_target "$target" "$bin"
    PRODUCED_TARGETS+=("$target")
  else
    warn "SKIPPED $target — could not produce a binary on this machine (see the reason above)"
    SKIPPED_TARGETS+=("$target")
  fi
done

[ "${#PRODUCED_TARGETS[@]}" -gt 0 ] || die "produced NOTHING — every requested target was un-buildable here. Supply a CI artifact via WG_PREBUILT_BIN_<TARGET>, or run on a native box."
echo
ok "produced: ${PRODUCED_TARGETS[*]}"
[ "${#SKIPPED_TARGETS[@]}" -gt 0 ] && warn "skipped:  ${SKIPPED_TARGETS[*]}  (get these from the fork CI or a native build box — docs/05 §2.1)"

if [ "$PUBLISH" -ne 1 ]; then
  echo
  say "dry run complete. To publish these as downloadable release assets, re-run with --publish"
  say "consumers install with:  ./bin/casa install-wg   ·   update with:  ./bin/casa update"
  exit 0
fi

# ── publish to the fork's GitHub Releases ────────────────────────────────────
command -v gh >/dev/null 2>&1 || die "gh CLI not found — needed to publish the release (brew install gh; gh auth login)"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run: gh auth login"

say "publishing to $WG_RELEASE_REPO release '$WG_RELEASE_TAG' …"
NOTES="Prebuilt \`wg\` engine for the Casa family-team stack (branch \`$WG_FORK_BRANCH\`, version $VERSION).

A new family installs it with **\`./bin/casa install-wg\`** — no Rust toolchain, no compile.
Assets are named \`wg-v<version>-<target>.tar.gz\` with matching \`.sha256\` checksums; \`SHA256SUMS\` lists them all.

Verify by hand:
\`\`\`
shasum -a 256 -c SHA256SUMS
\`\`\`"

ASSETS=("${PRODUCED_ASSETS[@]}" "$OUT_DIR/SHA256SUMS")
if gh release view "$WG_RELEASE_TAG" --repo "$WG_RELEASE_REPO" >/dev/null 2>&1; then
  gh release upload "$WG_RELEASE_TAG" "${ASSETS[@]}" --repo "$WG_RELEASE_REPO" --clobber \
    || die "gh release upload failed"
  ok "uploaded assets to existing release $WG_RELEASE_TAG"
else
  gh release create "$WG_RELEASE_TAG" "${ASSETS[@]}" \
    --repo "$WG_RELEASE_REPO" \
    --title "wg prebuilt ($WG_RELEASE_TAG)" \
    --notes "$NOTES" \
    --prerelease \
    || die "gh release create failed"
  ok "created release $WG_RELEASE_TAG with assets"
fi
echo
ok "published targets: ${PRODUCED_TARGETS[*]}"
[ "${#SKIPPED_TARGETS[@]}" -gt 0 ] && warn "NOT published (build them via CI / a native box, then re-run to add): ${SKIPPED_TARGETS[*]}"
say "published. A household now runs:  ./bin/casa install-wg   ·   updates with:  ./bin/casa update"
