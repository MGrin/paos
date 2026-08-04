//! Re-embed a COPY of a store with a different embedder, so a candidate can be scored on
//! the real corpus without touching the live one.
//!
//! Changing embedder invalidates every stored vector — `check_space` refuses to mix
//! spaces precisely because comparing coordinates from two models returns numbers that
//! look fine and mean nothing. So a candidate cannot be evaluated in place: it needs its
//! own copy of the corpus, embedded end to end, which is what this writes.
//!
//! Usage: `reembed <src.db> <dst.db>`
//!
//! `dst.db` must not exist. Refusing to overwrite is deliberate — the obvious typo here
//! is naming the live store as the destination, and that would destroy every vector in it
//! with no way back short of a backup.

use paos_memory::{BertEmbedder, Embedder};
use rusqlite::Connection;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: reembed <src.db> <dst.db>");
        std::process::exit(2);
    }
    let (src, dst) = (std::path::Path::new(&args[0]), std::path::Path::new(&args[1]));
    if dst.exists() {
        eprintln!("reembed: {} already exists — refusing to overwrite", dst.display());
        std::process::exit(2);
    }
    // Copy the FILE rather than re-inserting rows: ids, datasets, timestamps, usage
    // counters and the review queue all carry over untouched, so the only difference
    // between the two stores is the vector space. Anything else would confound the
    // comparison this exists to make.
    if let Err(e) = std::fs::copy(src, dst) {
        eprintln!("reembed: copying {} to {}: {e}", src.display(), dst.display());
        std::process::exit(1);
    }

    let embedder = match BertEmbedder::from_dir(&BertEmbedder::default_dir()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("reembed: {e}");
            std::process::exit(1);
        }
    };
    let conn = match Connection::open(dst) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("reembed: opening {}: {e}", dst.display());
            std::process::exit(1);
        }
    };

    let rows: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT id, text, aliases FROM memories")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .and_then(|it| it.collect())
        })
        .unwrap_or_default();
    println!("re-embedding {} fact(s) with {}", rows.len(), embedder.id());

    let started = std::time::Instant::now();
    let (mut done, mut aliased) = (0usize, 0usize);
    for (id, text, aliases) in &rows {
        let vec = paos_memory::encode_vec(&embedder.embed(text));
        let alias_vec = aliases
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| paos_memory::encode_vec(&embedder.embed(a)));
        if alias_vec.is_some() {
            aliased += 1;
        }
        if let Err(e) = conn.execute(
            "UPDATE memories SET embedding = ?1, alias_embedding = ?2 WHERE id = ?3",
            rusqlite::params![vec, alias_vec, id],
        ) {
            eprintln!("  {id}: {e}");
            continue;
        }
        done += 1;
        if done % 200 == 0 {
            println!("  {done}/{}", rows.len());
        }
    }
    // The space marker must change with the vectors. Leaving it would make the copy claim
    // to hold potion vectors while holding bert ones — check_space would wave it through
    // and every score afterwards would be meaningless.
    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO memory_meta(key, value) VALUES('embedder', ?1)",
        [embedder.id()],
    ) {
        eprintln!("reembed: could not record the vector space: {e}");
        std::process::exit(1);
    }
    println!(
        "done: {done} fact(s), {aliased} with phrasings, in {:.1}s",
        started.elapsed().as_secs_f64()
    );
}
