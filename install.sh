#!/usr/bin/env bash
# paos installer.
#
# Everything the README used to leave you to work out: build, install both binaries,
# put the skill where Claude Code looks for it, and get the daemon actually RUNNING.
# The last one matters most — the CLI is a thin client, so without a daemon every
# command fails with "cannot reach paosd" and nothing tells you the daemon was never
# started.
#
# Safe to re-run. Nothing here is destructive: no file is deleted, and an existing
# config is left alone.
set -euo pipefail

BIN="${BIN:-$HOME/.local/bin}"
SKILLS="${SKILLS:-$HOME/.claude/skills}"
AGENTS="${AGENTS:-$HOME/Library/LaunchAgents}"
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  ✓ %s\n' "$*"; }
warn() { printf '  ⚠ %s\n' "$*" >&2; }

command -v cargo >/dev/null 2>&1 || {
  echo "cargo not found — install Rust first: https://rustup.rs" >&2
  exit 1
}

say "building (a few minutes the first time)…"
( cd "$SRC/paos" && cargo build --release )

mkdir -p "$BIN"
install -m 755 "$SRC/paos/target/release/paosd" "$BIN/paosd"
install -m 755 "$SRC/paos/target/release/paos"  "$BIN/paos"
ok "paos + paosd → $BIN"

# macOS kills a freshly built binary with OS_REASON_CODESIGNING and launchctl reports
# only "spawn scheduled" — the daemon looks restarted and is not. Re-signing is free.
if [ "$(uname -s)" = "Darwin" ]; then
  codesign -f -s - "$BIN/paosd" 2>/dev/null || true
  codesign -f -s - "$BIN/paos"  2>/dev/null || true
fi

# The skill is the point of paos for a Claude Code user, and nothing else puts it there.
mkdir -p "$SKILLS/paos"
cp -R "$SRC/skill/." "$SKILLS/paos/"
ok "skill → $SKILLS/paos"

case "$(uname -s)" in
  Darwin)
    mkdir -p "$AGENTS" "$HOME/.paos/server-logs"
    # The plist ships with __HOME__ placeholders because a LaunchAgent cannot expand ~.
    sed "s|__HOME__|$HOME|g" "$SRC/install/ai.paos.daemon.plist" \
      > "$AGENTS/ai.paos.daemon.plist"
    launchctl bootstrap "gui/$(id -u)" "$AGENTS/ai.paos.daemon.plist" 2>/dev/null || true
    launchctl kickstart -k "gui/$(id -u)/ai.paos.daemon" 2>/dev/null || true
    ok "daemon installed and started"
    ;;
  *)
    warn "no LaunchAgent on $(uname -s) — start the daemon yourself: $BIN/paosd &"
    warn "  (and see SECURITY.md: the Keychain backend is macOS-only, Linux uses .env)"
    ;;
esac

# Verify rather than assume. An installer that prints success while the daemon is dead
# is the failure this whole script exists to prevent.
for _ in $(seq 1 10); do
  if "$BIN/paos" ping 2>/dev/null | grep -q pong; then
    ok "daemon answering"
    printf '\nNow run:  paos init\n'
    exit 0
  fi
  sleep 1
done

warn "the daemon is not answering yet."
warn "  logs: ~/.paos/server-logs/paosd.err.log"
warn "  start it by hand: $BIN/paosd"
exit 1
