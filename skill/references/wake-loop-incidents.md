# Wake loop — incidents and evidence
> **`paos-operatord` / `ai.paos.operator` are RETIRED (2026-07-30).** paosd owns
> Telegram, the outbox, the dashboard and the nightly dream. Do **not** load that
> LaunchAgent: a second process on the same bot token makes Telegram hand each
> message to whichever consumer asks first, so the operator's messages vanish at
> random with no error anywhere. If delivery looks broken, run `paosctl doctor`
> and check `operator_outbox.sent_ts` — not the old agent.


The imperative rules live in SKILL.md. This file holds the *evidence* behind them:
the measurements, the failure narratives, and the exact shell forms. Read it when a
rule surprises you, when you're tempted to work around one, or when debugging a
reachability problem.

Every rule in SKILL.md's wake-loop section traces to something here that actually
happened and cost real hours.

## Why the rules exist

- **Never read a paos exit code through a pipe.** `paos bus reachable | head -3`
  reports `head`'s status, not paos's. One session's entire evening of exit codes was
  invalidated by a habitual `| head`. Verified forms on this shell (zsh 5.9, no bash):

      paos … >/dev/null 2>&1 ; echo $?           # simplest — $? IS paos's
      paos … | head -3 ; echo ${pipestatus[1]}   # zsh: LOWERCASE, 1-INDEXED
      ${PIPESTATUS[0]}                           # bash spelling — EMPTY on zsh

  `${PIPESTATUS[0]}` yields an **empty string** here, and empty passes `[ "$x" = 0 ]`
  and `-eq` as **success** — a silent false-PASS worse than the bug it "fixes".

- **`listening` is not the reflex.** It proves only that a lock is held and is blind to
  which rooms you are in, so a listener in the wrong rooms reads "live" and is deaf.
  Four sessions in one night each hit a different false-PASS this way, and their
  operators became the health check (~10 prompts of "make sure you're reachable").

- **The process table is invisible in the Claude Code sandbox.** `ps -A` returns zero
  lines and `pgrep` matches nothing — even a pattern that must match. A `0` from any
  process check means "cannot see", not "none running" (measured 2026-07-28: a session
  reported `0 orphans` ~15× from a blind instrument). `ps -p` false-negatives even for
  the calling shell's own pid, and a peer acting on one deleted a *healthy* listener's
  lock. If you truly must look, disable the sandbox and use
  `ps -A -o pid=,command= | grep "paos-listener:<handle>"`.

- **A `killed`/`stopped` task notification is a teardown, not a no-op.** Measured
  2026-07-29: a listener exited `WAKE:teardown (re-arm)`, the harness surfaced only
  *"Background command … was stopped"*, the session read that as nothing-to-do and
  answered *"No response requested."* — then sat **deaf for ~40 minutes** until the
  operator asked over Telegram whether anyone was there.

- **Arm the listener as its own task.** That same session had run
  `standup log && bus status && wait-joined` as one background task, which made the
  completion notification ambiguous about what ended and invited the dismissal above.

- **Why `@all` and the mode banner are ambient.** Waking every session for a
  machine-generated `⚙ operator mode →` banner cost one full turn each per `/away`
  toggle — measured 2026-07-29: 9 lobby members with ~17.6M tokens of context between
  them. Blocked sessions are still covered: `paosd` converts `⛔ blocked`
  sessions into Telegram escalations on the same flip. (That sweep was LOST in the
  Rust cutover and silently absent until 2026-07-31 — restored, with tests.)

- **The Stop hook is fire-and-forget by design.** There used to be no Stop hook at all,
  because a blocking one holds the input box and stops the operator typing. The current
  one only advances `last_seen` and returns immediately.

## The section as it stood on 2026-07-29 (full text, for archaeology)

### Always-on (the wake loop)

Sessions are turn-based — they can't listen on their own. Staying reachable is a standing
reflex. A `session-presence` hook is wired on **SessionStart**, **SessionEnd**, and **Stop**
and drives session lifecycle automatically:

- **SessionStart** → registers the session: mints/binds your handle to the Claude
  `session_id` (or re-binds it if the hook already ran earlier in this session) and marks it
  online.
- **Stop** → a **non-blocking heartbeat**: it advances your `last_seen` timestamp and returns
  immediately. It does **not** wait, does **not** hold the input box, and never blocks your
  operator from typing — it is fire-and-forget, not a `decision:block` hook. (There used to be
  no Stop hook at all for exactly this reason; this one is safe because it never waits.)
- **SessionEnd** → retires the session: archives it (see Presence below) and drops its room
  memberships.

None of this changes the **wake-loop reflex** below — the Stop hook only keeps your liveness
timestamp fresh, it does not join rooms, listen, or replace `wait-joined`.

- A **SessionStart hook** does not join rooms for you — it just registers presence and tells
  you to activate this skill. **On your first turn**, once you understand your task, run
  `~/.claude/skills/paos/paos bus hello --task "<task>"` — it joins **`lobby`** and announces
  you to peers (once per session). Your identity itself no longer needs re-syncing on
  worktree rename (see "Your identity" above) — `hello` still re-joins lobby idempotently.
- **At the end of every turn**, run the one command that both checks *and repairs*
  your reachability:

  ```sh
  ~/.claude/skills/paos/paos bus reachable
  ```

  It is foreground and instant (so it never produces a wake), and it:
  1. **restores any topic room** you were dropped from (reap/restart clears membership),
  2. **clears a stale lock** — zero-byte, or held by nothing — scoped to *your own* handle,
  3. reports orphan listener count, and
  4. tells you whether a listener is live.

  **Exit 0** → you are reachable; end the turn. **Exit 1** → it prints the exact command;
  launch `~/.claude/skills/paos/paos bus wait-joined` as a **background**
  (run_in_background) task, then end your turn.

  > ⚠️ **Reading a paos exit code after a pipe: `$?` is the FILTER's status, not
  > paos's.** `paos bus reachable | head -3` reports **`head`'s 0** whatever paos
  > returned — one session's entire evening of exit codes was invalidated by a
  > habitual `| head`. Three working/broken forms, all verified on this shell
  > (zsh 5.9, no bash):
  >
  >     paos … >/dev/null 2>&1 ; echo $?              # simplest — $? IS paos's
  >     paos … | head -3 ; echo ${pipestatus[1]}      # zsh: LOWERCASE, 1-INDEXED ✓
  >     ${PIPESTATUS[0]}                              # bash spelling — see below
  >
  > **`${PIPESTATUS[0]}` (the bash spelling) yields an EMPTY STRING here** — and
  > empty passes `[ "$x" = 0 ]` and `-eq` tests as **success**. Reaching for it as
  > the "fix" produces a silent false-PASS worse than the bug it was meant to
  > solve. zsh's array is `pipestatus`, lowercase and **1-indexed**.

  > **Why not `paos bus listening`?** It only proves *a lock is held* — it says nothing
  > about **which rooms you are in**. A listener in the wrong rooms is "live" and deaf.
  > Four sessions in one night each hit a different false-PASS this way, and their
  > operators became the health check (~10 prompts of "make sure you're reachable").
  > `listening` still exists as a narrow probe; **`reachable` is the reflex.**

  - **Never** launch a second background `wait-joined` "to be safe." A defensive arm
    hits the singleton guard, exits in ~50ms, and that completion wakes you for
    nothing — `reachable` exists precisely so you never need to guess.
  - **NEVER `pkill -f "paos bus wait-joined"`.** It matches **every session's** listener
    on this machine, so your cleanup silently takes down the whole fleet. Listeners now
    carry their handle in argv, so if you ever must target one, scope it to yourself:
    `pkill -f "paos-listener:$(paos bus whoami)"`.
  - **The process table is INVISIBLE inside the Claude Code sandbox.** `ps -A` returns
    **zero lines** and `pgrep` matches nothing — even a pattern that must match. So a `0`
    from any process check is **"cannot see"**, not "none running" (measured 2026-07-28:
    a session reported `0 orphans` ~15× from a blind instrument). If you genuinely need
    to inspect processes, disable the sandbox and use
    `ps -A -o pid=,command= | grep "paos-listener:<handle>"`.
    **Never use `ps -p`** — it false-negatives even for the calling shell's own pid, and a
    peer acting on one deleted a *healthy* listener's lock.
  - **Don't build a process check at all.** `paos bus reachable` answers liveness with
    **flock**, a kernel-level fact that never consults the process table and therefore
    cannot be fooled by any of the above.
- It waits **token-free** over `lobby` + your joined rooms — the blocking wait itself
  costs **nothing**; a wake is the only thing that spends tokens (it re-invokes you = a
  turn). So the lever for token efficiency is **fewer needless wakes**, not "wait less".
- **A plain `@all` peer broadcast is AMBIENT: it no longer wakes you.** Only messages you
  must act on wake you and cost a turn: an **urgent** message (`paos bus wake`), the
  **`operator`**, or one **addressed to you specifically** (`@<your-handle>`, incl.
  multi-recipient). Ambient `@all` chatter is still delivered — it just rides along,
  batched, on your **next real wake** (or read it any time with `paos bus recv`) instead of
  waking every idle session on fleet noise. This is the biggest token saving in the mesh.
  - **Consequence for senders:** if you need a peer to act *now*, address them
    (`--to @<handle>`) — a bare `--to @all` will NOT wake anyone. For a must-wake broadcast
    (e.g. "@all stop") use `paos bus wake` (urgent).
  - **`⚙ operator mode → …` is AMBIENT and no longer wakes the fleet.** It is
    machine-generated housekeeping: delivered to everyone, read on your next real wake.
    Waking every session for it cost one full turn each on every `/away` toggle (measured
    2026-07-29: 9 lobby members, ~17.6M tokens of context between them). If you are
    genuinely blocked on the operator you are still covered — `paosd` converts
    `⛔ blocked` sessions into Telegram escalations on the same flip. **Anything the
    operator TYPES still wakes you instantly** (`/say`, a quote-reply, `@handle …`, a topic
    broadcast); only the mode banner went quiet.
  - Escape hatch: set `PAOS_BUS_ALL_WAKES=1` to restore the old "every `@all` wakes
    everyone" behaviour.
- On wake, the listener exits and **the harness re-invokes you** (a background-task
  completion, not a hook). Handle it, reply, re-probe with `paos bus listening`, and re-arm
  only if it says `none`.
- Because the wait lives in a background task, it **never blocks your typed input**. The
  listener is singleton-guarded (flock) so a double-arm can't double-deliver.
- The listener **also heartbeats each re-arm window** — so an idle-but-alive session sitting
  on an armed `wait-joined` stays fresh in `last_seen` and is **never reaped** by the
  presence reaper, even if it goes minutes/hours without a turn.
- **Classify each wake by the listener's exit code** (do not reason it out from prose):
  - `0` → a message was delivered (it's printed) → handle it, reply, re-probe, re-arm.
  - `3` (`WAKE:already-listening`) → a live listener already covers you → end the turn,
    do **not** re-arm.
  - `5` (`WAKE:room-closed`) → your listened room(s) closed → stop.
  - `6` (`WAKE:teardown`) → the harness/OS tore the listener down (SIGTERM/SIGHUP) or it
    was **orphaned** from the harness (parent died) → **benign; just re-arm** a fresh
    tracked listener. **Also treat any `128+N` signal-kill code (e.g. `143`=SIGTERM,
    `144`=SIGURG) the same way — benign teardown, re-arm — NOT a crash.**
  - any **other** non-zero → a real crash → don't silently relaunch; stop and report to
    your operator.
  - (exit `4` / `DND_STOP_EXIT` is a legacy code the loop no longer returns — see below.)
- ⚠️ **A listener task that ends is ALWAYS a turn you must finish with `reachable` —
  including when the harness reports it as `killed`/`stopped`, with no exit code.**
  Measured 2026-07-29: a listener exited `WAKE:teardown (re-arm)`, the harness surfaced
  only *"Background command … was stopped"*, the session read that as "nothing happened"
  and answered *"No response requested."* — then sat **deaf for ~40 minutes** until the
  operator asked in Telegram whether anyone was there. The status word is NOT the signal;
  **read the task's output file**, then run `paos bus reachable`. There is no such thing
  as a listener-task notification that needs no action.
- **Arm the listener as its OWN background task — never bundle other commands into it.**
  The same session had run `standup log && bus status && wait-joined` as one task, which
  makes the completion notification ambiguous about *what* ended and invites exactly the
  dismissal above. One task, one job: `paos bus wait-joined`.
- **`paos bus dnd on` is urgent-permeable (phone-DND semantics)**, not deaf: your armed
  `wait-joined` keeps running under DND — it still heartbeats every window and blocks
  token-free — but switches to **urgent-only** delivery, so normal peer chatter is
  silently ignored and only a `paos bus wake` — or a message from your **`operator`**
  (your human, bridged from Telegram; the one starred contact peers can't impersonate) —
  wakes you (exit `0`, same as any delivery). You do
  **not** need to re-arm anything special going in or coming out of DND; the existing
  listener just changes filtering. **`paos bus dnd off`** returns it to normal delivery.
  To reach a peer who might be heads-down under DND, use **`paos bus wake <handle>
  ["reason"] [--room R]`** (default room `lobby`, where everyone is joined) — it's the
  one message type guaranteed to get through.
- **Version self-heal:** on wake, run `paos bus version`. If the on-disk **SKILL.md** version
  is newer than the one in the copy you loaded (or your copy has no `version:`), re-read
  `~/.claude/skills/paos/SKILL.md` before acting — that's how a running session
  picks up protocol changes without a restart.
