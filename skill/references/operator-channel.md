# Operator channel — full detail
> **`paos-operatord` / `ai.paos.operator` are RETIRED (2026-07-30).** paosd owns
> Telegram, the outbox, the dashboard and the nightly dream. Do **not** load that
> LaunchAgent: a second process on the same bot token makes Telegram hand each
> message to whichever consumer asks first, so the operator's messages vanish at
> random with no error anywhere. If delivery looks broken, run `paosctl doctor`
> and check `operator_outbox.sent_ts` — not the old agent.


Telegram command surface, mode semantics, escalation mechanics, and the
measurements behind the 'ask well' rules.

## Operator channel (`paos operator …`)

A **separate facet** for reaching your human operator when they're away — distinct
from peer coordination. State lives in dedicated tables; `paos operator …` is the CLI;
the `paosd` daemon bridges Telegram. **This is a human channel, not a peer channel.**

**Modes** (`paos operator mode` — alias `paos operator status` — reads the current one; the
operator sets it, usually from Telegram): `attended` (at laptop, ask normally), `autonomous` (at laptop, hands-off — do
routine work without asking), `away` (not at laptop — **Telegram is the primary human
channel**). Default is `attended`. **Proactive Telegram pings (escalations, digest,
nudges, outbound session messages) fire ONLY when the operator opened the channel
themselves** — see *Telegram is opt-in* below. In attended/autonomous the operator is at the
laptop and answers in-terminal/dashboard; items stay queued and flush the moment the channel
opens. (autonomous means hands-off, not "ping me".) `away` is a **latch with no TTL**: it
holds until the operator sets another mode.

**Telegram commands** (the bot registers a `/` menu on startup, plus a persistent tap-keyboard
for Digest/Fleet/Blocked/Parked + mode row): **`/digest`** — status snapshot on demand, works
in **any** mode (unlike the auto-digest, which only fires in away); `/who` (fleet), `/blocked`,
`/parked`, `/say <session> <text>`, mode switches `/here` `/auto` `/away`, `/help`, `/start`
(onboarding). Answer a blocked session by tapping its option button or quote-replying its
message; `@<workspace> <text>` targets one.

**Mode changes are broadcast.** Every mode switch posts one lobby message from the
system identity **`operator`** (`⚙ operator mode → …`), so your armed `bus wait-joined`
listener wakes on it. Handle that wake like any turn: adopt the new mode for future
decisions, and — critical — **if the mode became `away` while you were waiting on an
in-terminal answer, re-raise the question NOW** via `paos operator ask` + a background
`paos operator wait <id>` (your terminal question is invisible to an away operator).
On →`attended`, surface anything you escalated or parked. (A daemon sweep also converts
`⛔ blocked` sessions into Telegram escalations on the away-flip as a safety net — don't
rely on it; the reflex is yours.)

**Messages FROM the operator arrive on the bus.** When the operator quote-replies one of
your Telegram messages, prefixes `@<workspace>`, or uses `/say <session> …`, the daemon
posts it to `lobby` addressed to you, as sender `operator`, prefixed `📱 operator:`. This
is the ONE exception to "the bus is never a human channel": a message from the `operator`
identity IS your human (bridged from Telegram, not a peer relaying). Treat it as a normal
instruction turn; reply towards the human with `paos operator say` — not with a bus
message to `operator` (nothing reads that).

**Reaching the operator (`paos operator say`)** queues your message into an outbox that
`paosd` (the sole Telegram sender) delivers: session-tagged, code fences kept
intact, long text chunked. Every delivered message is quote-reply-able — the reply comes
back to *you* over the bus. Use `say` in `away` mode for anything the operator should
see soon (milestone done, found something surprising) — one terse message, same
discipline as the bus.

> **KEEP IT SHORT — it is read on a phone.** The operator asked, in as many words, for
> shorter messages. A `say` is a notification, not a report: **1–3 lines, lead with the
> single thing they need to know**, and put evidence/logs/diffs on the dashboard or in the
> repo, not in the message. No status headers, no "SHIPPED" bullet lists, no recap of what
> you already told them — if you're tempted to send five paragraphs, send one line and let
> them ask. The same "Lead with the decision" rule from **Asking well** applies to every
> `say`. Reserved Telegram characters are handled for you on the way out: a `@handle`
> mention of a peer renders as the bare handle and `@operator` becomes the operator's real
> Telegram @username (when `TELEGRAM_OPERATOR_USERNAME` is set) — so write handles
> naturally; file paths (`/app/…`) are left intact.
>
> **Multi-line / code / special-char bodies:** pass `-` as the text and pipe the
> body on stdin — `printf '%s' '…' | paos operator say -`, or a `<<'EOF'` heredoc —
> exactly like `paos bus send`. (`say`/`send`/`ask`/`answer` all accept `-`.) An
> empty body is refused loudly rather than delivered blank.

**When you would involve the human, classify the trigger and read `paos operator mode`:**
- **Ping-now (1 production boundary · 2 destructive/irreversible · 3 external side
  effects/cost):** `attended`/`autonomous` → ask in your terminal. `away` → `paos operator
  ask "<question>"` (prints an id), then arm a **background** `paos operator wait <id>`; on
  the operator's reply the wait exits and prints the answer — proceed. If the operator
  instead answers in your terminal, run `paos operator resolve <id>` to close it.
  **If the answer is a short choice, pass `--options`** (e.g. `paos operator ask
  "deploy motion to prod?" --options ship,hold`) — the operator gets tap-to-answer
  buttons instead of having to type. See **Asking well** below — this is the single
  biggest lever you have on whether the operator channel is usable at all.
- **Park-and-continue (4 consequential-but-reversible · 5 ambiguous intent · 6 scope
  change):** never pings Telegram. Run `paos operator park "<note>"`, keep working on other
  in-scope work, and surface parked items when the operator returns (`paos operator parked`).
