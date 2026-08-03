//! Head-to-head: does a real embedder beat bag-of-words on SEMANTIC recall?
//!
//! `recall-bench` measures re-retrieval — query built from the target's own words, so
//! lexical overlap is 100% by construction and any bag-of-words method scores ~perfect.
//! That is a real use case (dedup, supersede) but it is not the main one.
//!
//! This measures the main one: an agent asks a question in **its own words** and the
//! right fact must come back. The golden set below is hand-written against facts known
//! to be in this corpus, and deliberately avoids the target's distinctive vocabulary —
//! the reported lexical-overlap figure keeps that honest. Where overlap is low, a
//! lexical model has nothing to work with, so any lift is genuinely semantic.
//!
//! Usage: semantic-bench <paos.db>

use paos_memory::{recall, Embedder, HashEmbedder, Model2VecEmbedder};
use rusqlite::Connection;
use std::collections::HashSet;

/// (scope, question in an agent's own words, substring that identifies the right fact)
const GOLDEN: &[(&str, &str, &str)] = &[
    ("proj_acme_dotfiles", "why did my edit to the home directory vanish", "copied"),
    ("proj_acme_dotfiles", "how do I reach my human when they are not at the keyboard", "away"),
    ("proj_acme_dotfiles", "what should I run at the end of a turn to stay contactable", "reachable"),
    ("proj_acme_dotfiles", "a peer stopped receiving my notes", "listener"),
    ("proj_acme_dotfiles", "where do credentials live on this machine", "credentials"),
    ("proj_acme_dotfiles", "which command shows me who else is working right now", "who"),
    ("global_memory", "how should I talk to him on his phone", "short"),
    ("global_memory", "does he want me to check in before finishing", "autonom"),
    ("global_memory", "what happens to my work once it is finished", "push"),
    ("proj_examplecorp_browser_cluster", "the automated sign-in stopped working", "login"),
];

fn lexical_overlap(q: &str, target: &str) -> f64 {
    let t: HashSet<String> = target.split_whitespace().map(|w| w.to_lowercase()).collect();
    let qw: Vec<String> = q.split_whitespace().map(|w| w.to_lowercase()).collect();
    if qw.is_empty() {
        return 0.0;
    }
    qw.iter().filter(|w| t.contains(*w)).count() as f64 / qw.len() as f64
}

fn score(conn: &Connection, e: &dyn Embedder, label: &str) -> (usize, usize, f64) {
    let (mut hit1, mut hit5, mut overlap) = (0usize, 0usize, 0.0f64);
    println!("\n  {label}");
    for (scope, question, needle) in GOLDEN {
        let hits = recall(conn, e, &[scope.to_string()], question, 5).unwrap_or_default();
        let pos = hits
            .iter()
            .position(|h| h.memory.text.to_lowercase().contains(&needle.to_lowercase()));
        match pos {
            Some(0) => {
                hit1 += 1;
                hit5 += 1;
            }
            Some(_) => hit5 += 1,
            None => {}
        }
        if let Some(p) = pos {
            overlap += lexical_overlap(question, &hits[p].memory.text);
        }
        let mark = match pos {
            Some(0) => "1st",
            Some(p) => match p {
                1 => "2nd",
                2 => "3rd",
                _ => "top5",
            },
            None => "MISS",
        };
        println!("    {mark:>4}  {question}");
    }
    (hit1, hit5, if hit5 == 0 { 0.0 } else { overlap / hit5 as f64 })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: semantic-bench <paos.db>");
        std::process::exit(2);
    }
    // TWO stores: each must be embedded with the embedder used to query it, or the
    // comparison is across incompatible coordinate systems and means nothing.
    if args.len() < 2 {
        eprintln!("usage: semantic-bench <hash.db> <model.db>");
        std::process::exit(2);
    }
    let conn = Connection::open(&args[0]).expect("open hash db");
    let conn_m = Connection::open(&args[1]).expect("open model db");
    let n = GOLDEN.len();

    let hash = HashEmbedder::new(512);
    paos_memory::check_space(&conn, &hash).expect("hash store must hold hash vectors");
    let (h1, h5, ho) = score(&conn, &hash, "hash-v1 (bag of words)");

    let dir = Model2VecEmbedder::default_dir();
    match Model2VecEmbedder::from_dir(&dir) {
        Ok(m) => {
            paos_memory::check_space(&conn_m, &m).expect("model store must hold model vectors");
            let (m1, m5, mo) = score(&conn_m, &m, m.id());
            println!("\n  ── results ──────────────────────────────");
            println!("  {:<24} recall@1 {:>2}/{n}   recall@5 {:>2}/{n}", "hash-v1", h1, h5);
            println!("  {:<24} recall@1 {:>2}/{n}   recall@5 {:>2}/{n}", m.id(), m1, m5);
            println!("\n  mean lexical overlap on hits: hash {:.0}%, model {:.0}%", ho * 100.0, mo * 100.0);
            println!("  (low overlap + a hit = the match was semantic, not lexical)");
        }
        Err(e) => {
            println!("\n  model unavailable: {e}");
            println!("  hash-v1 alone: recall@1 {h1}/{n}, recall@5 {h5}/{n}");
        }
    }
}
