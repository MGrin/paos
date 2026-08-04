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
- **Two-stage recall:** when the optional reranker model is installed, recall proposes 30
  candidates with the fast model and lets `bge-small-en-v1.5` reorder them. Measured on a
  70-question golden set: MRR 0.545 -> 0.633, hit@1 31/70 -> 38/70, at a cost of about
  27 ms per recall. It is OPTIONAL — without the model recall is single-stage and
  unchanged. `paos memory rerank-index` indexes existing facts (resumable, says what is
  left); new facts are indexed as they are written. `PAOS_RERANK_BLEND=0` turns the second
  stage off without uninstalling anything.
- **Phrasings:** `paos memory phrasings [--dataset <ds>] [--limit N] [--dry-run]` attaches
  the questions a fact answers, so a query that shares none of its words can still reach
  it. The phrasings are embedded and **never displayed** — the fact itself is untouched —
  so this one writes directly instead of queueing. Measured on 70 questions across 7
  brains it is **approximately neutral** — MRR 0.508 → 0.523, three brains better and
  three worse. An earlier 30-question run reported +8.6% and that was noise. `--clear`
  reverses a pass; `--reembed` re-vectorises phrasings already on disk without paying the
  model again.
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
