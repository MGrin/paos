# Security

## Reporting

Open a [private security advisory](https://github.com/MGrin/paos/security/advisories/new)
rather than a public issue. This is a personal project with one maintainer, so expect a
reply in days, not hours.

## What paos holds, so you can judge the blast radius

`~/.paos/paos.db` is the whole system: every remembered fact, every message between
sessions, and every escalation to the operator. It is a plain SQLite file with the
permissions your umask gave it, and **it is not encrypted**. Anyone who can read your home
directory can read all of it. That is a deliberate trade — the alternative is a key the
daemon must hold anyway — but it should be a decision you made rather than one you
discover.

## Secrets are held by reference

The Telegram bot token is never stored in the database. `paos_config` holds a *reference*:

- `keychain:paos/telegram_bot_token` — macOS Keychain, read through `security(1)`
- `env:PAOS_TELEGRAM_BOT_TOKEN` — an environment variable or the `.env` beside the store

Which is why the dashboard can report a token as configured or missing without ever
holding it: the web layer does not link the crate that can read secrets, and asks the
daemon for a status enum instead. A `.env` written by `paos init` is `0600`.

## The failure mode worth knowing about

**A second Telegram consumer is silent.** Telegram gives each update to whichever process
polls first, so a stray `paosd` — a test build, a second checkout — takes the operator's
messages and nothing anywhere reports an error. The symptom is commands that intermittently
do nothing.

The daemon therefore refuses to bridge unless it is the installed binary, and
`PAOS_ALLOW_BRIDGE=1` is the explicit override. Unsetting `TELEGRAM_BOT_TOKEN` does **not**
protect you: the daemon reads `~/.claude/skills/paos/.env` by absolute path.

## Trust boundaries

- **The dashboard** binds `127.0.0.1:8788` with no authentication. Anything that can reach
  localhost can read your memory and post as you. Do not forward that port.
- **The bus** is between sessions on one machine, over a unix socket. There is no network
  listener and no remote peer.
- **Telegram** authorises exactly one user id. Messages from anyone else in the group are
  ignored, because a peer that could impersonate the operator could instruct every session
  on the machine.
- **`paos init` downloads a model** from Hugging Face over HTTPS. Nothing else fetches code
  at runtime.
