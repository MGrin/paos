# paos — a personal agentic OS

paos gives long-running coding agents three things they do not have on their own: a
durable memory that survives the session, a message bus so several sessions can coordinate
as peers, and a channel to reach you on Telegram when they need a decision and you are not
at the keyboard.

It is a single Rust binary and a background daemon over SQLite. No cloud, no API keys for
the core, and everything except the Telegram bridge works offline.

## Install

```sh
cargo build --release
install -m 755 target/release/paosd ~/.local/bin/paosd
install -m 755 target/release/paos  ~/.local/bin/paos
paos init
```

`paos init` creates the store, downloads the embedding model (129 MB, once), and — if you
want it — configures Telegram. It asks for a bot token and then learns your chat and user
id from your first message to the bot, so you never have to find a numeric Telegram id.

Telegram is optional. Memory, the bus and the dashboard all work without it.

## Architecture

`paosd` (daemon) + `paos` (thin CLI), one binary each, no runtime dependencies. The daemon
is the **sole SQLite writer** and the **sole migration authority**; the CLI is a thin
client speaking length-prefixed JSON over a unix socket. The daemon also embeds locally,
serves the dashboard on `127.0.0.1:8788`, and runs the Telegram bridge when configured.

## The facets

| | |
|---|---|
| `paos memory` | durable facts, scoped global / org / project, recalled semantically |
| `paos bus` | rooms, peer messages, and a blocking wait so an idle session costs nothing |
| `paos operator` | reach your human on Telegram, and only when they opened that door |
| `paos task` | a shared work queue with a kanban board |

## Configuration

`paos config list` and the dashboard's settings page share one schema, so the UI can never
offer a knob the daemon does not read. Secrets are held **by reference** — the database
stores `keychain:paos/telegram_bot_token` or `env:PAOS_TELEGRAM_BOT_TOKEN`, never a value,
which is what lets the settings page report a token as configured or missing without ever
holding it.

## Honest limits

- **macOS first.** The Keychain backend, the LaunchAgent and the Übersicht widget are Mac.
  Linux works through the `.env` secret backend; nobody has run it there in anger.
- **Shaped for Claude Code.** The session lifecycle hooks assume it.
- **One operator per bot.** The Telegram bridge authorises a single user id by design.
- **You bring your own bot.** Create one with @BotFather; `paos init` prints the steps.

## Build and test

`cargo build --release` · `cargo test` (≈960 tests, all offline)

Design notes: `docs/superpowers/specs/2026-07-29-paos-rearchitecture-design.md`.

## Licence

MIT. Every dependency is MIT or Apache-2.0, and the embedding model
(`minishlab/potion-retrieval-32M`) is MIT and downloaded at install time rather than
vendored.
