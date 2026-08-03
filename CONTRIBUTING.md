# Contributing

## Before anything else

```sh
cargo test          # the whole suite, offline: no network, no daemon
```

If that does not pass on a clean checkout, that is a bug in this repository and worth an
issue on its own.

## The one rule that will bite you

**Never run a second `paosd` on a machine that already has one.** Telegram hands each
update to whichever consumer polls first, so a second one silently eats the operator's
messages and nothing errors anywhere.

The daemon now refuses to bridge unless it is the installed binary
(`~/.local/bin/paosd`), so `cargo run` is safe by default. `PAOS_ALLOW_BRIDGE=1` overrides
that, and you should only type it if you mean it.

Note what does **not** protect you: unsetting `TELEGRAM_BOT_TOKEN`. The daemon reads
`~/.claude/skills/paos/.env` by absolute path, so any build finds the real token whether
or not it is in your environment.

## House style

The tests and comments here explain **why**, not what. A comment that says what the line
below does is noise; a comment that says which failure the line prevents is the reason
anyone can change it safely later. Most of the comments in this codebase name a real
incident, because most of them were written after one.

Concretely:

- **A test names the behaviour, not the function.** `a_command_typed_as_a_reply_still_runs`
  beats `test_handle_message_3`.
- **Prove a new guard fails.** Break the thing it guards, watch it go red, put it back. A
  guard nobody has seen fail is a guard nobody knows works — this repository has shipped
  two that were passing vacuously.
- **Keep the daemon the single writer.** Everything that mutates state goes through it.
  Two writers to one SQLite file is the bug class this architecture exists to remove.
- **No new dependencies without a reason you can state.** The whole tree is MIT/Apache-2.0
  and the binary is self-contained; both are worth keeping.

## Layout

| | |
|---|---|
| `paos/crates/` | the twelve crates — `paosd` is the daemon, `paos-cli` the client |
| `skill/` | the Claude Code skill: `SKILL.md` plus its references |
| `widgets/` | an optional Übersicht widget |
| `install/` | the macOS LaunchAgent |

## Deploying a change

Testing does not update the running daemon. Three steps, and skipping any one leaves the
old binary running while your tests pass:

```sh
cargo build --release
install -m 755 target/release/paosd ~/.local/bin/paosd
launchctl kickstart -k gui/$(id -u)/ai.paos.daemon   # macOS
```

On macOS a freshly built binary may be killed by the kernel with `OS_REASON_CODESIGNING`,
and `launchctl` reports only `spawn scheduled` — so the daemon looks restarted and is not.
`codesign -f -s - ~/.local/bin/paosd` fixes it. Verify with `paos ping`, never with the
exit code of the install.
