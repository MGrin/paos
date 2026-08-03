# Memory — full usage notes

Supersede, librarian queue, dream, and the long-form rationale.

### When to use

**Make it a reflex, not an afterthought.** `recall` at the **start** of any task
that depends on something you might have established before — where a
resource/secret/config lives, a preference, project state, a convention, a
cheat-sheet. `remember` the **moment** you learn a durable, reusable fact;
don't wait to be asked. The cost of a needless recall is tiny; the cost of
re-deriving or re-scanning something you already knew is not.

- **Store** a durable fact — you MUST choose its scope every time:
  `paos memory remember --global|--org|--project "<fact>"`. Decide deliberately: true
  everywhere (`--global`), shared across this owner's repos (`--org` →
  `org_<owner>_memory`), or only about this repo (`--project`)? With no flag
  `remember` errors — there is no default. Returns
  immediately; durable + vector-searchable within seconds (`--wait` to block).
  **Writes work OFFLINE** (since 2026-07-30): they go through `paosd`, which embeds
  locally. A write either succeeds or says why — it is never silently skipped. If
  `paosd` is unreachable, `recall` FAILS LOUDLY (exit 69) rather than reporting an empty
  result for an index it never consulted.
  **Write SHORT, atomic facts** — one idea per `remember`. Memory is vector
  (semantic) recall, so small self-contained facts retrieve far more precisely than
  long multi-topic blocks; `paos memory` prints a hint if a fact is too long — split it.
  If `paos memory` warns that a near-duplicate already exists, decide whether the new fact
  **supersedes** it: `paos memory remember --supersede <data_id> --global|--org|--project "<fact>"`
  (forgets the old, stores the new) — or `paos memory forget <id>` the stale one.
- **Retrieve** what's known: `paos memory recall "<query>"`. In a repo it searches the
  **project + org + global** tiers; outside a repo, global only. Results are
  newest-first on near-ties and tagged with their date. `--global`/`--org`/`--project`
  to restrict scope. **`--synthesize` and the knowledge graph are both GONE**
  with cognee (retired 2026-07-30): synthesize answered from *every* scope at once,
  leaking work memories into personal repos. Recall the scoped facts and synthesize over
  them yourself. Measured on the real store: recall@5 91%, recall@1 57% — the right fact
  is almost always in the window, so read past the first hit.
- **Remove** a wrong/obsolete memory: find its id with `paos memory list` /
  `paos memory show <dataset>`, then `paos memory forget <data_id>` — gated (previews, then `--force`).
- **Review:** `paos memory list` (datasets + sizes), `paos memory show <dataset>` (its facts,
  with ids). `paos memory graph` is retired with cognee.
- **Librarian:** `paos memory draft "<notes>"` distills notes into candidate memories
  (queued, not written); `paos memory tidy` proposes merges of overlapping facts and
  `paos memory split` unbundles over-long ones — both by READING the facts, not comparing
  vectors (`curate` did that; 44 of its 55 proposals were rejected, so it is gone);
  `paos memory review` lists pending proposals; `paos memory approve <id>|--all` /
  `reject <id>` resolve them. Nothing is written to long-term memory without an
  explicit approve.
- **Phrasings:** `paos memory phrasings [--dataset <ds>] [--limit N] [--dry-run]` attaches
  the questions a fact answers, so a query that shares none of its words can still reach
  it. The phrasings are embedded and **never displayed** — the fact itself is untouched —
  so this one writes directly instead of queueing. Measured on a 30-question golden set:
  hit@1 11/30 → 13/30, MRR 0.509 → 0.553. `--clear` reverses a pass; `--reembed`
  re-vectorises phrasings already on disk without paying the model again.
- **Dream (learn from past sessions):** `paos memory dream [--since 24h] [--limit N]
  [--session <id>] [--dry-run]` reads recent **Claude Code** sessions (via the
  `trajectory` facet), normalizes + chunks them, and distills each into candidate
  memories — the same propose-then-approve queue as `draft`, so nothing lands until
  you `review`/`approve`. This is how lessons buried in past sessions become durable
  memory without a hand-written note. `--dry-run` shows what it would read. Needs the
  local distill LLM up (else it drafts nothing rather than storing raw transcript).
  Defaults are conservative (last 3 sessions); `--session` targets one by id.
  Nightly auto-run and the distill backend are operator knobs — see
  **`references/memory-dream-config.md`**. What matters to you: dreamed proposals are
  **queued, never stored**, and are scoped to the session's own project brain.
