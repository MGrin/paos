# paosd — the Rust daemon
> **`paos-operatord` / `ai.paos.operator` are RETIRED (2026-07-30).** paosd owns
> Telegram, the outbox, the dashboard and the nightly dream. Do **not** load that
> LaunchAgent: a second process on the same bot token makes Telegram hand each
> message to whichever consumer asks first, so the operator's messages vanish at
> random with no error anywhere. If delivery looks broken, run `paosctl doctor`
> and check `operator_outbox.sent_ts` — not the old agent.


What actually runs your operator channel, dashboard and fleet supervision as of
2026-07-29. Agents do not call it directly; this is here so you know what is watching.

## What it replaced

One LaunchAgent (`ai.paos.daemon`) in place of two (`ai.paos.operator` + `ai.paos.ui`).
Measured on the live machine: **0.029% CPU, ~6 MB RSS**, against ~50 MB and 92,900
SQLite queries/hour before.

## What it does

| | |
|---|---|
| Telegram bridge | escalations with tap-to-answer buttons, quote-reply routing, per-room forum topics, `@operator` → your real Telegram `@username` mention |
| Dashboard | `http://127.0.0.1:8788` — Inbox (answer/resolve/mode), memory search, fleet, bus, activity. Served from the binary; no node, no build step |
| Supervisor | flags **stale** (tasked, silent >30 min) and **DEAF** (in rooms, alive, nothing listening >40 min) every 60 s |
| Proactive alerts | pushes unprompted when a Claude account hits its weekly cap, or a session goes deaf. Once per crossing, re-armed on recovery |

## Telegram commands

`/digest` `/who` `/blocked` `/parked` `/say <session> <text>` `/accounts` `/switch`
`/here` `/auto` `/away` `/help` — all in the bot's `/` menu.

`/accounts` shows every Claude account's 5h and 7d usage worst-first; `/switch` rotates
to the one with the most weekly headroom. A maxed account also appears in `/digest`,
because a weekly cap stops every session at once.

## What is still Python

**The `paos bus` and `paos memory` CLI that sessions call.** Nothing about how you
invoke paos has changed — keep using `~/.claude/skills/paos/paos`. The Rust CLI is
installed as `paosctl` (daemon control only) precisely so it cannot shadow it.

Liveness is shared across both: the daemon reads the same per-handle advisory lock your
`wait-joined` listener holds, so it can tell a live Python session from a deaf one.

## Operating it

```sh
launchctl kickstart -k gui/$(id -u)/ai.paos.daemon   # restart
tail -f ~/.paos/server-logs/paosd.err.log            # logs
setup/paos-cutover.sh rollback                       # back to the Python daemons
```

Rollback is one command and nothing was deleted — the Python skill and the old
plists are all still in place.
