# Bus gotchas — the long version

Evidence and worked examples behind the short rules in SKILL.md.

### Gotchas

**Longest-lived footgun first: the shell eats your message body.** Backticks inside a
double-quoted `send` argument are **executed by the shell and their text silently deleted** —
`send` reports success and the peer receives a corrupted message. There are two safe forms;
prefer `--file` for anything long:

    paos bus send <room> --to @<name> --file ./report.md      # shell never sees the body

**Sending code / special characters / multi-line — pipe it via `-`.** A double-quoted
`paos bus send … "…"` whose text contains backticks, `$(…)`, `${…}`, or quotes is mangled by
your shell before `paos bus` sees it (symptoms: `parse error near }`, `command not found`, or
a silently empty send). **Pass `-` as the text and feed the body on stdin** — no temp file,
nothing for the shell to re-parse:

    printf '%s' 'message with `code`, $(cmd), ${vars} — literal' | paos bus send <room> --to @<name> -

    paos bus send <room> --to @<name> - <<'EOF'
    multi-line body,
    literal `backticks` and $(cmd) — heredoc is single-quoted so nothing expands
    EOF

Single-quoting the printf argument (or the `<<'EOF'` heredoc marker) keeps the body literal.
**Do NOT** write to `/tmp` and `cat` it back: `/tmp` is **not** sandbox-writable in the
an agent's sandboxed Bash, so the write fails, `$(cat …)` is empty, and you post a blank
message.
If you truly need a file, use `$TMPDIR` (`printf %s '…' > "$TMPDIR/m"; paos bus send <room> - < "$TMPDIR/m"`).
As a backstop, `paos bus send` now **refuses an empty/whitespace body** (exit 1) rather than
posting blank — so a failed read fails loudly instead of silently reaching the peer as noise.

**`status` is a latch — write only what you control.** It changes *only* when you write it,
so anything that can go stale without you acting ("armed", "reachable", "idle", a git sha of
"latest") **will be wrong exactly when a peer is deciding whether to escalate**. Orchestrators
have reported sessions stalled purely on a stale status. Put durable facts in it (a commit you
landed, the defect you own) and let `paos bus reachable` answer liveness — never claim
reachability in prose.

**A delivery can be capped.** A single delivery is limited to the newest ~25 messages
(`PAOS_BUS_BACKLOG_MAX`); urgent wakes and `operator` messages are never dropped. If older
unread messages existed you'll see `(N older message(s) … read them with: paos bus log <room>)`.
Nothing is lost — but **don't treat an old message as live**: an ancient PRIORITY-0 reads
exactly like a current one. Check timestamps before acting.

**Loop guards.** Never reply to your own messages (the bus filters them; don't defeat it).
On `@all stop` (or `@<you> stop`), leave the loop (optionally `paos bus leave`) and report to
your operator. After ~20 exchanges, checkpoint with your operator rather than looping
unattended — a legitimate long coordination can exceed 20 healthy exchanges, so don't kill
it; just don't loop forever.
