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
    )?;
    // Usage, added 2026-08-03 so ranking can tell a fact that EARNS its place from one
    // that merely matches. Until now the table said nothing about whether a fact had ever
    // been useful, so a note recalled weekly and one never returned since the day it was
    // written ranked identically.
    //
    // Added here rather than in the store's migration ladder because this table is not
    // the store's — `memories` is created by this function, and a ladder entry for it
    // fails on a database that has never held a memory.
    //
    // The errors are swallowed on purpose: `ALTER TABLE ADD COLUMN` fails once the column
    // exists, and this runs on every start. Nothing else here can tell the two apart, and
    // a start that dies because a column is already present would be a worse bug than the
    // one being guarded against.
    let _ = conn.execute("ALTER TABLE memories ADD COLUMN last_used TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE memories ADD COLUMN use_count INTEGER NOT NULL DEFAULT 0", []);
    // Alternate phrasings, added 2026-08-03. Measured on a 30-question golden set: four
    // of the six failures were not ranked badly, the right fact was absent from the top
    // THIRTY. "what does he do for a living" never reaches a fact reading "Product
    // Engineer / Software Architect". A static embedding has no reasoning to bridge that,
    // so no blend weight can — the fix has to put the question's own words somewhere the
    // embedding can see them.
    let _ = conn.execute("ALTER TABLE memories ADD COLUMN aliases TEXT", []);
    // ONE vector for the phrasings, scored against the fact's own with max(). Three
    // designs were measured on a 30-question golden set, and the obvious ones both lost:
    //
    //   baseline, no phrasings ............. hit@1 11/30  MRR 0.509
    //   text+phrasings in one vector ....... hit@1 10/30  MRR 0.509   (no change at all)
    //   phrasings as one vector, max() ..... hit@1 13/30  MRR 0.553   <- this
    //   one vector PER phrasing, max() ..... hit@1  7/30  MRR 0.442   (much worse)
    //
    // Folding the phrasings into the fact's vector does nothing: a static embedding
    // averages its tokens, so five short questions bolted onto a 400-character fact
    // barely move the centroid.
    //
    // Embedding each phrasing SEPARATELY is worse than doing nothing, which is the
    // counter-intuitive one. max() over many short vectors is a best-of-N draw, and with
    // ~600 of them in scope some unrelated fact's phrasing out-scores the right fact's
    // own text on almost any query. More candidates is not more signal.
    //
    // The fact's own embedding is never touched either way, so nothing that already
    // worked can regress.
    let _ = conn.execute("ALTER TABLE memories ADD COLUMN alias_embedding BLOB", []);
    Ok(())
}

/// How long it takes an untouched fact to lose half its ranking bonus.
///
/// Ninety days is deliberately long. These are curated durable facts, not chat turns —
/// the store holds decisions and gotchas that stay true for months, and a half-life tuned
/// for conversation would bury them while they were still correct.
pub const HALF_LIFE_DAYS: f64 = 90.0;

/// The floor a decayed fact cannot fall below.
///
/// Decay REORDERS, it never hides. An old fact ranked last is still findable; an old fact
/// multiplied towards zero is deleted without anyone deciding to delete it, and in a
/// human-curated store that is not forgetting, it is data loss with extra steps.
pub const DECAY_FLOOR: f64 = 0.6;

/// Ranking weight from usefulness: rewarded for being used, decayed for not being.
///
/// `age_days` is measured from the last USE, or from creation if it has never been used —
/// so a fact written an hour ago starts at full weight rather than being punished for
/// having no history. The newest facts are usually the ones that just cost someone an
/// afternoon.
/// Days since the unix epoch, now.
fn now_epoch_days() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() / 86_400.0)
        .unwrap_or(0.0)
}

/// Days since the epoch for an ISO timestamp. An unparseable one reads as NOW, so a
/// malformed row is ranked as fresh rather than as maximally stale — a parsing bug must
/// not quietly demote real facts.
fn epoch_days(iso: &str) -> f64 {
    parse_iso_days(iso).unwrap_or_else(now_epoch_days)
}

fn parse_iso_days(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // days_from_civil (Howard Hinnant), same as the operator crate's parser.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) as f64)
}

fn now_iso_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days, the inverse of the above.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// The most usefulness can add. Similarity still decides; this only breaks ties.
///
/// Uncapped, a fact used 200 times scored 3.6x — enough to beat a far better match on a
/// query it only half fits, which turns "has been useful before" into "is the answer to
/// everything". The test that caught it is the one asserting it cannot swamp similarity.
pub const MAX_USE_BONUS: f64 = 0.75;

pub fn usefulness(use_count: i64, age_days: f64) -> f64 {
    let used = 1.0 + ((1.0 + use_count.max(0) as f64).ln() * 0.5).min(MAX_USE_BONUS);
    let decay = 0.5_f64.powf(age_days.max(0.0) / HALF_LIFE_DAYS);
    used * (DECAY_FLOOR + (1.0 - DECAY_FLOOR) * decay)
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

/// Attach (or with `None`, remove) alternate phrasings and embed them separately.
///
/// The fact's own `embedding` is deliberately left untouched. Phrasings are additive: a
/// question that already reaches a fact must keep reaching it at the same rank, and the
/// only way to promise that is to not move the vector it matched.
pub fn set_aliases(
    conn: &Connection,
    embedder: &dyn Embedder,
    id: &str,
    aliases: Option<&str>,
) -> rusqlite::Result<bool> {
    let cleaned = aliases.map(str::trim).filter(|a| !a.is_empty());
    let vec = cleaned.map(|a| encode(&embedder.embed(a)));
    let n = conn.execute(
        "UPDATE memories SET aliases = ?1, alias_embedding = ?2 WHERE id = ?3",
        params![cleaned, vec, id],
    )?;
    Ok(n > 0)
}

/// How ALIKE the facts in one dataset are: mean pairwise cosine, and how many were used.
///
/// This is the number that explains a brain whose answers are present but badly ordered.
/// Recall can only separate facts that the embedding separates; in a corpus where every
/// entry opens with the same project name and shares the same jargon, the vectors cluster
/// so tightly that the right fact sits twentieth among near-identical neighbours. No
/// ranking knob repairs that, because there is nothing left to rank on.
///
/// Capped at `sample` facts because this is O(n²) and only ever wanted as an indicator.
pub fn corpus_spread(conn: &Connection, dataset: &str, sample: usize) -> Option<(f32, usize)> {
    let mut stmt = conn
        .prepare(
            "SELECT embedding FROM memories WHERE dataset=?1 AND superseded IS NULL LIMIT ?2",
        )
        .ok()?;
    let vecs: Vec<Vec<f32>> = stmt
        .query_map(params![dataset, sample as i64], |r| {
            Ok(decode(&r.get::<_, Vec<u8>>(0)?))
        })
        .ok()?
        .filter_map(|v| v.ok())
        .collect();
    if vecs.len() < 2 {
        return None;
    }
    let mut total = 0.0f64;
    let mut pairs = 0usize;
    for i in 0..vecs.len() {
        for j in (i + 1)..vecs.len() {
            total += cosine(&vecs[i], &vecs[j]) as f64;
            pairs += 1;
        }
    }
    Some(((total / pairs as f64) as f32, vecs.len()))
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
        "SELECT id, dataset, text, embedding, created_ts, \
                COALESCE(last_used, created_ts), COALESCE(use_count, 0), alias_embedding \
         FROM memories \
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
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Option<Vec<u8>>>(7)?.map(|b| decode(&b)),
        ))
    })?;

    let now = now_epoch_days();
    let mut hits: Vec<Hit> = Vec::new();
    for row in rows {
        let (memory, vec, touched, uses, alias_vec) = row?;
        // Similarity says a fact MATCHES; usefulness says it has earned being read. A
        // fact recalled weekly and one never returned since the day it was written used
        // to rank identically, and only similarity decided between them.
        let age = (now - epoch_days(&touched)).max(0.0);
        // max(), not a blend: the phrasings exist precisely for the query the fact's own
        // wording cannot answer, so averaging them back together would re-drown the
        // signal they were written to provide. A fact is reachable by what it SAYS or by
        // how someone would ASK for it, whichever fits better.
        let sim = match &alias_vec {
            // No discount on the phrasing match. Discounting it was the obvious repair
            // for the brains phrasings made worse, and it was swept from 0.000 to 0.200
            // on the 70-question set: every value scored at or below plain max(). A knob
            // that never wins is a knob that will be mis-tuned later.
            Some(a) => cosine(&q, &vec).max(cosine(&q, a)),
            None => cosine(&q, &vec),
        };
        hits.push(Hit { score: sim * usefulness(uses, age) as f32, memory });
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
    // REINFORCEMENT. Being pulled into a session's context is the only evidence a fact is
    // worth keeping, and it was being thrown away. Best-effort on purpose: a recall that
    // failed because its own bookkeeping failed would be a strictly worse trade.
    let touched: Vec<&str> = hits.iter().map(|h| h.memory.id.as_str()).collect();
    let _ = reinforce(conn, &touched);
    Ok(hits)
}

/// Record that these facts were just used.
pub fn reinforce(conn: &Connection, ids: &[&str]) -> rusqlite::Result<()> {
    let now = now_iso_ts();
    for id in ids {
        conn.execute(
            "UPDATE memories SET use_count = COALESCE(use_count,0) + 1, last_used = ?1 \
             WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
    }
    Ok(())
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
mod usefulness_tests {
    use super::*;

    #[test]
    fn a_fact_written_today_is_not_penalised_for_having_no_history() {
        // The reverse would be exactly backwards: the newest facts are usually the ones
        // that just cost someone an afternoon.
        assert!(usefulness(0, 0.0) >= 1.0);
    }

    #[test]
    fn being_used_raises_the_weight_and_never_runs_away() {
        let never = usefulness(0, 0.0);
        let often = usefulness(20, 0.0);
        assert!(often > never, "use has to count for something");
        // Logarithmic on purpose: a fact read 200 times is not 200 times more relevant,
        // and linear growth would let one popular fact win every query it half-matches.
        assert!(usefulness(200, 0.0) < never * 3.0, "and it must not swamp similarity");
    }

    #[test]
    fn disuse_decays_but_never_towards_zero() {
        let fresh = usefulness(0, 0.0);
        let old = usefulness(0, 365.0);
        assert!(old < fresh, "an untouched fact should sink");
        // DECAY REORDERS, IT DOES NOT HIDE. Multiplying an old fact towards zero deletes
        // it without anyone deciding to — data loss with extra steps, in a store whose
        // whole premise is that a human curates it.
        assert!(old > fresh * DECAY_FLOOR * 0.99, "and never become unfindable");
    }

    #[test]
    fn a_used_old_fact_still_beats_an_unused_old_one() {
        assert!(usefulness(10, 200.0) > usefulness(0, 200.0));
    }

    #[test]
    fn an_unparseable_timestamp_reads_as_fresh_rather_than_ancient() {
        // A parsing bug must not quietly demote real facts to the bottom of every query.
        let now = now_epoch_days();
        assert!((epoch_days("not a date") - now).abs() < 1.0);
    }

    #[test]
    fn the_iso_written_back_is_the_iso_that_parses() {
        // reinforce() writes this and recall reads it. If the two disagree, every
        // reinforced fact silently reads as ancient — the opposite of the intent.
        let s = now_iso_ts();
        assert!(parse_iso_days(&s).is_some(), "wrote an unparseable timestamp: {s}");
        assert!((epoch_days(&s) - now_epoch_days()).abs() < 1.0, "{s}");
    }
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
    fn phrasings_never_touch_the_facts_own_vector() {
        // Additive means additive. If attaching phrasings moved the fact's embedding, a
        // query that already found it could start finding something else — paying for
        // abstract queries with the direct ones, which is a worse store.
        let (c, e) = (db(), emb());
        let id = remember(&c, &e, "ds", "he is a product engineer", "2026-08-01").unwrap();
        let before: Vec<u8> =
            c.query_row("SELECT embedding FROM memories WHERE id=?1", [&id], |r| r.get(0))
                .unwrap();
        assert!(set_aliases(&c, &e, &id, Some("what does he do for a living")).unwrap());
        let (text, aliases, after): (String, Option<String>, Vec<u8>) = c
            .query_row("SELECT text, aliases, embedding FROM memories WHERE id=?1", [&id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        let alias_rows = alias_state(&c, &id).1;
        assert_eq!(text, "he is a product engineer", "the fact itself must not change");
        assert_eq!(aliases.as_deref(), Some("what does he do for a living"));
        assert_eq!(before, after, "the fact's own vector must be untouched");
        assert_eq!(alias_rows, 1, "the phrasing needs a vector of its own");
    }

    #[test]
    fn a_query_matching_only_a_phrasing_still_finds_the_fact() {
        // THE regression this feature exists for. Scored against the fact's text alone,
        // a question sharing none of its words is unreachable at any depth.
        let (c, e) = (db(), emb());
        let id = remember(&c, &e, "ds", "kettle boils at one hundred", "2026-08-01").unwrap();
        remember(&c, &e, "ds", "a totally unrelated note about invoices", "2026-08-01").unwrap();
        set_aliases(&c, &e, &id, Some("how hot does water get")).unwrap();
        let hits = recall(&c, &e, &["ds".to_string()], "how hot does water get", 1).unwrap();
        assert_eq!(hits[0].memory.id, id);
    }

    #[test]
    fn clearing_phrasings_removes_their_vector_too() {
        // Reversibility is what makes a bulk pass safe to run. Leaving the vector behind
        // would keep matching questions for phrasings the store no longer admits to.
        let (c, e) = (db(), emb());
        let id = remember(&c, &e, "ds", "some durable fact", "2026-08-01").unwrap();
        set_aliases(&c, &e, &id, Some("a phrasing")).unwrap();
        set_aliases(&c, &e, &id, None).unwrap();
        let (aliases, rows) = alias_state(&c, &id);
        assert_eq!(aliases, None);
        assert_eq!(rows, 0);
    }

    /// The phrasing list and the vectors that back it, which must never disagree.
    fn alias_state(c: &Connection, id: &str) -> (Option<String>, i64) {
        let a = c
            .query_row("SELECT aliases FROM memories WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        let v: Option<Vec<u8>> = c
            .query_row("SELECT alias_embedding FROM memories WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        (a, v.is_some() as i64)
    }

    #[test]
    fn whitespace_only_phrasings_are_the_same_as_none() {
        // A model that answers with a blank line must not leave a fact carrying an alias
        // column that reads as configured while contributing nothing.
        let (c, e) = (db(), emb());
        let id = remember(&c, &e, "ds", "a fact", "2026-08-01").unwrap();
        set_aliases(&c, &e, &id, Some("   \n ")).unwrap();
        let (aliases, rows) = alias_state(&c, &id);
        assert_eq!(aliases, None);
        assert_eq!(rows, 0);
    }

    #[test]
    fn setting_aliases_on_a_missing_fact_is_false_not_an_error() {
        let (c, e) = (db(), emb());
        assert!(!set_aliases(&c, &e, "no-such-id", Some("q")).unwrap());
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