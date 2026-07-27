#!/usr/bin/env bash
# Smoke: the reminder engine — plan rows + ad-hoc asks become scheduled nudges
# that fire EXACTLY once, restart-safe, with a `(late)` honesty window.
#
# The weekly plan already CONTAINS reminder rows in its `## 3. Calendar` table
# (`| Tue 07-14 | 19:30 | ⏰ Reminder: Luca PT check-in | Otto |`) but nothing
# fired them. `wg telegram remind` is the scheduler's CLI seam (same
# credential-free pattern as `wg telegram elect` / `feed-write`): it reads the
# current plan's reminders plus the ad-hoc store and decides what to send at
# `--now`, without touching a live bot.
#
# This drives the REAL binary end-to-end and pins the behaviour the unit tests
# assert, through the CLI a human/scheduler actually invokes:
#   1. a plan reminder row is listed as one pending reminder;
#   2. at its due minute it WOULD SEND on time; 40 min late it fires `(late)`;
#      over 2h late it is DROPPED (never a stale nag);
#   3. an ad-hoc "remind me Thursday to …" registers and confirms in one line;
#   4. a real fire records durable state so a restart NEVER re-fires (exactly
#      once), and the ad-hoc reminder is fired independently of the plan one.
# Credential-free: no notify.toml, so nothing is sent; state files prove intent.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg
# A stale deployed binary (pre-`reminders-that-reach`) has no `remind` subcommand
# — skip loudly rather than FAIL, so this scenario only gates a wg built with it.
if ! wg telegram remind --help >/dev/null 2>&1; then
    loud_skip "STALE WG BINARY" "wg has no 'telegram remind' subcommand; rebuild from the fork"
fi

scratch="$(make_scratch)"
export WG_DIR="$scratch/.wg"
mkdir -p "$scratch/plans" "$scratch/.wg/agency/bindings"

# A minimal plan with exactly one reminder row in its calendar section.
cat > "$scratch/plans/2026-W29-family-plan.md" <<'MD'
# Family plan — 2026-W29

**Week of Monday 2026-07-13 → Sunday 2026-07-19**
**Status:** DRAFT

## 3. Calendar (Otto) — combined projection

| Day | Time | Event | Source |
|-----|------|-------|--------|
| Mon 07-13 | 18:30 | Cook: chickpea & spinach curry | Bruno |
| Tue 07-14 | 19:30 | ⏰ Reminder: Luca PT check-in (if unanswered) | Otto |
MD

# One confirmed human so the recipient resolves and (in production) is DM-able.
cat > "$scratch/.wg/agency/bindings/telegram.yaml" <<'YAML'
bindings:
  - telegram_user: "55501234"
    agent_id: luca
    name: Luca
    bot_id: otto
    confirmed: true
    created_at: "2026-07-01T00:00:00Z"
YAML

# The row's Source column ("Otto") must resolve to ONE configured voice. Since
# the fail-closed owner rule landed, an unknown source yields no reminder at all
# — without this household the scenario went quietly green-to-red on step 1.
cat > "$scratch/household.toml" <<'TOML'
[[agent]]
id = "otto"
name = "Otto"
domains = ["coordination", "calendar"]
TOML

remind() { (cd "$scratch" && WG_DIR="$scratch/.wg" wg telegram remind "$@"); }

echo "1. the plan reminder row is listed as one pending reminder:"
out="$(remind --list --now 2026-07-14T09:00)"
echo "$out" | grep -q "1 total" || loud_fail "expected 1 reminder, got: $out"
echo "$out" | grep -q "Luca PT check-in" || loud_fail "reminder body missing: $out"
echo "$out" | grep -qi "pending" || loud_fail "expected pending state: $out"

echo "2a. at 19:30 it WOULD SEND on time (no late tag):"
out="$(remind --dry-run --now 2026-07-14T19:30)"
echo "$out" | grep -q "WOULD SEND to Luca" || loud_fail "expected on-time send: $out"
echo "$out" | grep -q "(late)" && loud_fail "on-time fire must not carry a late tag: $out"
echo "   → $out"

echo "2b. 40 min late it fires with the honest (late) tag:"
out="$(remind --dry-run --now 2026-07-14T20:10)"
echo "$out" | grep -q "WOULD SEND to Luca" || loud_fail "expected late send: $out"
echo "$out" | grep -q "(late)" || loud_fail "expected (late) tag: $out"
echo "   → $out"

echo "2c. over 2h late it is DROPPED, never sent:"
out="$(remind --dry-run --now 2026-07-14T22:40)"
echo "$out" | grep -q "WOULD DROP" || loud_fail "expected a drop past the late window: $out"
echo "$out" | grep -q "WOULD SEND" && loud_fail "a >2h-late reminder must not send: $out"
echo "   → $out"

echo "3. an ad-hoc 'remind me Thursday to …' registers and confirms in one line:"
out="$(remind --add 'Otto remind me Thursday to defrost the trout' --recipient Luca --now 2026-07-12T10:00)"
[ "$out" = "Will do — Thursday morning ✓" ] || loud_fail "unexpected confirmation: $out"
[ -f "$scratch/.casa/reminders-adhoc.json" ] || loud_fail "ad-hoc store not written"
grep -q "Defrost the trout" "$scratch/.casa/reminders-adhoc.json" \
    || loud_fail "ad-hoc body not persisted"
# The ad-hoc reminder now shows up in the list alongside the plan one.
remind --list --now 2026-07-12T10:00 | grep -q "2 total" \
    || loud_fail "ad-hoc reminder did not join the list"

echo "4. a real fire records durable state so a restart never re-fires (exactly once):"
# No notify.toml → the DM send is skipped, but state is recorded FIRST.
remind --now 2026-07-14T19:30 >/dev/null 2>&1 || true
state="$scratch/.casa/reminders-state.json"
[ -f "$state" ] || loud_fail "fired-state file not written"
grep -q '"plan:2026-W29:2026-07-14:1930' "$state" \
    || loud_fail "plan reminder not recorded in fired state: $(cat "$state")"
# A later tick (restart, a few minutes on) must fire NOTHING new for it.
out="$(remind --dry-run --now 2026-07-14T19:34)"
echo "$out" | grep -q "Luca PT check-in" \
    && loud_fail "restart re-fired an already-fired reminder: $out"

echo "PASS: reminder engine — plan+ad-hoc reminders list, fire on-time/late, drop stale, exactly-once across restart"
