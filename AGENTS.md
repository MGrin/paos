<!-- agents-md ceiling: 60 lines -->
# AGENTS.md — paos

A personal agentic OS: durable memory, a peer bus and a Telegram operator channel for
long-running coding agents. One Rust workspace, two binaries (`paosd` the daemon, `paos`
the client), SQLite, offline except the Telegram bridge.

**[`CONTRIBUTING.md`](CONTRIBUTING.md) is the working agreement and it is complete** — the
house style, the one rule about a second `paosd`, the layout, and the three-step deploy
that testing does not do for you. Read it before changing anything. [`README.md`](README.md)
is the user-facing document, including the honest limits. This file adds only what neither
of them says.

## The Cargo workspace is NOT at the repo root

```sh
cd paos && cargo test --workspace --locked    # ran 2026-09-09: 39 result blocks, 1014 passed, 0 failed, 0 ignored
cd paos && cargo build --release --locked
bats skill/tests/dispatcher.bats              # ran 2026-09-09: 3 tests, 3 ok — run from the REPO ROOT
```

From the repo root `cargo test` fails with `could not find Cargo.toml`. Every Rust step in
`.github/workflows/ci.yml` carries `working-directory: paos` for that reason, and the bats
step deliberately does not. Getting this wrong reads as a broken checkout rather than as a
wrong directory, which is why it is the first thing in this file.

## The gate

`.github/workflows/ci.yml`, on Linux **and** macOS: `cargo test --workspace --locked`,
`cargo build --release --locked`, then a step that runs the built binaries — because a
workspace that compiles and a binary that starts are different claims. macOS additionally
runs the bats suite over the skill's dispatcher. `release.yml` re-runs all of it, checks
the binaries are ad-hoc signed, and publishes.

The whole Rust suite is **offline** — no network, no daemon, no Telegram. A test that needs
any of those is the bug.

## Two things that will cost you an hour

- **A build needs a C++ compiler**, not just Rust: the embedding tokenizer builds
  `esaxx-rs` natively. It went unnoticed until someone built in a clean container.
- **Unsetting `TELEGRAM_BOT_TOKEN` protects nothing.** The daemon reads
  `~/.claude/skills/paos/.env` by absolute path, so any build finds the real token. The
  installed-binary check is what keeps `cargo run` safe; `PAOS_ALLOW_BRIDGE=1` removes it.

## Layout

| path | what it is |
|---|---|
| `paos/crates/` | thirteen crates — `paosd` the daemon, `paos-cli` the client, the rest one facet each |
| `paos/parity/` | `difflib_parity.py` and its README — the Python reference the Rust is checked against |
| `skill/` | the Claude Code skill: `SKILL.md`, `references/`, and the `paos` dispatcher with its bats suite |
| `widgets/`, `install/` | the optional Übersicht widget and the macOS LaunchAgent |
| `install.sh` | 80 lines of shell that build, install, start and then VERIFY — re-running it is safe |

**Nothing about who may merge, how agents are spawned, or how the maintainer's
machine handles secrets belongs in this file, and none of it is stated here.**
Those are properties of a working environment, not of this project; if you are
contributing, your own conventions apply and nothing in this repo depends on
the maintainer's.
