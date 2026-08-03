# Trajectory facet (`paos trajectory …`)

Pure ETL over local Claude Code transcripts. Its only consumer is `paos memory dream`;
no agent invokes it directly, which is why it is not in SKILL.md.

## Trajectory (`paos trajectory …`)

Normalizes this machine's local **Claude Code** session transcripts
(`~/.claude/projects/**/*.jsonl`) into a compact, agent-friendly record format —
so past experience can be fed to memory (see `paos memory dream`). Adapted from
Letta AI's [`trajectory`](https://github.com/letta-ai/trajectory) idea, re-implemented
stdlib-only (no Node dep); the record shape is a subset of their trajectory-v1 schema.
It drops harness bookkeeping (queue ops, attachments, hook noise, thinking signatures,
UI envelopes) and truncates long tool output, giving a **~5–8× token reduction** over the
native JSONL.

- `paos trajectory list [--limit N] [--since 24h] [--json]` — recent sessions, newest first.
- `paos trajectory show <session-id|path> [--json] [--no-truncate]` — the normalized records
  (or compact text) for one session.
- `paos trajectory stats [--limit N] [--since …]` — native-vs-normalized token estimate
  (makes the reduction visible).

Records: one leading `meta` (source, session id, cwd, branch), then `user` / `assistant`
(with `tool_calls`) / `reasoning` / `tool` records, each timestamped. Pure ETL — the
*consumer* is `paos memory dream`, which chunks a session and drafts candidate memories
into the human-gated proposal queue. Claude-Code-only today; the adapter boundary leaves
room for other sources. `python3 …/paos/test_trajectory_facet.py` runs the offline tests.
