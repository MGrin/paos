# `paos memory dream` — backend and nightly configuration
> **`paos-operatord` / `ai.paos.operator` are RETIRED (2026-07-30).** paosd owns
> Telegram, the outbox, the dashboard and the nightly dream. Do **not** load that
> LaunchAgent: a second process on the same bot token makes Telegram hand each
> message to whichever consumer asks first, so the operator's messages vanish at
> random with no error anywhere. If delivery looks broken, run `paosctl doctor`
> and check `operator_outbox.sent_ts` — not the old agent.


Operator knobs (distill backend, overnight window, nightly limits, dashboard
Settings). An agent never sets these, so they are not in SKILL.md.

**Distill backend** (`MEMORY_LLM_BACKEND`): defaults to **`claude`** — shells the Claude
  Code CLI on your subscription (`claude -p --model claude-haiku-4-5`, no local RAM, no API
  key). Set `local` to use LM Studio instead (offline/free, but loads a ~7 GB model). On the
  `claude` backend a whole normalized session goes to Claude in one call (no chunking).
  **Nightly auto-run is ON by default** (the RAM objection is gone now that it distills via
  the subscription CLI): `paosd` fires it once/day in an overnight window (default
  03:00–06:00 local; tune `DREAM_HOUR_START`/`DREAM_HOUR_END`/`DREAM_NIGHTLY_LIMIT`/
  `DREAM_NIGHTLY_SINCE`). Disable with **`PAOS_DREAM_ENABLED=0`** in the daemon env. **Manual**
  dreams are always available: `paos memory dream` on the CLI or the **"Dream now"** button in
  the dashboard Memory tab. Dreamed proposals show in the **dashboard Inbox** (source `dream`,
  blue) and the operator Telegram digest. Loop: sessions → nightly dream → Inbox → you approve.
  Dreamed captures are **scoped to the session's own project brain** (derived from its cwd's
  git origin), falling back to global only for non-repo sessions — so a session's facts land
  in `proj_<owner>_<repo>`, not dumped into global.
  All nightly-dream knobs (on/off, backend, overnight window, sessions/night, look-back) are
  live-editable from the **dashboard Settings page** (backed by `paos config`, a key/value
  store in paos.db that the daemon re-reads each tick — no restart); env `PAOS_DREAM_*` /
  `MEMORY_LLM_BACKEND` are the defaults.
