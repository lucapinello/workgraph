# Casa household.toml — the composition contract for wg-side persona identity

The Casa family team (poietic-family-team) lets a household compose its own
agents — names, roles, personalities, rooms, domains — by editing ONE committable
file, `household.toml`, at the repo root. That file is the **source of truth for
persona identity**; see `docs/18-compose-your-family.md` in poietic-family-team
for the full spec, the shipped four-persona example, and the `casa household
apply` command.

This note is the **wg-side contract**: it documents where the workgraph Telegram
code currently hardcodes persona identity, and how each of those points relates
to `household.toml` so the two stay in sync (and so a future change can make them
read the derived artifact directly).

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

3. **/standup voices** — `src/notify/telegram_standup.rs`. `DEFAULT_ROSTER`
   hardcodes the ordered persona ids (`["nora", "bruno", "mira", "otto"]`) and a
   `id → (display name, emoji)` match arm. Both are exactly the ordered
   `household.toml` `[[agent]]` list with its `name` + `emoji` fields; the roster
   order is the file order. Seeding `DEFAULT_ROSTER` and the name/emoji lookup
   from the derived snapshot would keep standup in step with a renamed or added
   persona.

## Migration shape (for a future wg-side task)

`casa household apply` already writes `.casa/household.generated.json`:

```json
{
  "household": { "name": "Casa Pinello", "languages": ["English","Italian"], "members": ["Luca","Nadin"] },
  "roster": [ { "id": "nora", "label": "Nora", "nameplate": "Nora — meals & nutrition",
                "room": "study", "emoji": "🍎", "color": "#e07a9b", "domains": ["meals","nutrition"] }, … ],
  "owners": { "meals": "nora", "cooking": "bruno", "workouts": "mira", "shopping": "otto", … }
}
```

A wg-side change would read this snapshot (path relative to the project root,
next to `.wg/`) at startup and use it to seed the three consumers above, keeping
the hardcoded tables as the fallback for a checkout with no snapshot. Because the
snapshot is keyed by the same `id` used in `notify.toml`, no token ever needs to
be read from the composition file, and renaming stays display-only.
