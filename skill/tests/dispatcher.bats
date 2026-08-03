#!/usr/bin/env bats
#
# The `paos` entry point.
#
# It was a 97-line Python dispatcher; every facet runs in Rust now, so it is `exec`. These
# replace the Python test that checked a routing table it no longer has — and they live
# here rather than in the machine-setup repo that used to vendor this skill, because a
# test pointing at a file its repo does not contain is a test that fails for the wrong
# reason. It did.

PAOS="$BATS_TEST_DIRNAME/../paos"

@test "paos execs paosctl with the arguments untouched" {
  fake="$BATS_TEST_TMPDIR/paosctl"
  printf '#!/bin/sh\nprintf "%%s\\n" "$@" > "%s/argv"\nexit 0\n' "$BATS_TEST_TMPDIR" > "$fake"
  chmod +x "$fake"
  run env PAOSCTL="$fake" bash "$PAOS" bus send lobby --to @peer "a body with spaces"
  [ "$status" -eq 0 ]
  # One argument per line: a wrapper that re-splits "a body with spaces" would post four
  # words, or a truncated message, and the sender would never know.
  [ "$(wc -l < "$BATS_TEST_TMPDIR/argv" | tr -d ' ')" = "6" ]
  grep -qx 'a body with spaces' "$BATS_TEST_TMPDIR/argv"
}

@test "paos passes paosctl's exit code through rather than inventing one" {
  fake="$BATS_TEST_TMPDIR/paosctl"
  printf '#!/bin/sh\nexit 69\n' > "$fake"; chmod +x "$fake"
  run env PAOSCTL="$fake" bash "$PAOS" doctor
  # 69 means "daemon unreachable" and callers branch on it. Collapsing it to 1 would make
  # an infrastructure problem indistinguishable from a command that ran and said no.
  [ "$status" -eq 69 ]
}

@test "a missing paosctl says what to do, not just command not found" {
  run env PAOSCTL=/nonexistent/paosctl bash "$PAOS" bus who
  [ "$status" -eq 127 ]
  [[ "$output" == *"cargo build --release"* ]]
}
