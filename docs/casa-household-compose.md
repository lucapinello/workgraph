# Casa household.toml — the composition contract for wg-side persona identity

The Casa family team (poietic-family-team) lets a household compose its own
agents — names, roles, personalities, rooms, domains — by editing ONE committable
file, `household.toml`, at the repo root. That file is the **source of truth for
persona identity**; see `docs/18-compose-your-family.md` in poietic-family-team
for the full spec, the shipped four-persona example, and the `casa household
apply` command.

This note is the **wg-side contract**: it documents how the workgraph Telegram
code joins persona identity to bot delivery without putting secrets in
`household.toml`.

## The join key is the agent `id`

Every persona is identified by a stable lowercase `id` (`nora`, `bruno`, `mira`,
`otto`). That id is the single key that joins all surfaces:

| Surface | Keyed by `id` |
|---------|---------------|
| `household.toml` `[[agent]] id` | the composition source |
| `.wg/notify.toml` `[telegram.bots.<id>]` | the bot **token** (secret) + `username` |
| chat session binding (`wg agent session`) | the persistent persona session |
| office actor id / room placement | the 3D home |

**Tokens never live in `household.toml`.** It is committable; bot tokens are
secrets and stay in `.wg/notify.toml`, keyed by the same `id`. `household.toml`
carries only the public `telegram_bot_username` (the `@handle`) so the mapping is
visible. The Casa loader refuses to read any secret-shaped key from
`household.toml`.

## wg-side consumers of persona identity

These are the places in the workgraph Telegram code that name personas today.
Each should read from `household.toml` (directly, or via the derived snapshot
`.casa/household.generated.json` that `casa household apply` writes).

1. **Group @mention resolution / election** — `src/notify/telegram_group.rs`.
   Already config-driven: it resolves an @mention or an addressing token
   (`"tell bruno …"`, `@nora_casapinello_bot`) against the configured
   `[telegram.bots.<id>]` keys case-insensitively, falling back to the leading
   underscore-segment of a `…bot` handle. Because `household.toml` and
   `notify.toml` share the `id`, renaming a persona's *display name* in
   `household.toml` does not affect election as long as the `id` (and its bot
   entry) is unchanged. No hardcoded name roster lives here.

2. **Family command owners** — `src/notify/telegram_family_commands.rs`. The
   `COMMANDS` table hardcodes the owning bot id per command
   (`/dinner` → `owner: "bruno"`, `/shopping` → `owner: "otto"`, …). In
   `household.toml` this is expressed as agent `domains`
   (`bruno` owns `cooking`/`recipes`, `otto` owns `shopping`/`calendar`/
   `coordination`); `casa household apply` derives a `domains → owner id` map
   into `.casa/household.generated.json` (`owners`). A command's owner is the
   agent that owns its domain (`/dinner` → `meals` → the meals owner). Reading
   the derived `owners` map here would let a family reassign a command by editing
   `domains`, no code change.

3. **Standup, collective, and discussion voices** —
   `src/notify/telegram_standup.rs` loads the ordered `household.toml`
   `[[agent]]` list directly and joins each `id` to
   `[telegram.bots.<id>]`. The file order is speaking order; `name` and `emoji`
   are the presentation. A configured bot outside the household roster is not
   promoted to a voice. A missing, malformed, duplicate, or incomplete
   multi-voice roster fails closed instead of falling back to bot-map order or
   compiled names. The legacy top-level single bot is accepted only for a
   one-person household, where its identity is unambiguous.

## Generated snapshot

`casa household apply` already writes `.casa/household.generated.json`:

```json
{
  "household": { "name": "Casa Pinello", "languages": ["English","Italian"], "members": ["Luca","Nadin"] },
  "roster": [ { "id": "nora", "label": "Nora", "nameplate": "Nora — meals & nutrition",
                "room": "study", "emoji": "🍎", "color": "#e07a9b", "domains": ["meals","nutrition"] }, … ],
  "owners": { "meals": "nora", "cooking": "bruno", "workouts": "mira", "shopping": "otto", … }
}
```

Telegram roster consumers intentionally read the authored `household.toml`
instead of requiring this generated snapshot, so a fresh checkout cannot drift
between the authored roster and an old `.casa` runtime artifact. The join remains
the same `id` used in `notify.toml`; no token is read from the composition file,
and renaming stays display-only.
