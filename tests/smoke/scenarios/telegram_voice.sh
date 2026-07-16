#!/usr/bin/env bash
# Smoke: Telegram voice notes — a recording becomes a normal message
# (task telegram-voice-notes).
#
# Pins the user-visible behaviour Luca asked for: a voice note (or audio /
# video_note) sent to any household bot is transcribed by the SAME whisper the
# kiosk mic uses, and the transcript then flows through the EXACT same inbound
# path as a typed line — election, single-owner routing, fast lane, composer.
#
# Drives the REAL binary via `wg telegram voice`, the dry-run of the full
# detect→transcribe→inject path the live listener runs. Credential-free: a STUB
# gateway (`--stub-ok` / `--stub-reason`) stands in for whisper, so no live
# engine, ffmpeg, or bot token is needed.
#
# What it locks:
#   - spoken "add milk to the shopping list" → fast-lane:shopping-add (a spoken
#     add routes IDENTICALLY to a typed add — reaches the plan seam)
#   - spoken "what's the plan today"         → composer (open ask, same as typed)
#   - transcribe unconfigured → the honest "casa voice-setup" line, never silent
#   - decode-failed / error   → the "couldn't make out that recording" line
#   - silence                 → the silence line
#   - the recording bytes actually reach the transcriber (byte count reported)
#
# Also pinned by the notify::telegram_voice unit/tokio tests (parse, classify,
# orchestrator, plan-seam) and notify::telegram decode_update + redaction tests.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./_helpers.sh
scenario_dir="$(pwd)"

require_wg

scratch="$(make_scratch)"

# A stand-in recording. Its bytes are never decoded in stub mode (the stub
# gateway ignores them), but the detect step reads the file and reports its size.
ogg="$scratch/note.ogg"
printf 'OggS\x00fake-opus-voice-note-bytes' >"$ogg"

voice() {
    (cd "$scratch" && WG_DIR= wg telegram voice --file "$ogg" "$@" 2>&1)
}
voice_json() {
    (cd "$scratch" && WG_DIR= wg --json telegram voice --file "$ogg" "$@" 2>&1)
}

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

echo "spoken 'add milk to the shopping list' → routes like a typed add (fast lane):"
out="$(voice_json --stub-ok 'add milk to the shopping list')"
expect_grep "transcript ok"        "$out" '"outcome":"transcript"'
expect_grep "injected body"        "$out" '"injected_body":"add milk to the shopping list"'
expect_grep "routes as shopping-add" "$out" '"route":"fast-lane:shopping-add"'

echo "spoken open question → the conversation composer (same as typed):"
out="$(voice_json --stub-ok "what's the plan today")"
expect_grep "transcript ok"     "$out" '"outcome":"transcript"'
expect_grep "routes to composer" "$out" '"route":"composer"'

echo "recording bytes actually reach the transcriber:"
out="$(voice --stub-ok 'add eggs to the shopping list')"
expect_grep "detect reports bytes" "$out" 'bytes, mime audio/ogg'
expect_grep "transcribe ok"        "$out" 'transcribe: ok'

echo "transcribe unconfigured → honest one-time-setup line, never silent:"
out="$(voice --stub-reason unconfigured)"
expect_grep "names casa voice-setup" "$out" 'casa voice-setup'

echo "decode-failed → the 'couldn't make out that recording' line:"
out="$(voice --stub-reason decode-failed)"
expect_grep "invites typing" "$out" 'mind typing it'

echo "silence → the silence line:"
out="$(voice --stub-reason silence)"
expect_grep "reports silence" "$out" 'nothing I could hear'

echo "PASS: a Telegram voice note transcribes via the gateway and its transcript routes exactly like a typed line; every failure reason produces an honest in-persona reply"
