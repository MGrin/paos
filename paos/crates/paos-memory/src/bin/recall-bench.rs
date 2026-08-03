//! Golden-set recall measurement.
//!
//! The spec gates the embedder choice on evidence, not on MTEB numbers from a paper:
//! MTEB retrieval is long-document, and PAOS stores 10–50-word atomic facts, where
//! static embeddings typically close much of the published gap. This measures the real
//! corpus instead of arguing about it.
//!
//! Method: for each sampled memory, build a query from a *held-out slice* of its own
//! text — the middle third — and ask whether the source fact comes back in the top-k of
//! its own scope. Using the middle third matters: querying with the whole fact would
//! make any lexical method look perfect, and querying with the first words would favour
//! them unfairly.
//!
//! Reports recall@1/@5 overall and for the smallest scopes separately, because scope
//! size is exactly where the old over-fetch-and-filter approach starved.
//!
//! Usage: recall-bench <paos.db> [sample_size]

use paos_memory::{best_available, check_space, recall, Embedder};
use rusqlite::Connection;
use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: recall-bench <paos.db> [sample_size]");
        std::process::exit(2);
    }
    let sample: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    if let Err(e) = run(&args[0], sample) {
        eprintln!("recall-bench: {e}");
        std::process::exit(1);
    }
}

/// The middle third of the words — held out from neither end.
fn query_from(text: &str) -> Option<String> {
    let w: Vec<&str> = text.split_whitespace().collect();
    if w.len() < 9 {
        return None; // too short to hold anything out meaningfully
    }
    let a = w.len() / 3;
    let b = (w.len() * 2) / 3;
    Some(w[a..b].join(" "))
}

fn run(db: &str, sample: usize) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open(db)?;
    // Use whatever the DAEMON would use, and refuse to run if the store was built with
    // something else. This binary hardcoded HashEmbedder while the live store holds
    // potion-retrieval-32M vectors, so it compared coordinates from unrelated spaces and
    // reported 3.3% recall@1 — a meaningless number, produced by the very tool meant to
    // gate the embedder decision. check_space already existed for exactly this; it was
    // simply never called here.
    let embedder: Box<dyn Embedder> = best_available();
    if let Err(e) = check_space(&conn, embedder.as_ref()) {
        eprintln!("recall-bench: {e}");
        std::process::exit(2);
    }

    let mut stmt = conn.prepare(
        "SELECT id, dataset, text FROM memories WHERE superseded IS NULL ORDER BY id",
    )?;
    let all: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut sizes: BTreeMap<String, usize> = BTreeMap::new();
    for (_, ds, _) in &all {
        *sizes.entry(ds.clone()).or_default() += 1;
    }

    // Deterministic spread across the corpus rather than a random sample, so re-running
    // after an embedder change compares like with like.
    let step = (all.len() / sample.max(1)).max(1);
    let mut n = 0usize;
    let (mut at1, mut at5) = (0usize, 0usize);
    let (mut small_n, mut small_at5) = (0usize, 0usize);
    let mut worst: Vec<(f32, String, String)> = Vec::new();

    for (i, (id, dataset, text)) in all.iter().enumerate() {
        if i % step != 0 || n >= sample {
            continue;
        }
        let Some(q) = query_from(text) else { continue };
        let hits = recall(&conn, embedder.as_ref(), std::slice::from_ref(dataset), &q, 5)?;
        let pos = hits.iter().position(|h| &h.memory.id == id);
        n += 1;
        match pos {
            Some(0) => {
                at1 += 1;
                at5 += 1;
            }
            Some(_) => at5 += 1,
            None => worst.push((
                hits.first().map(|h| h.score).unwrap_or(0.0),
                dataset.clone(),
                text.chars().take(60).collect(),
            )),
        }
        if sizes.get(dataset).copied().unwrap_or(0) <= 20 {
            small_n += 1;
            if pos.is_some() {
                small_at5 += 1;
            }
        }
    }

    // HOW HARD WAS THIS TEST? The query is a literal slice of the target, so lexical
    // overlap is ~100% by construction and a bag-of-words method is heavily favoured.
    // Print it so the recall number is never read as evidence of SEMANTIC retrieval.
    let mut overlap_sum = 0.0f64;
    let mut overlap_n = 0usize;
    for (i, (_, _, text)) in all.iter().enumerate() {
        if i % step != 0 { continue; }
        let Some(q) = query_from(text) else { continue };
        let target: std::collections::HashSet<String> =
            text.split_whitespace().map(|w| w.to_lowercase()).collect();
        let qw: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
        if qw.is_empty() { continue; }
        let shared = qw.iter().filter(|w| target.contains(*w)).count();
        overlap_sum += shared as f64 / qw.len() as f64;
        overlap_n += 1;
        if overlap_n >= sample { break; }
    }

    let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    println!("embedder: {} ({} dims)", embedder.id(), embedder.dimensions());
    println!("corpus:   {} memories across {} scopes", all.len(), sizes.len());
    println!("sampled:  {n} (query = middle third of each fact, searched in its own scope)");
    println!();
    println!("  recall@1  {:>6.1}%   ({at1}/{n})", pct(at1, n));
    println!("  recall@5  {:>6.1}%   ({at5}/{n})", pct(at5, n));
    println!("  recall@5 in scopes of <=20 facts  {:>6.1}%   ({small_at5}/{small_n})",
             pct(small_at5, small_n));
    println!();
    println!("  TEST DIFFICULTY: mean lexical overlap between query and target = {:.0}%",
             if overlap_n == 0 { 0.0 } else { overlap_sum * 100.0 / overlap_n as f64 });
    println!("  -> This measures RE-RETRIEVAL (\"find this fact again\"), which is what");
    println!("     dedup and supersede need. It does NOT measure semantic recall");
    println!("     (\"answer this question\"), where a bag-of-words model has no synonyms");
    println!("     and no word order. That test needs a real semantic embedder to compare");
    println!("     against, and is what gates the model2vec decision.");
    if !worst.is_empty() {
        println!("\n  {} miss(es); a sample:", worst.len());
        for (s, ds, t) in worst.iter().take(3) {
            println!("    top-score {s:.3} [{ds}] {t}…");
        }
    }
    Ok(())
}
