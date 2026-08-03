//! Memory hygiene: finding the problems that degrade recall silently.
//!
//! Nothing errors when memory is unhealthy. Recall still returns results — they are
//! just quietly worse than they should be, which is why this went unnoticed until it
//! was measured: **475 of 984 facts (48%) exceed the length at which retrieval starts
//! to suffer**, median 658 characters.
//!
//! Two problems, both measurable from data already in the store:
//!
//! * **Over-long facts.** A long fact is a bag of several ideas averaged into one
//!   vector, so it matches everything weakly and nothing strongly. Splitting one long
//!   fact into three atomic ones makes all three retrievable.
//! * **Near-duplicates.** Two vectors this close both match the same query, so they
//!   consume two of `top_k` slots to say one thing — crowding out a distinct fact.
//!
//! This module only *reports*. Fixing means rewriting a memory, which is a judgement
//! call that belongs in the human-gated proposal queue, not in a sweep.

use crate::cosine;
use rusqlite::Connection;

/// Facts longer than this retrieve measurably worse. Matches the Python's warning line
/// so the two agree about what "too long" means.
pub const LONG_FACT_CHARS: usize = 600;

/// Cosine above which two facts are saying the same thing. Deliberately high: the
/// curate pass previously proposed merges at 0.81–0.84 and the operator rejected 44 of
/// them, which is how a review queue teaches you to ignore it.
pub const NEAR_DUPLICATE: f32 = 0.95;

#[derive(Debug, Clone, PartialEq)]
pub struct LongFact {
    pub id: String,
    pub dataset: String,
    pub chars: usize,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DuplicatePair {
    pub a: String,
    pub b: String,
    pub dataset: String,
    pub similarity: f32,
    pub preview: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Health {
    pub total: usize,
    pub median_chars: usize,
    pub max_chars: usize,
    pub over_long: usize,
    pub longest: Vec<LongFact>,
    pub duplicates: Vec<DuplicatePair>,
    /// (dataset, count), largest first — an outsized scope is its own smell.
    pub by_dataset: Vec<(String, usize)>,
}

impl Health {
    /// Share of facts that are too long to retrieve well.
    pub fn over_long_pct(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.over_long as f64 * 100.0 / self.total as f64 }
    }
}

/// Analyse the store. `sample_limit` bounds the O(n²) duplicate scan.
///
/// The scan is per-dataset, not global: two identical facts in *different* scopes are
/// correct by design (the same truth can be relevant to a repo and to the machine), so
/// comparing across scopes would generate proposals that should always be rejected.
pub fn analyse(conn: &Connection, sample_limit: usize) -> rusqlite::Result<Health> {
    let mut stmt = conn.prepare(
        "SELECT id, dataset, text, embedding FROM memories WHERE superseded IS NULL",
    )?;
    let rows: Vec<(String, String, String, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(Result::ok)
        .collect();

    let mut h = Health { total: rows.len(), ..Default::default() };
    if rows.is_empty() {
        return Ok(h);
    }

    let mut lengths: Vec<usize> = rows.iter().map(|(_, _, t, _)| t.chars().count()).collect();
    lengths.sort_unstable();
    h.median_chars = lengths[lengths.len() / 2];
    h.max_chars = *lengths.last().unwrap_or(&0);
    h.over_long = lengths.iter().filter(|l| **l > LONG_FACT_CHARS).count();

    let mut long: Vec<LongFact> = rows
        .iter()
        .filter(|(_, _, t, _)| t.chars().count() > LONG_FACT_CHARS)
        .map(|(id, ds, t, _)| LongFact {
            id: id.clone(),
            dataset: ds.clone(),
            chars: t.chars().count(),
            preview: preview(t),
        })
        .collect();
    long.sort_by(|a, b| b.chars.cmp(&a.chars));
    long.truncate(10);
    h.longest = long;

    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for (_, ds, _, _) in &rows {
        *counts.entry(ds.as_str()).or_default() += 1;
    }
    let mut by: Vec<(String, usize)> = counts.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    by.sort_by(|a, b| b.1.cmp(&a.1));
    h.by_dataset = by;

    // Duplicates, per dataset. Bounded so a large store cannot make this pathological.
    let mut groups: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for (i, (_, ds, _, _)) in rows.iter().enumerate() {
        groups.entry(ds.as_str()).or_default().push(i);
    }
    let mut dupes = Vec::new();
    for (ds, idxs) in groups {
        let idxs = &idxs[..idxs.len().min(sample_limit)];
        let vecs: Vec<Vec<f32>> = idxs.iter().map(|i| decode(&rows[*i].3)).collect();
        for a in 0..idxs.len() {
            for b in (a + 1)..idxs.len() {
                let sim = cosine(&vecs[a], &vecs[b]);
                if sim >= NEAR_DUPLICATE {
                    dupes.push(DuplicatePair {
                        a: rows[idxs[a]].0.clone(),
                        b: rows[idxs[b]].0.clone(),
                        dataset: ds.to_string(),
                        similarity: sim,
                        preview: preview(&rows[idxs[a]].2),
                    });
                }
            }
        }
    }
    dupes.sort_by(|x, y| y.similarity.total_cmp(&x.similarity));
    dupes.truncate(20);
    h.duplicates = dupes;
    Ok(h)
}

fn preview(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(80).collect()
}

fn decode(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Render for a phone or a terminal. Leads with the number that matters.
pub fn render(h: &Health) -> String {
    if h.total == 0 {
        return "no memories stored".into();
    }
    let mut out = vec![format!("🧠 memory health — {} facts", h.total)];
    out.push(format!(
        "  length: median {} · max {} · {} over {} chars ({:.0}%)",
        h.median_chars, h.max_chars, h.over_long, LONG_FACT_CHARS, h.over_long_pct()
    ));
    // Length alone is NOT a defect, and saying so was misleading. Measured 2026-07-30:
    // the split pass read over-long entries from this store and judged most of them
    // single coherent facts — one detailed fact is fine, and rewriting it would only
    // lose detail. What hurts recall is an entry that BUNDLES several topics, because
    // its one vector then matches everything weakly and nothing strongly. Length is a
    // weak proxy for that, so report it as a hint about where to look, not a fault.
    if h.over_long_pct() >= 25.0 {
        out.push("    (length is a hint, not a fault — `paos memory split --dry-run` asks".into());
        out.push("     the model which of these actually bundle separate topics)".into());
    }
    if !h.duplicates.is_empty() {
        out.push(format!("  {} near-duplicate pair(s) ≥{:.2} — each wastes a top_k slot",
                         h.duplicates.len(), NEAR_DUPLICATE));
        for d in h.duplicates.iter().take(3) {
            out.push(format!("    {:.3} [{}] {}", d.similarity, d.dataset, d.preview));
        }
    }
    if !h.longest.is_empty() {
        out.push("  longest:".into());
        for f in h.longest.iter().take(3) {
            out.push(format!("    {} chars [{}] {}", f.chars, f.dataset, f.preview));
        }
    }
    if h.duplicates.is_empty() {
        out.push("  ✓ no near-duplicates".into());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{remember, ensure_schema, HashEmbedder};

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        ensure_schema(&c).unwrap();
        c
    }

    #[test]
    fn an_empty_store_is_healthy_not_an_error() {
        let h = analyse(&db(), 100).unwrap();
        assert_eq!(h.total, 0);
        assert_eq!(render(&h), "no memories stored");
    }

    #[test]
    fn over_long_facts_are_counted_and_ranked() {
        let c = db();
        let e = HashEmbedder::new(64);
        remember(&c, &e, "d", "short fact", "t").unwrap();
        remember(&c, &e, "d", &"x".repeat(LONG_FACT_CHARS + 50), "t").unwrap();
        remember(&c, &e, "d", &"y".repeat(LONG_FACT_CHARS + 500), "t").unwrap();
        let h = analyse(&c, 100).unwrap();
        assert_eq!(h.total, 3);
        assert_eq!(h.over_long, 2);
        assert_eq!(h.longest[0].chars, LONG_FACT_CHARS + 500, "longest first");
    }

    #[test]
    fn near_duplicates_are_found_within_a_dataset() {
        let c = db();
        let e = HashEmbedder::new(256);
        remember(&c, &e, "d", "the deploy runs on fly with a worker", "t").unwrap();
        remember(&c, &e, "d", "the deploy runs on fly with a worker!", "t").unwrap();
        remember(&c, &e, "d", "sourdough starter feeding schedule", "t").unwrap();
        let h = analyse(&c, 100).unwrap();
        assert_eq!(h.duplicates.len(), 1, "one pair, not three: {:?}", h.duplicates);
        assert!(h.duplicates[0].similarity >= NEAR_DUPLICATE);
    }

    #[test]
    fn identical_facts_in_different_scopes_are_not_duplicates() {
        // The same truth can legitimately be relevant to a repo AND to the machine.
        // Proposing a merge across scopes generates a proposal that should always be
        // rejected, which is how a review queue teaches you to ignore it.
        let c = db();
        let e = HashEmbedder::new(256);
        remember(&c, &e, "proj_x", "deploy runs on fly", "t").unwrap();
        remember(&c, &e, "global", "deploy runs on fly", "t").unwrap();
        assert!(analyse(&c, 100).unwrap().duplicates.is_empty());
    }

    #[test]
    fn superseded_facts_are_excluded_from_health() {
        let c = db();
        let e = HashEmbedder::new(64);
        let old = remember(&c, &e, "d", &"z".repeat(LONG_FACT_CHARS + 100), "t").unwrap();
        let new = remember(&c, &e, "d", "short replacement", "t").unwrap();
        crate::supersede(&c, &old, &new).unwrap();
        let h = analyse(&c, 100).unwrap();
        assert_eq!(h.total, 1);
        assert_eq!(h.over_long, 0, "a replaced fact is not a problem to fix");
    }

    #[test]
    fn dataset_counts_are_largest_first() {
        let c = db();
        let e = HashEmbedder::new(64);
        for i in 0..5 { remember(&c, &e, "big", &format!("fact {i}"), "t").unwrap(); }
        remember(&c, &e, "small", "one", "t").unwrap();
        let h = analyse(&c, 100).unwrap();
        assert_eq!(h.by_dataset[0], ("big".into(), 5));
        assert_eq!(h.by_dataset[1], ("small".into(), 1));
    }

    #[test]
    fn the_duplicate_scan_is_bounded() {
        // O(n^2) must not become pathological on a large store.
        let c = db();
        let e = HashEmbedder::new(64);
        for i in 0..60 { remember(&c, &e, "d", &format!("fact number {i}"), "t").unwrap(); }
        let h = analyse(&c, 10).unwrap();
        assert_eq!(h.total, 60, "all facts still counted");
        assert!(h.duplicates.len() <= 20);
    }

    #[test]
    fn the_report_leads_with_the_number_that_matters() {
        let c = db();
        let e = HashEmbedder::new(64);
        for i in 0..3 {
            remember(&c, &e, "d", &format!("{}{i}", "q".repeat(LONG_FACT_CHARS + 10)), "t").unwrap();
        }
        let out = render(&analyse(&c, 100).unwrap());
        assert!(out.contains("100%"), "should report the over-long share: {out}");
        // NOT a warning any more. The split pass read the real store's long entries and
        // judged most of them coherent single facts, so flagging length as a fault sent
        // the operator after 476 non-problems. It now points at the tool that can tell.
        assert!(out.contains("hint, not a fault"), "length must not read as a defect: {out}");
        assert!(out.contains("split --dry-run"), "and must name what does decide: {out}");
    }

    #[test]
    fn a_healthy_store_says_so_plainly() {
        let c = db();
        let e = HashEmbedder::new(256);
        remember(&c, &e, "d", "one short fact", "t").unwrap();
        remember(&c, &e, "d", "a completely different short fact", "t").unwrap();
        assert!(render(&analyse(&c, 100).unwrap()).contains("no near-duplicates"));
    }
}
