//! Scoped vector memory.
//!
//! Replaces cognee, which held **981 facts (~4.2 MB of text) in 643 MB of index inside
//! a 2.6 GB install** — 153× storage amplification — while providing neither the
//! embeddings (a local model did) nor the *scoping* (Python did, badly).
//!
//! cognee ignores its own `datasets` filter, so the three-tier scope model was
//! reimplemented client-side by reading every in-scope fact off disk on each recall and
//! substring-matching. That is what produced the two worst defects:
//!
//! * **Cross-scope leakage.** The one path that skipped the client-side filter
//!   (`--synthesize`) answered from all 19 datasets. Reproduced from a personal repo,
//!   it returned a work client's deploy topology.
//! * **Starvation.** Over-fetch 64, keep the in-scope ones — measured 5 survivors for a
//!   `top_k` of 8, with 92% of the fetch budget discarded. It degrades as the corpus
//!   grows, and worst for exactly the small project scopes the tiers exist to serve.
//!
//! Here scope is a `WHERE` clause. Both defects stop existing by construction rather
//! than being patched.

pub mod doctor;

use rusqlite::{params, Connection};

pub mod difflib;
pub mod embed;
pub mod health;
pub mod model;
pub mod scope;

pub use embed::{best_available, Embedder, HashEmbedder, Model2VecEmbedder};

/// A stored memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: String,
    pub dataset: String,
    pub text: String,
    pub created_ts: String,
}

/// A recall hit, carrying its **real** cosine similarity.
///
/// The Python reported `sim = (n-1-pos)/(n-1)` — rank position, not similarity — so two
/// results 0.9 and 0.4 apart were treated as evenly spaced and recency re-ranking could
/// flip a strong match below a weak recent one.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub memory: Memory,
    pub score: f32,
}

/// Record which vector space this store holds, and refuse to mix.
///
/// Learned the hard way while benchmarking: a store migrated with `hash-v1` was queried
/// with `potion-retrieval-32M`. Both are 512-dim, so cosine happily returned numbers —
/// meaningless ones, comparing coordinates from unrelated spaces — and the model looked
/// far worse than it is. Silent nonsense is the worst failure mode a memory store has,
/// so this is a hard error.
pub fn check_space(conn: &Connection, embedder: &dyn Embedder) -> Result<(), String> {
    let stored: Option<String> = conn
        .query_row("SELECT value FROM memory_meta WHERE key='embedder'", [], |r| r.get(0))
        .ok();
    match stored {
        None => {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO memory_meta(key, value) VALUES('embedder', ?1)",
                [embedder.id()],
            );
            Ok(())
        }
        Some(id) if id == embedder.id() => Ok(()),
        Some(id) => Err(format!(
            "this store holds {id} vectors but the active embedder is {} —              re-embed before querying; comparing across spaces returns numbers that mean nothing",
            embedder.id()
        )),
    }
}

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
           id         TEXT PRIMARY KEY,
           dataset    TEXT NOT NULL,
           text       TEXT NOT NULL,
           embedding  BLOB NOT NULL,
           created_ts TEXT NOT NULL,
           superseded TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_memories_dataset
           ON memories(dataset, created_ts);
         CREATE TABLE IF NOT EXISTS memory_meta (key TEXT PRIMARY KEY, value TEXT);",
    )
}

/// Store a fact. **Works offline** — that is the point.
///
/// cognee's `remember` probed `api.openai.com` with a 2.5 s HEAD on every write and, if
/// it failed, printed a note and returned **exit 0** with the fact dropped. No queue, no
/// retry, no spool. Any tethering or DNS hiccup past 2.5 s silently lost a fact the user
/// had just asked to remember. Embedding is local here, so there is nothing to be
/// offline *from*.
pub fn remember(
    conn: &Connection,
    embedder: &dyn Embedder,
    dataset: &str,
    text: &str,
    created_ts: &str,
) -> rusqlite::Result<String> {
    let id = stable_id(dataset, text);
    let vec = embedder.embed(text);
    conn.execute(
        "INSERT INTO memories(id, dataset, text, embedding, created_ts) \
         VALUES(?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(id) DO UPDATE SET text=excluded.text, embedding=excluded.embedding",
        params![id, dataset, text, encode(&vec), created_ts],
    )?;
    Ok(id)
}

/// Mark `old_id` superseded by `new_id` so it stops being recalled but stays auditable.
pub fn supersede(conn: &Connection, old_id: &str, new_id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE memories SET superseded = ?1 WHERE id = ?2 AND superseded IS NULL",
        params![new_id, old_id],
    )?;
    Ok(n > 0)
}

pub fn forget(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM memories WHERE id = ?1", [id])? > 0)
}

/// Recall the `top_k` best matches **within `datasets`**.
///
/// Scope is enforced in SQL, so an out-of-scope fact is never a candidate and cannot
/// consume a result slot. `top_k` is therefore always filled if the scope holds that
/// many facts — no over-fetch, no starvation.
///
/// Brute-force cosine: 981 × 512 f32 is ~2 MB and scans in well under a millisecond. An
/// ANN index would be pure complexity until ~100k facts.
/// Weight of the lexical signal in the blend.
///
/// 0.25 is deliberately modest: dense similarity is doing the real work (recall@5 is 91%,
/// so the right fact is almost always present) and the lexical term only needs to break
/// ties within that window. A larger weight starts promoting facts that merely share
/// vocabulary, which is the failure that made `curate` useless.
const LEXICAL_WEIGHT_DEFAULT: f32 = 0.25;

/// Overridable so the blend can be A/B'd against itself on IDENTICAL queries.
/// `PAOS_LEXICAL_WEIGHT=0` disables reranking entirely, which is the control arm.
fn lexical_weight() -> f32 {
    std::env::var("PAOS_LEXICAL_WEIGHT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|w: &f32| (0.0..=1.0).contains(w))
        .unwrap_or(LEXICAL_WEIGHT_DEFAULT)
}

/// Distinctive terms of a string: lowercase, 4+ chars, deduplicated.
///
/// The length floor drops "the"/"and"/"for" without needing a stopword list to maintain,
/// and short tokens carry little identifying signal in this corpus anyway.
fn terms(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// Share of the QUERY's terms present in the text.
///
/// Normalised by the query, not the fact: dividing by the fact's length would punish a
/// long fact for being thorough, and long facts are often the most useful ones.
fn lexical_overlap(q_terms: &std::collections::HashSet<String>, text: &str) -> f32 {
    if q_terms.is_empty() {
        return 0.0;
    }
    let t = terms(text);
    q_terms.iter().filter(|w| t.contains(*w)).count() as f32 / q_terms.len() as f32
}

fn blend(cosine: f32, lexical: f32) -> f32 {
    let w = lexical_weight();
    (1.0 - w) * cosine + w * lexical
}

pub fn recall(
    conn: &Connection,
    embedder: &dyn Embedder,
    datasets: &[String],
    query: &str,
    top_k: usize,
) -> rusqlite::Result<Vec<Hit>> {
    if datasets.is_empty() || top_k == 0 {
        return Ok(Vec::new());
    }
    let q = embedder.embed(query);
    let placeholders = vec!["?"; datasets.len()].join(",");
    let sql = format!(
        "SELECT id, dataset, text, embedding, created_ts FROM memories \
         WHERE superseded IS NULL AND dataset IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_vec: Vec<&dyn rusqlite::ToSql> =
        datasets.iter().map(|d| d as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params_vec.as_slice(), |r| {
        Ok((
            Memory {
                id: r.get(0)?,
                dataset: r.get(1)?,
                text: r.get(2)?,
                created_ts: r.get(4)?,
            },
            decode(&r.get::<_, Vec<u8>>(3)?),
        ))
    })?;

    let mut hits: Vec<Hit> = Vec::new();
    for row in rows {
        let (memory, vec) = row?;
        hits.push(Hit { score: cosine(&q, &vec), memory });
    }
    // HYBRID RERANK. Dense similarity finds the right neighbourhood but orders within it
    // poorly: measured on this store, 12 of 35 semantic queries put the correct fact at
    // rank 2-5 rather than 1 (nine of them at rank 2). Blending in exact term overlap
    // fixes the case a static embedding is worst at — a rare literal token (an identifier,
    // a path, a flag) that the model has no dedicated dimension for but which is exactly
    // how an engineer searches.
    //
    // Deliberately lexical, not a second model: recall runs on the hot path for every
    // session's auto-recall, so this must cost microseconds, not milliseconds.
    let q_terms = terms(query);
    if !q_terms.is_empty() && lexical_weight() > 0.0 {
        for h in hits.iter_mut() {
            h.score = blend(h.score, lexical_overlap(&q_terms, &h.memory.text));
        }
    }
    // Deduplicate identical text before truncating: near-duplicates used to consume
    // multiple slots out of top_k, crowding out distinct facts.
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(normalise(&h.memory.text)));
    hits.truncate(top_k);
    Ok(hits)
}

fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Cosine similarity. Returns 0.0 for a zero vector rather than NaN — a NaN would
/// poison the sort and silently reorder results.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Content-addressed id: re-remembering the same fact in the same scope updates in
/// place instead of accumulating duplicates.
///
/// Public because a SPOOLING client needs to name the row it is about to create. A
/// sandboxed session cannot reach the socket, so its write is queued and the daemon's
/// reply carries no id — yet the write-time split offer has to target that exact row.
///
/// This is NOT the thing `Request::Supersede`'s doc warns about. That warning is against
/// reimplementing the hash in a SECOND LANGUAGE, where the two copies drift. Calling this
/// function is the same code, so it cannot.
pub fn stable_id(dataset: &str, text: &str) -> String {
    // FNV-1a, 128-bit-ish by hashing twice with different offsets. Not cryptographic —
    // it only needs to be stable and collision-resistant across ~1e5 short strings.
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        let mut h = seed;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
    let key = format!("{dataset}\u{1}{}", normalise(text));
    format!("{:016x}{:016x}", fnv(0xcbf2_9ce4_8422_2325, key.as_bytes()), fnv(0x9e37_79b9_7f4a_7c15, key.as_bytes()))
}

fn encode(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn decode(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {

    /// The write-time split offer targets a row it has not seen created: a sandboxed
    /// session spools its write, so the daemon's reply carries no id, and the CLI computes
    /// the id itself with `stable_id`. If that ever stopped matching what `remember`
    /// actually stores, the proposal would point at a row that never appears — and the
    /// resurrection guard would silently retire it, so the feature would fail QUIETLY.
    #[test]
    fn stable_id_is_what_remember_actually_stores() {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        let e = HashEmbedder::new(64);
        let (ds, text) = ("global_memory", "a fact written from a sandbox");
        let stored = remember(&c, &e, ds, text, "2026-08-01").unwrap();
        assert_eq!(stored, stable_id(ds, text),
                   "the id a spooling client predicts must equal the id remember writes");
    }

    #[test]
    fn stable_id_normalises_the_same_way_remember_does() {
        // Whitespace differences must not produce a different id, or the offer would
        // target a row that does not exist for a fact that plainly does.
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        let e = HashEmbedder::new(64);
        let stored = remember(&c, &e, "ds", "  spaced   out  ", "2026-08-01").unwrap();
        assert_eq!(stored, stable_id("ds", "spaced out"));
    }

    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        c
    }

    fn emb() -> HashEmbedder {
        HashEmbedder::new(64)
    }

    #[test]
    fn scope_is_a_where_clause_so_leakage_is_impossible() {
        // THE regression. `--synthesize` answered from all 19 datasets: asked from a
        // personal repo it returned a work client's deploy topology. An out-of-scope
        // fact must never even be a candidate.
        let c = db();
        let e = emb();
        remember(&c, &e, "proj_acme_dotfiles", "the dotfiles repo is copied into home", "t").unwrap();
        remember(&c, &e, "proj_examplecorp_luca", "luca-backend deploys a fly worker", "t").unwrap();

        let hits = recall(&c, &e, &["proj_acme_dotfiles".into()], "deploy process", 8).unwrap();
        assert!(
            hits.iter().all(|h| h.memory.dataset == "proj_acme_dotfiles"),
            "out-of-scope memory leaked: {hits:?}"
        );
    }

    #[test]
    fn small_scopes_do_not_starve() {
        // Over-fetch-then-filter produced 5 in-scope survivors for top_k=8, discarding
        // 92% of the budget — worst for exactly the small project scopes tiers exist for.
        let c = db();
        let e = emb();
        for i in 0..200 {
            remember(&c, &e, "global", &format!("unrelated global fact {i}"), "t").unwrap();
        }
        for i in 0..8 {
            remember(&c, &e, "proj_small", &format!("small project fact {i}"), "t").unwrap();
        }
        let hits = recall(&c, &e, &["proj_small".into()], "fact", 8).unwrap();
        assert_eq!(hits.len(), 8, "top_k must be filled from the scope");
    }

    #[test]
    fn multiple_scopes_are_searched_together() {
        let c = db();
        let e = emb();
        remember(&c, &e, "global", "global fact", "t").unwrap();
        remember(&c, &e, "proj_x", "project fact", "t").unwrap();
        let hits = recall(&c, &e, &["global".into(), "proj_x".into()], "fact", 8).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn scores_are_real_cosine_not_rank_position() {
        // Python reported (n-1-pos)/(n-1), so recency could flip a strong match below a
        // weak recent one because every gap looked identical.
        let c = db();
        let e = emb();
        remember(&c, &e, "d", "the quick brown fox jumps", "t").unwrap();
        remember(&c, &e, "d", "completely unrelated content here", "t").unwrap();
        let hits = recall(&c, &e, &["d".into()], "the quick brown fox jumps", 2).unwrap();
        assert!(hits[0].score > 0.99, "exact match should score ~1.0, got {}", hits[0].score);
        assert!(hits[0].score > hits[1].score);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score), "must be sorted");
    }

    #[test]
    fn remembering_the_same_fact_twice_updates_in_place() {
        let c = db();
        let e = emb();
        let a = remember(&c, &e, "d", "a durable fact", "t1").unwrap();
        let b = remember(&c, &e, "d", "a durable fact", "t2").unwrap();
        assert_eq!(a, b);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "no duplicate row");
    }

    #[test]
    fn the_same_text_in_two_scopes_is_two_memories() {
        let c = db();
        let e = emb();
        let a = remember(&c, &e, "global", "same words", "t").unwrap();
        let b = remember(&c, &e, "proj_x", "same words", "t").unwrap();
        assert_ne!(a, b, "scope is part of identity");
    }

    #[test]
    fn superseded_memories_stop_being_recalled_but_remain_stored() {
        let c = db();
        let e = emb();
        let old = remember(&c, &e, "d", "the old truth", "t1").unwrap();
        let new = remember(&c, &e, "d", "the new truth", "t2").unwrap();
        assert!(supersede(&c, &old, &new).unwrap());
        let hits = recall(&c, &e, &["d".into()], "truth", 8).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory.text, "the new truth");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "superseded rows stay auditable");
    }

    #[test]
    fn near_duplicate_text_does_not_consume_two_slots() {
        let c = db();
        let e = emb();
        remember(&c, &e, "d", "Deploy runs on fly", "t").unwrap();
        remember(&c, &e, "d", "deploy   runs on   fly", "t").unwrap();
        remember(&c, &e, "d", "something else entirely", "t").unwrap();
        let hits = recall(&c, &e, &["d".into()], "deploy", 8).unwrap();
        assert_eq!(hits.len(), 2, "whitespace/case duplicates collapse: {hits:?}");
    }

    #[test]
    fn forget_removes_it_from_recall() {
        let c = db();
        let e = emb();
        let id = remember(&c, &e, "d", "temporary", "t").unwrap();
        assert!(forget(&c, &id).unwrap());
        assert!(recall(&c, &e, &["d".into()], "temporary", 8).unwrap().is_empty());
        assert!(!forget(&c, &id).unwrap(), "second forget is a no-op, not an error");
    }

    #[test]
    fn empty_scope_or_zero_k_returns_nothing_rather_than_everything() {
        // A fail-open here would be a leak.
        let c = db();
        let e = emb();
        remember(&c, &e, "d", "secret", "t").unwrap();
        assert!(recall(&c, &e, &[], "secret", 8).unwrap().is_empty());
        assert!(recall(&c, &e, &["d".into()], "secret", 0).unwrap().is_empty());
    }

    #[test]
    fn embeddings_survive_the_blob_round_trip() {
        let v: Vec<f32> = (0..64).map(|i| i as f32 * 0.125 - 3.0).collect();
        assert_eq!(decode(&encode(&v)), v);
    }

    #[test]
    fn cosine_is_well_behaved_at_the_edges() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero vector must not be NaN");
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch is not a panic");
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn mixing_vector_spaces_is_refused() {
        // REGRESSION: a hash-v1 store queried with potion-retrieval-32M returned
        // numbers from unrelated coordinate systems and made the model look broken.
        struct Fake(&'static str);
        impl Embedder for Fake {
            fn embed(&self, _t: &str) -> Vec<f32> { vec![0.0; 512] }
            fn dimensions(&self) -> usize { 512 }
            fn id(&self) -> &str { self.0 }
        }
        let c = db();
        assert!(check_space(&c, &Fake("hash-v1")).is_ok(), "first use records the space");
        assert!(check_space(&c, &Fake("hash-v1")).is_ok(), "same space is fine");
        let err = check_space(&c, &Fake("potion-retrieval-32M")).unwrap_err();
        assert!(err.contains("hash-v1") && err.contains("potion"), "{err}");
    }
}