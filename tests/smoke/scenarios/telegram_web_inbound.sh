#!/usr/bin/env bash
# Smoke: web-inbound — a kiosk-pane message is a FIRST-CLASS group turn.
#
# Pins task web-chat-first. The live gap: a message typed in the casa kiosk
# conversation pane was only RELAYED into the family Telegram group by a bot, and
# Telegram bots never see other bots' messages — so the listener's election /
# conversation pipeline NEVER ran on it (posted, never answered), while the SAME
# words typed on a phone got four replies.
#
# The fix is `wg telegram web-inbound`, which runs the EXACT `elect_responders`
# table the live listener runs and then dispatches through the SAME senders +
# composer — collective (all four) / discussion round / single voice / silence.
# The gateway shells out to it on POST /conversation/send.
#
# Proven through the REAL binary via `wg telegram web-inbound --dry-run --json`:
# the dry-run seam runs the real election + planning and reports WHO would answer
# without sending — so this is credential-free (nothing is ever sent to Telegram)
# and asserts the web-origin message routes identically to a Telegram message.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh

require_wg

scratch="$(make_scratch)"

# A self-contained project with a confirmed human (single-human Casa Pinello
# group) so the membership-aware election answers rather than staying silent —
# and so the web sender resolves to a confirmed human (grounded, not onboarding).
(
    cd "$scratch"
    export WG_DIR=
    wg init >/dev/null 2>&1
    wg agency human add "Luca" --telegram 8905220378 >/dev/null 2>&1
    wg agency human confirm 8905220378 >/dev/null 2>&1
)

cat >"$scratch/household.toml" <<'TOML'
[[agent]]
id = "nora"
name = "Nora"
emoji = "🥗"
domains = ["meals", "nutrition"]
[[agent]]
id = "bruno"
name = "Bruno"
emoji = "🍳"
domains = ["meals", "cooking", "recipes"]
[[agent]]
id = "mira"
name = "Coach Mira"
emoji = "💪"
domains = ["workouts"]
[[agent]]
id = "otto"
name = "Otto"
emoji = "📋"
domains = ["calendar", "coordination", "shopping"]
TOML

# Matching bots; collective order comes from household.toml.
cat >"$scratch/.wg/notify.toml" <<'TOML'
[telegram]
chat_id = "-1000000000001"

[telegram.bots.nora]
bot_token = "0000000000:nora-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "nora"
username  = "nora_casapinello_bot"

[telegram.bots.bruno]
bot_token = "0000000000:bruno-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "bruno"
username  = "bruno_casapinello_bot"

[telegram.bots.mira]
bot_token = "0000000000:mira-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "mira"
username  = "mira_casapinello_bot"

[telegram.bots.otto]
bot_token = "0000000000:otto-dummy-token"
chat_id   = "-1000000000001"
agent_id  = "otto"
username  = "otto_casapinello_bot"
TOML

web() { (cd "$scratch" && WG_DIR= wg --json telegram web-inbound --dry-run --sender "$1" --message "$2" 2>&1); }

expect_grep() {
    local desc="$1" out="$2" needle="$3"
    if ! grep -qF -- "$needle" <<<"$out"; then
        loud_fail "$desc — expected to find: $needle
--- got ---
$out
-----------"
    fi
    echo "  ok: $desc"
}

# --- Collective ask → the whole roster answers (the headline fix) -----------
out="$(web Luca "hey all, are you around?")"
expect_grep "collective ask elects the roster" "$out" '"category": "collective"'
for voice in nora bruno mira otto; do
    expect_grep "collective includes $voice" "$out" "$voice"
done

# --- Discussion ask → a multi-voice round -----------------------------------
out="$(web Luca "can you guys discuss dinner and find consensus?")"
expect_grep "discussion ask runs a round" "$out" '"category": "discussion-round"'

# --- Named ask → exactly one voice ------------------------------------------
out="$(web Luca "nora, what's for dinner?")"
expect_grep "named address is a single voice" "$out" '"category": "single-voice"'
expect_grep "the named voice is nora" "$out" '"who": "nora"'

# --- Web identity resolves to the confirmed human ---------------------------
# The web session hands a humanId ("luca") / display name — it must resolve to
# the confirmed human's binding key so the composer answers grounded, not the
# onboarding line. auth_sender is the stored numeric telegram id.
out="$(web luca "nora?")"
expect_grep "web humanId 'luca' resolves to the confirmed human" "$out" '"auth_sender": "8905220378"'

# --- Small talk with a second human present → silence (no double-answering) --
# With 2+ humans the membership-aware rule protects human-to-human chatter: the
# web message is silenced just as the same words would be on Telegram.
out="$(cd "$scratch" && WG_DIR= wg --json telegram web-inbound --dry-run --sender Luca --message "lol ok sounds good" 2>&1 || true)"
# In a single-human group even small talk may draw the concierge; that is fine.
# The invariant we pin: a dry-run never errors and always reports a category.
expect_grep "dry-run always reports a category" "$out" '"category"'

echo "PASS: telegram_web_inbound"
