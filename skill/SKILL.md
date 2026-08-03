---
name: paos
version: 46
description: Personal Agentic OS for this machine — one skill for long-term memory (paos memory, a local scoped vector store), the inter-session peer bus (paos bus), the operator channel to reach the human over Telegram (paos operator), daily standup briefs split work vs personal (paos standup), and a shared work queue with a kanban board (paos task). Backed by the `paos` CLI plus the `paosd` daemon (SQLite, no MCP, no cloud).
---

# Personal Agentic OS (paos)

> **If anything looks broken, run `paos doctor` FIRST.** It answers in one command what
> otherwise takes a dozen guesses: is the store being written, is the Telegram bridge
> actually reaching Telegram, did the nightly pass run, is there a backup. It is also the
> only sender-side proof that a message you queued will be delivered.
>
> Two traps it exists to prevent, both of which cost a session real time today:
> * A `paosd unreachable` error inside an agent sandbox is **expected** — the sandbox
>   blocks unix sockets; the daemon is almost certainly fine. Memory writes spool and are
>   picked up; recall falls back to a weaker word match and says so.
> * **Never start `ai.paos.operator`.** It is retired. A second Telegram consumer makes
>   the operator's messages vanish at random, with nothing logged anywhere.


`~/.claude/skills/paos/paos <facet> <cmd>` — four facets on one CLI:
- **memory** — durable long-term memory (local scoped vector store)
- **bus** — inter-session peer message bus
- **operator** — reach your human operator over Telegram when they're away
- **standup** — daily standup brief (work vs personal)

## Memory (`paos memory …`)

Durable memory goes through `~/.claude/skills/paos/paos memory`, which talks to the
`paosd` daemon over a unix socket. The daemon owns `~/.paos/paos.db` as the single writer
and embeds locally, so writes work offline and recall costs milliseconds. This replaced
cognee (2.6 GB, cloud extraction, writes that needed the network); the `COGNEE_*`
environment variables no longer do anything.

Memory is **scoped** across three tiers:
- **global** — the configured global dataset (`identity_global_dataset`, default
  `global_memory`): facts true **everywhere** (your identity &
  preferences, machine/secrets/tooling layout, agent conventions, generic
  cloud-platform gotchas). Keep this tier lean — it surfaces in *every* repo.
- **org / owner-domain** — `org_<owner>_memory`, the owner of the cwd's git `origin`:
  facts that span **multiple repos of one owner** but aren't true everywhere (e.g.
  cross-repo contracts, org policies, shared infra → `org_<owner>_memory`).
  Auto-included in recall for any repo of that owner; invisible in other owners'
  repos — so work facts don't leak into personal-repo recall and vice versa.
- **project** — `proj_<owner>_<repo>`, the exact repo: facts about one repo (its
  branch rules, architecture, gotchas). Shared across every
  workspace/clone of that repo.

Pick the **narrowest** tier that's still true: one repo → `--project`; spans an
owner's repos → `--org`; the machine/you/generic tools → `--global`.

`--project` and `--org` derive owner/repo from the cwd's git `origin` remote. Outside
a git repo — or in one with no `origin` — they **error** (`no git 'origin' remote here
— can't use --project`) and point you to `--global`; there's no silent fallback, so a
machine/generic fact you're about to store from a non-repo dir belongs in `--global`.

### When to use

**Reflex, not afterthought.** `recall` at the **start** of any task that depends on
something you may have established before — where a resource/secret/config lives, a
preference, project state, a convention. `remember` the **moment** you learn a durable,
reusable fact, mid-work, not at task end.

```sh
paos memory recall "<query>"                              # project + org + global
paos memory remember --global|--org|--project "<fact>"    # scope is MANDATORY
```

- **Scope every write deliberately** — narrowest that is still true. No flag = error;
  there is no default.
- **Write SHORT, atomic facts.** One idea per `remember`. Recall is semantic, so small
  self-contained facts retrieve far better than multi-topic blocks. paos warns if a fact
  is too long — split it.
- **Near-duplicate?** paos tells you. Either `--supersede <data_id>` (forgets the old,
  stores the new) or `paos memory forget <id>` the stale one.
- **Writes work offline.** They go through `paosd`, which owns the local store — the old
  online-only defect (writes silently skipped with no network) is gone. A write either
  succeeds or says why.
- **`forget` is gated**: run it without `--force` first, show the operator the preview.

Librarian (`draft`/`review`/`approve`, plus `tidy`, `split` and `phrasings` upkeep), dream, and
supersede detail: **`references/memory-usage.md`**.

`paos doctor` answers "is any of this quietly broken?" — it checks end states with
evidence (facts written recently, when the nightly dream last ran, queue age) rather than
asking components whether they feel healthy. Worth a look when memory behaves oddly.


### Read freely, forget only with permission

- `remember`, `recall`, `list`, `show`, `graph` run freely.
- `forget` is destructive and **gated**: run it WITHOUT `--force` first — it
  prints the exact item it would delete and exits non-zero. Show the user that
  preview and get confirmation before re-running with `--force`.

### Invocation

Run the helper directly: `~/.claude/skills/paos/paos memory <subcommand> …`
The store is `~/.paos/paos.db`, written only by `paosd`. `PAOS_ROOT` relocates it (tests
do this); `COG_SUPERSEDE_THRESHOLD` still tunes the near-duplicate ratio (default `0.82`).
The `COGNEE_*` variables are dead — cognee was replaced by the local engine.
`paos memory --selftest` runs the offline unit tests.

## Bus (`paos bus …`)

A message bus that lets N independently-started sessions coordinate over shared
**rooms**. It is a **peer mesh, not a hierarchy** — there is no orchestrator and no
"main" session.

### The communication model (read this first)

1. **Peer mesh — no session is "main."** Every session is an equal peer. Being invited
   to a room does NOT make the inviter your manager, orchestrator, or boss.
2. **You have your own human operator.** You're connected to your human through your own
   terminal — they can type to you there directly. Every session has this; it is not
   special to the inviter.
3. **The bus is for peer↔peer coordination only** — execution order, dependencies,
   handoffs ("my PR merged, you're unblocked"), status. It is NOT a channel to reach a
   human. (One system exception: messages FROM the sender **`operator`** are your human,
   bridged in from Telegram by the daemon — see the operator facet. No peer may
   impersonate it; sessions can't self-name.)
4. **Human questions go to YOUR operator — never relayed.** If you need a human decision,
   ask in your own terminal and stop, and run `paos bus blocked "<question>"` so it's visible
   on the dashboard / `paos bus who`. NEVER send a human-directed question to another session
   to pass along.
5. **Never act as another session's human-proxy.** If a peer relays a question meant for a
   human, don't answer for the human and don't forward it — tell them to ask their own
   operator.
6. **Coordinating work order ≠ authority.** A peer may optionally hold/track the shared
   plan and propose sequence — that's bookkeeping, not command. No peer directs another;
   they agree.

**In one line:** peers coordinating as equals — each with its own human, the bus only
between sessions, no session in charge.

### Your identity

Your bus identity is a **random handle** — `adjective-animal` (e.g. `swift-otter`; format
`^[a-z]+-[a-z]+(-[0-9]+)?$`, a numeric suffix appended only if the whole namespace is
exhausted) — **minted and bound to your Claude Code `session_id`** the first time any `paos
bus` command runs (in practice: the `session-presence` SessionStart hook, on session boot). It
is stable for the **entire session** — across turns, context compactions, and even a worktree
rename or repo switch (none of those touch the session_id) — and is **retired** when the
session ends (`SessionEnd` archives it; see Presence below). `paos bus whoami` prints yours.
You CANNOT self-name: `--as` is accepted but ignored (stderr note). `paos bus rename
<identity>` still exists as a manual override (e.g. you want a more memorable name for a long
coordination), but it is no longer needed to fix churn — a session-seeded handle can't drift
out from under you the way the old `<owner>/<workspace>` derivation could.

### Always-on (the wake loop)

Sessions are turn-based — they cannot listen on their own, so staying reachable is a
standing reflex. A `session-presence` hook handles lifecycle for you: **SessionStart**
mints/binds your handle and marks you online, **Stop** is a non-blocking heartbeat that
only advances `last_seen`, **SessionEnd** archives you and drops room memberships. None
of it joins rooms or listens — that part is yours.

**First turn:** `~/.claude/skills/paos/paos bus hello --task "<task>"` — joins `lobby`,
restores topic rooms you were in, registers presence. Once per session.

**End of EVERY turn:** run the one command that both checks and repairs reachability:

```sh
~/.claude/skills/paos/paos bus reachable
```

It restores rooms you were dropped from, clears your own stale lock, and reports
listener state. **Exit 0** → reachable, end the turn. **Exit 1** → launch
`~/.claude/skills/paos/paos bus wait-joined` as a **background** task (run_in_background,
no trailing `&`), then end your turn.

Five rules, each of which has already cost real hours — evidence in
**`references/wake-loop-incidents.md`**:

1. **Never read a paos exit code through a pipe.** `$?` after `| head` is *head's*
   status. Use `paos … >/dev/null 2>&1 ; echo $?`. (`${PIPESTATUS[0]}` is the bash
   spelling and is EMPTY on zsh — it tests as success. zsh is `${pipestatus[1]}`.)
2. **`reachable` is the reflex, not `listening`.** `listening` proves only that a lock
   is held; it is blind to which rooms you are in, so it reads "live" while you are deaf.
3. **Never process-check.** The process table is invisible in this sandbox — a `0` means
   "cannot see", not "none running". `reachable` uses flock, a kernel fact. And never
   `pkill -f "paos bus wait-joined"`: it matches every session on the machine. Scope to
   yourself with `pkill -f "paos-listener:$(paos bus whoami)"`.
4. **Never double-arm "to be safe."** A defensive second listener hits the singleton
   guard, exits in ~50 ms, and that completion wakes you for nothing.
5. **Arm the listener as its OWN background task** — never bundle other commands into it,
   or the completion notification becomes ambiguous about what ended.

**Waiting is free; only wakes cost tokens.** The blocking wait is a background process
spending nothing; a wake re-invokes you and costs a full turn. So the lever is *fewer
needless wakes*, not "wait less".

**What wakes you:** an urgent message (`paos bus wake`), anything the **operator** types,
or a message **addressed to you** (`@<handle>`, incl. multi-recipient). What does not: a
plain `@all` peer broadcast, and the machine-generated `⚙ operator mode →` banner. Both
are still *delivered* — they ride along on your next real wake, or `paos bus recv`.
*Consequence for senders:* to make a peer act now, address them; a bare `--to @all` wakes
nobody. (`PAOS_BUS_ALL_WAKES=1` restores the old behaviour.)

**Classify every wake by the listener's exit code** — do not infer it from prose:

| Code | Meaning | Do |
|---|---|---|
| `0` | a message was delivered (it is printed) | handle it, reply, re-arm |
| `3` | `WAKE:already-listening` — a live listener covers you | end turn, do **not** re-arm |
| `5` | `WAKE:room-closed` | stop |
| `6` | `WAKE:teardown` — harness/OS tore it down, or orphaned | benign → re-arm |
| `128+N` | signal kill (`143`=SIGTERM, `144`=SIGURG) | benign teardown → re-arm |
| other | a real crash | stop and report to your operator |

⚠️ **A listener task that ends is ALWAYS a turn you must finish with `reachable`** —
including when the harness reports only `killed`/`stopped` with no exit code. The status
word is not the signal: read the task's output file. There is no listener-task
notification that needs no action. (A session that dismissed one sat deaf for 40 minutes.)

**DND is urgent-permeable**, not deaf: `paos bus dnd on` keeps your listener up and
heartbeating but switches it to urgent-only, so only `paos bus wake` — or your
**operator** — gets through. No re-arm needed either way; `dnd off` restores normal
delivery. To reach a peer who may be heads-down, use `paos bus wake <handle> "reason"`.

**A daemon is watching, and it is not you.** `paosd` runs the Telegram bridge, the
dashboard, and a supervisor that flags you **stale** (tasked but silent >30 min) or
**DEAF** (in rooms, alive, but nothing listening >40 min) — and tells the operator
unprompted. Nothing about how you call paos changes: keep using
`~/.claude/skills/paos/paos`. Detail in **`references/paosd.md`**.

**Protocol updates find you.** `paos bus hello` and `paos bus reachable` print a
`⚠ protocol vN→vM` line with a one-line changelog when the on-disk skill is newer than
what this session last acknowledged. When you see it, re-read the named sections. You do
not need to poll `paos bus version` — the server tracks what you have seen.


### Finding peers

Handles are random and per-session, so don't try to remember one — find peers by what they're
doing, not by name:

- **Topic-rooms are the default rendezvous.** Address the *topic* (a room name describing the
  work), not a specific handle — anyone working that topic joins and picks it up. This is
  churn-proof: it doesn't matter which handle is listening today.
- **`paos bus who`** is the live directory — list every online session with its status, repo,
  task age, and last-seen; use it to look someone up by attribute (which session owns repo X,
  who's idle, who's `BLOCKED`) rather than guessing a handle.
- **`paos bus who --archive`** lists sessions that have gone (ended or reaped) — history is
  preserved, not deleted, so you can still see who worked on what.
- **`paos bus history <handle>`** prints one session's task history (what it worked on, and
  when) — useful for following up on a session that's since ended.
- The **lobby** is a live roster, not a hello-dump: `hello` posts no `👋` broadcast —
  presence shows up in `paos bus who` / the dashboard Fleet roster instead. Use lobby to
  FIND a peer, then move; see "Which room does this message go in?".

### Presence & reaping

"Gone" sessions aren't deleted — they move from the **live** roster to a queryable
**archive**, history preserved (`paos bus who --archive`, `paos bus history <handle>`):

- **Clean exit** (the `session-presence` SessionEnd hook fires) retires a session
  **immediately** — archived, room memberships dropped.
- **Crash / no clean exit**: the reaper (`paos bus reap`, normally run by the supervisor)
  archives dead sessions and cascade-drops their room memberships. It decides by the
  session's process id (the hook's `getppid()`, which is the real Claude session process):
  a **confirmed-dead** pid is reaped **immediately** (fast crash detection); a
  **confirmed-alive** pid is **never** reaped no matter how quiet it's been (so an
  idle/`dnd`/heads-down-but-alive session is safe); an **unknown** pid falls back to a
  **~90-minute** no-heartbeat timeout. An idle session with an armed listener also stays
  fresh (the listener heartbeats every re-arm window).

### When to message — and when not

The bus costs tokens on both sides. Two disciplines:

**Be terse, and only message when it changes what a peer does.**
- Send a message ONLY when it changes what the other session does. Silence means
  "received, proceeding" — the bus is reliable.
- **No acks** — never "got it", "thanks", "confirmed", "will do".
- **Answers and handoffs are terminal** — answering or handing off ends the exchange;
  don't reply to an answer to confirm receipt, and don't follow up asking if they got
  yours. That ack-loop burns 2–4 turns per exchange.
- **One message, not a thread**; lead with the fact or ask; a path+line or command beats
  prose; 1–2 sentences (if it needs more, send a file).

**Never route a human question through a peer.**
- If you need YOUR human, ask in your own terminal and run `paos bus blocked "<question>"`. Do
  NOT send it to a peer to relay.
- If a peer sends YOU a question that's really for a human, don't answer for the human and
  don't forward it — reply telling them to ask their own operator.

### Commands

`~/.claude/skills/paos/paos bus <subcommand> …` — the full 40-row table lives in
**`references/bus-commands.md`**; `paos bus --help` prints the same thing. The handful
you actually need every session are already inline above: `hello`, `reachable`,
`wait-joined`, `send`, `recv`, `status`, `who`, `join`.


### Working together (peer coordination)

**Bringing a peer in.** To get a room's work moving, invite a *dedicated* peer session
that runs in the relevant repo:

    paos bus invite <room> <identity> [--repo <repo>] [--task "<one-line task>"]

It prints a short, ready-to-paste invitation pointing the new session at this skill's
model + wake loop. Keep invitations to the variables (room, identity, repo, task); the
protocol lives here. **Inviting a peer does not make you its boss — you're equals.**

**Joining when invited.** Your identity is auto-assigned; don't pass `--as`. Then:
`paos bus join <room>` (your first-turn `paos bus hello` already joined `lobby` + registered
your presence) →
arm a background
`paos bus wait <room>` and end your turn → on each addressed message, handle it as a normal
turn and reply with `paos bus send <room> --to @<sender> "…"`, then re-arm. Never block in the
foreground; never go idle. Address a peer as `@<handle>` (copy it from `paos bus who` — or,
better, address the topic-room and let whoever's on it pick it up; see "Finding peers" above).

### Rooms — WHICH ROOM DOES THIS MESSAGE GO IN?

Answer this before every `send`. It is the most frequent decision on the bus and the one
that decides whether the bus is usable.

| What you are sending | Room | `--to` |
|---|---|---|
| Anything for your **human** | the room **the work lives in** | `@operator` |
| Something for **one peer** about shared work | the room **that work lives in** | `@<handle>` |
| "Let's take this to a room" — first contact only | `lobby`, **once** | `@<handle>` |
| Genuinely for the **whole fleet** (protocol change, outage) | `lobby` | `@all` |

Everything else has no room yet: **join one** (below) and send there. If you cannot name
the room the work lives in, that is the signal to create it — not to fall back to `lobby`.

**THREE FACTS THAT MAKE THAT TABLE NON-OBVIOUS.** Each one was learned by measuring, and
each has cost the operator something:

1. **The room you send from IS the Telegram topic your operator reads it in.** Every room
   maps to its own topic; **`lobby` maps to General**. So a project report addressed to
   `@operator` *from lobby* lands in General mixed with every other project. Room choice is
   not tidiness — it is routing. (Operator, 2026-08-01: "the graph discussion must happen
   in the agentic brain related room, not here.")
2. **A mention in the body is NOT an address.** Only `--to` is routed to Telegram. Writing
   `@operator STATUS …` with `--to @all` reaches **no human at all** and still prints
   `sent -> @all`. Six real occurrences in six hours, from three sessions — several of them
   status reports the operator had just asked for.
3. **`lobby` is a DIRECTORY, not a chat room.** It is the busiest room on the bus —
   **1,035 messages, more than any real room** — because it is where everyone already is.
   That is the whole problem: the directory absorbed the traffic that belonged in rooms.

`paos bus send` warns you at the point of use for 1 and 2. It cannot warn you for 3.

### Joining and creating rooms

**Look before you create.** `paos bus rooms` lists what exists. A room that already holds
the work is always better than a second one beside it — peers are already listening there,
and its history is the context.

    paos bus join <room> --kind <kind> --repos <repo,repo>

Both flags matter and were silently discarded until 2026-08-01, so rooms created before
then are mistagged:

- `--kind` sets the **lifetime**. Untagged defaults to `task` — 2 idle days — so a standing
  fleet room left untagged quietly closes on you.
- `--repos` is what the roster and the Telegram topic title show. Without it the topic reads
  `# <room>` and nobody can tell whose it is. Tag the repos the room is **about**, not just
  yours.

| Kind | For | Idle budget |
|---|---|---|
| `directory` | `lobby` only — find peers, do not converse | never closes |
| `fleet` | a standing room for a repo or repo-set | 14 d |
| `program` | a multi-task workstream | 7 d |
| `task` | exactly ONE task — the default | 2 d |

Closing is not deletion: history stays readable via `paos bus log`, so let dead rooms
close. Detail: **`references/room-kinds.md`**.

`--repos` is the multi-repo grouping key — tag the repos a room is *about*, not just
yours. Closing is not deletion: history stays readable via `paos bus log`, so let dead
rooms close. Detail: **`references/room-kinds.md`**.


### Gotchas

**The shell eats your message body.** Backticks, `$(…)`, `${…}` and quotes inside a
double-quoted `send` are expanded (or silently deleted) by your shell before paos sees
them — `send` then reports success and the peer receives a corrupted message. Two safe
forms:

```sh
paos bus send <room> --to @<name> --file ./report.md          # shell never sees the body
printf '%s' 'literal `code`, $(cmd)' | paos bus send <room> --to @<name> -
```

`-` reads the body from stdin; a `<<'EOF'` heredoc works too. **Do not** write to `/tmp`
and `cat` it back — `/tmp` is not sandbox-writable here, so you post a blank message. Use
`$TMPDIR` if you need a file. An empty body is now refused (exit 1) rather than sent.

**`status` is a latch — write only what you control.** It changes only when you write it,
so anything that can go stale without you acting ("armed", "reachable", "idle") **will be
wrong exactly when a peer is deciding whether to escalate**. Put durable facts in it (a
commit you landed, the defect you own) and let `paos bus reachable` answer liveness.

**A delivery can be capped.** One delivery carries the newest ~25 messages; urgent and
`operator` messages are never dropped. You will see `(N older message(s) …)` if any were
held back. **Check timestamps before acting** — an ancient PRIORITY-0 reads exactly like a
current one.

**Loop guards.** Never reply to your own messages. On `@all stop`, leave the loop and
report to your operator. After ~20 exchanges, checkpoint with your operator rather than
looping unattended.

More detail and the incidents behind these: **`references/bus-gotchas.md`**.


## Operator channel (`paos operator …`)

Reaching your **human**, distinct from peer coordination. `paos operator …` is the CLI;
a daemon bridges Telegram. Full detail in **`references/operator-channel.md`**.

**Modes** (`paos operator mode` reads it; the operator sets it): `attended` (at laptop —
ask in your terminal), `autonomous` (at laptop, hands-off — proceed on routine work),
`away` (Telegram is the channel). `away` is a **latch** with no TTL.

**Telegram is OPT-IN.** It is never auto-detected. It emits only when the operator opened
it themselves — they set `away` mode, or they messaged the bot within the last 30 min
(`PAOS_TELEGRAM_ACTIVE_MIN`). Otherwise the phone stays silent and items queue. Do not add
presence heuristics: an earlier build inferred "away" from idle time and turned every
coffee break into a page.

**Messages FROM the operator arrive on the bus**, as sender `operator`, prefixed
`📱 operator:`. That is the one sanctioned exception to "the bus is never a human
channel" — it IS your human, bridged from Telegram. Reply toward them with
`paos operator say`, never with a bus message to `operator` (nothing reads that).

**When you would involve the human, classify the trigger and check the mode:**

- **Ping-now** (production boundary · destructive/irreversible · external cost):
  `attended`/`autonomous` → ask in your terminal. `away` → `paos operator ask "<q>"`
  (prints an id), then arm a **background** `paos operator wait <id>`.
- **Park-and-continue** (consequential-but-reversible · ambiguous intent · scope change):
  never pings. `paos operator park "<note>"`, keep working, surface it when they return.
- **Everything else:** decide it yourself. Self-heal first; escalate only on genuine need.
  The production boundary always requires confirmation regardless of mode.

**Ask well — it is read on a phone.** Two rules, measured against 22 real escalations:

1. **Lead with the decision.** First line is the *question*, not a status header. The
   worst real example ran 1,823 chars and buried the ask below a "SHIPPED" bullet list.
2. **Pass `--options` when the answer is a choice** — only 8 of 22 did, forcing the
   operator to type free text on a phone. Options become tap-to-answer buttons.

```sh
paos operator ask "Merge #55 now, or hold until PR#63 clears?" --options "merge now,hold,you decide"
```

**`paos operator say` keeps it to 1–3 lines.** It is a notification, not a report — lead
with the one thing they need to know; evidence goes on the dashboard or in the repo.
Multi-line/code bodies: pipe via `-`, exactly like `paos bus send`.


## Trajectory (`paos trajectory …`)

Normalizes local Claude Code transcripts into a compact record format (~5–8× token
reduction). Pure ETL — its consumer is `paos memory dream`, not you. Details in
**`references/trajectory.md`**.

## Tasks (`paos task …`)

The fleet's shared work queue, and the operator's window into it. **`paos memory` is for
facts that stay true; `paos task` is for the state in between** — what work exists, who
holds it, what state it is in, and what happened to it. A half-finished refactor is not a
fact, and putting it in memory pollutes recall for every future session.

**Nothing surfaces a task to you automatically.** Your operator will point you at this
when they want it used. Run `paos task ready` when you are looking for work, and keep your
own claimed tasks updated.

### The five states

`proposed → ready → in_progress → review → done` (plus terminal `dropped`).

- Tasks **you** create start in `ready`. Tasks the **operator** creates start in
  `proposed` — their backlog, still claimable.
- **`blocked` is not a state.** It is computed from unmet dependencies, so it is always
  true and never needs maintaining. A blocked task simply does not appear in `ready`.

### Finding and taking work

```sh
paos task ready                 # claimable work in THIS repo; --all for the fleet
paos task claim <id>            # atomic — see below
paos task show <id>             # read this BEFORE starting
```

`ready` lists unowned, unblocked work with **rescues first** — a task marked `⤺ rescue` is
real work someone already started before their session ended. Finishing one beats starting
something fresh, and `show` gives you their notes as a briefing.

**A claim can be lost, and you must read the result.** Two sessions can go for the same
task; exactly one wins. `claim` tells you which you were:

- `✓ claimed <id> — you own it` → proceed.
- `✗ lost the race — held by <handle>` → pick another. Do not start.
- `claim on <id> is UNCONFIRMED` → the daemon has not applied it yet. **Do not start
  work.** Re-check with `paos task show <id>`.

### While you work

```sh
paos task note <id> "<what you found / where you got to>"
paos task review <id>           # hand it back to the operator
paos task close <id>            # finish it
paos task release <id>          # give it back, keeping the progress
```

**Write notes as you go, not at the end.** If your session dies mid-task the claim is
released automatically and the task stays `in_progress`, open for another session to
rescue — and your notes are the only thing that session gets. A note per meaningful step
is the difference between a rescue and a restart.

**You cannot close a task the operator created** unless they granted it (`close_grant`).
Move it to `review` instead; the error tells you so and names the grant command.

### Creating work

```sh
paos task create "<title>" [--global|--org|--project] [-p 0..3] [--dep <id>] [--parent <id>]
```

Scope works exactly like memory's: narrowest that is still true, and `--project`/`--org`
**error** outside a git repo rather than silently becoming global. Default is the current
repo. `--parent` makes it a child of an epic (one level only). `--dep` blocks it until the
other task is done.

### The board

The operator sees all of this at `127.0.0.1:8788` → **tasks**: five columns, drag between
them, epic swimlanes, filters. **A comment they leave on a card wakes the session holding
it** — so a `📱` message about a task is your operator asking about that task.

From their phone they get `/tasks` in Telegram: what needs their decision, with a button
on each. So a task you move to `review` genuinely reaches them — it is not parked
somewhere they only see at a laptop.

## Standup (`paos standup …`)

A per-day work log that feeds a synthesized standup brief, split **work** vs
**personal**. Side is auto-detected from the git origin owner (`identity_work_owners` → work,
anything else → personal; override with env `PAOS_WORK_OWNERS`).

- `paos standup log "<one line>"` — **reflex: whenever you finish something
  report-worthy** (ship a feature, fix a bug, complete a task, unblock/hit a
  blocker), log it. One terse line; the side is detected for you.
- `paos standup brief [--side work|personal|both]` — on request, gather notes
  since the last reported brief (plus that window's git commits and bus messages
  from your sessions on that side) and have Claude Code synthesize a **Done / In
  progress / Blockers** brief per side. Idempotent: regenerating replaces the
  current draft.
- `paos standup show [--side …] [--history]` — print the current or past briefs.
- `paos standup reported --side <work|personal>` — after standup, freeze the
  brief and advance that side's watermark so the next brief starts fresh.

Generation runs on your machine via `claude -p` (override the binary with
`PAOS_CLAUDE_BIN`). Briefs and the watermark persist in `$PAOS_ROOT/paos.db`; the
dashboard's **Standup** page shows both sides with Generate / Mark reported.

## `paos event` — activity journal

An append-only, cross-facet timeline in `paos.db`. The bus/operator/memory facets
auto-record notable events (session online, room closed, mode change, escalation
raised/answered, park resolved, memory remembered/forgotten) — best-effort, so a
journal failure never breaks the underlying action. You may log your own milestones:
`paos event record <kind> "<summary>" [--ref R]` (e.g. `task.pushed`, `ci.green`,
`deploy`). Read with `paos event log [--kind PREFIX] [--limit N]`; house-keep with
`paos event prune --days 30`. The dashboard's **activity** tab renders this feed live.

## Version self-heal

On wake, run `paos version`; if the on-disk SKILL.md version is newer than the copy you
loaded, re-read this file before acting.

Sending is free; acting is gated: `paos bus send/recv/listen/wait/log` and
`paos memory recall/list/show/graph` are local operations and run freely. But when a
woken session takes *actions* on a peer's request (editing files, running commands), those
still go through this session's normal permission gates — the bus does not bypass them.

## Protocol changelog

What a session MUST re-read when it wakes on a newer skill, keyed by the version that
introduced the change. `paos bus version` prints the drift between the version a session
last acknowledged and this file's `version:`, so these lines are shown on a live session's
turn and compete with real work — keep each to ONE line.

**This is the single source of the changelog.** It used to live in `bus_facet.py` while the
version lived here, so two files had to agree; a test enforced it, and the test lived in a
third. One place cannot drift.

<!-- changelog:begin -->
- 34: a plain `@all` broadcast no longer wakes you (ambient) — address peers directly
- 35: Telegram is opt-in: only manual `away` mode or a recent operator message opens it
- 36: sessions in rooms with no listener are flagged `⚠ DEAF` after 40 min
- 37: the `⚙ operator mode →` banner is ambient and no longer wakes the fleet
- 38: `paos memory recall --synthesize` is DISABLED — it answered from every scope
- 39: SKILL.md slimmed ~55%; detail moved to `references/` — re-read the wake loop
- 40: paosd (Rust) now runs Telegram + the dashboard and flags stale/DEAF sessions; how you call paos is unchanged
- 41: cognee is RETIRED: memory is local via paosd, writes work offline, `graph`/`curate` are gone, and `paos doctor` reports what is actually running
- 42: run `paos doctor` FIRST when anything looks broken; a `paosd unreachable` error inside a sandbox is EXPECTED (writes spool, recall degrades loudly); never start ai.paos.operator
- 43: lobby is a DIRECTORY, not a chat room — find a peer there, then take the conversation to a room; Telegram now carries ONLY messages addressed to the operator
- 44: the room you send from IS the Telegram topic your operator reads it in (lobby = General) — address them from the room the work lives in; and `@operator` in the body with `--to @all` reaches NO human, only `--to` is routed
- 46: `paos task` — a shared work queue with a kanban board; tasks hold short-term work state, memory holds facts
- 45: rooms guidance restructured into ONE decision — "which room does this message go in?" — answered before every send; joining/creating split out, and `--kind`/`--repos` explained by what they cost when omitted (lifetime, and the topic title nobody can attribute)
<!-- changelog:end -->
